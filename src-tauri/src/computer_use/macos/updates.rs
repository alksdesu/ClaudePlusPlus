use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};

use super::{
    atomic_json, execute, fingerprint, inspect, plist_at, running, verify_signature, Installation,
    VENDOR_REQUIREMENT,
};
use crate::computer_use::native::Selection;
use crate::computer_use::updates::{
    consume_event, names, parse_event, pending, Cursor, Journal, Phase, Status, Tick,
};

const JOURNAL: &str = "desktop-update.json";
const MAX_LOG: u64 = 256 * 1024;

struct Controller {
    installation: Installation,
    production: bool,
}

fn bundle_stamp(app: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    for relative in [
        "Contents/Info.plist",
        "Contents/_CodeSignature/CodeResources",
        "Contents/MacOS/Claude",
    ] {
        let path = app.join(relative);
        ensure!(!path.is_symlink(), "更新检测文件被替换为符号链接");
        digest.update(fs::read(path)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn version(app: &Path) -> Result<String> {
    let p = plist_at(app)?;
    let d = p.as_dictionary().context("应用信息格式无效")?;
    ensure!(
        d.get("CFBundleIdentifier")
            .and_then(plist::Value::as_string)
            == Some(super::BUNDLE_ID),
        "更新目标不是 Claude Desktop"
    );
    d.get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .map(str::to_owned)
        .context("应用缺少版本")
}

fn verify_vendor(app: &Path) -> Result<()> {
    ensure!(!app.is_symlink(), "更新目标不允许是符号链接");
    version(app)?;
    execute(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "--all-architectures"])
            .arg(format!("-R={VENDOR_REQUIREMENT}"))
            .arg(app),
        None,
    )?;
    Ok(())
}

fn installer_running() -> Result<bool> {
    let out = execute(Command::new("/bin/ps").args(["-axo", "comm="]), None)?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let p = line.trim();
        p.ends_with("/ShipIt")
            && (p.contains("/Claude.app/Contents/Frameworks/Squirrel.framework/")
                || p.contains("com.anthropic.claudefordesktop.ShipIt"))
    }))
}

fn compatible_selection(version: &str, desired: Selection) -> Selection {
    if super::asar::check_version(version).is_err() {
        return Selection::default();
    }
    let supports = |s| crate::computer_use::native::check_selection(version, s).is_ok();
    Selection {
        computer_use: desired.computer_use
            && supports(Selection {
                computer_use: true,
                ..Default::default()
            }),
        chinese: desired.chinese
            && supports(Selection {
                chinese: true,
                ..Default::default()
            }),
        simulators: desired.simulators
            && supports(Selection {
                simulators: true,
                ..Default::default()
            }),
        prewarm: desired.prewarm
            && supports(Selection {
                prewarm: true,
                ..Default::default()
            }),
        fast_mode_status: desired.fast_mode_status
            && supports(Selection {
                fast_mode_status: true,
                ..Default::default()
            }),
    }
}

fn remaining(desired: Selection, applied: Selection) -> Selection {
    Selection {
        computer_use: desired.computer_use && !applied.computer_use,
        chinese: desired.chinese && !applied.chinese,
        simulators: desired.simulators && !applied.simulators,
        prewarm: desired.prewarm && !applied.prewarm,
        fast_mode_status: desired.fast_mode_status && !applied.fast_mode_status,
    }
}

impl Controller {
    fn standard() -> Result<Self> {
        Ok(Self {
            installation: Installation::standard()?,
            production: true,
        })
    }

    fn lock(&self) -> Result<File> {
        let store = &self.installation.store;
        fs::create_dir_all(store)?;
        ensure!(!store.is_symlink(), "更新维护目录不允许是符号链接");
        fs::set_permissions(store, fs::Permissions::from_mode(0o700))?;
        let path = store.join("desktop-update.lock");
        ensure!(!path.is_symlink(), "更新维护锁不允许是符号链接");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .context("更新维护操作正在进行，请稍后重试")?;
        Ok(file)
    }

    fn read(&self) -> Result<Option<Journal>> {
        let p = self.installation.store.join(JOURNAL);
        if !p.try_exists()? {
            return Ok(None);
        }
        ensure!(!p.is_symlink(), "更新维护记录不允许是符号链接");
        let j: Journal =
            serde_json::from_slice(&fs::read(p)?).context("更新维护记录损坏，拒绝覆盖")?;
        ensure!(
            j.schema == 1 && j.target == self.installation.app,
            "更新维护记录与版本或目标不匹配"
        );
        Ok(Some(j))
    }

    fn save(&self, j: &Journal) -> Result<()> {
        atomic_json(&self.installation.store.join(JOURNAL), j)
    }

    fn view(&self, j: Option<&Journal>) -> Status {
        match j {
            None => Status {
                supported: true,
                enabled: false,
                phase: Phase::Watching,
                message: "自动维护已关闭".into(),
                desired_features: vec![],
                pending_features: vec![],
                error: None,
                busy: false,
            },
            Some(j) => Status {
                supported: true,
                enabled: j.enabled,
                phase: j.phase,
                message: j.message.clone(),
                desired_features: names(j.desired),
                pending_features: if j.phase == Phase::Unsupported {
                    names(remaining(j.desired, j.applied))
                } else {
                    vec![]
                },
                error: j.error.clone(),
                busy: matches!(j.phase, Phase::Restoring | Phase::Applying),
            },
        }
    }

    fn enable(&self, enabled: bool) -> Result<Status> {
        let _lock = self.lock()?;
        let existing = self.read()?;
        if !enabled {
            if let Some(mut j) = existing {
                j.enabled = false;
                j.phase = Phase::Watching;
                j.error = None;
                j.launch_pending = false;
                j.resume_phase = None;
                j.message = "自动维护已关闭".into();
                self.save(&j)?;
                return Ok(self.view(Some(&j)));
            }
            return Ok(self.view(None));
        }
        if let Some(j) = existing.as_ref().filter(|j| j.enabled) {
            return Ok(self.view(Some(j)));
        }
        if let Some(mut j) = existing.filter(|j| !j.desired.is_empty()) {
            if verify_vendor(&self.installation.app).is_ok() {
                let stamp = bundle_stamp(&self.installation.app)?;
                j.enabled = true;
                j.phase = if j.restored_stamp.as_deref() == Some(stamp.as_str()) {
                    Phase::AwaitingUpdate
                } else {
                    Phase::WaitingForReapply
                };
                j.launch_pending = j.phase == Phase::AwaitingUpdate && self.production;
                j.error = None;
                j.resume_phase = None;
                j.attempted_stamp = None;
                j.message = "自动维护已恢复".into();
                self.save(&j)?;
                return Ok(self.view(Some(&j)));
            }
        }
        let record = self
            .installation
            .record()?
            .context("请先安装原生增强，再开启自动更新维护")?;
        ensure!(
            record.phase == "installed",
            "请先完成原生增强安装或恢复事务"
        );
        ensure!(
            record.modified_hash.as_deref() == Some(fingerprint(&self.installation.app)?.as_str()),
            "当前应用与已记录补丁不一致"
        );
        verify_signature(&self.installation.app, None)?;
        let desired = record
            .selection
            .unwrap_or(super::selection_at(&self.installation.app)?);
        ensure!(!desired.is_empty(), "没有需要在更新后恢复的原生增强");
        let floor = fs::metadata(
            self.installation
                .app
                .join("Contents/_CodeSignature/CodeResources"),
        )?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs() as i64;
        let mut j = Journal {
            schema: 1,
            target: self.installation.app.clone(),
            enabled: true,
            desired,
            applied: desired,
            identity: record.identity,
            phase: Phase::Watching,
            source_version: version(&self.installation.app)?,
            observed_stamp: bundle_stamp(&self.installation.app)?,
            restored_stamp: None,
            last_event: floor,
            cursors: vec![],
            policy_blocked: false,
            launch_pending: false,
            attempted_stamp: None,
            adapter_version: env!("CARGO_PKG_VERSION").into(),
            message: "自动维护已开启，有更新时会等 Desktop 退出再处理".into(),
            error: None,
            resume_phase: None,
        };
        self.read_events(&mut j)?;
        self.save(&j)?;
        Ok(self.view(Some(&j)))
    }

    fn log_paths(&self) -> Vec<PathBuf> {
        if !self.production {
            return vec![self.installation.store.join("updater-fixture.log")];
        }
        dirs::home_dir()
            .map(|h| {
                ["Claude-3p", "Claude"].map(|n| h.join("Library/Logs").join(n).join("main.log"))
            })
            .map(Vec::from)
            .unwrap_or_default()
    }

    fn read_events(&self, j: &mut Journal) -> Result<()> {
        let mut events = Vec::new();
        for path in self.log_paths() {
            if !path.try_exists()? {
                continue;
            }
            ensure!(!path.is_symlink(), "更新日志不允许是符号链接");
            let mut file = File::open(&path)?;
            let meta = file.metadata()?;
            let key = path.to_string_lossy().into_owned();
            let old = j.cursors.iter().find(|c| c.path == key);
            let resume = old.filter(|c| c.inode == meta.ino() && c.offset <= meta.len());
            let offset = resume
                .map(|c| c.offset)
                .unwrap_or_else(|| meta.len().saturating_sub(MAX_LOG));
            let start = offset.max(meta.len().saturating_sub(MAX_LOG));
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            file.take(MAX_LOG).read_to_end(&mut bytes)?;
            let consumed = bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or_else(|| {
                    if bytes.len() as u64 == MAX_LOG {
                        bytes.len()
                    } else {
                        0
                    }
                });
            let text = String::from_utf8_lossy(&bytes[..consumed]);
            for (i, line) in text.lines().enumerate() {
                if i == 0 && start > 0 && resume.is_none_or(|c| c.offset != start) {
                    continue;
                }
                if let Some(event) = parse_event(line).filter(|e| e.time >= j.last_event) {
                    events.push(event);
                }
            }
            j.cursors.retain(|c| c.path != key);
            j.cursors.push(Cursor {
                path: key,
                inode: meta.ino(),
                offset: start + consumed as u64,
            });
        }
        events.sort_by_key(|e| e.time);
        for event in events {
            consume_event(j, event);
        }
        Ok(())
    }

    fn mark_official(&self) -> Result<()> {
        let _lock = self.installation.lock()?;
        verify_vendor(&self.installation.app)?;
        if let Some(mut r) = self.installation.record()? {
            if r.phase != "restored" {
                self.installation.clear_staging(&r)?;
                r.phase = "restored".into();
                r.previous_modified_hash = None;
                self.installation.save(&r)?;
            }
        }
        Ok(())
    }

    fn advance(&self, j: &mut Journal) -> Result<()> {
        let app = &self.installation.app;
        self.read_events(j)?;
        let stamp = match bundle_stamp(app) {
            Ok(s) => s,
            Err(_error) if !app.exists() => {
                j.message = "等官方安装器替换完应用".into();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let active = running(app)? || self.production && installer_running()?;
        if j.phase == Phase::Watching && stamp != j.observed_stamp {
            j.phase = Phase::WaitingForReapply;
            j.message = "Desktop 文件有变化，等它退出后检查".into();
        }
        if j.phase == Phase::Unsupported
            && (j.attempted_stamp.as_deref() != Some(stamp.as_str())
                || j.adapter_version != env!("CARGO_PKG_VERSION"))
        {
            j.phase = Phase::WaitingForReapply;
        }
        if j.policy_blocked
            && matches!(
                j.phase,
                Phase::Watching | Phase::WaitingForExit | Phase::Restoring
            )
        {
            return Ok(());
        }
        match j.phase {
            Phase::WaitingForExit | Phase::Restoring => {
                if active {
                    j.message = "等 Desktop 和官方安装器退出".into();
                    return Ok(());
                }
                if j.phase == Phase::Restoring && verify_vendor(app).is_ok() {
                    let baseline = self
                        .installation
                        .record()?
                        .context("还原恢复缺少安装记录")?
                        .baseline_hash;
                    if fingerprint(app)? != baseline {
                        self.mark_official()?;
                        j.phase = Phase::WaitingForReapply;
                        self.save(j)?;
                        return self.advance(j);
                    }
                }
                if stamp != j.observed_stamp && j.phase != Phase::Restoring {
                    verify_vendor(app)?;
                    j.phase = Phase::WaitingForReapply;
                    self.save(j)?;
                    return self.advance(j);
                }
                j.phase = Phase::Restoring;
                j.message = "正在还原官方包".into();
                self.save(j)?;
                self.installation.restore()?;
                verify_vendor(app)?;
                j.restored_stamp = Some(bundle_stamp(app)?);
                j.applied = Selection::default();
                j.phase = Phase::AwaitingUpdate;
                j.launch_pending = self.production;
                j.message = "官方包已还原，等官方更新装完".into();
                self.save(j)?;
                self.launch(j)?;
            }
            Phase::AwaitingUpdate => {
                if j.restored_stamp.as_deref() != Some(stamp.as_str()) {
                    j.phase = Phase::WaitingForReapply;
                    j.message = "更新已装好，等 Desktop 退出后把功能装回去".into();
                    self.save(j)?;
                    if !active {
                        return self.advance(j);
                    }
                } else if j.launch_pending && !active {
                    self.launch(j)?;
                }
            }
            Phase::WaitingForReapply | Phase::Applying => {
                if active {
                    j.message = "已排队，等 Desktop 和官方安装器退出后装回功能".into();
                    return Ok(());
                }
                let record = self.installation.record()?;
                if let Some(r) = &record {
                    if !matches!(r.phase.as_str(), "installed" | "restored") {
                        if verify_vendor(app).is_ok() {
                            self.mark_official()?;
                        } else {
                            self.installation.restore()?;
                        }
                    }
                }
                let record = self.installation.record()?;
                let managed = if let Some(r) = record.as_ref().filter(|r| r.phase == "installed") {
                    r.modified_hash.as_deref() == Some(fingerprint(app)?.as_str())
                } else {
                    false
                };
                let original = if managed {
                    self.installation.baseline(record.as_ref().unwrap())
                } else {
                    app.clone()
                };
                verify_vendor(&original)?;
                if !managed {
                    self.mark_official()?;
                }
                let v = version(&original)?;
                let selection = compatible_selection(&v, j.desired);
                j.adapter_version = env!("CARGO_PKG_VERSION").into();
                j.attempted_stamp = Some(stamp.clone());
                if selection.is_empty() || inspect(&original).is_err() {
                    j.phase = Phase::Unsupported;
                    j.applied = if managed {
                        super::selection_at(app)?
                    } else {
                        Selection::default()
                    };
                    j.message = "新版还没适配，先保持官方原样，适配后自动重试".into();
                    return Ok(());
                }
                j.phase = Phase::Applying;
                j.message = "正在把功能装回新版".into();
                self.save(j)?;
                self.installation.apply(&j.identity, selection)?;
                verify_signature(app, None)?;
                j.applied = selection;
                j.observed_stamp = bundle_stamp(app)?;
                j.source_version = v;
                j.restored_stamp = None;
                j.launch_pending = false;
                j.attempted_stamp = Some(j.observed_stamp.clone());
                j.error = None;
                j.resume_phase = None;
                j.phase = if selection == j.desired {
                    Phase::Watching
                } else {
                    Phase::Unsupported
                };
                j.message = if selection == j.desired {
                    "更新完成，功能已装回"
                } else {
                    "能装的功能已装回，其余等适配"
                }
                .into();
            }
            Phase::Watching | Phase::Unsupported | Phase::Failed => {}
        }
        Ok(())
    }

    fn launch(&self, j: &mut Journal) -> Result<()> {
        if !j.launch_pending || !self.production || j.policy_blocked {
            return Ok(());
        }
        verify_vendor(&self.installation.app)?;
        execute(
            Command::new("/usr/bin/open")
                .args(["-g", "-a"])
                .arg(&self.installation.app),
            None,
        )?;
        j.launch_pending = false;
        self.save(j)
    }

    fn tick(&self) -> Result<Tick> {
        let _lock = self.lock()?;
        let Some(mut j) = self.read()?.filter(|j| j.enabled) else {
            return Ok(Tick::default());
        };
        let before = (j.phase, j.message.clone(), j.error.clone());
        let resume = j.phase;
        if let Err(error) = self.advance(&mut j) {
            j.resume_phase = Some(match j.phase {
                Phase::Restoring => Phase::Restoring,
                Phase::Applying => Phase::WaitingForReapply,
                _ => resume,
            });
            j.phase = Phase::Failed;
            j.error = Some(format!("{error:#}"));
            j.message = "更新维护已停止，应用和备份都在；处理好后点「重新检测」".into();
        }
        self.save(&j)?;
        Ok(Tick {
            pending: pending(j.phase),
            changed: before != (j.phase, j.message.clone(), j.error.clone()),
            failed: j.phase == Phase::Failed,
            message: j.error.clone().unwrap_or(j.message),
        })
    }
}

pub(crate) fn status() -> Result<Status> {
    let c = Controller::standard()?;
    Ok(c.view(c.read()?.as_ref()))
}
pub(crate) fn set_enabled(enabled: bool) -> Result<Status> {
    Controller::standard()?.enable(enabled)
}
pub(crate) fn tick() -> Result<Tick> {
    Controller::standard()?.tick()
}
pub(crate) fn retry() -> Result<Status> {
    let c = Controller::standard()?;
    let _lock = c.lock()?;
    let mut j = c.read()?.context("自动维护尚未启用")?;
    ensure!(j.enabled, "请先开启自动维护");
    if j.phase == Phase::Failed {
        j.phase = j.resume_phase.take().unwrap_or(Phase::Watching);
    }
    if j.phase == Phase::Unsupported {
        j.phase = Phase::WaitingForReapply;
    }
    j.error = None;
    j.attempted_stamp = None;
    if j.phase == Phase::AwaitingUpdate {
        j.launch_pending = true;
    }
    j.message = "已安排重新检测".into();
    c.save(&j)?;
    Ok(c.view(Some(&j)))
}

pub(crate) fn manual_change<T>(restore: bool, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    Controller::standard()?.manual_change(restore, operation)
}

impl Controller {
    fn manual_change<T>(&self, restore: bool, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let c = self;
        let _lock = c.lock()?;
        let mut journal = c.read()?;
        if let Some(j) = &mut journal {
            if restore {
                j.enabled = false;
                j.launch_pending = false;
                j.phase = Phase::Watching;
                j.desired = Selection::default();
                j.applied = Selection::default();
                j.message = "手动还原后自动维护已关闭".into();
                j.error = None;
                c.save(j)?;
            } else {
                ensure!(
                    !j.enabled || !pending(j.phase),
                    "自动更新维护正在进行；请先关闭自动维护再手动改变功能组合"
                );
            }
        }
        let result = operation()?;
        if let Some(j) = &mut journal {
            if !restore && j.enabled {
                let r = c.installation.record()?.context("安装完成但缺少记录")?;
                j.desired = r.selection.context("安装记录缺少功能组合")?;
                j.applied = j.desired;
                j.identity = r.identity;
                j.observed_stamp = bundle_stamp(&c.installation.app)?;
                j.source_version = r.version;
                j.phase = Phase::Watching;
                j.error = None;
                j.message = "自动维护已同步新的功能组合".into();
                c.save(j)?;
            }
        }
        Ok(result)
    }
}

pub(crate) fn probe(app: &Path, store: &Path, action: &str) -> Result<Status> {
    let c = Controller {
        installation: Installation::copy(app, store)?,
        production: false,
    };
    match action {
        "enable" => c.enable(true),
        "disable" => c.enable(false),
        "tick" => {
            c.tick()?;
            Ok(c.view(c.read()?.as_ref()))
        }
        "status" => Ok(c.view(c.read()?.as_ref())),
        _ => anyhow::bail!("未知更新维护探针操作"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Controller, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Claude.app");
        fs::create_dir(&app).unwrap();
        let store = dir.path().join("store");
        fs::create_dir(&store).unwrap();
        let c = Controller {
            installation: Installation::copy(&app, &store).unwrap(),
            production: false,
        };
        let j = Journal {
            schema: 1,
            target: c.installation.app.clone(),
            enabled: true,
            desired: Selection {
                computer_use: true,
                ..Default::default()
            },
            applied: Selection {
                computer_use: true,
                ..Default::default()
            },
            identity: String::new(),
            phase: Phase::WaitingForExit,
            source_version: String::new(),
            observed_stamp: String::new(),
            restored_stamp: None,
            last_event: 0,
            cursors: vec![],
            policy_blocked: false,
            launch_pending: false,
            attempted_stamp: None,
            adapter_version: String::new(),
            message: String::new(),
            error: None,
            resume_phase: None,
        };
        c.save(&j).unwrap();
        (dir, c, j)
    }

    #[test]
    fn manual_restore_cancels_automatic_reinstallation() {
        let (_dir, c, _) = fixture();
        c.manual_change(true, || Ok(())).unwrap();
        let j = c.read().unwrap().unwrap();
        assert!(!j.enabled && j.desired.is_empty());
        assert!(!c.tick().unwrap().pending);
    }

    #[test]
    fn maintenance_blocks_competing_manual_changes_and_keeps_intent_on_failure() {
        let (_dir, c, j) = fixture();
        assert!(c.manual_change(false, || Ok(())).is_err());
        assert_eq!(c.read().unwrap().unwrap().desired, j.desired);
        let _guard = c.lock().unwrap();
        assert!(c.tick().is_err());
        assert!(c.enable(false).is_err());
    }

    #[test]
    fn corrupted_or_foreign_journals_are_never_replaced() {
        let (_dir, c, mut j) = fixture();
        j.target = PathBuf::from("/another/Claude.app");
        c.save(&j).unwrap();
        let before = fs::read(c.installation.store.join(JOURNAL)).unwrap();
        assert!(c.read().is_err());
        assert!(c.enable(false).is_err());
        assert_eq!(
            fs::read(c.installation.store.join(JOURNAL)).unwrap(),
            before
        );
    }

    #[test]
    fn unknown_versions_keep_all_features_pending() {
        let all = Selection {
            computer_use: true,
            chinese: true,
            simulators: true,
            prewarm: true,
            fast_mode_status: true,
        };
        assert_eq!(compatible_selection("unknown", all), Selection::default());
        assert_eq!(compatible_selection("3.1623.4", all), all);
        assert_eq!(compatible_selection("3.6556.2", all), all);
        assert_eq!(compatible_selection("3.6556.3", all), Selection::default());
        let old = compatible_selection("3.1623.1", all);
        assert!(old.computer_use && !old.fast_mode_status);
        assert_eq!(names(remaining(all, old)).len(), 4);
    }

    #[test]
    fn log_reader_handles_partial_lines_rotation_and_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Claude.app");
        fs::create_dir(&app).unwrap();
        let store = dir.path().join("store");
        fs::create_dir(&store).unwrap();
        let c = Controller {
            installation: Installation::copy(&app, &store).unwrap(),
            production: false,
        };
        let path = store.join("updater-fixture.log");
        let mut j = Journal {
            schema: 1,
            target: app.clone(),
            enabled: true,
            desired: Selection::default(),
            applied: Selection {
                computer_use: true,
                ..Default::default()
            },
            identity: String::new(),
            phase: Phase::Watching,
            source_version: String::new(),
            observed_stamp: String::new(),
            restored_stamp: None,
            last_event: 0,
            cursors: vec![],
            policy_blocked: false,
            launch_pending: false,
            attempted_stamp: None,
            adapter_version: String::new(),
            message: String::new(),
            error: None,
            resume_phase: None,
        };
        fs::write(
            &path,
            "2026-10-24 01:00:00 [info] [updater] Found an update, down",
        )
        .unwrap();
        c.read_events(&mut j).unwrap();
        assert_eq!(j.phase, Phase::Watching);
        fs::write(
            &path,
            "2026-10-24 01:00:00 [info] [updater] Found an update, downloading\n",
        )
        .unwrap();
        c.read_events(&mut j).unwrap();
        assert_eq!(j.phase, Phase::WaitingForExit);
        j.phase = Phase::Watching;
        c.read_events(&mut j).unwrap();
        assert_eq!(j.phase, Phase::Watching);
        fs::remove_file(&path).unwrap();
        fs::write(
            &path,
            "2026-10-23 01:00:00 [info] [updater] Found an update, downloading\n",
        )
        .unwrap();
        c.read_events(&mut j).unwrap();
        assert_eq!(j.phase, Phase::Watching);
    }
}

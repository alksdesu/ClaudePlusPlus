use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{asar, parse_identities, renderer, validate_identity, SigningIdentity, Status};

const APP: &str = "/Applications/Claude.app";
const BUNDLE_ID: &str = "com.anthropic.claudefordesktop";
const VENDOR_REQUIREMENT: &str = "anchor apple generic and certificate leaf[subject.OU] = \"Q6L2SF6YDW\" and identifier \"com.anthropic.claudefordesktop\"";
const IDENTITY_KEYS: &[&str] = &[
    "com.apple.application-identifier",
    "com.apple.developer.team-identifier",
    "keychain-access-groups",
    "com.apple.security.application-groups",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Record {
    schema: u32,
    id: uuid::Uuid,
    target: PathBuf,
    version: String,
    identity: String,
    baseline_hash: String,
    modified_hash: Option<String>,
    phase: String,
    #[serde(default)]
    selection: Option<super::native::Selection>,
    #[serde(default)]
    previous_modified_hash: Option<String>,
}

struct Installation {
    app: PathBuf,
    store: PathBuf,
}

fn execute(command: &mut Command, log: Option<&Path>) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("启动 {}", command.get_program().to_string_lossy()))?;
    if let Some(path) = log {
        let event = serde_json::json!({
            "command": std::iter::once(command.get_program()).chain(command.get_args()).map(|s| s.to_string_lossy()).collect::<Vec<_>>(),
            "stdout":String::from_utf8_lossy(&output.stdout), "stderr":String::from_utf8_lossy(&output.stderr),
            "exitStatus":output.status.code(),
        });
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{event}")?;
        file.sync_all()?;
    }
    ensure!(
        output.status.success(),
        "{} 失败（{}）：{}",
        command.get_program().to_string_lossy(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn identities() -> Result<Vec<SigningIdentity>> {
    let out = execute(
        Command::new("/usr/bin/security").args(["find-identity", "-v", "-p", "codesigning"]),
        None,
    )?;
    Ok(parse_identities(&String::from_utf8_lossy(&out.stdout)))
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, dir: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for item in fs::read_dir(dir)? {
            let path = item?.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_symlink() {
                ensure!(
                    fs::canonicalize(&path)?.starts_with(root),
                    "应用内符号链接越界：{}",
                    path.display()
                );
            } else if meta.is_dir() {
                visit(root, &path, paths)?;
            } else {
                ensure!(meta.is_file(), "应用中存在非常规文件：{}", path.display());
            }
            paths.push(path);
        }
        Ok(())
    }
    let canonical = fs::canonicalize(root)?;
    let mut paths = Vec::new();
    visit(&canonical, root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn fingerprint(root: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65536];
    for path in walk(root)? {
        let meta = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root)?.as_os_str().as_encoded_bytes();
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative);
        digest.update(meta.permissions().mode().to_le_bytes());
        if meta.is_symlink() {
            digest.update(b"link");
            let target = fs::read_link(path)?;
            let bytes = target.as_os_str().as_encoded_bytes();
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        } else if meta.is_dir() {
            digest.update(b"dir");
        } else {
            digest.update(b"file");
            digest.update(meta.len().to_le_bytes());
            let mut file = File::open(path)?;
            let mut contents = Sha256::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                contents.update(&buf[..n]);
            }
            digest.update(contents.finalize());
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn plist_at(app: &Path) -> Result<plist::Value> {
    let path = app.join("Contents/Info.plist");
    ensure!(
        !fs::symlink_metadata(&path)?.is_symlink(),
        "Info.plist 不允许是符号链接"
    );
    plist::Value::from_file(path).context("读取应用 Info.plist")
}

fn inspect(app: &Path) -> Result<(String, asar::PatchState)> {
    ensure!(
        !fs::symlink_metadata(app)?.is_symlink(),
        "不允许修改应用符号链接"
    );
    let info = plist_at(app)?;
    let dict = info.as_dictionary().context("Info.plist 不是字典")?;
    ensure!(
        dict.get("CFBundleIdentifier")
            .and_then(plist::Value::as_string)
            == Some(BUNDLE_ID),
        "目标不是 Claude Desktop"
    );
    let version = dict
        .get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .context("缺少 Desktop 版本")?;
    asar::check_version(version)?;
    let archive = app.join("Contents/Resources/app.asar");
    ensure!(
        !fs::symlink_metadata(&archive)?.is_symlink(),
        "app.asar 不允许是符号链接"
    );
    let (state, header_hash) = asar::inspect(&archive, version)?;
    let selected = asar::active_selection(&archive, version)?;
    renderer::inspect(
        app,
        version,
        if selected.computer_use {
            &state
        } else {
            &asar::PatchState::Pristine
        },
    )?;
    super::native::language::installed(app, version)?;
    super::fast_status::installed(app, version)?;
    let integrity = dict
        .get("ElectronAsarIntegrity")
        .and_then(plist::Value::as_dictionary)
        .and_then(|d| d.get("Resources/app.asar"))
        .and_then(plist::Value::as_dictionary)
        .context("缺少 Electron ASAR 完整性记录")?;
    ensure!(
        integrity.get("algorithm").and_then(plist::Value::as_string) == Some("SHA256")
            && integrity.get("hash").and_then(plist::Value::as_string)
                == Some(header_hash.as_str()),
        "Electron ASAR 文件头哈希不匹配"
    );
    Ok((version.into(), state))
}

fn verify_signature(app: &Path, log: Option<&Path>) -> Result<()> {
    execute(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
        log,
    )?;
    Ok(())
}

fn verify_original(app: &Path, log: Option<&Path>) -> Result<String> {
    let (version, state) = inspect(app)?;
    ensure!(
        state == asar::PatchState::Pristine,
        "需要未修改的官方 Desktop"
    );
    verify_signature(app, log)?;
    execute(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(format!("-R={VENDOR_REQUIREMENT}"))
            .arg(app),
        log,
    )?;
    Ok(version)
}

fn running(app: &Path) -> Result<bool> {
    let out = execute(Command::new("/bin/ps").args(["-axo", "comm="]), None)?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let path = Path::new(line.trim());
        path.starts_with(app) || fs::canonicalize(path).is_ok_and(|p| p.starts_with(app))
    }))
}

fn ensure_stopped(app: &Path) -> Result<()> {
    ensure!(
        !running(app)?,
        "目标 Claude Desktop 正在运行，请先退出再操作"
    );
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        out.set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut out, value)?;
        out.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(path.parent().context("记录缺少父目录")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn clean_entitlements(mut value: plist::Value) -> Result<plist::Value> {
    let dict = value.as_dictionary_mut().context("entitlements 不是字典")?;
    for key in IDENTITY_KEYS {
        dict.remove(key);
    }
    Ok(value)
}

fn is_macho(path: &Path) -> Result<bool> {
    let mut magic = [0; 4];
    let n = File::open(path)?.read(&mut magic)?;
    Ok(n == 4
        && matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        ))
}

fn signing_workspace(transaction: &Path) -> Result<PathBuf> {
    let path = transaction.join(format!("entitlements-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&path)?;
    Ok(path)
}

fn sign(app: &Path, identity: &str, transaction: &Path) -> Result<()> {
    let log = transaction.join("commands.jsonl");
    let paths = walk(app)?;
    let mut files = Vec::new();
    let mut bundles = Vec::new();
    for path in &paths {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_symlink() {
            continue;
        }
        if meta.is_file() && is_macho(path)? {
            files.push(path.clone());
        }
        if meta.is_dir()
            && matches!(
                path.extension().and_then(OsStr::to_str),
                Some("app" | "framework" | "xpc")
            )
        {
            bundles.push(path.clone());
        }
    }
    files.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    bundles.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    files.extend(bundles);
    files.push(app.to_path_buf());
    let entitlements = signing_workspace(transaction)?;
    let mut plans = Vec::new();
    for (i, target) in files.iter().enumerate() {
        let out = execute(
            Command::new("/usr/bin/codesign")
                .args(["-d", "--entitlements", ":-"])
                .arg(target),
            Some(&log),
        )?;
        let value = if out.stdout.is_empty() {
            plist::Value::Dictionary(plist::Dictionary::new())
        } else {
            plist::Value::from_reader(std::io::Cursor::new(out.stdout))
                .context("解析原始 entitlements")?
        };
        let path = entitlements.join(format!("{i:03}.plist"));
        clean_entitlements(value)?.to_file_xml(&path)?;
        plans.push((target, path));
    }
    for path in &paths {
        if path.file_name() == Some(OsStr::new("embedded.provisionprofile")) {
            ensure!(
                fs::symlink_metadata(path)?.is_file(),
                "签名 profile 类型异常"
            );
            fs::remove_file(path)?;
        }
    }
    for (target, entitlements) in plans {
        execute(
            Command::new("/usr/bin/codesign")
                .args([
                    "--force",
                    "--sign",
                    identity,
                    "--timestamp=none",
                    "--options",
                    "runtime",
                    "--entitlements",
                ])
                .arg(entitlements)
                .arg(target),
            Some(&log),
        )?;
    }
    verify_signature(app, Some(&log))
}

pub(super) fn selection_at(app: &Path) -> Result<super::native::Selection> {
    let info = plist_at(app)?;
    let version = info
        .as_dictionary()
        .and_then(|d| d.get("CFBundleShortVersionString"))
        .and_then(plist::Value::as_string)
        .context("缺少 Desktop 版本")?;
    let mut selected = asar::active_selection(&app.join("Contents/Resources/app.asar"), version)?;
    selected.chinese = super::native::language::installed(app, version)?;
    selected.fast_mode_status = super::fast_status::installed(app, version)?;
    Ok(selected)
}

fn patch(app: &Path, selected: super::native::Selection) -> Result<()> {
    let (version, _) = inspect(app)?;
    super::native::check_selection(&version, selected)?;
    let hash = asar::patch_features(&app.join("Contents/Resources/app.asar"), &version, selected)?;
    if selected.computer_use {
        renderer::patch(app, &version)?;
    }
    if selected.chinese {
        super::native::language::patch(app)?;
    }
    if selected.fast_mode_status {
        super::fast_status::patch(app)?;
    }
    let mut info = plist_at(app)?;
    let entry = info
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("ElectronAsarIntegrity"))
        .and_then(plist::Value::as_dictionary_mut)
        .and_then(|d| d.get_mut("Resources/app.asar"))
        .and_then(plist::Value::as_dictionary_mut)
        .context("缺少 Electron ASAR 完整性记录")?;
    entry.insert("hash".into(), plist::Value::String(hash));
    info.to_file_xml(app.join("Contents/Info.plist"))?;
    inspect(app)?;
    ensure!(selection_at(app)? == selected, "原生功能组合回读验证失败");
    Ok(())
}

fn swap(stage: &Path, target: &Path, retired: &Path) -> Result<()> {
    ensure!(
        !stage.is_symlink() && !target.is_symlink() && !retired.is_symlink(),
        "替换路径不允许是符号链接"
    );
    ensure!(!retired.try_exists()?, "替换暂存路径已存在");
    let had_target = target.try_exists()?;
    if had_target {
        fs::rename(target, retired).context("暂存原应用；请检查目标目录写权限")?;
    }
    if let Err(error) = fs::rename(stage, target) {
        if had_target {
            fs::rename(retired, target).context("替换失败，自动恢复也失败；请使用原包备份恢复")?;
        }
        return Err(error).context("替换失败，原应用已保留");
    }
    Ok(())
}

impl Installation {
    fn standard() -> Result<Self> {
        Ok(Self {
            app: PathBuf::from(APP),
            store: dirs::config_dir()
                .context("找不到配置目录")?
                .join("com.claudeplusplus.manager/computer-use"),
        })
    }

    fn copy(app: &Path, store: &Path) -> Result<Self> {
        ensure!(!app.is_symlink(), "不接受应用符号链接");
        let app = fs::canonicalize(app.parent().context("应用缺少父路径")?)?
            .join(app.file_name().context("应用路径无效")?);
        ensure!(
            !app.starts_with("/Applications")
                && !app.starts_with("/System")
                && !app.starts_with("/Library")
                && app.file_name() == Some(OsStr::new("Claude.app")),
            "探针只允许操作非系统目录里的 Claude.app 副本"
        );
        let store = if store.exists() {
            fs::canonicalize(store)?
        } else {
            fs::canonicalize(store.parent().context("记录目录缺少父路径")?)?
                .join(store.file_name().context("记录目录无效")?)
        };
        ensure!(
            !store.starts_with(&app) && !app.starts_with(&store),
            "应用与记录目录不能相互包含"
        );
        Ok(Self { app, store })
    }

    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.store)?;
        ensure!(
            !fs::symlink_metadata(&self.store)?.is_symlink(),
            "记录目录不允许是符号链接"
        );
        fs::set_permissions(&self.store, fs::Permissions::from_mode(0o700))?;
        let path = self.store.join("operation.lock");
        ensure!(!path.is_symlink(), "操作锁不允许是符号链接");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .map_err(|e| anyhow::anyhow!("另一个 Computer Use 操作正在进行：{e}"))?;
        Ok(file)
    }

    fn record(&self) -> Result<Option<Record>> {
        let path = self.store.join("installation.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        ensure!(!path.is_symlink(), "安装记录不允许是符号链接");
        let r: Record =
            serde_json::from_slice(&fs::read(path)?).context("安装记录损坏，拒绝覆盖备份")?;
        ensure!(
            (r.schema == 1 || r.schema == 2) && r.target == self.app,
            "安装记录与目标不匹配"
        );
        Ok(Some(r))
    }

    fn transaction(&self, record: &Record) -> PathBuf {
        self.store.join(record.id.to_string())
    }
    fn baseline(&self, record: &Record) -> PathBuf {
        self.transaction(record).join("baseline/Claude.app")
    }
    fn save(&self, record: &Record) -> Result<()> {
        atomic_json(&self.store.join("installation.json"), record)
    }
    fn sibling(&self, id: uuid::Uuid, suffix: &str) -> Result<PathBuf> {
        Ok(self
            .app
            .parent()
            .context("目标缺少父目录")?
            .join(format!(".claudeplusplus-cu-{id}.{suffix}")))
    }

    fn clear_staging(&self, record: &Record) -> Result<()> {
        for suffix in ["app", "previous"] {
            let path = self.sibling(record.id, suffix)?;
            ensure!(!path.is_symlink(), "暂存目录被替换为符号链接，拒绝清理");
            if path.try_exists()? {
                ensure!(!running(&path)?, "暂存应用正在运行，请先退出后重试还原");
                fs::remove_dir_all(&path).with_context(|| format!("清理 {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn status(&self) -> Result<Status> {
        let mut status = Status {
            supported: true,
            requires_signing: true,
            app_path: self.app.display().to_string(),
            desktop_version: None,
            state: "notFound".into(),
            message: "未找到 Claude Desktop".into(),
            identities: identities()?,
            backup_path: None,
            running: running(&self.app)?,
            can_install: false,
            can_restore: false,
        };
        let record = self.record()?;
        if let Some(r) = &record {
            status.backup_path = Some(self.baseline(r).display().to_string());
        }
        if !self.app.try_exists()? {
            if let Some(r) = &record {
                status.state = "recovery".into();
                status.message = "应用路径缺失；可从已记录的原包备份恢复".into();
                status.can_restore = self.baseline(r).is_dir() && !status.running;
            }
            return Ok(status);
        }
        status.desktop_version = plist_at(&self.app).ok().and_then(|p| {
            p.as_dictionary()
                .and_then(|d| d.get("CFBundleShortVersionString"))
                .and_then(plist::Value::as_string)
                .map(str::to_owned)
        });
        let current = if record.is_some() {
            Some(fingerprint(&self.app)?)
        } else {
            None
        };
        if let Some(r) = &record {
            let baseline = current.as_deref() == Some(r.baseline_hash.as_str());
            let modified = r.modified_hash.is_some() && current == r.modified_hash;
            let previous = r.phase != "installed"
                && r.previous_modified_hash.is_some()
                && current == r.previous_modified_hash;
            if r.phase != "restored" {
                status.can_restore = (baseline || modified || previous)
                    && self.baseline(r).is_dir()
                    && !status.running;
                if !baseline && !modified && !previous {
                    status.state = "changed".into();
                    status.message = "Desktop 已被外部更新或修改；为避免覆盖新版本，停止安装和还原，原包备份仍保留".into();
                    return Ok(status);
                }
                if r.phase != "installed" {
                    status.state = "recovery".into();
                    status.message = "上次安装未完成，请先还原以结束未完成事务".into();
                    return Ok(status);
                }
                if modified {
                    let (_, patch_state) = inspect(&self.app)?;
                    verify_signature(&self.app, None)?;
                    if patch_state == asar::PatchState::Legacy {
                        status.state = "upgradeAvailable".into();
                        status.message = "已安装旧版原生入口；升级将先验证备份并还原官方，再安装包含后台模式的补丁".into();
                        status.can_install = !status.running && !status.identities.is_empty();
                    } else {
                        status.state = "installed".into();
                        status.message =
                            "原生增强已安装；功能开关与设备授权仍由 Claude 原生界面管理".into();
                        status.can_install = !status.running && !status.identities.is_empty();
                    }
                    return Ok(status);
                }
            }
        }
        match inspect(&self.app) {
            Ok((_, asar::PatchState::Pristine)) => {
                verify_original(&self.app, None)?;
                status.state = "available".into();
                status.message = "可安装原生电脑操作；安装后仍需自行启用并授权".into();
                status.can_install = !status.running && !status.identities.is_empty();
            }
            Ok((_, asar::PatchState::Patched | asar::PatchState::Legacy)) => {
                status.state = "unmanaged".into();
                status.message =
                    "检测到已有补丁，但没有匹配的安装记录；不会将修改版当成官方备份".into();
            }
            Err(error) => {
                status.state = "unsupported".into();
                status.message = format!("{error:#}");
            }
        }
        Ok(status)
    }

    fn install(&self, identity: &str) -> Result<String> {
        let selected = selection_at(&self.app)?;
        self.apply(
            identity,
            super::native::Selection {
                computer_use: true,
                ..selected
            },
        )
    }

    fn apply(&self, identity: &str, selected: super::native::Selection) -> Result<String> {
        let _lock = self.lock()?;
        ensure_stopped(&self.app)?;
        if selected.is_empty() {
            return self.restore_locked();
        }
        validate_identity(identity, &identities()?)?;
        let before = fingerprint(&self.app)?;
        let old = self.record()?;
        if let Some(r) = &old {
            ensure!(
                r.phase == "installed" || r.phase == "restored",
                "存在未完成事务，请先还原"
            );
            if r.phase == "installed" {
                ensure!(
                    r.modified_hash.as_deref() == Some(&before) || before == r.baseline_hash,
                    "应用已被外部修改，拒绝覆盖"
                );
                if r.modified_hash.as_deref() == Some(&before)
                    && selection_at(&self.app)? == selected
                    && inspect(&self.app)?.1 != asar::PatchState::Legacy
                {
                    verify_signature(&self.app, None)?;
                    return Ok("所选原生功能已安装，无需重复修改".into());
                }
            }
        }
        let mut record = if let Some(r) = old
            .as_ref()
            .filter(|r| r.phase == "installed" || r.baseline_hash == before)
        {
            let baseline = self.baseline(r);
            ensure!(
                fingerprint(&baseline)? == r.baseline_hash,
                "原包备份哈希不一致"
            );
            verify_original(&baseline, None)?;
            r.clone()
        } else {
            let version = verify_original(&self.app, None)?;
            let r = Record {
                schema: 2,
                id: uuid::Uuid::new_v4(),
                target: self.app.clone(),
                version,
                identity: identity.into(),
                baseline_hash: before.clone(),
                modified_hash: None,
                phase: "restored".into(),
                selection: None,
                previous_modified_hash: None,
            };
            let transaction = self.transaction(&r);
            fs::create_dir(&transaction)?;
            fs::create_dir(transaction.join("baseline"))?;
            execute(
                Command::new("/usr/bin/ditto")
                    .arg(&self.app)
                    .arg(self.baseline(&r)),
                Some(&transaction.join("commands.jsonl")),
            )?;
            ensure!(
                fingerprint(&self.baseline(&r))? == before,
                "原包备份哈希不一致"
            );
            verify_original(&self.baseline(&r), None)?;
            r
        };
        super::native::check_selection(&record.version, selected)?;
        self.clear_staging(&record)?;
        let transaction = self.transaction(&record);
        let stage = self.sibling(record.id, "app")?;
        let retired = self.sibling(record.id, "previous")?;
        let log = transaction.join("commands.jsonl");
        record.schema = 2;
        record.identity = identity.into();
        record.selection = Some(selected);
        record.previous_modified_hash = Some(before.clone());
        record.phase = "preparing".into();
        self.save(&record)?;
        let result = (|| -> Result<()> {
            execute(
                Command::new("/usr/bin/ditto")
                    .arg(self.baseline(&record))
                    .arg(&stage),
                Some(&log),
            )?;
            patch(&stage, selected)?;
            sign(&stage, identity, &transaction)?;
            record.modified_hash = Some(fingerprint(&stage)?);
            record.phase = "ready".into();
            self.save(&record)?;
            ensure_stopped(&self.app)?;
            ensure!(
                fingerprint(&self.app)? == before,
                "生成补丁期间应用发生变化，停止替换"
            );
            swap(&stage, &self.app, &retired)?;
            if let Err(error) = verify_signature(&self.app, Some(&log)) {
                fs::rename(&self.app, &stage)?;
                fs::rename(&retired, &self.app)?;
                return Err(error).context("签名校验失败，已恢复此前应用");
            }
            record.phase = "installed".into();
            record.previous_modified_hash = None;
            self.save(&record)?;
            fs::remove_dir_all(&retired).context("安装完成，但旧暂存应用清理失败")?;
            Ok(())
        })();
        if let Err(error) = result {
            if fingerprint(&self.app).ok().as_deref() == Some(&before) {
                self.clear_staging(&record)?;
                if let Some(previous) = old {
                    self.save(&previous)?;
                } else {
                    record.phase = "restored".into();
                    record.modified_hash = None;
                    record.previous_modified_hash = None;
                    record.selection = None;
                    self.save(&record)?;
                }
            }
            return Err(error);
        }
        Ok("所选原生功能已安装并通过完整性验证。请在 Claude 原生设置中选择语言或授权设备；未自动授予任何权限".into())
    }

    fn restore(&self) -> Result<String> {
        let _lock = self.lock()?;
        self.restore_locked()
    }

    fn restore_locked(&self) -> Result<String> {
        ensure_stopped(&self.app)?;
        let mut record = self.record()?.context("没有可还原的安装记录")?;
        let baseline = self.baseline(&record);
        ensure!(
            fingerprint(&baseline)? == record.baseline_hash,
            "原包备份哈希不一致，拒绝还原"
        );
        verify_original(&baseline, None)?;
        if self.app.try_exists()? {
            let current = fingerprint(&self.app)?;
            if current == record.baseline_hash {
                self.clear_staging(&record)?;
                record.phase = "restored".into();
                self.save(&record)?;
                return Ok("应用已经是备份中的原版；安装记录已结束，原包备份保留".into());
            }
            ensure!(
                record.modified_hash.as_deref() == Some(current.as_str())
                    || record.phase != "installed"
                        && record.previous_modified_hash.as_deref() == Some(current.as_str()),
                "应用已被外部更新或修改，拒绝用旧备份覆盖；请检查备份路径"
            );
        }
        let id = uuid::Uuid::new_v4();
        let stage = self.sibling(id, "app")?;
        let retired = self.sibling(id, "previous")?;
        let log = self.transaction(&record).join("commands.jsonl");
        execute(
            Command::new("/usr/bin/ditto").arg(&baseline).arg(&stage),
            Some(&log),
        )?;
        ensure!(
            fingerprint(&stage)? == record.baseline_hash,
            "恢复副本哈希不一致"
        );
        verify_original(&stage, Some(&log))?;
        ensure_stopped(&self.app)?;
        if self.app.try_exists()? {
            ensure!(
                record.modified_hash.as_deref() == Some(fingerprint(&self.app)?.as_str())
                    || record.phase != "installed"
                        && record.previous_modified_hash.as_deref()
                            == Some(fingerprint(&self.app)?.as_str()),
                "还原期间应用发生变化"
            );
        }
        record.phase = "restoring".into();
        self.save(&record)?;
        swap(&stage, &self.app, &retired)?;
        ensure!(
            fingerprint(&self.app)? == record.baseline_hash,
            "还原后哈希不一致，备份仍保留"
        );
        self.clear_staging(&record)?;
        record.phase = "restored".into();
        self.save(&record)?;
        if retired.exists() {
            let _ = fs::remove_dir_all(retired);
        }
        Ok("已还原完整官方应用，原包哈希验证通过；会话、API 配置和用户授权设置未修改".into())
    }
}

pub fn status() -> Result<Status> {
    Installation::standard()?.status()
}
pub fn install(identity: &str) -> Result<String> {
    Installation::standard()?.install(identity)
}
pub fn apply(identity: &str, selected: super::native::Selection) -> Result<String> {
    Installation::standard()?.apply(identity, selected)
}
pub fn apply_copy(
    app: &Path,
    store: &Path,
    identity: &str,
    selected: super::native::Selection,
) -> Result<String> {
    Installation::copy(app, store)?.apply(identity, selected)
}
pub fn restore() -> Result<String> {
    Installation::standard()?.restore()
}

pub fn status_copy(app: &Path, store: &Path) -> Result<Status> {
    Installation::copy(app, store)?.status()
}
pub fn install_copy(app: &Path, store: &Path, identity: &str) -> Result<String> {
    Installation::copy(app, store)?.install(identity)
}
pub fn restore_copy(app: &Path, store: &Path) -> Result<String> {
    Installation::copy(app, store)?.restore()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combination_updates_keep_independent_signing_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let first = signing_workspace(dir.path()).unwrap();
        let second = signing_workspace(dir.path()).unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn signing_preserves_capabilities_but_not_vendor_identity() {
        let mut dict = plist::Dictionary::new();
        for key in IDENTITY_KEYS {
            dict.insert((*key).into(), plist::Value::String("vendor".into()));
        }
        for key in [
            "com.apple.security.virtualization",
            "com.apple.security.cs.allow-jit",
        ] {
            dict.insert(key.into(), plist::Value::Boolean(true));
        }
        let clean = clean_entitlements(plist::Value::Dictionary(dict)).unwrap();
        let clean = clean.as_dictionary().unwrap();
        assert_eq!(clean.len(), 2);
        assert_eq!(
            clean["com.apple.security.virtualization"].as_boolean(),
            Some(true)
        );
        assert!(clean_entitlements(plist::Value::String("bad".into())).is_err());
    }

    #[test]
    fn replacement_failure_restores_the_original_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Claude.app");
        let retired = dir.path().join("old");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("original"), "original").unwrap();
        assert!(swap(&dir.path().join("missing"), &target, &retired).is_err());
        assert_eq!(
            fs::read_to_string(target.join("original")).unwrap(),
            "original"
        );
        assert!(!retired.exists());
    }

    #[test]
    fn replacement_refuses_symlinks_and_occupied_recovery_paths() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Claude.app");
        let stage = dir.path().join("stage");
        let retired = dir.path().join("old");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&stage).unwrap();
        std::os::unix::fs::symlink("missing", &retired).unwrap();
        assert!(swap(&stage, &target, &retired).is_err());
        fs::remove_file(&retired).unwrap();
        fs::create_dir(&retired).unwrap();
        assert!(swap(&stage, &target, &retired).is_err());
        assert!(stage.is_dir() && target.is_dir());
    }

    #[test]
    fn manifests_detect_content_mode_and_link_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "a").unwrap();
        let original = fingerprint(dir.path()).unwrap();
        fs::write(&path, "b").unwrap();
        assert_ne!(fingerprint(dir.path()).unwrap(), original);
        fs::write(&path, "a").unwrap();
        assert_eq!(fingerprint(dir.path()).unwrap(), original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_ne!(fingerprint(dir.path()).unwrap(), original);
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
        assert!(fingerprint(dir.path()).is_err());
    }

    #[test]
    fn operation_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let installation = Installation {
            app: dir.path().join("Claude.app"),
            store: dir.path().join("store"),
        };
        let first = installation.lock().unwrap();
        assert!(installation.lock().is_err());
        drop(first);
        assert!(installation.lock().is_ok());
    }

    #[test]
    fn malformed_records_are_not_treated_as_uninstalled() {
        let dir = tempfile::tempdir().unwrap();
        let installation = Installation {
            app: dir.path().join("Claude.app"),
            store: dir.path().to_path_buf(),
        };
        fs::write(dir.path().join("installation.json"), "not json").unwrap();
        assert!(installation.record().is_err());
    }

    #[test]
    fn records_are_bound_to_the_exact_application() {
        let dir = tempfile::tempdir().unwrap();
        let installation = Installation {
            app: dir.path().join("Claude.app"),
            store: dir.path().to_path_buf(),
        };
        let mut record = Record {
            schema: 1,
            id: uuid::Uuid::new_v4(),
            target: installation.app.clone(),
            version: "3.1623.1".into(),
            identity: "A".repeat(40),
            baseline_hash: "baseline".into(),
            modified_hash: None,
            phase: "preparing".into(),
            selection: None,
            previous_modified_hash: None,
        };
        installation.save(&record).unwrap();
        assert!(installation.record().unwrap().is_some());
        record.target = dir.path().join("different.app");
        installation.save(&record).unwrap();
        assert!(installation.record().is_err());
        record.target = installation.app.clone();
        record.schema = 2;
        installation.save(&record).unwrap();
        assert!(installation.record().unwrap().is_some());
        record.schema = 3;
        installation.save(&record).unwrap();
        assert!(installation.record().is_err());
    }
}

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    bun, now_iso, plan_edits, smoke_test, write_edits, Action, InstallStatus, Needle, Status,
    MAX_EXE, NEEDLES,
};

#[derive(Clone)]
struct Installation {
    id: String,
    kind: String,
    binary: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
struct Record {
    schema: u32,
    binary: PathBuf,
    backup: PathBuf,
    original: String,
    modified: String,
    previous: String,
    original_unit: String,
    modified_unit: String,
    phase: String,
    version: String,
    applied_at: String,
    hits: BTreeMap<String, usize>,
}

#[derive(Deserialize)]
struct Legacy {
    schema: u32,
    binary: PathBuf,
    backup: PathBuf,
    original: String,
    modified: String,
    phase: String,
}

fn root() -> PathBuf {
    crate::fastmode::settings_dir().join("promptblock-macos")
}

fn hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn unit(binary: &Path) -> PathBuf {
    if binary
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "MacOS")
    {
        if let Some(contents) = binary.parent().and_then(Path::parent) {
            if contents.file_name().is_some_and(|n| n == "Contents") {
                if let Some(bundle) = contents
                    .parent()
                    .filter(|p| p.extension().is_some_and(|s| s == "app"))
                {
                    return bundle.to_path_buf();
                }
            }
        }
    }
    binary.to_path_buf()
}

fn unit_hash(path: &Path) -> Result<String> {
    if path.is_dir() {
        crate::computer_use::fingerprint_bundle(path)
    } else {
        hash(path)
    }
}

fn binary_in(copy: &Path, binary: &Path) -> Result<PathBuf> {
    let original = unit(binary);
    Ok(if original == binary {
        copy.to_path_buf()
    } else {
        copy.join(binary.strip_prefix(original)?)
    })
}

fn command(command: &mut Command) -> Result<()> {
    let output = command.output().context("执行 macOS 文件操作")?;
    ensure!(
        output.status.success(),
        "{}：{}",
        command.get_program().to_string_lossy(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn verify(path: &Path) -> Result<()> {
    command(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(path),
    )
}

fn copy(source: &Path, target: &Path) -> Result<()> {
    ensure!(!target.try_exists()?, "暂存目标已存在");
    command(Command::new("/usr/bin/ditto").arg(source).arg(target))
}

fn swap(left: &Path, right: &Path) -> Result<()> {
    unsafe extern "C" {
        fn renameatx_np(fd1: i32, path1: *const i8, fd2: i32, path2: *const i8, flags: u32) -> i32;
    }
    let left = CString::new(left.as_os_str().as_bytes())?;
    let right = CString::new(right.as_os_str().as_bytes())?;
    let result = unsafe { renameatx_np(-2, left.as_ptr(), -2, right.as_ptr(), 2) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("原子替换 CLI");
    }
    Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
    fn near(path: &Path) -> Result<Self> {
        let folder = path
            .parent()
            .context("CLI 缺少父目录")?
            .join(format!(".cpp-prompts-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder)?;
        Ok(Self(folder))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn store_path(store: &Path, binary: &Path) -> PathBuf {
    store.join(format!(
        "{:x}.json",
        Sha256::digest(binary.as_os_str().as_bytes())
    ))
}

fn read_record(store: &Path, binary: &Path) -> Result<Option<Record>> {
    let path = store_path(store, binary);
    if !path.try_exists()? {
        return Ok(None);
    }
    ensure!(!path.is_symlink(), "记录文件不允许符号链接");
    let record: Record = serde_json::from_slice(&fs::read(path)?).context("屏蔽记录损坏")?;
    ensure!(
        record.schema == 1 && record.binary == binary,
        "屏蔽记录不匹配"
    );
    ensure!(
        ["prepared", "installed", "restored"].contains(&record.phase.as_str()),
        "屏蔽记录状态无效"
    );
    Ok(Some(record))
}

fn save(store: &Path, record: &Record) -> Result<()> {
    let path = store_path(store, &record.binary);
    let temp = store.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(record)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn lock(store: &Path) -> Result<File> {
    fs::create_dir_all(store)?;
    ensure!(!store.is_symlink(), "记录目录不能是符号链接");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join("operation.lock"))?;
    file.try_lock().context("另一个提示词操作正在进行")?;
    Ok(file)
}

fn legacy(binary: &Path, current: &str) -> Result<Option<Legacy>> {
    let old = crate::fastmode::settings_dir().join("prompt-guard");
    let id = format!("{:x}", Sha256::digest(binary.as_os_str().as_bytes()));
    let directory = old.join(&id[..24]);
    let path = directory.join("installation.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let record: Legacy = serde_json::from_slice(&fs::read(path)?).context("旧 heron 记录损坏")?;
    ensure!(
        record.schema == 1 && record.binary == binary,
        "旧 heron 记录路径不匹配"
    );
    if record.phase != "installed" || record.modified != current {
        return Ok(None);
    }
    ensure!(
        record
            .backup
            .canonicalize()?
            .starts_with(directory.canonicalize()?),
        "旧 heron 备份越界"
    );
    ensure!(
        hash(&binary_in(&record.backup, binary)?)? == record.original,
        "旧 heron 备份损坏"
    );
    verify(&record.backup)?;
    Ok(Some(record))
}

fn discover() -> Vec<Installation> {
    let mut found = Vec::new();
    if let Some(data) = dirs::data_local_dir() {
        for (id, folder, kind) in [
            ("bundled", "Claude-3p", "Desktop 3p 自带 CLI"),
            ("bundled-1p", "Claude", "Desktop 自带 CLI"),
        ] {
            if let Some(cli) = crate::fastmode::latest_cli(&data.join(folder).join("claude-code")) {
                let original = cli.exe_dir.join("claude-real.app/Contents/MacOS/claude");
                let normal = cli.exe_dir.join("claude.app/Contents/MacOS/claude");
                let binary = if original.is_file() { original } else { normal };
                found.push(Installation {
                    id: id.into(),
                    kind: kind.into(),
                    binary,
                });
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        for (id, kind, binary) in [
            ("local-bin", "原生安装", home.join(".local/bin/claude")),
            (
                "homebrew",
                "Homebrew / npm 全局",
                PathBuf::from("/opt/homebrew/bin/claude"),
            ),
            (
                "usr-local",
                "全局 CLI",
                PathBuf::from("/usr/local/bin/claude"),
            ),
        ] {
            if binary.is_file() {
                found.push(Installation {
                    id: id.into(),
                    kind: kind.into(),
                    binary,
                });
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain_mut(|install| {
        if let Ok(path) = install.binary.canonicalize() {
            install.binary = path;
        }
        seen.insert(install.binary.clone())
    });
    found
}

fn contents(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.is_symlink() && meta.len() <= MAX_EXE && meta.len() > 1024 * 1024,
        "CLI 文件类型或大小异常"
    );
    let data = fs::read(path)?;
    ensure!(
        data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]),
        "需要原生 Mach-O CLI"
    );
    Ok(data)
}

fn version(data: &[u8]) -> Result<String> {
    let pattern = regex::bytes::Regex::new(r"// Version: (\d+\.\d+\.\d+)")?;
    let caps = pattern.captures(data).context("未识别 CLI 版本")?;
    Ok(std::str::from_utf8(&caps[1])?.into())
}

fn profile(version: &str) -> Vec<Needle> {
    let legacy = matches!(version, "2.1.308" | "2.1.309" | "2.1.310");
    let mut needles: Vec<_> = NEEDLES
        .iter()
        .enumerate()
        .filter(|(index, _)| !legacy || ![0, 1, 2, 4, 10, 11].contains(index))
        .map(|(_, needle)| *needle)
        .collect();
    if legacy {
        needles.push(Needle {
            segment: "fable_identity",
            label: "模型身份介绍（Fable 5.1）",
            text: "This iteration of Claude is Claude Fable 5.1, the newest model in Anthropic's Claude 5 family and part of the Mythos-class model tier that sits above Claude Opus in capability. Claude Fable 5.1 and Claude Mythos 5.1 share the same underlying model. Claude Fable 5.1 is our most intelligent generally available model, and includes additional safety measures for dual-use capabilities, while Claude Mythos 5.1 is available without those measures to only approved organizations. Fable 5.1 is the most advanced generally available Claude model. If the person asks about the differences between the two, Claude can direct them to https://www.anthropic.com/claude/fable for more information.",
            action: Action::Blank,
            min_hits: 2,
        });
    }
    needles
}

fn patched(source: &Path) -> Result<(Vec<u8>, String, BTreeMap<String, usize>)> {
    let mut bytes = contents(source)?;
    let version = version(&bytes)?;
    let (edits, hits) = plan_edits(&bytes, &profile(&version))?;
    write_edits(&mut bytes, &edits, false);
    bun::refresh_snapshots(&mut bytes)?;
    Ok((bytes, version, hits))
}

fn checked_backup(store: &Path, record: &Record) -> Result<PathBuf> {
    ensure!(
        record
            .backup
            .canonicalize()?
            .starts_with(store.canonicalize()?),
        "备份目录越界"
    );
    let binary = binary_in(&record.backup, &record.binary)?;
    ensure!(hash(&binary)? == record.original, "官方备份哈希不匹配");
    verify(&record.backup)?;
    ensure!(
        unit_hash(&record.backup)? == record.original_unit,
        "官方应用备份已被修改"
    );
    Ok(binary)
}

fn describe(install: &Installation, store: &Path) -> Result<InstallStatus> {
    let current = hash(&install.binary)?;
    let record = read_record(store, &install.binary)?;
    let mut status = InstallStatus {
        id: install.id.clone(),
        kind: install.kind.clone(),
        exe_path: install.binary.display().to_string(),
        cli_version: None,
        state: "pristine",
        applied_at: None,
        segment_hits: BTreeMap::new(),
        note: None,
    };
    if let Some(record) = record
        .filter(|r| r.phase != "restored" && !(r.phase == "prepared" && current == r.previous))
    {
        status.cli_version = Some(record.version.clone());
        status.applied_at = Some(record.applied_at.clone());
        status.segment_hits = record.hits.clone();
        if current == record.modified {
            checked_backup(store, &record)?;
            verify(&unit(&install.binary))?;
            ensure!(
                unit_hash(&unit(&install.binary))? == record.modified_unit,
                "CLI 应用已被外部修改"
            );
            status.state = "patched";
        } else if current == record.original {
            status.state = "pristine";
            status.applied_at = None;
        } else {
            status.state = "updated";
            status.applied_at = None;
            status.note = Some("文件已有变化，旧备份仍保留；应用前会重新验证".into());
        }
        return Ok(status);
    }
    let old = legacy(&install.binary, &current)?;
    let source = match &old {
        Some(r) => binary_in(&r.backup, &install.binary)?,
        None => install.binary.clone(),
    };
    let bytes = contents(&source)?;
    status.cli_version = Some(version(&bytes)?);
    match plan_edits(
        &bytes,
        &profile(status.cli_version.as_deref().unwrap_or_default()),
    ) {
        Ok((_, hits)) => {
            status.segment_hits = hits;
            if old.is_some() {
                status.state = "legacy";
                status.applied_at = Some("已有 heron 屏蔽".into());
                status.note = Some("可从已验证的官方备份升级为完整屏蔽，也可还原".into());
            }
        }
        Err(error) => {
            status.state = "unsupported";
            status.note = Some(format!("{error:#}"));
        }
    }
    Ok(status)
}

pub fn status() -> Status {
    let installs = discover()
        .iter()
        .map(|install| {
            describe(install, &root()).unwrap_or_else(|e| InstallStatus {
                id: install.id.clone(),
                kind: install.kind.clone(),
                exe_path: install.binary.display().to_string(),
                cli_version: None,
                state: "damaged",
                applied_at: None,
                segment_hits: BTreeMap::new(),
                note: Some(format!("{e:#}")),
            })
        })
        .collect();
    Status {
        supported: true,
        platform: "macos",
        installs,
    }
}

fn find(id: &str) -> Result<Installation> {
    discover()
        .into_iter()
        .find(|i| i.id == id)
        .context("未找到 CLI 安装")
}

fn ensure_idle(installation: &Installation) -> Result<()> {
    if installation.id.starts_with("bundled") {
        ensure!(
            !crate::procs::any_desktop_running(),
            "请先退出 Desktop 再修改自带 CLI"
        );
    }
    Ok(())
}

fn install_at(installation: &Installation, store: &Path) -> Result<String> {
    let _lock = lock(store)?;
    let binary = &installation.binary;
    let current = hash(binary)?;
    let unit = unit(binary);
    let before_unit = unit_hash(&unit)?;
    let previous = read_record(store, binary)?;
    if let Some(record) = &previous {
        if record.modified == current && record.phase != "restored" {
            checked_backup(store, record)?;
            verify(&unit)?;
            ensure!(before_unit == record.modified_unit, "CLI 应用已被外部修改");
            return Ok("这些提示词已经屏蔽".into());
        }
    }
    let old = legacy(binary, &current)?;
    verify(&unit)?;
    let source = old.as_ref().map(|r| r.backup.as_path()).unwrap_or(&unit);
    let source_binary = binary_in(source, binary)?;
    let (bytes, version, hits) = patched(&source_binary)?;
    let transaction = store.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&transaction)?;
    let backup = transaction.join(unit.file_name().context("CLI 名称缺失")?);
    copy(source, &backup)?;
    let original = hash(&binary_in(&backup, binary)?)?;
    ensure!(original == hash(&source_binary)?, "备份回读不一致");
    verify(&backup)?;
    let scratch = Scratch::near(&unit)?;
    let stage = scratch.0.join(unit.file_name().context("CLI 名称缺失")?);
    copy(&backup, &stage)?;
    let staged_binary = binary_in(&stage, binary)?;
    fs::write(&staged_binary, bytes)?;
    command(
        Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--preserve-metadata=entitlements,flags,runtime",
            ])
            .arg(&stage),
    )?;
    verify(&stage)?;
    ensure!(
        smoke_test(&staged_binary)? == format!("{version} (Claude Code)"),
        "CLI 启动版本不匹配"
    );
    let mut record = Record {
        schema: 1,
        binary: binary.clone(),
        backup,
        original,
        modified: hash(&staged_binary)?,
        previous: current.clone(),
        original_unit: unit_hash(source)?,
        modified_unit: unit_hash(&stage)?,
        phase: "prepared".into(),
        version,
        applied_at: now_iso(),
        hits,
    };
    ensure_idle(installation)?;
    ensure!(hash(binary)? == current, "CLI 在操作期间被更新");
    ensure!(unit_hash(&unit)? == before_unit, "CLI 应用在操作期间被更新");
    save(store, &record)?;
    if let Err(error) = swap(&stage, &unit) {
        if let Some(previous) = &previous {
            save(store, previous)?;
        } else {
            fs::remove_file(store_path(store, binary))?;
        }
        return Err(error);
    }
    if let Err(error) = verify(&unit).and_then(|_| {
        ensure!(hash(binary)? == record.modified, "替换结果回读不一致");
        Ok(())
    }) {
        swap(&stage, &unit)?;
        if let Some(previous) = previous {
            save(store, &previous)?;
        } else {
            fs::remove_file(store_path(store, binary))?;
        }
        return Err(error);
    }
    record.phase = "installed".into();
    save(store, &record)?;
    Ok(format!(
        "已屏蔽 {} 类提示词，签名与 CLI 自检通过。重新启动会话后生效",
        record.hits.len()
    ))
}

pub fn apply(id: &str) -> Result<String> {
    let installation = find(id)?;
    ensure_idle(&installation)?;
    install_at(&installation, &root())
}

fn restore_at(installation: &Installation, store: &Path) -> Result<String> {
    let _lock = lock(store)?;
    let binary = &installation.binary;
    let current = hash(binary)?;
    let before_unit = unit_hash(&unit(binary))?;
    let record =
        read_record(store, binary)?.filter(|r| !(r.phase == "prepared" && current == r.previous));
    let old = if record.is_none() {
        legacy(binary, &current)?
    } else {
        None
    };
    let (backup, original) = if let Some(record) = &record {
        if current == record.original {
            return Ok("已经是原始文件".into());
        }
        ensure!(
            current == record.modified,
            "CLI 已被外部修改，拒绝覆盖；原始备份仍保留"
        );
        ensure!(
            before_unit == record.modified_unit,
            "CLI 应用已被外部修改，拒绝还原覆盖"
        );
        checked_backup(store, record)?;
        (record.backup.clone(), record.original.clone())
    } else if let Some(old) = old {
        (old.backup, old.original)
    } else {
        bail!("没有可还原的记录");
    };
    let unit = unit(binary);
    verify(&unit)?;
    let scratch = Scratch::near(&unit)?;
    let stage = scratch.0.join(unit.file_name().context("CLI 名称缺失")?);
    copy(&backup, &stage)?;
    verify(&stage)?;
    ensure!(
        hash(&binary_in(&stage, binary)?)? == original,
        "备份内容不匹配"
    );
    ensure_idle(installation)?;
    ensure!(hash(binary)? == current, "CLI 在还原期间被更新");
    ensure!(unit_hash(&unit)? == before_unit, "CLI 应用在还原期间被更新");
    swap(&stage, &unit)?;
    if let Err(error) = verify(&unit).and_then(|_| {
        ensure!(hash(binary)? == original, "还原结果不匹配");
        Ok(())
    }) {
        swap(&stage, &unit)?;
        return Err(error);
    }
    if let Some(mut record) = record {
        record.phase = "restored".into();
        save(store, &record)?;
    }
    Ok("已还原原始 CLI 内容和签名".into())
}

pub fn restore(id: &str) -> Result<String> {
    let installation = find(id)?;
    ensure_idle(&installation)?;
    restore_at(&installation, &root())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "需要官方 macOS CLI 副本"]
    fn real_cli_roundtrip() {
        let source =
            PathBuf::from(std::env::var_os("CPP_MACOS_CLI_FIXTURE").expect("缺少官方副本"));
        let dir = tempfile::tempdir().unwrap();
        let source_unit = unit(&source);
        let target_unit = dir.path().join(source_unit.file_name().unwrap());
        copy(&source_unit, &target_unit).unwrap();
        let target = binary_in(&target_unit, &source).unwrap();
        let before = hash(&target).unwrap();
        let installation = Installation {
            id: "fixture".into(),
            kind: "fixture".into(),
            binary: target.clone(),
        };
        let store = dir.path().join("store");
        install_at(&installation, &store).unwrap();
        assert_eq!(describe(&installation, &store).unwrap().state, "patched");
        assert_eq!(
            install_at(&installation, &store).unwrap(),
            "这些提示词已经屏蔽"
        );
        if target_unit.is_dir() {
            let extra = target_unit.join("external-change");
            fs::write(&extra, b"changed").unwrap();
            assert!(restore_at(&installation, &store).is_err());
            fs::remove_file(extra).unwrap();
        }
        if let Some(output) = std::env::var_os("CPP_MACOS_CLI_OUTPUT") {
            let output = PathBuf::from(output);
            assert!(!output.exists());
            copy(&target_unit, &output).unwrap();
        }
        restore_at(&installation, &store).unwrap();
        assert_eq!(hash(&target).unwrap(), before);
        verify(&target_unit).unwrap();
    }

    #[test]
    fn snapshots_and_records_fail_closed() {
        for data in [
            vec![],
            vec![0; 32],
            [vec![0xcf, 0xfa, 0xed, 0xfe], vec![0; 28]].concat(),
        ] {
            assert!(bun::refresh_snapshots(&mut data.clone()).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("claude");
        fs::write(store_path(dir.path(), &binary), b"invalid").unwrap();
        assert!(read_record(dir.path(), &binary).is_err());
        let _lock = lock(dir.path()).unwrap();
        assert!(lock(dir.path()).is_err());
    }
}

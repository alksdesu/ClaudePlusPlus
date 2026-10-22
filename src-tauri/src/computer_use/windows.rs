use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::{fs::MetadataExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{asar, pe, Status};

const FILES: [&str; 2] = ["claude.exe", "resources/app.asar"];
const FLAG: &str = "--computer-use-elevated";
const NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    schema: u32,
    id: Uuid,
    target: PathBuf,
    version: String,
    baseline: [String; 2],
    modified: [String; 2],
    phase: String,
}

#[derive(Deserialize, Serialize)]
struct ElevatedResult {
    ok: bool,
    message: String,
}

struct Installation {
    app: PathBuf,
    store: PathBuf,
    version: String,
    elevated: bool,
}

fn quote(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('\'', "''")
    )
}

fn powershell(script: &str) -> Result<String> {
    let script =
        format!("[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false);{script}");
    // PowerShell 7 的模块路径不能交给系统 Windows PowerShell。
    let output = Command::new("powershell")
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(NO_WINDOW)
        .output()
        .context("运行 Windows 检测")?;
    ensure!(
        output.status.success(),
        "Windows 检测失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn plain(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("读取 {}", path.display()))?;
    ensure!(
        meta.file_attributes() & 0x400 == 0,
        "拒绝操作重解析点：{}",
        path.display()
    );
    ensure!(
        if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        },
        "路径类型异常：{}",
        path.display()
    );
    Ok(())
}

fn safe_directory(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        if parent.exists() {
            plain(parent, true)?;
        }
    }
    fs::create_dir_all(path)?;
    plain(path, true)
}

fn digest(path: &Path) -> Result<String> {
    plain(path, false)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hashes(app: &Path) -> Result<[String; 2]> {
    Ok([digest(&app.join(FILES[0]))?, digest(&app.join(FILES[1]))?])
}

fn verify_pair(app: &Path, version: &str) -> Result<asar::PatchState> {
    plain(app, true)?;
    plain(&app.join("resources"), true)?;
    for file in FILES {
        plain(&app.join(file), false)?;
    }
    let (state, hash) = asar::inspect_windows(&app.join(FILES[1]), version)?;
    ensure!(
        pe::inspect(&app.join(FILES[0]))? == hash,
        "exe 与 ASAR 的完整性记录不一致"
    );
    Ok(state)
}

fn verify_original(app: &Path, version: &str) -> Result<()> {
    ensure!(
        verify_pair(app, version)? == asar::PatchState::Pristine,
        "需要原始 Windows 主进程入口"
    );
    let script = format!(
        "$ErrorActionPreference='Stop'; $s=Get-AuthenticodeSignature -LiteralPath {}; [pscustomobject]@{{status=$s.Status.ToString();subject=$s.SignerCertificate.Subject}} | ConvertTo-Json -Compress",
        quote(&app.join(FILES[0])),
    );
    let signature: serde_json::Value = serde_json::from_str(&powershell(&script)?)?;
    ensure!(
        signature["status"] == "Valid"
            && signature["subject"]
                .as_str()
                .is_some_and(|s| s.contains("O=\"Anthropic, PBC\"")),
        "官方 Desktop 签名校验失败，拒绝建立备份"
    );
    Ok(())
}

fn running(app: &Path) -> Result<bool> {
    let output = powershell("$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process -Filter \"Name='claude.exe'\" | ForEach-Object { $_.ExecutablePath }")?;
    Ok(output.lines().any(|line| process_belongs_to(app, line)))
}

fn process_belongs_to(app: &Path, executable: &str) -> bool {
    let normalize = |s: &str| {
        s.trim()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_lowercase()
    };
    let prefix = format!(
        "{}\\",
        normalize(&app.to_string_lossy()).trim_end_matches('\\')
    );
    normalize(executable).starts_with(&prefix)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if path.exists() {
        plain(path, false)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn configuration() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("找不到配置目录")?
        .join("com.claudeplusplus.manager/computer-use/windows"))
}

fn normalize_version(version: &str) -> Result<String> {
    let parts: Vec<_> = version.split('.').collect();
    ensure!(
        (parts.len() == 3 || parts.len() == 4 && parts[3] == "0")
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "Desktop 版本格式异常"
    );
    Ok(parts[..3].join("."))
}

impl Installation {
    fn standard() -> Result<Option<Self>> {
        let text = powershell("$ErrorActionPreference='Stop'; Get-AppxPackage -Name Claude | Sort-Object Version -Descending | Select-Object -First 1 @{n='root';e={$_.InstallLocation}},@{n='version';e={$_.Version.ToString()}} | ConvertTo-Json -Compress")?;
        if text.is_empty() {
            return Ok(None);
        }
        let package: serde_json::Value = serde_json::from_str(&text)?;
        let root = PathBuf::from(package["root"].as_str().context("包路径缺失")?);
        plain(&root, true)?;
        plain(&root.join("app"), true)?;
        let app = fs::canonicalize(root.join("app"))?;
        Ok(Some(Self {
            app,
            store: configuration()?,
            version: normalize_version(package["version"].as_str().context("包版本缺失")?)?,
            elevated: true,
        }))
    }

    fn copy(app: &Path, store: &Path) -> Result<Self> {
        plain(app, true)?;
        let app = fs::canonicalize(app)?;
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "SystemRoot"] {
            if let Some(root) = std::env::var_os(variable) {
                ensure!(
                    !app.starts_with(fs::canonicalize(root)?),
                    "探针只允许用户目录中的应用副本"
                );
            }
        }
        if let Some(installed) = Self::standard()? {
            ensure!(
                app != installed.app && !app.starts_with(&installed.app),
                "探针不允许操作已安装的 Desktop"
            );
        }
        safe_directory(store)?;
        let store = fs::canonicalize(store)?;
        ensure!(
            !store.starts_with(&app) && !app.starts_with(&store),
            "应用与记录目录不能相互包含"
        );
        let version = powershell(&format!(
            "(Get-Item -LiteralPath {}).VersionInfo.FileVersion",
            quote(&app.join(FILES[0]))
        ))?;
        Ok(Self {
            app,
            store,
            version: normalize_version(&version)?,
            elevated: false,
        })
    }

    fn lock(&self) -> Result<File> {
        safe_directory(&self.store)?;
        let path = self.store.join("operation.lock");
        if path.exists() {
            plain(&path, false)?;
        }
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
        plain(&path, false)?;
        let record: Record = serde_json::from_slice(&fs::read(path)?).context("安装记录损坏")?;
        ensure!(
            record.schema == 1
                && matches!(
                    record.phase.as_str(),
                    "ready" | "installing" | "installed" | "restoring" | "restored"
                ),
            "安装记录版本或状态异常"
        );
        ensure!(
            record
                .baseline
                .iter()
                .chain(record.modified.iter())
                .all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "安装记录哈希无效"
        );
        Ok(Some(record))
    }

    fn transaction(&self, record: &Record) -> PathBuf {
        self.store.join(record.id.to_string())
    }
    fn baseline(&self, record: &Record) -> PathBuf {
        self.transaction(record).join("baseline")
    }
    fn stage(&self, record: &Record) -> PathBuf {
        self.transaction(record).join("stage")
    }

    fn save(&self, record: &Record) -> Result<()> {
        write_json(&self.transaction(record).join("record.json"), record)?;
        write_json(&self.store.join("installation.json"), record)
    }

    fn sibling(&self, record: &Record, index: usize, suffix: &str) -> PathBuf {
        self.app
            .join(FILES[index])
            .with_extension(format!("cpp-cu-{}.{suffix}", record.id))
    }

    fn owned(&self, record: &Record) -> Result<bool> {
        if record.target != self.app || record.version != self.version {
            return Ok(false);
        }
        for (i, file) in FILES.iter().enumerate() {
            let live = self.app.join(file);
            let path = if live.try_exists()? {
                live
            } else {
                self.sibling(record, i, "previous")
            };
            if !path.try_exists()? {
                return Ok(false);
            }
            let current = digest(&path)?;
            if current != record.baseline[i] && current != record.modified[i] {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn verify_backup(&self, record: &Record) -> Result<()> {
        plain(&self.transaction(record), true)?;
        ensure!(
            hashes(&self.baseline(record))? == record.baseline,
            "原始备份哈希不一致"
        );
        verify_original(&self.baseline(record), &record.version)
    }

    fn grant(&self, path: &Path) -> Result<()> {
        if !self.elevated {
            return Ok(());
        }
        let target = path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        let grant = format!("{}:F", std::env::var("USERNAME")?);
        for (program, args) in [
            ("takeown", vec!["/f", target.as_str()]),
            ("icacls", vec![target.as_str(), "/grant", grant.as_str()]),
        ] {
            let output = Command::new(program)
                .args(args)
                .creation_flags(NO_WINDOW)
                .output()?;
            ensure!(
                output.status.success(),
                "{program} 失败（{:?}）",
                output.status.code()
            );
        }
        Ok(())
    }

    fn clear_siblings(&self, record: &Record) -> Result<()> {
        for (i, file) in FILES.iter().enumerate() {
            for suffix in ["new", "previous"] {
                let path = self.sibling(record, i, suffix);
                if !path.try_exists()? {
                    continue;
                }
                let value = digest(&path)?;
                // 未提交的 new 文件可能因中断只复制了一半；previous 必须仍是已知的完整版本。
                ensure!(
                    suffix == "new" || value == record.baseline[i] || value == record.modified[i],
                    "替换暂存文件已被外部修改"
                );
                let live = self.app.join(file);
                if suffix == "previous" && !live.try_exists()? {
                    fs::rename(path, live)?;
                } else {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    fn replace(&self, record: &Record, source: &Path, expected: &[String; 2]) -> Result<()> {
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        ensure!(self.owned(record)?, "Desktop 已被外部更新或修改，拒绝替换");
        self.clear_siblings(record)?;
        for (i, file) in FILES.iter().enumerate() {
            let live = self.app.join(file);
            self.grant(live.parent().context("文件缺少父目录")?)?;
            self.grant(&live)?;
            let new = self.sibling(record, i, "new");
            ensure!(!new.try_exists()?, "替换暂存文件已存在");
            fs::copy(source.join(file), &new)?;
            ensure!(digest(&new)? == expected[i], "替换副本哈希不一致");
        }
        ensure!(
            !running(&self.app)? && self.owned(record)?,
            "替换前应用状态发生变化"
        );
        self.commit_files(record, |from, to| fs::rename(from, to))?;
        ensure!(
            &hashes(&self.app)? == expected,
            "替换结果回读不一致，原始备份仍保留"
        );
        Ok(())
    }

    fn commit_files(
        &self,
        record: &Record,
        mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<()> {
        for (i, file) in FILES.iter().enumerate() {
            let live = self.app.join(file);
            let previous = self.sibling(record, i, "previous");
            let result = rename(&live, &previous)
                .and_then(|_| rename(&self.sibling(record, i, "new"), &live));
            if let Err(error) = result {
                for (j, file) in FILES[..=i].iter().enumerate().rev() {
                    let previous = self.sibling(record, j, "previous");
                    if !previous.exists() {
                        continue;
                    }
                    let target = self.app.join(file);
                    if target.exists() {
                        fs::remove_file(&target).context("回滚时移除修改文件失败")?;
                    }
                    fs::rename(previous, target).context("自动回滚失败，请使用原始备份还原")?;
                }
                return Err(error).context("替换失败，已回滚已替换文件");
            }
        }
        Ok(())
    }

    fn status(&self) -> Result<Status> {
        let mut s = empty_status();
        s.app_path = self.app.display().to_string();
        s.desktop_version = Some(self.version.clone());
        s.running = running(&self.app)?;
        if let Some(record) = self.record()? {
            s.backup_path = Some(self.baseline(&record).display().to_string());
            if record.phase != "restored" {
                if !self.owned(&record)? {
                    s.state = "changed".into();
                    s.message = "Desktop 已被外部更新或修改；保留备份，不覆盖其他版本".into();
                    return Ok(s);
                }
                self.verify_backup(&record)?;
                s.can_restore = !s.running;
                if record.phase != "installed" {
                    s.state = "recovery".into();
                    s.message = "上次操作未完成，可先还原官方文件".into();
                    return Ok(s);
                }
                if hashes(&self.app)? == record.modified {
                    ensure!(
                        verify_pair(&self.app, &self.version)? == asar::PatchState::Patched,
                        "补丁状态异常"
                    );
                    s.state = "installed".into();
                    s.message = "Windows 原生电脑操作已安装；请在 Desktop 中自行启用并授权".into();
                    return Ok(s);
                }
            }
        }
        match verify_pair(&self.app, &self.version) {
            Ok(asar::PatchState::Pristine) => match verify_original(&self.app, &self.version) {
                Ok(()) => {
                    s.state = "available".into();
                    s.message = "可安装 Windows 原生电脑操作；安装和还原需要管理员确认".into();
                    s.can_install = !s.running;
                }
                Err(e) => {
                    s.state = "unsupported".into();
                    s.message = format!("{e:#}");
                }
            },
            Ok(_) => {
                s.state = "unmanaged".into();
                s.message = "检测到修改版，但没有匹配的原始备份记录".into();
            }
            Err(e) => {
                s.state = "unsupported".into();
                s.message = format!("{e:#}");
            }
        }
        Ok(s)
    }

    fn install(&self) -> Result<String> {
        let _lock = self.lock()?;
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        if let Some(record) = self.record()? {
            if record.phase == "installed"
                && self.owned(&record)?
                && hashes(&self.app)? == record.modified
            {
                self.verify_backup(&record)?;
                ensure!(
                    verify_pair(&self.app, &self.version)? == asar::PatchState::Patched,
                    "补丁状态异常"
                );
                return Ok("Windows 原生 Computer Use 已安装".into());
            }
            ensure!(record.phase == "restored", "请先还原以结束已有安装记录");
        }
        verify_original(&self.app, &self.version)?;
        plain(&self.app.join("resources/app.asar.unpacked/node_modules/@ant/claude-native/claude-native-binding.node"), false)?;
        let mut record = Record {
            schema: 1,
            id: Uuid::new_v4(),
            target: self.app.clone(),
            version: self.version.clone(),
            baseline: hashes(&self.app)?,
            modified: Default::default(),
            phase: "ready".into(),
        };
        let baseline = self.baseline(&record);
        let stage = self.stage(&record);
        for destination in [&baseline, &stage] {
            safe_directory(&destination.join("resources"))?;
            for file in FILES {
                fs::copy(self.app.join(file), destination.join(file))?;
            }
            ensure!(
                hashes(destination)? == record.baseline,
                "备份或暂存副本校验失败"
            );
        }
        verify_original(&baseline, &self.version)?;
        let original = pe::inspect(&stage.join(FILES[0]))?;
        let modified = asar::patch_windows(&stage.join(FILES[1]), &self.version)?;
        pe::update(&stage.join(FILES[0]), &original, &modified)?;
        ensure!(
            verify_pair(&stage, &self.version)? == asar::PatchState::Patched,
            "补丁副本完整性校验失败"
        );
        record.modified = hashes(&stage)?;
        self.save(&record)?;
        record.phase = "installing".into();
        self.save(&record)?;
        self.replace(&record, &stage, &record.modified)?;
        record.phase = "installed".into();
        self.save(&record)?;
        self.clear_siblings(&record)?;
        for file in FILES {
            let _ = fs::remove_file(stage.join(file));
        }
        Ok("Windows 原生 Computer Use 已安装；请打开 Desktop 设置，自行启用并完成原生授权".into())
    }

    fn restore(&self) -> Result<String> {
        let _lock = self.lock()?;
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        let mut record = self.record()?.context("没有可还原的 Windows 安装记录")?;
        ensure!(self.owned(&record)?, "Desktop 已被外部更新或修改，拒绝覆盖");
        self.verify_backup(&record)?;
        self.clear_siblings(&record)?;
        record.phase = "restoring".into();
        self.save(&record)?;
        self.replace(&record, &self.baseline(&record), &record.baseline)?;
        verify_original(&self.app, &record.version)?;
        record.phase = "restored".into();
        self.save(&record)?;
        self.clear_siblings(&record)?;
        Ok("已还原官方 exe 与 ASAR，签名和完整性校验通过；会话、配置和授权设置未修改".into())
    }
}

fn empty_status() -> Status {
    Status {
        supported: true,
        requires_signing: false,
        app_path: String::new(),
        desktop_version: None,
        state: "notFound".into(),
        message: "未找到 Claude Desktop MSIX 包".into(),
        identities: Vec::new(),
        backup_path: None,
        running: false,
        can_install: false,
        can_restore: false,
    }
}

pub fn status() -> Result<Status> {
    match Installation::standard()? {
        Some(i) => i.status(),
        None => Ok(empty_status()),
    }
}

fn elevate(mode: &str) -> Result<String> {
    let install = Installation::standard()?.context("未找到 Claude Desktop")?;
    ensure!(!running(&install.app)?, "Claude Desktop 正在运行，请先退出");
    let store = configuration()?;
    safe_directory(&store)?;
    let request = Uuid::new_v4();
    let output = store.join(format!("result-{request}.json"));
    let exe = std::env::current_exe()?;
    let script=format!("$ErrorActionPreference='Stop'; $p=Start-Process -FilePath {} -ArgumentList '{FLAG}','{mode}','{request}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",quote(&exe));
    let launched = powershell(&script);
    if !output.exists() {
        launched.context("管理员操作被取消或未完成")?;
        bail!("管理员进程未返回结果");
    }
    plain(&output, false)?;
    let result: ElevatedResult = serde_json::from_slice(&fs::read(&output)?)?;
    fs::remove_file(output)?;
    ensure!(result.ok, "{}", result.message);
    Ok(result.message)
}

pub fn install(_identity: &str) -> Result<String> {
    elevate("install")
}
pub fn restore() -> Result<String> {
    elevate("restore")
}

pub fn elevated_entry() -> Option<i32> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some(FLAG) {
        return None;
    }
    Some(
        (|| -> Result<i32> {
            ensure!(args.len() == 3, "提权参数无效");
            let request = Uuid::parse_str(&args[2])?;
            let operation = (|| -> Result<String> {
                let install = Installation::standard()?.context("未找到 Claude Desktop")?;
                match args[1].as_str() {
                    "install" => install.install(),
                    "restore" => install.restore(),
                    _ => bail!("未知操作"),
                }
            })();
            let result = match operation {
                Ok(message) => ElevatedResult { ok: true, message },
                Err(e) => ElevatedResult {
                    ok: false,
                    message: format!("{e:#}"),
                },
            };
            let store = configuration()?;
            safe_directory(&store)?;
            let path = store.join(format!("result-{request}.json"));
            ensure!(!path.exists(), "结果文件已存在");
            let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
            serde_json::to_writer(&mut file, &result)?;
            file.flush()?;
            file.sync_all()?;
            Ok(if result.ok { 0 } else { 1 })
        })()
        .unwrap_or(1),
    )
}

pub fn status_copy(app: &Path, store: &Path) -> Result<Status> {
    Installation::copy(app, store)?.status()
}
pub fn install_copy(app: &Path, store: &Path, _identity: &str) -> Result<String> {
    Installation::copy(app, store)?.install()
}
pub fn restore_copy(app: &Path, store: &Path) -> Result<String> {
    Installation::copy(app, store)?.restore()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_format_is_not_guessed() {
        assert_eq!(normalize_version("3.1623.4.0").unwrap(), "3.1623.4");
        assert_eq!(normalize_version("3.1623.4").unwrap(), "3.1623.4");
        for version in ["3.1623.4.1", "3.x.4", "3.4", "3.4.5;echo"] {
            assert!(normalize_version(version).is_err());
        }
    }

    #[test]
    fn process_paths_match_extended_paths_without_prefix_collisions() {
        let app = Path::new(r"\\?\C:\Program Files\WindowsApps\Claude_test\app");
        assert!(process_belongs_to(
            app,
            r"c:\program files\windowsapps\claude_test\app\Claude.exe"
        ));
        assert!(!process_belongs_to(
            app,
            r"C:\Program Files\WindowsApps\Claude_test\app-other\Claude.exe"
        ));
        assert!(!process_belongs_to(app, r"E:\tools\claude.exe"));
    }

    #[test]
    fn partial_transaction_is_owned_but_external_edits_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        let installation = Installation {
            app: app.clone(),
            store: dir.path().join("store"),
            version: "3.1623.4".into(),
            elevated: false,
        };
        for file in FILES {
            fs::write(app.join(file), b"original").unwrap();
        }
        let original = hashes(&app).unwrap();
        let record = Record {
            schema: 1,
            id: Uuid::new_v4(),
            target: app.clone(),
            version: installation.version.clone(),
            baseline: original,
            modified: [asar::hash(b"modified"), asar::hash(b"modified")],
            phase: "installing".into(),
        };
        fs::rename(
            app.join(FILES[0]),
            installation.sibling(&record, 0, "previous"),
        )
        .unwrap();
        fs::write(installation.sibling(&record, 0, "new"), b"partial").unwrap();
        assert!(installation.owned(&record).unwrap());
        installation.clear_siblings(&record).unwrap();
        assert_eq!(fs::read(app.join(FILES[0])).unwrap(), b"original");
        fs::write(app.join(FILES[1]), b"external").unwrap();
        assert!(!installation.owned(&record).unwrap());
    }

    #[test]
    fn second_file_failure_restores_the_first_and_second() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        fs::create_dir_all(app.join("resources")).unwrap();
        let installation = Installation {
            app: app.clone(),
            store: dir.path().join("store"),
            version: "3.1623.4".into(),
            elevated: false,
        };
        for file in FILES {
            fs::write(app.join(file), b"original").unwrap();
        }
        let record = Record {
            schema: 1,
            id: Uuid::new_v4(),
            target: app.clone(),
            version: installation.version.clone(),
            baseline: hashes(&app).unwrap(),
            modified: [asar::hash(b"modified"), asar::hash(b"modified")],
            phase: "installing".into(),
        };
        for i in 0..2 {
            fs::write(installation.sibling(&record, i, "new"), b"modified").unwrap();
        }
        let mut calls = 0;
        let result = installation.commit_files(&record, |from, to| {
            calls += 1;
            if calls == 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected",
                ));
            }
            fs::rename(from, to)
        });
        assert!(result.is_err());
        assert_eq!(hashes(&app).unwrap(), record.baseline);
        assert!(installation.owned(&record).unwrap());
        installation.clear_siblings(&record).unwrap();
    }

    #[test]
    fn corrupted_journal_is_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let installation = Installation {
            app: dir.path().join("app"),
            store: dir.path().join("store"),
            version: "3.1623.4".into(),
            elevated: false,
        };
        fs::create_dir(&installation.store).unwrap();
        fs::write(installation.store.join("installation.json"), b"broken").unwrap();
        assert!(installation.record().is_err());
        assert_eq!(
            fs::read(installation.store.join("installation.json")).unwrap(),
            b"broken"
        );
    }
}

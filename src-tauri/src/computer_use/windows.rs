use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::{fs::MetadataExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::native::Selection;
use super::{asar, pe, rules, Status};

const FILES: [&str; 2] = ["claude.exe", "resources/app.asar"];
const RENDERER: &str = "resources/ion-dist/assets/v1";
const CATALOGS: &str = "resources/ion-dist/i18n";
const FLAG: &str = "--computer-use-elevated";
const NO_WINDOW: u32 = 0x0800_0000;
const PLATFORM: rules::Platform = rules::Platform::Windows;

/// 事务里的一个文件：baseline 为 None 表示原本不存在（中文新增的词典）
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileRecord {
    path: String,
    baseline: Option<String>,
    modified: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    schema: u32,
    id: Uuid,
    target: PathBuf,
    version: String,
    baseline: [String; 2],
    modified: [String; 2],
    phase: String,
    /// schema 1 的记录只装过 Computer Use，读出来补成等价的组合
    #[serde(default)]
    selection: Option<Selection>,
    #[serde(default)]
    files: Option<Vec<FileRecord>>,
}

impl FileRecord {
    fn target(&self, restore: bool) -> Option<&String> {
        if restore {
            self.baseline.as_ref()
        } else {
            self.modified.as_ref()
        }
    }

    /// exe 与 ASAR 每次都入账，只装中文时它们内容不变，不该被替换
    fn unchanged(&self) -> bool {
        self.baseline == self.modified
    }
}

impl Record {
    fn selected(&self) -> Selection {
        self.selection.unwrap_or(Selection {
            computer_use: true,
            ..Default::default()
        })
    }

    /// schema 1 只记两个固定文件的哈希，转成统一的清单形式
    fn entries(&self) -> Vec<FileRecord> {
        self.files.clone().unwrap_or_else(|| {
            FILES
                .iter()
                .enumerate()
                .map(|(i, path)| FileRecord {
                    path: (*path).into(),
                    baseline: Some(self.baseline[i].clone()),
                    modified: Some(self.modified[i].clone()),
                })
                .collect()
        })
    }
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

/// Resolve only the AppX-registered package root, not links inside the app.
fn registered_package_app(root: &Path) -> Result<PathBuf> {
    let meta = fs::symlink_metadata(root)?;
    let resolved = if meta.file_attributes() & 0x400 != 0 {
        let target = junction::get_target(root).context("安装目录不是 NTFS junction")?;
        plain(&target, true)?;
        let resolved = fs::canonicalize(&target)?;
        let windows_apps = |path: &Path| {
            path.parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("WindowsApps"))
        };
        ensure!(
            windows_apps(root)
                && windows_apps(&resolved)
                && root.file_name() == resolved.file_name(),
            "安装目录链接必须指向 WindowsApps 中的同名包"
        );
        resolved
    } else {
        plain(root, true)?;
        fs::canonicalize(root)?
    };
    for ancestor in resolved.ancestors() {
        plain(ancestor, true)?;
    }
    let app = resolved.join("app");
    plain(&app, true)?;
    Ok(app)
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

/// 备份与暂存目录只放本次事务涉及的文件，那里缺 renderer 资源不算结构变化
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    App,
    Transaction,
}

fn required(found: Option<(PathBuf, String)>, scope: Scope, label: &str) -> Result<Option<String>> {
    match found {
        Some((_, text)) => Ok(Some(text)),
        None if scope == Scope::Transaction => Ok(None),
        None => bail!("未找到{label}入口"),
    }
}

/// 主进程开关与 renderer、词典必须讲同一个故事，半装状态一律拒绝
fn verify_pair(app: &Path, scope: Scope) -> Result<Selection> {
    plain(app, true)?;
    plain(&app.join("resources"), true)?;
    for file in FILES {
        plain(&app.join(file), false)?;
    }
    let mut selection = asar::rules_selection(&app.join(FILES[1]), PLATFORM)?;
    ensure!(
        pe::inspect(&app.join(FILES[0]))? == asar::header_hash(&app.join(FILES[1]))?,
        "exe 与 ASAR 的完整性记录不一致"
    );
    let renderer = app.join(RENDERER);
    let (fast, language) = if scope == Scope::Transaction && !renderer.is_dir() {
        (None, None)
    } else {
        (
            required(rules::fast_state_asset(&renderer)?, scope, "Fast 状态")?,
            required(rules::language_asset(&renderer)?, scope, "语言")?,
        )
    };
    let fast = fast.as_deref().map(rules::fast_state_installed).transpose()?;
    ensure!(
        fast.unwrap_or(false) == selection.fast_mode_status,
        "Fast 原生开关与主进程映射不一致"
    );
    let chinese = language.as_deref().map(rules::language_installed).transpose()?;
    selection.chinese = chinese.unwrap_or(false);
    ensure!(
        selection.chinese == super::native::catalogs::present(&app.join(CATALOGS))?,
        "中文词典与语言入口状态不一致"
    );
    Ok(selection)
}

/// Fast Mode 页的 renderer 补丁按 .orig 还原、原生开关按哈希记账，同一文件叠两份后谁都还原不了
fn legacy_fast_patch(renderer: &Path) -> Result<bool> {
    if !renderer.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(renderer)? {
        if entry?.file_name().to_string_lossy().ends_with(".js.orig") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn verify_original(app: &Path, scope: Scope) -> Result<()> {
    ensure!(
        verify_pair(app, scope)?.is_empty(),
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
    if normalize(executable).starts_with(&prefix) {
        return true;
    }
    // A running MSIX app can report the registered C: path even when the
    // installation has been resolved to its D: WindowsApps directory.
    fs::canonicalize(executable.trim())
        .is_ok_and(|path| normalize(&path.to_string_lossy()).starts_with(&prefix))
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

fn version_tuple(version: &str) -> Option<[u64; 3]> {
    let normalized = normalize_version(version).ok()?;
    let mut parts = normalized.split('.');
    Some([
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ])
}

impl Installation {
    fn standard() -> Result<Option<Self>> {
        let text = powershell("$ErrorActionPreference='Stop'; Get-AppxPackage -Name Claude | Sort-Object Version -Descending | Select-Object -First 1 @{n='root';e={$_.InstallLocation}},@{n='version';e={$_.Version.ToString()}} | ConvertTo-Json -Compress")?;
        if text.is_empty() {
            return Ok(None);
        }
        let package: serde_json::Value = serde_json::from_str(&text)?;
        let root = PathBuf::from(package["root"].as_str().context("包路径缺失")?);
        let app = registered_package_app(&root)?;
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
            matches!(record.schema, 1 | 2)
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

    fn is_package_update(&self, record: &Record) -> bool {
        self.app != record.target
            && matches!(
                (version_tuple(&self.version), version_tuple(&record.version)),
                (Some(current), Some(previous)) if current > previous
            )
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

    fn sibling(&self, record: &Record, path: &str, suffix: &str) -> PathBuf {
        self.app
            .join(path)
            .with_extension(format!("cpp-cu-{}.{suffix}", record.id))
    }

    fn owned(&self, record: &Record) -> Result<bool> {
        if record.target != self.app || record.version != self.version {
            return Ok(false);
        }
        for entry in record.entries() {
            let live = self.app.join(&entry.path);
            let path = if live.try_exists()? {
                live
            } else {
                self.sibling(record, &entry.path, "previous")
            };
            if !path.try_exists()? {
                // 中文词典原本就不存在，缺席本身就是已知状态之一
                if entry.baseline.is_none() {
                    continue;
                }
                return Ok(false);
            }
            let current = Some(digest(&path)?);
            if current != entry.baseline && current != entry.modified {
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
        verify_original(&self.baseline(record), Scope::Transaction)
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
        for entry in record.entries() {
            for suffix in ["new", "previous"] {
                let path = self.sibling(record, &entry.path, suffix);
                if !path.try_exists()? {
                    continue;
                }
                let value = Some(digest(&path)?);
                // 未提交的 new 文件可能因中断只复制了一半；previous 必须仍是已知的完整版本。
                ensure!(
                    suffix == "new" || value == entry.baseline || value == entry.modified,
                    "替换暂存文件已被外部修改"
                );
                let live = self.app.join(&entry.path);
                if suffix == "previous" && !live.try_exists()? {
                    fs::rename(path, live)?;
                } else {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    /// restore 为真时目标是 baseline，否则是 modified；目标为 None 表示该文件应消失
    fn replace(&self, record: &Record, source: &Path, restore: bool) -> Result<()> {
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        ensure!(self.owned(record)?, "Desktop 已被外部更新或修改，拒绝替换");
        self.clear_siblings(record)?;
        let entries = record.entries();
        for entry in entries.iter().filter(|e| !e.unchanged()) {
            let live = self.app.join(&entry.path);
            let parent = live.parent().context("文件缺少父目录")?;
            self.grant(parent)?;
            if live.try_exists()? {
                self.grant(&live)?;
            }
            let Some(expected) = entry.target(restore) else {
                continue;
            };
            let new = self.sibling(record, &entry.path, "new");
            ensure!(!new.try_exists()?, "替换暂存文件已存在");
            fs::copy(source.join(&entry.path), &new)
                .with_context(|| format!("准备 {}", entry.path))?;
            ensure!(digest(&new)? == *expected, "替换副本哈希不一致");
        }
        ensure!(
            !running(&self.app)? && self.owned(record)?,
            "替换前应用状态发生变化"
        );
        self.commit_files(record, |from, to| fs::rename(from, to))?;
        for entry in &entries {
            let live = self.app.join(&entry.path);
            match entry.target(restore) {
                Some(expected) => ensure!(
                    digest(&live)? == *expected,
                    "替换结果回读不一致，原始备份仍保留"
                ),
                None => {
                    ensure!(!live.try_exists()?, "应移除的文件仍然存在");
                    // 该文件本就不该存在，留着暂存会被中断恢复逻辑挪回原位
                    let previous = self.sibling(record, &entry.path, "previous");
                    if previous.try_exists()? {
                        fs::remove_file(previous)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn commit_files(
        &self,
        record: &Record,
        mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let entries: Vec<_> = record.entries().into_iter().filter(|e| !e.unchanged()).collect();
        for (index, entry) in entries.iter().enumerate() {
            let live = self.app.join(&entry.path);
            let previous = self.sibling(record, &entry.path, "previous");
            let new = self.sibling(record, &entry.path, "new");
            let result = (|| -> std::io::Result<()> {
                if live.try_exists()? {
                    rename(&live, &previous)?;
                }
                if new.try_exists()? {
                    rename(&new, &live)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                for done in entries[..=index].iter().rev() {
                    let previous = self.sibling(record, &done.path, "previous");
                    let target = self.app.join(&done.path);
                    if target.exists() && previous.exists() {
                        fs::remove_file(&target).context("回滚时移除修改文件失败")?;
                    }
                    if previous.exists() {
                        fs::rename(previous, target).context("自动回滚失败，请使用原始备份还原")?;
                    } else if target.exists() && done.baseline.is_none() {
                        fs::remove_file(&target).context("回滚时移除新增文件失败")?;
                    }
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
        let record = self.record()?;
        let package_update = match record.as_ref() {
            Some(record) if self.is_package_update(record) => {
                self.verify_backup(record)?;
                true
            }
            _ => false,
        };
        if let Some(record) = record.filter(|_| !package_update) {
            s.backup_path = Some(self.baseline(&record).display().to_string());
            if record.phase != "restored" {
                if !self.owned(&record)? {
                    s.state = "changed".into();
                    s.message = "Desktop 被外部更新或改动过，不会覆盖；备份仍保留".into();
                    return Ok(s);
                }
                self.verify_backup(&record)?;
                s.can_restore = !s.running;
                if record.phase != "installed" {
                    s.state = "recovery".into();
                    s.message = "上次操作没完成，请先还原".into();
                    return Ok(s);
                }
                let current = verify_pair(&self.app, Scope::App)?;
                if current == record.selected() && !current.is_empty() {
                    s.state = "installed".into();
                    s.message = format!(
                        "{} 已安装，去 Desktop 设置里打开",
                        super::updates::names(current).join("、")
                    );
                    // 换组合会在同一次提权里先还原再重装
                    s.can_install = !s.running;
                    return Ok(s);
                }
            }
        }
        match verify_pair(&self.app, Scope::App) {
            Ok(selection) if selection.is_empty() => {
                match verify_original(&self.app, Scope::App) {
                    Ok(()) => {
                        s.state = "available".into();
                        s.message = if package_update {
                            "Desktop 已更新，可以重新安装"
                        } else {
                            "可以安装，需要管理员确认"
                        }
                        .into();
                        s.can_install = !s.running;
                    }
                    Err(e) => {
                        s.state = "unsupported".into();
                        s.message = format!("{e:#}");
                    }
                }
            }
            Ok(_) => {
                s.state = "unmanaged".into();
                s.message = "检测到别处装的补丁，没有对应的备份记录".into();
            }
            Err(e) => {
                s.state = "unsupported".into();
                s.message = format!("{e:#}");
            }
        }
        Ok(s)
    }

    /// 本次事务要动的文件；renderer 的 chunk 名带哈希，只能按内容现场定位。
    /// exe 与 ASAR 始终入账：备份和校验都以它们为基准，内容没变的不会被替换
    fn plan(&self, selection: Selection) -> Result<Vec<String>> {
        let mut files: Vec<String> = FILES.iter().map(|f| (*f).to_string()).collect();
        let renderer = self.app.join(RENDERER);
        let relative = |path: &Path| -> Result<String> {
            Ok(path
                .strip_prefix(&self.app)?
                .to_string_lossy()
                .replace('\\', "/"))
        };
        if selection.fast_mode_status {
            let (path, _) = rules::fast_state_asset(&renderer)?.context("未找到 Fast 状态入口")?;
            files.push(relative(&path)?);
        }
        if selection.chinese {
            let (path, _) = rules::language_asset(&renderer)?.context("未找到语言入口")?;
            files.push(relative(&path)?);
            files.extend(
                super::native::catalogs::RESOURCES
                    .iter()
                    .map(|(name, _, _)| format!("{CATALOGS}/{name}")),
            );
        }
        Ok(files)
    }

    fn stage_patch(&self, stage: &Path, selection: Selection) -> Result<()> {
        if selection.mask() != 0 {
            let original = pe::inspect(&stage.join(FILES[0]))?;
            let header = asar::patch_with_rules(&stage.join(FILES[1]), PLATFORM, selection)?;
            pe::update(&stage.join(FILES[0]), &original, &header)?;
        }
        let renderer = stage.join(RENDERER);
        if selection.fast_mode_status {
            let (path, text) = rules::fast_state_asset(&renderer)?.context("未找到 Fast 状态入口")?;
            fs::write(path, rules::patch_fast_state(&text)?)?;
        }
        if selection.chinese {
            let (path, text) = rules::language_asset(&renderer)?.context("未找到语言入口")?;
            fs::write(path, rules::patch_language(&text)?)?;
            super::native::catalogs::install(&stage.join(CATALOGS), stage)?;
        }
        Ok(())
    }

    fn apply(&self, selection: Selection) -> Result<String> {
        let _lock = self.lock()?;
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        ensure!(
            selection.mask() & !PLATFORM.supported() == 0,
            "当前平台尚未提供所选原生功能"
        );
        if selection.is_empty() {
            return self.restore_locked();
        }
        if selection.fast_mode_status {
            ensure!(
                !legacy_fast_patch(&self.app.join(RENDERER))?,
                "Fast Mode 页的 renderer 补丁还在。先在 Fast Mode 页还原，装好原生开关后再回去装 wrapper"
            );
        }
        if let Some(record) = self.record()? {
            if self.is_package_update(&record) {
                self.verify_backup(&record)?;
            } else if record.phase == "installed" && self.owned(&record)? {
                if verify_pair(&self.app, Scope::App)? == selection {
                    self.verify_backup(&record)?;
                    return Ok("这些功能已经装好了".into());
                }
                // 组合必须从官方基线重新生成：先还原，再按新组合安装，同一次提权里完成
                self.restore_locked()?;
            } else {
                ensure!(record.phase == "restored", "请先还原以结束已有安装记录");
            }
        }
        verify_original(&self.app, Scope::App)?;
        plain(&self.app.join("resources/app.asar.unpacked/node_modules/@ant/claude-native/claude-native-binding.node"), false)?;
        let mut record = Record {
            schema: 2,
            id: Uuid::new_v4(),
            target: self.app.clone(),
            version: self.version.clone(),
            baseline: hashes(&self.app)?,
            modified: Default::default(),
            phase: "ready".into(),
            selection: Some(selection),
            files: None,
        };
        let baseline = self.baseline(&record);
        let stage = self.stage(&record);
        let planned = self.plan(selection)?;
        let mut entries = Vec::new();
        for file in &planned {
            let live = self.app.join(file);
            let present = live.try_exists()?;
            for destination in [&baseline, &stage] {
                let target = destination.join(file);
                safe_directory(target.parent().context("暂存文件缺少父目录")?)?;
                if present {
                    fs::copy(&live, &target).with_context(|| format!("备份 {file}"))?;
                }
            }
            entries.push(FileRecord {
                path: file.clone(),
                baseline: present.then(|| digest(&live)).transpose()?,
                modified: None,
            });
        }
        record.files = Some(entries.clone());
        verify_original(&baseline, Scope::Transaction)?;
        self.stage_patch(&stage, selection)?;
        ensure!(
            verify_pair(&stage, Scope::Transaction)? == selection,
            "补丁副本校验失败"
        );
        for entry in entries.iter_mut() {
            let staged = stage.join(&entry.path);
            entry.modified = staged.try_exists()?.then(|| digest(&staged)).transpose()?;
        }
        record.files = Some(entries);
        record.modified = hashes(&stage)?;
        self.save(&record)?;
        record.phase = "installing".into();
        self.save(&record)?;
        self.replace(&record, &stage, false)?;
        record.phase = "installed".into();
        self.save(&record)?;
        self.clear_siblings(&record)?;
        for entry in record.entries() {
            let _ = fs::remove_file(stage.join(&entry.path));
        }
        Ok(format!(
            "{} 已安装，去 Desktop 设置里打开",
            super::updates::names(selection).join("、")
        ))
    }

    fn restore(&self) -> Result<String> {
        let _lock = self.lock()?;
        self.restore_locked()
    }

    fn restore_locked(&self) -> Result<String> {
        ensure!(!running(&self.app)?, "Claude Desktop 正在运行，请先退出");
        let mut record = self.record()?.context("没有可还原的 Windows 安装记录")?;
        ensure!(self.owned(&record)?, "Desktop 已被外部更新或修改，拒绝覆盖");
        self.verify_backup(&record)?;
        self.clear_siblings(&record)?;
        record.phase = "restoring".into();
        self.save(&record)?;
        self.replace(&record, &self.baseline(&record), true)?;
        verify_original(&self.app, Scope::App)?;
        record.phase = "restored".into();
        self.save(&record)?;
        self.clear_siblings(&record)?;
        Ok("已还原官方文件".into())
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

pub(super) fn selection_at() -> Result<Selection> {
    match Installation::standard()? {
        Some(i) => verify_pair(&i.app, Scope::App),
        None => Ok(Selection::default()),
    }
}

/// 提权进程只收功能位和请求号，不接受任何路径参数
fn encode(selection: Selection) -> u8 {
    selection.mask() | (u8::from(selection.chinese) << 4)
}

fn decode(bits: u8) -> Selection {
    Selection {
        computer_use: bits & rules::COMPUTER_USE != 0,
        simulators: bits & rules::SIMULATORS != 0,
        prewarm: bits & rules::PREWARM != 0,
        fast_mode_status: bits & rules::FAST_STATUS != 0,
        chinese: bits & 16 != 0,
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

// 电脑操作页的安装只加这一项，已装的其他功能保留
pub fn install(_identity: &str) -> Result<String> {
    apply(
        "",
        Selection {
            computer_use: true,
            ..selection_at()?
        },
    )
}

pub fn apply(_identity: &str, selection: Selection) -> Result<String> {
    if selection.is_empty() {
        return elevate("restore");
    }
    elevate(&format!("apply-{}", encode(selection)))
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
                    "restore" => install.restore(),
                    mode => {
                        let bits: u8 = mode
                            .strip_prefix("apply-")
                            .context("未知操作")?
                            .parse()
                            .context("功能位无效")?;
                        ensure!(bits > 0 && bits <= 31, "功能位超出范围");
                        install.apply(decode(bits))
                    }
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
    let install = Installation::copy(app, store)?;
    install.apply(Selection {
        computer_use: true,
        ..verify_pair(&install.app, Scope::App)?
    })
}

pub fn apply_copy(
    app: &Path,
    store: &Path,
    _identity: &str,
    selection: Selection,
) -> Result<String> {
    Installation::copy(app, store)?.apply(selection)
}
pub fn restore_copy(app: &Path, store: &Path) -> Result<String> {
    Installation::copy(app, store)?.restore()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_package_accepts_plain_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("app")).unwrap();
        assert_eq!(
            registered_package_app(dir.path()).unwrap(),
            fs::canonicalize(dir.path()).unwrap().join("app")
        );
    }

    #[test]
    fn registered_package_accepts_same_package_junction_and_detects_running_alias() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source/WindowsApps/Claude_test");
        let target = dir.path().join("target/WindowsApps/Claude_test");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(target.join("app")).unwrap();
        fs::write(target.join("app/Claude.exe"), b"fixture").unwrap();
        junction::create(&target, &source).unwrap();
        let app = registered_package_app(&source).unwrap();
        assert_eq!(app, fs::canonicalize(target.join("app")).unwrap());
        assert!(process_belongs_to(
            &app,
            &source.join("app/Claude.exe").to_string_lossy()
        ));
        assert!(!process_belongs_to(
            &app,
            &source.join("app-other/Claude.exe").to_string_lossy()
        ));
    }

    #[test]
    fn registered_package_rejects_redirect_to_other_package_or_container() {
        let dir = tempfile::tempdir().unwrap();
        for (index, destination) in [
            "target/WindowsApps/OtherPackage",
            "target/OtherFolder/Claude_test",
        ]
        .iter()
        .enumerate()
        {
            let source = dir
                .path()
                .join(format!("source{index}/WindowsApps/Claude_test"));
            let target = dir.path().join(destination);
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::create_dir_all(target.join("app")).unwrap();
            junction::create(&target, &source).unwrap();
            assert!(registered_package_app(&source).is_err());
        }
    }

    #[test]
    fn registered_package_still_rejects_linked_app_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("WindowsApps/Claude_test");
        let other = dir.path().join("other");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&other).unwrap();
        junction::create(&other, root.join("app")).unwrap();
        assert!(registered_package_app(&root).is_err());
    }

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
    fn package_updates_require_both_a_new_path_and_a_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let old_app = dir.path().join("old/app");
        let new_app = dir.path().join("new/app");
        let record = Record {
            schema: 1,
            id: Uuid::new_v4(),
            target: old_app.clone(),
            version: "3.9.1".into(),
            baseline: [asar::hash(b"original"), asar::hash(b"original")],
            modified: [asar::hash(b"patched"), asar::hash(b"patched")],
            phase: "installed".into(),
            selection: None,
            files: None,
        };
        for (app, version, expected) in [
            (&new_app, "3.10.0", true),
            (&old_app, "3.10.0", false),
            (&new_app, "3.9.1", false),
            (&new_app, "3.8.9", false),
            (&new_app, "invalid", false),
        ] {
            let installation = Installation {
                app: app.clone(),
                store: dir.path().join("store"),
                version: version.into(),
                elevated: false,
            };
            assert_eq!(installation.is_package_update(&record), expected);
            assert!(!installation.owned(&record).unwrap());
        }
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
            selection: None,
            files: None,
        };
        fs::rename(
            app.join(FILES[0]),
            installation.sibling(&record, FILES[0], "previous"),
        )
        .unwrap();
        fs::write(installation.sibling(&record, FILES[0], "new"), b"partial").unwrap();
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
            selection: None,
            files: None,
        };
        for i in 0..2 {
            fs::write(installation.sibling(&record, FILES[i], "new"), b"modified").unwrap();
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

    /// 副本上跑完整事务：CLAUDEPP_WINDOWS_FIXTURE 指向官方 app 目录的独立副本
    #[test]
    #[ignore = "需要官方应用副本"]
    fn combinations_switch_and_restore_every_file() {
        let app = PathBuf::from(std::env::var_os("CLAUDEPP_WINDOWS_FIXTURE").expect("缺少副本路径"));
        let store = app.parent().unwrap().join("probe-store");
        let _ = fs::remove_dir_all(&store);
        let before: Vec<_> = FILES.iter().map(|f| digest(&app.join(f)).unwrap()).collect();
        let installation = Installation::copy(&app, &store).unwrap();
        let computer_use = Selection {
            computer_use: true,
            ..Default::default()
        };
        let full = Selection {
            computer_use: true,
            chinese: true,
            prewarm: true,
            fast_mode_status: true,
            simulators: false,
        };
        let chinese = Selection {
            chinese: true,
            ..Default::default()
        };
        let fast = Selection {
            fast_mode_status: true,
            ..Default::default()
        };
        for selection in [computer_use, full, chinese, fast] {
            installation.apply(selection).unwrap();
            assert_eq!(verify_pair(&app, Scope::App).unwrap(), selection);
            assert_eq!(
                super::super::native::catalogs::present(&app.join(CATALOGS)).unwrap(),
                selection.chinese
            );
            if selection.mask() == 0 {
                let current: Vec<_> = FILES.iter().map(|f| digest(&app.join(f)).unwrap()).collect();
                assert_eq!(before, current, "只装 renderer 功能时 exe 与 ASAR 不该变");
            }
            let status = installation.status().unwrap();
            assert_eq!(status.state, "installed");
            assert!(status.can_install, "已装状态必须允许换组合");
        }

        installation.restore().unwrap();
        assert!(verify_pair(&app, Scope::App).unwrap().is_empty());
        assert!(!super::super::native::catalogs::present(&app.join(CATALOGS)).unwrap());
        let after: Vec<_> = FILES.iter().map(|f| digest(&app.join(f)).unwrap()).collect();
        assert_eq!(before, after, "还原后文件未回到原始内容");
        assert_eq!(
            walkdir_count(&app, "cpp-cu-"),
            0,
            "还原后仍有替换暂存文件"
        );
    }

    #[cfg(test)]
    fn walkdir_count(root: &Path, needle: &str) -> usize {
        let mut count = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.to_string_lossy().contains(needle) {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn legacy_fast_patch_is_detected_by_its_backups() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!legacy_fast_patch(&dir.path().join("missing")).unwrap());
        fs::write(dir.path().join("chunk.js"), b"patched").unwrap();
        assert!(!legacy_fast_patch(dir.path()).unwrap());
        fs::write(dir.path().join("chunk.js.orig"), b"original").unwrap();
        assert!(legacy_fast_patch(dir.path()).unwrap());
    }

    #[test]
    fn unchanged_entries_are_left_in_place() {
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
        let original = digest(&app.join(FILES[0])).unwrap();
        let record = Record {
            schema: 2,
            id: Uuid::new_v4(),
            target: app.clone(),
            version: installation.version.clone(),
            baseline: hashes(&app).unwrap(),
            modified: hashes(&app).unwrap(),
            phase: "installing".into(),
            selection: Some(Selection {
                chinese: true,
                ..Default::default()
            }),
            files: Some(vec![
                FileRecord {
                    path: FILES[0].into(),
                    baseline: Some(original.clone()),
                    modified: Some(original.clone()),
                },
                FileRecord {
                    path: FILES[1].into(),
                    baseline: Some(original.clone()),
                    modified: Some(asar::hash(b"modified")),
                },
                FileRecord {
                    path: "resources/added.json".into(),
                    baseline: None,
                    modified: Some(asar::hash(b"added")),
                },
            ]),
        };
        fs::write(installation.sibling(&record, FILES[1], "new"), b"modified").unwrap();
        fs::write(installation.sibling(&record, "resources/added.json", "new"), b"added").unwrap();
        let mut moved = Vec::new();
        installation
            .commit_files(&record, |from, to| {
                moved.push(to.to_path_buf());
                fs::rename(from, to)
            })
            .unwrap();
        let skipped = [
            app.join(FILES[0]),
            installation.sibling(&record, FILES[0], "previous"),
        ];
        assert!(moved.iter().all(|path| !skipped.contains(path)));
        assert!(!skipped[1].exists());
        assert_eq!(fs::read(app.join(FILES[0])).unwrap(), b"original");
        assert_eq!(fs::read(app.join(FILES[1])).unwrap(), b"modified");
        assert_eq!(fs::read(app.join("resources/added.json")).unwrap(), b"added");
        assert!(installation.owned(&record).unwrap());
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

//! Claude Desktop 3p Fast Mode：wrapper 顶替 bundled CLI 注入 fastMode，renderer patch 亮出开关；
//! Desktop / CLI 版本变化后由 watch 线程自动修复。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const ELEVATED_FLAG: &str = "--fastmode-elevated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WrapperState {
    Deployed,
    Absent,
    Broken,
    NoCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RendererState {
    Patched,
    Pristine,
    Mismatch,
    NotFound,
    Unreadable,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererInfo {
    pub file: Option<String>,
    pub state: RendererState,
    pub missing_anchors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FastModeSettings {
    pub auto: bool,
    pub installed: bool,
    pub last_repair: Option<String>,
    pub patched_desktop_version: Option<String>,
    pub patched_cli_version: Option<String>,
    pub last_failure: Option<String>,
    /// 失败时的 "desktop|cli" 版本组合：同一组合不再自动重试，UAC 被拒后不会反复弹窗
    pub failed_for: Option<String>,
}

impl Default for FastModeSettings {
    fn default() -> Self {
        Self {
            auto: true,
            installed: false,
            last_repair: None,
            patched_desktop_version: None,
            patched_cli_version: None,
            last_failure: None,
            failed_for: None,
        }
    }
}

/// watch 线程自动修复的结果
#[derive(Debug, Clone)]
pub enum RepairOutcome {
    /// 未到检查间隔，什么都没看
    Skipped,
    Nothing,
    Repaired(String),
    /// Desktop 运行中，等它退出后重试
    Blocked,
    Failed(String),
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedStats {
    pub sessions: usize,
    pub fast: usize,
    pub standard: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FastModeStatus {
    pub supported: bool,
    pub desktop_version: Option<String>,
    pub msix_path: Option<PathBuf>,
    pub cli_version: Option<String>,
    pub cli_dir: Option<PathBuf>,
    pub wrapper: WrapperState,
    pub renderer: RendererInfo,
    pub settings: FastModeSettings,
    pub speed: SpeedStats,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReport {
    pub wrapper: String,
    pub renderer: String,
}

/// 提权子进程与主进程之间的回传格式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElevatedResult {
    pub ok: bool,
    pub message: String,
}

pub fn elevated_mode_from_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == ELEVATED_FLAG {
            return args.next();
        }
    }
    None
}

pub use imp::*;

#[cfg(windows)]
mod imp {
    use std::fs;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::procs;

    const WRAPPER_SOURCE: &str = include_str!("fastmode/wrapper.cs");
    const CSC: &str = r"C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 官方 exe 200 MB 上下，wrapper 几 KB：以 1 MB 分界判定顶替是否在位
    const WRAPPER_MAX_BYTES: u64 = 1024 * 1024;
    const HEAD_BYTES: usize = 4 * 1024;
    const TAIL_BYTES: u64 = 256 * 1024;
    const SPEED_SESSIONS: usize = 15;
    // Desktop 活跃时 userData 几秒一次写入都会唤醒 watch，自动检查含一次 powershell 与读 renderer，需节流
    const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(60);
    static LAST_AUTO_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

    struct Anchor {
        label: &'static str,
        from: &'static str,
        to: &'static str,
    }

    /// 基于 Desktop 1.46388.4.0 的 minified renderer；版本变化后三处 FROM 需重新定位
    const ANCHORS: [Anchor; 3] = [
        Anchor {
            label: "Bc 显示条件",
            from: r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}"#,
            to: r#"let u=a&&c;return{showFastModeToggle:u,fastModeToggleDisabled:!1}"#,
        },
        Anchor {
            label: "zc 模型支持判定",
            from: r#",S=b&&x,C=t(e=>o(F(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Ue()),[o]),w=b&&!x,"#,
            to: r#",S=!!(a&&(a.toLowerCase().includes("opus-5")||a.toLowerCase().includes("opus-4-8"))),C=t(e=>!!(e&&(e.toLowerCase().includes("opus-5")||e.toLowerCase().includes("opus-4-8"))),[]),w=!1,"#,
        },
        Anchor {
            label: "zc 禁用原因",
            from: r#"D=Vc(r?.fastModeDisabledReason,{hasRaven:E,canManageOrg:m}),O=D!==null"#,
            to: r#"D=null,O=!1"#,
        },
    ];

    fn powershell(script: &str) -> Option<String> {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let output = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && !text.is_empty()).then_some(text)
    }

    fn quiet(program: &str, args: &[&str]) -> bool {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.output().map(|o| o.status.success()).unwrap_or(false)
    }

    /// (version, install root)；WindowsApps 根目录用户态不可列，只能从包注册表问
    fn msix() -> Option<(String, PathBuf)> {
        let text = powershell(
            "Get-AppxPackage -Name Claude | Select-Object -First 1 | ForEach-Object { $_.Version + '|' + $_.InstallLocation }",
        )?;
        let (version, path) = text.split_once('|')?;
        Some((version.to_string(), PathBuf::from(path)))
    }

    fn renderer_dir(msix_root: &Path) -> PathBuf {
        msix_root
            .join("app")
            .join("resources")
            .join("ion-dist")
            .join("assets")
            .join("v1")
    }

    fn cli_root() -> Option<PathBuf> {
        dirs::data_local_dir().map(|d| d.join("Claude-3p").join("claude-code"))
    }

    fn parse_version(name: &str) -> Option<Vec<u64>> {
        let parts: Vec<u64> = name.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        (!parts.is_empty()).then_some(parts)
    }

    fn latest_version_dir(root: &Path) -> Option<(String, PathBuf)> {
        let mut best: Option<(Vec<u64>, String, PathBuf)> = None;
        for entry in fs::read_dir(root).ok()?.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(version) = parse_version(&name) else { continue };
            if best.as_ref().map(|(v, _, _)| version > *v).unwrap_or(true) {
                best = Some((version, name, entry.path()));
            }
        }
        best.map(|(_, name, path)| (name, path))
    }

    fn wrapper_state(dir: &Path) -> WrapperState {
        let exe_len = fs::metadata(dir.join("claude.exe")).map(|m| m.len());
        let real = dir.join("claude-real.exe").is_file();
        match (exe_len, real) {
            (Ok(len), true) if len <= WRAPPER_MAX_BYTES => WrapperState::Deployed,
            (Ok(_), false) => WrapperState::Absent,
            (Err(_), false) => WrapperState::NoCli,
            _ => WrapperState::Broken,
        }
    }

    fn renderer_file(v1: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(v1).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if text.contains("function Bc(") && text.contains("showFastModeToggle") {
                return Some(path);
            }
        }
        None
    }

    fn classify(text: &str) -> (RendererState, Vec<String>) {
        if ANCHORS.iter().all(|a| text.contains(a.to)) {
            return (RendererState::Patched, Vec::new());
        }
        let missing: Vec<String> = ANCHORS
            .iter()
            .filter(|a| !text.contains(a.from))
            .map(|a| a.label.to_string())
            .collect();
        if missing.is_empty() {
            (RendererState::Pristine, Vec::new())
        } else {
            (RendererState::Mismatch, missing)
        }
    }

    fn apply_anchors(text: &str) -> String {
        ANCHORS
            .iter()
            .fold(text.to_string(), |acc, a| acc.replace(a.from, a.to))
    }

    fn renderer_info(msix_root: Option<&Path>) -> RendererInfo {
        let not_found = RendererInfo {
            file: None,
            state: RendererState::NotFound,
            missing_anchors: Vec::new(),
        };
        let Some(root) = msix_root else { return not_found };
        let Some(path) = renderer_file(&renderer_dir(root)) else { return not_found };
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
        match fs::read_to_string(&path) {
            Ok(text) => {
                let (state, missing_anchors) = classify(&text);
                RendererInfo { file, state, missing_anchors }
            }
            Err(_) => RendererInfo {
                file,
                state: RendererState::Unreadable,
                missing_anchors: Vec::new(),
            },
        }
    }

    fn settings_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("com.claudeplusplus.manager")
    }

    fn settings_path() -> PathBuf {
        settings_dir().join("fastmode.json")
    }

    fn load_settings_from(path: &Path) -> FastModeSettings {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_settings_to(path: &Path, settings: &FastModeSettings) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load_settings() -> FastModeSettings {
        load_settings_from(&settings_path())
    }

    fn save_settings(settings: &FastModeSettings) -> Result<()> {
        save_settings_to(&settings_path(), settings)
    }

    fn now_iso() -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
    }

    /// 最近若干个 3p Desktop 会话里 Opus 5 / 4.8 回复的 usage.speed 分布；
    /// 状态栏的 Fast 标签有官方显示 bug，转录里的 speed 才是服务端实际给的
    fn speed_stats() -> SpeedStats {
        let mut stats = SpeedStats::default();
        let Some(projects) = crate::discovery::cli_projects_dir() else { return stats };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for project in fs::read_dir(&projects).ok().into_iter().flatten().flatten() {
            for file in fs::read_dir(project.path()).ok().into_iter().flatten().flatten() {
                let path = file.path();
                let is_session = path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && path.file_stem().map(|s| s.len() == 36).unwrap_or(false);
                if !is_session {
                    continue;
                }
                if let Ok(modified) = file.metadata().and_then(|m| m.modified()) {
                    files.push((modified, path));
                }
            }
        }
        files.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in files {
            if stats.sessions >= SPEED_SESSIONS {
                break;
            }
            let Ok(mut file) = fs::File::open(&path) else { continue };
            let mut head = vec![0u8; HEAD_BYTES];
            let read = file.read(&mut head).unwrap_or(0);
            if !String::from_utf8_lossy(&head[..read]).contains("\"entrypoint\":\"claude-desktop-3p\"") {
                continue;
            }
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).is_err() {
                continue;
            }
            let mut tail = Vec::new();
            if file.read_to_end(&mut tail).is_err() {
                continue;
            }
            stats.sessions += 1;
            for line in String::from_utf8_lossy(&tail).lines() {
                if !(line.contains("claude-opus-5") || line.contains("claude-opus-4-8")) {
                    continue;
                }
                if line.contains("\"speed\":\"fast\"") {
                    stats.fast += 1;
                } else if line.contains("\"speed\":\"standard\"") {
                    stats.standard += 1;
                }
            }
        }
        stats
    }

    fn status_light() -> FastModeStatus {
        let msix = msix();
        let cli = cli_root().and_then(|root| latest_version_dir(&root));
        let wrapper = cli
            .as_ref()
            .map(|(_, dir)| wrapper_state(dir))
            .unwrap_or(WrapperState::NoCli);
        FastModeStatus {
            supported: true,
            desktop_version: msix.as_ref().map(|(v, _)| v.clone()),
            msix_path: msix.as_ref().map(|(_, p)| p.clone()),
            cli_version: cli.as_ref().map(|(v, _)| v.clone()),
            cli_dir: cli.as_ref().map(|(_, d)| d.clone()),
            wrapper,
            renderer: renderer_info(msix.as_ref().map(|(_, p)| p.as_path())),
            settings: load_settings(),
            speed: SpeedStats::default(),
        }
    }

    /// 用脚本装过的机器：wrapper 或 patch 已在位但账本没记，认领下来自动守护才会接手
    fn adopt_existing(status: &FastModeStatus, settings: &mut FastModeSettings) -> bool {
        if settings.installed {
            return false;
        }
        if status.wrapper != WrapperState::Deployed && status.renderer.state != RendererState::Patched {
            return false;
        }
        settings.installed = true;
        settings.patched_desktop_version = status.desktop_version.clone();
        settings.patched_cli_version = status.cli_version.clone();
        true
    }

    pub fn status() -> FastModeStatus {
        let mut status = status_light();
        let mut settings = status.settings.clone();
        if adopt_existing(&status, &mut settings) {
            let _ = save_settings(&settings);
            status.settings = settings;
        }
        status.speed = speed_stats();
        status
    }

    fn dir_label(dir: &Path) -> String {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// 顶替版本目录的 claude.exe。若 claude.exe 是官方大文件而 real 也在（Desktop 重下过），以新官方为准
    fn deploy_wrapper(dir: &Path) -> Result<String> {
        let exe = dir.join("claude.exe");
        let real = dir.join("claude-real.exe");
        let exe_len = fs::metadata(&exe).map(|m| m.len()).ok();
        match (exe_len, real.is_file()) {
            (Some(len), _) if len > WRAPPER_MAX_BYTES => {
                fs::rename(&exe, &real).context("藏起官方 claude.exe")?;
            }
            (Some(_), true) => {
                fs::remove_file(&exe).context("移除旧 wrapper")?;
            }
            (Some(_), false) => bail!("claude.exe 不是官方 CLI 且没有 claude-real.exe，目录状态异常"),
            (None, true) => {}
            (None, false) => bail!("版本目录缺少 claude.exe"),
        }
        if !Path::new(CSC).is_file() {
            bail!("未找到 .NET Framework 编译器 {CSC}");
        }
        let source = std::env::temp_dir().join("claude-plus-plus-fast-wrapper.cs");
        fs::write(&source, WRAPPER_SOURCE)?;
        let out = format!("-out:{}", exe.display());
        let source_arg = source.to_string_lossy().into_owned();
        let ok = quiet(CSC, &["-nologo", "-optimize", &out, &source_arg]);
        let _ = fs::remove_file(&source);
        if !ok || !exe.is_file() {
            bail!("wrapper 编译失败");
        }
        Ok(format!("wrapper 已部署到 {}", dir_label(dir)))
    }

    fn remove_wrapper(dir: &Path) -> Result<bool> {
        let exe = dir.join("claude.exe");
        let real = dir.join("claude-real.exe");
        if !real.is_file() {
            return Ok(false);
        }
        if exe.exists() {
            fs::remove_file(&exe).context("移除 wrapper")?;
        }
        fs::rename(&real, &exe).context("还原官方 claude.exe")?;
        Ok(true)
    }

    fn take_ownership(path: &Path) -> Result<()> {
        let file = path.to_string_lossy().into_owned();
        let user = std::env::var("USERNAME").context("读取 USERNAME")?;
        if !quiet("takeown", &["/f", &file]) {
            bail!("takeown 失败: {file}");
        }
        let grant = format!("{user}:F");
        if !quiet("icacls", &[&file, "/grant", &grant]) {
            bail!("icacls 失败: {file}");
        }
        Ok(())
    }

    /// 始终从 .orig 出发 patch，保证幂等；.orig 必须是官方原始，否则拒绝
    fn patch_renderer(v1: &Path) -> Result<String> {
        let live = renderer_file(v1).context("未找到含 function Bc( 的 renderer")?;
        take_ownership(&live)?;
        let orig = live.with_extension("js.orig");
        if !orig.exists() {
            fs::copy(&live, &orig).context("备份 .orig")?;
        }
        let source = fs::read_to_string(&orig).context("读取 .orig")?;
        match classify(&source) {
            (RendererState::Pristine, _) => {}
            (RendererState::Mismatch, missing) => {
                bail!("renderer 版本变化，锚点未命中: {}", missing.join("、"))
            }
            (state, _) => bail!("备份文件状态异常: {state:?}"),
        }
        fs::write(&live, apply_anchors(&source)).context("写回 renderer")?;
        Ok(format!("renderer 已 patch: {}", dir_label(&live)))
    }

    fn restore_renderer(v1: &Path) -> Result<usize> {
        let mut restored = 0;
        for entry in fs::read_dir(v1).context("读取 renderer 目录")?.flatten() {
            let orig = entry.path();
            if !orig.to_string_lossy().ends_with(".js.orig") {
                continue;
            }
            let live = orig.with_extension("");
            take_ownership(&live)?;
            fs::copy(&orig, &live).with_context(|| format!("还原 {}", live.display()))?;
            fs::remove_file(&orig)?;
            restored += 1;
        }
        Ok(restored)
    }

    fn elevated_result_path() -> PathBuf {
        settings_dir().join("fastmode-elevated.json")
    }

    /// 以管理员重新启动自身执行 renderer 写操作；子进程不接受路径参数，自行定位 MSIX
    fn run_with_elevation(mode: &str) -> Result<String> {
        let exe = std::env::current_exe().context("定位自身 exe")?;
        let result_path = elevated_result_path();
        let _ = fs::remove_file(&result_path);
        fs::create_dir_all(settings_dir())?;
        let script = format!(
            "Start-Process -FilePath '{}' -ArgumentList '{ELEVATED_FLAG}','{mode}' -Verb RunAs -Wait",
            exe.to_string_lossy().replace('\'', "''")
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let output = cmd.output().context("启动提权进程")?;
        if !output.status.success() {
            bail!("提权被取消或失败（修改 WindowsApps 下的 renderer 需要管理员权限）");
        }
        let text = fs::read_to_string(&result_path).context("提权进程未返回结果")?;
        let result: ElevatedResult = serde_json::from_str(&text)?;
        let _ = fs::remove_file(&result_path);
        if result.ok {
            Ok(result.message)
        } else {
            bail!("{}", result.message)
        }
    }

    /// 提权子进程入口：只认 apply / restore，结果写文件供主进程读取
    pub fn run_elevated(mode: &str) -> i32 {
        let outcome = (|| -> Result<String> {
            let (_, root) = msix().context("未找到 Claude Desktop MSIX 包")?;
            let v1 = renderer_dir(&root);
            match mode {
                "apply" => patch_renderer(&v1),
                "restore" => restore_renderer(&v1).map(|n| format!("已还原 {n} 个 renderer 文件")),
                other => bail!("未知模式 {other}"),
            }
        })();
        let result = match outcome {
            Ok(message) => ElevatedResult { ok: true, message },
            Err(error) => ElevatedResult {
                ok: false,
                message: error.to_string(),
            },
        };
        let _ = fs::create_dir_all(settings_dir());
        let written = serde_json::to_string(&result)
            .ok()
            .and_then(|json| fs::write(elevated_result_path(), json).ok())
            .is_some();
        if result.ok && written {
            0
        } else {
            1
        }
    }

    fn record_repair(settings: &mut FastModeSettings, status: &FastModeStatus) {
        settings.installed = true;
        settings.last_repair = Some(now_iso());
        settings.patched_desktop_version = status.desktop_version.clone();
        settings.patched_cli_version = status.cli_version.clone();
        settings.last_failure = None;
        settings.failed_for = None;
    }

    fn version_key(status: &FastModeStatus) -> String {
        format!(
            "{}|{}",
            status.desktop_version.as_deref().unwrap_or("-"),
            status.cli_version.as_deref().unwrap_or("-")
        )
    }

    pub fn install() -> Result<ActionReport> {
        if procs::any_desktop_running() {
            bail!("Claude Desktop 正在运行，请先退出后再安装");
        }
        let status = status_light();
        let (_, cli_dir) = status
            .cli_version
            .clone()
            .zip(status.cli_dir.clone())
            .context("未找到 bundled CLI 版本目录（Claude-3p\\claude-code）")?;
        let wrapper = deploy_wrapper(&cli_dir)?;
        let renderer = match status.renderer.state {
            RendererState::Patched => "renderer 已是 patch 状态".to_string(),
            RendererState::Pristine => run_with_elevation("apply")?,
            RendererState::Mismatch => format!(
                "renderer 锚点未命中（{}），跳过；wrapper 仍让 Opus 会话默认 fast",
                status.renderer.missing_anchors.join("、")
            ),
            RendererState::NotFound => "未找到 renderer 文件，跳过".to_string(),
            RendererState::Unreadable => "renderer 文件不可读，跳过".to_string(),
        };
        let mut settings = load_settings();
        record_repair(&mut settings, &status);
        save_settings(&settings)?;
        Ok(ActionReport { wrapper, renderer })
    }

    pub fn uninstall() -> Result<ActionReport> {
        if procs::any_desktop_running() {
            bail!("Claude Desktop 正在运行，请先退出后再还原");
        }
        let mut restored = 0;
        if let Some(root) = cli_root() {
            for entry in fs::read_dir(&root).ok().into_iter().flatten().flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && remove_wrapper(&entry.path())?
                {
                    restored += 1;
                }
            }
        }
        let wrapper = format!("已还原 {restored} 个版本目录的官方 CLI");
        let status = status_light();
        let renderer = match status.renderer.state {
            RendererState::Patched => run_with_elevation("restore")?,
            _ => "renderer 无需还原".to_string(),
        };
        let mut settings = load_settings();
        settings.installed = false;
        settings.patched_desktop_version = None;
        settings.patched_cli_version = None;
        save_settings(&settings)?;
        Ok(ActionReport { wrapper, renderer })
    }

    pub fn set_auto(auto: bool) -> Result<FastModeSettings> {
        let mut settings = load_settings();
        settings.auto = auto;
        save_settings(&settings)?;
        Ok(settings)
    }

    /// watch 线程调用：装过且开了自动守护，wrapper 或 renderer 失效就补上
    pub fn auto_repair() -> RepairOutcome {
        let mut settings = load_settings();
        if !(settings.installed && settings.auto) {
            return RepairOutcome::Nothing;
        }
        {
            let mut last = LAST_AUTO_CHECK.lock().unwrap();
            if last.map(|t| t.elapsed() < AUTO_CHECK_INTERVAL).unwrap_or(false) {
                return RepairOutcome::Skipped;
            }
            *last = Some(Instant::now());
        }
        let status = status_light();
        let key = version_key(&status);
        if settings.failed_for.as_deref() == Some(key.as_str()) {
            return RepairOutcome::Nothing;
        }
        let need_wrapper = status.cli_dir.is_some() && status.wrapper != WrapperState::Deployed;
        let need_renderer = status.renderer.state == RendererState::Pristine;
        if !need_wrapper && !need_renderer {
            return RepairOutcome::Nothing;
        }
        if procs::any_desktop_running() {
            return RepairOutcome::Blocked;
        }
        let attempt = (|| -> Result<String> {
            let mut notes = Vec::new();
            if need_wrapper {
                if let Some(dir) = status.cli_dir.as_deref() {
                    notes.push(deploy_wrapper(dir)?);
                }
            }
            if need_renderer {
                notes.push(run_with_elevation("apply")?);
            }
            Ok(notes.join("；"))
        })();
        match attempt {
            Ok(summary) => {
                record_repair(&mut settings, &status);
                let _ = save_settings(&settings);
                RepairOutcome::Repaired(summary)
            }
            Err(error) => {
                settings.last_failure = Some(error.to_string());
                settings.failed_for = Some(key);
                let _ = save_settings(&settings);
                RepairOutcome::Failed(error.to_string())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn pristine_text() -> String {
            format!(
                "import x;function Bc(){{{}}}function zc(){{{}{}}}showFastModeToggle",
                ANCHORS[0].from, ANCHORS[1].from, ANCHORS[2].from
            )
        }

        #[test]
        fn classify_three_states() {
            let pristine = pristine_text();
            assert_eq!(classify(&pristine).0, RendererState::Pristine);
            let patched = apply_anchors(&pristine);
            assert_eq!(classify(&patched).0, RendererState::Patched);
            // 再 patch 一次不变：幂等
            assert_eq!(apply_anchors(&patched), patched);
            let drifted = pristine.replace(ANCHORS[1].from, "S=something_else,");
            let (state, missing) = classify(&drifted);
            assert_eq!(state, RendererState::Mismatch);
            assert_eq!(missing, vec!["zc 模型支持判定".to_string()]);
        }

        #[test]
        fn latest_version_dir_picks_numeric_max() {
            let dir = tempfile::tempdir().unwrap();
            for name in ["2.1.258", "2.1.260", "2.1.9", "junk", "2.1.260-beta"] {
                fs::create_dir(dir.path().join(name)).unwrap();
            }
            let (version, _) = latest_version_dir(dir.path()).unwrap();
            assert_eq!(version, "2.1.260");
        }

        #[test]
        fn wrapper_state_by_size_and_real() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::NoCli);
            fs::write(dir.path().join("claude.exe"), vec![0u8; 2 * 1024 * 1024]).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Absent);
            fs::write(dir.path().join("claude-real.exe"), b"real").unwrap();
            // 官方大 exe 与 real 并存：Desktop 重下覆盖了 wrapper
            assert_eq!(wrapper_state(dir.path()), WrapperState::Broken);
            fs::write(dir.path().join("claude.exe"), b"tiny wrapper").unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            fs::remove_file(dir.path().join("claude.exe")).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Broken);
        }

        #[test]
        fn remove_wrapper_puts_official_back() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("claude.exe"), b"wrapper").unwrap();
            fs::write(dir.path().join("claude-real.exe"), b"official").unwrap();
            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(dir.path().join("claude.exe")).unwrap(), b"official");
            assert!(!dir.path().join("claude-real.exe").exists());
            // 没装过的目录什么都不做
            assert!(!remove_wrapper(dir.path()).unwrap());
        }

        #[test]
        fn settings_roundtrip_and_defaults() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nested").join("fastmode.json");
            let defaults = load_settings_from(&path);
            assert!(defaults.auto && !defaults.installed);
            let mut s = defaults;
            s.installed = true;
            s.patched_cli_version = Some("2.1.260".into());
            save_settings_to(&path, &s).unwrap();
            let back = load_settings_from(&path);
            assert!(back.installed);
            assert_eq!(back.patched_cli_version.as_deref(), Some("2.1.260"));
            // 旧文件缺字段也能读：serde default
            fs::write(&path, r#"{"installed":true}"#).unwrap();
            let partial = load_settings_from(&path);
            assert!(partial.installed && partial.auto);
        }

        fn status_with(wrapper: WrapperState, renderer: RendererState) -> FastModeStatus {
            FastModeStatus {
                supported: true,
                desktop_version: Some("1.46388.4.0".into()),
                msix_path: None,
                cli_version: Some("2.1.260".into()),
                cli_dir: None,
                wrapper,
                renderer: RendererInfo { file: None, state: renderer, missing_anchors: Vec::new() },
                settings: FastModeSettings::default(),
                speed: SpeedStats::default(),
            }
        }

        #[test]
        fn adopt_existing_install_from_script() {
            let mut settings = FastModeSettings::default();
            // 官方原样：没什么可认领
            assert!(!adopt_existing(&status_with(WrapperState::Absent, RendererState::Pristine), &mut settings));
            assert!(!settings.installed);
            // wrapper 在位即认领，并记下当前版本组合
            assert!(adopt_existing(&status_with(WrapperState::Deployed, RendererState::Pristine), &mut settings));
            assert!(settings.installed);
            assert_eq!(settings.patched_cli_version.as_deref(), Some("2.1.260"));
            // 已记账的不再重复认领
            assert!(!adopt_existing(&status_with(WrapperState::Deployed, RendererState::Patched), &mut settings));
            // 只剩 renderer patch 也算装过
            let mut fresh = FastModeSettings::default();
            assert!(adopt_existing(&status_with(WrapperState::Absent, RendererState::Patched), &mut fresh));
        }

        #[test]
        fn version_parsing() {
            assert_eq!(parse_version("2.1.260"), Some(vec![2, 1, 260]));
            assert_eq!(parse_version("2.1.260-beta"), None);
            assert_eq!(parse_version(""), None);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{bail, Result};

    use super::*;

    pub fn load_settings() -> FastModeSettings {
        FastModeSettings::default()
    }

    pub fn status() -> FastModeStatus {
        FastModeStatus {
            supported: false,
            desktop_version: None,
            msix_path: None,
            cli_version: None,
            cli_dir: None,
            wrapper: WrapperState::NoCli,
            renderer: RendererInfo {
                file: None,
                state: RendererState::NotFound,
                missing_anchors: Vec::new(),
            },
            settings: FastModeSettings::default(),
            speed: SpeedStats::default(),
        }
    }

    pub fn install() -> Result<ActionReport> {
        bail!("Fast Mode 解锁仅支持 Windows 版 Claude Desktop")
    }

    pub fn uninstall() -> Result<ActionReport> {
        bail!("Fast Mode 解锁仅支持 Windows 版 Claude Desktop")
    }

    pub fn set_auto(_auto: bool) -> Result<FastModeSettings> {
        bail!("Fast Mode 解锁仅支持 Windows 版 Claude Desktop")
    }

    pub fn auto_repair() -> RepairOutcome {
        RepairOutcome::Nothing
    }

    pub fn run_elevated(_mode: &str) -> i32 {
        1
    }
}

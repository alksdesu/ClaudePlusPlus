//! Claude Desktop 3p Fast Mode：wrapper 顶替 bundled CLI 注入 fastMode；Windows 另有 renderer patch
//! 亮出实时开关（macOS 上会被 Gatekeeper 拒），版本变化后由 watch 线程自动修复。

// Windows / macOS 之外只有 stub，共享的 wrapper 与统计逻辑在那里用不到
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
    /// macOS：改 ion-dist 会让 Gatekeeper 判定 app 已损坏并拒绝启动，只能上 wrapper
    Unsupported,
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

const HEAD_BYTES: usize = 4 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;
const SPEED_SESSIONS: usize = 15;
// Desktop 活跃时 userData 几秒一次写入都会唤醒 watch，自动检查含一次进程扫描与读文件，需节流
const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(60);
static LAST_AUTO_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

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

fn dir_label(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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

/// CLI 槽位上现在放着什么。两平台的识别方式不同（Windows 看体积，macOS 看是不是软链），
/// 状态机共用
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotOccupant {
    Wrapper,
    Official,
    Empty,
}

fn wrapper_state_from(slot: SlotOccupant, real_present: bool) -> WrapperState {
    match (slot, real_present) {
        (SlotOccupant::Wrapper, true) => WrapperState::Deployed,
        (SlotOccupant::Empty, false) => WrapperState::NoCli,
        (SlotOccupant::Empty, _) => WrapperState::Broken,
        (_, false) => WrapperState::Absent,
        _ => WrapperState::Broken,
    }
}

/// 空出 wrapper 的位置。官方本体改名让位；若官方已被 Desktop 重新下载覆盖，以新官方为准
fn stage_wrapper_slot(exe: &Path, real: &Path, slot: SlotOccupant) -> Result<()> {
    let name = dir_label(exe);
    let real_name = dir_label(real);
    match (slot, real.is_file()) {
        (SlotOccupant::Official, _) => {
            fs::rename(exe, real).with_context(|| format!("藏起官方 {name}"))?;
        }
        (SlotOccupant::Wrapper, true) => {
            fs::remove_file(exe).context("移除旧 wrapper")?;
        }
        (SlotOccupant::Wrapper, false) => {
            anyhow::bail!("{name} 不是官方 CLI 且没有 {real_name}，目录状态异常")
        }
        (SlotOccupant::Empty, true) => {}
        (SlotOccupant::Empty, false) => anyhow::bail!("版本目录缺少 {name}"),
    }
    Ok(())
}

fn remove_wrapper_in(exe: &Path, real: &Path) -> Result<bool> {
    if !real.is_file() {
        return Ok(false);
    }
    // 不能用 exists()：断链的软链它返回 false，可文件本身还占着位置
    if exe.symlink_metadata().is_ok() {
        fs::remove_file(exe).context("移除 wrapper")?;
    }
    fs::rename(real, exe).with_context(|| format!("还原官方 {}", dir_label(exe)))?;
    Ok(true)
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

fn auto_check_due() -> bool {
    let mut last = LAST_AUTO_CHECK.lock().unwrap();
    if last.map(|t| t.elapsed() < AUTO_CHECK_INTERVAL).unwrap_or(false) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

pub use imp::*;

#[cfg(windows)]
mod imp {
    use std::fs;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::procs;

    const WRAPPER_SOURCE: &str = include_str!("fastmode/wrapper.cs");
    const CSC: &str = r"C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 官方 exe 200 MB 上下，wrapper 几 KB：以 1 MB 分界认出谁占着槽位
    const WRAPPER_MAX_BYTES: u64 = 1024 * 1024;

    fn slot_occupant(exe: &Path) -> SlotOccupant {
        match fs::metadata(exe).map(|m| m.len()) {
            Ok(len) if len > WRAPPER_MAX_BYTES => SlotOccupant::Official,
            Ok(_) => SlotOccupant::Wrapper,
            Err(_) => SlotOccupant::Empty,
        }
    }

    /// 一处改动：pattern 定位原始代码，patched 识别已改的形态，两者都只该命中一次。
    /// regex crate 不支持反向引用，同一变量的多处出现改为独立命名捕获，由 aliases 声明必须同名
    struct Anchor {
        label: &'static str,
        pattern: &'static str,
        patched: &'static str,
        aliases: &'static [(&'static str, &'static str)],
    }

    /// minify 只重命名局部变量与函数，属性名和字符串字面量不动 —— 锚点拿它们当骨架，
    /// 变量名捕获后回填，Desktop 更新重命名符号也不会失配
    struct Renderer<'a> {
        text: &'a str,
    }

    const ANCHORS: [Anchor; 3] = [
        Anchor {
            label: "显示条件",
            pattern: concat!(
                r"(?P<sig>fastModeIpcAvailable:(?P<ipc>\w+),(?:\w+:\w+,)*?modelSupportsFastMode:(?P<sup>\w+),",
                r"(?:\w+:\w+,)*?\w+:\w+\}\)\{)let (?P<show>\w+)=[^;]+;",
                r"return\{showFastModeToggle:(?P<show2>\w+),fastModeToggleDisabled:[^}]*\}",
            ),
            patched: r"return\{showFastModeToggle:\w+,fastModeToggleDisabled:!1\}",
            aliases: &[("show", "show2")],
        },
        Anchor {
            label: "模型支持判定",
            pattern: concat!(
                r",(?P<sup>\w+)=(?P<has>\w+)&&(?P<is46>\w+),(?P<fnc>\w+)=(?P<cb>\w+)\((?P<arg>\w+)=>",
                r"\w+\(\w+\((?P<arg2>\w+)\)\)!==void 0&&",
                r#"\((?P<arg3>\w+)\.toLowerCase\(\)\.includes\("opus-4-6"\)\|\|\w+\(\)\),\[\w+\]\),"#,
                r"(?P<needs>\w+)=(?P<has2>\w+)&&!(?P<is46b>\w+),",
            ),
            patched: r#",\w+=!!\(\w+&&\(\w+\.toLowerCase\(\)\.includes\("opus-5"\)"#,
            aliases: &[("arg", "arg2"), ("arg", "arg3"), ("has", "has2"), ("is46", "is46b")],
        },
        Anchor {
            label: "禁用原因",
            pattern: concat!(
                r"(?P<msg>\w+)=\w+\(\w+\?\.fastModeDisabledReason,",
                r"\{hasRaven:\w+,canManageOrg:\w+\}\),(?P<flag>\w+)=(?P<msg2>\w+)!==null",
            ),
            patched: r",\w+=null,\w+=!1,",
            aliases: &[("msg", "msg2")],
        },
    ];

    /// 目标函数的 modelId 形参：全文有多个 modelId: 属性，取被改代码所在函数的那个
    const MODEL_ID_SIGNATURE: &str = r"function \w+\(\{[^{}]*?modelId:(\w+)[^{}]*?\}\)\{";

    fn rx(pattern: &str) -> regex::Regex {
        regex::Regex::new(pattern).expect("锚点正则")
    }

    /// 恰好一处命中才可信：零处说明版本漂移，多处说明骨架太松会误伤
    fn unique_match<'a>(
        text: &'a str,
        pattern: &str,
        aliases: &[(&str, &str)],
    ) -> Option<regex::Captures<'a>> {
        let re = rx(pattern);
        let mut hits = re.captures_iter(text).filter(|c| {
            aliases
                .iter()
                .all(|(a, b)| c.name(a).map(|m| m.as_str()) == c.name(b).map(|m| m.as_str()))
        });
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    }

    impl<'a> Renderer<'a> {
        fn new(text: &'a str) -> Self {
            Self { text }
        }

        fn find(&self, anchor: &Anchor) -> Option<regex::Captures<'a>> {
            unique_match(self.text, anchor.pattern, anchor.aliases)
        }

        fn model_id_before(&self, offset: usize) -> Option<String> {
            rx(MODEL_ID_SIGNATURE)
                .captures_iter(&self.text[..offset])
                .last()
                .map(|c| c[1].to_string())
        }

        fn is_patched(&self) -> bool {
            ANCHORS
                .iter()
                .all(|a| unique_match(self.text, a.patched, &[]).is_some())
        }

        fn missing(&self) -> Vec<String> {
            ANCHORS
                .iter()
                .filter(|a| self.find(a).is_none())
                .map(|a| a.label.to_string())
                .collect()
        }

        /// 逐处改写：显示条件收敛为 IPC 可用且模型支持，模型支持改按 ID 判定，禁用原因清空
        fn patch(&self) -> Result<String> {
            let show = self.find(&ANCHORS[0]).context("显示条件锚点未唯一命中")?;
            let replacement = format!(
                "{}let {}={}&&{};return{{showFastModeToggle:{},fastModeToggleDisabled:!1}}",
                &show["sig"], &show["show"], &show["ipc"], &show["sup"], &show["show"]
            );
            let mut text = self.text.replace(&show[0], &replacement);

            let support = {
                let staged = Renderer::new(&text);
                let caps = staged.find(&ANCHORS[1]).context("模型支持锚点未唯一命中")?;
                let whole = caps[0].to_string();
                let model_id = staged
                    .model_id_before(caps.get(0).unwrap().start())
                    .context("未能在目标函数签名里定位 modelId")?;
                // Opus 4.6 客户端认它但服务端已不再给 fast，只放 5 与 4.8
                let supports = |id: &str| {
                    format!(
                        r#"!!({id}&&({id}.toLowerCase().includes("opus-5")||{id}.toLowerCase().includes("opus-4-8")))"#
                    )
                };
                let replacement = format!(
                    ",{}={},{}={}({}=>{},[]),{}=!1,",
                    &caps["sup"],
                    supports(&model_id),
                    &caps["fnc"],
                    &caps["cb"],
                    &caps["arg"],
                    supports(&caps["arg"]),
                    &caps["needs"]
                );
                (whole, replacement)
            };
            text = text.replace(&support.0, &support.1);

            let reason = {
                let staged = Renderer::new(&text);
                let caps = staged.find(&ANCHORS[2]).context("禁用原因锚点未唯一命中")?;
                (caps[0].to_string(), format!("{}=null,{}=!1", &caps["msg"], &caps["flag"]))
            };
            Ok(text.replace(&reason.0, &reason.1))
        }
    }

    fn powershell(script: &str) -> Option<String> {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let output = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && !text.is_empty()).then_some(text)
    }

    fn quiet(program: &str, args: &[&str]) -> bool {
        run_checked(program, args).is_ok()
    }

    fn run_checked(program: &str, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let output = cmd.output().with_context(|| format!("执行 {program}"))?;
        if output.status.success() {
            return Ok(());
        }
        bail!("{program} 退出码 {:?}", output.status.code())
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

    fn wrapper_state(dir: &Path) -> WrapperState {
        wrapper_state_from(
            slot_occupant(&dir.join("claude.exe")),
            dir.join("claude-real.exe").is_file(),
        )
    }

    /// 认文件靠属性名：函数名随 minify 变，showFastModeToggle 也出现在别的 bundle 里，
    /// fastModeToggleDisabled 才只属于要改的那个
    fn renderer_file(v1: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(v1).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if text.contains("fastModeToggleDisabled") && text.contains("fastModeDisabledReason") {
                return Some(path);
            }
        }
        None
    }

    fn classify(text: &str) -> (RendererState, Vec<String>) {
        let renderer = Renderer::new(text);
        if renderer.is_patched() {
            return (RendererState::Patched, Vec::new());
        }
        let missing = renderer.missing();
        if missing.is_empty() {
            (RendererState::Pristine, Vec::new())
        } else {
            (RendererState::Mismatch, missing)
        }
    }

    /// 供 example 对真实 renderer 做端到端演练，不写文件
    pub fn probe_patch(text: &str) -> String {
        let before = classify(text);
        let patched = match Renderer::new(text).patch() {
            Ok(patched) => patched,
            Err(error) => return format!("before={:?} patch failed: {error}", before.0),
        };
        let after = classify(&patched);
        let shown = |pattern: &str| {
            rx(pattern)
                .find(&patched)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default()
        };
        format!(
            "before={:?} after={:?} delta={} bytes\n  show: {}\n  supports: {}",
            before.0,
            after.0,
            patched.len() as i64 - text.len() as i64,
            shown(r"return\{showFastModeToggle:[^}]*\}"),
            shown(r",\w+=!!\(\w+&&\(\w+\.toLowerCase\(\)[^,]*,"),
        )
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

    /// 顶替版本目录的 claude.exe
    fn deploy_wrapper(dir: &Path) -> Result<String> {
        let exe = dir.join("claude.exe");
        let real = dir.join("claude-real.exe");
        stage_wrapper_slot(&exe, &real, slot_occupant(&exe))?;
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
        remove_wrapper_in(&dir.join("claude.exe"), &dir.join("claude-real.exe"))
    }

    /// WindowsApps 下 TrustedInstaller 与 SYSTEM 之外无人可写，管理员也不行。
    /// 改文件内容只需文件权限，新建或删除 .orig 是在目录里增删条目，还要目录权限。
    fn grant_write(path: &Path) -> Result<()> {
        let target = path.to_string_lossy().into_owned();
        let user = std::env::var("USERNAME").context("读取 USERNAME")?;
        run_checked("takeown", &["/f", &target])
            .with_context(|| format!("接管 {target}"))?;
        // 目录加 (OI)(CI)，让之后建出来的 .orig 直接继承可写
        let grant = if path.is_dir() {
            format!("{user}:(OI)(CI)F")
        } else {
            format!("{user}:F")
        };
        run_checked("icacls", &[&target, "/grant", &grant])
            .with_context(|| format!("授权 {target}"))?;
        Ok(())
    }

    /// 始终从 .orig 出发 patch，保证幂等；.orig 必须是官方原始，否则拒绝
    fn patch_renderer(v1: &Path) -> Result<String> {
        let live = renderer_file(v1).context("未找到 fast mode 所在的 renderer")?;
        let orig = live.with_extension("js.orig");
        if !orig.exists() {
            grant_write(v1)?;
            fs::copy(&live, &orig).context("备份 .orig")?;
        }
        grant_write(&live)?;
        let source = fs::read_to_string(&orig).context("读取 .orig")?;
        match classify(&source) {
            (RendererState::Pristine, _) => {}
            (RendererState::Mismatch, missing) => {
                bail!("renderer 版本变化，锚点未命中: {}", missing.join("、"))
            }
            (state, _) => bail!("备份文件状态异常: {state:?}"),
        }
        let patched = Renderer::new(&source).patch()?;
        fs::write(&live, patched).context("写回 renderer")?;
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
            grant_write(&live)?;
            fs::copy(&orig, &live).with_context(|| format!("还原 {}", live.display()))?;
            // 删 .orig 同样是改目录条目
            grant_write(v1)?;
            fs::remove_file(&orig).with_context(|| format!("移除 {}", orig.display()))?;
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

    /// Windows 的 wrapper 是 csc 编译出的独立 exe，主程序不兼任这个角色
    pub fn wrapper_argv0() -> Option<PathBuf> {
        None
    }

    pub fn run_wrapper(_argv0: &Path) -> i32 {
        1
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
                message: format!("{error:#}"),
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
            RendererState::Unsupported => "该平台不 patch renderer".to_string(),
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
        if !auto_check_due() {
            return RepairOutcome::Skipped;
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
                settings.last_failure = Some(format!("{error:#}"));
                settings.failed_for = Some(key);
                let _ = save_settings(&settings);
                RepairOutcome::Failed(format!("{error:#}"))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 1.46388.4.0 的真实片段：变量名与 1.49585 那版不同，用来钉住跨版本能力
        const OLD_RENDERER: &str = concat!(
            r#"function zc({sessionRef:t,sessionMeta:n,selectedFolder:r,modelId:a,fastModeFor:o,capabilities:s,config:l,openingKey:u}){"#,
            r#"let b=i?o(F(i))!==void 0:!1,x=(a?.toLowerCase().includes("opus-4-6")??!1)||Ue(),"#,
            r#",S=b&&x,C=t(e=>o(F(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Ue()),[o]),w=b&&!x,"#,
            r#"D=Vc(r?.fastModeDisabledReason,{hasRaven:E,canManageOrg:m}),O=D!==null,j=1}"#,
            r#"function Bc({isNew:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,perSessionOptInAllowed:o,"#,
            r#"fastModeNeedsDesktopUpdate:s,modelSupportsFastMode:c,fastModeBlocked:l}){"#,
            r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;"#,
            r#"return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}}"#,
        );

        /// 1.49585.0.0 的真实片段：函数改名 zc→df / Bc→xf，modelId 形参 a→i
        const NEW_RENDERER: &str = concat!(
            r#"function df({sessionRef:t,sessionMeta:n,selectedFolder:r,modelId:i,fastModeFor:o,capabilities:s,config:l,openingKey:u}){"#,
            r#"let b=i?o(D(i))!==void 0:!1,x=(i?.toLowerCase().includes("opus-4-6")??!1)||Be(),"#,
            r#",S=b&&x,C=e(e=>o(D(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Be()),[o]),w=b&&!x,"#,
            r#"O=pf(n?.fastModeDisabledReason,{hasRaven:E,canManageOrg:h}),A=O!==null,j=1}"#,
            r#"function xf({isNew:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,perSessionOptInAllowed:o,"#,
            r#"fastModeNeedsDesktopUpdate:s,modelSupportsFastMode:c,fastModeBlocked:l}){"#,
            r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;"#,
            r#"return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}}"#,
        );

        #[test]
        fn patches_both_desktop_versions() {
            for (label, source) in [("1.46388", OLD_RENDERER), ("1.49585", NEW_RENDERER)] {
                assert_eq!(classify(source).0, RendererState::Pristine, "{label} 原始识别");
                let patched = Renderer::new(source).patch().expect(label);
                assert_eq!(classify(&patched).0, RendererState::Patched, "{label} patch 后识别");
                assert!(patched.contains("fastModeToggleDisabled:!1"), "{label} 开关不再禁用");
                assert!(!patched.contains("fastModeDisabledReason,"), "{label} 禁用原因已断开");
                assert!(patched.contains(r#"includes("opus-4-8")"#), "{label} 放行 4.8");
                // 4.6 只该留在被弃用的 x 定义里，判定里不该再出现
                assert!(!patched.contains(r#"includes("opus-4-6")||"#), "{label} 不再认 4.6");
            }
        }

        #[test]
        fn model_id_comes_from_the_target_function() {
            // 两版的 modelId 形参不同名，硬编码任一个都会在另一版生成引用错变量的代码
            for (source, model_id, other) in [(OLD_RENDERER, "a", "i"), (NEW_RENDERER, "i", "a")] {
                let patched = Renderer::new(source).patch().unwrap();
                let expected = format!(r#"!!({model_id}&&({model_id}.toLowerCase()"#);
                assert!(patched.contains(&expected), "应引用 {model_id}");
                assert!(
                    !patched.contains(&format!(r#"!!({other}&&({other}.toLowerCase()"#)),
                    "不该引用 {other}"
                );
            }
        }

        #[test]
        fn patch_is_idempotent() {
            let once = Renderer::new(NEW_RENDERER).patch().unwrap();
            // 已改过的文本锚点不再命中，重复 patch 直接报错而非改坏
            assert!(Renderer::new(&once).patch().is_err());
            assert_eq!(classify(&once).0, RendererState::Patched);
        }

        #[test]
        fn drifted_anchor_is_reported_not_patched() {
            let drifted = NEW_RENDERER.replace(",S=b&&x,C=e(", ",S=totally_new_shape,C=e(");
            let (state, missing) = classify(&drifted);
            assert_eq!(state, RendererState::Mismatch);
            assert_eq!(missing, vec!["模型支持判定".to_string()]);
            assert!(Renderer::new(&drifted).patch().is_err());
        }

        #[test]
        fn ambiguous_match_is_refused() {
            // 骨架太松会误伤：同一形态出现两次即视为不可信
            let doubled = format!("{NEW_RENDERER}{NEW_RENDERER}");
            assert_eq!(classify(&doubled).0, RendererState::Mismatch);
            assert!(Renderer::new(&doubled).patch().is_err());
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::procs;

    const DESKTOP_APP: &str = "/Applications/Claude.app";
    const RENDERER_NOTE: &str =
        "macOS 不 patch renderer：改 ion-dist 会让 Gatekeeper 判定 app 已损坏并拒绝启动";
    const WRAPPER_NAME: &str = "claude";
    const REAL_NAME: &str = "claude-real";
    const FAST_ONLY: &str = r#"{"fastMode":true}"#;

    fn cli_root() -> Option<PathBuf> {
        dirs::data_local_dir().map(|d| d.join("Claude-3p").join("claude-code"))
    }

    /// 版本目录下 CLI 装在自己的 bundle 里，Desktop spawn 的是 bundle 内的可执行文件本体
    fn cli_bin_dir(version_dir: &Path) -> PathBuf {
        version_dir.join("claude.app").join("Contents").join("MacOS")
    }

    fn wrapper_slot(version_dir: &Path) -> (PathBuf, PathBuf) {
        let bin = cli_bin_dir(version_dir);
        (bin.join(WRAPPER_NAME), bin.join(REAL_NAME))
    }

    /// 软链就是我们的 wrapper。不能按体积认：Desktop 启动时只读前 8 字节验 Mach-O 魔数，
    /// 脚本会被判 not_macho 而重下整个 bundle，所以顶替物必须是软链到 Claude++ 本体
    fn slot_occupant(exe: &Path) -> SlotOccupant {
        match exe.symlink_metadata() {
            Ok(meta) if meta.is_symlink() => SlotOccupant::Wrapper,
            Ok(_) => SlotOccupant::Official,
            Err(_) => SlotOccupant::Empty,
        }
    }

    fn wrapper_state(version_dir: &Path) -> WrapperState {
        let (exe, real) = wrapper_slot(version_dir);
        wrapper_state_from(slot_occupant(&exe), real.is_file())
    }

    /// Info.plist 是 XML，取 CFBundleShortVersionString 不值得引 plist 依赖
    fn desktop_version() -> Option<String> {
        let text = fs::read_to_string(Path::new(DESKTOP_APP).join("Contents").join("Info.plist")).ok()?;
        let after_key = &text[text.find("<key>CFBundleShortVersionString</key>")?..];
        let start = after_key.find("<string>")? + "<string>".len();
        let end = after_key[start..].find("</string>")?;
        Some(after_key[start..start + end].trim().to_string())
    }

    /// 顶替 bundle 里的 claude：软链到 Claude++ 本体，它是合格的 Mach-O，能过 Desktop 的头部检查。
    /// bundle 签名随之失效，但 Desktop 用 posix_spawn 直接执行，不过 Gatekeeper
    fn deploy_wrapper(version_dir: &Path) -> Result<String> {
        let (exe, real) = wrapper_slot(version_dir);
        let target = std::env::current_exe().context("定位 Claude++ 自身")?;
        stage_wrapper_slot(&exe, &real, slot_occupant(&exe))?;
        std::os::unix::fs::symlink(&target, &exe)
            .with_context(|| format!("软链 {WRAPPER_NAME} -> {}", target.display()))?;
        Ok(format!("wrapper 已部署到 {}", dir_label(version_dir)))
    }

    fn remove_wrapper(version_dir: &Path) -> Result<bool> {
        let (exe, real) = wrapper_slot(version_dir);
        remove_wrapper_in(&exe, &real)
    }

    fn status_light() -> FastModeStatus {
        let cli = cli_root().and_then(|root| latest_version_dir(&root));
        let wrapper = cli
            .as_ref()
            .map(|(_, dir)| wrapper_state(dir))
            .unwrap_or(WrapperState::NoCli);
        FastModeStatus {
            supported: true,
            desktop_version: desktop_version(),
            msix_path: None,
            cli_version: cli.as_ref().map(|(v, _)| v.clone()),
            cli_dir: cli.as_ref().map(|(_, d)| d.clone()),
            wrapper,
            renderer: RendererInfo {
                file: None,
                state: RendererState::Unsupported,
                missing_anchors: Vec::new(),
            },
            settings: load_settings(),
            speed: SpeedStats::default(),
        }
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

    pub fn install() -> Result<ActionReport> {
        if procs::any_desktop_running() {
            bail!("Claude Desktop 正在运行，请先退出后再安装");
        }
        let status = status_light();
        let cli_dir = status
            .cli_dir
            .clone()
            .context("未找到 bundled CLI 版本目录（Claude-3p/claude-code）")?;
        let wrapper = deploy_wrapper(&cli_dir)?;
        let mut settings = load_settings();
        record_repair(&mut settings, &status);
        save_settings(&settings)?;
        Ok(ActionReport { wrapper, renderer: RENDERER_NOTE.to_string() })
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
        let mut settings = load_settings();
        settings.installed = false;
        settings.patched_desktop_version = None;
        settings.patched_cli_version = None;
        save_settings(&settings)?;
        Ok(ActionReport {
            wrapper: format!("已还原 {restored} 个版本目录的官方 CLI"),
            renderer: "renderer 无需还原".to_string(),
        })
    }

    pub fn set_auto(auto: bool) -> Result<FastModeSettings> {
        let mut settings = load_settings();
        settings.auto = auto;
        save_settings(&settings)?;
        Ok(settings)
    }

    /// watch 线程调用：装过且开了自动守护，Desktop 更新出新版本目录后把 wrapper 补上
    pub fn auto_repair() -> RepairOutcome {
        let mut settings = load_settings();
        if !(settings.installed && settings.auto) {
            return RepairOutcome::Nothing;
        }
        if !auto_check_due() {
            return RepairOutcome::Skipped;
        }
        let status = status_light();
        let key = version_key(&status);
        if settings.failed_for.as_deref() == Some(key.as_str()) {
            return RepairOutcome::Nothing;
        }
        let Some(cli_dir) = status.cli_dir.clone() else {
            return RepairOutcome::Nothing;
        };
        if status.wrapper == WrapperState::Deployed {
            return RepairOutcome::Nothing;
        }
        if procs::any_desktop_running() {
            return RepairOutcome::Blocked;
        }
        match deploy_wrapper(&cli_dir) {
            Ok(summary) => {
                record_repair(&mut settings, &status);
                let _ = save_settings(&settings);
                RepairOutcome::Repaired(summary)
            }
            Err(error) => {
                settings.last_failure = Some(format!("{error:#}"));
                settings.failed_for = Some(key);
                let _ = save_settings(&settings);
                RepairOutcome::Failed(format!("{error:#}"))
            }
        }
    }

    /// mac 全程用户态，没有提权子进程
    pub fn run_elevated(_mode: &str) -> i32 {
        1
    }

    /// Desktop 顺着软链把我们当 bundled CLI 起起来时，argv[0] 是软链自己的路径；
    /// current_exe() 会解析软链拿到 Claude++ 本体，认不出这一层，只能看 argv[0]
    pub fn wrapper_argv0() -> Option<PathBuf> {
        let arg0 = PathBuf::from(std::env::args_os().next()?);
        (arg0.file_name()? == WRAPPER_NAME).then_some(arg0)
    }

    /// 保留 Desktop 自带的 disableAutoMode 等键；解析不了就退回只带 fastMode 的最小设置
    fn merge_fast(settings: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(settings) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.insert("fastMode".into(), serde_json::Value::Bool(true));
                serde_json::to_string(&serde_json::Value::Object(map))
                    .unwrap_or_else(|_| FAST_ONLY.to_string())
            }
            _ => FAST_ONLY.to_string(),
        }
    }

    fn merged_args() -> Vec<OsString> {
        let settings_flag = std::ffi::OsStr::new("--settings");
        let mut out = Vec::new();
        let mut merged = false;
        let mut rest = std::env::args_os().skip(1);
        while let Some(arg) = rest.next() {
            if arg == settings_flag {
                if let Some(value) = rest.next() {
                    out.push(arg);
                    out.push(merge_fast(&value.to_string_lossy()).into());
                    merged = true;
                    continue;
                }
            }
            out.push(arg);
        }
        if !merged {
            out.push(settings_flag.to_os_string());
            out.push(FAST_ONLY.into());
        }
        out
    }

    /// 合并 fastMode 后 exec 官方本体：换的是进程映像不是子进程，PID 不变，
    /// Desktop 杀 wrapper 就是杀 CLI，不会留孤儿占着管道
    pub fn run_wrapper(argv0: &Path) -> i32 {
        let real = argv0.with_file_name(REAL_NAME);
        let error = Command::new(&real).args(merged_args()).exec();
        eprintln!("[fast-wrapper] exec {} 失败: {error}", real.display());
        1
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wrapper_symlinks_into_the_bundle() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            let exe = bin.join(WRAPPER_NAME);
            let real = bin.join(REAL_NAME);
            fs::write(&exe, b"official binary").unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Absent);

            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            // 顶替物必须是软链：Desktop 读前 8 字节验 Mach-O，会跟着链子读到 Claude++ 本体
            assert!(exe.symlink_metadata().unwrap().is_symlink());
            assert_eq!(fs::read_link(&exe).unwrap(), std::env::current_exe().unwrap());
            // 官方本体原样躺在隔壁，没有被复制或改写
            assert_eq!(fs::read(&real).unwrap(), b"official binary");

            // 重复部署幂等：不会把软链自己当官方藏起来
            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            assert_eq!(fs::read(&real).unwrap(), b"official binary");

            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(&exe).unwrap(), b"official binary");
            assert!(!real.exists());
        }

        /// Desktop 判 not_macho 后会重下整个 bundle，官方本体盖回槽位：以新官方为准
        #[test]
        fn official_redownload_takes_over() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            let exe = bin.join(WRAPPER_NAME);
            fs::write(&exe, b"official v1").unwrap();
            deploy_wrapper(dir.path()).unwrap();

            fs::remove_file(&exe).unwrap();
            fs::write(&exe, b"official v2 redownloaded").unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Broken);

            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            assert_eq!(fs::read(bin.join(REAL_NAME)).unwrap(), b"official v2 redownloaded");
        }

        /// Claude++ 被移走会让软链断掉，还原不能被 exists() 的假 false 卡住
        #[test]
        fn broken_symlink_still_restores() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            let exe = bin.join(WRAPPER_NAME);
            let real = bin.join(REAL_NAME);
            fs::write(&real, b"official").unwrap();
            std::os::unix::fs::symlink(dir.path().join("moved-away"), &exe).unwrap();
            assert!(!exe.exists());
            assert!(exe.symlink_metadata().is_ok());
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);

            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(&exe).unwrap(), b"official");
        }

        #[test]
        fn merge_fast_keeps_desktop_keys() {
            let merged = merge_fast(r#"{"disableAutoMode":"disable"}"#);
            let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
            assert_eq!(v["fastMode"], serde_json::json!(true));
            assert_eq!(v["disableAutoMode"], serde_json::json!("disable"));
            // 显式关掉的就地改写，不留重复键
            let over: serde_json::Value =
                serde_json::from_str(&merge_fast(r#"{"fastMode":false,"x":1}"#)).unwrap();
            assert_eq!(over["fastMode"], serde_json::json!(true));
            assert_eq!(over["x"], serde_json::json!(1));
            // 空的、坏的、不是对象的，一律退回最小设置
            assert_eq!(merge_fast("{}"), FAST_ONLY);
            assert_eq!(merge_fast("not json"), FAST_ONLY);
            assert_eq!(merge_fast("[1,2]"), FAST_ONLY);
        }

        #[test]
        fn wrapper_argv0_only_fires_for_the_slot_name() {
            assert_eq!(PathBuf::from("/x/claude").file_name().unwrap(), WRAPPER_NAME);
            assert_ne!(PathBuf::from("/x/claude-plus-plus").file_name().unwrap(), WRAPPER_NAME);
            assert_ne!(PathBuf::from("/x/claude-real").file_name().unwrap(), WRAPPER_NAME);
        }

        #[test]
        fn desktop_version_parses_xml_plist() {
            assert!(desktop_version().is_none() || desktop_version().unwrap().contains('.'));
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use anyhow::{bail, Result};

    use super::*;

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

    pub fn wrapper_argv0() -> Option<std::path::PathBuf> {
        None
    }

    pub fn run_wrapper(_argv0: &std::path::Path) -> i32 {
        1
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;

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
    fn version_parsing() {
        assert_eq!(parse_version("2.1.260"), Some(vec![2, 1, 260]));
        assert_eq!(parse_version("2.1.260-beta"), None);
        assert_eq!(parse_version(""), None);
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
    fn wrapper_state_covers_every_slot_shape() {
        use SlotOccupant::*;
        assert_eq!(wrapper_state_from(Wrapper, true), WrapperState::Deployed);
        // 官方本体与 real 并存：Desktop 重下覆盖了 wrapper
        assert_eq!(wrapper_state_from(Official, true), WrapperState::Broken);
        assert_eq!(wrapper_state_from(Official, false), WrapperState::Absent);
        // wrapper 在而官方本体丢了：仍报未部署，装的时候会在 stage 阶段拒绝
        assert_eq!(wrapper_state_from(Wrapper, false), WrapperState::Absent);
        assert_eq!(wrapper_state_from(Empty, false), WrapperState::NoCli);
        assert_eq!(wrapper_state_from(Empty, true), WrapperState::Broken);
    }

    #[test]
    fn stage_slot_handles_every_shape() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claude");
        let real = dir.path().join("claude-real");
        // 空目录：没得顶替
        assert!(stage_wrapper_slot(&exe, &real, SlotOccupant::Empty).is_err());
        // 官方原样：改名让位
        fs::write(&exe, b"official").unwrap();
        stage_wrapper_slot(&exe, &real, SlotOccupant::Official).unwrap();
        assert!(!exe.exists() && real.is_file());
        // 已装过：旧 wrapper 就地删掉等待重写
        fs::write(&exe, b"old wrapper").unwrap();
        stage_wrapper_slot(&exe, &real, SlotOccupant::Wrapper).unwrap();
        assert!(!exe.exists() && real.is_file());
        // real 丢了而槽位上是 wrapper：宁可报错也不拿 wrapper 当官方藏起来
        fs::remove_file(&real).unwrap();
        fs::write(&exe, b"orphan wrapper").unwrap();
        assert!(stage_wrapper_slot(&exe, &real, SlotOccupant::Wrapper).is_err());
        // Desktop 重下过官方：以新官方为准，旧 real 被覆盖
        fs::write(&real, b"stale real").unwrap();
        fs::write(&exe, b"fresh official").unwrap();
        stage_wrapper_slot(&exe, &real, SlotOccupant::Official).unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"fresh official");
    }

    #[test]
    fn remove_wrapper_puts_official_back() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claude");
        let real = dir.path().join("claude-real");
        fs::write(&exe, b"wrapper").unwrap();
        fs::write(&real, b"official").unwrap();
        assert!(remove_wrapper_in(&exe, &real).unwrap());
        assert_eq!(fs::read(&exe).unwrap(), b"official");
        assert!(!real.exists());
        // 没装过的目录什么都不做
        assert!(!remove_wrapper_in(&exe, &real).unwrap());
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
        // mac 上 renderer 恒为 Unsupported，认领只看 wrapper
        let mut mac = FastModeSettings::default();
        assert!(!adopt_existing(&status_with(WrapperState::Absent, RendererState::Unsupported), &mut mac));
        assert!(adopt_existing(&status_with(WrapperState::Deployed, RendererState::Unsupported), &mut mac));
    }
}

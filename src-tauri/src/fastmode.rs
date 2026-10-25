//! Claude Desktop 3p Fast Mode：wrapper 顶替 bundled CLI 注入 fastMode；Windows 直接 patch，macOS 通过原生增强安装开关
//! macOS 的状态显示补丁由原生增强的整包重签流程管理。

// Windows / macOS 之外只有 stub，共享的 wrapper 与统计逻辑在那里用不到
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
#[cfg(windows)]
use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const ELEVATED_FLAG: &str = "--fastmode-elevated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WrapperState {
    Deployed,
    Outdated,
    Absent,
    Broken,
    NoCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RendererState {
    Patched,
    Outdated,
    Pristine,
    Mismatch,
    NotFound,
    Unreadable,
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererInfo {
    /// renderer 由「功能增强」页的原生开关接管，这里只管 wrapper
    pub native: bool,
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
    /// 失败时的 "desktop|cli|claude++" 版本组合：同一组合不再自动重试，UAC 被拒后不会反复弹窗
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

const VERIFIED_MARKER: &str = ".verified";

/// Desktop 2.19675 起 CLI 装在 版本目录/<清单 sha256 前 12 位>/ 下
fn is_build_dir_name(name: &str) -> bool {
    name.len() == 12 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn build_dirs(version_dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(version_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|entry| is_build_dir_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect()
}

fn verified_at(dir: &Path) -> Option<SystemTime> {
    fs::metadata(dir.join(VERIFIED_MARKER)).and_then(|m| m.modified()).ok()
}

/// Desktop 实际 spawn 的那层：多份构建并存时它取 .verified 最新的，没有已发布的构建就是旧布局。
/// 哪层都没有 .verified 说明还没装完，Desktop 不会用它
fn exe_dir_of(version_dir: &Path) -> Option<PathBuf> {
    build_dirs(version_dir)
        .into_iter()
        .filter_map(|dir| verified_at(&dir).map(|at| (at, dir)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, dir)| dir)
        .or_else(|| verified_at(version_dir).map(|_| version_dir.to_path_buf()))
}

/// exe_dir 是 Desktop spawn 的那层，旧布局下就是 version_dir
#[derive(Debug, Clone, PartialEq, Eq)]
struct CliInstall {
    version: String,
    version_dir: PathBuf,
    exe_dir: PathBuf,
}

/// 下载中的版本目录还没有 .verified，跳过它才不会把半成品当成 CLI 失踪记成失败
fn latest_cli(root: &Path) -> Option<CliInstall> {
    let mut versions: Vec<(Vec<u64>, String, PathBuf)> = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            parse_version(&name).map(|version| (version, name, entry.path()))
        })
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    versions.into_iter().find_map(|(_, version, version_dir)| {
        let exe_dir = exe_dir_of(&version_dir)?;
        Some(CliInstall { version, version_dir, exe_dir })
    })
}

fn cli_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Claude-3p").join("claude-code"))
}

fn current_cli() -> Option<CliInstall> {
    cli_root().and_then(|root| latest_cli(&root))
}

/// 构建子目录名是哈希，提示里带上版本号才看得懂
fn cli_dir_label(dir: &Path) -> String {
    let name = dir_label(dir);
    match dir.parent() {
        Some(version_dir) if is_build_dir_name(&name) => format!("{}/{name}", dir_label(version_dir)),
        _ => name,
    }
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
        (SlotOccupant::Official, false) => WrapperState::Absent,
        (SlotOccupant::Empty, false) => WrapperState::NoCli,
        // 官方被重下盖回、槽位空着、wrapper 丢了本体（Desktop 搬布局会这样）：Desktop 都跑不到 wrapper
        (SlotOccupant::Official, true) | (SlotOccupant::Empty, true) | (SlotOccupant::Wrapper, false) => {
            WrapperState::Broken
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrayStash {
    AdoptInto(usize),
    Discard,
    Keep,
}

/// Desktop 搬旧布局时只带走自己的文件，藏起的官方本体留在版本目录：
/// 缺本体的 wrapper 先接手，没有谁缺而某份构建已有官方，外层那份就只是残留
fn stray_stash_fate(version_slot: SlotOccupant, builds: &[(SlotOccupant, bool)]) -> StrayStash {
    // 版本目录自己的槽位还有东西就仍是旧布局，那份本体是正主
    if version_slot != SlotOccupant::Empty {
        return StrayStash::Keep;
    }
    if let Some(index) = builds.iter().position(|&shape| shape == (SlotOccupant::Wrapper, false)) {
        return StrayStash::AdoptInto(index);
    }
    if builds.iter().any(|&(slot, real)| slot == SlotOccupant::Official || real) {
        return StrayStash::Discard;
    }
    StrayStash::Keep
}

/// 空出 wrapper 的位置。官方本体改名让位；若官方已被 Desktop 重新下载覆盖，以新官方为准。
/// macOS 不走这条：那边官方本体一改名签名就失配，得连 bundle 一起挪
#[cfg(windows)]
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
        (SlotOccupant::Empty, false) => {
            let dir = exe.parent().map(cli_dir_label).unwrap_or_default();
            anyhow::bail!("{dir} 缺少 {name}")
        }
    }
    Ok(())
}

#[cfg(windows)]
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
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
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
    if status.wrapper != WrapperState::Deployed
        && (status.renderer.native
            || !matches!(status.renderer.state, RendererState::Patched | RendererState::Outdated))
    {
        return false;
    }
    settings.installed = true;
    record_live_versions(settings, status);
    true
}

/// 账本只记确实生效的组件：没 patch 上的 renderer 不能顶着当前 Desktop 版本号
fn record_live_versions(settings: &mut FastModeSettings, status: &FastModeStatus) -> bool {
    let wrapper_live = status.wrapper == WrapperState::Deployed;
    let renderer_live = status.renderer.state == RendererState::Patched;
    settings.patched_cli_version = status.cli_version.clone().filter(|_| wrapper_live);
    settings.patched_desktop_version = status.desktop_version.clone().filter(|_| renderer_live);
    wrapper_live || renderer_live
}

/// 一轮修复里各组件的结果，没轮到的为 None。wrapper 与 renderer 互不依赖，一边失败不拖住另一边
struct Outcome {
    wrapper: Option<Result<String>>,
    renderer: Option<Result<String>>,
}

impl Outcome {
    fn steps(&self) -> [(&'static str, Option<&Result<String>>); 2] {
        [("wrapper", self.wrapper.as_ref()), ("renderer", self.renderer.as_ref())]
    }

    fn failure(&self) -> Option<String> {
        let errors: Vec<String> = self
            .steps()
            .into_iter()
            .filter_map(|(label, step)| match step? {
                Ok(_) => None,
                Err(error) => Some(format!("{label}：{error:#}")),
            })
            .collect();
        (!errors.is_empty()).then(|| errors.join("；"))
    }

    /// 成败一起列：只报失败会让人以为另一半也没做
    fn describe(&self) -> String {
        self.steps()
            .into_iter()
            .filter_map(|(label, step)| match step? {
                Ok(message) => Some(message.clone()),
                Err(error) => Some(format!("{label}：{error:#}")),
            })
            .collect::<Vec<_>>()
            .join("；")
    }

    fn into_report(self) -> Result<ActionReport> {
        if self.failure().is_some() {
            anyhow::bail!("{}", self.describe());
        }
        let message = |step: Option<Result<String>>| step.and_then(Result::ok).unwrap_or_default();
        Ok(ActionReport { wrapper: message(self.wrapper), renderer: message(self.renderer) })
    }

    fn into_repair(self) -> RepairOutcome {
        match self.failure() {
            None => RepairOutcome::Repaired(self.describe()),
            Some(_) => RepairOutcome::Failed(self.describe()),
        }
    }
}

/// 按修复后的实际状态记账；有失败就记下版本组合，同一组合不再自动重试
fn record_outcome(settings: &mut FastModeSettings, after: &FastModeStatus, outcome: &Outcome) {
    if record_live_versions(settings, after) {
        settings.installed = true;
    }
    let failure = outcome.failure();
    if failure.is_none() {
        settings.last_repair = Some(now_iso());
    }
    settings.failed_for = failure.as_ref().map(|_| version_key(after));
    settings.last_failure = failure;
}

fn record_uninstall(settings: &mut FastModeSettings) {
    settings.installed = false;
    settings.patched_desktop_version = None;
    settings.patched_cli_version = None;
    settings.last_failure = None;
    settings.failed_for = None;
}

fn version_key(status: &FastModeStatus) -> String {
    // 带上自身版本：旧版修不好的组合，升级 Claude++ 后值得再试一次
    format!(
        "{}|{}|{}",
        status.desktop_version.as_deref().unwrap_or("-"),
        status.cli_version.as_deref().unwrap_or("-"),
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn deploy_cli(cli: &CliInstall) -> Result<String> {
    imp::reclaim_stray_stash(&cli.version_dir)?;
    imp::deploy_wrapper(&cli.exe_dir)?;
    Ok(format!("wrapper 已部署到 {}", cli_dir_label(&cli.exe_dir)))
}

/// Desktop 搬布局时可能把 wrapper 带进任意一份构建：每份都还原，旧布局的版本目录本身也算一份
#[cfg(any(windows, target_os = "macos"))]
fn restore_version_dir(version_dir: &Path) -> Result<bool> {
    imp::reclaim_stray_stash(version_dir)?;
    let mut restored = false;
    for build in build_dirs(version_dir) {
        restored |= imp::remove_wrapper(&build)?;
    }
    restored |= imp::remove_wrapper(version_dir)?;
    Ok(restored)
}

#[cfg(any(windows, target_os = "macos"))]
fn restore_all_cli() -> Result<usize> {
    let Some(root) = cli_root() else { return Ok(0) };
    let mut restored = 0;
    for entry in fs::read_dir(&root).ok().into_iter().flatten().flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && restore_version_dir(&entry.path())? {
            restored += 1;
        }
    }
    Ok(restored)
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

    use anyhow::{bail, ensure, Context, Result};

    use super::*;
    use crate::procs;

    const WRAPPER_SOURCE: &str = include_str!("fastmode/wrapper.cs");
    const CSC: &str = r"C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 官方 exe 200 MB 上下，wrapper 几 KB：以 1 MB 分界认出谁占着槽位
    const WRAPPER_MAX_BYTES: u64 = 1024 * 1024;
    const CLI_EXE: &str = "claude.exe";
    const REAL_EXE: &str = "claude-real.exe";
    const NATIVE_NOTE: &str = "已装原生开关，renderer 由「功能增强」页管理";

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
        required_marker: Option<&'static str>,
        pattern: &'static str,
        patched: &'static str,
        aliases: &'static [(&'static str, &'static str)],
        alternates: &'static [(&'static str, &'static [(&'static str, &'static str)])],
    }

    /// minify 只重命名局部变量与函数，属性名和字符串字面量不动 —— 锚点拿它们当骨架，
    /// 变量名捕获后回填，Desktop 更新重命名符号也不会失配
    struct Renderer<'a> {
        text: &'a str,
    }

    const ANCHORS: [Anchor; 4] = [
        Anchor {
            label: "显示条件",
            required_marker: None,
            // let 里可能先声明别的中间量，只改被 return 的那一个
            pattern: concat!(
                r"(?P<sig>fastModeIpcAvailable:(?P<ipc>[\w$]+),(?:[\w$]+:[\w$]+,)*?modelSupportsFastMode:(?P<sup>[\w$]+),",
                r"(?:[\w$]+:[\w$]+,)*?[\w$]+:[\w$]+\}\)\{)(?P<declarations>let (?P<pre>[^;]*,)?(?P<show>[\w$]+)=[^=>;][^;]*;)",
                r"return\{showFastModeToggle:(?P<show2>[\w$]+),fastModeToggleDisabled:(?P<disabled>[\w$]+&&[\w$]+)",
                r"(?P<carrier>,fastModeCarrier:(?P<route>[\w$]+))?\}",
            ),
            patched: concat!(
                r"return\{showFastModeToggle:[\w$]+,fastModeToggleDisabled:!1\}",
                r"|return\{showFastModeToggle:[\w$]+\?[^,;]+:[\w$]+,",
                r"fastModeToggleDisabled:[\w$]+\?!1:[\w$]+&&[\w$]+,",
                r#"fastModeCarrier:[\w$]+\?"local":[\w$]+\}"#,
            ),
            aliases: &[("show", "show2")],
            alternates: &[],
        },
        Anchor {
            label: "模型支持判定",
            required_marker: None,
            // 本地形态与带 remote 的形态各用一组捕获，别名要求同在同缺，混搭的结构匹配不上
            pattern: concat!(
                r",(?P<sup>[\w$]+)=(?:(?P<has>[\w$]+)&&(?P<is46>[\w$]+)",
                r"|(?P<rhas>[\w$]+)&&\((?P<remote>[\w$]+)\|\|(?P<ris46>[\w$]+)\)),",
                r"(?P<fnc>[\w$]+)=(?P<cb>[\w$]+)\((?P<arg>[\w$]+)=>",
                r"[\w$]+\([\w$]+\((?P<arg2>[\w$]+)\)\)!==void 0&&",
                r#"\((?P<arg3>[\w$]+)\.toLowerCase\(\)\.includes\("opus-4-6"\)\|\|[\w$]+\(\)\),\[[\w$]+\]\),"#,
                r"(?P<needs>[\w$]+)=(?:(?P<has2>[\w$]+)&&!(?P<is46b>[\w$]+)",
                r"|!(?P<remote2>[\w$]+)&&(?P<rhas2>[\w$]+)&&!(?P<ris46b>[\w$]+)),",
            ),
            patched: concat!(
                r#",[\w$]+=!!\([\w$]+&&\([\w$]+\.toLowerCase\(\)\.includes\("opus-5"\)"#,
                r"|,[\w$]+=[\w$]+\([\w$]+=>[\w$]+\?[^;]+?:",
                r#"!!\([\w$]+&&\([\w$]+\.toLowerCase\(\)\.includes\("opus-5"\)"#,
                r"|,[\w$]+=[\w$]+\([\w$]+,\d+,[\w$]+,[\w$]+\)\?[\w$]+\([\w$]+,\d+,[\w$]+,[\w$]+,",
                r#"[\w$]+=>[\w$]+\?[^;]+?:!!\([\w$]+&&\([\w$]+\.toLowerCase\(\)\.includes\("opus-5"\)"#,
            ),
            aliases: &[
                ("arg", "arg2"),
                ("arg", "arg3"),
                ("has", "has2"),
                ("is46", "is46b"),
                ("rhas", "rhas2"),
                ("remote", "remote2"),
                ("ris46", "ris46b"),
            ],
            alternates: &[(
                CALLBACK_SUPPORT,
                &[
                    ("arg", "arg2"),
                    ("arg", "arg3"),
                    ("fnc", "fnc2"),
                    ("lookup", "lookup2"),
                    ("remote", "remote2"),
                    ("remote", "remote3"),
                    ("model", "model2"),
                ],
            ), (
                COMPILED_SUPPORT,
                &[
                    ("arg", "arg2"), ("arg", "arg3"), ("fnc", "fnc2"),
                    ("lookup", "lookup2"), ("lookup", "lookup3"),
                    ("remote", "remote2"), ("remote", "remote3"), ("remote", "remote4"),
                    ("cache", "cache2"), ("cache", "cache3"), ("slot", "slot2"),
                    ("model", "model2"),
                ],
            )],
        },
        Anchor {
            label: "禁用原因",
            required_marker: None,
            pattern: concat!(
                r"(?P<msg>[\w$]+)=[\w$]+\([\w$]+\?\.fastModeDisabledReason,",
                r"\{hasRaven:[\w$]+,canManageOrg:[\w$]+\}\),(?P<flag>[\w$]+)=(?P<msg2>[\w$]+)!==null",
                // 前缀是 remote 会话的 CLI 版本拦截，本地会话恒假，随禁用原因一并清掉
                r"|(?P<direct_flag>[\w$]+)=(?:[\w$]+\|\|)?[\w$]+\([\w$]+\?\.fastModeDisabledReason\)",
                r"(?P<tail>,[\w$]+=[\w$]+\?\.fastMode\?\.value===!0,)",
            ),
            patched: concat!(
                r"[\w$]+=null,[\w$]+=!1,[\w$]+=[\w$]+\?\.fastMode\?\.value===!0,",
                r"|[\w$]+=!1,[\w$]+=[\w$]+\?\.fastMode\?\.value===!0,",
                r"|[\w$]+=[\w$]+\?\((?:[\w$]+\|\|)?[\w$]+\([\w$]+\?\.fastModeDisabledReason\)\):!1,",
                r"[\w$]+=[\w$]+\?\.fastMode\?\.value===!0,",
                r"|[\w$]+=[\w$]+\?\.fastModeDisabledReason,[\w$]+=[\w$]+\?\([\w$]+\([\w$]+,\d+,[\w$]+\)\?",
                r"[\w$]+\([\w$]+,\d+,[\w$]+,[\w$]+\([\w$]+\)\):[\w$]+\[\d+\]\):!1,",
                r#"[\w$]+=[\w$]+\?\(![\w$]+&&\([\w$]+==="extra_usage_disabled"\|\|[\w$]+&&![\w$]+&&![\w$]+\)\):!1,"#,
                r"[\w$]+=[\w$]+\|\|[\w$]+\|\|[\w$]+,",
            ),
            aliases: &[("msg", "msg2")],
            alternates: &[(COMPILED_REASON, &[
                ("reason", "reason2"), ("reason", "reason3"), ("reason", "reason4"), ("reason", "reason5"),
                ("blocked", "blocked2"), ("blocked", "blocked3"), ("credits", "credits2"), ("old", "old2"),
                ("cache", "cache2"), ("cache", "cache3"), ("slot", "slot2"),
            ])],
        },
        Anchor {
            label: "模型菜单",
            required_marker: Some("epitaxy-cds-model-selector"),
            pattern: concat!(
                r"(?P<models>return\{\.\.\.[\w$]+,models:[\w$]+\.map\([\w$]+\))",
                // memo 结束到读取 .models.find 之间可能夹着别的声明
                r"(?P<tail>\}\},\[[^\]]*\]\)[^;]*?,[\w$]+=[\w$]+\?\.models\.find\()",
            ),
            patched: concat!(
                r"(?:return\{\.\.\.[\w$]+,models:[\w$]+\.map\([\w$]+\)",
                r"|[\w$]+\([\w$]+,\d+,[\w$]+,[\w$]+,\{\.\.\.[\w$]+,models:[\w$]+)\.map\([\w$]+=>[^{};]+",
                r#"\?\{\.\.\.[\w$]+,fast_mode:\{type:"toggle",name:"Fast mode",options:\["#,
                r#"\{id:"fast",name:"Enable fast mode",selector_label:"Fast"\},\{id:"off",name:"Off"\}"#,
                r"\]\}\}:[\w$]+\)",
            ),
            aliases: &[],
            alternates: &[(
                concat!(
                    r"(?P<models>[\w$]+\((?P<cache>[\w$]+),\d+,(?P<catalog>[\w$]+),(?P<list>[\w$]+),",
                    r"\{\.\.\.(?P<catalog2>[\w$]+),models:(?P<list2>[\w$]+))(?P<tail>\}\):(?P<cache2>[\w$]+)\[\d+\])",
                ),
                &[("cache", "cache2"), ("catalog", "catalog2"), ("list", "list2")],
            )],
        },
    ];

    const CALLBACK_SUPPORT: &str = concat!(
        r",(?P<fnc>[\w$]+)=(?P<cb>[\w$]+)\((?P<arg>[\w$]+)=>(?P<body>(?P<lookup>[\w$]+)\([\w$]+\((?P<arg2>[\w$]+)\)\)!==void 0&&",
        r#"\((?P<remote>[\w$]+)\|\|(?P<arg3>[\w$]+)\.toLowerCase\(\)\.includes\("opus-4-6"\)\|\|[\w$]+\(\)\)),"#,
        r"(?P<deps>\[(?P<lookup2>[\w$]+),(?P<remote2>[\w$]+)\])\),",
        r"(?P<sup>[\w$]+)=!!(?P<model>[\w$]+)&&(?P<fnc2>[\w$]+)\((?P<model2>[\w$]+)\),",
        r"(?P<needs>[\w$]+)=!(?P<remote3>[\w$]+)&&[\w$]+&&![\w$]+,",
    );

    const MENU_DEFINITION: &str = r#""data-testid":"epitaxy-cds-model-selector""#;

    const COMPILED_SUPPORT: &str = concat!(
        r",(?P<fnc>[\w$]+)=(?P<cache_open>[\w$]+\((?P<cache>[\w$]+),(?P<slot>\d+),(?P<lookup>[\w$]+),(?P<remote>[\w$]+)\)\?",
        r"[\w$]+\((?P<cache2>[\w$]+),(?P<slot2>\d+),(?P<lookup2>[\w$]+),(?P<remote2>[\w$]+),)",
        r"(?P<arg>[\w$]+)=>(?P<body>(?P<lookup3>[\w$]+)\([\w$]+\((?P<arg2>[\w$]+)\)\)!==void 0&&",
        r#"\((?P<remote3>[\w$]+)\|\|(?P<arg3>[\w$]+)\.toLowerCase\(\)\.includes\("opus-4-6"\)\|\|[\w$]+\(\)\))"#,
        r"(?P<cache_close>\):(?P<cache3>[\w$]+)\[\d+\]),",
        r"(?P<sup>[\w$]+)=!!(?P<model>[\w$]+)&&(?P<fnc2>[\w$]+)\((?P<model2>[\w$]+)\),",
        r"(?P<needs>[\w$]+)=!(?P<remote4>[\w$]+)&&[\w$]+&&![\w$]+,",
    );

    const COMPILED_REASON: &str = concat!(
        r"(?P<reason>[\w$]+)=(?P<meta>[\w$]+)\?\.fastModeDisabledReason,",
        r"(?P<blocked>[\w$]+)=(?P<blocked_expr>[\w$]+\((?P<cache>[\w$]+),(?P<slot>\d+),(?P<reason2>[\w$]+)\)\?",
        r"[\w$]+\((?P<cache2>[\w$]+),(?P<slot2>\d+),(?P<reason3>[\w$]+),[\w$]+\((?P<reason4>[\w$]+)\)\):(?P<cache3>[\w$]+)\[\d+\]),",
        r#"(?P<credits>[\w$]+)=(?P<credits_expr>!(?P<old>[\w$]+)&&\((?P<reason5>[\w$]+)==="extra_usage_disabled"\|\|[\w$]+&&![\w$]+&&!(?P<blocked2>[\w$]+)\)),"#,
        r"(?P<flag>[\w$]+)=(?P<old2>[\w$]+)\|\|(?P<credits2>[\w$]+)\|\|(?P<blocked3>[\w$]+),",
    );

    impl Anchor {
        fn matches<'a>(&self, text: &'a str) -> Vec<regex::Captures<'a>> {
            let mut hits = Vec::new();
            for (pattern, aliases) in std::iter::once((self.pattern, self.aliases)).chain(self.alternates.iter().copied()) {
                hits.extend(rx(pattern).captures_iter(text).filter(|c| {
                    aliases.iter().all(|(a, b)| c.name(a).map(|m| m.as_str()) == c.name(b).map(|m| m.as_str()))
                }));
            }
            hits
        }
    }

    fn supports_fast_js(id: &str) -> String {
        format!(
            r#"!!({id}&&({id}.toLowerCase().includes("opus-5")||{id}.toLowerCase().includes("opus-4-8")))"#
        )
    }

    /// 目标函数的 modelId 形参：全文有多个 modelId: 属性，取被改代码所在函数的那个
    const MODEL_ID_SIGNATURE: &str = concat!(
        r"function [\w$]+\(\{[^{}]*?modelId:(?P<direct>[\w$]+)[^{}]*?\}\)\{",
        r"|function [\w$]+\((?P<props>[\w$]+)\)\{let [\w$]+=[\w$]+\(\d+\),",
        r"\{[^{}]*?modelId:(?P<compiled>[\w$]+)[^{}]*?\}=(?P<props2>[\w$]+),",
    );

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
            let mut hits = anchor.matches(self.text);
            (hits.len() == 1).then(|| hits.remove(0))
        }

        fn anchors(&self) -> impl Iterator<Item = &'static Anchor> + '_ {
            let menu_only = is_model_menu(self.text) && !is_renderer(self.text);
            ANCHORS.iter().filter(move |a| match a.required_marker {
                Some(_) => is_model_menu(self.text),
                None => !menu_only,
            })
        }

        fn model_id_before(&self, offset: usize) -> Option<String> {
            rx(MODEL_ID_SIGNATURE)
                .captures_iter(&self.text[..offset])
                .last()
                .and_then(|c| {
                    if c.name("props").map(|m| m.as_str()) != c.name("props2").map(|m| m.as_str()) {
                        return None;
                    }
                    c.name("direct").or_else(|| c.name("compiled")).map(|m| m.as_str().to_string())
                })
        }

        fn is_patched(&self) -> bool {
            self.anchors()
                .all(|a| unique_match(self.text, a.patched, &[]).is_some())
        }

        fn is_outdated(&self) -> bool {
            is_renderer(self.text)
                && is_model_menu(self.text)
                && ANCHORS[..3].iter().all(|a| unique_match(self.text, a.patched, &[]).is_some())
                && self.find(&ANCHORS[3]).is_some()
        }

        fn missing(&self) -> Vec<String> {
            self.anchors()
                .filter(|a| self.find(a).is_none())
                .map(|a| a.label.to_string())
                .collect()
        }

        fn patch(&self) -> Result<String> {
            let text = self.patch_unchecked()?;
            if !Renderer::new(&text).is_patched() {
                bail!("renderer 改写后未通过完整补丁校验");
            }
            Ok(text)
        }

        /// 逐处改写：显示条件收敛为 IPC 可用且模型支持，模型支持改按 ID 判定，禁用原因清空
        fn patch_unchecked(&self) -> Result<String> {
            let mut text = if is_renderer(self.text) { self.patch_core()? } else { self.text.to_string() };
            if is_model_menu(self.text) {
                let staged = Renderer::new(&text);
                let caps = staged.find(&ANCHORS[3]).context("模型菜单锚点未唯一命中")?;
                let replacement = format!(
                    concat!(
                        "{}.map(e=>{}&&",
                        r#"!(e.fast_mode?.type==="toggle"&&e.fast_mode.options?.some(e=>e.id==="fast"))"#,
                        r#"?{{...e,fast_mode:{{type:"toggle",name:"Fast mode",options:["#,
                        r#"{{id:"fast",name:"Enable fast mode",selector_label:"Fast"}},{{id:"off",name:"Off"}}"#,
                        "]}}}}:e){}"
                    ),
                    &caps["models"], supports_fast_js("e.id"), &caps["tail"]
                );
                text = text.replace(&caps[0], &replacement);
            }
            Ok(text)
        }

        fn patch_core(&self) -> Result<String> {
            let show = self.find(&ANCHORS[0]).context("显示条件锚点未唯一命中")?;
            let replacement = if let Some(route) = show.name("route") {
                let signature = rx(concat!(
                    r"function [\w$]+\(\{envType:(?P<env>[\w$]+),sessionRef:(?P<session>[\w$]+),",
                    r"isLocalSession:(?P<local>[\w$]+),[^{}]*\}\)\{",
                ));
                let context = signature
                    .captures_iter(&self.text[..show.get(0).unwrap().end()])
                    .last()
                    .context("未找到开关所在函数的会话参数")?;
                if context.get(0).unwrap().end() != show.name("sig").unwrap().end()
                    || self.text.contains("__cppLocalFast")
                {
                    bail!("开关上下文不唯一");
                }
                // carrier 决定新会话的设置路由，只为本地会话补齐，远端沿用原判定。
                format!(
                    concat!(
                        "{}{}let __cppLocalFast={}&&({}?{}:{}===\"local\"||{}===\"ssh\");",
                        "return{{showFastModeToggle:__cppLocalFast?{}&&{}:{},",
                        "fastModeToggleDisabled:__cppLocalFast?!1:{},fastModeCarrier:__cppLocalFast?\"local\":{}}}",
                    ),
                    &show["sig"],
                    &show["declarations"],
                    &show["ipc"],
                    &context["session"],
                    &context["local"],
                    &context["env"],
                    &context["env"],
                    &show["ipc"],
                    &show["sup"],
                    &show["show"],
                    &show["disabled"],
                    route.as_str(),
                )
            } else {
                format!(
                    "{}let {}{}={}&&{};return{{showFastModeToggle:{},fastModeToggleDisabled:!1}}",
                    &show["sig"],
                    show.name("pre").map_or("", |m| m.as_str()),
                    &show["show"],
                    &show["ipc"],
                    &show["sup"],
                    &show["show"]
                )
            };
            let mut text = self.text.replace(&show[0], &replacement);

            let (support, remote) = {
                let staged = Renderer::new(&text);
                let caps = staged.find(&ANCHORS[1]).context("模型支持锚点未唯一命中")?;
                let whole = caps[0].to_string();
                let model_id =
                    staged.model_id_before(caps.get(0).unwrap().start()).context("未能在目标函数签名里定位 modelId")?;
                // Opus 4.6 客户端认它但服务端已不再给 fast，只放 5 与 4.8
                if let Some(model) = caps.name("model") {
                    if model.as_str() != model_id {
                        bail!("模型支持判定引用了其他 modelId");
                    }
                    let callback = if let Some(open) = caps.name("cache_open") {
                        format!("{}{}=>{}?{}:{}{}", open.as_str(), &caps["arg"], &caps["remote"],
                            &caps["body"], supports_fast_js(&caps["arg"]), &caps["cache_close"])
                    } else {
                        format!("{}({}=>{}?{}:{},{})", &caps["cb"], &caps["arg"], &caps["remote"],
                            &caps["body"], supports_fast_js(&caps["arg"]), &caps["deps"])
                    };
                    let replacement = format!(
                        ",{}={},{}=!!{}&&{}({}),{}=!1,",
                        &caps["fnc"],
                        callback,
                        &caps["sup"],
                        model_id,
                        &caps["fnc"],
                        model_id,
                        &caps["needs"],
                    );
                    ((whole, replacement), Some(caps["remote"].to_string()))
                } else {
                    let replacement = format!(
                        ",{}={},{}={}({}=>{},[]),{}=!1,",
                        &caps["sup"],
                        supports_fast_js(&model_id),
                        &caps["fnc"],
                        &caps["cb"],
                        &caps["arg"],
                        supports_fast_js(&caps["arg"]),
                        &caps["needs"]
                    );
                    ((whole, replacement), None)
                }
            };
            text = text.replace(&support.0, &support.1);

            let reason = {
                let staged = Renderer::new(&text);
                let caps = staged.find(&ANCHORS[2]).context("禁用原因锚点未唯一命中")?;
                let replacement = match caps.name("direct_flag") {
                    Some(flag) => {
                        if let Some(remote) = remote.as_deref() {
                            let body = &text[flag.end() + 1..caps.name("tail").unwrap().start()];
                            format!("{}={remote}?({body}):!1{}", flag.as_str(), &caps["tail"])
                        } else {
                            format!("{}=!1{}", flag.as_str(), &caps["tail"])
                        }
                    }
                    None if caps.name("blocked_expr").is_some() => {
                        let remote = remote.as_deref().context("缓存禁用原因缺少远程会话上下文")?;
                        format!(
                            "{}={}?.fastModeDisabledReason,{}={remote}?({}):!1,{}={remote}?({}):!1,{}={}||{}||{},",
                            &caps["reason"], &caps["meta"], &caps["blocked"], &caps["blocked_expr"],
                            &caps["credits"], &caps["credits_expr"], &caps["flag"], &caps["old"], &caps["credits"], &caps["blocked"]
                        )
                    }
                    None => format!("{}=null,{}=!1", &caps["msg"], &caps["flag"]),
                };
                (caps[0].to_string(), replacement)
            };
            text = text.replace(&reason.0, &reason.1);

            Ok(text)
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

    fn wrapper_state(dir: &Path) -> WrapperState {
        let exe = dir.join(CLI_EXE);
        let state = wrapper_state_from(slot_occupant(&exe), dir.join(REAL_EXE).is_file());
        if state == WrapperState::Deployed {
            return match fs::read(exe) {
                Ok(bytes) if memchr::memmem::find(&bytes, wrapper_source_hash().as_bytes()).is_some() => state,
                Ok(_) => WrapperState::Outdated,
                Err(_) => WrapperState::Broken,
            };
        }
        state
    }

    fn wrapper_source_hash() -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(WRAPPER_SOURCE.as_bytes()))
    }

    fn wrapper_source() -> String {
        WRAPPER_SOURCE.replace("__CPP_WRAPPER_SOURCE_HASH__", &wrapper_source_hash())
    }

    pub(super) fn reclaim_stray_stash(version_dir: &Path) -> Result<()> {
        let stray = version_dir.join(REAL_EXE);
        if !stray.is_file() {
            return Ok(());
        }
        let builds = build_dirs(version_dir);
        let shapes: Vec<(SlotOccupant, bool)> = builds
            .iter()
            .map(|dir| (slot_occupant(&dir.join(CLI_EXE)), dir.join(REAL_EXE).is_file()))
            .collect();
        match stray_stash_fate(slot_occupant(&version_dir.join(CLI_EXE)), &shapes) {
            StrayStash::AdoptInto(index) => {
                let build = &builds[index];
                fs::rename(&stray, build.join(REAL_EXE))
                    .with_context(|| format!("把 {REAL_EXE} 挪回 {}", cli_dir_label(build)))?;
            }
            StrayStash::Discard => {
                fs::remove_file(&stray)
                    .with_context(|| format!("清掉 {} 外层残留的 {REAL_EXE}", dir_label(version_dir)))?;
            }
            StrayStash::Keep => {}
        }
        Ok(())
    }

    /// 认文件靠属性名（函数名随 minify 变），且必须在 patch 前后都成立：fastModeDisabledReason
    /// 正是锚点要抹掉的调用。showFastModeToggle 另一个 bundle 里也有，fastModeToggleDisabled 才只属于目标
    fn is_renderer(text: &str) -> bool {
        text.contains("fastModeToggleDisabled") && text.contains("modelSupportsFastMode")
    }

    fn is_model_menu(text: &str) -> bool {
        text.contains(MENU_DEFINITION)
    }

    struct RendererAsset {
        path: PathBuf,
        text: String,
    }

    fn renderer_assets(v1: &Path) -> Result<Vec<RendererAsset>> {
        let mut assets = Vec::new();
        for entry in fs::read_dir(v1).context("读取 renderer 目录")? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
            if is_renderer(&text) || is_model_menu(&text) {
                assets.push(RendererAsset { path, text });
            }
        }
        let core: Vec<_> = assets.iter().filter(|a| is_renderer(&a.text)).collect();
        if core.len() != 1 {
            bail!("主 renderer 应唯一，找到 {} 个", core.len());
        }
        let menus = assets.iter().filter(|a| is_model_menu(&a.text)).count();
        if menus > 1 || (core[0].text.contains("fastModeCarrier:") && menus != 1) {
            bail!("模型菜单 renderer 应唯一，找到 {menus} 个");
        }
        assets.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(assets)
    }

    fn classify_assets(assets: &[RendererAsset]) -> (RendererState, Vec<String>) {
        let states: Vec<_> = assets.iter().map(|a| classify(&a.text)).collect();
        let missing = assets
            .iter()
            .zip(&states)
            .flat_map(|(asset, (_, labels))| {
                labels.iter().map(move |label| format!("{}：{label}", dir_label(&asset.path)))
            })
            .collect::<Vec<_>>();
        let state = if !missing.is_empty() || assets.is_empty() {
            RendererState::Mismatch
        } else if states.iter().all(|(s, _)| *s == RendererState::Patched) {
            RendererState::Patched
        } else if states.iter().all(|(s, _)| *s == RendererState::Pristine) {
            RendererState::Pristine
        } else {
            RendererState::Outdated
        };
        (state, missing)
    }

    fn classify(text: &str) -> (RendererState, Vec<String>) {
        let renderer = Renderer::new(text);
        if renderer.is_patched() {
            return (RendererState::Patched, Vec::new());
        }
        if renderer.is_outdated() {
            return (RendererState::Outdated, Vec::new());
        }
        let missing = renderer.missing();
        if missing.is_empty() {
            (RendererState::Pristine, Vec::new())
        } else {
            (RendererState::Mismatch, missing)
        }
    }

    /// 供 example 导出改写结果：锚点命中只说明找对了地方，拼出来的 JS 能不能解析要另行检查
    pub fn patch_renderer_text(text: &str) -> Result<String> {
        Renderer::new(text).patch()
    }

    /// 供 example 对真实 renderer 做端到端演练，不写文件。
    /// 逐锚点报告命中数：Desktop 更新后靠这个定位是哪一处漂了
    pub fn probe_patch(text: &str) -> String {
        let renderer = Renderer::new(text);
        let mut out = vec![format!("before={:?}", classify(text).0)];
        for anchor in renderer.anchors() {
            out.push(format!(
                "  [{}] pattern={} patched={}",
                anchor.label,
                anchor.matches(text).len(),
                rx(anchor.patched).find_iter(text).count(),
            ));
        }
        match renderer.patch_unchecked() {
            Ok(patched) => {
                let after = Renderer::new(&patched);
                out.push(format!(
                    "after={:?} delta={} bytes",
                    classify(&patched).0,
                    patched.len() as i64 - text.len() as i64
                ));
                for anchor in after.anchors() {
                    let hits = rx(anchor.patched).find_iter(&patched).count();
                    out.push(format!(
                        "  [{}] patched={}{}",
                        anchor.label,
                        hits,
                        if hits == 1 { "" } else { "   <-- 校验卡在这里" }
                    ));
                }
            }
            Err(error) => out.push(format!("patch failed: {error:#}")),
        }
        out.join("\n")
    }

    pub fn probe_renderer_dir(v1: &Path, output: Option<&Path>) -> Result<String> {
        let assets = renderer_assets(v1)?;
        let mut out = vec![format!("bundle before={:?} files={}", classify_assets(&assets).0, assets.len())];
        for asset in &assets {
            out.push(format!("{}\n{}", dir_label(&asset.path), probe_patch(&asset.text)));
        }
        let plan = renderer_plan(&assets)?;
        if let Some(output) = output {
            fs::create_dir_all(output)?;
            let output = fs::canonicalize(output)?;
            let input = fs::canonicalize(v1)?;
            if output.starts_with(&input) || input.starts_with(&output) {
                bail!("探测输出目录不能包含或位于原始资源目录中");
            }
            for change in &plan {
                let name = change.path.file_name().context("renderer 文件名缺失")?;
                let dest = output.join(name).with_extension("mjs");
                if dest.exists() && fs::canonicalize(&dest)? == fs::canonicalize(&change.path)? {
                    bail!("探测输出不能覆盖原始 renderer");
                }
                fs::write(dest, &change.patched)?;
            }
        }
        out.push("bundle after=Patched".to_string());
        Ok(out.join("\n"))
    }

    fn renderer_info(msix_root: Option<&Path>) -> RendererInfo {
        let not_found = RendererInfo {
            native: false,
            file: None,
            state: RendererState::NotFound,
            missing_anchors: Vec::new(),
        };
        let Some(root) = msix_root else { return not_found };
        let v1 = renderer_dir(root);
        native_renderer(&v1).unwrap_or_else(|| renderer_info_in(&v1))
    }

    // 原生 Fast 开关改的就是旧补丁那个 chunk，两者不能叠加
    fn native_renderer(v1: &Path) -> Option<RendererInfo> {
        let path = crate::computer_use::installed_fast_state(v1)?;
        Some(RendererInfo {
            native: true,
            file: Some(dir_label(&path)),
            state: RendererState::Patched,
            missing_anchors: Vec::new(),
        })
    }

    fn renderer_info_in(v1: &Path) -> RendererInfo {
        match renderer_assets(v1) {
            Ok(assets) => {
                let file = Some(assets.iter().map(|a| dir_label(&a.path)).collect::<Vec<_>>().join(" · "));
                let (state, missing_anchors) = classify_assets(&assets);
                RendererInfo { native: false, file, state, missing_anchors }
            }
            Err(error) => RendererInfo {
                native: false,
                file: None,
                state: if !v1.is_dir() {
                    RendererState::NotFound
                } else if error.downcast_ref::<std::io::Error>().is_some() {
                    RendererState::Unreadable
                } else {
                    RendererState::Mismatch
                },
                missing_anchors: vec![format!("{error:#}")],
            },
        }
    }

    fn snapshot(cli: Option<&CliInstall>) -> FastModeStatus {
        let msix = msix();
        FastModeStatus {
            supported: true,
            desktop_version: msix.as_ref().map(|(v, _)| v.clone()),
            msix_path: msix.as_ref().map(|(_, p)| p.clone()),
            cli_version: cli.map(|c| c.version.clone()),
            cli_dir: cli.map(|c| c.exe_dir.clone()),
            wrapper: cli.map(|c| wrapper_state(&c.exe_dir)).unwrap_or(WrapperState::NoCli),
            renderer: renderer_info(msix.as_ref().map(|(_, p)| p.as_path())),
            settings: load_settings(),
            speed: SpeedStats::default(),
        }
    }

    fn status_light() -> FastModeStatus {
        snapshot(current_cli().as_ref())
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

    pub(super) fn deploy_wrapper(dir: &Path) -> Result<()> {
        if !Path::new(CSC).is_file() {
            bail!("未找到 .NET Framework 编译器 {CSC}");
        }
        let exe = dir.join(CLI_EXE);
        let real = dir.join(REAL_EXE);
        stage_wrapper_slot(&exe, &real, slot_occupant(&exe))?;
        let source = std::env::temp_dir().join("claude-plus-plus-fast-wrapper.cs");
        let compiled = fs::write(&source, wrapper_source()).is_ok() && {
            let out = format!("-out:{}", exe.display());
            let source_arg = source.to_string_lossy().into_owned();
            quiet(CSC, &["-nologo", "-optimize", "-r:System.Web.Extensions.dll", &out, &source_arg])
        };
        let _ = fs::remove_file(&source);
        if !compiled || !exe.is_file() {
            // 槽位已经腾空，不放回官方 Desktop 就起不来 CLI
            remove_wrapper_in(&exe, &real).context("wrapper 编译失败，放回官方 CLI 也失败")?;
            bail!("wrapper 编译失败，已放回官方 CLI");
        }
        Ok(())
    }

    pub(super) fn remove_wrapper(dir: &Path) -> Result<bool> {
        remove_wrapper_in(&dir.join(CLI_EXE), &dir.join(REAL_EXE))
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

    struct RendererChange {
        path: PathBuf,
        original: String,
        patched: String,
    }

    fn renderer_plan(assets: &[RendererAsset]) -> Result<Vec<RendererChange>> {
        assets
            .iter()
            .map(|asset| {
                let backup = asset.path.with_extension("js.orig");
                let source = if backup.exists() {
                    fs::read_to_string(&backup).with_context(|| format!("读取 {}", backup.display()))?
                } else {
                    asset.text.clone()
                };
                if classify(&source).0 != RendererState::Pristine {
                    bail!("{} 缺少可唯一匹配的原始代码：{:?}", dir_label(&asset.path), classify(&source));
                }
                let patched =
                    Renderer::new(&source).patch().with_context(|| format!("改写 {}", dir_label(&asset.path)))?;
                Ok(RendererChange { path: asset.path.clone(), original: asset.text.clone(), patched })
            })
            .collect()
    }

    fn write_renderer_changes(
        changes: &[RendererChange],
        mut write: impl FnMut(&Path, &str) -> Result<()>,
    ) -> Result<()> {
        for (index, change) in changes.iter().enumerate() {
            if let Err(error) = write(&change.path, &change.patched) {
                let mut failures = Vec::new();
                // 失败的写入也可能已经截断文件，必须包含在回滚范围内。
                for previous in changes[..=index].iter().rev() {
                    if let Err(rollback) = fs::write(&previous.path, &previous.original) {
                        failures.push(format!("{}：{rollback}", dir_label(&previous.path)));
                    }
                }
                if !failures.is_empty() {
                    bail!("写入失败：{error:#}；回滚失败：{}；原始备份仍保留", failures.join("；"));
                }
                return Err(error.context("renderer 写入失败，已回滚本次改动"));
            }
        }
        Ok(())
    }

    fn patch_renderer_in(v1: &Path, mut grant: impl FnMut(&Path) -> Result<()>) -> Result<String> {
        let assets = renderer_assets(v1)?;
        let plan = renderer_plan(&assets)?;
        grant(v1)?;
        for change in &plan {
            grant(&change.path)?;
            let backup = change.path.with_extension("js.orig");
            if !backup.exists() {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&backup)
                    .with_context(|| format!("备份 {}", backup.display()))?;
                file.write_all(change.original.as_bytes()).context("写入原始备份")?;
                file.sync_all().context("保存原始备份")?;
            }
        }
        write_renderer_changes(&plan, |path, text| {
            fs::write(path, text).with_context(|| format!("写回 {}", path.display()))
        })?;
        Ok(format!("renderer 已 patch: {}", plan.iter().map(|c| dir_label(&c.path)).collect::<Vec<_>>().join("、")))
    }

    fn patch_renderer(v1: &Path) -> Result<String> {
        ensure!(
            crate::computer_use::installed_fast_state(v1).is_none(),
            "已装原生 Fast 开关，不能再叠加 renderer 补丁"
        );
        patch_renderer_in(v1, grant_write)
    }

    fn restore_renderer_in(v1: &Path, mut grant: impl FnMut(&Path) -> Result<()>) -> Result<usize> {
        let mut plan = Vec::new();
        for entry in fs::read_dir(v1).context("读取 renderer 目录")? {
            let backup = entry?.path();
            if !backup.to_string_lossy().ends_with(".js.orig") {
                continue;
            }
            let path = backup.with_extension("");
            plan.push(RendererChange {
                original: fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?,
                patched: fs::read_to_string(&backup).with_context(|| format!("读取 {}", backup.display()))?,
                path,
            });
        }
        if plan.is_empty() {
            return Ok(0);
        }
        grant(v1)?;
        for change in &plan {
            grant(&change.path)?;
        }
        write_renderer_changes(&plan, |path, text| {
            fs::write(path, text).with_context(|| format!("还原 {}", path.display()))
        })?;
        for change in &plan {
            fs::remove_file(change.path.with_extension("js.orig")).context("移除原始备份")?;
        }
        Ok(plan.len())
    }

    fn restore_renderer(v1: &Path) -> Result<usize> {
        restore_renderer_in(v1, grant_write)
    }

    fn has_renderer_backups(root: &Path) -> bool {
        fs::read_dir(renderer_dir(root))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().to_string_lossy().ends_with(".js.orig"))
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
        let cli = current_cli();
        let before = snapshot(cli.as_ref());
        let wrapper = cli
            .as_ref()
            .context("未找到 Desktop 装好的 bundled CLI（Claude-3p\\claude-code）")
            .and_then(deploy_cli);
        let renderer = match before.renderer.state {
            _ if before.renderer.native => Ok(NATIVE_NOTE.to_string()),
            RendererState::Patched => Ok("renderer 已是 patch 状态".to_string()),
            RendererState::Pristine | RendererState::Outdated => run_with_elevation("apply"),
            RendererState::Mismatch => Ok(format!(
                "renderer 锚点未命中（{}），跳过；wrapper 仍让 Opus 会话默认 fast",
                before.renderer.missing_anchors.join("、")
            )),
            RendererState::NotFound => Ok("未找到 renderer 文件，跳过".to_string()),
            RendererState::Unreadable => Ok("renderer 文件不可读，跳过".to_string()),
            RendererState::Unsupported => Ok("该平台不 patch renderer".to_string()),
        };
        let outcome = Outcome { wrapper: Some(wrapper), renderer: Some(renderer) };
        let mut settings = load_settings();
        record_outcome(&mut settings, &status_light(), &outcome);
        save_settings(&settings)?;
        outcome.into_report()
    }

    pub fn uninstall() -> Result<ActionReport> {
        if procs::any_desktop_running() {
            bail!("Claude Desktop 正在运行，请先退出后再还原");
        }
        let wrapper = restore_all_cli().map(|n| format!("已还原 {n} 个版本目录的官方 CLI"));
        let status = status_light();
        let renderer = if status.renderer.native {
            Ok(NATIVE_NOTE.to_string())
        } else if matches!(status.renderer.state, RendererState::Patched | RendererState::Outdated)
            || status.msix_path.as_deref().is_some_and(has_renderer_backups)
        {
            run_with_elevation("restore")
        } else {
            Ok("renderer 无需还原".to_string())
        };
        // 部分失败也停掉守护：否则它会把刚还原的那一半又装回去
        let mut settings = load_settings();
        record_uninstall(&mut settings);
        save_settings(&settings)?;
        Outcome { wrapper: Some(wrapper), renderer: Some(renderer) }.into_report()
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
        let cli = current_cli();
        let status = snapshot(cli.as_ref());
        if settings.failed_for.as_deref() == Some(version_key(&status).as_str()) {
            return RepairOutcome::Nothing;
        }
        let wrapper_target = cli.as_ref().filter(|_| status.wrapper != WrapperState::Deployed);
        let need_renderer = matches!(status.renderer.state, RendererState::Pristine | RendererState::Outdated);
        if wrapper_target.is_none() && !need_renderer {
            return RepairOutcome::Nothing;
        }
        if procs::any_desktop_running() {
            return RepairOutcome::Blocked;
        }
        let outcome = Outcome {
            wrapper: wrapper_target.map(deploy_cli),
            renderer: need_renderer.then(|| run_with_elevation("apply")),
        };
        record_outcome(&mut settings, &status_light(), &outcome);
        let _ = save_settings(&settings);
        outcome.into_repair()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 1.46388.4.0 的真实片段：变量名与 1.49585 那版不同，用来钉住跨版本能力
        const OLD_RENDERER: &str = concat!(
            r#"function zc({sessionRef:t,sessionMeta:n,selectedFolder:r,modelId:a,fastModeFor:o,capabilities:s,config:l,openingKey:u}){"#,
            r#"let b=i?o(F(i))!==void 0:!1,x=(a?.toLowerCase().includes("opus-4-6")??!1)||Ue(),"#,
            r#"S=b&&x,C=t(e=>o(F(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Ue()),[o]),w=b&&!x,"#,
            r#"D=Vc(r?.fastModeDisabledReason,{hasRaven:E,canManageOrg:m}),O=D!==null,j=l?.fastMode?.value===!0,M=1}"#,
            r#"function Bc({isNew:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,perSessionOptInAllowed:o,"#,
            r#"fastModeNeedsDesktopUpdate:s,modelSupportsFastMode:c,fastModeBlocked:l}){"#,
            r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;"#,
            r#"return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}}"#,
        );

        /// 1.49585.0.0 的真实片段：函数改名 zc→df / Bc→xf，modelId 形参 a→i
        const NEW_RENDERER: &str = concat!(
            r#"function df({sessionRef:t,sessionMeta:n,selectedFolder:r,modelId:i,fastModeFor:o,capabilities:s,config:l,openingKey:u}){"#,
            r#"let b=i?o(D(i))!==void 0:!1,x=(i?.toLowerCase().includes("opus-4-6")??!1)||Be(),"#,
            r#"S=b&&x,C=e(e=>o(D(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Be()),[o]),w=b&&!x,"#,
            r#"O=pf(n?.fastModeDisabledReason,{hasRaven:E,canManageOrg:h}),A=O!==null,j=l?.fastMode?.value===!0,M=1}"#,
            r#"function xf({isNew:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,perSessionOptInAllowed:o,"#,
            r#"fastModeNeedsDesktopUpdate:s,modelSupportsFastMode:c,fastModeBlocked:l}){"#,
            r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;"#,
            r#"return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}}"#,
        );

        const CURRENT_RENDERER: &str = concat!(
            r#"function ul({sessionRef:e,sessionMeta:t,selectedFolder:n,modelId:i,fastModeFor:a,capabilities:o,config:s,openingKey:c}){"#,
            r#"let y=i?a(F(i))!==void 0:!1,b=(i?.toLowerCase().includes("opus-4-6")??!1)||Le(),"#,
            r#"x=y&&b,S=p(e=>a(F(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||Le()),[a]),C=y&&!b,"#,
            r#"w=s?.fastModePerSessionOptIn?.value!==!1,T=fl(t?.fastModeDisabledReason),E=s?.fastMode?.value===!0,"#,
            r#"D=s?.fastModePerSessionOptIn?.value===!0;"#,
            r#"return{modelSupportsFastMode:x,modelSupportsFastModeFor:S,fastModeNeedsDesktopUpdate:C,fastModeBlocked:T}}"#,
            r#"function dl({envType:e,sessionRef:t,isLocalSession:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,"#,
            r#"perSessionOptInAllowed:o,fastModeNeedsDesktopUpdate:s,modelSupportsFastMode:c,fastModeBlocked:l}){"#,
            r#"let u=(r||i)&&a&&o&&!i&&(t?n:e==="local"||e==="ssh")&&!s&&c;"#,
            r#"return{showFastModeToggle:u,fastModeToggleDisabled:u&&l}}"#,
        );

        /// 2.9939.2.0：显示条件前多了 remote 判定，模型支持与禁用原因都带上 remote 分支
        const REMOTE_RENDERER: &str = concat!(
            r#"function ql({sessionRef:e,sessionMeta:t,draftEnvType:n,transportFor:a,selectedFolder:o,modelId:s,"#,
            r#"fastModeFor:c,capabilities:l,config:u,openingKey:d,offerRemoteFastMode:f=!0}){"#,
            r#"let g=C?.setFastMode,b=l.fastMode!==!1,w=!!g,T=e?.type==="local",E=t?.environmentKind,"#,
            r#"D=f&&(e?e.type==="remote"&&E!=="bridge":n==="anthropic_cloud"),"#,
            r#"A=D&&t?.cliVersion!==void 0&&es(t.cliVersion,Kl)<0,"#,
            r#"j=s?c(V(s))!==void 0:!1,M=(s?.toLowerCase().includes("opus-4-6")??!1)||ve(),"#,
            r#"N=j&&(D||M),P=p(e=>c(V(e))!==void 0&&(e.toLowerCase().includes("opus-4-6")||ve()),[c]),"#,
            r#"F=!D&&j&&!M,I=D||u?.fastModePerSessionOptIn?.value!==!1,"#,
            r#"L=A||Yl(t?.fastModeDisabledReason),R=u?.fastMode?.value===!0,"#,
            r#"z=u?.fastModePerSessionOptIn?.value===!0;"#,
            r#"return{modelSupportsFastMode:N,modelSupportsFastModeFor:P,fastModeNeedsDesktopUpdate:F,fastModeBlocked:L}}"#,
            r#"function tl({envType:e,sessionRef:t,isLocalSession:n,fastModeCapable:r,fastModeEnableHint:i,"#,
            r#"fastModeIpcAvailable:a,remoteFastMode:o,remoteFastModeAwaitingReport:s,perSessionOptInAllowed:c,"#,
            r#"fastModeNeedsDesktopUpdate:l,modelSupportsFastMode:u,fastModeBlocked:d}){"#,
            r#"let f=o&&(t?t.type==="remote":e==="anthropic_cloud"||e==="byoc"||e==="pool"),"#,
            r#"p=(r||i)&&(a||f)&&c&&!i&&(a&&(t?n:e==="local"||e==="ssh")||f)&&!s&&!l&&u;"#,
            r#"return{showFastModeToggle:p,fastModeToggleDisabled:p&&d}}"#,
        );

        /// modelId 形参各版本不同名，随样本一起登记
        struct Case {
            label: &'static str,
            source: &'static str,
            model_id: &'static str,
        }

        const RENDERER_CASES: [Case; 4] = [
            Case { label: "1.46388.4.0", source: OLD_RENDERER, model_id: "a" },
            Case { label: "1.49585.0.0", source: NEW_RENDERER, model_id: "i" },
            Case { label: "2.110.0.0", source: CURRENT_RENDERER, model_id: "i" },
            Case { label: "2.9939.2.0", source: REMOTE_RENDERER, model_id: "s" },
        ];

        /// 与 fast mode 无关、形态却相近的真实片段：精简样本里没有这种噪声，
        /// patched 正则写松了也测不出来
        const DECOY: &str = concat!(
            r#"function jh({doc:e}=R,t=e.defaultView,n=!1,r=()=>{U.current&&W(null,!0)},"#,
            r#"i=e=>{H.current=null,n=!1,r(),W(R.tileOf(e.target))}){return null}"#,
        );

        const MODEL_MENU: &str = concat!(
            r#"function Rd({catalog:E,contextModel:i}){let k=x(()=>{let s=E.models,a=e=>e;"#,
            r#"return{...E,models:s.map(a)}},[E,i]),A=k?.models.find(e=>e.id===i);"#,
            r#"return dr({contextModel:i,shapedCatalog:k,"data-testid":"epitaxy-cds-model-selector"})}"#,
        );

        const SPLIT_CORE: &str = concat!(
            r#"function Bs({sessionRef:e,sessionMeta:t,draftEnvType:n,transportFor:r,selectedFolder:a,modelId:o,fastModeFor:c,capabilities:l,config:u,openingKey:d,offerRemoteFastMode:f=!0}){"#,
            r#"let T=f&&(e?e.type==="remote":n==="anthropic_cloud"),O=T&&t?.cliVersion!==void 0,"#,
            r#"k=o?c(H(o))!==void 0:!1,A=(o?.toLowerCase().includes("opus-4-6")??!1)||He(),"#,
            r#"j=x(e=>c(H(e))!==void 0&&(T||e.toLowerCase().includes("opus-4-6")||He()),[c,T]),te=!!o&&j(o),N=!T&&k&&!A,"#,
            r#"P=T||u?.fastModePerSessionOptIn?.value!==!1,I=O||Hs(t?.fastModeDisabledReason),L=u?.fastMode?.value===!0,ne=u?.fastModePerSessionOptIn?.value===!0;"#,
            r#"return{modelSupportsFastMode:te,modelSupportsFastModeFor:j,fastModeBlocked:I}}"#,
            r#"function ps({envType:e,sessionRef:t,isLocalSession:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,remoteFastMode:o,remoteFastModeAwaitingReport:s,perSessionOptInAllowed:c,fastModeNeedsDesktopUpdate:l,modelSupportsFastMode:u,fastModeBlocked:d}){"#,
            r#"let f=o&&(t?t.type==="remote":e==="anthropic_cloud"||e==="byoc"||e==="pool"),p=r&&!i&&c&&!s?f?"remote":a&&(t?n:e==="local"||e==="ssh")?"local":null:null,m=p!==null&&!l&&u;"#,
            r#"return{showFastModeToggle:m,fastModeToggleDisabled:m&&d,fastModeCarrier:p}}"#,
        );

        const COMPILED_CORE: &str = concat!(
            r#"function rc(e){let $=J9(97),{sessionRef:t,sessionMeta:n,modelId:s,fastModeFor:c,capabilities:l,config:u,offerRemoteFastMode:f}=e,"#,
            r#"x=l.fastMode!==!1,E=f&&t?.type==="remote",D=n?.fastModeState,O=D==="on"||D==="cooldown",A=E&&n?.cliVersion!==void 0&&ka(n.cliVersion,nc)<0&&!O,"#,
            r#"N=J2($,0,c,s)?K2($,0,c,s,s?c(U(s))!==void 0:!1):$[2],P=J1($,3,s)?K1($,3,s,(s?.toLowerCase().includes("opus-4-6")??!1)||ft()):$[4],"#,
            r#"ee=J2($,5,c,E)?K2($,5,c,E,e=>c(U(e))!==void 0&&(E||e.toLowerCase().includes("opus-4-6")||ft())):$[7],L=!!s&&ee(s),R=!E&&N&&!P,"#,
            r#"z=E||u?.fastModePerSessionOptIn?.value!==!1,te=tc(x),B=n?.fastModeDisabledReason,V=J1($,8,B)?K1($,8,B,cc(B)):$[9],"#,
            r#"H=!A&&(B==="extra_usage_disabled"||te&&!O&&!V),W=A||H||V,ne=J5($,10,W,H,A,E,B,n)?K5($,10,W,H,A,E,B,n,W&&!H?Qs(B,{engineTooOld:A,host:E?fr(n):void 0}):null):$[16],G=u?.fastMode?.value===!0,K=u?.fastModePerSessionOptIn?.value===!0;"#,
            r#"return{modelSupportsFastMode:L,modelSupportsFastModeFor:ee,fastModeNeedsDesktopUpdate:R,fastModeBlocked:W,fastModeNeedsCredits:H,fastModeChoice:!A&&!V&&G,reason:ne}}"#,
            r#"function ps({envType:e,sessionRef:t,isLocalSession:n,fastModeCapable:r,fastModeEnableHint:i,fastModeIpcAvailable:a,remoteFastMode:o,remoteFastModeAwaitingReport:s,perSessionOptInAllowed:c,fastModeNeedsDesktopUpdate:l,modelSupportsFastMode:u,fastModeBlocked:d}){"#,
            r#"let f=o&&(t?t.type==="remote":e==="anthropic_cloud"||e==="byoc"||e==="pool"),p=r&&!i&&c&&!s?f?"remote":a&&(t?n:e==="local"||e==="ssh")?"local":null:null,m=p!==null&&!l&&u;"#,
            r#"return{showFastModeToggle:m,fastModeToggleDisabled:m&&d,fastModeCarrier:p}}"#,
        );

        const COMPILED_MENU: &str = concat!(
            r#"function menu(R,s){let $=J9(43),l=R.models,u=J2($,37,l,s)?K2($,37,l,s,l.map(s)):$[39],K=J2($,40,R,u)?K2($,40,R,u,{...R,models:u}):$[42];"#,
            r#"return{catalog:K,"data-testid":"epitaxy-cds-model-selector"}}"#,
        );

        #[test]
        fn compiler_cache_shapes_patch_and_restore() {
            for (core, menu) in [
                (COMPILED_CORE.to_string(), COMPILED_MENU.to_string()),
                (
                    rx(r"\b(?:[A-Za-z]|rc|ps|ee|ne|te|J9|J1|J2|J5|K1|K2|K5)\b")
                        .replace_all(COMPILED_CORE, |c: &regex::Captures<'_>| format!("${}$", &c[0])).into_owned(),
                    rx(r"\b(?:[A-Za-z]|menu|J9|J2|K2)\b")
                        .replace_all(COMPILED_MENU, |c: &regex::Captures<'_>| format!("${}$", &c[0])).into_owned(),
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                fs::write(dir.path().join("core.js"), &core).unwrap();
                fs::write(dir.path().join("menu.js"), &menu).unwrap();
                assert_eq!(renderer_info_in(dir.path()).state, RendererState::Pristine);
                patch_renderer_in(dir.path(), |_| Ok(())).unwrap();
                assert_eq!(renderer_info_in(dir.path()).state, RendererState::Patched);
                let patched = fs::read_to_string(dir.path().join("core.js")).unwrap();
                patch_renderer_in(dir.path(), |_| Ok(())).unwrap();
                assert_eq!(fs::read_to_string(dir.path().join("core.js")).unwrap(), patched);
                assert_eq!(restore_renderer_in(dir.path(), |_| Ok(())).unwrap(), 2);
                assert_eq!(fs::read_to_string(dir.path().join("core.js")).unwrap(), core);
                assert_eq!(fs::read_to_string(dir.path().join("menu.js")).unwrap(), menu);
            }
        }

        #[test]
        fn compiler_cache_alias_drift_and_duplicate_anchors_are_refused() {
            for (from, to) in [
                ("K2($,5,c,E,e=>", "K2($,5,c,Z,e=>"),
                ("K2($,5,c,E,e=>", "K2(other,5,c,E,e=>"),
                ("K2($,5,c,E,e=>", "K2($,6,c,E,e=>"),
                ("L=!!s&&ee(s)", "L=!!other&&ee(other)"),
                ("}=e,x=", "}=other,x="),
                ("K1($,8,B,cc(B))", "K1($,8,B,cc(other))"),
                ("W=A||H||V", "W=A||H||other"),
            ] {
                assert!(COMPILED_CORE.contains(from));
                assert!(Renderer::new(&COMPILED_CORE.replace(from, to)).patch().is_err(), "{from}");
            }
            for source in [COMPILED_CORE, COMPILED_MENU] {
                assert!(Renderer::new(&format!("{source}{source}")).patch().is_err());
            }
            for (from, to) in [("models:u}", "models:other}"), ("...R,", "...other,")] {
                assert!(Renderer::new(&COMPILED_MENU.replace(from, to)).patch().is_err());
            }
        }

        fn split_assets(dir: &Path) {
            fs::write(dir.join("core.js"), SPLIT_CORE).unwrap();
            fs::write(dir.join("menu.js"), MODEL_MENU).unwrap();
            fs::write(dir.join("noise.js"), DECOY).unwrap();
            fs::write(dir.join("reference.js"), r#"e.querySelector('[data-testid="epitaxy-cds-model-selector"]')"#)
                .unwrap();
        }

        #[test]
        fn callback_support_keeps_remote_settings_and_carrier() {
            for source in [
                SPLIT_CORE.to_string(),
                rx(r"\b(?:[A-Za-z]|te|ps|Bs)\b")
                    .replace_all(SPLIT_CORE, |c: &regex::Captures<'_>| format!("${}$", &c[0]))
                    .into_owned(),
            ] {
                assert_eq!(classify(&source).0, RendererState::Pristine);
                for anchor in Renderer::new(&source).anchors() {
                    assert!(unique_match(&source, anchor.patched, &[]).is_none(), "{}", anchor.label);
                }
                let patched = Renderer::new(&source).patch().unwrap();
                assert_eq!(classify(&patched).0, RendererState::Patched);
                assert!(patched.contains("fastModeCarrier:__cppLocalFast?\"local\":"));
                assert!(patched.contains("fastModeDisabledReason"));
                assert!(is_renderer(&patched));
            }
            let patched = Renderer::new(SPLIT_CORE).patch().unwrap();
            assert!(patched.contains("j=x(e=>T?c(H(e))"));
            assert!(patched.contains("te=!!o&&j(o),N=!1,"));
            assert!(patched.contains("I=T?(O||Hs(t?.fastModeDisabledReason)):!1,"));
            for (from, to) in [
                ("te=!!o&&j(o)", "te=!!o&&wrong(o)"),
                ("[c,T]", "[c,Z]"),
                ("N=!T&&k&&!A", "N=!Z&&k&&!A"),
                ("te=!!o&&j(o)", "te=!!a&&j(a)"),
            ] {
                assert!(Renderer::new(&SPLIT_CORE.replace(from, to)).patch().is_err());
            }
        }

        #[test]
        fn split_renderer_patch_restore_and_partial_state() {
            let dir = tempfile::tempdir().unwrap();
            split_assets(dir.path());
            let state = renderer_info_in(dir.path());
            assert_eq!(state.state, RendererState::Pristine);
            assert!(state.file.unwrap().contains("menu.js"));
            patch_renderer_in(dir.path(), |_| Ok(())).unwrap();
            assert_eq!(renderer_info_in(dir.path()).state, RendererState::Patched);
            assert_eq!(fs::read_to_string(dir.path().join("core.js.orig")).unwrap(), SPLIT_CORE);
            assert_eq!(fs::read_to_string(dir.path().join("menu.js.orig")).unwrap(), MODEL_MENU);
            fs::write(dir.path().join("menu.js"), MODEL_MENU).unwrap();
            assert_eq!(renderer_info_in(dir.path()).state, RendererState::Outdated);
            patch_renderer_in(dir.path(), |_| Ok(())).unwrap();
            assert_eq!(renderer_info_in(dir.path()).state, RendererState::Patched);
            assert_eq!(restore_renderer_in(dir.path(), |_| Ok(())).unwrap(), 2);
            assert_eq!(renderer_info_in(dir.path()).state, RendererState::Pristine);
            assert_eq!(fs::read_to_string(dir.path().join("core.js")).unwrap(), SPLIT_CORE);
            assert_eq!(fs::read_to_string(dir.path().join("menu.js")).unwrap(), MODEL_MENU);
            assert_eq!(fs::read_to_string(dir.path().join("noise.js")).unwrap(), DECOY);
        }

        #[test]
        fn bundle_validation_finishes_before_any_write() {
            for menu in [None, Some(MODEL_MENU.replace("models:s.map(a)", "models:s.flatMap(a)"))] {
                let dir = tempfile::tempdir().unwrap();
                fs::write(dir.path().join("core.js"), SPLIT_CORE).unwrap();
                if let Some(menu) = menu {
                    fs::write(dir.path().join("menu.js"), menu).unwrap();
                }
                let mut touched = false;
                assert!(patch_renderer_in(dir.path(), |_| {
                    touched = true;
                    Ok(())
                })
                .is_err());
                assert!(!touched);
                assert_eq!(fs::read_to_string(dir.path().join("core.js")).unwrap(), SPLIT_CORE);
                assert!(!dir.path().join("core.js.orig").exists());
            }
            let dir = tempfile::tempdir().unwrap();
            split_assets(dir.path());
            fs::write(dir.path().join("duplicate.js"), MODEL_MENU).unwrap();
            assert!(renderer_assets(dir.path()).is_err());
        }

        #[test]
        fn failed_second_write_rolls_back_both_files() {
            let dir = tempfile::tempdir().unwrap();
            split_assets(dir.path());
            let plan = renderer_plan(&renderer_assets(dir.path()).unwrap()).unwrap();
            let mut count = 0;
            let result = write_renderer_changes(&plan, |path, text| {
                count += 1;
                if count == 2 {
                    fs::write(path, "partial write")?;
                    bail!("injected write failure");
                }
                fs::write(path, text)?;
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(dir.path().join("core.js")).unwrap(), SPLIT_CORE);
            assert_eq!(fs::read_to_string(dir.path().join("menu.js")).unwrap(), MODEL_MENU);
        }

        #[test]
        fn combined_and_legacy_bundles_still_roundtrip() {
            for source in [OLD_RENDERER.to_string(), format!("{CURRENT_RENDERER}{MODEL_MENU}")] {
                let dir = tempfile::tempdir().unwrap();
                fs::write(dir.path().join("combined.js"), &source).unwrap();
                patch_renderer_in(dir.path(), |_| Ok(())).unwrap();
                assert_eq!(renderer_info_in(dir.path()).state, RendererState::Patched);
                assert_eq!(restore_renderer_in(dir.path(), |_| Ok(())).unwrap(), 1);
                assert_eq!(fs::read_to_string(dir.path().join("combined.js")).unwrap(), source);
            }
        }

        #[test]
        fn model_menu_patch_is_required_for_the_cds_selector() {
            let source = format!("{CURRENT_RENDERER}{MODEL_MENU}");
            assert_eq!(classify(&source).0, RendererState::Pristine);
            let patched = Renderer::new(&source).patch().unwrap();
            assert_eq!(classify(&patched).0, RendererState::Patched);
            assert!(patched.contains(r#"fast_mode:{type:"toggle",name:"Fast mode""#));
            assert!(patched.contains(r#"selector_label:"Fast""#));
            assert!(Renderer::new(&patched).patch().is_err());

            let legacy = format!("{}{MODEL_MENU}", Renderer::new(CURRENT_RENDERER).patch().unwrap());
            assert_eq!(classify(&legacy).0, RendererState::Outdated);
            assert_eq!(serde_json::to_string(&RendererState::Outdated).unwrap(), r#""outdated""#);
            assert!(Renderer::new(&legacy).patch().is_err());
            assert_eq!(Renderer::new(&source).patch().unwrap(), patched);
        }

        #[test]
        fn model_menu_is_not_injected_into_legacy_selectors() {
            let menu = MODEL_MENU.replace("epitaxy-cds-model-selector", "legacy-model-selector");
            let source = format!("{OLD_RENDERER}{menu}");
            let patched = Renderer::new(&source).patch().unwrap();
            assert_eq!(classify(&patched).0, RendererState::Patched);
            assert!(!patched.contains("fast_mode:"));
            assert!(patched.ends_with(&menu));
        }

        #[test]
        fn model_menu_drift_and_duplicates_are_refused() {
            let drifted_menu = MODEL_MENU.replace("models:s.map(a)", "models:s.flatMap(a)");
            for menu in [drifted_menu, format!("{MODEL_MENU}{MODEL_MENU}")] {
                let source = format!("{CURRENT_RENDERER}{menu}");
                assert_eq!(classify(&source), (RendererState::Mismatch, vec!["模型菜单".to_string()]));
                assert!(Renderer::new(&source).patch().is_err());
                let legacy = format!("{}{menu}", Renderer::new(CURRENT_RENDERER).patch().unwrap());
                assert_eq!(classify(&legacy).0, RendererState::Mismatch);
            }
        }

        #[test]
        fn model_menu_identifiers_can_be_renamed() {
            let identifiers = rx(r"\b(?:[A-Za-z]|Rd)\b");
            let source = format!("{CURRENT_RENDERER}{MODEL_MENU}");
            let renamed = identifiers.replace_all(&source, |caps: &regex::Captures<'_>| {
                format!("${}$", &caps[0])
            });
            assert_eq!(classify(&renamed).0, RendererState::Pristine);
            let patched = Renderer::new(&renamed).patch().unwrap();
            assert_eq!(classify(&patched).0, RendererState::Patched);
            assert!(patched.contains("models:$s$.map($a$).map("));
        }

        #[test]
        fn patches_supported_desktop_versions() {
            for Case { label, source, .. } in RENDERER_CASES {
                assert_eq!(classify(source).0, RendererState::Pristine, "{label} 原始识别");
                let patched = Renderer::new(source).patch().expect(label);
                assert_eq!(classify(&patched).0, RendererState::Patched, "{label} patch 后识别");
                assert!(patched.contains("fastModeToggleDisabled:!1"), "{label} 开关不再禁用");
                assert!(!patched.contains("fastModeDisabledReason"), "{label} 禁用原因已断开");
                assert!(patched.contains(r#"includes("opus-4-8")"#), "{label} 放行 4.8");
                // 4.6 只该留在被弃用的 x 定义里，判定里不该再出现
                assert!(!patched.contains(r#"includes("opus-4-6")||"#), "{label} 不再认 4.6");
            }
        }

        #[test]
        fn patched_markers_never_match_untouched_code() {
            for Case { label, source: base, .. } in RENDERER_CASES {
                let source = format!("{DECOY}{base}");
                for anchor in Renderer::new(&source).anchors() {
                    // 原始文本里就命中，patch 后会变成两处，唯一性校验随之失败
                    assert_eq!(
                        rx(anchor.patched).find_iter(&source).count(),
                        0,
                        "{label} 的「{}」patched 正则误命中未改动代码",
                        anchor.label
                    );
                }
                assert_eq!(classify(&source).0, RendererState::Pristine, "{label} 原始识别");
                let patched = Renderer::new(&source).patch().expect(label);
                assert_eq!(classify(&patched).0, RendererState::Patched, "{label} patch 后识别");
            }
        }

        #[test]
        fn renderer_stays_recognisable_after_patching() {
            for Case { label, source, .. } in RENDERER_CASES {
                assert!(is_renderer(source), "{label} 原始应被认出");
                let patched = Renderer::new(source).patch().expect(label);
                // patch 抹掉 fastModeDisabledReason，识别不能依赖它，否则改完就报未找到
                assert!(is_renderer(&patched), "{label} patch 后仍应被认出");
            }
        }

        #[test]
        fn model_id_comes_from_the_target_function() {
            // modelId 形参不同名，硬编码会在另一版生成引用错变量的代码
            for Case { label, source, model_id } in RENDERER_CASES {
                let patched = Renderer::new(source).patch().unwrap();
                let references =
                    |id: &str| patched.contains(&format!(r#"!!({id}&&({id}.toLowerCase()"#));
                assert!(references(model_id), "{label} 应引用 {model_id}");
                for other in RENDERER_CASES.iter().map(|c| c.model_id).filter(|id| *id != model_id) {
                    assert!(!references(other), "{label} 不该引用 {other}");
                }
            }
        }

        #[test]
        fn patch_is_idempotent() {
            for Case { label, source, .. } in RENDERER_CASES {
                let once = Renderer::new(source).patch().unwrap();
                // 已改过的文本锚点不再命中，重复 patch 直接报错而非改坏
                assert!(Renderer::new(&once).patch().is_err(), "{label}");
                assert_eq!(classify(&once).0, RendererState::Patched, "{label}");
                assert_eq!(Renderer::new(source).patch().unwrap(), once, "{label} 备份重放");
            }
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
            for Case { label, source, .. } in RENDERER_CASES {
                let doubled = format!("{source}{source}");
                assert_eq!(classify(&doubled).0, RendererState::Mismatch, "{label}");
                assert!(Renderer::new(&doubled).patch().is_err(), "{label}");
            }
        }

        #[test]
        fn display_rewrite_keeps_preceding_declarations() {
            let patched = Renderer::new(REMOTE_RENDERER).patch().unwrap();
            assert!(patched.contains(
                r#"let f=o&&(t?t.type==="remote":e==="anthropic_cloud"||e==="byoc"||e==="pool"),p=a&&u;"#
            ));
            assert!(patched.contains("return{showFastModeToggle:p,fastModeToggleDisabled:!1}"));
        }

        #[test]
        fn remote_shape_of_support_is_fully_replaced() {
            let patched = Renderer::new(REMOTE_RENDERER).patch().unwrap();
            assert!(patched.contains(",N=!!(s&&(s.toLowerCase().includes(\"opus-5\")"));
            assert!(patched.contains(",F=!1,"));
            assert!(!patched.contains("N=j&&(D||M)"));
            assert!(!patched.contains("F=!D&&j&&!M"));
        }

        #[test]
        fn mixed_support_shapes_are_rejected() {
            // 模型支持用了 remote 形态而 desktop-update 用了本地形态：结构自相矛盾，不能当作可 patch
            let mixed = REMOTE_RENDERER.replace("F=!D&&j&&!M,", "F=j&&!M,");
            assert_eq!(
                classify(&mixed),
                (RendererState::Mismatch, vec!["模型支持判定".to_string()])
            );
            assert!(Renderer::new(&mixed).patch().is_err());
        }

        #[test]
        fn prefixed_direct_reason_is_cleared() {
            let patched = Renderer::new(REMOTE_RENDERER).patch().unwrap();
            assert!(patched.contains(",L=!1,R=u?.fastMode?.value===!0,"));
            assert!(!patched.contains("L=A||"));
            assert!(patched.contains("fastModeBlocked:L}"));
        }

        #[test]
        fn model_menu_tolerates_declarations_before_the_lookup() {
            let menu = MODEL_MENU.replace(
                "},[E,i]),A=k?.models.find(",
                "},[E,i]),P=p(e=>{c(e,{defaultWritten:!u})},[c,u]),A=k?.models.find(",
            );
            let source = format!("{REMOTE_RENDERER}{menu}");
            assert_eq!(classify(&source).0, RendererState::Pristine);
            let patched = Renderer::new(&source).patch().unwrap();
            assert_eq!(classify(&patched).0, RendererState::Patched);
            assert!(patched.contains(r#"fast_mode:{type:"toggle",name:"Fast mode""#));
            assert!(patched.contains("P=p(e=>{c(e,{defaultWritten:!u})},[c,u]),A=k?.models.find("));
        }

        #[test]
        fn direct_reason_preserves_settings_and_consumers() {
            let patched = Renderer::new(CURRENT_RENDERER).patch().unwrap();
            assert!(patched.contains(
                ",T=!1,E=s?.fastMode?.value===!0,D=s?.fastModePerSessionOptIn?.value===!0;"
            ));
            assert!(patched.contains("fastModeBlocked:T}"));
        }

        #[test]
        fn identifiers_can_contain_dollars() {
            let identifiers = rx(r"\b(?:[A-Za-z]|zc|df|Bc|xf|ul|dl|ql|tl)\b");
            for Case { label, source, model_id } in RENDERER_CASES {
                let renamed = identifiers.replace_all(source, |caps: &regex::Captures<'_>| {
                    format!("${}$", &caps[0])
                });
                assert_eq!(classify(&renamed).0, RendererState::Pristine, "{label}");
                let patched = Renderer::new(&renamed).patch().unwrap();
                assert_eq!(classify(&patched).0, RendererState::Patched, "{label}");
                assert!(is_renderer(&patched), "{label}");
                let model_id = format!("${model_id}$");
                assert!(
                    patched.contains(&format!("!!({model_id}&&({model_id}.toLowerCase()")),
                    "{label}"
                );
            }
        }

        #[test]
        fn mixed_reason_shapes_are_ambiguous() {
            let legacy = Renderer::new(NEW_RENDERER).find(&ANCHORS[2]).unwrap();
            let mixed = format!("{CURRENT_RENDERER}function extra(){{let {},j=1}}", &legacy[0]);
            assert_eq!(classify(&mixed), (RendererState::Mismatch, vec!["禁用原因".to_string()]));
            assert!(Renderer::new(&mixed).patch().is_err());

            // 复制一整段改完的形态才算真歧义：只有 x=null,y=!1 这种碎片属于无关代码，不该参与判定
            let patched = Renderer::new(CURRENT_RENDERER).patch().unwrap();
            let direct = "T=!1,E=s?.fastMode?.value===!0,";
            assert!(patched.contains(direct));
            let mixed = format!("{patched}function extra(){{let {direct}k=2}}");
            assert_eq!(classify(&mixed).0, RendererState::Mismatch);

            let legacy_patched = Renderer::new(NEW_RENDERER).patch().unwrap();
            let legacy = "O=null,A=!1,j=l?.fastMode?.value===!0,";
            assert!(legacy_patched.contains(legacy));
            for extra in [legacy, direct] {
                let mixed = format!("{legacy_patched}function extra(){{let {extra}k=2}}");
                assert_eq!(classify(&mixed).0, RendererState::Mismatch, "{extra}");
            }
        }

        #[test]
        fn direct_reason_requires_settings_context() {
            let drifted = CURRENT_RENDERER.replace("E=s?.fastMode?.value===!0,", "E=!0,");
            assert_eq!(classify(&drifted), (RendererState::Mismatch, vec!["禁用原因".to_string()]));
            assert!(Renderer::new(&drifted).patch().is_err());

            let patched = Renderer::new(CURRENT_RENDERER).patch().unwrap();
            let incomplete = patched.replace(
                "T=!1,E=s?.fastMode?.value===!0,",
                "T=fl(t?.fastModeDisabledReason),E=!0,"
            );
            let unrelated = format!("{incomplete}function extra(){{let a=1,b=!1,c=2}}");
            assert_eq!(classify(&unrelated).0, RendererState::Mismatch);
        }

        #[test]
        fn legacy_reason_requires_the_same_message_variable() {
            let drifted = NEW_RENDERER.replace("A=O!==null", "A=other!==null");
            assert_eq!(classify(&drifted), (RendererState::Mismatch, vec!["禁用原因".to_string()]));
            assert!(Renderer::new(&drifted).patch().is_err());
        }

        // 过了 1 MB 才会被认成官方本体
        fn official(tag: &str) -> Vec<u8> {
            let mut bytes = vec![0u8; WRAPPER_MAX_BYTES as usize + 1];
            bytes.extend_from_slice(tag.as_bytes());
            bytes
        }

        #[test]
        fn wrapper_source_changes_require_redeployment() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(CLI_EXE), b"legacy wrapper").unwrap();
            fs::write(dir.path().join(REAL_EXE), official("current")).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Outdated);
            fs::write(dir.path().join(CLI_EXE), wrapper_source_hash()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            fs::write(dir.path().join(CLI_EXE), b"other source hash").unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Outdated);
        }

        #[test]
        fn windows_wrapper_preserves_settings_and_prepares_the_child_environment() {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("wrapper.cs");
            let tests = dir.path().join("wrapper.tests.cs");
            let real = dir.path().join(REAL_EXE);
            fs::write(&source, wrapper_source()).unwrap();
            fs::write(&tests, include_str!("fastmode/wrapper.tests.cs")).unwrap();
            let output = Command::new(CSC).args([
                "-nologo", "-optimize", "-r:System.Web.Extensions.dll", "-main:WrapperTests",
                &format!("-out:{}", real.display()), &source.to_string_lossy(), &tests.to_string_lossy(),
            ]).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            let result = Command::new(real).output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            assert!(String::from_utf8_lossy(&result.stdout).contains("assertions passed"));
        }

        /// Desktop 2.19675 搬旧布局的现场：wrapper 被带进构建子目录，claude-real.exe 留在外层
        #[test]
        fn migrated_layout_gets_its_body_back() {
            let version = tempfile::tempdir().unwrap();
            let build = version.path().join("3f4bed3e44ad");
            fs::create_dir_all(&build).unwrap();
            fs::write(build.join(CLI_EXE), wrapper_source_hash()).unwrap();
            fs::write(version.path().join(REAL_EXE), official("284")).unwrap();
            assert_eq!(wrapper_state(&build), WrapperState::Broken);

            reclaim_stray_stash(version.path()).unwrap();
            assert_eq!(wrapper_state(&build), WrapperState::Deployed);
            assert!(!version.path().join(REAL_EXE).exists());
            assert_eq!(fs::read(build.join(REAL_EXE)).unwrap(), official("284"));
            // 归位后再跑一遍什么都不动
            reclaim_stray_stash(version.path()).unwrap();
            assert_eq!(wrapper_state(&build), WrapperState::Deployed);
        }

        /// 构建目录里已是 Desktop 新下的官方，外层那份只是残留
        #[test]
        fn stray_body_beside_a_fresh_build_is_dropped() {
            let version = tempfile::tempdir().unwrap();
            let build = version.path().join("635c1867224a");
            fs::create_dir_all(&build).unwrap();
            fs::write(build.join(CLI_EXE), official("286")).unwrap();
            fs::write(version.path().join(REAL_EXE), official("284")).unwrap();

            reclaim_stray_stash(version.path()).unwrap();
            assert!(!version.path().join(REAL_EXE).exists());
            assert_eq!(wrapper_state(&build), WrapperState::Absent);
        }

        /// 还在旧布局上的部署：外层那对 wrapper + 本体是正主，不能当残留删掉
        #[test]
        fn legacy_deployment_keeps_its_own_body() {
            let version = tempfile::tempdir().unwrap();
            fs::write(version.path().join(CLI_EXE), wrapper_source_hash()).unwrap();
            fs::write(version.path().join(REAL_EXE), official("284")).unwrap();
            let build = version.path().join("635c1867224a");
            fs::create_dir_all(&build).unwrap();
            fs::write(build.join(CLI_EXE), official("286")).unwrap();

            reclaim_stray_stash(version.path()).unwrap();
            assert_eq!(wrapper_state(version.path()), WrapperState::Deployed);
        }

        /// 还原官方覆盖每份构建：搬过家的那份先接回本体，再放回槽位
        #[test]
        fn restore_covers_every_build() {
            let version = tempfile::tempdir().unwrap();
            let migrated = version.path().join("3f4bed3e44ad");
            let deployed = version.path().join("635c1867224a");
            for dir in [&migrated, &deployed] {
                fs::create_dir_all(dir).unwrap();
                fs::write(dir.join(CLI_EXE), b"wrapper").unwrap();
            }
            fs::write(version.path().join(REAL_EXE), official("284")).unwrap();
            fs::write(deployed.join(REAL_EXE), official("286")).unwrap();

            assert!(restore_version_dir(version.path()).unwrap());
            assert_eq!(fs::read(migrated.join(CLI_EXE)).unwrap(), official("284"));
            assert_eq!(fs::read(deployed.join(CLI_EXE)).unwrap(), official("286"));
            for dir in [version.path(), migrated.as_path(), deployed.as_path()] {
                assert!(!dir.join(REAL_EXE).exists());
            }
            assert!(!restore_version_dir(version.path()).unwrap());
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
    const RENDERER_NOTE: &str = "原生开关在「功能增强」页安装";
    const WRAPPER_NAME: &str = "claude";
    const REAL_NAME: &str = "claude-real";
    const CLI_BUNDLE: &str = "claude.app";
    const REAL_BUNDLE: &str = "claude-real.app";
    /// 壳里指回官方本体：MacOS → Contents → claude.app → CLI 所在的那层目录
    const REAL_LINK: &str = "../../../claude-real.app/Contents/MacOS/claude";
    const FAST_ONLY: &str = r#"{"fastMode":true}"#;

    fn bundle_bin_dir(bundle: &Path) -> PathBuf {
        bundle.join("Contents").join("MacOS")
    }

    /// CLI 装在自己的 bundle 里，Desktop spawn 的是 bundle 内的可执行文件本体
    fn cli_bin_dir(dir: &Path) -> PathBuf {
        bundle_bin_dir(&dir.join(CLI_BUNDLE))
    }

    fn wrapper_slot(dir: &Path) -> (PathBuf, PathBuf) {
        let bin = cli_bin_dir(dir);
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

    fn real_bundle_bin(dir: &Path) -> PathBuf {
        bundle_bin_dir(&dir.join(REAL_BUNDLE)).join(WRAPPER_NAME)
    }

    /// 主程序位上放的是官方本体（普通文件），wrapper 那份是软链
    fn bundle_holds_official(bundle: &Path) -> bool {
        bundle_bin_dir(bundle)
            .join(WRAPPER_NAME)
            .symlink_metadata()
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    }

    /// 0.4.6 之前把官方本体抽出 bundle 改名 claude-real：一改名它就不再是 bundle 主程序，
    /// 配不上 Contents/Info.plist，签名当场失效，再加 hardened runtime，exec 即被内核 SIGKILL
    fn legacy_stash(dir: &Path) -> Option<PathBuf> {
        let stash = cli_bin_dir(dir).join(REAL_NAME);
        stash.symlink_metadata().ok().filter(|m| m.is_file()).map(|_| stash)
    }

    /// 把本体放回主程序位，签名随即恢复有效
    fn restore_legacy(dir: &Path) -> Result<bool> {
        let Some(stash) = legacy_stash(dir) else { return Ok(false) };
        let exe = cli_bin_dir(dir).join(WRAPPER_NAME);
        if exe.symlink_metadata().is_ok() {
            fs::remove_file(&exe).context("移除旧 wrapper")?;
        }
        fs::rename(&stash, &exe).context("把官方本体放回 bundle 主程序位")?;
        Ok(true)
    }

    fn wrapper_state(dir: &Path) -> WrapperState {
        if legacy_stash(dir).is_some() {
            return WrapperState::Broken;
        }
        let (exe, _) = wrapper_slot(dir);
        wrapper_state_from(slot_occupant(&exe), real_bundle_bin(dir).is_file())
    }

    pub(super) fn reclaim_stray_stash(version_dir: &Path) -> Result<()> {
        let stray = version_dir.join(REAL_BUNDLE);
        if !bundle_holds_official(&stray) {
            return Ok(());
        }
        let builds = build_dirs(version_dir);
        let shapes: Vec<(SlotOccupant, bool)> = builds
            .iter()
            .map(|dir| (slot_occupant(&wrapper_slot(dir).0), real_bundle_bin(dir).is_file()))
            .collect();
        match stray_stash_fate(slot_occupant(&wrapper_slot(version_dir).0), &shapes) {
            StrayStash::AdoptInto(index) => {
                let build = &builds[index];
                let stash = build.join(REAL_BUNDLE);
                if stash.exists() {
                    fs::remove_dir_all(&stash).context("清掉不完整的 claude-real.app")?;
                }
                fs::rename(&stray, &stash)
                    .with_context(|| format!("把 {REAL_BUNDLE} 挪回 {}", cli_dir_label(build)))?;
            }
            StrayStash::Discard => {
                fs::remove_dir_all(&stray)
                    .with_context(|| format!("清掉 {} 外层残留的 {REAL_BUNDLE}", dir_label(version_dir)))?;
            }
            StrayStash::Keep => {}
        }
        Ok(())
    }

    /// Info.plist 是 XML，取 CFBundleShortVersionString 不值得引 plist 依赖
    fn desktop_version() -> Option<String> {
        let text = fs::read_to_string(Path::new(DESKTOP_APP).join("Contents").join("Info.plist")).ok()?;
        let after_key = &text[text.find("<key>CFBundleShortVersionString</key>")?..];
        let start = after_key.find("<string>")? + "<string>".len();
        let end = after_key[start..].find("</string>")?;
        Some(after_key[start..start + end].trim().to_string())
    }

    /// 官方本体连整个 bundle 一起挪进 claude-real.app：签名认的是 Contents/MacOS/<主程序>
    /// 这层相对结构，bundle 目录叫什么它不在意，但把本体单独拎出来改名就立刻失配。
    /// 腾出的 claude.app 只留一个壳，Desktop 读前 8 字节验 Mach-O 会顺着软链读到 Claude++ 本体
    pub(super) fn deploy_wrapper(dir: &Path) -> Result<()> {
        restore_legacy(dir)?;
        let shell = dir.join(CLI_BUNDLE);
        let stash = dir.join(REAL_BUNDLE);

        if bundle_holds_official(&shell) {
            if stash.exists() {
                fs::remove_dir_all(&stash).context("清掉上一版官方 bundle")?;
            }
            fs::rename(&shell, &stash).with_context(|| format!("挪走官方 {CLI_BUNDLE}"))?;
        } else if !bundle_holds_official(&stash) {
            bail!("{} 里找不到官方 CLI 本体", cli_dir_label(dir));
        }

        let bin = cli_bin_dir(dir);
        fs::create_dir_all(&bin).context("建 wrapper 壳")?;
        let plist = stash.join("Contents").join("Info.plist");
        if plist.is_file() {
            fs::copy(&plist, shell.join("Contents").join("Info.plist")).context("复制 Info.plist")?;
        }

        let target = std::env::current_exe().context("定位 Claude++ 自身")?;
        let (exe, real) = wrapper_slot(dir);
        for link in [&exe, &real] {
            if link.symlink_metadata().is_ok() {
                fs::remove_file(link).with_context(|| format!("移除旧 {}", dir_label(link)))?;
            }
        }
        std::os::unix::fs::symlink(&target, &exe)
            .with_context(|| format!("软链 {WRAPPER_NAME} -> {}", target.display()))?;
        std::os::unix::fs::symlink(REAL_LINK, &real)
            .with_context(|| format!("软链 {REAL_NAME} -> {REAL_LINK}"))?;
        Ok(())
    }

    pub(super) fn remove_wrapper(dir: &Path) -> Result<bool> {
        let restored = restore_legacy(dir)?;
        let shell = dir.join(CLI_BUNDLE);
        let stash = dir.join(REAL_BUNDLE);
        if !bundle_holds_official(&stash) {
            return Ok(restored);
        }
        // Desktop 重下过官方，壳已被真本体盖回：留着 stash 只会多占一份
        if bundle_holds_official(&shell) {
            fs::remove_dir_all(&stash).context("清掉多余的 claude-real.app")?;
            return Ok(true);
        }
        if shell.exists() {
            fs::remove_dir_all(&shell).context("移除 wrapper 壳")?;
        }
        fs::rename(&stash, &shell).with_context(|| format!("还原官方 {CLI_BUNDLE}"))?;
        Ok(true)
    }

    fn snapshot(cli: Option<&CliInstall>) -> FastModeStatus {
        let version = desktop_version();
        let mut renderer = RendererInfo {
            native: true,
            file: None,
            state: RendererState::Unsupported,
            missing_anchors: Vec::new(),
        };
        if let Some(version) = version.as_deref().filter(|v| crate::computer_use::native::supports_extended(v)) {
            match crate::computer_use::fast_status::installed(Path::new(DESKTOP_APP), version) {
                Ok(true) => renderer.state = RendererState::Patched,
                Ok(false) => renderer.state = RendererState::Pristine,
                Err(error) => {
                    renderer.state = RendererState::Mismatch;
                    renderer.missing_anchors.push(error.to_string());
                }
            }
        } else if version.is_none() {
            renderer.state = RendererState::NotFound;
        }
        FastModeStatus {
            supported: true,
            desktop_version: version,
            msix_path: None,
            cli_version: cli.map(|c| c.version.clone()),
            cli_dir: cli.map(|c| c.exe_dir.clone()),
            wrapper: cli.map(|c| wrapper_state(&c.exe_dir)).unwrap_or(WrapperState::NoCli),
            renderer,
            settings: load_settings(),
            speed: SpeedStats::default(),
        }
    }

    fn status_light() -> FastModeStatus {
        snapshot(current_cli().as_ref())
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
        let wrapper = current_cli()
            .context("未找到 Desktop 装好的 bundled CLI（Claude-3p/claude-code）")
            .and_then(|cli| deploy_cli(&cli));
        let outcome = Outcome { wrapper: Some(wrapper), renderer: None };
        let mut settings = load_settings();
        record_outcome(&mut settings, &status_light(), &outcome);
        save_settings(&settings)?;
        let report = outcome.into_report()?;
        Ok(ActionReport { renderer: RENDERER_NOTE.to_string(), ..report })
    }

    pub fn uninstall() -> Result<ActionReport> {
        if procs::any_desktop_running() {
            bail!("Claude Desktop 正在运行，请先退出后再还原");
        }
        let restored = restore_all_cli();
        // 部分失败也停掉守护：否则它会把刚还原的 wrapper 又装回去
        let mut settings = load_settings();
        record_uninstall(&mut settings);
        save_settings(&settings)?;
        Ok(ActionReport {
            wrapper: format!("已还原 {} 个版本目录的官方 CLI", restored?),
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
        let cli = current_cli();
        let status = snapshot(cli.as_ref());
        if settings.failed_for.as_deref() == Some(version_key(&status).as_str()) {
            return RepairOutcome::Nothing;
        }
        let Some(cli) = cli.filter(|_| status.wrapper != WrapperState::Deployed) else {
            return RepairOutcome::Nothing;
        };
        if procs::any_desktop_running() {
            return RepairOutcome::Blocked;
        }
        let outcome = Outcome { wrapper: Some(deploy_cli(&cli)), renderer: None };
        record_outcome(&mut settings, &status_light(), &outcome);
        let _ = save_settings(&settings);
        outcome.into_repair()
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

    /// 保留 Desktop 自带的 disableAutoMode 等键；文件路径等非对象设置原样交给官方 CLI 解析
    fn merge_fast(settings: &str) -> String {
        if settings.trim().is_empty() {
            return FAST_ONLY.to_string();
        }
        match serde_json::from_str::<serde_json::Value>(settings) {
            Ok(serde_json::Value::Object(mut map)) if !map.contains_key("fastMode") => {
                map.insert("fastMode".into(), serde_json::Value::Bool(true));
                serde_json::Value::Object(map).to_string()
            }
            _ => settings.to_string(),
        }
    }

    fn merge_fast_arg(value: OsString) -> OsString {
        match value.to_str() {
            Some(settings) => merge_fast(settings).into(),
            None => value,
        }
    }

    fn merged_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        const FLAG: &str = "--settings";
        const INLINE: &[u8] = b"--settings=";
        let mut out = Vec::new();
        let mut merged = false;
        let mut rest = args.into_iter();
        while let Some(arg) = rest.next() {
            if arg == FLAG {
                out.push(arg);
                out.extend(rest.next().map(merge_fast_arg));
                merged = true;
                continue;
            }
            match arg.as_bytes().strip_prefix(INLINE) {
                Some(value) => {
                    let value = merge_fast_arg(std::ffi::OsStr::from_bytes(value).to_os_string());
                    out.push(OsString::from_vec([INLINE, value.as_bytes()].concat()));
                    merged = true;
                }
                None => out.push(arg),
            }
        }
        if !merged {
            out.push(FLAG.into());
            out.push(FAST_ONLY.into());
        }
        out
    }

    fn managed_fast_mode_restricted(settings: &serde_json::Value) -> bool {
        settings.get("fastMode").and_then(|v| v.as_bool()) == Some(false)
            || settings
                .get("fastModePerSessionOptIn")
                .and_then(|v| v.as_bool())
                == Some(true)
            || settings.get("processWrapper").is_some()
            || settings.get("env").is_some_and(|env| {
                env.get("CLAUDE_CODE_DISABLE_FAST_MODE").is_some()
                    || env.get("CLAUDE_CODE_PROCESS_WRAPPER").is_some()
            })
    }

    fn managed_fast_mode_file_restricted(path: &Path) -> bool {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map(|settings| !settings.is_object() || managed_fast_mode_restricted(&settings))
                .unwrap_or(true),
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        }
    }

    /// 与官方 CLI 同规则：只读 .json 结尾、不以点开头的文件或软链
    fn managed_fast_mode_dropins_restricted(dir: &Path) -> bool {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
        };
        entries.into_iter().any(|entry| {
            let Ok(entry) = entry else { return true };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.starts_with('.') {
                return false;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() || kind.is_symlink() => {
                    managed_fast_mode_file_restricted(&entry.path())
                }
                Ok(_) => false,
                Err(_) => true,
            }
        })
    }

    fn managed_root_restricted(root: &Path) -> bool {
        managed_fast_mode_file_restricted(&root.join("managed-settings.json"))
            || managed_fast_mode_dropins_restricted(&root.join("managed-settings.d"))
    }

    fn desktop_fast_mode_override(
        env: impl Fn(&str) -> Option<OsString>,
        managed_root: &Path,
        managed_preferences: &Path,
    ) -> bool {
        let is = |name, value| env(name).as_deref() == Some(std::ffi::OsStr::new(value));
        if !is("CLAUDE_CODE_ENTRYPOINT", "claude-desktop-3p")
            || !is("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1")
            || !is("CLAUDE_CODE_DISABLE_FAST_MODE", "1")
            || env("CLAUDE_CODE_PROCESS_WRAPPER").is_some()
            || env("CLAUDE_CODE_MANAGED_SETTINGS_PATH").is_some()
            || managed_root_restricted(managed_root)
        {
            return false;
        }
        let entries = match fs::read_dir(managed_preferences) {
            Ok(entries) => entries,
            Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = entry.path();
            if path
                .file_name()
                .is_some_and(|name| name == "com.anthropic.claudecode.plist")
            {
                return false;
            }
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    let policy = path.join("com.anthropic.claudecode.plist");
                    match policy.try_exists() {
                        Ok(false) => {}
                        _ => return false,
                    }
                }
                Ok(meta) if !meta.file_type().is_symlink() => {}
                _ => return false,
            }
        }
        true
    }

    fn fast_mode_api_key(current: Option<OsString>, token: Option<OsString>) -> Option<OsString> {
        if current.as_ref().is_some_and(|key| !key.is_empty()) {
            return None;
        }
        let token = token?;
        let value = token.to_str()?;
        (value.starts_with("sk-ant-api")
            && value.len() > 24
            && value.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
        .then_some(token)
    }

    fn wrapper_command(
        real: &Path,
        args: Vec<OsString>,
        clear_desktop_default: bool,
        api_key: Option<OsString>,
    ) -> Command {
        let mut command = Command::new(real);
        command.args(args);
        if clear_desktop_default {
            // Desktop 的 3p 默认禁用早于 fastMode 设置；组织策略仍由官方 CLI 校验。
            command.env_remove("CLAUDE_CODE_DISABLE_FAST_MODE");
            if let Some(key) = api_key {
                // 官方 Fast 资格预检不读取 Desktop 的 ANTHROPIC_AUTH_TOKEN。
                command.env("ANTHROPIC_API_KEY", key);
            }
        }
        command
    }

    pub fn run_wrapper(argv0: &Path) -> i32 {
        let real = argv0.with_file_name(REAL_NAME);
        let clear_desktop_default = desktop_fast_mode_override(
            |name| std::env::var_os(name),
            Path::new("/Library/Application Support/ClaudeCode"),
            Path::new("/Library/Managed Preferences"),
        );
        let api_key = fast_mode_api_key(
            std::env::var_os("ANTHROPIC_API_KEY"),
            std::env::var_os("ANTHROPIC_AUTH_TOKEN"),
        );
        let args = merged_args(std::env::args_os().skip(1));
        let error = wrapper_command(&real, args, clear_desktop_default, api_key).exec();
        eprintln!("[fast-wrapper] exec {} 失败: {error}", real.display());
        1
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn desktop_env(name: &str) -> Option<OsString> {
            match name {
                "CLAUDE_CODE_ENTRYPOINT" => Some("claude-desktop-3p".into()),
                "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST" | "CLAUDE_CODE_DISABLE_FAST_MODE" => {
                    Some("1".into())
                }
                _ => None,
            }
        }

        #[test]
        fn fast_override_only_accepts_the_desktop_default() {
            let dir = tempfile::tempdir().unwrap();
            let preferences = dir.path().join("preferences");
            assert!(desktop_fast_mode_override(
                desktop_env,
                dir.path(),
                &preferences
            ));
            for (name, value) in [
                ("CLAUDE_CODE_ENTRYPOINT", None),
                ("CLAUDE_CODE_ENTRYPOINT", Some("cli")),
                ("CLAUDE_CODE_ENTRYPOINT", Some("claude-desktop")),
                ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", None),
                ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", Some("0")),
                ("CLAUDE_CODE_DISABLE_FAST_MODE", None),
                ("CLAUDE_CODE_DISABLE_FAST_MODE", Some("0")),
                ("CLAUDE_CODE_DISABLE_FAST_MODE", Some("true")),
                ("CLAUDE_CODE_PROCESS_WRAPPER", Some("/org/launcher")),
                ("CLAUDE_CODE_MANAGED_SETTINGS_PATH", Some("/org/settings")),
            ] {
                let env = |key: &str| {
                    if key == name {
                        value.map(OsString::from)
                    } else {
                        desktop_env(key)
                    }
                };
                assert!(
                    !desktop_fast_mode_override(env, dir.path(), &preferences),
                    "{name}"
                );
            }
        }

        #[test]
        fn fast_override_preserves_managed_restrictions_and_launchers() {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join("managed-settings.json");
            let preferences = dir.path().join("preferences");
            for source in [
                r#"{"fastMode":false}"#,
                r#"{"fastModePerSessionOptIn":true}"#,
                r#"{"processWrapper":"/org/launcher"}"#,
                r#"{"env":{"CLAUDE_CODE_PROCESS_WRAPPER":"/org/launcher"}}"#,
                r#"{"env":{"CLAUDE_CODE_DISABLE_FAST_MODE":"1"}}"#,
                "not json",
                "[]",
                "null",
            ] {
                fs::write(&settings, source).unwrap();
                assert!(
                    !desktop_fast_mode_override(desktop_env, dir.path(), &preferences),
                    "{source}"
                );
            }
            fs::write(&settings, r#"{"sandbox":{"enabled":true}}"#).unwrap();
            assert!(desktop_fast_mode_override(
                desktop_env,
                dir.path(),
                &preferences
            ));
        }

        #[test]
        fn fast_override_reads_managed_dropins_like_the_cli() {
            let dir = tempfile::tempdir().unwrap();
            let preferences = dir.path().join("preferences");
            let dropins = dir.path().join("managed-settings.d");
            fs::create_dir_all(dropins.join("nested.json")).unwrap();
            fs::write(dropins.join(".disabled.json"), r#"{"fastMode":false}"#).unwrap();
            fs::write(dropins.join("notes.txt"), "not json").unwrap();
            fs::write(dropins.join("10-sandbox.json"), r#"{"sandbox":{"enabled":true}}"#).unwrap();
            std::os::unix::fs::symlink(dir.path().join("missing.json"), dropins.join("20-gone.json"))
                .unwrap();
            assert!(desktop_fast_mode_override(desktop_env, dir.path(), &preferences));
            for source in [
                r#"{"fastMode":false}"#,
                r#"{"processWrapper":"/org/launcher"}"#,
                r#"{"env":{"CLAUDE_CODE_DISABLE_FAST_MODE":"1"}}"#,
                "not json",
            ] {
                fs::write(dropins.join("30-policy.json"), source).unwrap();
                assert!(
                    !desktop_fast_mode_override(desktop_env, dir.path(), &preferences),
                    "{source}"
                );
            }
            fs::remove_dir_all(&dropins).unwrap();
            fs::write(&dropins, "not a directory").unwrap();
            assert!(!desktop_fast_mode_override(desktop_env, dir.path(), &preferences));
        }

        #[test]
        fn fast_override_preserves_machine_and_user_mdm_settings() {
            let dir = tempfile::tempdir().unwrap();
            let preferences = dir.path().join("preferences");
            fs::create_dir_all(preferences.join("user")).unwrap();
            assert!(desktop_fast_mode_override(
                desktop_env,
                dir.path(),
                &preferences
            ));
            for parent in [&preferences, &preferences.join("user")] {
                let policy = parent.join("com.anthropic.claudecode.plist");
                fs::write(&policy, "managed").unwrap();
                assert!(!desktop_fast_mode_override(
                    desktop_env,
                    dir.path(),
                    &preferences
                ));
                fs::remove_file(policy).unwrap();
            }
            fs::remove_dir_all(&preferences).unwrap();
            fs::write(&preferences, "not a directory").unwrap();
            assert!(!desktop_fast_mode_override(
                desktop_env,
                dir.path(),
                &preferences
            ));
        }

        #[test]
        fn wrapper_command_only_removes_the_desktop_fast_default() {
            let args = vec!["--settings".into(), FAST_ONLY.into()];
            for clear in [false, true] {
                let command = wrapper_command(Path::new("/cli/claude-real"), args.clone(), clear, None);
                assert_eq!(command.get_args().collect::<Vec<_>>(), args);
                let changes = command.get_envs().collect::<Vec<_>>();
                if clear {
                    assert_eq!(
                        changes,
                        vec![(std::ffi::OsStr::new("CLAUDE_CODE_DISABLE_FAST_MODE"), None)]
                    );
                } else {
                    assert!(changes.is_empty());
                }
            }
        }

        #[test]
        fn fast_api_key_alias_only_accepts_official_static_keys() {
            let key = OsString::from("sk-ant-api03-test-only-not-a-real-key");
            assert_eq!(fast_mode_api_key(None, Some(key.clone())), Some(key.clone()));
            assert_eq!(fast_mode_api_key(Some("".into()), Some(key.clone())), Some(key.clone()));
            assert_eq!(fast_mode_api_key(Some("existing-key".into()), Some(key)), None);
            assert_eq!(fast_mode_api_key(None, None), None);
            for token in [
                "gateway-token",
                "sk-ant-oat01-test-only-not-a-real-token",
                "sk-ant-sid01-test-only-not-a-real-session",
                "sk-ant-api03-short",
                "sk-ant-api03-test-only-with whitespace",
                "sk-ant-api03-test-only-with\nnewline",
            ] {
                assert_eq!(fast_mode_api_key(None, Some(token.into())), None, "{token}");
            }
        }

        #[test]
        fn api_key_alias_is_scoped_to_the_desktop_override() {
            let key = OsString::from("sk-ant-api03-test-only-not-a-real-key");
            for clear in [false, true] {
                let command = wrapper_command(Path::new("/cli/claude-real"), vec![], clear, Some(key.clone()));
                let changes: std::collections::BTreeMap<_, _> = command.get_envs().collect();
                if clear {
                    assert_eq!(changes.get(std::ffi::OsStr::new("ANTHROPIC_API_KEY")), Some(&Some(key.as_os_str())));
                } else {
                    assert!(changes.is_empty());
                }
                assert!(!changes.contains_key(std::ffi::OsStr::new("ANTHROPIC_AUTH_TOKEN")));
                assert!(!changes.contains_key(std::ffi::OsStr::new("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK")));
            }
        }

        /// 官方本体连 bundle 一起挪走，主程序位上的文件名和相对层级都保持原样
        #[test]
        fn wrapper_shell_stands_in_for_the_bundle() {
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
            // 本体仍叫 claude 且仍在 Contents/MacOS 下，签名才不会失配
            assert_eq!(fs::read(real_bundle_bin(dir.path())).unwrap(), b"official binary");
            assert!(real.symlink_metadata().unwrap().is_symlink());
            assert_eq!(fs::read(&real).unwrap(), b"official binary");

            // 重复部署幂等：不会把壳自己当官方挪走
            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            assert_eq!(fs::read(real_bundle_bin(dir.path())).unwrap(), b"official binary");

            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(&exe).unwrap(), b"official binary");
            assert!(!dir.path().join(REAL_BUNDLE).exists());
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
            assert_eq!(fs::read(real_bundle_bin(dir.path())).unwrap(), b"official v2 redownloaded");
        }

        /// Claude++ 被移走会让软链断掉，还原不能被 exists() 的假 false 卡住
        #[test]
        fn broken_symlink_still_restores() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            let exe = bin.join(WRAPPER_NAME);
            fs::write(&exe, b"official").unwrap();
            deploy_wrapper(dir.path()).unwrap();

            fs::remove_file(&exe).unwrap();
            std::os::unix::fs::symlink(dir.path().join("moved-away"), &exe).unwrap();
            assert!(!exe.exists());
            assert!(exe.symlink_metadata().is_ok());
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);

            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(&exe).unwrap(), b"official");
        }

        /// 0.4.6 之前装出来的布局：本体被抽出 bundle 改名，一 exec 就 SIGKILL。
        /// 必须判成待修，让自动守护接手就地迁移
        #[test]
        fn legacy_layout_is_detected_and_migrated() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join(REAL_NAME), b"official binary").unwrap();
            std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bin.join(WRAPPER_NAME))
                .unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Broken);

            deploy_wrapper(dir.path()).unwrap();
            assert_eq!(wrapper_state(dir.path()), WrapperState::Deployed);
            assert_eq!(fs::read(real_bundle_bin(dir.path())).unwrap(), b"official binary");
            // 裸文件换成了软链，本体回到主程序位
            assert!(bin.join(REAL_NAME).symlink_metadata().unwrap().is_symlink());
        }

        /// 旧布局上直接卸载：本体放回主程序位就恢复可执行，不用先修再卸
        #[test]
        fn legacy_layout_uninstalls_clean() {
            let dir = tempfile::tempdir().unwrap();
            let bin = cli_bin_dir(dir.path());
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join(REAL_NAME), b"official binary").unwrap();
            std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bin.join(WRAPPER_NAME))
                .unwrap();

            assert!(remove_wrapper(dir.path()).unwrap());
            assert_eq!(fs::read(bin.join(WRAPPER_NAME)).unwrap(), b"official binary");
            assert!(!bin.join(REAL_NAME).exists());
        }

        #[test]
        fn merge_fast_keeps_desktop_keys() {
            let merged = merge_fast(r#"{"disableAutoMode":"disable"}"#);
            let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
            assert_eq!(v["fastMode"], serde_json::json!(true));
            assert_eq!(v["disableAutoMode"], serde_json::json!("disable"));
            let explicit = r#"{"fastMode":false,"x":1}"#;
            assert_eq!(merge_fast(explicit), explicit);
            assert_eq!(merge_fast("{}"), FAST_ONLY);
            assert_eq!(merge_fast(" "), FAST_ONLY);
            // --settings 也接受文件路径，非对象内容交给官方 CLI 解析
            for settings in ["not json", "/Users/me/settings file.json", "[1,2]", "null"] {
                assert_eq!(merge_fast(settings), settings);
            }
        }

        #[test]
        fn merged_args_merge_every_settings_form_once() {
            use std::os::unix::ffi::OsStringExt;
            let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
            let json = |value: &OsString, prefix: &str| -> serde_json::Value {
                serde_json::from_str(value.to_str().unwrap().strip_prefix(prefix).unwrap()).unwrap()
            };
            let expected = serde_json::json!({"x": 1, "fastMode": true});

            assert_eq!(merged_args(args(&["-p"])), args(&["-p", "--settings", FAST_ONLY]));
            let spaced = merged_args(args(&["--settings", r#"{"x":1}"#, "-p"]));
            assert_eq!(spaced.len(), 3);
            assert_eq!(spaced[0], "--settings");
            assert_eq!(spaced[2], "-p");
            assert_eq!(json(&spaced[1], ""), expected);
            let inline = merged_args(args(&[r#"--settings={"x":1}"#]));
            assert_eq!(inline.len(), 1);
            assert_eq!(json(&inline[0], "--settings="), expected);

            for kept in [
                args(&["--settings"]),
                args(&["--settings", "/tmp/settings.json"]),
                args(&["--settings=/tmp/settings.json"]),
                args(&["--settings", r#"{"fastMode":false}"#]),
            ] {
                assert_eq!(merged_args(kept.clone()), kept);
            }
            let raw = vec![OsString::from("--settings"), OsString::from_vec(b"/tmp/\xff.json".to_vec())];
            assert_eq!(merged_args(raw.clone()), raw);
            let raw_inline = vec![OsString::from_vec(b"--settings=/tmp/\xff.json".to_vec())];
            assert_eq!(merged_args(raw_inline.clone()), raw_inline);
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

        /// 旧布局上装好 wrapper，再照 Desktop 2.19675 的做法只把 claude.app 搬进构建子目录
        fn migrated_install(version: &Path) -> PathBuf {
            let bin = cli_bin_dir(version);
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join(WRAPPER_NAME), b"official binary").unwrap();
            deploy_wrapper(version).unwrap();
            let build = version.join("3f4bed3e44ad");
            fs::create_dir_all(&build).unwrap();
            fs::rename(version.join(CLI_BUNDLE), build.join(CLI_BUNDLE)).unwrap();
            build
        }

        #[test]
        fn migrated_shell_gets_its_bundle_back() {
            let version = tempfile::tempdir().unwrap();
            let build = migrated_install(version.path());
            // claude-real.app 留在外层，壳里的相对软链落空
            assert_eq!(wrapper_state(&build), WrapperState::Broken);
            assert!(fs::read(wrapper_slot(&build).1).is_err());

            reclaim_stray_stash(version.path()).unwrap();
            assert_eq!(wrapper_state(&build), WrapperState::Deployed);
            assert!(!version.path().join(REAL_BUNDLE).exists());
            assert_eq!(fs::read(wrapper_slot(&build).1).unwrap(), b"official binary");
        }

        #[test]
        fn restore_puts_the_migrated_bundle_back() {
            let version = tempfile::tempdir().unwrap();
            let build = migrated_install(version.path());

            assert!(restore_version_dir(version.path()).unwrap());
            let (exe, _) = wrapper_slot(&build);
            assert!(exe.symlink_metadata().unwrap().is_file());
            assert_eq!(fs::read(&exe).unwrap(), b"official binary");
            assert!(!version.path().join(REAL_BUNDLE).exists());
            assert!(!build.join(REAL_BUNDLE).exists());
        }

        /// 构建目录里是 Desktop 新下的官方，外层 claude-real.app 只是残留
        #[test]
        fn stray_bundle_beside_a_fresh_build_is_dropped() {
            let version = tempfile::tempdir().unwrap();
            let build = version.path().join("635c1867224a");
            let bin = cli_bin_dir(&build);
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join(WRAPPER_NAME), b"fresh official").unwrap();
            let stray_bin = bundle_bin_dir(&version.path().join(REAL_BUNDLE));
            fs::create_dir_all(&stray_bin).unwrap();
            fs::write(stray_bin.join(WRAPPER_NAME), b"stale official").unwrap();

            reclaim_stray_stash(version.path()).unwrap();
            assert!(!version.path().join(REAL_BUNDLE).exists());
            assert_eq!(wrapper_state(&build), WrapperState::Absent);
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
                native: false,
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
            renderer: RendererInfo { native: false, file: None, state: renderer, missing_anchors: Vec::new() },
            settings: FastModeSettings::default(),
            speed: SpeedStats::default(),
        }
    }

    #[test]
    fn native_display_patch_does_not_enable_the_wrapper_guardian() {
        let mut status = status_with(WrapperState::Absent, RendererState::Patched);
        status.renderer.native = true;
        let mut settings = FastModeSettings::default();
        assert!(!adopt_existing(&status, &mut settings));
        assert!(!settings.installed);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("2.1.260"), Some(vec![2, 1, 260]));
        assert_eq!(parse_version("2.1.260-beta"), None);
        assert_eq!(parse_version(""), None);
    }

    fn mark_verified(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(VERIFIED_MARKER), "0".repeat(64)).unwrap();
    }

    fn age(file: &Path, by: Duration) {
        let past = SystemTime::now() - by;
        fs::File::options().write(true).open(file).unwrap().set_modified(past).unwrap();
    }

    #[test]
    fn latest_cli_picks_newest_installed_version() {
        let root = tempfile::tempdir().unwrap();
        for name in ["2.1.258", "2.1.9", "junk", "2.1.260-beta"] {
            mark_verified(&root.path().join(name));
        }
        // 按数字比：2.1.258 在 2.1.9 之上
        assert_eq!(latest_cli(root.path()).unwrap().version, "2.1.258");

        let build = root.path().join("2.1.286").join("635c1867224a");
        mark_verified(&build);
        let cli = latest_cli(root.path()).unwrap();
        assert_eq!(cli.version, "2.1.286");
        assert_eq!(cli.version_dir, root.path().join("2.1.286"));
        assert_eq!(cli.exe_dir, build);

        // 还在下载的更高版本没有 .verified，不能被当成 CLI
        let downloading = root.path().join("2.1.290");
        fs::create_dir_all(downloading.join("0123456789ab")).unwrap();
        fs::write(downloading.join("download.0123456789ab.zst.partial"), b"").unwrap();
        assert_eq!(latest_cli(root.path()).unwrap().version, "2.1.286");
    }

    #[test]
    fn exe_dir_follows_the_build_desktop_would_spawn() {
        let version = tempfile::tempdir().unwrap();
        fs::write(version.path().join(VERIFIED_MARKER), "0").unwrap();
        // 旧布局：CLI 直接在版本目录里
        assert_eq!(exe_dir_of(version.path()).unwrap(), version.path());

        let newer = version.path().join("3f4bed3e44ad");
        let older = version.path().join("635c1867224a");
        mark_verified(&newer);
        mark_verified(&older);
        age(&older.join(VERIFIED_MARKER), Duration::from_secs(3600));
        // 名字不合格式、没有 .verified 的子目录都不是已发布的构建
        mark_verified(&version.path().join("ABCDEF123456"));
        mark_verified(&version.path().join("635c1867224"));
        fs::create_dir_all(version.path().join("0123456789ab")).unwrap();
        // 按 .verified 新旧选，不按目录名
        assert_eq!(exe_dir_of(version.path()).unwrap(), newer);

        let unfinished = tempfile::tempdir().unwrap();
        fs::create_dir_all(unfinished.path().join("635c1867224a")).unwrap();
        assert_eq!(exe_dir_of(unfinished.path()), None);
    }

    #[test]
    fn cli_dir_label_names_the_version_of_a_build() {
        assert_eq!(cli_dir_label(Path::new("/x/claude-code/2.1.286/635c1867224a")), "2.1.286/635c1867224a");
        assert_eq!(cli_dir_label(Path::new("/x/claude-code/2.1.284")), "2.1.284");
    }

    #[test]
    fn wrapper_state_covers_every_slot_shape() {
        use SlotOccupant::*;
        assert_eq!(wrapper_state_from(Wrapper, true), WrapperState::Deployed);
        // 官方本体与 real 并存：Desktop 重下覆盖了 wrapper
        assert_eq!(wrapper_state_from(Official, true), WrapperState::Broken);
        assert_eq!(wrapper_state_from(Official, false), WrapperState::Absent);
        // Desktop 搬布局带走了 wrapper、本体留在外层：CLI 起不来，不能报官方原样
        assert_eq!(wrapper_state_from(Wrapper, false), WrapperState::Broken);
        assert_eq!(wrapper_state_from(Empty, false), WrapperState::NoCli);
        assert_eq!(wrapper_state_from(Empty, true), WrapperState::Broken);
    }

    #[test]
    fn stray_stash_goes_to_the_wrapper_missing_it() {
        use SlotOccupant::*;
        // 版本目录自己的槽位有东西：仍是旧布局，外层本体是正主
        assert_eq!(stray_stash_fate(Wrapper, &[(Wrapper, false)]), StrayStash::Keep);
        // 缺本体的 wrapper 优先，排在已有官方的构建后面也一样
        assert_eq!(stray_stash_fate(Empty, &[(Official, false), (Wrapper, false)]), StrayStash::AdoptInto(1));
        // 没有谁缺，而某份构建已有官方（在槽位上或藏着）：外层那份是残留
        assert_eq!(stray_stash_fate(Empty, &[(Official, false)]), StrayStash::Discard);
        assert_eq!(stray_stash_fate(Empty, &[(Wrapper, true)]), StrayStash::Discard);
        // 看不出归属就不动
        assert_eq!(stray_stash_fate(Empty, &[(Empty, false)]), StrayStash::Keep);
        assert_eq!(stray_stash_fate(Empty, &[]), StrayStash::Keep);
    }

    #[test]
    #[cfg(windows)]
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
    #[cfg(windows)]
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
        // wrapper 在位即认领，只记生效的那一半
        assert!(adopt_existing(&status_with(WrapperState::Deployed, RendererState::Pristine), &mut settings));
        assert!(settings.installed);
        assert_eq!(settings.patched_cli_version.as_deref(), Some("2.1.260"));
        assert_eq!(settings.patched_desktop_version, None);
        // 已记账的不再重复认领
        assert!(!adopt_existing(&status_with(WrapperState::Deployed, RendererState::Patched), &mut settings));
        // 只剩 renderer patch 也算装过
        let mut fresh = FastModeSettings::default();
        assert!(adopt_existing(&status_with(WrapperState::Absent, RendererState::Patched), &mut fresh));
        assert_eq!(fresh.patched_desktop_version.as_deref(), Some("1.46388.4.0"));
        assert_eq!(fresh.patched_cli_version, None);

        // 旧版补丁也认领让守护补齐，但它还没完全生效，不记版本
        let mut outdated = FastModeSettings::default();
        assert!(adopt_existing(&status_with(WrapperState::Absent, RendererState::Outdated), &mut outdated));
        assert_eq!(outdated.patched_desktop_version, None);
        // 无 renderer 的部署只认领已在位的 wrapper
        let mut mac = FastModeSettings::default();
        assert!(!adopt_existing(&status_with(WrapperState::Absent, RendererState::Unsupported), &mut mac));
        assert!(adopt_existing(&status_with(WrapperState::Deployed, RendererState::Unsupported), &mut mac));
        assert_eq!(mac.patched_desktop_version, None);
    }

    fn ok(message: &str) -> Option<Result<String>> {
        Some(Ok(message.to_string()))
    }

    fn err(message: &str) -> Option<Result<String>> {
        Some(Err(anyhow::anyhow!(message.to_string())))
    }

    #[test]
    fn partial_outcome_keeps_the_half_that_worked() {
        let mut settings = FastModeSettings::default();
        let partial = Outcome { wrapper: err("缺少 claude.exe"), renderer: ok("renderer 已 patch") };
        record_outcome(&mut settings, &status_with(WrapperState::Absent, RendererState::Patched), &partial);
        assert!(settings.installed);
        assert_eq!(settings.patched_desktop_version.as_deref(), Some("1.46388.4.0"));
        assert_eq!(settings.patched_cli_version, None);
        assert_eq!(settings.last_failure.as_deref(), Some("wrapper：缺少 claude.exe"));
        assert_eq!(settings.failed_for, Some(version_key(&status_with(WrapperState::Absent, RendererState::Patched))));
        assert_eq!(settings.last_repair, None);
        // 报错时成败一起列，免得以为 renderer 也没做
        assert_eq!(partial.describe(), "wrapper：缺少 claude.exe；renderer 已 patch");
        assert!(partial.into_report().is_err());

        let fixed = Outcome { wrapper: ok("wrapper 已部署到 2.1.260"), renderer: None };
        record_outcome(&mut settings, &status_with(WrapperState::Deployed, RendererState::Patched), &fixed);
        assert_eq!(settings.patched_cli_version.as_deref(), Some("2.1.260"));
        assert_eq!(settings.last_failure, None);
        assert_eq!(settings.failed_for, None);
        assert!(settings.last_repair.is_some());
        assert!(matches!(fixed.into_repair(), RepairOutcome::Repaired(summary) if summary == "wrapper 已部署到 2.1.260"));
    }

    #[test]
    fn failed_first_install_is_not_adopted() {
        let mut settings = FastModeSettings::default();
        let nothing = Outcome { wrapper: err("csc 不存在"), renderer: ok("renderer 锚点未命中，跳过") };
        record_outcome(&mut settings, &status_with(WrapperState::Absent, RendererState::Mismatch), &nothing);
        assert!(!settings.installed);
        assert!(settings.last_failure.is_some());
    }

    #[test]
    fn full_outcome_reports_both_halves() {
        let both = Outcome { wrapper: ok("wrapper 已部署到 2.1.260"), renderer: ok("renderer 已 patch") };
        let report = both.into_report().unwrap();
        assert_eq!(report.wrapper, "wrapper 已部署到 2.1.260");
        assert_eq!(report.renderer, "renderer 已 patch");
    }

    #[test]
    fn uninstall_clears_the_whole_ledger() {
        let mut settings = FastModeSettings {
            installed: true,
            patched_desktop_version: Some("2.19675.0.0".into()),
            patched_cli_version: Some("2.1.286".into()),
            last_failure: Some("wrapper：x".into()),
            failed_for: Some("2.19675.0.0|2.1.286|0.4.10".into()),
            ..FastModeSettings::default()
        };
        record_uninstall(&mut settings);
        assert!(!settings.installed);
        assert!(settings.auto);
        assert_eq!(settings.patched_desktop_version, None);
        assert_eq!(settings.patched_cli_version, None);
        assert_eq!(settings.last_failure, None);
        assert_eq!(settings.failed_for, None);
    }

    /// 旧版记下的失败组合对新版无效：升级 Claude++ 后守护会再试一次
    #[test]
    fn version_key_carries_own_version() {
        let key = version_key(&status_with(WrapperState::Absent, RendererState::Pristine));
        assert_eq!(key, format!("1.46388.4.0|2.1.260|{}", env!("CARGO_PKG_VERSION")));
        assert_ne!(key, "1.46388.4.0|2.1.260");
    }
}

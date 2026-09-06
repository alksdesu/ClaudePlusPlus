pub mod codex;
pub mod discovery;
pub mod fastmode;
pub mod link;
pub mod migrate;
pub mod preview;
pub mod procs;
pub mod unify;
pub mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use codex::{CodexSession, MigrateOutcome};
use discovery::{DiscoveryReport, PoolKind};
use fastmode::{ActionReport, FastModeSettings, FastModeStatus};
use migrate::{
    ConflictPolicy, DeleteOutcome, DesktopSession, PurgeOutcome, RegisterOutcome, TombstoneInfo,
    UnregisterOutcome,
};
use unify::{UnifyPlan, UnifyReport};
use watch::{WatchState, WatchStatus};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningDesktop {
    pub root_label: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterReport {
    pub session_id: String,
    pub outcome: Option<RegisterOutcome>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnregisterReport {
    pub metadata_file: String,
    pub outcome: Option<UnregisterOutcome>,
    pub error: Option<String>,
}

// 同步 command 默认跑在 UI 线程；标 async 让它们进线程池，扫描、子进程、大文件读都不再冻结窗口
#[tauri::command(async)]
fn scan() -> DiscoveryReport {
    discovery::scan_all()
}

#[tauri::command(async)]
fn desktop_running() -> Vec<RunningDesktop> {
    let lines = procs::desktop_command_lines();
    discovery::default_roots()
        .into_iter()
        .map(|root| RunningDesktop {
            running: procs::desktop_running_for(&root.path, &lines),
            root_label: root.label,
        })
        .collect()
}

#[tauri::command(async)]
fn plan_unify_all() -> Vec<UnifyPlan> {
    let report = discovery::scan_combos();
    [PoolKind::Code, PoolKind::Agent]
        .into_iter()
        .filter_map(|pool| unify::plan_unify(&report.combos, pool))
        .collect()
}

fn ensure_desktop_stopped() -> Result<(), String> {
    if procs::any_desktop_running() {
        return Err("Claude Desktop 正在运行，请先退出后再执行写操作".into());
    }
    Ok(())
}

#[tauri::command(async)]
fn apply_unify_all() -> Result<Vec<UnifyReport>, String> {
    ensure_desktop_stopped()?;
    let report = discovery::scan_combos();
    let mut results = Vec::new();
    for pool in [PoolKind::Code, PoolKind::Agent] {
        if let Some(plan) = unify::plan_unify(&report.combos, pool) {
            if plan.to_merge.is_empty() {
                continue;
            }
            results.push(unify::apply_unify(&plan).map_err(|e| e.to_string())?);
        }
    }
    Ok(results)
}

#[tauri::command(async)]
fn restore_combo(path: String, org_id: String) -> Result<(), String> {
    ensure_desktop_stopped()?;
    unify::restore_combo(&PathBuf::from(path), &org_id).map_err(|e| e.to_string())
}

fn canonical_code_dir() -> Result<PathBuf, String> {
    let report = discovery::scan_combos();
    migrate::canonical_code_dir_from(&report.combos).ok_or_else(|| "未发现任何 code 会话池".into())
}

#[tauri::command(async)]
fn list_desktop_sessions() -> Result<Vec<DesktopSession>, String> {
    Ok(migrate::list_desktop_sessions(&canonical_code_dir()?))
}

#[tauri::command(async)]
fn register_sessions(session_ids: Vec<String>, policy: String) -> Result<Vec<RegisterReport>, String> {
    ensure_desktop_stopped()?;
    let policy = match policy.as_str() {
        "overwrite" => ConflictPolicy::Overwrite,
        "revive" => ConflictPolicy::Revive,
        _ => ConflictPolicy::Skip,
    };
    let code_dir = canonical_code_dir()?;
    let report = discovery::scan_all();
    let mut results = Vec::new();
    // 同批次内已注册的组：挡住一次提交里勾了同组多个分支的情况
    let mut done_groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in session_ids {
        let Some(cli) = report.cli_sessions.iter().find(|s| s.session_id == id) else {
            results.push(RegisterReport {
                session_id: id,
                outcome: None,
                error: Some("CLI 会话不存在".into()),
            });
            continue;
        };
        let siblings: Vec<String> = report
            .cli_sessions
            .iter()
            .filter(|s| s.group_id == cli.group_id && s.session_id != cli.session_id)
            .map(|s| s.session_id.clone())
            .collect();
        if policy != ConflictPolicy::Overwrite && !done_groups.insert(cli.group_id.clone()) {
            results.push(RegisterReport {
                session_id: id,
                outcome: None,
                error: Some("同一会话的另一分支已在本次注册中处理".into()),
            });
            continue;
        }
        match migrate::register_cli_session_with_siblings(&code_dir, cli, policy, &siblings) {
            Ok(outcome) => results.push(RegisterReport {
                session_id: id,
                outcome: Some(outcome),
                error: None,
            }),
            Err(error) => results.push(RegisterReport {
                session_id: id,
                outcome: None,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(results)
}

#[tauri::command(async)]
fn unregister_sessions(metadata_files: Vec<String>, hard_delete: bool) -> Result<Vec<UnregisterReport>, String> {
    ensure_desktop_stopped()?;
    let code_dir = canonical_code_dir()?;
    let mut results = Vec::new();
    for file in metadata_files {
        match migrate::unregister_desktop_session(&code_dir, &file, hard_delete) {
            Ok(outcome) => results.push(UnregisterReport {
                metadata_file: file,
                outcome: Some(outcome),
                error: None,
            }),
            Err(error) => results.push(UnregisterReport {
                metadata_file: file,
                outcome: None,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(results)
}

fn cli_projects_dir() -> Result<PathBuf, String> {
    discovery::cli_projects_dir().ok_or_else(|| "找不到 ~/.claude/projects".into())
}

#[tauri::command(async)]
fn list_tombstones() -> Result<Vec<TombstoneInfo>, String> {
    Ok(migrate::list_tombstones(
        &canonical_code_dir()?,
        &cli_projects_dir()?,
    ))
}

#[tauri::command(async)]
fn purge_tombstones(
    file_names: Vec<String>,
    delete_transcripts: bool,
) -> Result<Vec<PurgeOutcome>, String> {
    ensure_desktop_stopped()?;
    Ok(migrate::purge_tombstones(
        &canonical_code_dir()?,
        &cli_projects_dir()?,
        &file_names,
        delete_transcripts,
        true,
    ))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport {
    pub target: String,
    pub outcome: Option<DeleteOutcome>,
    pub error: Option<String>,
}

/// 彻底删除会话：Desktop 元数据 + 墓碑 + CLI 转录（转录走回收站）。
/// 与 unregister 的分工——注销只退回 CLI 侧，删除是两边一起消失。
#[tauri::command(async)]
fn delete_sessions(
    cli_session_ids: Vec<String>,
    metadata_files: Vec<String>,
) -> Result<Vec<DeleteReport>, String> {
    ensure_desktop_stopped()?;
    let code_dir = canonical_code_dir()?;
    let projects_dir = cli_projects_dir()?;
    let mut results = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Desktop 侧先做：它能解出 cliSessionId，两侧勾到同一会话时不会删第二遍
    for file in metadata_files {
        match migrate::delete_desktop_session(&code_dir, &projects_dir, &file, true) {
            Ok(outcome) => {
                if let Some(id) = outcome.cli_session_id.clone() {
                    done.insert(id);
                }
                results.push(DeleteReport {
                    target: file,
                    outcome: Some(outcome),
                    error: None,
                });
            }
            Err(error) => results.push(DeleteReport {
                target: file,
                outcome: None,
                error: Some(error.to_string()),
            }),
        }
    }
    for id in cli_session_ids {
        if !done.insert(id.clone()) {
            continue;
        }
        match migrate::delete_cli_session(&code_dir, &projects_dir, &id, true) {
            Ok(outcome) => results.push(DeleteReport {
                target: id,
                outcome: Some(outcome),
                error: None,
            }),
            Err(error) => results.push(DeleteReport {
                target: id,
                outcome: None,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(results)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexMigrateReport {
    pub thread_id: String,
    pub outcome: Option<MigrateOutcome>,
    pub error: Option<String>,
}

#[tauri::command(async)]
fn list_codex_sessions() -> Vec<CodexSession> {
    codex::list_codex_sessions()
}

#[tauri::command(async)]
fn codex_running() -> bool {
    procs::codex_running()
}

#[tauri::command(async)]
fn migrate_codex_sessions(thread_ids: Vec<String>) -> Result<Vec<CodexMigrateReport>, String> {
    // 迁移会写 Codex 的导入记录（防循环），Codex 运行中可能覆盖它
    if procs::codex_running() {
        return Err("Codex 正在运行，请先退出后再迁移".into());
    }
    let codex_home = codex::codex_home().ok_or("找不到 ~/.codex")?;
    let projects = cli_projects_dir()?;
    let sessions = codex::list_codex_sessions();
    let mut results = Vec::new();
    for id in thread_ids {
        let Some(session) = sessions.iter().find(|s| s.thread_id == id) else {
            results.push(CodexMigrateReport {
                thread_id: id,
                outcome: None,
                error: Some("Codex 会话不存在".into()),
            });
            continue;
        };
        match codex::migrate_session(&codex_home, &projects, &session.rollout_path) {
            Ok(outcome) => results.push(CodexMigrateReport {
                thread_id: id,
                outcome: Some(outcome),
                error: None,
            }),
            Err(error) => results.push(CodexMigrateReport {
                thread_id: id,
                outcome: None,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(results)
}

/// 打开（或聚焦）某会话的预览窗口；id 进入窗口 label，须先消毒。
/// async 必需：同步 command 在主线程创建 webview 会与消息泵死锁（Windows）
#[tauri::command]
async fn open_preview(app: tauri::AppHandle, kind: String, id: String, title: String) -> Result<(), String> {
    if !matches!(kind.as_str(), "claude" | "codex") {
        return Err("未知预览类型".into());
    }
    if !migrate::is_session_id(&id) {
        return Err("非法会话 id".into());
    }
    let label = format!("preview-{id}");
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_focus();
        return Ok(());
    }
    // WebviewUrl::App 是路径而非 URL，query 会被当作文件名的一部分导致 404 白屏；
    // 参数经 initialization_script 注入（kind/id 已过白名单与 uuid 校验）
    tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App("index.html".into()))
        .initialization_script(&format!(
            "window.__PREVIEW__ = {{ kind: {kind:?}, id: {id:?} }};"
        ))
        .title(if title.is_empty() { "会话预览".into() } else { title })
        .inner_size(760.0, 640.0)
        .min_inner_size(480.0, 360.0)
        .decorations(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command(async)]
fn load_preview(kind: String, id: String) -> Result<preview::SessionPreview, String> {
    if !migrate::is_session_id(&id) {
        return Err("非法会话 id".into());
    }
    match kind.as_str() {
        "claude" => {
            let projects = cli_projects_dir()?;
            let target = format!("{id}.jsonl");
            let path = std::fs::read_dir(&projects)
                .map_err(|e| e.to_string())?
                .flatten()
                .map(|p| p.path().join(&target))
                .find(|p| p.is_file())
                .ok_or("找不到该会话的转录（可能已删除或位于沙箱内）")?;
            preview::preview_claude_jsonl(&path).map_err(|e| e.to_string())
        }
        "codex" => {
            let session = codex::list_codex_sessions()
                .into_iter()
                .find(|s| s.thread_id == id)
                .ok_or("找不到该 Codex 会话")?;
            preview::preview_codex_rollout(&session.rollout_path).map_err(|e| e.to_string())
        }
        _ => Err("未知预览类型".into()),
    }
}

#[tauri::command]
fn watch_status(state: tauri::State<'_, Arc<WatchState>>) -> WatchStatus {
    state.status()
}

#[tauri::command]
fn watch_set_paused(state: tauri::State<'_, Arc<WatchState>>, paused: bool) -> WatchStatus {
    state.set_paused(paused);
    state.status()
}

#[tauri::command(async)]
fn fastmode_status() -> FastModeStatus {
    fastmode::status()
}

#[tauri::command(async)]
fn fastmode_install() -> Result<ActionReport, String> {
    fastmode::install().map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn fastmode_uninstall() -> Result<ActionReport, String> {
    fastmode::uninstall().map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn fastmode_set_auto(auto: bool) -> Result<FastModeSettings, String> {
    fastmode::set_auto(auto).map_err(|e| e.to_string())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn run() {
    // macOS：被 Desktop 顺着软链当 bundled CLI 起起来，合并 fastMode 后转发，绝不初始化 GUI
    if let Some(argv0) = fastmode::wrapper_argv0() {
        std::process::exit(fastmode::run_wrapper(&argv0));
    }
    // 提权子进程：只做 renderer 写操作，不起窗口不起托盘
    if let Some(mode) = fastmode::elevated_mode_from_args() {
        std::process::exit(fastmode::run_elevated(&mode));
    }
    let watch_state = Arc::new(WatchState::new());

    tauri::Builder::default()
        .manage(watch_state.clone())
        .invoke_handler(tauri::generate_handler![
            scan,
            desktop_running,
            plan_unify_all,
            apply_unify_all,
            restore_combo,
            list_desktop_sessions,
            register_sessions,
            unregister_sessions,
            list_tombstones,
            purge_tombstones,
            delete_sessions,
            list_codex_sessions,
            codex_running,
            migrate_codex_sessions,
            open_preview,
            load_preview,
            watch_status,
            watch_set_paused,
            fastmode_status,
            fastmode_install,
            fastmode_uninstall,
            fastmode_set_auto,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let emitter = handle.clone();
            watch::spawn(watch_state.clone(), move |kind, payload| {
                let _ = emitter.emit(kind, payload);
            });

            let open = MenuItem::with_id(app, "open", "打开 Claude++", true, None::<&str>)?;
            let rescan = MenuItem::with_id(app, "rescan", "立即扫描并归一", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "暂停自动守护", true, None::<&str>)?;
            let resume = MenuItem::with_id(app, "resume", "恢复自动守护", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &rescan, &pause, &resume, &quit])?;

            let tray_state = watch_state.clone();
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().cloned().expect("bundled icon"))
                .tooltip("Claude++ 会话守护")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => show_main_window(app),
                    "rescan" => {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            if procs::any_desktop_running() {
                                let _ = app.emit("unify-blocked", "Desktop 正在运行");
                                return;
                            }
                            match watch::unify_new_combos() {
                                Ok(Some(summary)) => {
                                    let _ = app.emit("unify-auto", summary);
                                }
                                Ok(None) => {
                                    let _ = app.emit("unify-noop", "已全部归一");
                                }
                                Err(error) => {
                                    let _ = app.emit("unify-error", error.to_string());
                                }
                            }
                        });
                    }
                    "pause" => tray_state.set_paused(true),
                    "resume" => tray_state.set_paused(false),
                    "quit" => std::process::exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // close-to-tray keeps the watcher alive; preview windows close for real
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

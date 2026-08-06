pub mod discovery;
pub mod migrate;
pub mod procs;
pub mod unify;
pub mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use discovery::{DiscoveryReport, PoolKind};
use migrate::{
    ConflictPolicy, DesktopSession, PurgeOutcome, RegisterOutcome, TombstoneInfo, UnregisterOutcome,
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

#[tauri::command]
fn scan() -> DiscoveryReport {
    discovery::scan_all()
}

#[tauri::command]
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

#[tauri::command]
fn plan_unify_all() -> Vec<UnifyPlan> {
    let report = discovery::scan_all();
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

#[tauri::command]
fn apply_unify_all() -> Result<Vec<UnifyReport>, String> {
    ensure_desktop_stopped()?;
    let report = discovery::scan_all();
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

#[tauri::command]
fn restore_combo(path: String, org_id: String) -> Result<(), String> {
    ensure_desktop_stopped()?;
    unify::restore_combo(&PathBuf::from(path), &org_id).map_err(|e| e.to_string())
}

fn canonical_code_dir() -> Result<PathBuf, String> {
    let report = discovery::scan_all();
    migrate::canonical_code_dir_from(&report.combos).ok_or_else(|| "未发现任何 code 会话池".into())
}

#[tauri::command]
fn list_desktop_sessions() -> Result<Vec<DesktopSession>, String> {
    Ok(migrate::list_desktop_sessions(&canonical_code_dir()?))
}

#[tauri::command]
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
    for id in session_ids {
        let Some(cli) = report.cli_sessions.iter().find(|s| s.session_id == id) else {
            results.push(RegisterReport {
                session_id: id,
                outcome: None,
                error: Some("CLI 会话不存在".into()),
            });
            continue;
        };
        match migrate::register_cli_session(&code_dir, cli, policy) {
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

#[tauri::command]
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

#[tauri::command]
fn list_tombstones() -> Result<Vec<TombstoneInfo>, String> {
    Ok(migrate::list_tombstones(
        &canonical_code_dir()?,
        &cli_projects_dir()?,
    ))
}

#[tauri::command]
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

#[tauri::command]
fn watch_status(state: tauri::State<'_, Arc<WatchState>>) -> WatchStatus {
    state.status()
}

#[tauri::command]
fn watch_set_paused(state: tauri::State<'_, Arc<WatchState>>, paused: bool) -> WatchStatus {
    state.set_paused(paused);
    state.status()
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn run() {
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
            watch_status,
            watch_set_paused,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let emitter = handle.clone();
            watch::spawn(watch_state.clone(), move |summary| {
                let _ = emitter.emit("unify-auto", summary);
            });

            let open = MenuItem::with_id(app, "open", "打开 Claude++", true, None::<&str>)?;
            let rescan = MenuItem::with_id(app, "rescan", "立即扫描并归一", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "暂停自动归一", true, None::<&str>)?;
            let resume = MenuItem::with_id(app, "resume", "恢复自动归一", true, None::<&str>)?;
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
            // close-to-tray keeps the watcher alive
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

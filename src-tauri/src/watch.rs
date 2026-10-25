use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use serde::Serialize;

use crate::discovery::{self, PoolKind};
use crate::fastmode::{self, RepairOutcome};
use crate::{procs, unify};

const DEBOUNCE: Duration = Duration::from_secs(2);
const DESKTOP_POLL: Duration = Duration::from_secs(30);
// Desktop 本体更新发生在 WindowsApps，不在 notify 监控范围，空闲时定期看一眼
const IDLE_POLL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchStatus {
    pub running: bool,
    pub paused: bool,
    pub pending_unify: bool,
    pub pending_fastmode: bool,
    pub pending_desktop_update: bool,
    pub last_event: Option<String>,
}

pub struct WatchState {
    paused: AtomicBool,
    pending: AtomicBool,
    fastmode_pending: AtomicBool,
    update_pending: AtomicBool,
    update_error: Mutex<Option<String>>,
    wakeup: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    running: AtomicBool,
    last_event: Mutex<Option<String>>,
}

impl WatchState {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            fastmode_pending: AtomicBool::new(false),
            update_pending: AtomicBool::new(false),
            update_error: Mutex::new(None),
            wakeup: Mutex::new(None),
            running: AtomicBool::new(false),
            last_event: Mutex::new(None),
        }
    }

    pub fn status(&self) -> WatchStatus {
        WatchStatus {
            running: self.running.load(Ordering::Relaxed),
            paused: self.paused.load(Ordering::Relaxed),
            pending_unify: self.pending.load(Ordering::Relaxed),
            pending_fastmode: self.fastmode_pending.load(Ordering::Relaxed),
            pending_desktop_update: self.update_pending.load(Ordering::Relaxed),
            last_event: self.last_event.lock().unwrap().clone(),
        }
    }

    pub fn wake(&self) {
        if let Some(sender) = self.wakeup.lock().unwrap().as_ref() { let _ = sender.send(()); }
    }

    pub fn set_paused(&self, value: bool) {
        self.paused.store(value, Ordering::Relaxed);
        self.wake();
    }

    fn note(&self, message: impl Into<String>) {
        *self.last_event.lock().unwrap() = Some(message.into());
    }
}

/// Merge every non-junction, non-canonical combo. Returns a human summary
/// when anything was merged.
pub fn unify_new_combos() -> anyhow::Result<Option<String>> {
    let report = discovery::scan_combos();
    let mut merged = 0usize;
    let mut notes = Vec::new();
    for pool in [PoolKind::Code, PoolKind::Agent] {
        let Some(plan) = unify::plan_unify(&report.combos, pool) else {
            continue;
        };
        if plan.to_merge.is_empty() {
            continue;
        }
        let result = unify::apply_unify(&plan)?;
        merged += result.merged.len();
        notes.push(format!(
            "{}: {} 个组合并入 {}",
            pool.dir_name(),
            result.merged.len(),
            result.canonical.display()
        ));
    }
    Ok((merged > 0).then(|| notes.join("；")))
}

fn watch_roots() -> Vec<PathBuf> {
    discovery::default_roots()
        .into_iter()
        .flat_map(|root| {
            // claude-code 下是 bundled CLI 版本目录，Desktop 更新后新目录出现在这里
            [PoolKind::Code.dir_name(), PoolKind::Agent.dir_name(), "claude-code"]
                .into_iter()
                .map(move |p| root.path.join(p))
        })
        .filter(|p| p.is_dir())
        .collect()
}

fn has_unmerged_combo() -> bool {
    let report = discovery::scan_combos();
    [PoolKind::Code, PoolKind::Agent].into_iter().any(|pool| {
        unify::plan_unify(&report.combos, pool)
            .map(|plan| !plan.to_merge.is_empty())
            .unwrap_or(false)
    })
}

pub fn spawn(state: Arc<WatchState>, on_event: impl Fn(&str, String) + Send + 'static) {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        *state.wakeup.lock().unwrap() = Some(tx.clone());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                use notify::EventKind;
                if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    let _ = tx.send(());
                }
            }
        }).ok();
        let roots = watch_roots();
        for root in &roots {
            // depth-2 combos appear under these roots; recursive keeps it simple
            if let Some(watcher) = &mut watcher { let _ = watcher.watch(root, RecursiveMode::Recursive); }
        }
        #[cfg(target_os = "macos")]
        if let Some(home) = dirs::home_dir() {
            for name in ["Claude", "Claude-3p"] {
                if let Some(watcher) = &mut watcher { let _ = watcher.watch(&home.join("Library/Logs").join(name), RecursiveMode::NonRecursive); }
            }
            if let Some(watcher) = &mut watcher { let _ = watcher.watch(std::path::Path::new("/Applications"), RecursiveMode::NonRecursive); }
        }
        state.running.store(true, Ordering::Relaxed);
        state.wake();

        loop {
            let waiting_desktop = state.pending.load(Ordering::Relaxed)
                || state.fastmode_pending.load(Ordering::Relaxed)
                || state.update_pending.load(Ordering::Relaxed);
            let interval = if waiting_desktop { DESKTOP_POLL } else { IDLE_POLL };
            let woke = matches!(
                rx.recv_timeout(interval),
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            if !woke {
                break;
            }
            // swallow the burst that a directory move generates
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && rx.recv_timeout(DEBOUNCE).is_ok() {}

            if state.paused.load(Ordering::Relaxed) {
                continue;
            }
            // only an in-flight update defers the other guards; an unreadable journal must not starve them
            match crate::computer_use::updates::tick() {
                Ok(result) => {
                    state.update_pending.store(result.pending, Ordering::Relaxed);
                    *state.update_error.lock().unwrap() = None;
                    if result.changed {
                        state.note(result.message.clone());
                        on_event(if result.failed { "desktop-update-error" } else { "desktop-update" }, result.message);
                    }
                    if result.pending { continue; }
                }
                Err(error) => {
                    let message = format!("更新维护暂缓：{error:#}");
                    let mut last = state.update_error.lock().unwrap();
                    if last.as_deref() != Some(&message) {
                        state.note(message.clone());
                        on_event("desktop-update-error", message.clone());
                        *last = Some(message);
                    }
                }
            }
            match fastmode::auto_repair() {
                RepairOutcome::Skipped => {}
                RepairOutcome::Nothing => state.fastmode_pending.store(false, Ordering::Relaxed),
                RepairOutcome::Blocked => {
                    if !state.fastmode_pending.swap(true, Ordering::Relaxed) {
                        state.note("Fast Mode 需要修复，等待 Desktop 退出");
                    }
                }
                RepairOutcome::Repaired(summary) => {
                    state.fastmode_pending.store(false, Ordering::Relaxed);
                    state.note(format!("Fast Mode 已自动修复：{summary}"));
                    on_event("fastmode-auto", summary);
                }
                RepairOutcome::Failed(error) => {
                    state.fastmode_pending.store(false, Ordering::Relaxed);
                    state.note(format!("Fast Mode 自动修复失败：{error}"));
                    on_event("fastmode-error", error);
                }
            }
            if !has_unmerged_combo() {
                state.pending.store(false, Ordering::Relaxed);
                continue;
            }
            if procs::any_desktop_running() {
                state.pending.store(true, Ordering::Relaxed);
                state.note("检测到新组合，等待 Desktop 退出后归一");
                continue;
            }
            match unify_new_combos() {
                Ok(Some(summary)) => {
                    state.pending.store(false, Ordering::Relaxed);
                    state.note(summary.clone());
                    on_event("unify-auto", summary);
                }
                Ok(None) => state.pending.store(false, Ordering::Relaxed),
                Err(error) => state.note(format!("自动归一失败: {error}")),
            }
        }
    });
}

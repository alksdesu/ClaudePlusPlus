use serde::{Deserialize, Serialize};

use super::native::Selection;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    Watching,
    WaitingForExit,
    Restoring,
    AwaitingUpdate,
    WaitingForReapply,
    Applying,
    Unsupported,
    Failed,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Cursor {
    pub path: String,
    pub inode: u64,
    pub offset: u64,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Journal {
    pub schema: u32,
    pub target: std::path::PathBuf,
    pub enabled: bool,
    pub desired: Selection,
    pub applied: Selection,
    pub identity: String,
    pub phase: Phase,
    pub source_version: String,
    pub observed_stamp: String,
    pub restored_stamp: Option<String>,
    pub last_event: i64,
    pub cursors: Vec<Cursor>,
    pub policy_blocked: bool,
    pub launch_pending: bool,
    pub attempted_stamp: Option<String>,
    pub adapter_version: String,
    pub message: String,
    pub error: Option<String>,
    pub resume_phase: Option<Phase>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub supported: bool,
    pub enabled: bool,
    pub phase: Phase,
    pub message: String,
    pub desired_features: Vec<String>,
    pub pending_features: Vec<String>,
    pub error: Option<String>,
    pub busy: bool,
}

impl Status {
    fn unavailable() -> Self {
        Self {
            supported: cfg!(target_os = "macos"),
            enabled: false,
            phase: Phase::Watching,
            message: "更新维护已关闭；此流程仅 macOS 需要，Windows 使用官方更新".into(),
            desired_features: Vec::new(),
            pending_features: Vec::new(),
            error: None,
            busy: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Tick {
    pub pending: bool,
    pub changed: bool,
    pub failed: bool,
    pub message: String,
}

pub(crate) fn names(s: Selection) -> Vec<String> {
    [
        (s.computer_use, "Computer Use"),
        (s.chinese, "简体与繁体中文"),
        // Windows 版 Desktop 不带 iOS 模拟器
        (s.simulators, if cfg!(windows) { "Android 模拟器" } else { "iOS / Android 模拟器" }),
        (s.prewarm, "CLI 原生预热"),
        (s.fast_mode_status, "Fast Mode 原生开关"),
    ]
    .into_iter()
    .filter(|(selected, _)| *selected)
    .map(|(_, name)| name.into())
    .collect()
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn pending(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::WaitingForExit
            | Phase::Restoring
            | Phase::AwaitingUpdate
            | Phase::WaitingForReapply
            | Phase::Applying
    )
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    Available,
    Ready,
    SignatureRejected,
    PolicyDisabled,
    Checking,
    Offline,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Event {
    pub time: i64,
    pub kind: EventKind,
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn parse_event(line: &str) -> Option<Event> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let time = NaiveDateTime::parse_from_str(line.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()?;
    let rest = line.get(19..)?;
    let message = rest
        .strip_prefix(" [info] [updater] ")
        .or_else(|| rest.strip_prefix(" [error] [updater] "))?;
    let kind = if message == "Found an update, downloading" {
        EventKind::Available
    } else if message.starts_with("Update downloaded and ready to install ") {
        EventKind::Ready
    } else if message.starts_with("Auto-update error: ")
        && message.contains("Code signature at URL ")
        && message.contains("did not pass validation")
    {
        EventKind::SignatureRejected
    } else if message == "Auto-updates disabled by enterprise policy" {
        EventKind::PolicyDisabled
    } else if message == "Checking for updates" {
        EventKind::Checking
    } else if message == "Skipping update check: offline" {
        EventKind::Offline
    } else {
        return None;
    };
    Some(Event {
        time: Local.from_local_datetime(&time).single()?.timestamp(),
        kind,
    })
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn consume_event(j: &mut Journal, e: Event) {
    if e.time < j.last_event {
        return;
    }
    j.last_event = e.time;
    match e.kind {
        EventKind::PolicyDisabled => {
            j.policy_blocked = true;
            j.message = "组织策略关闭了自动更新，更新维护暂停".into();
        }
        EventKind::Checking => j.policy_blocked = false,
        EventKind::Available | EventKind::SignatureRejected
            if j.enabled
                && !j.policy_blocked
                && matches!(j.phase, Phase::Watching | Phase::Unsupported)
                && !j.applied.is_empty() =>
        {
            j.phase = Phase::WaitingForExit;
            j.message = "发现更新，等 Desktop 退出后还原官方包".into();
        }
        EventKind::Ready if j.phase == Phase::AwaitingUpdate => {
            j.message = "更新已下载，等官方安装器装完".into()
        }
        EventKind::Offline if j.phase == Phase::AwaitingUpdate => {
            j.message = "离线中，联网后继续等更新".into()
        }
        _ => {}
    }
}

pub fn status() -> Status {
    #[cfg(target_os = "macos")]
    return super::macos::updates::status().unwrap_or_else(|error| Status {
        supported: true,
        error: Some(format!("{error:#}")),
        busy: true,
        phase: Phase::Failed,
        message: "更新维护记录无法读取，已停止自动操作".into(),
        ..Status::unavailable()
    });
    #[cfg(not(target_os = "macos"))]
    Status::unavailable()
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<Status> {
    #[cfg(target_os = "macos")]
    return super::macos::updates::set_enabled(enabled);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = enabled;
        anyhow::bail!("Windows 不需要此流程；更新维护仅支持 macOS")
    }
}

pub fn retry() -> anyhow::Result<Status> {
    #[cfg(target_os = "macos")]
    return super::macos::updates::retry();
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("Windows 不需要此流程；更新维护仅支持 macOS")
}

pub fn tick() -> anyhow::Result<Tick> {
    #[cfg(target_os = "macos")]
    return super::macos::updates::tick();
    #[cfg(not(target_os = "macos"))]
    Ok(Tick::default())
}

pub fn manual_change<T>(
    restore: bool,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    #[cfg(target_os = "macos")]
    return super::macos::updates::manual_change(restore, operation);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = restore;
        operation()
    }
}

#[cfg(target_os = "macos")]
pub fn probe(
    app: &std::path::Path,
    store: &std::path::Path,
    action: &str,
) -> anyhow::Result<Status> {
    super::macos::updates::probe(app, store, action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> Journal {
        Journal {
            schema: 1,
            target: "/copy/Claude.app".into(),
            enabled: true,
            desired: Selection {
                computer_use: true,
                ..Default::default()
            },
            applied: Selection {
                computer_use: true,
                ..Default::default()
            },
            identity: "identity".into(),
            phase: Phase::Watching,
            source_version: "known".into(),
            observed_stamp: "stamp".into(),
            restored_stamp: None,
            last_event: 100,
            cursors: vec![],
            policy_blocked: false,
            launch_pending: false,
            attempted_stamp: None,
            adapter_version: "adapter".into(),
            message: String::new(),
            error: None,
            resume_phase: None,
        }
    }

    #[test]
    fn only_native_updater_events_are_accepted() {
        assert_eq!(
            parse_event("2026-10-24 01:00:00 [info] [updater] Found an update, downloading")
                .unwrap()
                .kind,
            EventKind::Available
        );
        for s in [
            "[updater] Found an update, downloading",
            "2026-10-24 01:00:00 [info] message: [updater] Found an update, downloading",
            "2026-10-24 01:00:00 [info] [updater] Found an update, downloading evil",
        ] {
            assert!(parse_event(s).is_none());
        }
    }

    #[test]
    fn stale_disabled_and_policy_blocked_events_do_not_start_restore() {
        let mut j = journal();
        consume_event(
            &mut j,
            Event {
                time: 99,
                kind: EventKind::Available,
            },
        );
        assert_eq!(j.phase, Phase::Watching);
        consume_event(
            &mut j,
            Event {
                time: 101,
                kind: EventKind::PolicyDisabled,
            },
        );
        consume_event(
            &mut j,
            Event {
                time: 102,
                kind: EventKind::Available,
            },
        );
        assert_eq!(j.phase, Phase::Watching);
        consume_event(
            &mut j,
            Event {
                time: 103,
                kind: EventKind::Checking,
            },
        );
        consume_event(
            &mut j,
            Event {
                time: 104,
                kind: EventKind::Available,
            },
        );
        assert_eq!(j.phase, Phase::WaitingForExit);
        j.phase = Phase::Watching;
        j.enabled = false;
        consume_event(
            &mut j,
            Event {
                time: 105,
                kind: EventKind::Available,
            },
        );
        assert_eq!(j.phase, Phase::Watching);
    }

    #[test]
    fn downloads_and_timeouts_never_claim_an_update_was_installed() {
        let mut j = journal();
        j.phase = Phase::AwaitingUpdate;
        for kind in [
            EventKind::Ready,
            EventKind::Offline,
            EventKind::SignatureRejected,
            EventKind::Available,
        ] {
            consume_event(&mut j, Event { time: 105, kind });
            assert_eq!(j.phase, Phase::AwaitingUpdate);
        }
    }

    #[test]
    fn journal_preserves_intent_across_restarts() {
        let mut j = journal();
        j.phase = Phase::Applying;
        j.resume_phase = Some(Phase::WaitingForReapply);
        let restored: Journal = serde_json::from_slice(&serde_json::to_vec(&j).unwrap()).unwrap();
        assert_eq!(restored.desired, j.desired);
        assert_eq!(restored.phase, Phase::Applying);
        assert!(pending(restored.phase));
        assert!(!pending(Phase::Unsupported));
        assert!(!pending(Phase::Watching));
    }
}

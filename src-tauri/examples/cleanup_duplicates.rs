//! 一次性清理：同一逻辑会话组在 Desktop 里的多条注册，留最新分支的那条；
//! --dangling 转为清理转录已不存在的死链注册（硬删元数据，不留无意义墓碑）。
//! 默认 dry-run，--apply 执行。

use std::collections::HashMap;

use claude_plus_plus_lib::{discovery, migrate, procs};

fn main() {
    let apply = std::env::args().any(|a| a == "--apply");
    let dangling_mode = std::env::args().any(|a| a == "--dangling");

    let report = discovery::scan_all();
    let Some(code_dir) = migrate::canonical_code_dir_from(&report.combos) else {
        eprintln!("未发现 code 会话池");
        std::process::exit(1);
    };
    let desktop = migrate::list_desktop_sessions(&code_dir);

    // cliSessionId -> (group_id, 转录 mtime)
    let cli_index: HashMap<&str, (&str, u64)> = report
        .cli_sessions
        .iter()
        .map(|s| (s.session_id.as_str(), (s.group_id.as_str(), s.last_activity_ms)))
        .collect();

    // group_id -> 该组的 Desktop 注册（含对应转录活跃时间）
    let mut by_group: HashMap<&str, Vec<(&migrate::DesktopSession, u64)>> = HashMap::new();
    let mut dangling = Vec::new();
    for session in desktop.iter().filter(|s| !s.sandboxed) {
        let Some(cli_id) = session.cli_session_id.as_deref() else {
            continue;
        };
        match cli_index.get(cli_id) {
            Some(&(group, activity)) => by_group.entry(group).or_default().push((session, activity)),
            None => dangling.push(session),
        }
    }

    if dangling_mode {
        println!("== 死链注册：cliSessionId 找不到转录，无法恢复对话 ==");
        for s in &dangling {
            println!("  {}  {:?}", s.file_name, s.title.as_deref().unwrap_or("-"));
        }
        println!("计划：硬删 {} 条死链元数据（转录已不存在，不留墓碑）", dangling.len());
        if !apply {
            println!("dry-run 结束，追加 --apply 执行");
            return;
        }
        if procs::any_desktop_running() {
            eprintln!("Claude Desktop 正在运行，请完全退出后重试");
            std::process::exit(1);
        }
        for session in &dangling {
            match migrate::unregister_desktop_session(&code_dir, &session.file_name, true) {
                Ok(outcome) => println!("已移除 {}", outcome.removed),
                Err(error) => eprintln!("失败 {}: {error}", session.file_name),
            }
        }
        return;
    }

    if !dangling.is_empty() {
        println!("== 跳过：cliSessionId 找不到转录（用 --dangling 清理） ==");
        for s in &dangling {
            println!("  {}  {:?}", s.file_name, s.title.as_deref().unwrap_or("-"));
        }
        println!();
    }

    let mut to_remove: Vec<&migrate::DesktopSession> = Vec::new();
    println!("== 重复注册的会话组 ==");
    let mut dup_groups = 0;
    for (group, mut members) in by_group {
        if members.len() < 2 {
            continue;
        }
        dup_groups += 1;
        // 转录 mtime 为主、Desktop 活跃时间 tie-break，最新者保留
        members.sort_by_key(|(s, activity)| std::cmp::Reverse((*activity, s.last_activity_at)));
        let (keep, keep_activity) = members[0];
        println!(
            "组 {}  共 {} 条注册  标题 {:?}",
            &group[..8.min(group.len())],
            members.len(),
            keep.title.as_deref().unwrap_or("-")
        );
        println!(
            "  保留 {}  cli={}  转录活跃 {}",
            keep.file_name,
            keep.cli_session_id.as_deref().map(|s| &s[..8]).unwrap_or("-"),
            keep_activity
        );
        for (session, activity) in &members[1..] {
            println!(
                "  注销 {}  cli={}  转录活跃 {}",
                session.file_name,
                session.cli_session_id.as_deref().map(|s| &s[..8]).unwrap_or("-"),
                activity
            );
            to_remove.push(session);
        }
    }
    if dup_groups == 0 {
        println!("  无重复注册，Desktop 列表已是每会话一条");
        return;
    }
    println!();
    println!("计划：{dup_groups} 个组，注销 {} 条重复注册", to_remove.len());

    if !apply {
        println!("dry-run 结束，追加 --apply 执行");
        return;
    }
    if procs::any_desktop_running() {
        eprintln!("Claude Desktop 正在运行，请完全退出后重试");
        std::process::exit(1);
    }
    for session in to_remove {
        match migrate::unregister_desktop_session(&code_dir, &session.file_name, false) {
            Ok(outcome) => println!("已注销 {}  墓碑 {:?}", outcome.removed, outcome.tombstone),
            Err(error) => eprintln!("失败 {}: {error}", session.file_name),
        }
    }
}

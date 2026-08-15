fn main() {
    let sessions = claude_plus_plus_lib::codex::list_codex_sessions();
    let count = |s: claude_plus_plus_lib::codex::CodexStatus| {
        sessions.iter().filter(|x| x.status == s).count()
    };
    use claude_plus_plus_lib::codex::CodexStatus::*;
    println!(
        "total={} nativeOnly={} migrated={} mirror={} orphanMirror={}",
        sessions.len(),
        count(NativeOnly),
        count(Migrated),
        count(Mirror),
        count(OrphanMirror)
    );
    println!("codex running: {}", claude_plus_plus_lib::procs::codex_running());
    for s in sessions.iter().take(15) {
        println!(
            "  {}  {:?}  {}  {}  {:?}",
            &s.thread_id[..13],
            s.status,
            s.originator.as_deref().unwrap_or("-"),
            s.cwd.as_deref().unwrap_or("-"),
            s.title.as_deref().unwrap_or("-")
        );
    }
}

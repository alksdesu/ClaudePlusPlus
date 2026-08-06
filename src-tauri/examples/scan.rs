fn main() {
    let report = claude_plus_plus_lib::discovery::scan_all();
    println!("== roots ==");
    for root in &report.roots {
        println!(
            "  {}  ant-did={}  {}",
            root.label,
            root.ant_did.as_deref().unwrap_or("-"),
            root.path.display()
        );
    }
    println!("== combos ==");
    for combo in &report.combos {
        println!(
            "  [{}/{:?}] {}/{}  sessions={} tombstones={} junction={}",
            combo.root_label,
            combo.pool,
            &combo.account_id[..8],
            &combo.org_id[..8],
            combo.session_count,
            combo.tombstone_count,
            combo
                .junction_target
                .as_ref()
                .map(|t| t.display().to_string())
                .unwrap_or_else(|| "no".into()),
        );
    }
    let registered = report.cli_sessions.iter().filter(|s| s.registered).count();
    let tombstoned = report.cli_sessions.iter().filter(|s| s.tombstoned).count();
    let with_cwd = report.cli_sessions.iter().filter(|s| s.cwd.is_some()).count();
    let entrypoints: std::collections::BTreeMap<&str, usize> =
        report.cli_sessions.iter().fold(Default::default(), |mut m, s| {
            *m.entry(s.entrypoint.as_deref().unwrap_or("-")).or_default() += 1;
            m
        });
    println!("== cli sessions ==");
    println!(
        "  total={} registered={} tombstoned={} with_cwd={}",
        report.cli_sessions.len(),
        registered,
        tombstoned,
        with_cwd
    );
    println!("  entrypoints={entrypoints:?}");
    for s in report.cli_sessions.iter().take(5) {
        println!(
            "  {}  reg={} tomb={}  {}  {:?}",
            &s.session_id[..8],
            s.registered,
            s.tombstoned,
            s.cwd.as_deref().unwrap_or("-"),
            s.first_user_text.as_deref().unwrap_or("-")
        );
    }
}

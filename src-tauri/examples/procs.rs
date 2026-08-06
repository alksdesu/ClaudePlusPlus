fn main() {
    let lines = claude_plus_plus_lib::procs::desktop_command_lines();
    println!("== desktop process lines ({}) ==", lines.len());
    for line in &lines {
        let head: String = line.chars().take(120).collect();
        println!("  {head}");
    }
    println!("== binding ==");
    for root in claude_plus_plus_lib::discovery::default_roots() {
        let running = claude_plus_plus_lib::procs::desktop_running_for(&root.path, &lines);
        println!("  {}  running={}", root.label, running);
    }
}

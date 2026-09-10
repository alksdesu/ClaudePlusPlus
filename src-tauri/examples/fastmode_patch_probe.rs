// 对任意一份 renderer 跑通 classify → patch → 再 classify，用于 Desktop 更新后验证锚点
#[cfg(windows)]
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: fastmode_patch_probe <renderer.js>");
    let text = std::fs::read_to_string(&path).expect("读取 renderer");
    println!("{}", claude_plus_plus_lib::fastmode::probe_patch(&text));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("renderer patch 只在 Windows 进行，macOS 不改 ion-dist");
    std::process::exit(1);
}

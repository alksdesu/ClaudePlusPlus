use std::path::PathBuf;

// 对任意一份 renderer 跑通 classify → patch → 再 classify，用于 Desktop 更新后验证锚点
fn main() {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: fastmode_patch_probe <renderer.js>"),
    );
    let text = std::fs::read_to_string(&path).expect("读取 renderer");
    println!("{}", claude_plus_plus_lib::fastmode::probe_patch(&text));
}

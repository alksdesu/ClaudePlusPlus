// 对任意一份 renderer 跑通 classify → patch → 再 classify，用于 Desktop 更新后验证锚点；
// 给第二个参数则把改写结果写出去，交给 node --check 验证语法
#[cfg(windows)]
fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: fastmode_patch_probe <renderer.js|assets-dir> [output]");
    if std::path::Path::new(&path).is_dir() {
        let output = args.next().map(std::path::PathBuf::from);
        match claude_plus_plus_lib::fastmode::probe_renderer_dir(std::path::Path::new(&path), output.as_deref()) {
            Ok(report) => println!("{report}"),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    let text = std::fs::read_to_string(&path).expect("读取 renderer");
    println!("{}", claude_plus_plus_lib::fastmode::probe_patch(&text));
    if let Some(out) = args.next() {
        let patched =
            claude_plus_plus_lib::fastmode::patch_renderer_text(&text).expect("改写 renderer");
        std::fs::write(&out, patched).expect("写出改写结果");
        println!("patched written to {out}");
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("renderer patch 只在 Windows 进行，macOS 不改 ion-dist");
    std::process::exit(1);
}

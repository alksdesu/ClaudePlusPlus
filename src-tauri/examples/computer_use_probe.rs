#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use claude_plus_plus_lib::computer_use;
    use std::path::Path;

    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() >= 3,
        "usage: computer_use_probe status|install|restore COPY/Claude.app STORE [IDENTITY]"
    );
    let app = Path::new(&args[1]);
    let store = Path::new(&args[2]);
    match args[0].as_str() {
        "status" => println!(
            "{}",
            serde_json::to_string_pretty(&computer_use::status_copy(app, store)?)?
        ),
        "install" => println!(
            "{}",
            computer_use::install_copy(
                app,
                store,
                args.get(3).ok_or_else(|| anyhow::anyhow!("缺少签名身份"))?
            )?
        ),
        "restore" => println!("{}", computer_use::restore_copy(app, store)?),
        _ => anyhow::bail!("未知操作"),
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Computer Use 副本验证仅支持 macOS");
    std::process::exit(1);
}

#[cfg(any(windows, target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use claude_plus_plus_lib::computer_use;
    use std::path::Path;

    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "status" {
        println!(
            "{}",
            serde_json::to_string_pretty(&computer_use::status()?)?
        );
        return Ok(());
    }
    anyhow::ensure!(
        args.len() >= 3,
        "usage: computer_use_probe status | status|install|restore COPY_APP STORE [IDENTITY]"
    );
    let app = Path::new(&args[1]);
    let store = Path::new(&args[2]);
    match args[0].as_str() {
        "status" => println!(
            "{}",
            serde_json::to_string_pretty(&computer_use::status_copy(app, store)?)?
        ),
        "install" => {
            let identity = args.get(3).map(String::as_str).unwrap_or("");
            #[cfg(target_os = "macos")]
            anyhow::ensure!(!identity.is_empty(), "缺少签名身份");
            println!("{}", computer_use::install_copy(app, store, identity)?);
        }
        #[cfg(target_os = "macos")]
        "apply" => {
            let identity = args.get(3).context("缺少签名身份")?;
            let selection = serde_json::from_str(args.get(4).context("缺少功能组合 JSON")?)?;
            println!(
                "{}",
                computer_use::apply_copy(app, store, identity, selection)?
            );
        }
        "restore" => println!("{}", computer_use::restore_copy(app, store)?),
        _ => anyhow::bail!("未知操作"),
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("Computer Use 副本验证仅支持 Windows 和 macOS");
    std::process::exit(1);
}

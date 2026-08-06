use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Command lines of every running claude.exe that is a Desktop app process
/// (executable under WindowsApps or an explicit --user-data-dir), excluding
/// the npm claude-code CLI which shares the binary name.
#[cfg(windows)]
pub fn desktop_command_lines() -> Vec<String> {
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-Command",
        "Get-CimInstance Win32_Process -Filter \"Name='claude.exe'\" | ForEach-Object { $_.CommandLine }",
    ]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    collect_lines(cmd)
}

/// Command lines of every running Claude Desktop process: the main bundle
/// executable, its Electron helpers, and the embedded claude-code child, all
/// living under a `Claude.app/Contents/` path. Standalone CLI installs
/// (npm / native) don't match.
#[cfg(not(windows))]
pub fn desktop_command_lines() -> Vec<String> {
    let mut cmd = Command::new("ps");
    cmd.args(["-axo", "command="]);
    collect_lines(cmd)
}

fn collect_lines(mut cmd: Command) -> Vec<String> {
    let Ok(output) = cmd.output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| is_desktop_command_line(line))
        .map(String::from)
        .collect()
}

#[cfg(windows)]
fn is_desktop_command_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if lower.contains("\\npm\\") || lower.contains("node_modules") {
        return false;
    }
    lower.contains("windowsapps\\claude") || lower.contains("--user-data-dir")
}

// --user-data-dir 是 Electron 通用参数（Discord、VS Code 都带），不能当识别特征；
// 判定锚定在可执行路径的第一个 .app bundle 名上，参数里出现的路径不参与
#[cfg(not(windows))]
fn is_desktop_command_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if lower.contains("/npm/") || lower.contains("node_modules") {
        return false;
    }
    let Some(pos) = lower.find(".app/contents/") else {
        return false;
    };
    let bundle_start = lower[..pos].rfind('/').map(|i| i + 1).unwrap_or(0);
    lower[bundle_start..pos].contains("claude")
}

/// True when a Desktop instance bound to `user_data` is alive. The default
/// (official) userData never appears as --user-data-dir, so any Desktop
/// process without that flag counts against the official root.
#[cfg(windows)]
pub fn desktop_running_for(user_data: &Path, command_lines: &[String]) -> bool {
    let needle = user_data.to_string_lossy().to_lowercase();
    let is_default_root = !needle.ends_with("claude-3p");
    command_lines.iter().any(|line| {
        let lower = line.to_lowercase();
        match lower.find("--user-data-dir") {
            Some(_) => lower.contains(&needle),
            None => is_default_root,
        }
    })
}

/// True when a Desktop instance bound to `user_data` is alive. The mac main
/// process carries no arguments; the bound userData leaks through child
/// processes instead (crashpad --database, the embedded claude-code path).
/// Matching requires a `/` boundary because the official root path is a
/// string prefix of the 3p one. If Desktop runs but no line locates any
/// root, every root counts as busy — refusing writes beats corrupting them.
#[cfg(not(windows))]
pub fn desktop_running_for(user_data: &Path, command_lines: &[String]) -> bool {
    if command_lines.is_empty() {
        return false;
    }
    let needle = user_data.to_string_lossy().to_lowercase();
    let bounded = format!("{needle}/");
    if command_lines.iter().any(|line| {
        let lower = line.to_lowercase();
        lower.contains(&bounded) || lower.ends_with(&needle)
    }) {
        return true;
    }
    let locates_some_root = command_lines.iter().any(|line| {
        let lower = line.to_lowercase();
        lower.contains("application support/claude-3p/") || lower.contains("application support/claude/")
    });
    !locates_some_root
}

pub fn any_desktop_running() -> bool {
    !desktop_command_lines().is_empty()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cli_binary_is_not_desktop() {
        assert!(!is_desktop_command_line(
            r#""C:\Users\x\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe" -c"#
        ));
        assert!(is_desktop_command_line(
            r#""C:\Program Files\WindowsApps\Claude_1.25927.0.0_x64__p\app\claude.exe""#
        ));
    }

    #[test]
    fn user_data_binding() {
        let lines = vec![
            r#""C:\Program Files\WindowsApps\Claude_1\app\claude.exe" --type=gpu-process --user-data-dir="C:\Users\x\AppData\Local\Claude-3p""#.to_string(),
        ];
        assert!(desktop_running_for(
            &PathBuf::from(r"C:\Users\x\AppData\Local\Claude-3p"),
            &lines
        ));
        assert!(!desktop_running_for(
            &PathBuf::from(r"C:\Users\x\AppData\Roaming\Claude"),
            &lines
        ));

        let official = vec![
            r#""C:\Program Files\WindowsApps\Claude_1\app\claude.exe""#.to_string(),
        ];
        assert!(desktop_running_for(
            &PathBuf::from(r"C:\Users\x\AppData\Roaming\Claude"),
            &official
        ));
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root_3p() -> PathBuf {
        PathBuf::from("/Users/x/Library/Application Support/Claude-3p")
    }

    fn root_official() -> PathBuf {
        PathBuf::from("/Users/x/Library/Application Support/Claude")
    }

    #[test]
    fn cli_binary_is_not_desktop() {
        assert!(!is_desktop_command_line(
            "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js -c"
        ));
        assert!(!is_desktop_command_line("/Users/x/.local/bin/claude -c"));
        assert!(is_desktop_command_line(
            "/Applications/Claude.app/Contents/MacOS/Claude"
        ));
        assert!(is_desktop_command_line(
            "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler --database=/Users/x/Library/Application Support/Claude-3p/Crashpad"
        ));
        // Desktop 分发的内嵌 claude-code 也是 Desktop 活动
        assert!(is_desktop_command_line(
            "/Users/x/Library/Application Support/Claude-3p/claude-code/2.1.219/claude.app/Contents/MacOS/claude --resume=x"
        ));
    }

    #[test]
    fn other_electron_apps_are_not_desktop() {
        // --user-data-dir 是 Electron 通用参数，Discord 不是 Desktop
        assert!(!is_desktop_command_line(
            "/Applications/Discord.app/Contents/Frameworks/Discord Helper.app/Contents/MacOS/Discord Helper --type=gpu-process --user-data-dir=/Users/x/Library/Application Support/discord"
        ));
        // 参数里出现含 claude 的项目路径不该让 VS Code 变成 Desktop
        assert!(!is_desktop_command_line(
            "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper /Users/x/work/ClaudePlusPlus"
        ));
    }

    #[test]
    fn child_process_paths_bind_user_data() {
        let lines = vec![
            "/Applications/Claude.app/Contents/MacOS/Claude".to_string(),
            "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler --database=/Users/x/Library/Application Support/Claude-3p/Crashpad".to_string(),
        ];
        assert!(desktop_running_for(&root_3p(), &lines));
        // 官方根是 3p 根的字符串前缀，边界匹配必须挡住误报
        assert!(!desktop_running_for(&root_official(), &lines));
    }

    #[test]
    fn official_root_binds_with_boundary() {
        let lines = vec![
            "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler --database=/Users/x/Library/Application Support/Claude/Crashpad".to_string(),
        ];
        assert!(desktop_running_for(&root_official(), &lines));
        assert!(!desktop_running_for(&root_3p(), &lines));
    }

    #[test]
    fn unlocatable_desktop_blocks_every_root() {
        let lines = vec!["/Applications/Claude.app/Contents/MacOS/Claude".to_string()];
        assert!(desktop_running_for(&root_3p(), &lines));
        assert!(desktop_running_for(&root_official(), &lines));
        assert!(!desktop_running_for(&root_3p(), &[]));
    }
}

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

/// 自身可执行路径，小写缓存一份供逐行比对
#[cfg(not(windows))]
fn self_exe_lower() -> Option<&'static str> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().to_lowercase())
        })
        .as_deref()
}

#[cfg(not(windows))]
fn is_desktop_command_line(line: &str) -> bool {
    is_desktop_line_excluding(line, self_exe_lower())
}

// --user-data-dir 是 Electron 通用参数（Discord、VS Code 都带），不能当识别特征；
// 判定锚定在可执行路径的第一个 .app bundle 名上，参数里出现的路径不参与。
// Claude++ 自己装在 Claude++.app 里，bundle 名同样含 claude，必须先按自身路径排掉，
// 否则它会把自己算成 Desktop，写操作全被自己挡住
#[cfg(not(windows))]
fn is_desktop_line_excluding(line: &str, self_exe: Option<&str>) -> bool {
    let lower = line.to_lowercase();
    if self_exe.is_some_and(|me| lower.starts_with(me)) {
        return false;
    }
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

/// Codex 本体进程存活检测（迁移会写它的导入记录，运行中拒绝）。
/// codex-plus-plus 是独立管理工具，与 Codex 本体无关，须排除。
#[cfg(windows)]
pub fn codex_running() -> bool {
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-Command",
        "Get-CimInstance Win32_Process -Filter \"Name='codex.exe' OR Name='codex-code-mode-host.exe'\" | ForEach-Object { $_.ProcessId }",
    ]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn codex_running() -> bool {
    let mut cmd = Command::new("ps");
    cmd.args(["-axo", "comm="]);
    cmd.output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).lines().any(|line| {
                let name = line.trim().rsplit('/').next().unwrap_or("");
                name == "codex" || name == "codex-code-mode-host" || name.eq_ignore_ascii_case("Codex")
            })
        })
        .unwrap_or(false)
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

    /// Claude++ 的 bundle 名同样含 claude，不按自身路径排掉就会把自己当成 Desktop
    #[test]
    fn claude_plus_plus_does_not_block_itself() {
        let me = "/applications/claude++.app/contents/macos/claude-plus-plus";
        let self_line = "/Applications/Claude++.app/Contents/MacOS/claude-plus-plus";
        assert!(is_desktop_line_excluding(self_line, None));
        assert!(!is_desktop_line_excluding(self_line, Some(me)));
        // 排除只认自身这一条路径，Desktop 本体照常识别
        assert!(is_desktop_line_excluding(
            "/Applications/Claude.app/Contents/MacOS/Claude",
            Some(me)
        ));
        // 被软链当 bundled CLI 起起来时 ps 显示的是软链路径，那仍是 Desktop 活动
        assert!(is_desktop_line_excluding(
            "/Users/x/Library/Application Support/Claude-3p/claude-code/2.1.260/claude.app/Contents/MacOS/claude --resume=y",
            Some(me)
        ));
        // 别的进程把 Claude++ 路径带在参数里，不该被当成自身而漏判
        assert!(is_desktop_line_excluding(
            "/Applications/Claude.app/Contents/MacOS/Claude --open /Applications/Claude++.app/Contents/MacOS/claude-plus-plus",
            Some(me)
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

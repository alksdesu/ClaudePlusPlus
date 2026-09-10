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
    let me = self_exe_lower();
    let hits: Vec<(String, String)> = ps_by_pid("pid=,comm=")
        .into_iter()
        .filter(|(_, exe)| is_desktop_exe_excluding(exe, me))
        .collect();
    if hits.is_empty() {
        return Vec::new();
    }
    // userData 只从参数里露出来（crashpad --database、内嵌 CLI 路径），配回完整命令行
    let commands = ps_by_pid("pid=,command=");
    hits.into_iter()
        .map(|(pid, exe)| {
            commands
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, line)| line.clone())
                .unwrap_or(exe)
        })
        .collect()
}

/// pid 占首列且靠右对齐，取值本身可能含空格（`Discord Helper`），只切第一个空白
#[cfg(not(windows))]
fn ps_by_pid(fields: &str) -> Vec<(String, String)> {
    let Ok(output) = Command::new("ps").args(["-axo", fields]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let rest = rest.trim();
            (!rest.is_empty()).then(|| (pid.to_string(), rest.to_string()))
        })
        .collect()
}

#[cfg(windows)]
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

// --user-data-dir 是 Electron 通用参数（Discord、VS Code 都带），不能当识别特征；
// 判定锚定在可执行路径的第一个 .app bundle 名上。只能拿 ps 的 comm 来判，不能拿整条
// 命令行：任何把 Claude.app/Contents/ 带在参数里的进程（终端、编辑器、构建脚本）
// 都会被算成 Desktop，把所有写操作挡死。Claude++ 自己装在 Claude++.app 里，
// bundle 名同样含 claude，必须先按自身路径排掉
#[cfg(not(windows))]
fn is_desktop_exe_excluding(exe: &str, self_exe: Option<&str>) -> bool {
    let lower = exe.to_lowercase();
    if self_exe.is_some_and(|me| lower == me) {
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
        assert!(!is_desktop_exe_excluding(
            "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            None
        ));
        assert!(!is_desktop_exe_excluding("/Users/x/.local/bin/claude", None));
        assert!(is_desktop_exe_excluding(
            "/Applications/Claude.app/Contents/MacOS/Claude",
            None
        ));
        assert!(is_desktop_exe_excluding(
            "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler",
            None
        ));
        // Desktop 分发的内嵌 claude-code 也是 Desktop 活动
        assert!(is_desktop_exe_excluding(
            "/Users/x/Library/Application Support/Claude-3p/claude-code/2.1.219/claude.app/Contents/MacOS/claude",
            None
        ));
    }

    /// Claude++ 的 bundle 名同样含 claude，不按自身路径排掉就会把自己当成 Desktop
    #[test]
    fn claude_plus_plus_does_not_block_itself() {
        let me = "/applications/claude++.app/contents/macos/claude-plus-plus";
        let self_exe = "/Applications/Claude++.app/Contents/MacOS/claude-plus-plus";
        assert!(is_desktop_exe_excluding(self_exe, None));
        assert!(!is_desktop_exe_excluding(self_exe, Some(me)));
        // 排除只认自身这一条路径，Desktop 本体照常识别
        assert!(is_desktop_exe_excluding(
            "/Applications/Claude.app/Contents/MacOS/Claude",
            Some(me)
        ));
        // 被软链当 bundled CLI 起起来时 ps 显示的是软链路径，那仍是 Desktop 活动
        assert!(is_desktop_exe_excluding(
            "/Users/x/Library/Application Support/Claude-3p/claude-code/2.1.260/claude.app/Contents/MacOS/claude",
            Some(me)
        ));
    }

    #[test]
    fn other_electron_apps_are_not_desktop() {
        assert!(!is_desktop_exe_excluding(
            "/Applications/Discord.app/Contents/Frameworks/Discord Helper.app/Contents/MacOS/Discord Helper",
            None
        ));
        assert!(!is_desktop_exe_excluding(
            "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper",
            None
        ));
    }

    /// 判定输入必须是 comm 而非整条命令行：把 Claude.app/Contents/ 带在参数里的
    /// 终端、编辑器、构建脚本一律不是 Desktop，否则写操作会被自己挡死
    #[test]
    fn argv_paths_do_not_make_a_desktop() {
        assert!(!is_desktop_exe_excluding("/bin/zsh", None));
        assert!(!is_desktop_exe_excluding("/usr/bin/make", None));
        assert!(!is_desktop_exe_excluding("/opt/homebrew/bin/node", None));
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

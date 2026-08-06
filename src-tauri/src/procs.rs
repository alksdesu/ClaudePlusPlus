use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Command lines of every running claude.exe that is a Desktop app process
/// (executable under WindowsApps or an explicit --user-data-dir), excluding
/// the npm claude-code CLI which shares the binary name.
pub fn desktop_command_lines() -> Vec<String> {
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-Command",
        "Get-CimInstance Win32_Process -Filter \"Name='claude.exe'\" | ForEach-Object { $_.CommandLine }",
    ]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
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

fn is_desktop_command_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if lower.contains("\\npm\\") || lower.contains("node_modules") {
        return false;
    }
    lower.contains("windowsapps\\claude") || lower.contains("--user-data-dir")
}

/// True when a Desktop instance bound to `user_data` is alive. The default
/// (official) userData never appears as --user-data-dir, so any Desktop
/// process without that flag counts against the official root.
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

pub fn any_desktop_running() -> bool {
    !desktop_command_lines().is_empty()
}

#[cfg(test)]
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

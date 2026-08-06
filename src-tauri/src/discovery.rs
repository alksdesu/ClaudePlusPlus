use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;

pub const CODE_POOL: &str = "claude-code-sessions";
pub const AGENT_POOL: &str = "local-agent-mode-sessions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PoolKind {
    Code,
    Agent,
}

impl PoolKind {
    pub fn dir_name(self) -> &'static str {
        match self {
            PoolKind::Code => CODE_POOL,
            PoolKind::Agent => AGENT_POOL,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDataRoot {
    pub label: String,
    pub path: PathBuf,
    pub ant_did: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolCombo {
    pub root_label: String,
    pub pool: PoolKind,
    pub account_id: String,
    pub org_id: String,
    pub path: PathBuf,
    pub is_junction: bool,
    pub junction_target: Option<PathBuf>,
    pub session_count: usize,
    pub tombstone_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliSession {
    pub session_id: String,
    pub jsonl_path: PathBuf,
    pub project_dir: String,
    pub cwd: Option<String>,
    pub entrypoint: Option<String>,
    pub first_user_text: Option<String>,
    pub first_timestamp: Option<String>,
    pub last_activity_ms: u64,
    pub size_bytes: u64,
    pub registered: bool,
    pub tombstoned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryReport {
    pub roots: Vec<UserDataRoot>,
    pub combos: Vec<PoolCombo>,
    pub cli_sessions: Vec<CliSession>,
}

fn is_uuid_dir(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        let expects_dash = matches!(i, 8 | 13 | 18 | 23);
        if expects_dash != (*b == b'-') {
            return false;
        }
        if !expects_dash && !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

pub fn decode_ant_did(root: &Path) -> Option<String> {
    let raw = fs::read_to_string(root.join("ant-did")).ok()?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let trimmed = text.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub fn default_roots() -> Vec<UserDataRoot> {
    let mut roots = Vec::new();
    let candidates = [
        ("Claude-3p", dirs::data_local_dir().map(|d| d.join("Claude-3p"))),
        ("Claude", dirs::data_dir().map(|d| d.join("Claude"))),
    ];
    for (label, path) in candidates {
        let Some(path) = path else { continue };
        if path.is_dir() {
            let ant_did = decode_ant_did(&path);
            roots.push(UserDataRoot {
                label: label.to_string(),
                path,
                ant_did,
            });
        }
    }
    roots
}

fn junction_target(path: &Path) -> Option<PathBuf> {
    junction::get_target(path).ok()
}

fn count_code_entries(dir: &Path) -> (usize, usize) {
    let mut sessions = 0;
    let mut tombstones = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("local_") && name.ends_with(".json") {
            sessions += 1;
        } else if name.starts_with("deleted_") {
            tombstones += 1;
        }
    }
    (sessions, tombstones)
}

fn count_agent_entries(dir: &Path) -> (usize, usize) {
    let mut sessions = 0;
    let mut tombstones = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name.starts_with("local_") && is_dir {
            sessions += 1;
        } else if name.starts_with("deleted_") {
            tombstones += 1;
        }
    }
    (sessions, tombstones)
}

pub fn list_combos(root: &UserDataRoot) -> Vec<PoolCombo> {
    let mut combos = Vec::new();
    for pool in [PoolKind::Code, PoolKind::Agent] {
        let pool_dir = root.path.join(pool.dir_name());
        let Ok(accounts) = fs::read_dir(&pool_dir) else {
            continue;
        };
        for account in accounts.flatten() {
            let account_name = account.file_name().to_string_lossy().into_owned();
            // skills-plugin lives at the account level but is not an account
            if !is_uuid_dir(&account_name) {
                continue;
            }
            let Ok(orgs) = fs::read_dir(account.path()) else {
                continue;
            };
            for org in orgs.flatten() {
                let org_name = org.file_name().to_string_lossy().into_owned();
                if !is_uuid_dir(&org_name) {
                    continue;
                }
                let path = org.path();
                let target = junction_target(&path);
                let (session_count, tombstone_count) = match pool {
                    PoolKind::Code => count_code_entries(&path),
                    PoolKind::Agent => count_agent_entries(&path),
                };
                combos.push(PoolCombo {
                    root_label: root.label.clone(),
                    pool,
                    account_id: account_name.clone(),
                    org_id: org_name,
                    is_junction: target.is_some(),
                    junction_target: target,
                    path,
                    session_count,
                    tombstone_count,
                });
            }
        }
    }
    combos
}

const HEAD_PROBE_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
struct JsonlProbe {
    cwd: Option<String>,
    entrypoint: Option<String>,
    first_user_text: Option<String>,
    first_timestamp: Option<String>,
}

fn extract_user_text(message: &serde_json::Value) -> Option<String> {
    let content = message.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                (p.get("type")?.as_str()? == "text").then(|| p.get("text")?.as_str().map(String::from))?
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("Caveat:")
    {
        return None;
    }
    let mut clipped: String = trimmed.chars().take(80).collect();
    if trimmed.chars().count() > 80 {
        clipped.push('…');
    }
    Some(clipped)
}

fn probe_jsonl_head(path: &Path) -> JsonlProbe {
    let mut probe = JsonlProbe::default();
    let Ok(mut file) = fs::File::open(path) else {
        return probe;
    };
    let mut buf = vec![0u8; HEAD_PROBE_BYTES];
    let Ok(read) = file.read(&mut buf) else {
        return probe;
    };
    buf.truncate(read);
    let head = String::from_utf8_lossy(&buf);
    for line in head.lines() {
        // the tail line of a truncated read is likely cut mid-JSON; parse errors just skip
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if probe.cwd.is_none() {
            probe.cwd = value.get("cwd").and_then(|v| v.as_str()).map(String::from);
        }
        if probe.entrypoint.is_none() {
            probe.entrypoint = value
                .get("entrypoint")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        if probe.first_timestamp.is_none() {
            probe.first_timestamp = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        if probe.first_user_text.is_none()
            && value.get("type").and_then(|v| v.as_str()) == Some("user")
            && !value
                .get("isSidechain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            if let Some(message) = value.get("message") {
                probe.first_user_text = extract_user_text(message);
            }
        }
        if probe.cwd.is_some() && probe.first_user_text.is_some() && probe.entrypoint.is_some() {
            break;
        }
    }
    probe
}

/// cliSessionIds referenced by code-pool metadata (active + tombstones).
pub struct RegisteredIndex {
    pub active: std::collections::HashSet<String>,
    pub tombstoned: std::collections::HashSet<String>,
}

pub fn registered_cli_ids(code_combos: &[&PoolCombo]) -> RegisteredIndex {
    let mut active = std::collections::HashSet::new();
    let mut tombstoned = std::collections::HashSet::new();
    for combo in code_combos {
        let Ok(entries) = fs::read_dir(&combo.path) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("local_") && name.ends_with(".json") {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(id) = value.get("cliSessionId").and_then(|v| v.as_str()) {
                            active.insert(id.to_string());
                        }
                    }
                }
            } else if let Some(id) = name.strip_prefix("deleted_") {
                tombstoned.insert(id.to_string());
            }
        }
    }
    RegisteredIndex { active, tombstoned }
}

pub fn cli_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

pub fn scan_cli_sessions(registered: &RegisteredIndex) -> Vec<CliSession> {
    let Some(projects) = cli_projects_dir() else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    let Ok(project_dirs) = fs::read_dir(&projects) else {
        return Vec::new();
    };
    for project in project_dirs.flatten() {
        if !project.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let project_name = project.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_uuid_dir(stem) {
                continue;
            }
            let path = file.path();
            let meta = file.metadata().ok();
            let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let last_activity_ms = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let probe = probe_jsonl_head(&path);
            sessions.push(CliSession {
                session_id: stem.to_string(),
                jsonl_path: path,
                project_dir: project_name.clone(),
                cwd: probe.cwd,
                entrypoint: probe.entrypoint,
                first_user_text: probe.first_user_text,
                first_timestamp: probe.first_timestamp,
                last_activity_ms,
                size_bytes,
                registered: registered.active.contains(stem),
                tombstoned: registered.tombstoned.contains(stem),
            });
        }
    }
    sessions.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
    sessions
}

pub fn scan_all() -> DiscoveryReport {
    let roots = default_roots();
    let mut combos = Vec::new();
    for root in &roots {
        combos.extend(list_combos(root));
    }
    let code_combos: Vec<&PoolCombo> = combos.iter().filter(|c| c.pool == PoolKind::Code).collect();
    let registered = registered_cli_ids(&code_combos);
    let cli_sessions = scan_cli_sessions(&registered);
    DiscoveryReport {
        roots,
        combos,
        cli_sessions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_dir_validation() {
        assert!(is_uuid_dir("82a69c9c-4852-476f-971f-40e5860d964b"));
        assert!(is_uuid_dir("00000000-0000-4000-8000-000000000001"));
        assert!(!is_uuid_dir("skills-plugin"));
        assert!(!is_uuid_dir("82a69c9c-4852-476f-971f-40e5860d964"));
        assert!(!is_uuid_dir("82a69c9c_4852_476f_971f_40e5860d964b"));
    }

    #[test]
    fn ant_did_decodes_base64_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("82a69c9c-4852-476f-971f-40e5860d964b");
        fs::write(dir.path().join("ant-did"), encoded).unwrap();
        assert_eq!(
            decode_ant_did(dir.path()).as_deref(),
            Some("82a69c9c-4852-476f-971f-40e5860d964b")
        );
    }

    #[test]
    fn combos_exclude_non_uuid_and_count_entries() {
        let dir = tempfile::tempdir().unwrap();
        let acc = "82a69c9c-4852-476f-971f-40e5860d964b";
        let org = "00000000-0000-4000-8000-000000000001";
        let code_org = dir.path().join(CODE_POOL).join(acc).join(org);
        fs::create_dir_all(&code_org).unwrap();
        fs::create_dir_all(dir.path().join(CODE_POOL).join("skills-plugin")).unwrap();
        fs::write(code_org.join("local_a.json"), "{}").unwrap();
        fs::write(code_org.join("deleted_b"), "123").unwrap();
        fs::write(code_org.join("scheduled-tasks.json"), "{}").unwrap();

        let root = UserDataRoot {
            label: "test".into(),
            path: dir.path().to_path_buf(),
            ant_did: None,
        };
        let combos = list_combos(&root);
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0].session_count, 1);
        assert_eq!(combos[0].tombstone_count, 1);
        assert!(!combos[0].is_junction);
    }

    #[test]
    fn user_text_skips_command_noise() {
        let msg = serde_json::json!({"role":"user","content":"<command-name>/model</command-name>"});
        assert_eq!(extract_user_text(&msg), None);
        let msg = serde_json::json!({"role":"user","content":[{"type":"text","text":"帮我看看这个项目"}]});
        assert_eq!(extract_user_text(&msg).as_deref(), Some("帮我看看这个项目"));
    }
}

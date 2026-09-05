use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    /// 同一逻辑会话（rewind/resume 分支、compact 续接）的所有 jsonl 共享同一组 id
    pub group_id: String,
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
    crate::link::target(path)
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
    first_message_uuid: Option<String>,
    first_is_compact_boundary: bool,
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
        || trimmed.starts_with("This session is being continued from a previous conversation")
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
        let line_type = value.get("type").and_then(|v| v.as_str());
        if probe.first_message_uuid.is_none()
            && matches!(line_type, Some("user" | "assistant" | "system"))
        {
            if let Some(uuid) = value.get("uuid").and_then(|v| v.as_str()) {
                probe.first_message_uuid = Some(uuid.to_string());
                probe.first_is_compact_boundary =
                    value.get("subtype").and_then(|v| v.as_str()) == Some("compact_boundary");
            }
        }
        if probe.first_user_text.is_none()
            && line_type == Some("user")
            && !value
                .get("isSidechain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            if let Some(message) = value.get("message") {
                probe.first_user_text = extract_user_text(message);
            }
        }
        if probe.cwd.is_some()
            && probe.first_user_text.is_some()
            && probe.entrypoint.is_some()
            && probe.first_message_uuid.is_some()
        {
            break;
        }
    }
    probe
}

const SCAN_CHUNK_BYTES: usize = 64 * 1024;

/// 流式判断文件是否包含任一 needle，命中即停；块间保留重叠避免跨界漏配。
fn file_contains(path: &Path, needles: &[String]) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let finders: Vec<memchr::memmem::Finder> = needles
        .iter()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()))
        .collect();
    let overlap = needles.iter().map(|n| n.len()).max().unwrap_or(0).saturating_sub(1);
    let mut window: Vec<u8> = Vec::with_capacity(SCAN_CHUNK_BYTES + overlap);
    let mut chunk = vec![0u8; SCAN_CHUNK_BYTES];
    loop {
        let Ok(read) = file.read(&mut chunk) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        window.extend_from_slice(&chunk[..read]);
        if finders.iter().any(|f| f.find(&window).is_some()) {
            return true;
        }
        let keep = window.len().saturating_sub(overlap);
        window.drain(..keep);
    }
}

static BRIDGE_CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), (u64, bool)>>> = OnceLock::new();

/// jsonl 只追加不改写：命中过的文件再长也还命中，未命中只在长度不变时可信
fn bridged(path: &Path, boundary: &str) -> bool {
    let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let key = (path.to_path_buf(), boundary.to_string());
    let cache = BRIDGE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache.lock().unwrap().get(&key).copied();
    if let Some((seen_len, found)) = cached {
        if (found && len >= seen_len) || (!found && len == seen_len) {
            return found;
        }
    }
    // 结构化匹配防误报：正文里被讨论的 uuid 经 JSON 转义带反斜杠，不会命中
    let needles = [
        format!("\"uuid\":\"{boundary}\""),
        format!("\"uuid\": \"{boundary}\""),
    ];
    let found = file_contains(path, &needles);
    cache.lock().unwrap().insert(key, (len, found));
    found
}

fn uf_find(uf: &mut [usize], mut x: usize) -> usize {
    while uf[x] != x {
        uf[x] = uf[uf[x]];
        x = uf[x];
    }
    x
}

fn uf_union(uf: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (uf_find(uf, a), uf_find(uf, b));
    if ra != rb {
        uf[ra] = rb;
    }
}

/// 同一逻辑会话的血缘统一判据：文件 X 的首条消息 uuid 出现在文件 Y 里即同组。
/// fork（rewind/resume）把公共前缀整段复制，首条消息 uuid 天然相同（头部即证）；
/// compact 续接则把 compact_boundary 行写进母文件中部后从它开始复制，boundary 的
/// 位置无规律，需对候选文件做全文流式搜索——仅 compact 开头的文件触发。
fn assign_groups(sessions: &mut [CliSession], probes: &[(Option<String>, bool)]) {
    let mut buckets: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (i, session) in sessions.iter().enumerate() {
        buckets.entry(session.project_dir.clone()).or_default().push(i);
    }
    for indices in buckets.values() {
        let n = indices.len();
        let mut uf: Vec<usize> = (0..n).collect();

        let mut by_first: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (local, &gi) in indices.iter().enumerate() {
            if let Some(first) = probes[gi].0.as_deref() {
                if let Some(&seen) = by_first.get(first) {
                    uf_union(&mut uf, local, seen);
                } else {
                    by_first.insert(first, local);
                }
            }
        }

        for (local, &gi) in indices.iter().enumerate() {
            if !probes[gi].1 {
                continue;
            }
            let Some(boundary) = probes[gi].0.as_deref() else { continue };
            for other in 0..n {
                if other == local || uf_find(&mut uf, other) == uf_find(&mut uf, local) {
                    continue;
                }
                if bridged(&sessions[indices[other]].jsonl_path, boundary) {
                    uf_union(&mut uf, local, other);
                }
            }
        }

        for (local, &gi) in indices.iter().enumerate() {
            let root = indices[uf_find(&mut uf, local)];
            let key = probes[root]
                .0
                .clone()
                .unwrap_or_else(|| sessions[root].session_id.clone());
            sessions[gi].group_id = key;
        }
    }
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
    let mut probes: Vec<(Option<String>, bool)> = Vec::new();
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
            probes.push((probe.first_message_uuid, probe.first_is_compact_boundary));
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
                group_id: String::new(),
            });
        }
    }
    // probes 与 sessions 按下标对齐，排序必须放在分组之后
    assign_groups(&mut sessions, &probes);
    sessions.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
    sessions
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComboReport {
    pub roots: Vec<UserDataRoot>,
    pub combos: Vec<PoolCombo>,
}

/// 只枚举 userData 下的会话池目录，毫秒级；不碰 CLI 转录
pub fn scan_combos() -> ComboReport {
    let roots = default_roots();
    let mut combos = Vec::new();
    for root in &roots {
        combos.extend(list_combos(root));
    }
    ComboReport { roots, combos }
}

pub fn scan_all() -> DiscoveryReport {
    let ComboReport { roots, combos } = scan_combos();
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
        let msg = serde_json::json!({"role":"user","content":"This session is being continued from a previous conversation that ran out of context."});
        assert_eq!(extract_user_text(&msg), None);
        let msg = serde_json::json!({"role":"user","content":[{"type":"text","text":"帮我看看这个项目"}]});
        assert_eq!(extract_user_text(&msg).as_deref(), Some("帮我看看这个项目"));
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn msg_line(sid: &str, uuid: &str, parent: Option<&str>, text: &str) -> String {
        serde_json::json!({
            "parentUuid": parent,
            "sessionId": sid,
            "type": "user",
            "uuid": uuid,
            "timestamp": "2026-08-01T10:00:00.000Z",
            "cwd": "E:\\Proj\\demo",
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn session_stub(stem: &str, path: PathBuf) -> CliSession {
        CliSession {
            session_id: stem.to_string(),
            jsonl_path: path,
            project_dir: "E--Proj-demo".into(),
            cwd: None,
            entrypoint: None,
            first_user_text: None,
            first_timestamp: None,
            last_activity_ms: 0,
            size_bytes: 0,
            registered: false,
            tombstoned: false,
            group_id: String::new(),
        }
    }

    fn boundary_line(uuid: &str, lp: &str) -> String {
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","uuid":"{uuid}","logicalParentUuid":"{lp}","content":"Conversation compacted"}}"#
        )
    }

    fn probe_pair(path: &Path) -> (Option<String>, bool) {
        let probe = probe_jsonl_head(path);
        (probe.first_message_uuid, probe.first_is_compact_boundary)
    }

    #[test]
    fn probe_extracts_fork_and_compact_markers() {
        let dir = tempfile::tempdir().unwrap();
        let forked = write_jsonl(
            dir.path(),
            "forked",
            &[
                r#"{"type":"custom-title","title":"x"}"#.to_string(),
                msg_line("forked", "m1", None, "第一句"),
            ],
        );
        let probe = probe_jsonl_head(&forked);
        assert_eq!(probe.first_message_uuid.as_deref(), Some("m1"));
        assert!(!probe.first_is_compact_boundary);

        let compacted = write_jsonl(
            dir.path(),
            "compacted",
            &[
                boundary_line("c1", "m9"),
                msg_line("compacted", "m10", Some("c1"), "This session is being continued from a previous conversation that ran out of context."),
            ],
        );
        let probe = probe_jsonl_head(&compacted);
        assert_eq!(probe.first_message_uuid.as_deref(), Some("c1"));
        assert!(probe.first_is_compact_boundary);
        assert_eq!(probe.first_user_text, None);
    }

    #[test]
    fn fork_and_compact_files_share_one_group() {
        let dir = tempfile::tempdir().unwrap();
        // compact 把 boundary 行写进母文件，续接文件从同一 boundary 行开始复制
        let root = write_jsonl(
            dir.path(),
            "root",
            &[
                msg_line("root", "m1", None, "起点"),
                msg_line("root", "m2", Some("m1"), "继续"),
                boundary_line("c1", "m2"),
            ],
        );
        let fork = write_jsonl(
            dir.path(),
            "fork",
            &[msg_line("fork", "m1", None, "起点"), msg_line("fork", "m3", Some("m1"), "分叉")],
        );
        let compact = write_jsonl(
            dir.path(),
            "compact",
            &[boundary_line("c1", "m2"), msg_line("compact", "m4", Some("c1"), "压缩后继续")],
        );
        let other = write_jsonl(dir.path(), "other", &[msg_line("other", "z1", None, "无关会话")]);
        // 孤儿 compact：母文件已删，boundary 无宿主，独立成组
        let orphan = write_jsonl(
            dir.path(),
            "orphan",
            &[boundary_line("c9", "m8"), msg_line("orphan", "m5", Some("c9"), "孤儿续接")],
        );

        let mut sessions = vec![
            session_stub("root", root.clone()),
            session_stub("fork", fork.clone()),
            session_stub("compact", compact.clone()),
            session_stub("other", other.clone()),
            session_stub("orphan", orphan.clone()),
        ];
        let probes: Vec<_> = [&root, &fork, &compact, &other, &orphan]
            .iter()
            .map(|p| probe_pair(p))
            .collect();
        assign_groups(&mut sessions, &probes);

        let group_of = |stem: &str| {
            sessions
                .iter()
                .find(|s| s.session_id == stem)
                .unwrap()
                .group_id
                .clone()
        };
        assert_eq!(group_of("root"), group_of("fork"));
        assert_eq!(group_of("root"), group_of("compact"));
        assert_ne!(group_of("root"), group_of("other"));
        assert_ne!(group_of("root"), group_of("orphan"));
        assert_ne!(group_of("other"), group_of("orphan"));
        assert!(!group_of("other").is_empty());
    }

    #[test]
    fn quoted_uuid_in_message_body_does_not_bridge() {
        let dir = tempfile::tempdir().unwrap();
        // 正文里讨论 uuid "c1" 的会话：JSON 转义使 "uuid":"c1" 变成 \"uuid\":\"c1\"，不得误桥
        let chatter = write_jsonl(
            dir.path(),
            "chatter",
            &[msg_line("chatter", "a1", None, r#"看这段元数据 "uuid":"c1" 是什么意思"#)],
        );
        let compact = write_jsonl(
            dir.path(),
            "compact",
            &[boundary_line("c1", "m9")],
        );
        let mut sessions = vec![session_stub("chatter", chatter.clone()), session_stub("compact", compact.clone())];
        let probes: Vec<_> = [&chatter, &compact].iter().map(|p| probe_pair(p)).collect();
        assign_groups(&mut sessions, &probes);
        assert_ne!(sessions[0].group_id, sessions[1].group_id);
    }

    #[test]
    fn file_contains_matches_across_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let needle = "\"uuid\":\"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\"".to_string();
        // needle 恰好横跨 64KB 块边界
        let path = dir.path().join("big.jsonl");
        let mut content = "x".repeat(SCAN_CHUNK_BYTES - 10);
        content.push_str(&needle);
        content.push_str(&"y".repeat(1000));
        fs::write(&path, &content).unwrap();
        assert!(file_contains(&path, std::slice::from_ref(&needle)));
        assert!(!file_contains(&path, &["\"uuid\":\"not-there\"".to_string()]));
    }

    #[test]
    fn bridge_cache_trusts_misses_only_at_same_length() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mother.jsonl");
        fs::write(&path, "{\"type\":\"user\",\"uuid\":\"m1\"}\n").unwrap();
        assert!(!bridged(&path, "c1"));

        // 同长度原地改写：真实 jsonl 不会这样，缓存按长度未变沿用未命中结果
        fs::write(&path, "{\"type\":\"user\",\"uuid\":\"c1\"}\n").unwrap();
        assert!(!bridged(&path, "c1"));

        // 追加 boundary 行：长度变了触发重搜，命中
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"type\":\"system\",\"uuid\":\"c1\"}\n").unwrap();
        drop(f);
        assert!(bridged(&path, "c1"));

        // 命中后继续追加仍命中；另一个 boundary 独立缓存
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"type\":\"user\",\"uuid\":\"m9\"}\n").unwrap();
        drop(f);
        assert!(bridged(&path, "c1"));
        assert!(!bridged(&path, "c2"));
    }
}

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::discovery::CliSession;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSession {
    pub file_name: String,
    pub session_id: String,
    pub cli_session_id: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub is_archived: bool,
    pub created_at: Option<u64>,
    pub last_activity_at: Option<u64>,
    pub completed_turns: Option<u64>,
    /// cwd points inside local-agent-mode-sessions: a sandboxed cowork session
    pub sandboxed: bool,
}

pub fn list_desktop_sessions(code_dir: &Path) -> Vec<DesktopSession> {
    let Ok(entries) = fs::read_dir(code_dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("local_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let str_of = |key: &str| value.get(key).and_then(|v| v.as_str()).map(String::from);
        let num_of = |key: &str| value.get(key).and_then(|v| v.as_u64());
        let cwd = str_of("cwd");
        let sandboxed = cwd
            .as_deref()
            .map(|c| c.contains("local-agent-mode-sessions"))
            .unwrap_or(false);
        sessions.push(DesktopSession {
            file_name: name,
            session_id: str_of("sessionId").unwrap_or_default(),
            cli_session_id: str_of("cliSessionId"),
            cwd,
            title: str_of("title"),
            model: str_of("model"),
            is_archived: value
                .get("isArchived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            created_at: num_of("createdAt"),
            last_activity_at: num_of("lastActivityAt"),
            completed_turns: num_of("completedTurns"),
            sandboxed,
        });
    }
    sessions.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));
    sessions
}

const TAIL_PROBE_BYTES: u64 = 128 * 1024;

#[derive(Debug, Default)]
struct JsonlTail {
    model: Option<String>,
    permission_mode: Option<String>,
    user_turns: u64,
}

fn probe_jsonl_tail(path: &Path) -> JsonlTail {
    let mut tail = JsonlTail::default();
    let Ok(mut file) = fs::File::open(path) else {
        return tail;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    // user turns need the whole file; count raw line prefixes without JSON parsing
    let mut reader = std::io::BufReader::new(&mut file);
    let mut line = String::new();
    while let Ok(n) = std::io::BufRead::read_line(&mut reader, &mut line) {
        if n == 0 {
            break;
        }
        if line.contains("\"type\":\"user\"") && !line.contains("\"isSidechain\":true") {
            tail.user_turns += 1;
        }
        line.clear();
    }

    let start = len.saturating_sub(TAIL_PROBE_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return tail;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return tail;
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(model) = value
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
        {
            tail.model = Some(model.to_string());
        }
        if let Some(mode) = value.get("permissionMode").and_then(|v| v.as_str()) {
            tail.permission_mode = Some(mode.to_string());
        }
    }
    tail
}

fn iso_to_ms(iso: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp_millis() as u64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Revive,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "status", rename_all_fields = "camelCase")]
pub enum RegisterOutcome {
    Registered { metadata_file: String },
    AlreadyRegistered { metadata_file: String },
    /// 同一逻辑会话的另一分支已在 Desktop，再注册只会得到重复条目
    SiblingRegistered { metadata_file: String, sibling_id: String },
    TombstoneBlocked,
    Skipped { reason: String },
}

fn find_active_registration(code_dir: &Path, cli_session_id: &str) -> Option<String> {
    let entries = fs::read_dir(code_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("local_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if value.get("cliSessionId").and_then(|v| v.as_str()) == Some(cli_session_id) {
                return Some(name);
            }
        }
    }
    None
}

fn title_from(cli: &CliSession) -> String {
    let raw = cli
        .first_user_text
        .as_deref()
        .unwrap_or("导入的 CLI 会话");
    let mut title: String = raw.chars().take(50).collect();
    if raw.chars().count() > 50 {
        title.push('…');
    }
    title
}

pub fn register_cli_session(
    code_dir: &Path,
    cli: &CliSession,
    policy: ConflictPolicy,
) -> Result<RegisterOutcome> {
    register_cli_session_with_siblings(code_dir, cli, policy, &[])
}

/// `siblings` 为同组其他分支的 cliSessionId：任一已注册即拒绝，避免同一逻辑
/// 会话在 Desktop 里出现多条。Overwrite 由调用方显式选择，不受此限。
pub fn register_cli_session_with_siblings(
    code_dir: &Path,
    cli: &CliSession,
    policy: ConflictPolicy,
    siblings: &[String],
) -> Result<RegisterOutcome> {
    if let Some(existing) = find_active_registration(code_dir, &cli.session_id) {
        return Ok(match policy {
            ConflictPolicy::Overwrite => {
                fs::remove_file(code_dir.join(&existing))?;
                write_registration(code_dir, cli)?
            }
            _ => RegisterOutcome::AlreadyRegistered {
                metadata_file: existing,
            },
        });
    }
    if policy != ConflictPolicy::Overwrite {
        for sibling in siblings {
            if let Some(existing) = find_active_registration(code_dir, sibling) {
                return Ok(RegisterOutcome::SiblingRegistered {
                    metadata_file: existing,
                    sibling_id: sibling.clone(),
                });
            }
        }
    }
    let tombstone = code_dir.join(format!("deleted_{}", cli.session_id));
    if tombstone.exists() {
        match policy {
            ConflictPolicy::Revive | ConflictPolicy::Overwrite => {
                fs::remove_file(&tombstone).context("移除墓碑")?;
            }
            ConflictPolicy::Skip => return Ok(RegisterOutcome::TombstoneBlocked),
        }
    }
    Ok(write_registration(code_dir, cli)?)
}

fn write_registration(code_dir: &Path, cli: &CliSession) -> Result<RegisterOutcome> {
    let cwd = cli
        .cwd
        .clone()
        .context("jsonl 缺少 cwd，无法注册（Desktop 需要项目路径）")?;
    let tail = probe_jsonl_tail(&cli.jsonl_path);
    let desktop_id = format!("local_{}", Uuid::new_v4());
    let created_at = cli
        .first_timestamp
        .as_deref()
        .and_then(iso_to_ms)
        .unwrap_or(cli.last_activity_ms);
    let last_activity = if cli.last_activity_ms > 0 {
        cli.last_activity_ms
    } else {
        now_ms()
    };

    let metadata = serde_json::json!({
        "sessionId": desktop_id,
        "cliSessionId": cli.session_id,
        "cwd": cwd,
        "originCwd": cwd,
        "lastFocusedAt": last_activity,
        "createdAt": created_at,
        "lastActivityAt": last_activity,
        "model": tail.model.unwrap_or_else(|| "claude-fable-5".to_string()),
        "isArchived": false,
        "title": title_from(cli),
        "titleSource": "auto",
        "permissionMode": tail.permission_mode.unwrap_or_else(|| "default".to_string()),
        "enabledMcpTools": {},
        "remoteMcpServersConfig": [],
        "completedTurns": tail.user_turns,
        "alwaysAllowedReasons": [],
        "sessionPermissionUpdates": [],
        "spawnSeed": {}
    });

    let file_name = format!("{desktop_id}.json");
    let dest = code_dir.join(&file_name);
    let tmp = code_dir.join(format!("{desktop_id}.json.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(&metadata)?)?;
    fs::rename(&tmp, &dest)?;
    Ok(RegisterOutcome::Registered {
        metadata_file: file_name,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnregisterOutcome {
    pub tombstone: Option<String>,
    pub removed: String,
}

pub fn unregister_desktop_session(
    code_dir: &Path,
    metadata_file: &str,
    hard_delete: bool,
) -> Result<UnregisterOutcome> {
    if !is_metadata_file(metadata_file) {
        bail!("非法元数据文件名: {metadata_file}");
    }
    let path = code_dir.join(metadata_file);
    let text = fs::read_to_string(&path).context("读取元数据")?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let cli_id = value
        .get("cliSessionId")
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut tombstone_name = None;
    if !hard_delete {
        // Desktop's own convention: deleted_<cliSessionId> holding the deletion epoch
        let id = cli_id.context("元数据缺少 cliSessionId，无法墓碑化")?;
        let name = format!("deleted_{id}");
        fs::write(code_dir.join(&name), now_ms().to_string())?;
        tombstone_name = Some(name);
    }
    fs::remove_file(&path)?;
    Ok(UnregisterOutcome {
        tombstone: tombstone_name,
        removed: metadata_file.to_string(),
    })
}

pub fn canonical_code_dir_from(combos: &[crate::discovery::PoolCombo]) -> Option<PathBuf> {
    crate::unify::plan_unify(combos, crate::discovery::PoolKind::Code).map(|p| p.canonical.path)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TombstoneInfo {
    pub file_name: String,
    pub cli_session_id: String,
    pub deleted_at: Option<u64>,
    pub jsonl_path: Option<PathBuf>,
    pub jsonl_size: u64,
}

fn find_cli_jsonl(projects_dir: &Path, cli_session_id: &str) -> Option<PathBuf> {
    let target = format!("{cli_session_id}.jsonl");
    for project in fs::read_dir(projects_dir).ok()?.flatten() {
        let candidate = project.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn list_tombstones(code_dir: &Path, projects_dir: &Path) -> Vec<TombstoneInfo> {
    let Ok(entries) = fs::read_dir(code_dir) else {
        return Vec::new();
    };
    let mut tombstones = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(cli_id) = name.strip_prefix("deleted_").filter(|id| is_session_id(id)) else {
            continue;
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let deleted_at = fs::read_to_string(entry.path())
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok());
        let jsonl_path = find_cli_jsonl(projects_dir, cli_id);
        let jsonl_size = jsonl_path
            .as_deref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        let cli_session_id = cli_id.to_string();
        tombstones.push(TombstoneInfo {
            file_name: name,
            cli_session_id,
            deleted_at,
            jsonl_path,
            jsonl_size,
        });
    }
    tombstones.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    tombstones
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeOutcome {
    pub file_name: String,
    pub marker_removed: bool,
    pub transcript_removed: bool,
    pub error: Option<String>,
}

fn remove_path(path: &Path, recycle: bool) -> Result<()> {
    if recycle {
        trash::delete(path).with_context(|| format!("送回收站失败: {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("删除失败: {}", path.display()))
    }
}

/// Purge tombstone markers, optionally taking the CLI transcript with them.
/// `recycle` routes deletions through the OS recycle bin so they stay
/// recoverable; tests pass false to avoid littering the real bin.
pub fn purge_tombstones(
    code_dir: &Path,
    projects_dir: &Path,
    file_names: &[String],
    delete_transcripts: bool,
    recycle: bool,
) -> Vec<PurgeOutcome> {
    let mut outcomes = Vec::new();
    for name in file_names {
        let Some(cli_id) = name.strip_prefix("deleted_").filter(|id| is_session_id(id)) else {
            outcomes.push(PurgeOutcome {
                file_name: name.clone(),
                marker_removed: false,
                transcript_removed: false,
                error: Some("非法墓碑文件名".into()),
            });
            continue;
        };
        let mut outcome = PurgeOutcome {
            file_name: name.clone(),
            marker_removed: false,
            transcript_removed: false,
            error: None,
        };
        // transcript first: a marker-less session with a live transcript would
        // resurface in Desktop scans, the reverse order can't happen
        if delete_transcripts {
            if let Some(jsonl) = find_cli_jsonl(projects_dir, cli_id) {
                match remove_path(&jsonl, recycle) {
                    Ok(()) => outcome.transcript_removed = true,
                    Err(error) => {
                        outcome.error = Some(format!("{error:#}"));
                        outcomes.push(outcome);
                        continue;
                    }
                }
            }
        }
        match remove_path(&code_dir.join(name), recycle) {
            Ok(()) => outcome.marker_removed = true,
            Err(error) => outcome.error = Some(format!("{error:#}")),
        }
        outcomes.push(outcome);
    }
    outcomes
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOutcome {
    pub cli_session_id: Option<String>,
    pub metadata_removed: Option<String>,
    pub tombstone_removed: bool,
    pub transcript_removed: bool,
}

pub fn is_session_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// 前缀后缀之外还须是纯文件名，否则 local_..\..\x.json 能拼出 code_dir 之外的路径
pub fn is_metadata_file(name: &str) -> bool {
    name.starts_with("local_")
        && name.ends_with(".json")
        && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// Desktop 侧先删：转录先没了而元数据还在会留下死链条目，反序最坏只是转录
/// 重新出现在 CLI 列表，可重试。
fn delete_traces(
    code_dir: &Path,
    projects_dir: &Path,
    cli_session_id: Option<&str>,
    metadata_file: Option<String>,
    recycle: bool,
) -> Result<DeleteOutcome> {
    let mut outcome = DeleteOutcome {
        cli_session_id: cli_session_id.map(String::from),
        metadata_removed: None,
        tombstone_removed: false,
        transcript_removed: false,
    };
    if let Some(name) = metadata_file {
        let path = code_dir.join(&name);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("删除元数据 {name}"))?;
            outcome.metadata_removed = Some(name);
        }
    }
    let Some(id) = cli_session_id else {
        return Ok(outcome);
    };
    let tombstone = code_dir.join(format!("deleted_{id}"));
    if tombstone.exists() {
        fs::remove_file(&tombstone).context("移除墓碑")?;
        outcome.tombstone_removed = true;
    }
    if let Some(jsonl) = find_cli_jsonl(projects_dir, id) {
        remove_path(&jsonl, recycle)?;
        outcome.transcript_removed = true;
    }
    Ok(outcome)
}

/// 从 CLI 侧删除：转录 + 它在 Desktop 的注册与墓碑一并抹掉
pub fn delete_cli_session(
    code_dir: &Path,
    projects_dir: &Path,
    cli_session_id: &str,
    recycle: bool,
) -> Result<DeleteOutcome> {
    if !is_session_id(cli_session_id) {
        bail!("非法会话 id: {cli_session_id}");
    }
    let metadata = find_active_registration(code_dir, cli_session_id);
    delete_traces(
        code_dir,
        projects_dir,
        Some(cli_session_id),
        metadata,
        recycle,
    )
}

/// 从 Desktop 侧删除：元数据 + 墓碑 + 它指向的 CLI 转录
pub fn delete_desktop_session(
    code_dir: &Path,
    projects_dir: &Path,
    metadata_file: &str,
    recycle: bool,
) -> Result<DeleteOutcome> {
    if !is_metadata_file(metadata_file) {
        bail!("非法元数据文件名: {metadata_file}");
    }
    // 元数据损坏时仍要能删掉这条，读不出 cliSessionId 就只清 Desktop 侧
    let cli_id = fs::read_to_string(code_dir.join(metadata_file))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("cliSessionId")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .filter(|id| is_session_id(id));
    delete_traces(
        code_dir,
        projects_dir,
        cli_id.as_deref(),
        Some(metadata_file.to_string()),
        recycle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cli(dir: &Path) -> CliSession {
        let jsonl = dir.join("12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9.jsonl");
        let lines = [
            r#"{"type":"user","cwd":"E:\\Proj\\demo","sessionId":"12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"user","content":"帮我修一个 bug"}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:05.000Z","message":{"role":"assistant","model":"claude-fable-5","content":[{"type":"text","text":"好"}]}}"#,
            r#"{"type":"user","timestamp":"2026-08-01T10:01:00.000Z","message":{"role":"user","content":"继续"}}"#,
        ];
        fs::write(&jsonl, lines.join("\n")).unwrap();
        CliSession {
            session_id: "12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9".into(),
            jsonl_path: jsonl,
            project_dir: "E--Proj-demo".into(),
            cwd: Some("E:\\Proj\\demo".into()),
            entrypoint: Some("cli".into()),
            first_user_text: Some("帮我修一个 bug".into()),
            first_timestamp: Some("2026-08-01T10:00:00.000Z".into()),
            last_activity_ms: 1_785_578_460_000,
            size_bytes: 1,
            registered: false,
            tombstoned: false,
            group_id: "12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9".into(),
        }
    }

    #[test]
    fn register_writes_expected_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cli = sample_cli(dir.path());
        let outcome = register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).unwrap();
        let RegisterOutcome::Registered { metadata_file } = outcome else {
            panic!("expected Registered, got {outcome:?}");
        };
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(&metadata_file)).unwrap())
                .unwrap();
        assert_eq!(value["cliSessionId"], "12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9");
        assert_eq!(value["cwd"], "E:\\Proj\\demo");
        assert_eq!(value["originCwd"], value["cwd"]);
        assert_eq!(value["model"], "claude-fable-5");
        assert_eq!(value["title"], "帮我修一个 bug");
        assert_eq!(value["titleSource"], "auto");
        assert_eq!(value["completedTurns"], 2);
        assert_eq!(value["createdAt"], 1_785_578_400_000u64); // 2026-08-01T10:00:00Z
        assert!(value["sessionId"].as_str().unwrap().starts_with("local_"));
        assert_eq!(value["isArchived"], false);
    }

    #[test]
    fn duplicate_and_tombstone_flow() {
        let dir = tempfile::tempdir().unwrap();
        let cli = sample_cli(dir.path());

        let first = register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).unwrap();
        let RegisterOutcome::Registered { metadata_file } = first else {
            panic!()
        };
        // duplicate → AlreadyRegistered
        let dup = register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).unwrap();
        assert!(matches!(dup, RegisterOutcome::AlreadyRegistered { .. }));

        // unregister leaves a tombstone with the Desktop naming convention
        let out = unregister_desktop_session(dir.path(), &metadata_file, false).unwrap();
        assert_eq!(
            out.tombstone.as_deref(),
            Some("deleted_12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9")
        );
        assert!(!dir.path().join(&metadata_file).exists());

        // tombstone blocks Skip policy, Revive clears it
        let blocked = register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).unwrap();
        assert!(matches!(blocked, RegisterOutcome::TombstoneBlocked));
        let revived = register_cli_session(dir.path(), &cli, ConflictPolicy::Revive).unwrap();
        assert!(matches!(revived, RegisterOutcome::Registered { .. }));
        assert!(!dir
            .path()
            .join("deleted_12ad197f-2a0f-451d-b6eb-2e2ed6ab30a9")
            .exists());
    }

    #[test]
    fn sibling_branch_blocks_duplicate_registration() {
        let dir = tempfile::tempdir().unwrap();
        let cli = sample_cli(dir.path());
        register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).unwrap();

        // 同组另一分支：cliSessionId 不同，逐个查重挡不住，须靠 siblings
        let mut branch = sample_cli(dir.path());
        branch.session_id = "99999999-2a0f-451d-b6eb-2e2ed6ab30a9".into();
        let siblings = vec![cli.session_id.clone()];
        let outcome =
            register_cli_session_with_siblings(dir.path(), &branch, ConflictPolicy::Skip, &siblings)
                .unwrap();
        assert!(matches!(outcome, RegisterOutcome::SiblingRegistered { .. }));

        // 无 siblings 上下文时照旧放行，保证单会话路径不受影响
        let outcome = register_cli_session(dir.path(), &branch, ConflictPolicy::Skip).unwrap();
        assert!(matches!(outcome, RegisterOutcome::Registered { .. }));
    }

    #[test]
    fn register_without_cwd_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut cli = sample_cli(dir.path());
        cli.cwd = None;
        assert!(register_cli_session(dir.path(), &cli, ConflictPolicy::Skip).is_err());
    }

    #[test]
    fn tombstone_listing_and_purge() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&code).unwrap();
        let proj = projects.join("E--P1");
        fs::create_dir_all(&proj).unwrap();

        fs::write(code.join("deleted_aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9"), "1785000000000").unwrap();
        fs::write(code.join("deleted_bbb22222-2a0f-451d-b6eb-2e2ed6ab30a9"), "1786000000000").unwrap();
        fs::write(code.join("local_x.json"), "{}").unwrap();
        fs::write(proj.join("aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9.jsonl"), "line").unwrap();

        let listed = list_tombstones(&code, &projects);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].cli_session_id, "bbb22222-2a0f-451d-b6eb-2e2ed6ab30a9"); // newest deletion first
        assert!(listed[0].jsonl_path.is_none());
        let with_transcript = listed.iter().find(|t| t.cli_session_id == "aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9").unwrap();
        assert!(with_transcript.jsonl_path.is_some());
        assert_eq!(with_transcript.jsonl_size, 4);

        // markers only: transcript survives
        let outcomes = purge_tombstones(
            &code,
            &projects,
            &["deleted_bbb22222-2a0f-451d-b6eb-2e2ed6ab30a9".to_string()],
            false,
            false,
        );
        assert!(outcomes[0].marker_removed && !outcomes[0].transcript_removed);
        assert!(!code.join("deleted_bbb22222-2a0f-451d-b6eb-2e2ed6ab30a9").exists());

        // full purge takes the transcript too
        let outcomes = purge_tombstones(
            &code,
            &projects,
            &["deleted_aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9".to_string()],
            true,
            false,
        );
        assert!(outcomes[0].marker_removed && outcomes[0].transcript_removed);
        assert!(!code.join("deleted_aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9").exists());
        assert!(!proj.join("aaa11111-2a0f-451d-b6eb-2e2ed6ab30a9.jsonl").exists());

        // invalid name is reported, not silently skipped
        let outcomes = purge_tombstones(&code, &projects, &["local_x.json".to_string()], true, false);
        assert!(outcomes[0].error.is_some());
        assert!(code.join("local_x.json").exists());
    }

    #[test]
    fn sandboxed_detection() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("local_a.json"),
            r#"{"sessionId":"local_a","cliSessionId":"c1","cwd":"C:\\U\\x\\AppData\\Local\\Claude-3p\\local-agent-mode-sessions\\a\\b\\local_a\\outputs"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("local_b.json"),
            r#"{"sessionId":"local_b","cliSessionId":"c2","cwd":"E:\\Real\\project"}"#,
        )
        .unwrap();
        let sessions = list_desktop_sessions(dir.path());
        let by_id: std::collections::HashMap<_, _> =
            sessions.iter().map(|s| (s.session_id.clone(), s.sandboxed)).collect();
        assert_eq!(by_id["local_a"], true);
        assert_eq!(by_id["local_b"], false);
    }

    #[test]
    fn delete_cli_session_wipes_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let projects = dir.path().join("projects");
        let proj = projects.join("E--Proj-demo");
        fs::create_dir_all(&code).unwrap();
        fs::create_dir_all(&proj).unwrap();
        let cli = sample_cli(&proj);
        register_cli_session(&code, &cli, ConflictPolicy::Skip).unwrap();
        fs::write(code.join(format!("deleted_{}", cli.session_id)), "1785000000000").unwrap();

        let outcome = delete_cli_session(&code, &projects, &cli.session_id, false).unwrap();
        assert!(outcome.metadata_removed.is_some());
        assert!(outcome.tombstone_removed);
        assert!(outcome.transcript_removed);
        assert!(!cli.jsonl_path.exists());
        assert!(find_active_registration(&code, &cli.session_id).is_none());
        assert!(!code.join(format!("deleted_{}", cli.session_id)).exists());

        // 已经删干净后重来一次不报错，只是什么都没删
        let again = delete_cli_session(&code, &projects, &cli.session_id, false).unwrap();
        assert!(again.metadata_removed.is_none() && !again.transcript_removed);
    }

    #[test]
    fn delete_desktop_session_follows_cli_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let projects = dir.path().join("projects");
        let proj = projects.join("E--Proj-demo");
        fs::create_dir_all(&code).unwrap();
        fs::create_dir_all(&proj).unwrap();
        let cli = sample_cli(&proj);
        let RegisterOutcome::Registered { metadata_file } =
            register_cli_session(&code, &cli, ConflictPolicy::Skip).unwrap()
        else {
            panic!()
        };

        let outcome = delete_desktop_session(&code, &projects, &metadata_file, false).unwrap();
        assert_eq!(outcome.cli_session_id.as_deref(), Some(cli.session_id.as_str()));
        assert_eq!(outcome.metadata_removed.as_deref(), Some(metadata_file.as_str()));
        assert!(outcome.transcript_removed);
        assert!(!code.join(&metadata_file).exists());
        assert!(!cli.jsonl_path.exists());
        // 删除不留墓碑：墓碑的作用是挡住重新注册，转录都没了没有可挡的
        assert!(!code.join(format!("deleted_{}", cli.session_id)).exists());
    }

    #[test]
    fn delete_desktop_session_survives_broken_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&code).unwrap();
        fs::create_dir_all(&projects).unwrap();
        fs::write(code.join("local_broken.json"), "{ not json").unwrap();

        let outcome = delete_desktop_session(&code, &projects, "local_broken.json", false).unwrap();
        assert!(outcome.cli_session_id.is_none());
        assert!(outcome.metadata_removed.is_some());
        assert!(!code.join("local_broken.json").exists());
    }

    #[test]
    fn delete_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&code).unwrap();
        fs::create_dir_all(&projects).unwrap();
        assert!(delete_cli_session(&code, &projects, "../../etc/passwd", false).is_err());
        assert!(delete_cli_session(&code, &projects, "not-a-uuid", false).is_err());
        assert!(delete_desktop_session(&code, &projects, "../secrets.json", false).is_err());
        assert!(delete_desktop_session(&code, &projects, "deleted_x", false).is_err());
        // 前缀后缀都合法，中间夹相对路径
        assert!(delete_desktop_session(&code, &projects, "local_../../evil.json", false).is_err());
        // 反斜杠只在 Windows 是分隔符，mac 上这是个合法文件名
        #[cfg(windows)]
        assert!(delete_desktop_session(&code, &projects, r"local_..\..\evil.json", false).is_err());
        assert!(unregister_desktop_session(&code, "local_../../evil.json", false).is_err());
        let escaped = purge_tombstones(&code, &projects, &["deleted_../../evil".to_string()], true, false);
        assert!(escaped[0].error.is_some());
        // 元数据里塞非法 cliSessionId 也不能带着拼路径
        fs::write(
            code.join("local_evil.json"),
            r#"{"cliSessionId":"../../../boom"}"#,
        )
        .unwrap();
        let outcome = delete_desktop_session(&code, &projects, "local_evil.json", false).unwrap();
        assert!(outcome.cli_session_id.is_none());
    }
}

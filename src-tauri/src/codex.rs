//! Codex 会话发现与向 Claude 的迁移：rollout 事件流 → Claude CLI 转录。
//! 迁移产物以 threadId 命名，与导入记录互为防循环闭环。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::Digest;
use uuid::Uuid;

const CLAUDE_VERSION: &str = "2.1.219";
const MODEL: &str = "claude-fable-5";
const HEAD_PROBE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexStatus {
    /// Claude 侧存在迁移产物 <threadId>.jsonl
    Migrated,
    /// 由 Claude 转录导入而来，且源转录仍在
    Mirror,
    /// 由 Claude 转录导入而来，但源转录已删除
    OrphanMirror,
    /// Codex 原生独有，可迁移
    NativeOnly,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSession {
    pub thread_id: String,
    pub rollout_path: PathBuf,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub title: Option<String>,
    pub size_bytes: u64,
    pub last_activity_ms: u64,
    pub status: CodexStatus,
    pub claude_path: Option<PathBuf>,
}

pub fn codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

fn thread_id_of(rollout: &Path) -> Option<String> {
    let stem = rollout.file_stem()?.to_string_lossy();
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_string())
}

fn list_rollouts(codex: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![codex.join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

/// 导入记录：imported_thread_id → source_path。
/// 迁移产物记录的 source stem == threadId，与真正的 Claude→Codex 导入互斥。
fn import_sources(codex: &Path) -> std::collections::HashMap<String, String> {
    let path = codex.join("external_agent_session_imports.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Default::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Default::default();
    };
    let mut map = std::collections::HashMap::new();
    for record in value.get("records").and_then(|v| v.as_array()).into_iter().flatten() {
        let (Some(tid), Some(src)) = (
            record.get("imported_thread_id").and_then(|v| v.as_str()),
            record.get("source_path").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        map.insert(tid.to_string(), src.trim_start_matches("\\\\?\\").to_string());
    }
    map
}

/// session_index.jsonl 里用户命名过的 thread 标题
fn index_titles(codex: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = fs::read_to_string(codex.join("session_index.jsonl")) else {
        return map;
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            value.get("id").and_then(|v| v.as_str()),
            value.get("thread_name").and_then(|v| v.as_str()),
        ) {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}

fn is_noise_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.is_empty()
        || trimmed.starts_with("# Files mentioned by the user")
        || trimmed.starts_with("<user_instructions>")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("Caveat:")
        || trimmed.starts_with("This session is being continued from a previous conversation")
}

#[derive(Debug, Default)]
struct RolloutHead {
    cwd: Option<String>,
    originator: Option<String>,
    first_user_text: Option<String>,
}

fn probe_rollout_head(path: &Path) -> RolloutHead {
    let mut head = RolloutHead::default();
    let Ok(mut file) = fs::File::open(path) else {
        return head;
    };
    let mut buf = vec![0u8; HEAD_PROBE_BYTES];
    let Ok(read) = file.read(&mut buf) else {
        return head;
    };
    buf.truncate(read);
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = value.get("payload");
        match value.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                if let Some(p) = payload {
                    head.cwd = p.get("cwd").and_then(|v| v.as_str()).map(String::from);
                    head.originator = p.get("originator").and_then(|v| v.as_str()).map(String::from);
                }
            }
            Some("event_msg") if head.first_user_text.is_none() => {
                let Some(p) = payload else { continue };
                if p.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                    if let Some(message) = p.get("message").and_then(|v| v.as_str()) {
                        if !is_noise_user_text(message) {
                            let trimmed = message.trim();
                            let mut clipped: String = trimmed.chars().take(80).collect();
                            if trimmed.chars().count() > 80 {
                                clipped.push('…');
                            }
                            head.first_user_text = Some(clipped);
                        }
                    }
                }
            }
            _ => {}
        }
        if head.cwd.is_some() && head.first_user_text.is_some() {
            break;
        }
    }
    head
}

fn claude_transcript_for(thread_id: &str, projects: &Path) -> Option<PathBuf> {
    let target = format!("{thread_id}.jsonl");
    for project in fs::read_dir(projects).ok()?.flatten() {
        let candidate = project.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn list_codex_sessions() -> Vec<CodexSession> {
    let Some(codex) = codex_home() else {
        return Vec::new();
    };
    let Some(projects) = crate::discovery::cli_projects_dir() else {
        return Vec::new();
    };
    let imports = import_sources(&codex);
    let titles = index_titles(&codex);

    let mut sessions = Vec::new();
    for rollout in list_rollouts(&codex) {
        let Some(thread_id) = thread_id_of(&rollout) else {
            continue;
        };
        let meta = fs::metadata(&rollout).ok();
        let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let last_activity_ms = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let claude_path = claude_transcript_for(&thread_id, &projects);
        let status = if claude_path.is_some() {
            CodexStatus::Migrated
        } else {
            match imports.get(&thread_id) {
                // 迁移产物记录（source stem == threadId）不算 Claude→Codex 导入；
                // 产物已被删除时回到 NativeOnly，允许重新迁移
                Some(source) if Path::new(source).file_stem().is_some_and(|s| s.to_string_lossy() == thread_id.as_str()) => {
                    CodexStatus::NativeOnly
                }
                Some(source) => {
                    if Path::new(source).is_file() {
                        CodexStatus::Mirror
                    } else {
                        CodexStatus::OrphanMirror
                    }
                }
                None => CodexStatus::NativeOnly,
            }
        };

        let head = probe_rollout_head(&rollout);
        sessions.push(CodexSession {
            title: titles.get(&thread_id).cloned().or(head.first_user_text),
            thread_id,
            rollout_path: rollout,
            cwd: head.cwd,
            originator: head.originator,
            size_bytes,
            last_activity_ms,
            status,
            claude_path,
        });
    }
    sessions.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
    sessions
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "status", rename_all_fields = "camelCase")]
pub enum MigrateOutcome {
    Migrated { claude_path: PathBuf, turns: usize },
    AlreadyMigrated { claude_path: PathBuf },
    Skipped { reason: String },
}

fn encode_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn det_uuid(thread_id: &str, index: usize) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("codex-import/{thread_id}/{index}").as_bytes(),
    )
    .to_string()
}

fn iso_to_secs(iso: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
}

struct Turn {
    role: &'static str,
    timestamp: String,
    text: String,
}

fn extract_turns(rollout: &Path) -> Result<(Option<String>, Vec<Turn>)> {
    let text = fs::read_to_string(rollout).with_context(|| format!("读取 {}", rollout.display()))?;
    let mut cwd = None;
    let mut turns = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = value.get("payload");
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("1970-01-01T00:00:00.000Z")
            .to_string();
        match value.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                if cwd.is_none() {
                    cwd = payload
                        .and_then(|p| p.get("cwd"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
            }
            Some("event_msg") => {
                let Some(p) = payload else { continue };
                let role = match p.get("type").and_then(|v| v.as_str()) {
                    Some("user_message") => "user",
                    Some("agent_message") => "assistant",
                    _ => continue,
                };
                let Some(message) = p.get("message").and_then(|v| v.as_str()) else {
                    continue;
                };
                if message.trim().is_empty() {
                    continue;
                }
                turns.push(Turn {
                    role,
                    timestamp,
                    text: message.to_string(),
                });
            }
            _ => {}
        }
    }
    Ok((cwd, turns))
}

fn claude_line(thread_id: &str, cwd: &str, parent: Option<&str>, index: usize, turn: &Turn) -> serde_json::Value {
    let uuid = det_uuid(thread_id, index);
    let message = if turn.role == "user" {
        serde_json::json!({"role": "user", "content": turn.text})
    } else {
        serde_json::json!({
            "id": format!("msg_{uuid}"),
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [{"type": "text", "text": turn.text}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}
        })
    };
    serde_json::json!({
        "parentUuid": parent,
        "isSidechain": false,
        "userType": "external",
        "cwd": cwd,
        "sessionId": thread_id,
        "version": CLAUDE_VERSION,
        "entrypoint": "cli",
        "type": turn.role,
        "message": message,
        "uuid": uuid,
        "timestamp": turn.timestamp,
    })
}

/// 在导入记录里登记迁移产物，Codex 据此不再把它同步回去
fn register_import_record(codex: &Path, thread_id: &str, dest: &Path, content: &str, last_secs: f64) -> Result<()> {
    let path = codex.join("external_agent_session_imports.json");
    let mut value: serde_json::Value = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(_) => serde_json::json!({"records": []}),
    };
    let win_path = format!("\\\\?\\{}", dest.display().to_string().replace('/', "\\"));
    let records = value
        .get_mut("records")
        .and_then(|v| v.as_array_mut())
        .context("records 字段缺失")?;
    let sha = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    if let Some(existing) = records.iter_mut().find(|r| {
        r.get("imported_thread_id").and_then(|v| v.as_str()) == Some(thread_id)
            && r.get("source_path").and_then(|v| v.as_str()) == Some(win_path.as_str())
    }) {
        existing["content_sha256"] = serde_json::Value::String(sha);
        existing["source_modified_at"] = serde_json::json!((last_secs * 1e9) as u64);
    } else {
        records.push(serde_json::json!({
            "source_path": win_path,
            "content_sha256": sha,
            "imported_thread_id": thread_id,
            "imported_at": last_secs as u64,
            "source_modified_at": (last_secs * 1e9) as u64,
            "connector_names": [],
            "title": null
        }));
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&value)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn migrate_session(codex: &Path, projects: &Path, rollout: &Path) -> Result<MigrateOutcome> {
    let thread_id = thread_id_of(rollout).context("rollout 文件名缺少 thread id")?;
    if let Some(existing) = claude_transcript_for(&thread_id, projects) {
        return Ok(MigrateOutcome::AlreadyMigrated {
            claude_path: existing,
        });
    }
    let (cwd, turns) = extract_turns(rollout)?;
    if turns.is_empty() {
        return Ok(MigrateOutcome::Skipped {
            reason: "会话没有可迁移的对话内容".into(),
        });
    }
    let cwd = cwd.or_else(|| dirs::home_dir().map(|h| h.display().to_string()))
        .context("无法确定会话工作目录")?;

    let project_dir = projects.join(encode_project_dir(&cwd));
    fs::create_dir_all(&project_dir)?;
    let dest = project_dir.join(format!("{thread_id}.jsonl"));

    let mut lines = Vec::with_capacity(turns.len());
    let mut parent: Option<String> = None;
    for (index, turn) in turns.iter().enumerate() {
        let line = claude_line(&thread_id, &cwd, parent.as_deref(), index, turn);
        parent = Some(det_uuid(&thread_id, index));
        lines.push(serde_json::to_string(&line)?);
    }
    let content = format!("{}\n", lines.join("\n"));

    let tmp = project_dir.join(format!("{thread_id}.jsonl.tmp"));
    fs::write(&tmp, &content)?;
    fs::rename(&tmp, &dest)?;

    let last_secs = turns
        .last()
        .and_then(|t| iso_to_secs(&t.timestamp))
        .unwrap_or(0.0);
    if last_secs > 0.0 {
        let ft = filetime::FileTime::from_unix_time(last_secs as i64, 0);
        let _ = filetime::set_file_mtime(&dest, ft);
    }
    register_import_record(codex, &thread_id, &dest, &content, last_secs)?;
    Ok(MigrateOutcome::Migrated {
        claude_path: dest,
        turns: turns.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout_lines(tid: &str, cwd: &str) -> String {
        [
            format!(
                r#"{{"timestamp":"2026-08-01T10:00:00.000Z","type":"session_meta","payload":{{"session_id":"{tid}","id":"{tid}","cwd":"{}","originator":"Codex Desktop"}}}}"#,
                cwd.replace('\\', "\\\\")
            ),
            r##"{"timestamp":"2026-08-01T10:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"# Files mentioned by the user\n附件注入"}}"##.into(),
            r#"{"timestamp":"2026-08-01T10:00:02.000Z","type":"event_msg","payload":{"type":"user_message","message":"帮我看看这个项目"}}"#.into(),
            r#"{"timestamp":"2026-08-01T10:00:03.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"thinking"}}"#.into(),
            r#"{"timestamp":"2026-08-01T10:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"好的，这是一个测试项目"}}"#.into(),
            r#"{"timestamp":"2026-08-01T10:00:06.000Z","type":"response_item","payload":{"type":"function_call","name":"shell"}}"#.into(),
        ]
        .join("\n")
    }

    fn setup(dir: &Path, tid: &str) -> (PathBuf, PathBuf, PathBuf) {
        let codex = dir.join(".codex");
        let projects = dir.join("projects");
        let day = codex.join("sessions").join("2026").join("08").join("01");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir_all(&projects).unwrap();
        let rollout = day.join(format!("rollout-2026-08-01T10-00-00-{tid}.jsonl"));
        fs::write(&rollout, rollout_lines(tid, "E:\\Proj\\demo")).unwrap();
        (codex, projects, rollout)
    }

    #[test]
    fn head_probe_skips_attachment_noise() {
        let dir = tempfile::tempdir().unwrap();
        let tid = "019dd272-52ff-76b3-9814-4e3c1c534dd0";
        let (_, _, rollout) = setup(dir.path(), tid);
        let head = probe_rollout_head(&rollout);
        assert_eq!(head.cwd.as_deref(), Some("E:\\Proj\\demo"));
        assert_eq!(head.originator.as_deref(), Some("Codex Desktop"));
        assert_eq!(head.first_user_text.as_deref(), Some("帮我看看这个项目"));
    }

    #[test]
    fn migrate_writes_claude_transcript_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let tid = "019dd272-52ff-76b3-9814-4e3c1c534dd0";
        let (codex, projects, rollout) = setup(dir.path(), tid);

        let outcome = migrate_session(&codex, &projects, &rollout).unwrap();
        let MigrateOutcome::Migrated { claude_path, turns } = outcome else {
            panic!("expected Migrated");
        };
        assert_eq!(turns, 3);
        assert!(claude_path.ends_with(format!("E--Proj-demo/{tid}.jsonl")));

        let text = fs::read_to_string(&claude_path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["parentUuid"], serde_json::Value::Null);
        assert_eq!(lines[0]["sessionId"], tid);
        assert_eq!(lines[0]["cwd"], "E:\\Proj\\demo");
        assert_eq!(lines[2]["type"], "assistant");
        assert_eq!(lines[2]["parentUuid"], lines[1]["uuid"]);
        assert_eq!(lines[2]["message"]["model"], MODEL);

        // 防循环记录已登记
        let imports: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(codex.join("external_agent_session_imports.json")).unwrap(),
        )
        .unwrap();
        let recs = imports["records"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["imported_thread_id"], tid);

        // 再迁移 → AlreadyMigrated，不追加记录
        let again = migrate_session(&codex, &projects, &rollout).unwrap();
        assert!(matches!(again, MigrateOutcome::AlreadyMigrated { .. }));

        // 状态判定：产物存在 → Migrated；删除产物后 → NativeOnly（可重迁）
        let sources = import_sources(&codex);
        assert!(sources.contains_key(tid));
        fs::remove_file(&claude_path).unwrap();
        let source = &sources[tid];
        assert!(Path::new(source)
            .file_stem()
            .is_some_and(|s| s.to_string_lossy() == tid));
    }

    #[test]
    fn deterministic_uuids_are_stable() {
        assert_eq!(det_uuid("t1", 0), det_uuid("t1", 0));
        assert_ne!(det_uuid("t1", 0), det_uuid("t1", 1));
        assert_ne!(det_uuid("t1", 0), det_uuid("t2", 0));
    }

    #[test]
    fn project_dir_encoding_matches_claude_convention() {
        assert_eq!(encode_project_dir("E:\\Codex++\\ClaudePlusPlus"), "E--Codex---ClaudePlusPlus");
        assert_eq!(encode_project_dir("F:\\123"), "F--123");
        assert_eq!(encode_project_dir("E:\\SillyTavern\\vertex2openai2\\webchat"), "E--SillyTavern-vertex2openai2-webchat");
    }
}

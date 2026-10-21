//! Codex 会话发现与向 Claude 的迁移：rollout 条目 → Claude CLI 转录。
//! 迁移产物以 threadId 命名，与导入记录互为防循环闭环。

mod names;
mod rollout;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::Digest;
use uuid::Uuid;

pub use rollout::Entry;
use rollout::{CodexThread, RolloutIndex};

const CLAUDE_VERSION: &str = "2.1.219";
const MODEL: &str = "claude-fable-5";
// 标题兜底只读最早一页的开头：首条用户输入之前只有 session_meta 和注入的上下文
const TITLE_PROBE_BYTES: u64 = 1024 * 1024;
const TITLE_CHARS: usize = 80;
const EPOCH_ISO: &str = "1970-01-01T00:00:00.000Z";
const FILE_MENTIONS_HEADER: &str = "# Files mentioned by the user";
const REQUEST_HEADING: &str = "My request";
// 不是用户亲手输入的包装消息：Claude 转录镜像过来的命令回显、通知、续接摘要
const TITLE_NOISE_PREFIXES: &[&str] = &[
    "<user_instructions>",
    "<environment_context>",
    "<command-name>",
    "<local-command",
    "<task-notification",
    "<teammate-message",
    "Caveat:",
    "This session is being continued from a previous conversation",
];

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

/// 预览与迁移共用的一份对话内容
pub struct CodexTranscript {
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub entries: Vec<Entry>,
}

pub fn codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// 扫一遍 Codex 的 rollout 存储，列表、预览、迁移共用
pub struct CodexStore {
    home: PathBuf,
    index: RolloutIndex,
}

impl CodexStore {
    pub fn open() -> Option<Self> {
        codex_home().map(|home| Self::open_at(&home))
    }

    pub fn open_at(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            index: RolloutIndex::scan(&home.join("sessions")),
        }
    }

    /// 子代理线程是代理派生的工作线程，Codex 里也只挂在父对话下面
    pub fn sessions(&self, projects: &Path) -> Vec<CodexSession> {
        let names = names::thread_names(&self.home);
        let imports = import_sources(&self.home);
        let mut sessions: Vec<CodexSession> = self
            .index
            .threads()
            .into_iter()
            .filter(|thread| !thread.subagent())
            .map(|thread| {
                let origin = self.index.origin(&thread);
                let claude_path = claude_transcript_for(thread.thread_id, projects);
                CodexSession {
                    thread_id: thread.thread_id.to_string(),
                    rollout_path: thread.head.path.clone(),
                    cwd: origin.cwd.clone().or_else(|| thread.head.cwd.clone()),
                    originator: origin.originator.clone().or_else(|| thread.head.originator.clone()),
                    title: names
                        .get(thread.thread_id)
                        .cloned()
                        .or_else(|| self.index.first_user_text(&thread, TITLE_PROBE_BYTES, title_text)),
                    size_bytes: thread.size_bytes(),
                    last_activity_ms: thread.last_activity_ms(),
                    status: status_of(thread.thread_id, claude_path.is_some(), &imports),
                    claude_path,
                }
            })
            .collect();
        sessions.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
        sessions
    }

    pub fn transcript(&self, thread_id: &str) -> Result<CodexTranscript> {
        let thread = self.index.thread(thread_id).context("找不到该 Codex 会话")?;
        self.transcript_of(&thread)
    }

    /// 先在索引里认出这个对话，路径一律用索引里校验过的 ID 拼
    pub fn migrate(&self, projects: &Path, thread_id: &str) -> Result<MigrateOutcome> {
        let thread = self.index.thread(thread_id).context("找不到该 Codex 会话")?;
        let thread_id = thread.thread_id;
        if let Some(existing) = claude_transcript_for(thread_id, projects) {
            return Ok(MigrateOutcome::AlreadyMigrated {
                claude_path: existing,
            });
        }
        let transcript = self.transcript_of(&thread)?;
        let turns: Vec<Turn> = transcript.entries.into_iter().filter_map(Turn::from_entry).collect();
        if turns.is_empty() {
            return Ok(MigrateOutcome::Skipped {
                reason: "会话没有可迁移的对话内容".into(),
            });
        }
        let cwd = transcript
            .cwd
            .or_else(|| dirs::home_dir().map(|h| h.display().to_string()))
            .context("无法确定会话工作目录")?;

        let project_dir = projects.join(encode_project_dir(&cwd));
        fs::create_dir_all(&project_dir)?;
        let dest = project_dir.join(format!("{thread_id}.jsonl"));

        let mut lines = Vec::with_capacity(turns.len());
        let mut parent: Option<String> = None;
        for (index, turn) in turns.iter().enumerate() {
            let line = claude_line(thread_id, &cwd, parent.as_deref(), index, turn);
            parent = Some(det_uuid(thread_id, index));
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
        register_import_record(&self.home, thread_id, &dest, &content, last_secs)?;
        Ok(MigrateOutcome::Migrated {
            claude_path: dest,
            turns: turns.len(),
        })
    }

    fn transcript_of(&self, thread: &CodexThread<'_>) -> Result<CodexTranscript> {
        let origin = self.index.origin(thread);
        Ok(CodexTranscript {
            name: names::thread_names(&self.home).remove(thread.thread_id),
            cwd: origin.cwd.clone().or_else(|| thread.head.cwd.clone()),
            entries: self.index.read_entries(thread)?,
        })
    }
}

pub fn list_codex_sessions() -> Vec<CodexSession> {
    match (CodexStore::open(), crate::discovery::cli_projects_dir()) {
        (Some(store), Some(projects)) => store.sessions(&projects),
        _ => Vec::new(),
    }
}

fn status_of(thread_id: &str, migrated: bool, imports: &HashMap<String, String>) -> CodexStatus {
    if migrated {
        return CodexStatus::Migrated;
    }
    match imports.get(thread_id) {
        // 迁移产物记录（source stem == threadId）不算 Claude→Codex 导入；
        // 产物已被删除时回到 NativeOnly，允许重新迁移
        Some(source) if Path::new(source).file_stem().is_some_and(|s| s.to_string_lossy() == thread_id) => {
            CodexStatus::NativeOnly
        }
        Some(source) if Path::new(source).is_file() => CodexStatus::Mirror,
        Some(_) => CodexStatus::OrphanMirror,
        None => CodexStatus::NativeOnly,
    }
}

/// 导入记录：imported_thread_id → source_path。
/// 迁移产物记录的 source stem == threadId，与真正的 Claude→Codex 导入互斥。
fn import_sources(codex: &Path) -> HashMap<String, String> {
    let path = codex.join("external_agent_session_imports.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Default::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Default::default();
    };
    let mut map = HashMap::new();
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

/// Codex 带附件时把消息包成 `# Files mentioned by the user:`、每个文件一段 `## <文件名>: <路径>`、
/// 最后 `## My request…:` 接正文，正文可能和这个标题同一行
pub(crate) struct FileMentions<'a> {
    pub files: Vec<&'a str>,
    pub request: &'a str,
}

pub(crate) fn split_file_mentions(text: &str) -> Option<FileMentions<'_>> {
    let body = text.trim_start();
    if !body.starts_with(FILE_MENTIONS_HEADER) {
        return None;
    }
    let mut files = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let Some(heading) = line.trim_end().strip_prefix("## ") else {
            continue;
        };
        if heading.starts_with(REQUEST_HEADING) {
            let after_colon = line.find(':').map_or(line.len(), |colon| colon + 1);
            return Some(FileMentions {
                files,
                request: body[start + after_colon..].trim(),
            });
        }
        let name = heading.split_once(": ").map_or(heading, |(name, _)| name);
        files.push(name.trim_end_matches(':'));
    }
    Some(FileMentions { files, request: "" })
}

/// 列表标题：附件包装只取请求正文，跳过不是用户亲手输入的包装消息
fn title_text(raw: &str) -> Option<String> {
    let text = split_file_mentions(raw).map_or(raw, |mentions| mentions.request).trim();
    if text.is_empty() || TITLE_NOISE_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
        return None;
    }
    let mut title: String = text.chars().take(TITLE_CHARS).collect();
    if text.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    Some(title)
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

impl Turn {
    /// Claude 转录里没有对应的工具调用记录可写，只迁对话正文
    fn from_entry(entry: Entry) -> Option<Self> {
        let (role, text, timestamp) = match entry {
            Entry::User { text, timestamp } => ("user", text, timestamp),
            Entry::Assistant { text, timestamp } => ("assistant", text, timestamp),
            Entry::Tool { .. } => return None,
        };
        Some(Self {
            role,
            text,
            timestamp: timestamp.unwrap_or_else(|| EPOCH_ISO.to_string()),
        })
    }
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
    // Codex 的台账在 Windows 记 \\?\ 前缀的扩展路径，其他平台是普通绝对路径；格式不一致就匹配不上，防循环失效
    let ledger_path = if cfg!(windows) {
        format!("\\\\?\\{}", dest.display().to_string().replace('/', "\\"))
    } else {
        dest.display().to_string()
    };
    let records = value
        .get_mut("records")
        .and_then(|v| v.as_array_mut())
        .context("records 字段缺失")?;
    let sha = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    if let Some(existing) = records.iter_mut().find(|r| {
        r.get("imported_thread_id").and_then(|v| v.as_str()) == Some(thread_id)
            && r.get("source_path").and_then(|v| v.as_str()) == Some(ledger_path.as_str())
    }) {
        existing["content_sha256"] = serde_json::Value::String(sha);
        existing["source_modified_at"] = serde_json::json!((last_secs * 1e9) as u64);
    } else {
        records.push(serde_json::json!({
            "source_path": ledger_path,
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

#[cfg(test)]
mod tests {
    use super::rollout::fixtures::*;
    use super::*;
    use serde_json::json;

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

    /// 分页对话：压缩的首页在分叉点之后还有一段废弃分支，续页接着分叉点往下写
    fn setup_paginated(dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let codex = dir.join(".codex");
        let sessions = codex.join("sessions");
        let projects = dir.join("projects");
        fs::create_dir_all(&projects).unwrap();
        let base = vec![
            meta(THREAD, json!({"thread_source": "user"})),
            user("2026-10-21T09:00:01.000Z", "第一问"),
            agent("2026-10-21T09:00:02.000Z", "第一答"),
            user("2026-10-21T09:00:03.000Z", "编辑前的提问"),
        ];
        let first = write_page(&sessions, THREAD, None, &base, true);
        let head = write_page(
            &sessions,
            THREAD,
            Some(PAGE_2),
            &[
                meta(THREAD, history_base(THREAD, offset_after(&base, 3))),
                user("2026-10-21T09:01:00.000Z", "编辑后的提问"),
            ],
            false,
        );
        write_page(
            &sessions,
            SUBAGENT,
            None,
            &[meta(SUBAGENT, json!({"thread_source": "subagent"})), user("t", "子代理任务")],
            false,
        );
        (codex, projects, first, head)
    }

    #[test]
    fn legacy_rollout_lists_with_title_cwd_and_originator() {
        let dir = tempfile::tempdir().unwrap();
        let tid = "019dd272-52ff-76b3-9814-4e3c1c534dd0";
        let (codex, projects, rollout) = setup(dir.path(), tid);
        let sessions = CodexStore::open_at(&codex).sessions(&projects);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.thread_id, tid);
        assert_eq!(session.rollout_path, rollout);
        assert_eq!(session.cwd.as_deref(), Some("E:\\Proj\\demo"));
        assert_eq!(session.originator.as_deref(), Some("Codex Desktop"));
        // 没有请求正文的附件包装被跳过
        assert_eq!(session.title.as_deref(), Some("帮我看看这个项目"));
        assert_eq!(session.status, CodexStatus::NativeOnly);
    }

    #[test]
    fn migrate_writes_claude_transcript_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let tid = "019dd272-52ff-76b3-9814-4e3c1c534dd0";
        let (codex, projects, _) = setup(dir.path(), tid);
        let store = CodexStore::open_at(&codex);

        let outcome = store.migrate(&projects, tid).unwrap();
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
        let again = store.migrate(&projects, tid).unwrap();
        assert!(matches!(again, MigrateOutcome::AlreadyMigrated { .. }));

        // 状态判定：产物存在 → Migrated；删除产物后 → NativeOnly（可重迁）
        assert_eq!(store.sessions(&projects)[0].status, CodexStatus::Migrated);
        fs::remove_file(&claude_path).unwrap();
        assert_eq!(store.sessions(&projects)[0].status, CodexStatus::NativeOnly);
    }

    #[test]
    fn paginated_thread_lists_once_with_the_codex_name_and_hides_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let (codex, projects, first, head) = setup_paginated(dir.path());
        fs::write(
            codex.join("session_index.jsonl"),
            json!({"id": THREAD, "thread_name": "Locate blog project"}).to_string(),
        )
        .unwrap();

        let sessions = CodexStore::open_at(&codex).sessions(&projects);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.thread_id, THREAD);
        assert_eq!(session.title.as_deref(), Some("Locate blog project"));
        assert_eq!(session.rollout_path, head);
        assert_eq!(
            session.size_bytes,
            fs::metadata(&first).unwrap().len() + fs::metadata(&head).unwrap().len()
        );
        assert_eq!(session.status, CodexStatus::NativeOnly);
    }

    #[test]
    fn migrating_a_paginated_thread_follows_the_page_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (codex, projects, _, _) = setup_paginated(dir.path());

        let outcome = CodexStore::open_at(&codex).migrate(&projects, THREAD).unwrap();
        let MigrateOutcome::Migrated { claude_path, turns } = outcome else {
            panic!("expected Migrated");
        };
        assert_eq!(turns, 3);
        assert!(claude_path.ends_with(format!("E--Proj-demo/{THREAD}.jsonl")));
        let lines: Vec<serde_json::Value> = fs::read_to_string(&claude_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let bodies: Vec<String> = lines
            .iter()
            .map(|line| {
                line["message"]["content"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| line["message"]["content"][0]["text"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(bodies, ["第一问", "第一答", "编辑后的提问"]);
        assert_eq!(lines[2]["timestamp"], "2026-10-21T09:01:00.000Z");
        assert_eq!(lines[2]["sessionId"], THREAD);
    }

    #[test]
    fn unknown_or_malformed_threads_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (codex, projects, _, _) = setup_paginated(dir.path());
        let store = CodexStore::open_at(&codex);
        assert!(store.migrate(&projects, "../../etc/passwd").is_err());
        assert!(store.migrate(&projects, PAGE_2).is_err());
        assert!(store.transcript("01a00000-0000-7000-8000-000000000000").is_err());
        assert!(fs::read_dir(&projects).unwrap().next().is_none());
    }

    #[test]
    fn title_falls_back_to_the_request_inside_file_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join(".codex");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&projects).unwrap();
        write_page(
            &codex.join("sessions"),
            THREAD,
            None,
            &[
                meta(THREAD, json!({})),
                user("t", "# Files mentioned by the user:\n\n## a.png: C:\\x\\a.png\n\n## My request for Codex:\n帮我看图\n"),
            ],
            false,
        );
        let sessions = CodexStore::open_at(&codex).sessions(&projects);
        assert_eq!(sessions[0].title.as_deref(), Some("帮我看图"));
    }

    #[test]
    fn split_file_mentions_handles_both_heading_styles() {
        let next_line = split_file_mentions(
            "# Files mentioned by the user:\n\n## a.png: C:\\x\\a.png\n\n## My request for Codex:\n帮我看图\n第二行\n",
        )
        .unwrap();
        assert_eq!(next_line.files, ["a.png"]);
        assert_eq!(next_line.request, "帮我看图\n第二行");

        let same_line = split_file_mentions(
            "# Files mentioned by the user:\n\n## b.json: /tmp/b.json\n## c.py:\n\n## My request: 内联请求\n",
        )
        .unwrap();
        assert_eq!(same_line.files, ["b.json", "c.py"]);
        assert_eq!(same_line.request, "内联请求");

        let without_request = split_file_mentions("# Files mentioned by the user\n附件注入").unwrap();
        assert!(without_request.files.is_empty());
        assert!(without_request.request.is_empty());
        assert!(split_file_mentions("普通消息").is_none());
    }

    #[test]
    fn title_skips_wrapped_messages_and_clips_long_text() {
        assert_eq!(title_text("<task-notification>done</task-notification>"), None);
        assert_eq!(
            title_text("This session is being continued from a previous conversation that ran out of context."),
            None
        );
        assert_eq!(title_text("  帮我看看  ").as_deref(), Some("帮我看看"));
        let long = "字".repeat(TITLE_CHARS + 5);
        let clipped = title_text(&long).unwrap();
        assert_eq!(clipped.chars().count(), TITLE_CHARS + 1);
        assert!(clipped.ends_with('…'));
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

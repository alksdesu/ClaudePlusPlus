//! 会话内容只读预览：Claude jsonl 与 Codex rollout 提取为统一的消息流。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

const MAX_MESSAGES: usize = 300;
const MAX_CHARS: usize = 4000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewMessage {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPreview {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub total_messages: usize,
    pub truncated: bool,
    pub messages: Vec<PreviewMessage>,
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(MAX_CHARS).collect();
    clipped.push_str("\n…（本条已截断）");
    clipped
}

fn finish(title: Option<String>, cwd: Option<String>, mut messages: Vec<PreviewMessage>) -> SessionPreview {
    let total_messages = messages.len();
    let truncated = total_messages > MAX_MESSAGES;
    if truncated {
        // 尾部是最新进展，保留尾部截掉中段之前的
        messages.drain(..total_messages - MAX_MESSAGES);
    }
    SessionPreview {
        title,
        cwd,
        total_messages,
        truncated,
        messages,
    }
}

fn is_noise_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.is_empty()
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("Caveat:")
        || trimmed.starts_with("# Files mentioned by the user")
        || trimmed.starts_with("<user_instructions>")
        || trimmed.starts_with("<environment_context>")
}

/// 相邻工具调用聚合为一条 role=tool 的摘要行（Desktop 的 "Ran N commands" 风格）
struct ToolRun {
    names: Vec<String>,
}

impl ToolRun {
    fn new() -> Self {
        Self { names: Vec::new() }
    }

    fn push(&mut self, name: &str) {
        self.names.push(name.to_string());
    }

    fn flush(&mut self, messages: &mut Vec<PreviewMessage>) {
        if self.names.is_empty() {
            return;
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        for name in self.names.drain(..) {
            match counts.iter_mut().find(|(n, _)| *n == name) {
                Some((_, c)) => *c += 1,
                None => counts.push((name, 1)),
            }
        }
        let text = counts
            .iter()
            .map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.clone() })
            .collect::<Vec<_>>()
            .join(" · ");
        messages.push(PreviewMessage {
            role: "tool".into(),
            text,
            timestamp: None,
        });
    }
}

/// Claude 转录：user/assistant 文本为对话主干，工具调用聚合成摘要行
pub fn preview_claude_jsonl(path: &Path) -> Result<SessionPreview> {
    let text = fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
    let mut cwd = None;
    let mut title = None;
    let mut messages = Vec::new();
    let mut tools = ToolRun::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(|v| v.as_str()).map(String::from);
        }
        let role = match value.get("type").and_then(|v| v.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if value.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(message) = value.get("message") else { continue };
        let mut text_parts = Vec::new();
        let mut own_tools: Vec<String> = Vec::new();
        match message.get("content") {
            Some(serde_json::Value::String(s)) => {
                if !is_noise_text(s) {
                    text_parts.push(s.clone());
                }
            }
            Some(serde_json::Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                if !is_noise_text(t) {
                                    text_parts.push(t.to_string());
                                }
                            }
                        }
                        Some("tool_use") => {
                            own_tools.push(
                                block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string(),
                            );
                        }
                        // tool_result 是工具回显，预览只保留对话叙事
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let body = text_parts.join("\n");
        if !body.trim().is_empty() {
            // 正文结束上一段工具循环；本条自带的工具发生在正文之后
            tools.flush(&mut messages);
            if title.is_none() && role == "user" {
                title = Some(body.chars().take(60).collect());
            }
            messages.push(PreviewMessage {
                role: role.to_string(),
                text: clip(&body),
                timestamp: value.get("timestamp").and_then(|v| v.as_str()).map(String::from),
            });
        }
        for name in own_tools {
            tools.push(&name);
        }
    }
    tools.flush(&mut messages);
    Ok(finish(title, cwd, messages))
}

/// Codex rollout：user/agent 消息为主干，function/custom tool 调用聚合成摘要行
pub fn preview_codex_rollout(path: &Path) -> Result<SessionPreview> {
    let text = fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
    let mut cwd = None;
    let mut title = None;
    let mut messages = Vec::new();
    let mut tools = ToolRun::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else { continue };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                if cwd.is_none() {
                    cwd = payload.get("cwd").and_then(|v| v.as_str()).map(String::from);
                }
            }
            Some("event_msg") => {
                let role = match payload.get("type").and_then(|v| v.as_str()) {
                    Some("user_message") => "user",
                    Some("agent_message") => "assistant",
                    _ => continue,
                };
                let Some(message) = payload.get("message").and_then(|v| v.as_str()) else {
                    continue;
                };
                if message.trim().is_empty() || is_noise_text(message) {
                    continue;
                }
                tools.flush(&mut messages);
                if title.is_none() && role == "user" {
                    title = Some(message.trim().chars().take(60).collect());
                }
                messages.push(PreviewMessage {
                    role: role.into(),
                    text: clip(message),
                    timestamp: value.get("timestamp").and_then(|v| v.as_str()).map(String::from),
                });
            }
            Some("response_item") => {
                if matches!(
                    payload.get("type").and_then(|v| v.as_str()),
                    Some("function_call" | "custom_tool_call")
                ) {
                    tools.push(payload.get("name").and_then(|v| v.as_str()).unwrap_or("?"));
                }
            }
            _ => {}
        }
    }
    tools.flush(&mut messages);
    Ok(finish(title, cwd, messages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_preview_extracts_dialogue_and_folds_tools() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"user","cwd":"E:\\P","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"你好"}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"我来看看"},{"type":"tool_use","name":"Read","input":{}}]}}"#,
            r#"{"type":"user","timestamp":"2026-08-01T10:00:06Z","message":{"role":"user","content":[{"type":"tool_result","content":"file data"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:07Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:08Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-08-01T10:00:09Z","message":{"role":"assistant","content":[{"type":"text","text":"搞定了"}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"子代理消息"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let preview = preview_claude_jsonl(&path).unwrap();
        assert_eq!(preview.cwd.as_deref(), Some("E:\\P"));
        assert_eq!(preview.title.as_deref(), Some("你好"));
        // 用户正文 → 助手正文 → 工具摘要（Read ×2 · Bash）→ 助手正文
        let roles: Vec<&str> = preview.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        assert_eq!(preview.messages[1].text, "我来看看");
        assert_eq!(preview.messages[2].text, "Read ×2 · Bash");
        assert_eq!(preview.messages[3].text, "搞定了");
        assert!(!preview.truncated);
    }

    #[test]
    fn codex_preview_aggregates_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let lines = [
            r#"{"timestamp":"2026-08-01T10:00:00Z","type":"session_meta","payload":{"cwd":"F:\\P"}}"#,
            r#"{"timestamp":"2026-08-01T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"跑一下测试"}}"#,
            r#"{"timestamp":"2026-08-01T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"shell"}}"#,
            r#"{"timestamp":"2026-08-01T10:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch"}}"#,
            r#"{"timestamp":"2026-08-01T10:00:04Z","type":"event_msg","payload":{"type":"agent_message","message":"测试全过"}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let preview = preview_codex_rollout(&path).unwrap();
        let roles: Vec<&str> = preview.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "tool", "assistant"]);
        assert_eq!(preview.messages[1].text, "shell · apply_patch");
        assert_eq!(preview.cwd.as_deref(), Some("F:\\P"));
    }

    #[test]
    fn long_message_is_clipped() {
        let long = "字".repeat(MAX_CHARS + 100);
        let clipped = clip(&long);
        assert!(clipped.chars().count() < MAX_CHARS + 20);
        assert!(clipped.ends_with("…（本条已截断）"));
    }

    #[test]
    fn overflow_keeps_tail() {
        let messages: Vec<PreviewMessage> = (0..MAX_MESSAGES + 10)
            .map(|i| PreviewMessage {
                role: "user".into(),
                text: format!("m{i}"),
                timestamp: None,
            })
            .collect();
        let preview = finish(None, None, messages);
        assert!(preview.truncated);
        assert_eq!(preview.messages.len(), MAX_MESSAGES);
        assert_eq!(preview.messages.last().unwrap().text, format!("m{}", MAX_MESSAGES + 9));
    }
}

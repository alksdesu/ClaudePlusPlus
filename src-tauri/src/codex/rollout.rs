//! Codex rollout 存储：单文件旧格式与分页新格式统一读成对话条目。
//! 分页格式下一个对话拆成多页 `rollout-<时间>-<对话>_<页>.jsonl`，旧页会被压成 `.jsonl.zst`，
//! 每页的 history_base 指向上一页和分叉点。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use serde_json::Value;

use crate::discovery::is_uuid;

const PLAIN_EXT: &str = ".jsonl";
const ZST_EXT: &str = ".jsonl.zst";
const UUID_LEN: usize = 36;
// session_meta 首行带完整 base_instructions，实测二三十 KB
const META_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const META_SCAN_LINES: usize = 8;
const IMAGE_PLACEHOLDER: &str = "[图片]";

/// 对话里的一条可见内容；工具只留名字，预览聚合成摘要、迁移时跳过
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User { text: String, timestamp: Option<String> },
    Assistant { text: String, timestamp: Option<String> },
    Tool { name: String },
}

/// 字段名叫 thread_id，实际存的是上一页的页 ID；偏移按解压后的字节计
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryBase {
    pub page_id: String,
    pub end_byte_offset: u64,
}

#[derive(Debug, Clone)]
pub struct RolloutPage {
    pub path: PathBuf,
    pub page_id: String,
    pub thread_id: String,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub subagent: bool,
    pub base: Option<HistoryBase>,
    pub size_bytes: u64,
    pub modified_ms: u64,
}

impl RolloutPage {
    fn compressed(&self) -> bool {
        self.path.to_string_lossy().ends_with(ZST_EXT)
    }
}

pub struct CodexThread<'a> {
    pub thread_id: &'a str,
    /// Codex 里当前看到的那一页，回退或编辑重发后是最新分出的页
    pub head: &'a RolloutPage,
    pub pages: Vec<&'a RolloutPage>,
}

impl CodexThread<'_> {
    pub fn subagent(&self) -> bool {
        self.head.subagent
    }

    pub fn size_bytes(&self) -> u64 {
        self.pages.iter().map(|page| page.size_bytes).sum()
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.pages.iter().map(|page| page.modified_ms).max().unwrap_or(0)
    }
}

struct ChainLink<'a> {
    page: &'a RolloutPage,
    /// 子页从这里分叉，之后的部分是编辑重发或回退前的废弃分支
    end: Option<u64>,
}

pub struct RolloutIndex {
    pages: Vec<RolloutPage>,
    by_page: HashMap<String, usize>,
}

impl RolloutIndex {
    pub fn scan(sessions: &Path) -> Self {
        let mut files = Vec::new();
        collect_rollouts(sessions, &mut files);
        files.sort();
        let mut pages: Vec<RolloutPage> = Vec::new();
        let mut by_page: HashMap<String, usize> = HashMap::new();
        for path in files {
            let Some(page) = read_page(path) else { continue };
            match by_page.get(&page.page_id) {
                // 压缩进行中明文与压缩包并存，明文是完整的
                Some(&existing) => {
                    if pages[existing].compressed() && !page.compressed() {
                        pages[existing] = page;
                    }
                }
                None => {
                    by_page.insert(page.page_id.clone(), pages.len());
                    pages.push(page);
                }
            }
        }
        Self { pages, by_page }
    }

    pub fn threads(&self) -> Vec<CodexThread<'_>> {
        let mut grouped: HashMap<&str, Vec<&RolloutPage>> = HashMap::new();
        for page in &self.pages {
            grouped.entry(page.thread_id.as_str()).or_default().push(page);
        }
        grouped
            .into_iter()
            .filter_map(|(thread_id, pages)| {
                Some(CodexThread {
                    thread_id,
                    head: head_of(&pages)?,
                    pages,
                })
            })
            .collect()
    }

    pub fn thread(&self, thread_id: &str) -> Option<CodexThread<'_>> {
        let pages: Vec<&RolloutPage> = self.pages.iter().filter(|page| page.thread_id == thread_id).collect();
        Some(CodexThread {
            thread_id: pages.first()?.thread_id.as_str(),
            head: head_of(&pages)?,
            pages,
        })
    }

    /// 链上最早还在的那一页，对话的 cwd 与首条输入在这里
    pub fn origin<'a>(&'a self, thread: &CodexThread<'a>) -> &'a RolloutPage {
        self.chain(thread.head).first().map_or(thread.head, |link| link.page)
    }

    /// 沿 history_base 从首页拼到 head，每一页只取到子页的分叉点为止
    pub fn read_entries(&self, thread: &CodexThread<'_>) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for link in self.chain(thread.head) {
            let reader = open_page(&link.page.path)?;
            let page = page_entries(reader.take(link.end.unwrap_or(u64::MAX)))
                .with_context(|| format!("读取 {}", link.page.path.display()))?;
            entries.extend(page);
        }
        Ok(entries)
    }

    /// 只读最早一页的开头，找第一条 `accept` 认可的用户输入
    pub fn first_user_text(
        &self,
        thread: &CodexThread<'_>,
        budget: u64,
        accept: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let chain = self.chain(thread.head);
        let root = chain.first()?;
        let reader = open_page(&root.page.path).ok()?;
        let limit = root.end.map_or(budget, |end| end.min(budget));
        page_entries(reader.take(limit))
            .ok()?
            .into_iter()
            .find_map(|entry| match entry {
                Entry::User { text, .. } => accept(&text),
                _ => None,
            })
    }

    fn chain<'a>(&'a self, head: &'a RolloutPage) -> Vec<ChainLink<'a>> {
        let mut links = vec![ChainLink { page: head, end: None }];
        let mut seen = HashSet::from([head.page_id.as_str()]);
        let mut current = head;
        while let Some(base) = &current.base {
            let Some(parent) = self.by_page.get(&base.page_id).map(|&index| &self.pages[index]) else {
                break;
            };
            if !seen.insert(parent.page_id.as_str()) {
                break;
            }
            links.push(ChainLink {
                page: parent,
                end: Some(base.end_byte_offset),
            });
            current = parent;
        }
        links.reverse();
        links
    }
}

/// 没被别的页当作上一页的就是末端；同一页分出多页时取最新的，成环时退回全体里最新的
fn head_of<'a>(pages: &[&'a RolloutPage]) -> Option<&'a RolloutPage> {
    let referenced: HashSet<&str> = pages
        .iter()
        .filter_map(|page| page.base.as_ref())
        .map(|base| base.page_id.as_str())
        .collect();
    let newest = |candidates: &mut dyn Iterator<Item = &'a RolloutPage>| {
        candidates.max_by(|a, b| (a.modified_ms, &a.page_id).cmp(&(b.modified_ms, &b.page_id)))
    };
    newest(&mut pages.iter().copied().filter(|page| !referenced.contains(page.page_id.as_str())))
        .or_else(|| newest(&mut pages.iter().copied()))
}

fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_rollouts(&path, out);
        } else {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(PLAIN_EXT) || name.ends_with(ZST_EXT) {
                out.push(path);
            }
        }
    }
}

/// 文件名里按出现顺序的 UUID：旧格式只有对话 ID，分页格式是 对话_页
fn uuids_in(name: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut start = 0;
    while start + UUID_LEN <= name.len() {
        // 起点落在多字节字符中间时 get 返回 None，不会切坏字符串
        match name.get(start..start + UUID_LEN).filter(|window| is_uuid(window)) {
            Some(window) => {
                found.push(window);
                start += UUID_LEN;
            }
            None => start += 1,
        }
    }
    found
}

fn read_page(path: PathBuf) -> Option<RolloutPage> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let ids = uuids_in(&name);
    let page_id = ids.last()?.to_string();
    let meta = read_meta(&path).unwrap_or(Value::Null);
    let text = |key: &str| meta.get(key).and_then(Value::as_str);
    let thread_id = text("id")
        .or_else(|| text("session_id"))
        .filter(|id| is_uuid(id))
        .or_else(|| ids.first().copied())?
        .to_string();
    let base = meta.get("history_base").and_then(|base| {
        Some(HistoryBase {
            page_id: base.get("thread_id")?.as_str().filter(|id| is_uuid(id))?.to_string(),
            end_byte_offset: base.get("end_byte_offset")?.as_u64()?,
        })
    });
    let subagent = text("thread_source") == Some("subagent")
        || meta.get("source").and_then(|source| source.get("subagent")).is_some();
    let file_meta = fs::metadata(&path).ok();
    let modified_ms = file_meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(RolloutPage {
        page_id,
        thread_id,
        cwd: text("cwd").map(String::from),
        originator: text("originator").map(String::from),
        subagent,
        base,
        size_bytes: file_meta.map(|m| m.len()).unwrap_or(0),
        modified_ms,
        path,
    })
}

fn read_meta(path: &Path) -> Option<Value> {
    let reader = open_page(path).ok()?;
    let mut meta = None;
    let mut seen = 0;
    for_each_line(reader.take(META_SCAN_BYTES), |line| {
        seen += 1;
        if line.get("type").and_then(Value::as_str) == Some("session_meta") {
            meta = line.get("payload").cloned();
            return false;
        }
        seen < META_SCAN_LINES
    })
    .ok()?;
    meta
}

/// 解压后的字节流，分页链的截断偏移按它计
pub fn open_page(path: &Path) -> Result<Box<dyn Read>> {
    let file = fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    if path.to_string_lossy().ends_with(ZST_EXT) {
        let frames = ZstdFrames::new(BufReader::new(file))
            .with_context(|| format!("解压 {}", path.display()))?;
        Ok(Box::new(frames))
    } else {
        Ok(Box::new(file))
    }
}

type FrameReader = StreamingDecoder<BufReader<fs::File>, FrameDecoder>;

/// zstd 允许多帧首尾相接，ruzstd 一个解码器只解一帧
struct ZstdFrames {
    decoder: Option<FrameReader>,
}

impl ZstdFrames {
    fn new(source: BufReader<fs::File>) -> io::Result<Self> {
        Ok(Self {
            decoder: Some(StreamingDecoder::new(source).map_err(invalid_data)?),
        })
    }
}

impl Read for ZstdFrames {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(decoder) = self.decoder.as_mut() else { return Ok(0) };
            let read = decoder.read(buf)?;
            if read > 0 || buf.is_empty() {
                return Ok(read);
            }
            let Some(finished) = self.decoder.take() else { return Ok(0) };
            let mut source = finished.into_inner();
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.decoder = Some(StreamingDecoder::new(source).map_err(invalid_data)?);
        }
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// 逐行解析，坏行跳过；回调返回 false 时提前结束
fn for_each_line(reader: impl Read, mut visit: impl FnMut(&Value) -> bool) -> io::Result<()> {
    let mut lines = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if lines.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buf) else { continue };
        if !visit(&value) {
            return Ok(());
        }
    }
}

fn page_entries(reader: impl Read) -> io::Result<Vec<Entry>> {
    let mut page = PageEntries::default();
    for_each_line(reader, |line| {
        page.push(line);
        true
    })?;
    Ok(page.finish())
}

/// 新格式用 item_completed 记录 Codex 界面里的条目，旧格式用 event_msg；同一页只认一种，免得重复
#[derive(Default)]
struct PageEntries {
    items: Vec<Entry>,
    legacy: Vec<Entry>,
    has_items: bool,
}

impl PageEntries {
    fn push(&mut self, line: &Value) {
        let Some(payload) = line.get("payload") else { return };
        let timestamp = || line.get("timestamp").and_then(Value::as_str).map(String::from);
        let kind = payload.get("type").and_then(Value::as_str);
        match (line.get("type").and_then(Value::as_str), kind) {
            (Some("event_msg"), Some("item_completed")) => {
                self.has_items = true;
                if let Some(entry) = payload.get("item").and_then(|item| item_entry(item, timestamp())) {
                    self.items.push(entry);
                }
            }
            (Some("event_msg"), Some(role @ ("user_message" | "agent_message"))) => {
                let Some(text) = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                else {
                    return;
                };
                let text = text.to_string();
                self.legacy.push(if role == "user_message" {
                    Entry::User { text, timestamp: timestamp() }
                } else {
                    Entry::Assistant { text, timestamp: timestamp() }
                });
            }
            (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
                let name = payload.get("name").and_then(Value::as_str).unwrap_or("?");
                self.legacy.push(Entry::Tool { name: name.to_string() });
            }
            _ => {}
        }
    }

    fn finish(self) -> Vec<Entry> {
        if self.has_items {
            self.items
        } else {
            self.legacy
        }
    }
}

fn item_entry(item: &Value, timestamp: Option<String>) -> Option<Entry> {
    let field = |key: &str| item.get(key).and_then(Value::as_str);
    let tool = |name: &str| Some(Entry::Tool { name: name.to_string() });
    let joined = |parts: Vec<String>| {
        let text = parts.join("\n");
        (!text.trim().is_empty()).then_some(text)
    };
    let content = || item.get("content").and_then(Value::as_array).into_iter().flatten();
    match field("type")? {
        "UserMessage" => joined(content().filter_map(user_part_text).collect())
            .map(|text| Entry::User { text, timestamp }),
        "AgentMessage" => joined(
            content()
                .filter_map(|part| part.get("text")?.as_str().map(String::from))
                .collect(),
        )
        .map(|text| Entry::Assistant { text, timestamp }),
        // 推理、子代理心跳、上下文压缩标记都不是对话内容
        "Reasoning" | "SubAgentActivity" | "ContextCompaction" => None,
        // 与旧格式的工具名对齐，两种格式在预览里聚合成一样的摘要
        "CommandExecution" => tool("shell"),
        "FileChange" => tool("apply_patch"),
        "ImageView" => tool("view_image"),
        "McpToolCall" => match (field("server"), field("tool")) {
            (Some(server), Some(name)) => tool(&format!("{server}.{name}")),
            (None, Some(name)) => tool(name),
            _ => tool("mcp"),
        },
        "Extension" => tool(field("kind").unwrap_or("extension")),
        "CollabAgentToolCall" => tool(field("tool").unwrap_or("agent")),
        other => tool(other),
    }
}

fn user_part_text(part: &Value) -> Option<String> {
    match part.get("type")?.as_str()? {
        "text" => part.get("text")?.as_str().map(String::from),
        "local_image" | "image" => Some(IMAGE_PLACEHOLDER.to_string()),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};

    pub const THREAD: &str = "01a14955-020a-7380-90fe-ed31bb2feb2d";
    pub const PAGE_2: &str = "01a14977-13a4-7000-8000-000000000002";
    pub const PAGE_3: &str = "01a14977-8dfb-7000-8000-000000000003";
    pub const SUBAGENT: &str = "01a1495b-1d92-7000-8000-00000000000a";

    pub fn meta(thread: &str, extra: Value) -> String {
        let mut payload = json!({
            "id": thread,
            "session_id": thread,
            "cwd": "E:\\Proj\\demo",
            "originator": "Codex Desktop",
            "history_mode": "paginated"
        });
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        json!({"timestamp": "2026-10-21T09:00:00.000Z", "type": "session_meta", "payload": payload}).to_string()
    }

    pub fn item(ts: &str, item: Value) -> String {
        json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "item_completed", "item": item}}).to_string()
    }

    pub fn user(ts: &str, text: &str) -> String {
        item(ts, json!({"type": "UserMessage", "id": "u", "content": [{"type": "text", "text": text, "text_elements": []}]}))
    }

    pub fn agent(ts: &str, text: &str) -> String {
        item(ts, json!({"type": "AgentMessage", "id": "a", "content": [{"type": "text", "text": text}]}))
    }

    pub fn history_base(page: &str, end: u64) -> Value {
        json!({"history_base": {"thread_id": page, "end_ordinal_exclusive": 0, "end_byte_offset": end}})
    }

    pub fn page_body(lines: &[String]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    /// 上一页前 `kept` 行解压后的字节数，即子页的分叉点
    pub fn offset_after(lines: &[String], kept: usize) -> u64 {
        page_body(&lines[..kept]).len() as u64
    }

    /// 按 Codex 的布局落一页：日期目录下 `rollout-<时间>-<对话>[_<页>].jsonl[.zst]`
    pub fn write_page(sessions: &Path, thread: &str, page: Option<&str>, lines: &[String], compressed: bool) -> PathBuf {
        let dir = sessions.join("2026").join("10").join("21");
        fs::create_dir_all(&dir).unwrap();
        let stem = match page {
            Some(page) => format!("rollout-2026-10-21T09-00-00-{thread}_{page}"),
            None => format!("rollout-2026-10-21T09-00-00-{thread}"),
        };
        let body = page_body(lines);
        if compressed {
            let path = dir.join(format!("{stem}.jsonl.zst"));
            let packed = ruzstd::encoding::compress_to_vec(body.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
            fs::write(&path, packed).unwrap();
            path
        } else {
            let path = dir.join(format!("{stem}.jsonl"));
            fs::write(&path, body).unwrap();
            path
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    fn texts(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Entry::User { text, .. } => format!("user:{text}"),
                Entry::Assistant { text, .. } => format!("assistant:{text}"),
                Entry::Tool { name } => format!("tool:{name}"),
            })
            .collect()
    }

    fn set_mtime(path: &Path, secs: i64) {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(secs, 0)).unwrap();
    }

    #[test]
    fn uuids_in_handles_both_layouts_and_non_ascii() {
        assert_eq!(
            uuids_in("rollout-2026-10-21T17-18-54-01a0f37b-7d94-7fa0-a340-09d6447ee7ed_01a15dc1-a43e-7203-85ed-076ec47c0fbb.jsonl"),
            ["01a0f37b-7d94-7fa0-a340-09d6447ee7ed", "01a15dc1-a43e-7203-85ed-076ec47c0fbb"]
        );
        assert_eq!(
            uuids_in("rollout-2026-10-21T03-56-52-01a15ae3-5e75-71a2-80fe-31ce4fea4056.jsonl.zst"),
            ["01a15ae3-5e75-71a2-80fe-31ce4fea4056"]
        );
        // 字节窗口会落进多字节字符中间，必须安全跳过
        let tricky = format!("我{}.jsonl", "a".repeat(35));
        assert!(uuids_in(&tricky).is_empty());
        assert_eq!(uuids_in("测试-01a15ae3-5e75-71a2-80fe-31ce4fea4056.jsonl"), ["01a15ae3-5e75-71a2-80fe-31ce4fea4056"]);
    }

    #[test]
    fn non_ascii_file_names_are_skipped_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(format!("我{}.jsonl", "a".repeat(35))), "{}\n").unwrap();
        let index = RolloutIndex::scan(dir.path());
        assert!(index.threads().is_empty());
    }

    #[test]
    fn paginated_thread_reads_each_base_page_up_to_the_fork_point() {
        let dir = tempfile::tempdir().unwrap();
        let base = vec![
            meta(THREAD, json!({})),
            user("2026-10-21T09:00:01Z", "第一问"),
            agent("2026-10-21T09:00:02Z", "第一答"),
            user("2026-10-21T09:00:03Z", "编辑前的提问"),
            agent("2026-10-21T09:00:04Z", "废弃分支"),
        ];
        let child = vec![
            meta(THREAD, history_base(THREAD, offset_after(&base, 3))),
            user("2026-10-21T09:01:00Z", "编辑后的提问"),
            agent("2026-10-21T09:01:01Z", "新的回答"),
        ];
        write_page(dir.path(), THREAD, None, &base, true);
        let head = write_page(dir.path(), THREAD, Some(PAGE_2), &child, false);

        let index = RolloutIndex::scan(dir.path());
        let threads = index.threads();
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.thread_id, THREAD);
        assert_eq!(thread.head.path, head);
        assert_eq!(index.origin(thread).page_id, THREAD);
        assert_eq!(
            texts(&index.read_entries(thread).unwrap()),
            ["user:第一问", "assistant:第一答", "user:编辑后的提问", "assistant:新的回答"]
        );
    }

    #[test]
    fn newest_leaf_is_the_head_after_rewinding_twice() {
        let dir = tempfile::tempdir().unwrap();
        let base = vec![meta(THREAD, json!({})), user("t", "起点"), agent("t", "回答")];
        let fork = offset_after(&base, 3);
        write_page(dir.path(), THREAD, None, &base, false);
        let older = write_page(
            dir.path(),
            THREAD,
            Some(PAGE_2),
            &[meta(THREAD, history_base(THREAD, fork)), user("t", "先试的分支")],
            false,
        );
        let newer = write_page(
            dir.path(),
            THREAD,
            Some(PAGE_3),
            &[meta(THREAD, history_base(THREAD, fork)), user("t", "后来的分支")],
            false,
        );
        set_mtime(&older, 1_790_000_000);
        set_mtime(&newer, 1_790_000_100);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(thread.head.path, newer);
        assert_eq!(thread.pages.len(), 3);
        assert_eq!(
            texts(&index.read_entries(&thread).unwrap()),
            ["user:起点", "assistant:回答", "user:后来的分支"]
        );
    }

    #[test]
    fn plain_copy_wins_while_compression_is_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let plain = write_page(dir.path(), THREAD, None, &[meta(THREAD, json!({})), user("t", "明文")], false);
        write_page(dir.path(), THREAD, None, &[meta(THREAD, json!({})), user("t", "压缩包")], true);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(thread.pages.len(), 1);
        assert_eq!(thread.head.path, plain);
        assert_eq!(texts(&index.read_entries(&thread).unwrap()), ["user:明文"]);
    }

    #[test]
    fn thread_id_comes_from_session_meta_not_the_page_id() {
        let dir = tempfile::tempdir().unwrap();
        write_page(dir.path(), THREAD, Some(PAGE_2), &[meta(THREAD, json!({})), user("t", "只剩续页")], false);
        // 首行坏掉的页退回文件名里的第一个 UUID
        write_page(dir.path(), SUBAGENT, None, &["{not json".to_string(), user("t", "无元数据")], false);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(thread.head.page_id, PAGE_2);
        assert_eq!(thread.head.cwd.as_deref(), Some("E:\\Proj\\demo"));
        assert!(index.thread(PAGE_2).is_none());
        assert!(index.thread(SUBAGENT).unwrap().head.cwd.is_none());
    }

    #[test]
    fn subagent_threads_are_flagged_by_either_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_page(dir.path(), THREAD, None, &[meta(THREAD, json!({"thread_source": "user", "source": "vscode"}))], false);
        write_page(dir.path(), SUBAGENT, None, &[meta(SUBAGENT, json!({"thread_source": "subagent"}))], false);
        write_page(
            dir.path(),
            PAGE_3,
            None,
            &[meta(PAGE_3, json!({"source": {"subagent": {"thread_spawn": {"parent_thread_id": THREAD}}}}))],
            false,
        );

        let index = RolloutIndex::scan(dir.path());
        assert!(!index.thread(THREAD).unwrap().subagent());
        assert!(index.thread(SUBAGENT).unwrap().subagent());
        assert!(index.thread(PAGE_3).unwrap().subagent());
    }

    #[test]
    fn items_become_entries_with_tool_names_aligned_to_the_legacy_format() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            meta(THREAD, json!({})),
            item("t", json!({"type": "UserMessage", "content": [{"type": "text", "text": "看这张图"}, {"type": "local_image", "path": "C:\\a.png"}]})),
            item("t", json!({"type": "Reasoning", "summary_text": ["想一想"]})),
            item("t", json!({"type": "CommandExecution", "command": "ls"})),
            item("t", json!({"type": "FileChange", "changes": {}})),
            item("t", json!({"type": "McpToolCall", "server": "codex_app", "tool": "read_thread"})),
            item("t", json!({"type": "Extension", "kind": "web.search"})),
            item("t", json!({"type": "CollabAgentToolCall", "tool": "spawn_agent"})),
            item("t", json!({"type": "SubAgentActivity", "kind": "started"})),
            item("t", json!({"type": "TodoList"})),
            agent("t", "看完了"),
            item("t", json!({"type": "AgentMessage", "content": [{"type": "text", "text": "  "}]})),
        ];
        write_page(dir.path(), THREAD, None, &lines, false);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(
            texts(&index.read_entries(&thread).unwrap()),
            [
                "user:看这张图\n[图片]",
                "tool:shell",
                "tool:apply_patch",
                "tool:codex_app.read_thread",
                "tool:web.search",
                "tool:spawn_agent",
                "tool:TodoList",
                "assistant:看完了",
            ]
        );
    }

    #[test]
    fn legacy_events_are_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            meta(THREAD, json!({})),
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"旧格式提问"}}"#.to_string(),
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"shell"}}"#.to_string(),
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"agent_message","message":"旧格式回答"}}"#.to_string(),
        ];
        write_page(dir.path(), THREAD, None, &lines, false);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(
            texts(&index.read_entries(&thread).unwrap()),
            ["user:旧格式提问", "tool:shell", "assistant:旧格式回答"]
        );
    }

    #[test]
    fn items_win_over_legacy_events_on_the_same_page() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            meta(THREAD, json!({})),
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"重复的旧事件"}}"#.to_string(),
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch"}}"#.to_string(),
            user("t", "条目里的提问"),
        ];
        write_page(dir.path(), THREAD, None, &lines, false);

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(texts(&index.read_entries(&thread).unwrap()), ["user:条目里的提问"]);
    }

    #[test]
    fn missing_base_pages_and_cycles_terminate() {
        let dir = tempfile::tempdir().unwrap();
        // 上一页已经不在了：只读得到还在的部分
        write_page(
            dir.path(),
            THREAD,
            Some(PAGE_2),
            &[meta(THREAD, history_base(PAGE_3, 10)), user("t", "孤页")],
            false,
        );
        // 两页互指成环
        write_page(
            dir.path(),
            SUBAGENT,
            Some(SUBAGENT),
            &[meta(SUBAGENT, history_base("01a15000-0000-7000-8000-00000000000b", 1)), user("t", "甲")],
            false,
        );
        write_page(
            dir.path(),
            SUBAGENT,
            Some("01a15000-0000-7000-8000-00000000000b"),
            &[meta(SUBAGENT, history_base(SUBAGENT, 1)), user("t", "乙")],
            false,
        );

        let index = RolloutIndex::scan(dir.path());
        let orphan = index.thread(THREAD).unwrap();
        assert_eq!(texts(&index.read_entries(&orphan).unwrap()), ["user:孤页"]);
        let cyclic = index.thread(SUBAGENT).unwrap();
        assert_eq!(cyclic.pages.len(), 2);
        assert!(index.read_entries(&cyclic).is_ok());
    }

    #[test]
    fn multi_frame_zstd_pages_are_read_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let first = page_body(&[meta(THREAD, json!({})), user("t", "第一帧")]);
        let second = page_body(&[agent("t", "第二帧")]);
        let mut packed = ruzstd::encoding::compress_to_vec(first.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
        packed.extend(ruzstd::encoding::compress_to_vec(second.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest));
        let day = dir.path().join("2026").join("10").join("21");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join(format!("rollout-2026-10-21T09-00-00-{THREAD}.jsonl.zst")), packed).unwrap();

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        assert_eq!(texts(&index.read_entries(&thread).unwrap()), ["user:第一帧", "assistant:第二帧"]);
    }

    #[test]
    fn first_user_text_takes_the_first_accepted_input_of_the_origin_page() {
        let dir = tempfile::tempdir().unwrap();
        let base = vec![
            meta(THREAD, json!({})),
            user("t", "<command-name>/clear</command-name>"),
            user("t", "真正的问题"),
        ];
        write_page(dir.path(), THREAD, None, &base, true);
        write_page(
            dir.path(),
            THREAD,
            Some(PAGE_2),
            &[meta(THREAD, history_base(THREAD, offset_after(&base, 3))), user("t", "续页提问")],
            false,
        );

        let index = RolloutIndex::scan(dir.path());
        let thread = index.thread(THREAD).unwrap();
        let accept = |text: &str| (!text.starts_with('<')).then(|| text.to_string());
        assert_eq!(index.first_user_text(&thread, 1024 * 1024, accept).as_deref(), Some("真正的问题"));
        // 预算只够读到第一条时找不到
        assert_eq!(index.first_user_text(&thread, offset_after(&base, 2), accept), None);
    }
}

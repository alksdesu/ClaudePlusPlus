use anyhow::{ensure, Context, Result};
use regex::Regex;

const TRAILER: &[u8] = b"\n---- Bun! ----\n";
const SNAPSHOT: &str = "type:\"prompt_snapshot\",";
const STAMPED: &str = "type:\"prompt_snapshot\",cppPromptBlock:1,";
pub(super) const MARKER: &str = "/*cpp:prompt-block:1*/";

fn number(data: &[u8], at: usize) -> Result<usize> {
    let bytes = data
        .get(at..at.checked_add(4).context("二进制范围溢出")?)
        .context("二进制截断")?;
    Ok(u32::from_le_bytes(bytes.try_into()?) as usize)
}

fn wide(data: &[u8], at: usize) -> Result<usize> {
    let bytes = data
        .get(at..at.checked_add(8).context("二进制范围溢出")?)
        .context("二进制截断")?;
    usize::try_from(u64::from_le_bytes(bytes.try_into()?)).context("二进制范围溢出")
}

fn put(data: &mut [u8], at: usize, value: usize) -> Result<()> {
    let value = u32::try_from(value).context("二进制范围溢出")?;
    data.get_mut(at..at + 4)
        .context("二进制截断")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

struct Module {
    record: usize,
    source: usize,
    length: usize,
    hash: usize,
}

struct Graph {
    base: usize,
    flags: usize,
    graph_record: usize,
    arena: usize,
    arena_size: usize,
    modules: Vec<Module>,
}

fn graph(data: &[u8]) -> Result<Graph> {
    ensure!(
        data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]),
        "CLI 不是受支持的 64 位 Mach-O"
    );
    let commands_end = 32usize
        .checked_add(number(data, 20)?)
        .context("Mach-O 命令越界")?;
    ensure!(commands_end <= data.len(), "Mach-O 命令越界");
    let mut at = 32;
    let mut section = None;
    for _ in 0..number(data, 16)? {
        let size = number(data, at + 4)?;
        ensure!(size >= 8 && at + size <= commands_end, "Mach-O 命令无效");
        if number(data, at)? == 0x19 {
            ensure!(size >= 72, "Mach-O 段无效");
            let count = number(data, at + 64)?;
            ensure!(count <= (size - 72) / 80, "Mach-O 节表越界");
            for i in 0..count {
                let offset = at + 72 + i * 80;
                if data[offset..offset + 16].split(|b| *b == 0).next() == Some(b"__bun".as_slice())
                {
                    ensure!(section.is_none(), "Bun section 不唯一");
                    section = Some((number(data, offset + 48)?, wide(data, offset + 40)?));
                }
            }
        }
        at += size;
    }
    let (section, length) = section.context("缺少 Bun section")?;
    let end = section
        .checked_add(length)
        .filter(|end| *end <= data.len())
        .context("Bun section 越界")?;
    ensure!(
        length >= 56 && data.get(end - TRAILER.len()..end) == Some(TRAILER),
        "Bun trailer 不匹配"
    );
    ensure!(wide(data, section)? == length - 8, "Bun 长度不匹配");
    let metadata = end - TRAILER.len() - 32;
    let base = metadata
        .checked_sub(wide(data, metadata)?)
        .context("Bun 数据越界")?;
    ensure!(base == section + 8, "Bun 起点不匹配");
    let flags = number(data, metadata + 28)?;
    ensure!(
        matches!(flags, 0x1fff | 0x3fff),
        "Bun 布局未适配或已有其他补丁"
    );
    let table = base + number(data, metadata + 8)?;
    let table_size = number(data, metadata + 12)?;
    ensure!(
        table_size % 52 == 0 && table_size > 0 && table_size / 52 < 100000,
        "Bun 模块表无效"
    );
    let count = table_size / 52;
    ensure!(table + table_size + count * 4 <= metadata, "Bun 模块表越界");
    let mut modules = Vec::new();
    let mut regions = Vec::new();
    for index in 0..count {
        let record = table + index * 52;
        for field in (0..48).step_by(8) {
            let begin = base + number(data, record + field)?;
            let size = number(data, record + field + 4)?;
            ensure!(
                begin <= metadata && size <= metadata - begin,
                "Bun 字段越界"
            );
            if size > 0 {
                regions.push((begin, begin + size));
            }
        }
        if data[record + 48..record + 51] == [1, 1, 1] {
            modules.push(Module {
                record,
                source: base + number(data, record + 8)?,
                length: number(data, record + 12)?,
                hash: table + table_size + index * 4,
            });
        }
    }
    let mut record = table + table_size + count * 4;
    let builtins = number(data, record)?;
    ensure!(builtins < 100000, "Bun 内置模块数无效");
    record += 4 + builtins * 12 + 20;
    let arena = base + number(data, record)?;
    let arena_size = number(data, record + 4)?;
    ensure!(
        arena <= metadata && arena_size <= metadata - arena,
        "Bun 预链接区越界"
    );
    ensure!(
        regions
            .iter()
            .all(|(start, end)| *end <= arena || *start >= arena + arena_size),
        "Bun 预链接区与模块重叠"
    );
    Ok(Graph {
        base,
        flags: metadata + 28,
        graph_record: record,
        arena,
        arena_size,
        modules,
    })
}

fn replace_once(
    source: &str,
    regex: &Regex,
    replacement: impl FnOnce(&regex::Captures<'_>) -> String,
) -> Result<String> {
    let hits: Vec<_> = regex.captures_iter(source).collect();
    ensure!(hits.len() == 1, "提示词结构命中 {} 处", hits.len());
    let span = hits[0].get(0).context("缺少代码范围")?;
    let mut result = source.to_owned();
    result.replace_range(span.range(), &replacement(&hits[0]));
    Ok(result)
}

fn snapshot_reader<'a>(source: &'a str, reader: &Regex) -> Result<Option<regex::Captures<'a>>> {
    let mut found = None;
    for caps in reader.captures_iter(source) {
        if caps["arg"] != caps["messages"] || caps["snapshot"] != caps["check"] {
            continue;
        }
        let predicate = Regex::new(&format!(
            r#"function {}\((?P<arg>[\w$]+)\)\{{return (?P<a>[\w$]+)\.type==="attachment"&&(?P<b>[\w$]+)\.attachment\.type==="prompt_snapshot"\}}"#,
            regex::escape(&caps["predicate"]),
        ))?;
        if !predicate.captures_iter(source).any(|p| p["arg"] == p["a"] && p["arg"] == p["b"]) {
            continue;
        }
        ensure!(found.is_none(), "提示词快照读取入口不唯一");
        found = Some(caps);
    }
    Ok(found)
}

fn reader_pattern() -> Result<Regex> {
    Ok(Regex::new(concat!(
        r"(?P<prefix>function [\w$]+\((?P<arg>[\w$]+)\)\{let (?P<snapshot>[\w$]+)=",
        r"(?P<messages>[\w$]+)\.findLast\((?P<predicate>[\w$]+)\)\?\.attachment;)",
        r"if\((?P<check>[\w$]+)===void 0\)return;",
    ))?)
}

pub(super) fn refresh_snapshots(data: &mut [u8]) -> Result<()> {
    let graph = graph(data)?;
    let mut changed = Vec::new();
    let getter = Regex::new(
        r#"function ([\w$]+)\(\)\{let [\w$]+=[\w$]+\(\)\?\.[tx]engu_heron_brook;[^\n]{0,500}?return null\}"#,
    )?;
    let reader = reader_pattern()?;
    let mut getter_count = 0;
    let mut reader_count = 0;
    let mut snapshot_count = 0;
    let mut reader_writes = 0;
    for module in &graph.modules {
        let source = std::str::from_utf8(&data[module.source..module.source + module.length])?;
        let count = source.matches(SNAPSHOT).count();
        let has_getter = getter.is_match(source);
        let reader_match = snapshot_reader(source, &reader)?;
        if count == 0 && !has_getter && reader_match.is_none() {
            continue;
        }
        ensure!(
            count == 0 || count == 1 || count == 3,
            "提示词快照写入数量已变化"
        );
        let mut rewritten = source.to_owned();
        if has_getter {
            rewritten = replace_once(&rewritten, &getter, |caps| {
                format!("function {}(){{return null;{MARKER}}}", &caps[1])
            })?;
            getter_count += 1;
        }
        if reader_match.is_some() {
            let caps = snapshot_reader(&rewritten, &reader)?.context("提示词快照读取入口已变化")?;
            let span = caps.get(0).context("提示词快照读取范围缺失")?.range();
            let replacement = format!(
                "{}if({}===void 0||{}.cppPromptBlock!==1)return;",
                &caps["prefix"], &caps["snapshot"], &caps["snapshot"]
            );
            rewritten.replace_range(span, &replacement);
            reader_count += 1;
            reader_writes += count;
        }
        rewritten = rewritten.replace(SNAPSHOT, STAMPED);
        snapshot_count += count;
        changed.push((module, rewritten));
    }
    ensure!(
        getter_count == 1 && reader_count == 1
            && matches!((snapshot_count, reader_writes), (4, 1) | (3, 0)),
        "提示词快照模块未适配：消费入口 {getter_count}，读取入口 {reader_count}，写入入口 {snapshot_count}"
    );
    let mut arena = graph.arena;
    for (module, source) in changed {
        if source.len() <= module.length {
            data[module.source..module.source + module.length].fill(b' ');
            data[module.source..module.source + source.len()].copy_from_slice(source.as_bytes());
        } else {
            ensure!(
                source.len() + 1 <= graph.arena + graph.arena_size - arena,
                "预链接空闲区不足"
            );
            data[arena..arena + source.len()].copy_from_slice(source.as_bytes());
            data[arena + source.len()] = 0;
            put(data, module.record + 8, arena - graph.base)?;
            put(data, module.record + 12, source.len())?;
            arena += source.len() + 1;
        }
        data[module.record + 24..module.record + 40].fill(0);
        put(data, module.hash, 0)?;
    }
    data[graph.graph_record..graph.graph_record + 8].fill(0);
    put(data, graph.flags, number(data, graph.flags)? & !16)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const READER: &str = concat!(
        r#"function isSnapshot(row){return row.type==="attachment"&&row.attachment.type==="prompt_snapshot"}"#,
        "function read(rows){let snapshot=rows.findLast(isSnapshot)?.attachment;if(snapshot===void 0)return;return snapshot}",
    );

    #[test]
    fn reader_is_recognized_independently_of_snapshot_writes() {
        let pattern = reader_pattern().unwrap();
        for source in [READER.to_string(), format!("{READER}const value={{{SNAPSHOT}systemPrompt:[]}};")] {
            let caps = snapshot_reader(&source, &pattern).unwrap().unwrap();
            assert_eq!(&caps["snapshot"], "snapshot");
        }
        assert!(snapshot_reader(&format!("{READER}{READER}"), &pattern).is_err());
        for (old, new) in [
            ("prompt_snapshot", "model"),
            ("snapshot===void", "other===void"),
            ("rows.findLast", "other.findLast"),
            ("row.attachment", "other.attachment"),
        ] {
            assert!(snapshot_reader(&READER.replace(old, new), &pattern).unwrap().is_none());
        }
    }
}

//! Codex 对话名称：以 state 库 threads.name 为准，读不了时退回 session_index.jsonl。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

const STATE_DB_PREFIX: &str = "state_";
const STATE_DB_SUFFIX: &str = ".sqlite";
// Codex 写库时持锁，拿不到就退回索引文件，不拖慢列表
const BUSY_TIMEOUT: Duration = Duration::from_millis(500);

pub fn thread_names(codex: &Path) -> HashMap<String, String> {
    let mut names = index_names(codex);
    if let Some(db) = latest_state_db(codex) {
        if let Ok(from_db) = state_db_names(&db) {
            names.extend(from_db);
        }
    }
    names
}

/// 文件名里的数字是 Codex 的库结构版本，升级后旧库可能还留着
fn latest_state_db(codex: &Path) -> Option<PathBuf> {
    fs::read_dir(codex)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version: u32 = name
                .strip_prefix(STATE_DB_PREFIX)?
                .strip_suffix(STATE_DB_SUFFIX)?
                .parse()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

fn state_db_names(db: &Path) -> rusqlite::Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // 列是随版本加的，缺了就当没有名称
    let columns: i64 = conn.query_row(
        "select count(*) from pragma_table_info('threads') where name in ('id', 'name')",
        [],
        |row| row.get(0),
    )?;
    if columns < 2 {
        return Ok(HashMap::new());
    }
    let mut statement = conn.prepare("select id, name from threads where name <> ''")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    Ok(rows
        .flatten()
        .filter_map(|(id, name)| Some((id?, name?)))
        .collect())
}

/// 改名会追加新行，后写的覆盖先写的
fn index_names(codex: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(text) = fs::read_to_string(codex.join("session_index.jsonl")) else {
        return names;
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            value.get("id").and_then(|v| v.as_str()),
            value.get("thread_name").and_then(|v| v.as_str()),
        ) {
            if !name.trim().is_empty() {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(codex: &Path, rows: &[(&str, &str)]) {
        let lines: Vec<String> = rows
            .iter()
            .map(|(id, name)| serde_json::json!({"id": id, "thread_name": name, "updated_at": "2026-10-21T00:00:00Z"}).to_string())
            .collect();
        fs::write(codex.join("session_index.jsonl"), lines.join("\n")).unwrap();
    }

    fn write_db(path: &Path, with_name: bool, rows: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        let schema = if with_name {
            "create table threads (id text primary key, title text not null default '', name text)"
        } else {
            "create table threads (id text primary key, title text not null default '')"
        };
        conn.execute_batch(schema).unwrap();
        for (id, name) in rows {
            if with_name {
                conn.execute("insert into threads (id, name) values (?1, ?2)", [id, name]).unwrap();
            } else {
                conn.execute("insert into threads (id) values (?1)", [id]).unwrap();
            }
        }
    }

    #[test]
    fn state_db_names_override_the_index() {
        let dir = tempfile::tempdir().unwrap();
        write_index(dir.path(), &[("a", "索引旧名"), ("b", "只在索引"), ("b", "索引改名")]);
        write_db(&dir.path().join("state_5.sqlite"), true, &[("a", "库里的名"), ("c", "只在库里"), ("d", "")]);

        let names = thread_names(dir.path());
        assert_eq!(names.get("a").map(String::as_str), Some("库里的名"));
        assert_eq!(names.get("b").map(String::as_str), Some("索引改名"));
        assert_eq!(names.get("c").map(String::as_str), Some("只在库里"));
        assert!(!names.contains_key("d"));
    }

    #[test]
    fn newest_state_db_version_wins() {
        let dir = tempfile::tempdir().unwrap();
        write_db(&dir.path().join("state_4.sqlite"), true, &[("a", "旧版本库")]);
        write_db(&dir.path().join("state_5.sqlite"), true, &[("a", "新版本库")]);
        fs::write(dir.path().join("state_9.sqlite-wal"), b"not a db").unwrap();

        assert_eq!(latest_state_db(dir.path()), Some(dir.path().join("state_5.sqlite")));
        assert_eq!(thread_names(dir.path()).get("a").map(String::as_str), Some("新版本库"));
    }

    #[test]
    fn missing_name_column_or_broken_db_falls_back_to_the_index() {
        let dir = tempfile::tempdir().unwrap();
        write_index(dir.path(), &[("a", "索引里的名")]);
        write_db(&dir.path().join("state_5.sqlite"), false, &[("a", "")]);
        assert_eq!(thread_names(dir.path()).get("a").map(String::as_str), Some("索引里的名"));

        fs::write(dir.path().join("state_6.sqlite"), b"definitely not sqlite").unwrap();
        assert_eq!(thread_names(dir.path()).get("a").map(String::as_str), Some("索引里的名"));
    }

    #[test]
    fn no_codex_state_at_all_yields_no_names() {
        let dir = tempfile::tempdir().unwrap();
        assert!(thread_names(dir.path()).is_empty());
    }
}

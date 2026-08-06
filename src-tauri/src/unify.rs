use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::discovery::{PoolCombo, PoolKind};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifyPlan {
    pub pool: PoolKind,
    pub canonical: PoolCombo,
    pub to_merge: Vec<PoolCombo>,
    pub already_unified: Vec<PoolCombo>,
    pub foreign_junctions: Vec<PoolCombo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeConflict {
    pub entry: String,
    pub kept: PathBuf,
    pub shadowed: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifyReport {
    pub pool: PoolKind,
    pub canonical: PathBuf,
    pub merged: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub conflicts: Vec<MergeConflict>,
    pub moved_entries: usize,
}

fn same_target(combo: &PoolCombo, canonical: &Path) -> bool {
    combo
        .junction_target
        .as_deref()
        .map(|t| paths_equal(t, canonical))
        .unwrap_or(false)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .to_lowercase()
            .replace('/', "\\")
            .trim_start_matches("\\\\?\\")
            .trim_end_matches('\\')
            .to_string()
    };
    norm(a) == norm(b)
}

/// Pick the combo with the most sessions (tombstones break ties) as canonical;
/// junctions are views of somewhere else, so they never win.
pub fn plan_unify(combos: &[PoolCombo], pool: PoolKind) -> Option<UnifyPlan> {
    let pool_combos: Vec<&PoolCombo> = combos.iter().filter(|c| c.pool == pool).collect();
    let canonical = pool_combos
        .iter()
        .filter(|c| !c.is_junction)
        .max_by(|a, b| {
            (a.session_count, a.tombstone_count, std::cmp::Reverse(a.path.clone()))
                .cmp(&(b.session_count, b.tombstone_count, std::cmp::Reverse(b.path.clone())))
        })?
        .to_owned()
        .clone();

    let mut to_merge = Vec::new();
    let mut already_unified = Vec::new();
    let mut foreign_junctions = Vec::new();
    for combo in pool_combos {
        if paths_equal(&combo.path, &canonical.path) {
            continue;
        }
        if combo.is_junction {
            if same_target(combo, &canonical.path) {
                already_unified.push(combo.clone());
            } else {
                foreign_junctions.push(combo.clone());
            }
        } else {
            to_merge.push(combo.clone());
        }
    }
    Some(UnifyPlan {
        pool,
        canonical,
        to_merge,
        already_unified,
        foreign_junctions,
    })
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn merge_scheduled_tasks(canonical: &Path, incoming: &Path) -> Result<()> {
    let base: serde_json::Value = serde_json::from_str(&fs::read_to_string(canonical)?)?;
    let add: serde_json::Value = serde_json::from_str(&fs::read_to_string(incoming)?)?;
    let mut merged = base.clone();

    let mut tasks = base
        .get("scheduledTasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let known: std::collections::HashSet<String> = tasks
        .iter()
        .filter_map(|t| t.get("taskId").and_then(|v| v.as_str()).map(String::from))
        .collect();
    for task in add.get("scheduledTasks").and_then(|v| v.as_array()).into_iter().flatten() {
        let id = task.get("taskId").and_then(|v| v.as_str());
        if id.map(|i| !known.contains(i)).unwrap_or(true) {
            tasks.push(task.clone());
        }
    }
    merged["scheduledTasks"] = serde_json::Value::Array(tasks);

    if let (Some(base_skips), Some(add_skips)) = (
        merged.get("recordedSkips").and_then(|v| v.as_object()).cloned(),
        add.get("recordedSkips").and_then(|v| v.as_object()),
    ) {
        let mut skips = base_skips;
        for (k, v) in add_skips {
            skips.entry(k.clone()).or_insert_with(|| v.clone());
        }
        merged["recordedSkips"] = serde_json::Value::Object(skips);
    }

    write_atomic(canonical, &serde_json::to_string_pretty(&merged)?)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn merge_entry(
    canonical_dir: &Path,
    source: &Path,
    name: &str,
    conflicts: &mut Vec<MergeConflict>,
) -> Result<bool> {
    let dest = canonical_dir.join(name);
    let is_dir = source.is_dir();
    if !dest.exists() {
        if is_dir {
            copy_dir_recursive(source, &dest)?;
        } else {
            fs::copy(source, &dest)?;
        }
        return Ok(true);
    }
    if name == "scheduled-tasks.json" {
        if merge_scheduled_tasks(&dest, source).is_ok() {
            return Ok(true);
        }
    }
    if is_dir {
        // same session uuid on both sides can only come from an earlier copy;
        // canonical wins, the .bak keeps the loser intact
        conflicts.push(MergeConflict {
            entry: name.to_string(),
            kept: dest,
            shadowed: source.to_path_buf(),
            reason: "目录已存在，保留 canonical，原目录留在 .bak".into(),
        });
        return Ok(false);
    }
    if mtime_ms(source) > mtime_ms(&dest) {
        fs::copy(source, &dest)?;
        conflicts.push(MergeConflict {
            entry: name.to_string(),
            kept: source.to_path_buf(),
            shadowed: canonical_dir.join(name),
            reason: "重名文件，较新 mtime 覆盖".into(),
        });
        Ok(true)
    } else {
        conflicts.push(MergeConflict {
            entry: name.to_string(),
            kept: dest,
            shadowed: source.to_path_buf(),
            reason: "重名文件，canonical 较新，保留".into(),
        });
        Ok(false)
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn bak_name(org_id: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{org_id}.bak-{ts}")
}

pub fn apply_unify(plan: &UnifyPlan) -> Result<UnifyReport> {
    if plan.canonical.is_junction {
        bail!("canonical 不能是 junction");
    }
    let canonical_dir = plan.canonical.path.clone();
    let mut report = UnifyReport {
        pool: plan.pool,
        canonical: canonical_dir.clone(),
        merged: Vec::new(),
        skipped: Vec::new(),
        conflicts: Vec::new(),
        moved_entries: 0,
    };

    for combo in &plan.to_merge {
        let entries: Vec<_> = fs::read_dir(&combo.path)
            .with_context(|| format!("读取 {}", combo.path.display()))?
            .flatten()
            .collect();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if merge_entry(&canonical_dir, &entry.path(), &name, &mut report.conflicts)? {
                report.moved_entries += 1;
            }
        }
        let parent = combo
            .path
            .parent()
            .context("组合目录缺少父目录")?
            .to_path_buf();
        let bak = parent.join(bak_name(&combo.org_id));
        fs::rename(&combo.path, &bak)
            .with_context(|| format!("备份 {} → {}", combo.path.display(), bak.display()))?;
        junction::create(&canonical_dir, &combo.path)
            .with_context(|| format!("创建 junction {}", combo.path.display()))?;
        report.merged.push(combo.path.clone());
    }

    for combo in &plan.already_unified {
        report.skipped.push(combo.path.clone());
    }
    Ok(report)
}

/// Remove the junction and restore the newest `.bak-*` sibling.
pub fn restore_combo(combo_path: &Path, org_id: &str) -> Result<()> {
    let meta = fs::symlink_metadata(combo_path).context("组合路径不存在")?;
    if !meta.file_type().is_symlink() && junction::get_target(combo_path).is_err() {
        bail!("{} 不是 junction，无需还原", combo_path.display());
    }
    let parent = combo_path.parent().context("缺少父目录")?;
    let prefix = format!("{org_id}.bak-");
    let newest = fs::read_dir(parent)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .max_by_key(|e| e.file_name().to_string_lossy().into_owned());
    let Some(bak) = newest else {
        bail!("找不到 {prefix}* 备份");
    };
    fs::remove_dir(combo_path).context("移除 junction")?;
    fs::rename(bak.path(), combo_path).context("还原备份")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{list_combos, UserDataRoot, CODE_POOL};

    fn combo_at(dir: &Path, label: &str) -> Vec<PoolCombo> {
        let root = UserDataRoot {
            label: label.into(),
            path: dir.to_path_buf(),
            ant_did: None,
        };
        list_combos(&root)
    }

    fn seed(dir: &Path, acc: &str, org: &str, sessions: &[&str]) -> PathBuf {
        let org_dir = dir.join(CODE_POOL).join(acc).join(org);
        fs::create_dir_all(&org_dir).unwrap();
        for s in sessions {
            fs::write(
                org_dir.join(format!("local_{s}.json")),
                format!("{{\"sessionId\":\"local_{s}\",\"cliSessionId\":\"{s}\"}}"),
            )
            .unwrap();
        }
        org_dir
    }

    #[test]
    fn unify_merges_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let acc_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let acc_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let org = "00000000-0000-4000-8000-000000000001";
        let big = seed(dir.path(), acc_a, org, &["s1", "s2", "s3"]);
        let small = seed(dir.path(), acc_b, org, &["s4"]);

        let combos = combo_at(dir.path(), "t");
        let plan = plan_unify(&combos, PoolKind::Code).unwrap();
        assert!(paths_equal(&plan.canonical.path, &big));
        assert_eq!(plan.to_merge.len(), 1);

        let report = apply_unify(&plan).unwrap();
        assert_eq!(report.moved_entries, 1);
        assert!(big.join("local_s4.json").exists());
        assert!(junction::get_target(&small).is_ok());

        // second run sees the junction and does nothing
        let combos = combo_at(dir.path(), "t");
        let plan = plan_unify(&combos, PoolKind::Code).unwrap();
        assert!(plan.to_merge.is_empty());
        assert_eq!(plan.already_unified.len(), 1);

        // sessions listed through the junction match canonical
        let through: Vec<_> = fs::read_dir(&small)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(through.contains(&"local_s1.json".to_string()));
        assert!(through.contains(&"local_s4.json".to_string()));
    }

    #[test]
    fn restore_puts_original_back() {
        let dir = tempfile::tempdir().unwrap();
        let acc_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let acc_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let org = "00000000-0000-4000-8000-000000000001";
        seed(dir.path(), acc_a, org, &["s1", "s2"]);
        let small = seed(dir.path(), acc_b, org, &["s9"]);

        let combos = combo_at(dir.path(), "t");
        let plan = plan_unify(&combos, PoolKind::Code).unwrap();
        apply_unify(&plan).unwrap();

        restore_combo(&small, org).unwrap();
        assert!(junction::get_target(&small).is_err());
        assert!(small.join("local_s9.json").exists());
        assert!(!small.join("local_s1.json").exists());
    }

    #[test]
    fn scheduled_tasks_merge_by_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        fs::write(&a, r#"{"scheduledTasks":[{"taskId":"x"}],"recordedSkips":{"k1":1}}"#).unwrap();
        fs::write(&b, r#"{"scheduledTasks":[{"taskId":"x"},{"taskId":"y"}],"recordedSkips":{"k2":2}}"#).unwrap();
        merge_scheduled_tasks(&a, &b).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(merged["scheduledTasks"].as_array().unwrap().len(), 2);
        assert_eq!(merged["recordedSkips"]["k1"], 1);
        assert_eq!(merged["recordedSkips"]["k2"], 2);
    }
}

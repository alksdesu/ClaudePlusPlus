use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use super::asar::{hash, PatchState};

const ORIGINAL_HASH: &str = "5e6004d9ad8fb611e2cf0031829c0362e87b26f69f61029c31811a80e9598a7e";
const PATCHED_HASH: &str = "f38a6efbc3311e4229074dfd37e9a540b51190d8d3ca688dbb061a64aaff2ebc";
const BEFORE: &str = "if(JE()?.platform!==\"darwin\")return\"absent\";";
const AFTER: &str = "if(![\"darwin\",\"win32\"].includes(JE()?.platform))return\"absent\";";
const MAX_SOURCE: u64 = 64 * 1024 * 1024;

fn locate(app: &Path) -> Result<(PathBuf, String)> {
    locate_source(app, BEFORE, AFTER, "后台模式")
}

pub(super) fn locate_source(app: &Path, before: &str, after: &str, label: &str) -> Result<(PathBuf, String)> {
    let directory = app.join("Contents/Resources/ion-dist/assets/v1");
    let canonical = fs::canonicalize(app)?;
    ensure!(
        fs::canonicalize(&directory)?.starts_with(&canonical),
        "renderer 目录越界"
    );
    let mut found = None;
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("js") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.is_symlink(),
            "renderer 不是普通文件"
        );
        ensure!(metadata.len() <= MAX_SOURCE, "renderer 文件过大");
        let source = fs::read_to_string(&path)?;
        let matches = source.matches(before).count() + source.matches(after).count();
        if matches == 0 {
            continue;
        }
        ensure!(
            matches == 1 && found.is_none(),
            "{label} renderer 锚点不唯一"
        );
        found = Some((path, source));
    }
    found.with_context(|| format!("未找到{label} renderer 入口"))
}

pub fn inspect(app: &Path, version: &str, state: &PatchState) -> Result<()> {
    if version != "3.1623.4" {
        return Ok(());
    }
    let (_, source) = locate(app)?;
    let expected = if *state == PatchState::Patched {
        PATCHED_HASH
    } else {
        ORIGINAL_HASH
    };
    ensure!(
        hash(source.as_bytes()) == expected,
        "后台模式 renderer 与主进程补丁状态不匹配"
    );
    Ok(())
}

fn patch_source(source: &str) -> Result<String> {
    ensure!(
        source.matches(BEFORE).count() == 1 && !source.contains(AFTER),
        "后台模式 renderer 锚点必须唯一且未修改"
    );
    ensure!(
        hash(source.as_bytes()) == ORIGINAL_HASH,
        "后台模式 renderer 内容未适配"
    );
    let modified = source.replacen(BEFORE, AFTER, 1);
    ensure!(
        hash(modified.as_bytes()) == PATCHED_HASH,
        "后台模式 renderer 输出哈希不匹配"
    );
    Ok(modified)
}

pub fn patch(app: &Path, version: &str) -> Result<()> {
    if version != "3.1623.4" {
        return Ok(());
    }
    let (path, source) = locate(app)?;
    fs::write(path, patch_source(&source)?).context("写入后台模式 renderer")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Contents/Resources/ion-dist/assets/v1");
        fs::create_dir_all(&path).unwrap();
        (root, path)
    }

    #[test]
    fn locator_follows_chunk_renames_and_both_states() {
        for anchor in [BEFORE, AFTER] {
            let (root, directory) = directory();
            fs::write(directory.join("unrelated.js"), "other();").unwrap();
            let target = directory.join("renamed.chunk.js");
            fs::write(&target, anchor).unwrap();
            assert_eq!(locate(root.path()).unwrap(), (target, anchor.into()));
        }
    }

    #[test]
    fn duplicate_missing_and_unverified_sources_are_rejected() {
        let (root, directory) = directory();
        assert!(locate(root.path()).is_err());
        fs::write(directory.join("one.js"), BEFORE).unwrap();
        fs::write(directory.join("two.js"), AFTER).unwrap();
        assert!(locate(root.path()).is_err());
        assert!(patch_source(BEFORE).is_err());
        assert!(patch_source(AFTER).is_err());
    }

    #[test]
    fn older_builds_keep_their_original_renderer() {
        let (root, _) = directory();
        assert!(inspect(root.path(), "3.1623.1", &PatchState::Patched).is_ok());
        assert!(patch(root.path(), "3.1623.1").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn renderer_symlinks_are_rejected() {
        let (root, directory) = directory();
        fs::write(root.path().join("source.js"), BEFORE).unwrap();
        std::os::unix::fs::symlink(root.path().join("source.js"), directory.join("linked.js"))
            .unwrap();
        assert!(locate(root.path()).is_err());
    }
}

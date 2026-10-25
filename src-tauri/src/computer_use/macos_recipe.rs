use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;

use super::asar::hash;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    before: String,
    after: String,
    count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Asset {
    pub anchor: Option<String>,
    pub original: String,
    pub modified: String,
    original_size: u64,
    modified_size: u64,
    changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Field {
    mask: u8,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Main {
    pub anchor: String,
    config: String,
    prefix: String,
    fields: Vec<Field>,
    fast_changes: Vec<Change>,
    profiles: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recipe {
    pub version: String,
    pub main: Main,
    pub prewarm: Asset,
    pub background: Asset,
    pub fast_state: Asset,
    pub fast_widget: Asset,
    pub language: Asset,
}

pub(super) fn get(version: &str) -> Option<&'static Recipe> {
    static RECIPE: OnceLock<Recipe> = OnceLock::new();
    let recipe = RECIPE.get_or_init(|| {
        serde_json::from_str(include_str!("resources/macos-3.6556.2.json"))
            .expect("内置 macOS 适配记录必须有效")
    });
    (recipe.version == version).then_some(recipe)
}

fn change_source(source: &str, changes: &[Change]) -> Result<String> {
    let mut output = source.to_owned();
    for change in changes {
        ensure!(
            !change.before.is_empty()
                && change.count > 0
                && change.before != change.after
                && output.matches(&change.before).count() == change.count,
            "原生入口数量与适配记录不一致"
        );
        output = output.replace(&change.before, &change.after);
    }
    Ok(output)
}

impl Main {
    #[cfg(test)]
    pub fn profile(&self, mask: u8) -> Option<&str> {
        self.profiles.get(usize::from(mask)).map(String::as_str)
    }

    pub fn mask(&self, source: &str) -> Result<u8> {
        let digest = hash(source.as_bytes());
        self.profiles
            .iter()
            .position(|h| *h == digest)
            .and_then(|i| u8::try_from(i).ok())
            .context("主进程内容不属于已验证的原生功能组合")
    }

    pub fn patch(&self, source: &str, mask: u8) -> Result<String> {
        ensure!(self.mask(source)? == 0, "必须从官方主进程基线生成补丁");
        let expected = self.profiles.get(usize::from(mask)).context("未知原生功能组合")?;
        ensure!(source.matches(&self.config).count() == 1, "原生配置入口不唯一");
        let fields: Vec<_> = self.fields.iter()
            .filter(|field| mask & field.mask != 0)
            .map(|field| field.source.as_str())
            .collect();
        let mut output = if fields.is_empty() {
            source.to_owned()
        } else {
            source.replacen(&self.config, &format!("{}{}}}}}", self.prefix, fields.join(",")), 1)
        };
        if mask & 8 != 0 {
            output = change_source(&output, &self.fast_changes)?;
        }
        ensure!(hash(output.as_bytes()) == *expected, "原生组合补丁输出哈希不匹配");
        Ok(output)
    }
}

impl Asset {
    pub fn verify(&self, source: &str, enabled: bool) -> Result<()> {
        let expected = if enabled { &self.modified } else { &self.original };
        ensure!(hash(source.as_bytes()) == *expected, "原生模块内容与功能状态不一致");
        Ok(())
    }

    pub fn patch(&self, source: &str) -> Result<String> {
        self.verify(source, false)?;
        let output = change_source(source, &self.changes)?;
        self.verify(&output, true)?;
        Ok(output)
    }

    fn locate(&self, app: &Path) -> Result<(PathBuf, String)> {
        let directory = app.join("Contents/Resources/ion-dist/assets/v1");
        ensure!(directory.canonicalize()?.starts_with(app.canonicalize()?), "原生资源目录越界");
        let mut found = None;
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("js") { continue; }
            let meta = fs::symlink_metadata(&path)?;
            ensure!(meta.is_file() && !meta.is_symlink() && meta.len() <= 64 * 1024 * 1024, "原生资源不是安全的普通文件");
            if meta.len() != self.original_size && meta.len() != self.modified_size { continue; }
            let bytes = fs::read(&path)?;
            let digest = hash(&bytes);
            if digest != self.original && digest != self.modified { continue; }
            ensure!(found.is_none(), "多个文件命中同一原生资源，拒绝修改");
            found = Some((path, String::from_utf8(bytes)?));
        }
        found.context("未找到内容校验匹配的原生资源")
    }

    pub fn installed(&self, app: &Path) -> Result<bool> {
        let (_, source) = self.locate(app)?;
        Ok(self.modified != self.original && hash(source.as_bytes()) == self.modified)
    }

    pub fn inspect(&self, app: &Path, enabled: bool) -> Result<()> {
        let (_, source) = self.locate(app)?;
        self.verify(&source, enabled)
    }

    pub fn apply(&self, app: &Path) -> Result<()> {
        let (path, source) = self.locate(app)?;
        let modified = self.patch(&source)?;
        if modified != source { fs::write(path, modified)?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_cover_all_combinations_without_weakening_unknown_build_checks() {
        let recipe = get("3.6556.2").unwrap();
        assert_eq!(recipe.main.profiles.len(), 16);
        let unique: std::collections::HashSet<_> = recipe.main.profiles.iter().collect();
        assert_eq!(unique.len(), 16);
        assert!(unique.iter().all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())));
        assert!(get("3.6556.3").is_none());
        assert!(recipe.main.mask("unknown").is_err());
        assert!(recipe.main.patch(&recipe.main.config, 1).is_err());
    }

    #[test]
    fn asset_changes_require_exact_inputs_outputs_and_anchor_counts() {
        let asset = Asset {
            anchor: None,
            original: hash(b"one guard two"),
            modified: hash(b"one  two"),
            original_size: 13,
            modified_size: 8,
            changes: vec![Change { before: "guard".into(), after: String::new(), count: 1 }],
        };
        assert_eq!(asset.patch("one guard two").unwrap(), "one  two");
        assert!(asset.patch("one guard guard two").is_err());
        assert!(asset.patch("one  two").is_err());
        assert!(change_source("guard guard", &asset.changes).is_err());
    }

    #[test]
    fn renderer_discovery_is_content_based_and_refuses_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("Contents/Resources/ion-dist/assets/v1");
        fs::create_dir_all(&assets).unwrap();
        let asset = Asset { anchor: None, original: hash(b"original"), modified: hash(b"modified"), original_size: 8, modified_size: 8, changes: vec![Change { before: "original".into(), after: "modified".into(), count: 1 }] };
        fs::write(assets.join("renamed-chunk.js"), "original").unwrap();
        assert!(!asset.installed(dir.path()).unwrap());
        asset.apply(dir.path()).unwrap();
        assert!(asset.installed(dir.path()).unwrap());
        fs::write(assets.join("duplicate.js"), "modified").unwrap();
        assert!(asset.installed(dir.path()).is_err());
        assert!(asset.apply(dir.path()).is_err());
    }
}

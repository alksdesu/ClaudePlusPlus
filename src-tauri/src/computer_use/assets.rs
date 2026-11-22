use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use super::{
    asar,
    native::{catalogs, Selection},
    rules,
};

#[cfg(any(test, windows))]
pub(crate) const PACKED: &str = "resources/ion-dist.asar";
#[cfg(test)]
const LOOSE: &str = "resources/ion-dist";
const ASSETS: &str = "assets/v1";
const MAX_ASSET: u64 = 64 * 1024 * 1024;

pub(crate) struct Resources {
    root: PathBuf,
    archive: Option<asar::Archive>,
}

fn plain(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            },
        "资源路径类型异常：{}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "资源路径不允许是重解析点"
        );
    }
    Ok(())
}

impl Resources {
    pub(crate) fn open(app: &Path) -> Result<Self> {
        Self::at(app.join("resources"))
    }

    pub(crate) fn at(resources: PathBuf) -> Result<Self> {
        plain(&resources, true)?;
        let packed = resources.join("ion-dist.asar");
        if packed.try_exists()? {
            plain(&packed, false)?;
            return Ok(Self {
                archive: Some(asar::Archive::open(&packed)?),
                root: packed,
            });
        }
        let root = resources.join("ion-dist");
        if root.try_exists()? {
            plain(&root, true)?;
        }
        Ok(Self {
            root,
            archive: None,
        })
    }

    pub(crate) fn packed(&self) -> bool {
        self.archive.is_some()
    }

    pub(crate) fn available(&self) -> bool {
        self.packed() || self.root.join(ASSETS).is_dir()
    }

    pub(crate) fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn loose_path(&self, relative: &str) -> Result<PathBuf> {
        let mut path = self.root.clone();
        let parts: Vec<_> = relative.split('/').collect();
        ensure!(
            parts.iter().all(|part| !part.is_empty()
                && *part != "."
                && *part != ".."
                && !part.contains(['\\', ':', '\0'])),
            "资源路径无效"
        );
        for (index, part) in parts.iter().enumerate() {
            path.push(part);
            if path.symlink_metadata().is_ok() {
                plain(&path, index + 1 < parts.len())?;
            }
        }
        Ok(path)
    }

    fn read(&mut self, relative: &str) -> Result<Option<Vec<u8>>> {
        if let Some(archive) = &mut self.archive {
            let parts: Vec<_> = relative.split('/').collect();
            return archive
                .contains(&parts)?
                .then(|| archive.source(&parts))
                .transpose();
        }
        let path = self.loose_path(relative)?;
        if !path.try_exists()? {
            return Ok(None);
        }
        ensure!(fs::metadata(&path)?.len() <= MAX_ASSET, "界面资源过大");
        Ok(Some(fs::read(path)?))
    }

    fn find(
        &mut self,
        marker: &str,
        accept: impl Fn(&str) -> bool,
        label: &str,
    ) -> Result<Option<(String, String)>> {
        let names = if let Some(archive) = &self.archive {
            archive.files(&["assets", "v1"])?
        } else {
            let directory = self.root.join(ASSETS);
            plain(&directory, true)?;
            fs::read_dir(directory)?
                .map(|entry| {
                    entry?
                        .file_name()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("界面资源名不是 UTF-8"))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut found = None;
        for name in names.into_iter().filter(|name| name.ends_with(".js")) {
            let relative = format!("{ASSETS}/{name}");
            let bytes = if let Some(archive) = &mut self.archive {
                archive.read(&["assets", "v1", &name])?
            } else {
                self.read(&relative)?.context("界面资源已消失")?
            };
            if memchr::memmem::find(&bytes, marker.as_bytes()).is_none() {
                continue;
            }
            let text = String::from_utf8(bytes).context("界面资源不是 UTF-8")?;
            if !accept(&text) {
                continue;
            }
            ensure!(found.is_none(), "{label}入口不唯一，拒绝修改");
            if let Some(archive) = &mut self.archive {
                archive.source(&["assets", "v1", &name])?;
            }
            found = Some((relative, text));
        }
        Ok(found)
    }

    pub(crate) fn fast_state(&mut self) -> Result<Option<(String, String)>> {
        self.find(
            "fastModeOffByCredits",
            |text| rules::fast_state_installed(text).is_ok(),
            "Fast 状态",
        )
    }

    pub(crate) fn language(&mut self) -> Result<Option<(String, String)>> {
        self.find(
            "startsWith(\"zh-\")",
            |text| rules::language_installed(text).is_ok(),
            "语言",
        )
    }

    pub(crate) fn catalogs_present(&mut self) -> Result<bool> {
        let mut found = 0;
        for (name, _, expected) in catalogs::RESOURCES {
            if let Some(bytes) = self.read(&format!("i18n/{name}"))? {
                ensure!(asar::hash(&bytes) == *expected, "中文资源校验失败：{name}");
                found += 1;
            }
        }
        ensure!(
            found == 0 || found == catalogs::RESOURCES.len(),
            "中文词典只安装了一部分"
        );
        Ok(found > 0)
    }

    pub(crate) fn apply(mut self, selection: Selection, boundary: &Path) -> Result<Option<String>> {
        let mut changes = Vec::new();
        if selection.fast_mode_status {
            let (path, text) = self.fast_state()?.context("未找到 Fast 状态入口")?;
            changes.push((path, rules::patch_fast_state(&text)?.into_bytes()));
        }
        if selection.chinese {
            let (path, text) = self.language()?.context("未找到语言入口")?;
            changes.push((path, rules::patch_language(&text)?.into_bytes()));
            ensure!(!self.catalogs_present()?, "中文资源已存在，拒绝覆盖");
            if self.packed() {
                for (name, bytes, expected) in catalogs::RESOURCES {
                    ensure!(asar::hash(bytes) == *expected, "内置中文资源校验失败");
                    changes.push((format!("i18n/{name}"), bytes.to_vec()));
                }
            }
        }
        if self.packed() {
            drop(self.archive.take());
            return if changes.is_empty() {
                Ok(None)
            } else {
                asar::write_files(&self.root, &changes).map(Some)
            };
        }
        for (relative, bytes) in changes {
            fs::write(self.loose_path(&relative)?, bytes)?;
        }
        if selection.chinese {
            catalogs::install(&self.root.join("i18n"), boundary)?;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: &str = "let cr=l&&meta?.fastModeOffByCredits===!0,ag=l?meta?.fastModeByAgent:void 0,on=pick??(remote?st===\"on\"||st===\"cooldown\":cr?!1:ag??(fallback));";
    const LANGUAGE: &str = "var a=[\"en-US\",\"de-DE\",\"fr-FR\",\"ko-KR\",\"ja-JP\",\"es-419\",\"es-ES\",\"it-IT\",\"hi-IN\",\"pt-BR\",\"id-ID\"];function resolve(x,t=a){return x.startsWith(\"zh-\")?x:t[0]}function fallback(x,t=a){let n=new Map;return t[0]}";

    fn fixture(packed: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let entries: Vec<(&str, &[u8])> = vec![
            ("assets/v1/fast.js", FAST.as_bytes()),
            ("assets/v1/locale.js", LANGUAGE.as_bytes()),
            ("assets/v1/untouched.js", b"unrelated();"),
            ("i18n/en-US.json", b"{}"),
            ("i18n/dynamic/en-US.json", b"{}"),
        ];
        fs::create_dir_all(dir.path().join("resources")).unwrap();
        if packed {
            asar::fixture_resources(&dir.path().join(PACKED), &entries);
        } else {
            for (name, bytes) in entries {
                let path = dir.path().join(LOOSE).join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
        }
        dir
    }

    #[test]
    fn packed_resources_allow_a_loose_html_entry() {
        let dir = fixture(true);
        let loose = dir.path().join(LOOSE);
        fs::create_dir(&loose).unwrap();
        fs::write(loose.join("index.html"), b"<html></html>").unwrap();
        let mut resources = Resources::open(dir.path()).unwrap();
        assert!(resources.packed());
        assert!(resources.fast_state().unwrap().is_some());
        assert!(resources.language().unwrap().is_some());
        resources.apply(Selection { chinese: true, fast_mode_status: true, ..Default::default() }, dir.path()).unwrap();
        assert_eq!(fs::read(loose.join("index.html")).unwrap(), b"<html></html>");
        assert!(Resources::open(dir.path()).unwrap().catalogs_present().unwrap());
    }

    #[test]
    fn directory_and_archive_apply_the_same_features() {
        for packed in [false, true] {
            let dir = fixture(packed);
            let mut resources = Resources::open(dir.path()).unwrap();
            assert_eq!(resources.packed(), packed);
            assert!(!resources.catalogs_present().unwrap());
            assert!(
                !rules::fast_state_installed(&resources.fast_state().unwrap().unwrap().1).unwrap()
            );
            let selection = Selection {
                chinese: true,
                fast_mode_status: true,
                ..Default::default()
            };
            assert_eq!(
                resources.apply(selection, dir.path()).unwrap().is_some(),
                packed
            );
            let mut resources = Resources::open(dir.path()).unwrap();
            assert!(resources.catalogs_present().unwrap());
            assert!(
                rules::fast_state_installed(&resources.fast_state().unwrap().unwrap().1).unwrap()
            );
            assert!(rules::language_installed(&resources.language().unwrap().unwrap().1).unwrap());
            assert_eq!(
                resources.read("assets/v1/untouched.js").unwrap().unwrap(),
                b"unrelated();"
            );
            assert!(resources.apply(selection, dir.path()).is_err());
        }
    }

    #[test]
    fn archive_refuses_corruption_duplicate_targets_and_path_traversal() {
        let dir = fixture(true);
        let path = dir.path().join(PACKED);
        let original = fs::read(&path).unwrap();
        for names in [
            vec!["../escape.js"],
            vec!["assets/v1/fast.js", "assets/v1/fast.js"],
            vec!["i18n"],
            vec!["absent/new.json"],
        ] {
            let changes: Vec<_> = names
                .into_iter()
                .map(|name| (name.to_owned(), b"changed".to_vec()))
                .collect();
            assert!(asar::write_files(&path, &changes).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        let mut corrupt = original;
        let index = memchr::memmem::find(&corrupt, FAST.as_bytes()).unwrap();
        corrupt[index] = b'x';
        fs::write(&path, &corrupt).unwrap();
        assert!(Resources::open(dir.path()).unwrap().fast_state().is_err());
        assert!(asar::write_files(
            &path,
            &[("assets/v1/fast.js".into(), b"replacement".to_vec())]
        )
        .is_err());
        assert_eq!(fs::read(path).unwrap(), corrupt);
    }

    #[test]
    fn partial_catalogs_and_ambiguous_assets_are_refused() {
        for packed in [false, true] {
            let dir = fixture(packed);
            let extra = [
                ("assets/v1/duplicate.js".into(), FAST.as_bytes().to_vec()),
                (
                    "i18n/zh-Hans.json".into(),
                    catalogs::RESOURCES[0].1.to_vec(),
                ),
            ];
            if packed {
                asar::write_files(&dir.path().join(PACKED), &extra).unwrap();
            } else {
                for (name, bytes) in extra {
                    fs::write(dir.path().join(LOOSE).join(name), bytes).unwrap();
                }
            }
            let mut resources = Resources::open(dir.path()).unwrap();
            assert!(resources.fast_state().is_err());
            assert!(resources.catalogs_present().is_err());
        }
    }
}

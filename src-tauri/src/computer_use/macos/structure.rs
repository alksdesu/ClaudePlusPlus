use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use crate::computer_use::{
    asar,
    native::{catalogs, Selection},
    rules,
};

const MAIN: &str = "Contents/Resources/app.asar";
pub(super) const PACKED: &str = "Contents/Resources/ion-dist.asar";
const LOOSE: &str = "Contents/Resources/ion-dist";
const ASSETS: &str = "assets/v1";

enum Renderer {
    Packed(PathBuf),
    Loose(PathBuf),
}

fn bounded(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        !path.is_symlink() && path.canonicalize()?.starts_with(root.canonicalize()?),
        "原生资源路径越界"
    );
    Ok(())
}

impl Renderer {
    fn open(app: &Path) -> Result<Self> {
        let packed = app.join(PACKED);
        let loose = app.join(LOOSE);
        ensure!(
            packed.try_exists()? != loose.try_exists()?,
            "renderer 布局缺失或不唯一"
        );
        if packed.try_exists()? {
            bounded(app, &packed)?;
            Ok(Self::Packed(packed))
        } else {
            bounded(app, &loose)?;
            bounded(&loose, &loose.join(ASSETS))?;
            Ok(Self::Loose(loose))
        }
    }

    fn find(
        &self,
        marker: &str,
        accept: fn(&str) -> Result<bool>,
        label: &str,
    ) -> Result<(String, String)> {
        let mut found = None;
        let mut visit = |name: String, bytes: Vec<u8>| -> Result<()> {
            if memchr::memmem::find(&bytes, marker.as_bytes()).is_none() {
                return Ok(());
            }
            let source = String::from_utf8(bytes).context("原生资源不是 UTF-8")?;
            if accept(&source).is_err() {
                return Ok(());
            }
            ensure!(found.is_none(), "{label}入口不唯一");
            found = Some((name, source));
            Ok(())
        };
        match self {
            Self::Packed(path) => {
                let mut archive = asar::Archive::open(path)?;
                for name in archive.files(&["assets", "v1"])? {
                    if name.ends_with(".js") {
                        let path = ["assets", "v1", &name];
                        let bytes = archive.read(&path)?;
                        if memchr::memmem::find(&bytes, marker.as_bytes()).is_some() {
                            archive.verify_entry(&path, &bytes)?;
                            visit(format!("{ASSETS}/{name}"), bytes)?;
                        }
                    }
                }
            }
            Self::Loose(root) => {
                for entry in fs::read_dir(root.join(ASSETS))? {
                    let path = entry?.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("js") {
                        continue;
                    }
                    let metadata = fs::symlink_metadata(&path)?;
                    ensure!(
                        metadata.is_file()
                            && !metadata.is_symlink()
                            && metadata.len() <= 64 * 1024 * 1024,
                        "原生资源不是安全的普通文件"
                    );
                    let name = path
                        .strip_prefix(root)?
                        .to_str()
                        .context("原生资源路径不是 UTF-8")?
                        .to_owned();
                    visit(name, fs::read(path)?)?;
                }
            }
        }
        found.with_context(|| format!("未找到可识别的{label}入口"))
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Packed(path) => {
                let mut archive = asar::Archive::open(path)?;
                let parts: Vec<_> = name.split('/').collect();
                if archive.contains(&parts)? {
                    Ok(Some(archive.source(&parts)?))
                } else {
                    Ok(None)
                }
            }
            Self::Loose(root) => {
                let path = root.join(name);
                if !path.try_exists()? {
                    ensure!(!path.is_symlink(), "原生资源不允许是符号链接");
                    return Ok(None);
                }
                bounded(root, &path)?;
                Ok(Some(fs::read(path)?))
            }
        }
    }

    fn write(&self, files: &[(String, Vec<u8>)]) -> Result<()> {
        match self {
            Self::Packed(path) => {
                asar::write_files(path, files)?;
            }
            Self::Loose(root) => {
                for (name, bytes) in files {
                    let path = root.join(name);
                    ensure!(!path.is_symlink(), "原生资源不允许是符号链接");
                    bounded(root, path.parent().context("原生资源缺少父目录")?)?;
                    fs::write(path, bytes)?;
                }
            }
        }
        Ok(())
    }

    fn catalogs_present(&self) -> Result<bool> {
        let mut found = 0;
        for (relative, _, expected) in catalogs::RESOURCES {
            if let Some(bytes) = self.read(&format!("i18n/{relative}"))? {
                ensure!(
                    asar::hash(&bytes) == *expected,
                    "官方语言资源校验失败：{relative}"
                );
                found += 1;
            }
        }
        ensure!(
            found == 0 || found == catalogs::RESOURCES.len(),
            "中文词典只安装了一部分"
        );
        Ok(found > 0)
    }
}

pub(super) fn selection(app: &Path) -> Result<Selection> {
    let main = app.join(MAIN);
    bounded(app, &main)?;
    let mut selected = asar::rules_selection(&main, rules::Platform::Macos)?;
    asar::validate_rules(&main)?;
    let renderer = Renderer::open(app)?;
    let (_, fast) = renderer.find(
        "fastModeOffByCredits",
        rules::fast_state_installed,
        "Fast 状态",
    )?;
    ensure!(
        rules::fast_state_installed(&fast)? == selected.fast_mode_status,
        "Fast 原生开关与主进程映射不一致"
    );
    if !selected.fast_mode_status {
        rules::patch_fast_state(&fast)?;
    }
    let (_, language) = renderer.find("startsWith(\"zh-\")", rules::language_installed, "语言")?;
    selected.chinese = rules::language_installed(&language)?;
    if !selected.chinese {
        rules::patch_language(&language)?;
    }
    ensure!(
        selected.chinese == renderer.catalogs_present()?,
        "中文词典与语言入口状态不一致"
    );
    Ok(selected)
}

pub(super) fn patch(app: &Path, selected: Selection) -> Result<()> {
    ensure!(selection(app)?.is_empty(), "必须从官方基线组合原生补丁");
    let renderer = Renderer::open(app)?;
    let mut files = Vec::new();
    if selected.fast_mode_status {
        let (name, source) = renderer.find(
            "fastModeOffByCredits",
            rules::fast_state_installed,
            "Fast 状态",
        )?;
        files.push((name, rules::patch_fast_state(&source)?.into_bytes()));
    }
    if selected.chinese {
        let (name, source) =
            renderer.find("startsWith(\"zh-\")", rules::language_installed, "语言")?;
        files.push((name, rules::patch_language(&source)?.into_bytes()));
        for (relative, bytes, expected) in catalogs::RESOURCES {
            ensure!(asar::hash(bytes) == *expected, "内置官方语言资源校验失败");
            let name = format!("i18n/{relative}");
            ensure!(renderer.read(&name)?.is_none(), "中文资源已存在，拒绝覆盖");
            files.push((name, bytes.to_vec()));
        }
    }
    asar::patch_with_rules(&app.join(MAIN), rules::Platform::Macos, selected)?;
    if !files.is_empty() {
        renderer.write(&files)?;
    }
    ensure!(selection(app)? == selected, "原生功能组合回读验证失败");
    Ok(())
}

use std::path::Path;

use anyhow::{ensure, Context, Result};

use crate::computer_use::{asar, assets::Resources, native::Selection, rules};

const MAIN: &str = "Contents/Resources/app.asar";
pub(super) const PACKED: &str = "Contents/Resources/ion-dist.asar";

fn bounded(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        !path.is_symlink() && path.canonicalize()?.starts_with(root.canonicalize()?),
        "原生资源路径越界"
    );
    Ok(())
}

pub(super) fn selection(app: &Path) -> Result<Selection> {
    let main = app.join(MAIN);
    bounded(app, &main)?;
    let mut selected = asar::rules_selection(&main, rules::Platform::Macos)?;
    asar::validate_rules(&main)?;
    let mut renderer = Resources::at(app.join("Contents/Resources"))?;
    let (_, fast) = renderer.fast_state()?.context("未找到 Fast 状态入口")?;
    ensure!(
        rules::fast_state_installed(&fast)? == selected.fast_mode_status,
        "Fast 原生开关与主进程映射不一致"
    );
    if !selected.fast_mode_status {
        rules::patch_fast_state(&fast)?;
    }
    let (_, lookup) = renderer.fast_model_lookup()?.context("未找到 Fast 模型查找入口")?;
    let lookup_installed = rules::fast_model_lookup_installed(&lookup)?;
    ensure!(!lookup_installed || selected.fast_mode_status, "Fast 模型查找与主进程状态不一致");
    if !lookup_installed {
        rules::patch_fast_model_lookup(&lookup)?;
    }
    let (_, language) = renderer.language()?.context("未找到语言入口")?;
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

pub(super) fn needs_upgrade(app: &Path) -> Result<bool> {
    if asar::rules_upgrade_needed(&app.join(MAIN))? {
        return Ok(true);
    }
    let (_, lookup) = Resources::at(app.join("Contents/Resources"))?
        .fast_model_lookup()?.context("未找到 Fast 模型查找入口")?;
    Ok(!rules::fast_model_lookup_installed(&lookup)?)
}

pub(super) fn patch(app: &Path, selected: Selection) -> Result<()> {
    ensure!(selection(app)?.is_empty(), "必须从官方基线组合原生补丁");
    asar::patch_with_rules(&app.join(MAIN), rules::Platform::Macos, selected)?;
    Resources::at(app.join("Contents/Resources"))?.apply(selected, app)?;
    ensure!(selection(app)? == selected, "原生功能组合回读验证失败");
    Ok(())
}

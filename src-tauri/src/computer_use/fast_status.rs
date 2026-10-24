use std::fs;
use std::path::Path;

use anyhow::{ensure, Context, Result};

use super::{asar, asar::hash, renderer::locate_source};

const ORIGINAL: &str = "3c6299ff0ae0402b635ce84f9c9d7f2fb02183b0f432072d71dc4c7f0d56ce55";
const MODIFIED: &str = "a3ff31e6faec9752e6f54c0646b8431a17879b24e1d1a31be93084289ac9c2fe";
const LEGACY: &str = "c2f24de36bcba7f50d0f65dc15f061c1a3f4c194dee673c3c91af634b9810fef";
const BEFORE: &str = r#"rt=_e??(E?O==="on"||O==="cooldown":Se?!1:$e??(u===null||!t&&re?!1:ie??ne))"#;
const AFTER: &str = r#"rt=_e??(E?O==="on"||O==="cooldown":Se?!1:w&&["on","off","cooldown"].includes(O)?O!=="off":$e??(u===null||!t&&re?!1:ie??ne))"#;
const STATUS_BEFORE: &str = r#"role:"status","aria-live":"polite",className:"sr-only",children:gn"#;
const STATUS_LEGACY: &str = r#"role:"status","aria-live":"polite",children:gn"#;
const STATUS_ORIGINAL: &str = "3fc9692d2daeaff939e813e7bdd3f52b4fd88cc810d1f63145f9e4750f13f326";
const STATUS_MODIFIED: &str = "d5b9470e71137bef21af5e097456fdec57385a5de4d64f4fff534acfe1203bbc";

fn installed_hashes(state: &str, status: &str, native: bool) -> Result<bool> {
    ensure!([ORIGINAL, MODIFIED, LEGACY].contains(&state), "Fast Mode 状态入口内容未适配");
    ensure!([STATUS_ORIGINAL, STATUS_MODIFIED].contains(&status), "Fast Mode 原生组件内容未适配");
    if native {
        ensure!(state == MODIFIED && status == STATUS_ORIGINAL, "Fast Mode 原生开关与状态不匹配");
    } else {
        ensure!(state != MODIFIED && (status == STATUS_ORIGINAL || state == LEGACY), "Fast Mode 原生映射缺失");
    }
    Ok(native)
}

pub fn installed(app: &Path, version: &str) -> Result<bool> {
    if let Some(recipe) = super::macos_recipe::get(version) {
        let selected = asar::active_selection(&app.join("Contents/Resources/app.asar"), version)?.fast_mode_status;
        recipe.fast_widget.inspect(app, false)?;
        recipe.fast_state.inspect(app, selected)?;
        return Ok(selected);
    }
    if version != "3.1623.4" {
        return Ok(false);
    }
    let (_, source) = locate_source(app, BEFORE, AFTER, "Fast Mode 状态")?;
    let (_, status) = locate_source(app, STATUS_BEFORE, STATUS_LEGACY, "Fast Mode 原生组件")?;
    let native = asar::active_selection(&app.join("Contents/Resources/app.asar"), version)?.fast_mode_status;
    if native && !asar::fast_controls_current(&app.join("Contents/Resources/app.asar"), version)? {
        ensure!(hash(source.as_bytes()) == MODIFIED && hash(status.as_bytes()) == STATUS_ORIGINAL, "旧版 Fast Mode 原生映射不完整");
        return Ok(false);
    }
    installed_hashes(&hash(source.as_bytes()), &hash(status.as_bytes()), native)
}

fn patch_source(source: &str) -> Result<String> {
    ensure!(source.matches(BEFORE).count() == 1 && !source.contains(AFTER), "Fast Mode 状态锚点必须唯一且未修改");
    ensure!(hash(source.as_bytes()) == ORIGINAL, "Fast Mode 状态入口内容未适配");
    let modified = source.replacen(BEFORE, AFTER, 1);
    ensure!(hash(modified.as_bytes()) == MODIFIED, "Fast Mode 状态输出哈希不匹配");
    Ok(modified)
}

pub fn patch(app: &Path, version: &str) -> Result<()> {
    if let Some(recipe) = super::macos_recipe::get(version) {
        recipe.fast_widget.inspect(app, false)?;
        return recipe.fast_state.apply(app);
    }
    let (path, source) = locate_source(app, BEFORE, AFTER, "Fast Mode 状态")?;
    let (_, status) = locate_source(app, STATUS_BEFORE, STATUS_LEGACY, "Fast Mode 原生组件")?;
    ensure!(hash(status.as_bytes()) == STATUS_ORIGINAL, "需要未修改的 Fast Mode 原生组件");
    fs::write(path, patch_source(&source)?).context("写入 Fast Mode 状态补丁")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_sources_and_repeated_anchors_are_refused() {
        assert!(patch_source(BEFORE).is_err());
        assert!(patch_source(&format!("{BEFORE};{BEFORE}")).is_err());
        assert!(patch_source(AFTER).is_err());
    }

    #[test]
    fn unrelated_builds_do_not_claim_a_status_patch() {
        assert!(!installed(Path::new("/absent"), "3.1623.1").unwrap());
    }

    #[test]
    fn legacy_status_can_upgrade_but_incomplete_native_controls_are_rejected() {
        assert!(!installed_hashes(ORIGINAL, STATUS_ORIGINAL, false).unwrap());
        assert!(!installed_hashes(LEGACY, STATUS_ORIGINAL, false).unwrap());
        assert!(!installed_hashes(LEGACY, STATUS_MODIFIED, false).unwrap());
        assert!(installed_hashes(MODIFIED, STATUS_ORIGINAL, true).unwrap());
        assert!(installed_hashes(ORIGINAL, STATUS_ORIGINAL, true).is_err());
        assert!(installed_hashes(MODIFIED, STATUS_ORIGINAL, false).is_err());
        assert!(installed_hashes(MODIFIED, STATUS_MODIFIED, true).is_err());
        assert!(installed_hashes("unknown", STATUS_ORIGINAL, true).is_err());
    }
}

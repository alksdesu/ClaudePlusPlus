use serde::{Deserialize, Serialize};

use super::Status;
use anyhow::{ensure, Result};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Selection {
    pub computer_use: bool,
    pub chinese: bool,
    pub simulators: bool,
    pub prewarm: bool,
    pub fast_mode_status: bool,
}

impl Selection {
    pub fn mask(self) -> u8 {
        u8::from(self.computer_use)
            | (u8::from(self.simulators) << 1)
            | (u8::from(self.prewarm) << 2)
            | (u8::from(self.fast_mode_status) << 3)
    }

    pub fn is_empty(self) -> bool {
        self.mask() == 0 && !self.chinese && !self.fast_mode_status
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStatus {
    pub installation: Status,
    pub selection: Selection,
    pub supported_features: Vec<String>,
    pub update: super::updates::Status,
}

pub fn status() -> Result<NativeStatus> {
    let installation = super::status()?;
    let mut selection = Selection::default();
    #[cfg(target_os = "macos")]
    if installation.state == "installed" || installation.state == "upgradeAvailable" {
        selection = super::macos::selection_at(std::path::Path::new(&installation.app_path))?;
    }
    #[cfg(windows)]
    {
        selection.computer_use = installation.state == "installed";
    }
    let mut supported = Vec::new();
    if installation.supported
        && !matches!(
            installation.state.as_str(),
            "notFound" | "unsupported" | "unmanaged" | "changed"
        )
    {
        supported.push("computerUse".into());
        #[cfg(target_os = "macos")]
        if installation.desktop_version.as_deref().is_some_and(supports_extended) {
            supported.extend(["chinese", "simulators", "prewarm", "fastModeStatus"].map(String::from));
        }
    }
    Ok(NativeStatus {
        installation,
        selection,
        supported_features: supported,
        update: super::updates::status(),
    })
}

pub fn apply(identity: &str, selection: Selection) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        super::macos::apply(identity, selection)
    }
    #[cfg(not(target_os = "macos"))]
    {
        ensure!(
            !selection.chinese && !selection.simulators && !selection.prewarm && !selection.fast_mode_status,
            "当前平台未验证这些原生功能补丁"
        );
        if selection.computer_use {
            super::install(identity)
        } else {
            super::restore()
        }
    }
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn supports_extended(version: &str) -> bool {
    version == "3.1623.4" || super::macos_recipe::get(version).is_some()
}

#[cfg(any(test, target_os = "macos"))]
pub(super) fn check_selection(version: &str, selection: Selection) -> Result<()> {
    ensure!(
        supports_extended(version)
            || (!selection.chinese && !selection.simulators && !selection.prewarm && !selection.fast_mode_status),
        "当前 Desktop 尚未验证所选原生功能，不会猜测入口"
    );
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
pub(super) mod language {
    use super::super::asar::hash;
    use anyhow::{ensure, Context, Result};
    use std::fs;
    use std::path::{Path, PathBuf};

    const ORIGINAL: &str = "c73bdcdea4f25a67315e2c25da403a410f1fcbe4b31edc1ccba149a9e59c8f54";
    const MODIFIED: &str = "c22da6602d1295ae78666765b478a0988296a76349fc68af1a4cc6ca6b5f7615";
    const LIST: &str = r#"["en-US","de-DE","fr-FR","ko-KR","ja-JP","es-419","es-ES","it-IT","hi-IN","pt-BR","id-ID"]"#;
    const RESOURCES: &[(&str, &[u8], &str)] = &[
        (
            "zh-Hans.json",
            include_bytes!("resources/zh-Hans.json"),
            "007a0c928759f2a0c7b53154d8b22bbbb36288079182b99fba6a0ac57ce159c0",
        ),
        (
            "zh-Hant.json",
            include_bytes!("resources/zh-Hant.json"),
            "39e36d0c11bd2a8c430c3a9def80abe58e1c526c32733cc32a599aa83708392f",
        ),
        (
            "dynamic/zh-Hans.json",
            include_bytes!("resources/dynamic-zh-Hans.json"),
            "8121dfdeb01237ec4b46a9d252d777da2ee2c5edcf35b885a76fc82810c09fb4",
        ),
        (
            "dynamic/zh-Hant.json",
            include_bytes!("resources/dynamic-zh-Hant.json"),
            "761fe8c8c0bdf03c1aeb1c8c8eca17f8323bdb7dbd45a18dd22479ae4fd8ea5b",
        ),
    ];

    fn locate(app: &Path) -> Result<(PathBuf, String)> {
        let root = app.join("Contents/Resources/ion-dist/assets/v1");
        ensure!(
            root.canonicalize()?.starts_with(app.canonicalize()?),
            "语言资源目录越界"
        );
        let mut found = None;
        for item in fs::read_dir(root)? {
            let path = item?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("js") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                metadata.is_file() && !metadata.is_symlink() && metadata.len() <= 64 * 1024 * 1024,
                "语言入口不是安全的普通文件"
            );
            let text = fs::read_to_string(&path)?;
            if !text.contains("function Wv(e,t=jv)") || !text.contains("i18n-catalogs") {
                continue;
            }
            ensure!(found.is_none(), "语言入口不唯一");
            found = Some((path, text));
        }
        found.context("没有找到已验证的原生语言入口")
    }

    pub fn installed(app: &Path, version: &str) -> Result<bool> {
        let active = if let Some(recipe) = super::super::macos_recipe::get(version) {
            recipe.language.installed(app)?
        } else if version == "3.1623.4" {
            let (_, text) = locate(app)?;
            let digest = hash(text.as_bytes());
            ensure!(digest == ORIGINAL || digest == MODIFIED, "语言入口内容已变化，拒绝覆盖");
            digest == MODIFIED
        } else { false };
        if !active { return Ok(false); }
        let root = app.join("Contents/Resources/ion-dist/i18n");
        for (relative, _, expected) in RESOURCES {
            let path = root.join(relative);
            ensure!(
                path.canonicalize()?.starts_with(app.canonicalize()?)
                    && !fs::symlink_metadata(&path)?.is_symlink(),
                "语言包路径越界"
            );
            ensure!(
                hash(&fs::read(path)?) == *expected,
                "官方语言资源校验失败：{relative}"
            );
        }
        Ok(true)
    }

    fn patch_text(text: &str) -> Result<String> {
        ensure!(
            hash(text.as_bytes()) == ORIGINAL && text.matches(LIST).count() == 2,
            "语言白名单内容未适配"
        );
        let list = format!("{},\"zh-Hans\",\"zh-Hant\"]", &LIST[..LIST.len() - 1]);
        let mut output = text.replace(LIST, &list);
        let start = output.find("function qv(").context("缺少语言回退函数")?;
        let end = output[start..]
            .find("function Jv()")
            .context("缺少语言资源入口")?
            + start;
        output.replace_range(start..end, "function qv(e,t=Kv){return Wv(e,t)}");
        ensure!(hash(output.as_bytes()) == MODIFIED, "语言补丁输出校验失败");
        Ok(output)
    }

    pub fn patch(app: &Path, version: &str) -> Result<()> {
        let recipe = super::super::macos_recipe::get(version);
        let legacy = if recipe.is_none() {
            ensure!(version == "3.1623.4", "当前版本没有语言适配记录");
            let (path, text) = locate(app)?;
            Some((path, patch_text(&text)?))
        } else { None };
        let root = app.join("Contents/Resources/ion-dist/i18n");
        for (relative, bytes, expected) in RESOURCES {
            ensure!(hash(bytes) == *expected, "内置官方语言包校验失败");
            let path = root.join(relative);
            ensure!(
                !path.try_exists()? && !path.is_symlink(),
                "中文资源已存在，拒绝覆盖"
            );
            let parent = path.parent().context("资源缺少父目录")?;
            fs::create_dir_all(parent)?;
            ensure!(
                parent.canonicalize()?.starts_with(app.canonicalize()?),
                "中文资源目录越界"
            );
            fs::write(path, bytes)?;
        }
        if let Some(recipe) = recipe { recipe.language.apply(app)?; }
        if let Some((path, modified)) = legacy { fs::write(path, modified)?; }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn catalogs_are_original_parseable_json() {
            for (_, bytes, expected) in RESOURCES {
                assert_eq!(hash(bytes), *expected);
                assert!(serde_json::from_slice::<serde_json::Value>(bytes)
                    .unwrap()
                    .is_object());
            }
        }
        #[test]
        fn unknown_language_code_is_never_rewritten() {
            assert!(patch_text("function qv(e){return e}").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selections_roundtrip_and_reject_unknown_fields() {
        for mask in 0..16 {
            let s = Selection {
                computer_use: mask & 1 != 0,
                simulators: mask & 2 != 0,
                prewarm: mask & 4 != 0,
                chinese: true,
                fast_mode_status: mask & 8 != 0,
            };
            assert_eq!(s.mask(), mask);
            assert_eq!(
                serde_json::from_value::<Selection>(serde_json::to_value(s).unwrap()).unwrap(),
                s
            );
        }
        assert!(serde_json::from_str::<Selection>(r#"{"disablePermissions":true}"#).is_err());
        assert!(!serde_json::from_str::<Selection>(r#"{"computerUse":true}"#).unwrap().fast_mode_status);
        assert!(!Selection { fast_mode_status: true, ..Default::default() }.is_empty());
    }
    #[test]
    fn older_builds_do_not_receive_unverified_features() {
        assert!(check_selection(
            "3.1623.1",
            Selection {
                computer_use: true,
                ..Default::default()
            }
        )
        .is_ok());
        assert!(check_selection(
            "3.1623.1",
            Selection {
                simulators: true,
                ..Default::default()
            }
        )
        .is_err());
    }
}

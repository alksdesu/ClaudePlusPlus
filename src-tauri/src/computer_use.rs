#[cfg(any(test, windows, target_os = "macos"))]
mod asar;
#[cfg(target_os = "macos")]
mod macos;
pub mod native;
#[cfg(any(test, windows))]
mod pe;
#[cfg(any(test, target_os = "macos"))]
mod renderer;
#[cfg(any(test, target_os = "macos"))]
pub(crate) mod fast_status;
#[cfg(windows)]
mod windows;
#[cfg(any(test, windows))]
mod windows_recipe;

#[cfg(any(test, not(windows)))]
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SigningIdentity {
    pub fingerprint: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub supported: bool,
    pub requires_signing: bool,
    pub app_path: String,
    pub desktop_version: Option<String>,
    pub state: String,
    pub message: String,
    pub identities: Vec<SigningIdentity>,
    pub backup_path: Option<String>,
    pub running: bool,
    pub can_install: bool,
    pub can_restore: bool,
}

#[cfg(target_os = "macos")]
pub use macos::{apply_copy, install, install_copy, restore, restore_copy, status, status_copy};
#[cfg(windows)]
pub use windows::{
    elevated_entry, install, install_copy, restore, restore_copy, status, status_copy,
};

#[cfg(not(any(windows, target_os = "macos")))]
pub fn status() -> Result<Status> {
    Ok(Status {
        supported: false,
        requires_signing: false,
        app_path: String::new(),
        desktop_version: None,
        state: "unsupported".into(),
        message: "原生 Computer Use 集成目前仅支持 Windows 和 macOS".into(),
        identities: Vec::new(),
        backup_path: None,
        running: false,
        can_install: false,
        can_restore: false,
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_identity: &str) -> Result<String> {
    anyhow::bail!("原生 Computer Use 集成目前仅支持 Windows 和 macOS")
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn restore() -> Result<String> {
    anyhow::bail!("原生 Computer Use 集成目前仅支持 Windows 和 macOS")
}

#[cfg(any(test, target_os = "macos"))]
fn parse_identities(text: &str) -> Vec<SigningIdentity> {
    text.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(')')?;
            let (fingerprint, name) = rest.trim().split_once(char::is_whitespace)?;
            if fingerprint.len() != 40 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let name = name.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(SigningIdentity {
                fingerprint: fingerprint.to_uppercase(),
                name: name.into(),
            })
        })
        .collect()
}

#[cfg(any(test, target_os = "macos"))]
fn validate_identity(identity: &str, identities: &[SigningIdentity]) -> Result<()> {
    if !identities.iter().any(|i| i.fingerprint == identity) {
        anyhow::bail!("请选择钥匙串中有效的代码签名身份；不支持 ad-hoc 签名")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_requirement_is_an_explicit_status_capability() {
        for requires_signing in [false, true] {
            let status = Status {
                supported: false,
                requires_signing,
                app_path: String::new(),
                desktop_version: None,
                state: "unsupported".into(),
                message: String::new(),
                identities: Vec::new(),
                backup_path: None,
                running: false,
                can_install: false,
                can_restore: false,
            };
            assert_eq!(
                serde_json::to_value(status).unwrap()["requiresSigning"],
                requires_signing
            );
        }
    }

    #[test]
    fn identities_require_a_complete_valid_fingerprint() {
        let fingerprint = "A".repeat(40);
        let identities = parse_identities(&format!(
            "  1) {fingerprint} \"Apple Development: Test (TEAM)\"\n  2) invalid \"Bad\"\n  1 valid identities found\n"
        ));
        assert_eq!(identities.len(), 1);
        assert!(validate_identity(&fingerprint, &identities).is_ok());
        assert!(validate_identity("-", &identities).is_err());
        assert!(validate_identity("--force", &identities).is_err());
    }
}

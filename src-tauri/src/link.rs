//! 目录链接的跨平台抽象：Windows 用 NTFS junction，Unix 用目录 symlink。
//! 两者对 Desktop（Node readdir）都是透明解引用，语义等价。

use std::io;
use std::path::{Path, PathBuf};

/// 在 `link` 处创建指向 `target` 的目录链接。
#[cfg(windows)]
pub fn create(target: &Path, link: &Path) -> io::Result<()> {
    junction::create(target, link)
}

#[cfg(unix)]
pub fn create(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// `path` 是目录链接时返回其目标，否则 None。
#[cfg(windows)]
pub fn target(path: &Path) -> Option<PathBuf> {
    junction::get_target(path).ok()
}

#[cfg(unix)]
pub fn target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

/// 移除目录链接本身，不触及目标内容。
/// junction 在 NTFS 里是目录，symlink 在 Unix 里是文件，删法不同。
#[cfg(windows)]
pub fn remove(path: &Path) -> io::Result<()> {
    std::fs::remove_dir(path)
}

#[cfg(unix)]
pub fn remove(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// `path` 本身是否为目录链接（不跟随）。
pub fn is_link(path: &Path) -> bool {
    target(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn create_target_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("a.txt"), "x").unwrap();
        let link_path = dir.path().join("view");

        create(&real, &link_path).unwrap();
        assert!(is_link(&link_path));
        assert_eq!(
            fs::canonicalize(target(&link_path).unwrap()).unwrap(),
            fs::canonicalize(&real).unwrap()
        );

        // 链接透明列出目标内容
        let names: Vec<_> = fs::read_dir(&link_path)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));

        remove(&link_path).unwrap();
        assert!(!link_path.exists());
        assert!(real.join("a.txt").exists());
    }

    #[test]
    fn plain_dir_is_not_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        assert!(!is_link(&real));
        assert!(target(&real).is_none());
    }
}

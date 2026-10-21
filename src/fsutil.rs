//! Filesystem helpers: atomic writes, hashing, directory copies.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Write `data` to `path` atomically: a temporary file in the same
/// directory is written, flushed to disk and renamed over `path`, so a crash
/// leaves either the old or the new content, never a partial file.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("路径没有父目录")?;
    std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp-{}", file_name(path), std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("无法写入 {}", tmp.display()))?;
        set_mode(&tmp, mode)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("无法替换 {}", path.display()))?;
    Ok(())
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("无法设置 {} 的权限", path.display()))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Recursively copy `src` into `dst` (regular files and directories only;
/// sockets, devices and symlinks are skipped).  Returns the number of files.
pub fn copy_dir(src: &Path, dst: &Path) -> Result<usize> {
    std::fs::create_dir_all(dst).with_context(|| format!("无法创建目录 {}", dst.display()))?;
    let mut n = 0;
    for entry in std::fs::read_dir(src).with_context(|| format!("无法读取目录 {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            n += copy_dir(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("无法复制 {} 到 {}", entry.path().display(), to.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Move a file or directory, falling back to copy + delete across
/// filesystems.
pub fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if src.is_dir() {
        copy_dir(src, dst)?;
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::copy(src, dst).with_context(|| format!("无法移动 {} 到 {}", src.display(), dst.display()))?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

/// Remove a file or directory if it exists.
pub fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path).with_context(|| format!("无法删除 {}", path.display())),
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("无法删除 {}", path.display())),
        Err(_) => Ok(()),
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A unique, sortable directory name component: `<epoch secs>-<pid>`.
pub fn stamp() -> String {
    format!("{}-{}", now(), std::process::id())
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Children of `dir` (empty if it doesn't exist).
pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/state.json");
        atomic_write(&p, b"one", 0o600).unwrap();
        atomic_write(&p, b"two", 0o600).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(list_dir(&d.path().join("sub")).len(), 1, "no temporary files left behind");
    }

    #[test]
    fn copy_and_hash() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f"), b"abc").unwrap();
        assert_eq!(copy_dir(&d.path().join("a"), &d.path().join("c")).unwrap(), 1);
        assert_eq!(
            sha256_file(&d.path().join("c/b/f")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

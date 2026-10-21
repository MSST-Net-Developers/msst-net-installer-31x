//! 3.0.x installations: detection, backup before migration, and restore.
//!
//! Isolation rules:
//! - 3.1 files never overwrite 3.0 files: 3.1 lives under `versions/`, and
//!   3.0's binaries are *moved* into a backup, not deleted;
//! - nothing of 3.0 is touched until every 3.1 artifact has been downloaded
//!   and verified;
//! - Core's data (identity, config) is copied into the backup first; Core
//!   3.1 then keeps using the 3.0 data directory in place, so the device
//!   keeps its keys, IDs and virtual IPs;
//! - if 3.1 doesn't come up healthy, the backup is put back and 3.0 restarted.

use crate::cmd;
use crate::fsutil;
use crate::layout::{self, Layout};
use crate::platform::Os;
use crate::service::{self, Service};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The 3.0.x installer's Tauri launcher (Linux).
const TAURI_WRAPPER: &str = "/usr/local/bin/msst-net-tauri";
/// 3.0.x release assets all start with this.
const LEGACY_PREFIX: &str = "msst-net-client-";

#[derive(Debug, Default)]
pub struct LegacyNative {
    /// 3.0.x binaries in the install root (core, controller, wintun.dll).
    pub files: Vec<PathBuf>,
    /// The Core service runs a 3.0.x binary.
    pub service: bool,
    pub tauri_wrapper: Option<PathBuf>,
    /// Installed packages that look like the 3.0.x Tauri Controller.
    pub packages: Vec<String>,
}

impl LegacyNative {
    pub fn describe(&self) -> Vec<String> {
        let mut v: Vec<String> = self.files.iter().map(|f| format!("文件 {}", f.display())).collect();
        if self.service {
            v.push("系统服务 msst-net（运行 3.0.x 核心）".into());
        }
        if let Some(w) = &self.tauri_wrapper {
            v.push(format!("Tauri 启动脚本 {}", w.display()));
        }
        v.extend(self.packages.iter().map(|p| format!("软件包 {p}（3.0.x 桌面控制器）")));
        v
    }
}

/// Find a 3.0.x native installation (only meaningful when no 3.1 state exists).
pub fn detect_native(layout: &Layout) -> Option<LegacyNative> {
    let os = layout.os;
    let mut found = LegacyNative {
        files: fsutil::list_dir(&layout.root)
            .into_iter()
            .filter(|p| p.is_file())
            .filter(|p| {
                let n = fsutil::file_name(p);
                n.starts_with(LEGACY_PREFIX) || n.eq_ignore_ascii_case("wintun.dll")
            })
            .collect(),
        service: service::current_exe(os, Service::Core).is_some_and(|e| e.contains(LEGACY_PREFIX)),
        tauri_wrapper: (os == Os::Linux).then(|| PathBuf::from(TAURI_WRAPPER)).filter(|p| p.exists()),
        packages: if os == Os::Linux { legacy_packages() } else { Vec::new() },
    };
    // wintun.dll alone is not a 3.0 installation.
    if !found.files.iter().any(|p| fsutil::file_name(p).starts_with(LEGACY_PREFIX)) && !found.service {
        found.files.clear();
    }
    (!found.files.is_empty() || found.service || found.tauri_wrapper.is_some()).then_some(found)
}

fn legacy_packages() -> Vec<String> {
    let list = if cmd::exists("dpkg-query") {
        cmd::output("dpkg-query", &["-W", "-f", "${Package}\n"]).unwrap_or_default()
    } else if cmd::exists("rpm") {
        cmd::output("rpm", &["-qa", "--qf", "%{NAME}\n"]).unwrap_or_default()
    } else {
        String::new()
    };
    list.lines()
        .map(str::trim)
        .filter(|n| n.contains("msst") && *n != crate::desktop::PACKAGE_NAME)
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Program file moved out of the way (restored by moving back).
    Moved,
    /// Data or definition copied (restored by copying back).
    Copied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub original: PathBuf,
    /// Relative to the backup directory.
    pub stored: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub created_at: u64,
    pub label: String,
    pub entries: Vec<Entry>,
    /// Windows: the Core service's binPath before migration.
    #[serde(default)]
    pub windows_bin_path: Option<String>,
}

pub struct Backup {
    pub dir: PathBuf,
    pub record: BackupRecord,
}

const RECORD: &str = "backup.json";

impl Backup {
    pub fn create(layout: &Layout, label: &str) -> Result<Backup> {
        let dir = layout.backups_dir().join(format!("{}-{label}", fsutil::stamp()));
        std::fs::create_dir_all(&dir).with_context(|| format!("无法创建备份目录 {}", dir.display()))?;
        fsutil::set_mode(&dir, 0o700)?;
        let b = Backup { dir, record: BackupRecord { created_at: fsutil::now(), label: label.into(), entries: Vec::new(), windows_bin_path: None } };
        b.save()?;
        Ok(b)
    }

    pub fn open(dir: &Path) -> Result<Backup> {
        let bytes = std::fs::read(dir.join(RECORD)).with_context(|| format!("{} 不是有效的备份", dir.display()))?;
        Ok(Backup { dir: dir.into(), record: serde_json::from_slice(&bytes)? })
    }

    pub fn save(&self) -> Result<()> {
        fsutil::atomic_write(&self.dir.join(RECORD), &serde_json::to_vec_pretty(&self.record)?, 0o600)
    }

    fn slot(&self, kind: EntryKind, original: &Path) -> PathBuf {
        let sub = if kind == EntryKind::Moved { "files" } else { "copies" };
        PathBuf::from(sub).join(format!("{}-{}", self.record.entries.len(), fsutil::file_name(original)))
    }

    /// Copy a file or directory into the backup.
    pub fn copy(&mut self, original: &Path) -> Result<()> {
        let stored = self.slot(EntryKind::Copied, original);
        let to = self.dir.join(&stored);
        if original.is_dir() {
            fsutil::copy_dir(original, &to)?;
        } else {
            std::fs::create_dir_all(to.parent().unwrap())?;
            std::fs::copy(original, &to).with_context(|| format!("无法备份 {}", original.display()))?;
        }
        self.record.entries.push(Entry { kind: EntryKind::Copied, original: original.into(), stored });
        self.save()
    }

    /// Move a file or directory into the backup.  Recorded before the move,
    /// so an interrupted move can still be found.
    pub fn take(&mut self, original: &Path) -> Result<()> {
        let stored = self.slot(EntryKind::Moved, original);
        self.record.entries.push(Entry { kind: EntryKind::Moved, original: original.into(), stored: stored.clone() });
        self.save()?;
        fsutil::move_path(original, &self.dir.join(stored)).with_context(|| format!("无法移动 {}", original.display()))
    }

    /// Put everything back: moved files are moved back, copies are copied
    /// over the current content (files the backup doesn't have are kept).
    pub fn restore(&self) -> Result<()> {
        for e in self.record.entries.iter().rev() {
            let from = self.dir.join(&e.stored);
            if !from.exists() {
                continue;
            }
            match e.kind {
                EntryKind::Moved => {
                    fsutil::remove_any(&e.original)?;
                    fsutil::move_path(&from, &e.original)?;
                }
                EntryKind::Copied if from.is_dir() => {
                    fsutil::copy_dir(&from, &e.original)?;
                }
                EntryKind::Copied => {
                    if let Some(p) = e.original.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::copy(&from, &e.original)?;
                }
            }
        }
        Ok(())
    }
}

/// Snapshot Core's data and config directories (identity!) into `backup`.
pub fn snapshot_data(os: Os, home: &Path, backup: &mut Backup) -> Result<Vec<PathBuf>> {
    let mut saved = Vec::new();
    for loc in layout::data_locations(os, home) {
        if loc.path.is_dir() {
            backup.copy(&loc.path)?;
            saved.push(loc.path);
        }
    }
    Ok(saved)
}

/// Stop 3.0.x and move its program files into `backup`.  The Core service
/// definition is copied (the 3.1 definition replaces it under the same
/// name).
pub fn retire_native(os: Os, legacy: &LegacyNative, backup: &mut Backup) -> Result<()> {
    service::stop(os, Service::Core);
    match os {
        Os::Windows => {
            backup.record.windows_bin_path = service::current_exe(os, Service::Core);
            backup.save()?;
        }
        _ => {
            if let Some(def) = Service::Core.definition_path(os).filter(|p| p.exists()) {
                backup.copy(&def)?;
            }
        }
    }
    for f in &legacy.files {
        backup.take(f)?;
    }
    if let Some(w) = &legacy.tauri_wrapper {
        backup.take(w)?;
    }
    Ok(())
}

/// Undo [`retire_native`] (and the data snapshot): 3.0.x runs again.
pub fn restore_native(os: Os, backup: &Backup) -> Result<()> {
    service::stop(os, Service::Core);
    backup.restore()?;
    match os {
        Os::Linux => {
            cmd::run_quiet("systemctl", &["daemon-reload"])?;
            cmd::run_quiet("systemctl", &["enable", Service::Core.name(os)])?;
        }
        Os::Windows => {
            if let Some(bin) = &backup.record.windows_bin_path {
                cmd::run_quiet("sc", &["config", Service::Core.name(os), "binPath=", bin])?;
            }
        }
        Os::Macos => {}
    }
    service::start(os, Service::Core)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_3_0_files_only() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { os: Os::Macos, root: d.path().into() };
        // Not a 3.0.x installation: only wintun / 3.1 layout.
        std::fs::write(d.path().join("wintun.dll"), b"x").unwrap();
        std::fs::create_dir_all(d.path().join("versions/3.1.0")).unwrap();
        assert!(detect_native(&layout).is_none());
        std::fs::write(d.path().join("msst-net-client-core-macos-aarch64"), b"x").unwrap();
        let found = detect_native(&layout).unwrap();
        assert_eq!(found.files.len(), 2, "core and wintun.dll");
    }

    #[test]
    fn backup_take_copy_restore() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { os: Os::Linux, root: d.path().join("root") };
        let bin = d.path().join("root/msst-net-client-core-linux-x86_64");
        let data = d.path().join("home/MSST-Net");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"old core").unwrap();
        std::fs::write(data.join("identity.json"), b"keys").unwrap();

        let mut b = Backup::create(&layout, "3.0.x").unwrap();
        b.copy(&data).unwrap();
        b.take(&bin).unwrap();
        assert!(!bin.exists(), "3.0 binary moved out of the install root");

        // 3.1 modifies data and adds files.
        std::fs::write(data.join("identity.json"), b"changed").unwrap();
        std::fs::write(data.join("peer-cache.json"), b"new").unwrap();

        let reopened = Backup::open(&b.dir).unwrap();
        reopened.restore().unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"old core");
        assert_eq!(std::fs::read(data.join("identity.json")).unwrap(), b"keys");
        assert!(data.join("peer-cache.json").exists(), "restore never deletes data");
    }
}

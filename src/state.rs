//! `installer-state.json`: what this installer has installed.  Written
//! atomically; `pending` marks a transaction in progress so an interrupted
//! run is detected (and fixed by `repair`) next time.

use crate::fsutil;
use crate::layout::Layout;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deployment {
    /// Core (and the WebUI, if chosen) as system services.
    Native,
    /// Core (and the WebUI, if chosen) as containers (Linux).
    Docker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControllerKind {
    /// Compose Desktop application (replaces the 3.0.x Tauri app).
    Desktop,
    Webui,
    /// Command-line Controller `msst-net-cli` (run by users in a terminal;
    /// on the host also when Core runs in Docker).
    Cli,
    /// Core only (headless; manage it from another device's WebUI later).
    None,
}

impl ControllerKind {
    pub fn label(self) -> &'static str {
        match self {
            ControllerKind::Desktop => "桌面版（Compose Desktop）",
            ControllerKind::Webui => "WebUI",
            ControllerKind::Cli => "命令行（msst-net-cli）",
            ControllerKind::None => "无（仅核心）",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesktopInstall {
    pub version: String,
    pub format: crate::manifest::Format,
    /// Where it was installed: package name (deb/rpm), kept MSI path,
    /// .app path or distributable directory.
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub action: String,
    pub target: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: u64,
    pub action: String,
    pub version: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub schema: u32,
    pub installer_version: String,
    pub deployment: Deployment,
    pub controller: ControllerKind,
    /// Installed (active) version.
    pub version: String,
    /// The version before the last upgrade (rollback target), if kept.
    #[serde(default)]
    pub previous: Option<String>,
    /// Release source used last (`github`, `gitee`, `dir:<path>`).
    pub source: String,
    /// WebUI installed as a system service (native deployment).
    #[serde(default)]
    pub webui_service: bool,
    #[serde(default)]
    pub desktop: Option<DesktopInstall>,
    /// Users added to the admin group by this installer.
    #[serde(default)]
    pub admin_users: Vec<String>,
    /// Docker: image references in use and before the last upgrade.
    #[serde(default)]
    pub images: Option<crate::manifest::Images>,
    #[serde(default)]
    pub previous_images: Option<crate::manifest::Images>,
    /// Backup of a 3.0.x installation migrated by this installer.
    #[serde(default)]
    pub legacy_backup: Option<PathBuf>,
    #[serde(default)]
    pub pending: Option<Pending>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

impl InstallState {
    pub fn new(deployment: Deployment, controller: ControllerKind, version: &str, source: &str) -> InstallState {
        InstallState {
            schema: SCHEMA,
            installer_version: env!("CARGO_PKG_VERSION").into(),
            deployment,
            controller,
            version: version.into(),
            previous: None,
            source: source.into(),
            webui_service: false,
            desktop: None,
            admin_users: Vec::new(),
            images: None,
            previous_images: None,
            legacy_backup: None,
            pending: None,
            history: Vec::new(),
        }
    }

    pub fn load(layout: &Layout) -> Result<Option<InstallState>> {
        let path = layout.state_file();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).with_context(|| format!("无法读取 {}", path.display()))?;
        let state: InstallState =
            serde_json::from_slice(&bytes).with_context(|| format!("{} 已损坏，请检查或删除后执行修复安装", path.display()))?;
        if state.schema > SCHEMA {
            bail!("{} 由更新的安装器写入（格式 {}），请使用新版安装器。", path.display(), state.schema);
        }
        Ok(Some(state))
    }

    pub fn save(&mut self, layout: &Layout) -> Result<()> {
        self.installer_version = env!("CARGO_PKG_VERSION").into();
        // Bounded history.
        if self.history.len() > 50 {
            self.history.drain(..self.history.len() - 50);
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        fsutil::atomic_write(&layout.state_file(), &bytes, 0o644)
    }

    pub fn record(&mut self, action: &str, version: &str, ok: bool, note: Option<String>) {
        self.history.push(HistoryEntry { at: fsutil::now(), action: action.into(), version: version.into(), ok, note });
    }

    pub fn begin(&mut self, layout: &Layout, action: &str, target: &str) -> Result<()> {
        self.pending = Some(Pending { action: action.into(), target: target.into(), started_at: fsutil::now() });
        self.save(layout)
    }
}

/// Exclusive lock on the installation, held for the life of the process.
pub struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    pub fn acquire(layout: &Layout) -> Result<Lock> {
        std::fs::create_dir_all(&layout.root).with_context(|| format!("无法创建 {}", layout.root.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(layout.lock_file())
            .context("无法创建锁文件")?;
        match file.try_lock() {
            Ok(()) => Ok(Lock(file)),
            Err(std::fs::TryLockError::WouldBlock) => bail!("另一个安装程序正在运行，请等待其完成。"),
            Err(std::fs::TryLockError::Error(e)) => Err(e).context("无法锁定安装目录"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Os;

    #[test]
    fn state_round_trip_and_lock() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { os: Os::Linux, root: d.path().into() };
        assert!(InstallState::load(&layout).unwrap().is_none());
        let mut s = InstallState::new(Deployment::Native, ControllerKind::Webui, "3.1.0", "gitee");
        s.begin(&layout, "install", "3.1.0").unwrap();
        let loaded = InstallState::load(&layout).unwrap().unwrap();
        assert_eq!(loaded.pending.unwrap().target, "3.1.0");

        let _lock = Lock::acquire(&layout).unwrap();
        assert!(Lock::acquire(&layout).is_err(), "second installer is refused");
    }

    #[test]
    fn corrupt_state_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { os: Os::Linux, root: d.path().into() };
        std::fs::write(layout.state_file(), b"{").unwrap();
        assert!(InstallState::load(&layout).is_err());
    }
}

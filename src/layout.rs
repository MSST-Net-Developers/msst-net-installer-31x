//! Where things live on disk.
//!
//! Program files (owned by the installer, replaceable at any time):
//!
//! ```text
//! <root>/                     /opt/msst-net, /usr/local/msst-net, %ProgramFiles%\MSST-Net
//!   installer-state.json      what is installed (see state.rs)
//!   .lock                     held while the installer runs
//!   versions/<version>/       msst-net-core, msst-net-webui, wintun.dll, manifest.json(.sig)
//!   staging/<stamp>/          downloads in progress — never referenced by a service
//!   desktop/<version>/        desktop Controller (tar.gz distributable only)
//!   backups/<stamp>-<label>/  3.0.x files and data snapshots taken before migration
//! ```
//!
//! User data (owned by Core / the WebUI, never deleted without `--purge`):
//! see [`data_locations`].  The 3.0.x installer used the same root but put
//! its binaries directly inside it; 3.1 never writes there, so the two
//! layouts can't collide.

use crate::platform::Os;
use std::path::{Path, PathBuf};

/// The 3.0.x and 3.1.x Docker deployment directory (Linux only).
pub const DOCKER_DIR: &str = "/opt/msst-net-docker";

#[derive(Clone, Debug)]
pub struct Layout {
    pub os: Os,
    pub root: PathBuf,
}

impl Layout {
    /// The default layout, or `MSST_INSTALL_ROOT` when set (tests, custom
    /// prefixes).
    pub fn new(os: Os) -> Layout {
        let root = std::env::var_os("MSST_INSTALL_ROOT").map(PathBuf::from).unwrap_or_else(|| default_root(os));
        Layout { os, root }
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("installer-state.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join(".lock")
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions_dir().join(version)
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn desktop_dir(&self, version: &str) -> PathBuf {
        self.root.join("desktop").join(version)
    }

    pub fn core_exe(&self, version: &str) -> PathBuf {
        self.version_dir(version).join(format!("msst-net-core{}", self.os.exe_suffix()))
    }

    pub fn webui_exe(&self, version: &str) -> PathBuf {
        self.version_dir(version).join(format!("msst-net-webui{}", self.os.exe_suffix()))
    }

    pub fn docker_dir(&self) -> PathBuf {
        std::env::var_os("MSST_DOCKER_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DOCKER_DIR))
    }
}

fn default_root(os: Os) -> PathBuf {
    match os {
        Os::Linux => PathBuf::from("/opt/msst-net"),
        Os::Macos => PathBuf::from("/usr/local/msst-net"),
        Os::Windows => {
            let base = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
            PathBuf::from(base).join("MSST-Net")
        }
    }
}

/// The native WebUI service's config directory (`MSST_WEBUI_CONFIG_DIR`):
/// password hash, config.toml, TLS certificate.
pub fn webui_config_dir(os: Os) -> PathBuf {
    match os {
        Os::Linux => PathBuf::from("/var/lib/msst-net-webui"),
        Os::Macos => PathBuf::from("/Library/Application Support/MSST-Net/webui"),
        Os::Windows => {
            let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
            PathBuf::from(base).join(r"MSST-Net\webui")
        }
    }
}

/// A directory holding data that must survive upgrades and uninstalls.
#[derive(Clone, Debug)]
pub struct DataLocation {
    pub label: &'static str,
    pub path: PathBuf,
}

/// Core's data/config directories for the service account, following Core
/// 3.1's own resolution (`config.rs`: `resolve_data_dir`,
/// `default_config_path`, `legacy_data_dir`), and the WebUI's.
pub fn data_locations(os: Os, home: &Path) -> Vec<DataLocation> {
    let (data, config) = match os {
        Os::Linux => (home.join(".local/share/msst-net"), home.join(".config/msst-net")),
        Os::Macos => (home.join("Library/Application Support/msst-net"), home.join("Library/Application Support/msst-net")),
        Os::Windows => (home.join(r"AppData\Roaming\msst-net"), home.join(r"AppData\Roaming\msst-net")),
    };
    let mut v = vec![
        // 3.0.x data dir; Core 3.1 keeps using it while it holds identity.json.
        DataLocation { label: "3.0.x 数据目录（身份）", path: home.join("MSST-Net") },
        DataLocation { label: "Core 数据目录（身份、缓存）", path: data.clone() },
    ];
    if config != data {
        v.push(DataLocation { label: "Core 配置（core.toml）", path: config });
    }
    v.push(DataLocation { label: "WebUI 配置（密码、证书）", path: webui_config_dir(os) });
    v
}

/// The data directory Core 3.1 will use, as Core decides it: the 3.0.x
/// directory while it holds an identity, otherwise the 3.1 default.
pub fn effective_core_data_dir(os: Os, home: &Path) -> PathBuf {
    let locs = data_locations(os, home);
    if locs[0].path.join("identity.json").exists() { locs[0].path.clone() } else { locs[1].path.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_data_dir_prefers_3_0_identity() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(effective_core_data_dir(Os::Linux, home.path()), home.path().join(".local/share/msst-net"));
        std::fs::create_dir_all(home.path().join("MSST-Net")).unwrap();
        assert_eq!(effective_core_data_dir(Os::Linux, home.path()), home.path().join(".local/share/msst-net"));
        std::fs::write(home.path().join("MSST-Net/identity.json"), "{}").unwrap();
        assert_eq!(effective_core_data_dir(Os::Linux, home.path()), home.path().join("MSST-Net"));
    }

    #[test]
    fn layout_paths() {
        let l = Layout { os: Os::Windows, root: PathBuf::from("R") };
        assert_eq!(l.core_exe("3.1.0"), PathBuf::from("R").join("versions").join("3.1.0").join("msst-net-core.exe"));
        let l = Layout { os: Os::Linux, root: PathBuf::from("/opt/msst-net") };
        assert_eq!(l.webui_exe("3.1.0"), PathBuf::from("/opt/msst-net/versions/3.1.0/msst-net-webui"));
    }
}

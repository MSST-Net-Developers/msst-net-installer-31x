//! The desktop Controller: a Compose Desktop (JVM) application packaged by
//! jpackage (`compose/desktopApp`, package name `msst-net-controller`).  It
//! replaces the 3.0.x Tauri app, so none of the Tauri/WebKitGTK workarounds
//! (AppImage, EGL launcher script) apply any more.
//!
//! | OS | Preferred | Fallback |
//! |---|---|---|
//! | Linux | `.deb` (dpkg systems) / `.rpm` (rpm systems) | `.tar.gz` distributable + launcher |
//! | Windows | `.msi` | — |
//! | macOS | `.dmg` | — |
//!
//! The app runs unprivileged and talks to the Core service over IPC; users
//! get the admin role through the admin group (accounts.rs).

use crate::cmd;
use crate::fsutil;
use crate::layout::Layout;
use crate::manifest::{Artifact, Component, Format, Manifest};
use crate::platform::{Arch, Os};
use crate::state::DesktopInstall;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const PACKAGE_NAME: &str = "msst-net-controller";
const LAUNCHER: &str = "/usr/local/bin/msst-net-controller";
const DESKTOP_ENTRY: &str = "/usr/share/applications/msst-net-controller.desktop";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxPkg {
    Deb,
    Rpm,
    Other,
}

fn linux_pkg() -> LinuxPkg {
    if cmd::exists("dpkg") && (Path::new("/etc/debian_version").exists() || !cmd::exists("rpm")) {
        LinuxPkg::Deb
    } else if cmd::exists("rpm") {
        LinuxPkg::Rpm
    } else {
        LinuxPkg::Other
    }
}

/// The desktop artifact to install on this system, in preference order.
pub fn select(manifest: &Manifest, os: Os, arch: Arch) -> Result<&Artifact> {
    let prefs: &[Format] = match os {
        Os::Linux => match linux_pkg() {
            LinuxPkg::Deb => &[Format::Deb, Format::TarGz],
            LinuxPkg::Rpm => &[Format::Rpm, Format::TarGz],
            LinuxPkg::Other => &[Format::TarGz],
        },
        Os::Windows => &[Format::Msi],
        Os::Macos => &[Format::Dmg],
    };
    prefs
        .iter()
        .find_map(|f| manifest.find(Component::Desktop, os, arch, *f))
        .with_context(|| format!("版本 {} 未提供适用于 {}/{} 的桌面控制器", manifest.version, os.display_name(), arch.as_str()))
}

/// Install (or upgrade) the desktop Controller from a verified package.
pub fn install(layout: &Layout, artifact: &Artifact, package: &Path, version: &str, previous: Option<&DesktopInstall>) -> Result<DesktopInstall> {
    let location = match artifact.format {
        Format::Deb => {
            let p = package.to_string_lossy();
            // apt resolves dependencies; plain dpkg is the fallback.
            if cmd::exists("apt-get") {
                cmd::run("apt-get", &["install", "-y", "--allow-downgrades", &p])?;
            } else {
                cmd::run("dpkg", &["-i", &p])?;
            }
            PACKAGE_NAME.to_string()
        }
        Format::Rpm => {
            let p = package.to_string_lossy();
            let done = ["dnf", "yum", "zypper"].iter().filter(|pm| cmd::exists(pm)).any(|pm| {
                let args: Vec<&str> =
                    if *pm == "zypper" { vec!["--non-interactive", "install", "--allow-unsigned-rpm", &p] } else { vec!["install", "-y", &p] };
                cmd::run(pm, &args).is_ok()
            });
            if !done {
                cmd::run("rpm", &["-Uvh", "--replacepkgs", &p])?;
            }
            PACKAGE_NAME.to_string()
        }
        Format::Msi => {
            // jpackage MSIs are only upgraded in place when every version
            // shares an upgrade code; remove the previous one explicitly so
            // two versions never end up installed side by side.
            if let Some(prev) = previous.filter(|p| p.format == Format::Msi) {
                let _ = cmd::run("msiexec", &["/x", &prev.location, "/qn", "/norestart"]);
            }
            // Keep the MSI: uninstalling needs it.
            let keep = layout.desktop_dir(version).join(&artifact.name);
            std::fs::create_dir_all(keep.parent().unwrap())?;
            std::fs::copy(package, &keep)?;
            let log = layout.desktop_dir(version).join("msiexec.log");
            cmd::run("msiexec", &[
                "/i".as_ref(),
                keep.as_os_str(),
                "/qn".as_ref(),
                "/norestart".as_ref(),
                "/l*v".as_ref(),
                log.as_os_str(),
            ])
            .with_context(|| format!("MSI 安装失败，日志：{}", log.display()))?;
            keep.to_string_lossy().into_owned()
        }
        Format::Dmg => install_dmg(package)?,
        Format::TarGz => install_distributable(layout, package, version)?,
        other => bail!("不支持的桌面控制器格式 {other:?}"),
    };
    Ok(DesktopInstall { version: version.into(), format: artifact.format, location })
}

fn install_dmg(package: &Path) -> Result<String> {
    let mount = std::env::temp_dir().join(format!("msst-net-dmg-{}", std::process::id()));
    std::fs::create_dir_all(&mount)?;
    cmd::run_quiet("hdiutil", &["attach", "-nobrowse", "-readonly", "-mountpoint", &mount.to_string_lossy(), &package.to_string_lossy()])?;
    let result = (|| -> Result<String> {
        let app = fsutil::list_dir(&mount)
            .into_iter()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .context("磁盘映像中没有 .app")?;
        let dest = PathBuf::from("/Applications").join(app.file_name().unwrap());
        let staged = dest.with_extension("app.new");
        fsutil::remove_any(&staged)?;
        cmd::run_quiet("ditto", &[app.as_os_str(), staged.as_os_str()])?;
        fsutil::remove_any(&dest)?;
        std::fs::rename(&staged, &dest)?;
        Ok(dest.to_string_lossy().into_owned())
    })();
    let _ = cmd::run_quiet("hdiutil", &["detach", &mount.to_string_lossy()]);
    let _ = std::fs::remove_dir(&mount);
    result
}

/// Linux without deb/rpm: unpack the jpackage app image under
/// `<root>/desktop/<version>/`, point the launcher and menu entry at it.
fn install_distributable(layout: &Layout, package: &Path, version: &str) -> Result<String> {
    let dir = layout.desktop_dir(version);
    fsutil::remove_any(&dir)?;
    std::fs::create_dir_all(&dir)?;
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(package)?);
    // `unpack` refuses entries that would land outside `dir`.
    tar::Archive::new(gz).unpack(&dir).context("无法解压桌面控制器")?;
    let exe = find_launcher(&dir).context("压缩包中没有 bin/msst-net-controller")?;
    let link = Path::new(LAUNCHER);
    fsutil::remove_any(link)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&exe, link)?;
    let icon = exe.parent().and_then(Path::parent).map(|p| p.join("lib/msst-net-controller.png")).filter(|p| p.exists());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=MSST-Net\nComment=MSST-Net Controller\nExec={LAUNCHER}\n{}Categories=Network;\nTerminal=false\n",
        icon.map(|i| format!("Icon={}\n", i.display())).unwrap_or_default()
    );
    fsutil::atomic_write(Path::new(DESKTOP_ENTRY), entry.as_bytes(), 0o644)?;
    Ok(dir.to_string_lossy().into_owned())
}

fn find_launcher(dir: &Path) -> Option<PathBuf> {
    for p in fsutil::list_dir(dir) {
        let candidate = p.join("bin").join(PACKAGE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if p.is_dir()
            && let Some(found) = find_launcher(&p)
        {
            return Some(found);
        }
    }
    None
}

pub fn is_installed(d: &DesktopInstall) -> bool {
    match d.format {
        Format::Deb => cmd::succeeds("dpkg", &["-s", PACKAGE_NAME]),
        Format::Rpm => cmd::succeeds("rpm", &["-q", PACKAGE_NAME]),
        Format::Dmg => Path::new(&d.location).exists(),
        Format::TarGz => Path::new(&d.location).exists() && Path::new(LAUNCHER).exists(),
        // The MSI's own uninstall registration isn't looked up; trust the record.
        _ => Path::new(&d.location).exists(),
    }
}

pub fn uninstall(d: &DesktopInstall) -> Result<()> {
    match d.format {
        Format::Deb => {
            if cmd::exists("apt-get") {
                cmd::run("apt-get", &["remove", "-y", PACKAGE_NAME])
            } else {
                cmd::run("dpkg", &["-r", PACKAGE_NAME])
            }
        }
        Format::Rpm => cmd::run("rpm", &["-e", PACKAGE_NAME]),
        Format::Msi => cmd::run("msiexec", &["/x", &d.location, "/qn", "/norestart"]),
        Format::Dmg => fsutil::remove_any(Path::new(&d.location)),
        Format::TarGz => {
            fsutil::remove_any(Path::new(LAUNCHER))?;
            fsutil::remove_any(Path::new(DESKTOP_ENTRY))?;
            fsutil::remove_any(Path::new(&d.location))
        }
        other => bail!("不支持的桌面控制器格式 {other:?}"),
    }
}

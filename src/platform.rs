//! Operating system / CPU detection and privilege checks.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Windows,
    Macos,
}

impl Os {
    /// The OS this installer was built for (each release asset is built per OS).
    pub fn current() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
            Os::Macos => "macos",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Os::Linux => "Linux",
            Os::Windows => "Windows",
            Os::Macos => "macOS",
        }
    }

    pub fn exe_suffix(self) -> &'static str {
        if self == Os::Windows { ".exe" } else { "" }
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn current() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }


    /// Directory name inside the wintun release zip.
    pub fn wintun_dir(self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }
}

#[cfg(unix)]
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn is_elevated() -> bool {
    // `net session` only succeeds in an elevated process.
    std::process::Command::new("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Home directory of the account the Core service runs as (root /
/// LocalSystem).  Core resolves its data and config directories from it.
pub fn service_account_home(os: Os) -> PathBuf {
    match os {
        Os::Windows => {
            let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            PathBuf::from(sysroot).join(r"System32\config\systemprofile")
        }
        Os::Macos => home_of("root").unwrap_or_else(|| PathBuf::from("/var/root")),
        Os::Linux => home_of("root").unwrap_or_else(|| PathBuf::from("/root")),
    }
}

#[cfg(unix)]
pub fn home_of(user: &str) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};
    let name = CString::new(user).ok()?;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0 as libc::c_char; 4096];
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pw.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { CStr::from_ptr(pw.pw_dir) }.to_string_lossy().into_owned();
    Some(PathBuf::from(dir))
}

#[cfg(not(unix))]
pub fn home_of(_user: &str) -> Option<PathBuf> {
    None
}

/// The person who started the installer, when it can be told apart from the
/// elevated account: `SUDO_USER` on Unix, `USERNAME` on Windows.
pub fn invoking_user() -> Option<String> {
    let var = if cfg!(windows) { "USERNAME" } else { "SUDO_USER" };
    std::env::var(var).ok().filter(|u| !u.is_empty() && u != "root")
}

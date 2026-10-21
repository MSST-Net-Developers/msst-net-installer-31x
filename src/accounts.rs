//! Accounts behind Core's IPC permission model (Core `ipc/auth.rs`,
//! `ipc/windows_pipe.rs`):
//!
//! - **admin**: root / LocalSystem, elevated Administrators, and members of
//!   the local admin group (`msst-net`; Windows `MSST-Net Administrators`);
//! - **viewer**: other logged-in users (read-only Controller);
//! - everyone else is refused.
//!
//! So the installer creates the admin group and adds the people who should
//! manage MSST-Net to it, instead of loosening socket permissions.  The
//! native WebUI runs as its own system user whose primary group is the
//! admin group (Core matches the primary group or the member list, not the
//! process's supplementary groups).

use crate::cmd;
use crate::platform::Os;
use anyhow::{Context, Result, bail};

pub const ADMIN_GROUP_UNIX: &str = "msst-net";
pub const ADMIN_GROUP_WINDOWS: &str = "MSST-Net Administrators";
pub const WEBUI_USER: &str = "msst-webui";

pub fn admin_group(os: Os) -> &'static str {
    if os == Os::Windows { ADMIN_GROUP_WINDOWS } else { ADMIN_GROUP_UNIX }
}

pub fn ensure_admin_group(os: Os) -> Result<()> {
    match os {
        Os::Linux => {
            if !cmd::succeeds("getent", &["group", ADMIN_GROUP_UNIX]) {
                cmd::run_quiet("groupadd", &["--system", ADMIN_GROUP_UNIX]).context("无法创建 msst-net 用户组")?;
            }
        }
        Os::Macos => {
            if !cmd::succeeds("dscl", &[".", "-read", &format!("/Groups/{ADMIN_GROUP_UNIX}")]) {
                cmd::run_quiet("dseditgroup", &["-o", "create", "-r", "MSST-Net administrators", ADMIN_GROUP_UNIX])
                    .context("无法创建 msst-net 用户组")?;
            }
        }
        Os::Windows => {
            if !cmd::succeeds("net", &["localgroup", ADMIN_GROUP_WINDOWS]) {
                cmd::run_quiet("net", &["localgroup", ADMIN_GROUP_WINDOWS, "/add", "/comment:MSST-Net 管理员（可通过控制器管理 Core）"])
                    .context("无法创建 MSST-Net Administrators 本地组")?;
            }
        }
    }
    Ok(())
}

pub fn user_exists(os: Os, user: &str) -> bool {
    match os {
        Os::Windows => cmd::succeeds("net", &["user", user]),
        _ => cmd::succeeds("id", &["-u", user]),
    }
}

pub fn is_admin_member(os: Os, user: &str) -> bool {
    match os {
        Os::Linux => cmd::output("id", &["-nG", user]).is_ok_and(|o| o.split_whitespace().any(|g| g == ADMIN_GROUP_UNIX)),
        Os::Macos => cmd::succeeds("dseditgroup", &["-o", "checkmember", "-m", user, ADMIN_GROUP_UNIX]),
        Os::Windows => cmd::output("net", &["localgroup", ADMIN_GROUP_WINDOWS])
            .is_ok_and(|o| o.lines().any(|l| l.trim().eq_ignore_ascii_case(user) || l.trim().ends_with(&format!("\\{user}")))),
    }
}

pub fn add_admin(os: Os, user: &str) -> Result<()> {
    if !user_exists(os, user) {
        bail!("用户 {user} 不存在");
    }
    if is_admin_member(os, user) {
        return Ok(());
    }
    match os {
        Os::Linux => cmd::run_quiet("usermod", &["-aG", ADMIN_GROUP_UNIX, user]),
        Os::Macos => cmd::run_quiet("dseditgroup", &["-o", "edit", "-a", user, "-t", "user", ADMIN_GROUP_UNIX]),
        Os::Windows => cmd::run_quiet("net", &["localgroup", ADMIN_GROUP_WINDOWS, user, "/add"]),
    }
    .with_context(|| format!("无法将 {user} 加入 {}", admin_group(os)))
}

/// The WebUI service account (Linux): system user, primary group
/// `msst-net`, no login shell, home = its config directory.
pub fn ensure_webui_user(home: &std::path::Path) -> Result<()> {
    if cmd::succeeds("id", &["-u", WEBUI_USER]) {
        // An existing account must still be in the admin group.
        if !is_admin_member(Os::Linux, WEBUI_USER) {
            cmd::run_quiet("usermod", &["-g", ADMIN_GROUP_UNIX, WEBUI_USER])?;
        }
        return Ok(());
    }
    let nologin = ["/usr/sbin/nologin", "/sbin/nologin", "/usr/bin/nologin"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("/bin/false");
    cmd::run_quiet(
        "useradd",
        &[
            "--system",
            "--gid",
            ADMIN_GROUP_UNIX,
            "--home-dir",
            &home.to_string_lossy(),
            "--no-create-home",
            "--shell",
            nologin,
            "--comment",
            "MSST-Net WebUI",
            WEBUI_USER,
        ],
    )
    .context("无法创建 msst-webui 系统用户")
}

/// Give a path to the WebUI user (Linux).
#[cfg(unix)]
pub fn chown_webui(path: &std::path::Path) -> Result<()> {
    cmd::run_quiet("chown", &["-R", &format!("{WEBUI_USER}:{ADMIN_GROUP_UNIX}"), &path.to_string_lossy()])
}

#[cfg(not(unix))]
pub fn chown_webui(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

pub fn remove_webui_user() {
    let _ = cmd::run_quiet("userdel", &[WEBUI_USER]);
}

pub fn remove_admin_group(os: Os) {
    let _ = match os {
        Os::Linux => cmd::run_quiet("groupdel", &[ADMIN_GROUP_UNIX]),
        Os::Macos => cmd::run_quiet("dseditgroup", &["-o", "delete", ADMIN_GROUP_UNIX]),
        Os::Windows => cmd::run_quiet("net", &["localgroup", ADMIN_GROUP_WINDOWS, "/delete"]),
    };
}

//! Post-start health checks.  An upgrade only counts as done when the new
//! Core is running *and* answering on its IPC endpoint; otherwise it is
//! rolled back.

use crate::layout;
use crate::platform::Os;
use crate::service::{self, Service};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(45);

/// Core's IPC endpoint when it runs as root / LocalSystem (Core
/// `config.rs::resolve_ipc_socket`).
pub fn core_ipc_path(os: Os) -> PathBuf {
    match os {
        Os::Linux => PathBuf::from("/run/msst-net/ipc.sock"),
        Os::Macos => PathBuf::from("/var/run/msst-net/ipc.sock"),
        Os::Windows => PathBuf::from(r"\\.\pipe\msst-net-ipc"),
    }
}

/// Wait until the Core service runs and its IPC endpoint accepts a
/// connection.  The service must stay up for the whole check: a Core that
/// crash-loops is caught even if it is briefly "active".
pub fn wait_core(os: Os) -> Result<()> {
    let ipc = core_ipc_path(os);
    let start = Instant::now();
    let mut ok_since: Option<Instant> = None;
    while start.elapsed() < TIMEOUT {
        if service::is_running(os, Service::Core) && ipc_accepts(&ipc) {
            // Healthy for 3 s in a row.
            let since = *ok_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(3) {
                return Ok(());
            }
        } else {
            ok_since = None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let logs = service::recent_logs(os, Service::Core);
    bail!(
        "Core 在 {} 秒内未就绪（服务{}，IPC {} {}）{}",
        TIMEOUT.as_secs(),
        if service::is_running(os, Service::Core) { "运行中" } else { "未运行" },
        ipc.display(),
        if ipc_accepts(&ipc) { "可连接" } else { "不可连接" },
        if logs.is_empty() { String::new() } else { format!("\n最近日志：\n{logs}") }
    )
}

#[cfg(unix)]
fn ipc_accepts(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(windows)]
fn ipc_accepts(path: &Path) -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open(path).is_ok()
}

/// Wait until the native WebUI answers its own health check
/// (`msst-net-webui healthcheck` → `GET /healthz`).
pub fn wait_webui(os: Os, exe: &Path) -> Result<()> {
    let start = Instant::now();
    let config = layout::webui_config_dir(os);
    while start.elapsed() < TIMEOUT {
        let ok = std::process::Command::new(exe)
            .arg("healthcheck")
            .env("MSST_WEBUI_CONFIG_DIR", &config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let logs = service::recent_logs(os, Service::Webui);
    bail!(
        "WebUI 在 {} 秒内未通过健康检查{}",
        TIMEOUT.as_secs(),
        if logs.is_empty() { String::new() } else { format!("\n最近日志：\n{logs}") }
    )
}

/// Run `exe --version` and check it reports `version` — catches a wrong
/// architecture or a broken binary before any service is touched.  A
/// program that doesn't exit within 10 s (one that ignores `--version` and
/// starts serving) is killed and fails the check.
pub fn preflight(exe: &Path, version: &str) -> Result<()> {
    use std::io::Read;
    let mut child = std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("无法运行 {}：{e}（架构不符或文件损坏？）", exe.display()))?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} 自检超时（不支持 --version？）", exe.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut text = String::new();
    child.stdout.take().unwrap().read_to_string(&mut text)?;
    if !status.success() || !text.contains(version) {
        bail!("{} 自检失败：期望版本 {version}，输出：{}", exe.display(), text.trim());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn preflight_checks_version_output() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("prog");
        std::fs::write(&exe, "#!/bin/sh\necho \"msst-net-webui 3.1.0-alpha.2\"\n").unwrap();
        crate::fsutil::set_mode(&exe, 0o755).unwrap();
        preflight(&exe, "3.1.0-alpha.2").unwrap();
        assert!(preflight(&exe, "3.1.0-alpha.3").is_err(), "wrong version");
        assert!(preflight(&d.path().join("missing"), "3.1.0").is_err());
    }
}

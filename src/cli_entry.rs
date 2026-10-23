//! The `msst-net-cli` command entry (command-line Controller).
//!
//! The CLI is not a service: users run it in a terminal.  The installer
//! keeps one entry pointing at the active version and switches it together
//! with the services (after Core passed its health check):
//!
//! - Linux / macOS: symlink `/usr/local/bin/msst-net-cli` →
//!   `<root>/versions/<version>/msst-net-cli`;
//! - Windows: `<root>\bin\msst-net-cli.exe`, a copy of the version's
//!   executable, with `<root>\bin` on the system `PATH`;
//! - Docker (Linux): Core runs in a container and the CLI on the host; the
//!   entry is a wrapper script that points the CLI at the container's socket
//!   (`MSST_IPC_PATH`).  Core in the container doesn't know the host's users
//!   and groups, so only root may connect: the script says so when a
//!   non-root user can't reach Core.

use crate::fsutil;
use crate::layout::Layout;
use crate::platform::Os;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// First lines of the Docker wrapper script (identifies it as ours).
const MARKER: &str = "# Managed by msst-net-installer";

/// What the entry should run.
pub enum Target<'a> {
    /// A native installation's executable.
    Native(&'a Path),
    /// Docker: the host executable and the container's IPC socket.
    Docker { exe: &'a Path, socket: &'a Path },
}

/// Whether the entry was created by this installer (or doesn't exist).
pub fn is_free_or_ours(layout: &Layout) -> bool {
    let entry = layout.cli_entry();
    let Ok(meta) = std::fs::symlink_metadata(&entry) else { return true };
    if layout.os == Os::Windows {
        // Inside our own program directory.
        return true;
    }
    if meta.file_type().is_symlink() {
        return std::fs::read_link(&entry).is_ok_and(|t| t.starts_with(&layout.root) || t.starts_with(layout.docker_dir()));
    }
    std::fs::read_to_string(&entry).is_ok_and(|t| t.lines().nth(1).is_some_and(|l| l.starts_with(MARKER)))
}

/// Refuse (before anything is changed) to replace a `msst-net-cli` this
/// installer didn't create.
pub fn check_free(layout: &Layout) -> Result<()> {
    if !is_free_or_ours(layout) {
        bail!("{} 已存在且不是本安装器创建的；请先移走它，再选择命令行控制器。", layout.cli_entry().display());
    }
    Ok(())
}

/// Point the entry at `target` (replacing the previous one atomically).
pub fn point_to(layout: &Layout, target: Target) -> Result<()> {
    check_free(layout)?;
    let entry = layout.cli_entry();
    let dir = entry.parent().context("路径没有父目录")?;
    std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    match target {
        Target::Native(exe) if layout.os == Os::Windows => {
            let tmp = dir.join(format!(".msst-net-cli.tmp-{}.exe", std::process::id()));
            std::fs::copy(exe, &tmp).with_context(|| format!("无法复制 {}", exe.display()))?;
            if let Err(e) = std::fs::rename(&tmp, &entry) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("无法替换 {}（msst-net-cli 正在运行？）", entry.display()));
            }
            add_to_path(&layout.bin_dir())?;
        }
        Target::Native(exe) => symlink(exe, &entry)?,
        Target::Docker { exe, socket } => fsutil::atomic_write(&entry, wrapper_script(exe, socket).as_bytes(), 0o755)?,
    }
    Ok(())
}

/// Remove the entry (and, on Windows, `<root>\bin` from `PATH`).  An entry
/// this installer didn't create is left alone.
pub fn remove(layout: &Layout) -> Result<()> {
    if layout.os == Os::Windows {
        fsutil::remove_any(&layout.bin_dir())?;
        return remove_from_path(&layout.bin_dir());
    }
    if is_free_or_ours(layout) {
        fsutil::remove_any(&layout.cli_entry())?;
    }
    Ok(())
}

/// Whether the entry exists and is ours.
pub fn exists(layout: &Layout) -> bool {
    std::fs::symlink_metadata(layout.cli_entry()).is_ok() && is_free_or_ours(layout)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    let tmp = link.with_file_name(format!(".msst-net-cli.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).with_context(|| format!("无法创建符号链接 {}", link.display()))?;
    std::fs::rename(&tmp, link).with_context(|| format!("无法替换 {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    bail!("此平台不支持符号链接：{}", link.display())
}

/// Shell single-quoting.
fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

fn wrapper_script(exe: &Path, socket: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         {MARKER}: MSST-Net Core runs in Docker, msst-net-cli on the host.\n\
         # Core in the container admits root only (it doesn't know the host's users).\n\
         export MSST_IPC_PATH=\"${{MSST_IPC_PATH:-{socket}}}\"\n\
         {exe} \"$@\"\n\
         rc=$?\n\
         if [ \"$rc\" -eq 3 ] && [ \"$(id -u)\" -ne 0 ]; then\n\
         \x20   echo '提示：Core 运行在 Docker 容器中，只接受 root 的连接，请使用 sudo msst-net-cli …' >&2\n\
         fi\n\
         exit \"$rc\"\n",
        socket = socket.display(),
        exe = sh_quote(exe),
    )
}

/// PowerShell that edits the machine `Path` (kept as REG_EXPAND_SZ, so
/// entries like `%SystemRoot%` stay unexpanded) and broadcasts the change
/// to new processes (`SetEnvironmentVariable` sends WM_SETTINGCHANGE).
fn path_script(dir: &Path, add: bool) -> String {
    let d = dir.to_string_lossy().trim_end_matches('\\').replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $k = 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment'\n\
         $d = '{d}'\n\
         $old = (Get-Item $k).GetValue('Path', '', 'DoNotExpandEnvironmentNames')\n\
         $parts = @($old -split ';' | Where-Object {{ $_ -ne '' -and $_.TrimEnd('\\') -ne $d }})\n\
         if ({add}) {{ $parts += $d }}\n\
         $new = $parts -join ';'\n\
         if ($new -ne $old) {{\n\
         \x20   Set-ItemProperty -Path $k -Name Path -Value $new -Type ExpandString\n\
         \x20   [Environment]::SetEnvironmentVariable('MSST_NET_INSTALLER_NOTIFY', $null, 'Machine')\n\
         }}\n",
        add = if add { "$true" } else { "$false" },
    )
}

fn add_to_path(dir: &Path) -> Result<()> {
    run_powershell(&path_script(dir, true)).with_context(|| format!("无法将 {} 加入系统 PATH", dir.display()))
}

fn remove_from_path(dir: &Path) -> Result<()> {
    run_powershell(&path_script(dir, false)).with_context(|| format!("无法从系统 PATH 中移除 {}", dir.display()))
}

fn run_powershell(script: &str) -> Result<()> {
    crate::cmd::run_quiet("powershell", &["-NoProfile", "-NonInteractive", "-Command", script])
}

/// Where the entry points, for `status`.
pub fn describe(layout: &Layout) -> Option<String> {
    let entry = layout.cli_entry();
    std::fs::symlink_metadata(&entry).ok()?;
    let target: Option<PathBuf> = std::fs::read_link(&entry).ok();
    Some(match target {
        Some(t) => format!("{} → {}", entry.display(), t.display()),
        None => entry.display().to_string(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn layout(d: &Path) -> Layout {
        // SAFETY: tests in this module are the only users of MSST_CLI_ENTRY.
        unsafe { std::env::set_var("MSST_CLI_ENTRY", d.join("bin/msst-net-cli")) };
        Layout { os: Os::Linux, root: d.join("root") }
    }

    #[test]
    fn entry_switches_and_refuses_foreign_files() {
        let d = tempfile::tempdir().unwrap();
        let l = layout(d.path());
        let entry = l.cli_entry();
        assert!(is_free_or_ours(&l));
        point_to(&l, Target::Native(&l.cli_exe("3.1.0+3.1.8"))).unwrap();
        assert_eq!(std::fs::read_link(&entry).unwrap(), l.cli_exe("3.1.0+3.1.8"));
        point_to(&l, Target::Native(&l.cli_exe("3.1.0+3.1.9"))).unwrap();
        assert_eq!(std::fs::read_link(&entry).unwrap(), l.cli_exe("3.1.0+3.1.9"));
        assert!(exists(&l));

        let socket = d.path().join("docker/run/ipc.sock");
        point_to(&l, Target::Docker { exe: &d.path().join("docker/cli/3.1.9/msst-net-cli"), socket: &socket }).unwrap();
        let script = std::fs::read_to_string(&entry).unwrap();
        assert!(script.contains(&format!("MSST_IPC_PATH:-{}", socket.display())));
        assert!(is_free_or_ours(&l), "the wrapper is recognised as ours");
        remove(&l).unwrap();
        assert!(!entry.exists());

        std::fs::write(&entry, "#!/bin/sh\necho someone else's\n").unwrap();
        assert!(check_free(&l).is_err());
        assert!(point_to(&l, Target::Native(&l.cli_exe("3.1.0"))).is_err());
        remove(&l).unwrap();
        assert!(entry.exists(), "a foreign file is left alone");
    }

    #[test]
    fn wrapper_quotes_paths() {
        let s = wrapper_script(Path::new("/opt/it's/msst-net-cli"), Path::new("/opt/msst-net-docker/run/ipc.sock"));
        assert!(s.contains(r"'/opt/it'\''s/msst-net-cli' "));
        assert!(s.lines().nth(1).unwrap().starts_with(MARKER));
    }

    #[test]
    fn path_script_keeps_expand_sz() {
        let s = path_script(Path::new(r"C:\Program Files\MSST-Net\bin"), true);
        assert!(s.contains(r"$d = 'C:\Program Files\MSST-Net\bin'"));
        assert!(s.contains("DoNotExpandEnvironmentNames") && s.contains("-Type ExpandString"));
    }
}

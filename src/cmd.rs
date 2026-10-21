//! Running external programs (systemctl, sc, docker, package managers).

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::process::{Command, Stdio};

/// Run a program, inheriting stdout/stderr; fail on a non-zero exit.
pub fn run<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<()> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("无法执行 '{prog}'"))?;
    if !status.success() {
        bail!("命令 '{} {}' 失败（退出码 {:?}）", prog, join(args), status.code());
    }
    Ok(())
}

/// Run a program quietly; fail on a non-zero exit with its stderr.
pub fn run_quiet<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<()> {
    output(prog, args).map(|_| ())
}

/// Run a program and capture stdout; fail on a non-zero exit with its stderr.
pub fn output<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("无法执行 '{prog}'"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
        bail!("命令 '{} {}' 失败（退出码 {:?}）：{}", prog, join(args), out.status.code(), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a program runs and exits successfully (output discarded).
pub fn succeeds<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> bool {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether `prog` is on PATH.
pub fn exists(prog: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(&path).any(|dir| exts.iter().any(|ext| dir.join(format!("{prog}{ext}")).is_file()))
}

fn join<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter().map(|a| a.as_ref().to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

//! Native deployment: Core (and optionally the WebUI) as system services.
//!
//! Every change is a transaction:
//!
//! 1. resolve the Core release (and the Controller release) and verify
//!    their manifests;
//! 2. download every artifact into a fresh staging directory and verify it
//!    (size + SHA-256), then self-test Core (`--version`) — the running
//!    installation is untouched until here;
//! 3. (3.0.x) back up data and move 3.0 program files aside;
//! 4. move the staged files to `versions/<core>+<controller>/`, mark the
//!    state pending;
//! 5. point the services at the new version, start them and wait for health;
//! 6. on failure, point them back at the previous version (or restore
//!    3.0.x) and report; on success, commit the state and prune old
//!    versions (current + previous are kept for rollback).

use crate::accounts;
use crate::bundle::{Bundle, Versions};
use crate::context::Ctx;
use crate::desktop;
use crate::fsutil;
use crate::health;
use crate::layout::{self, Layout};
use crate::legacy::{self, Backup, LegacyNative};
use crate::manifest::{self, Artifact, Component, Format, Manifest, Product};
use crate::platform::Os;
use crate::release::{self, Release};
use crate::service::{self, Definition, Service};
use crate::state::{ControllerKind, Deployment, InstallState, Lock};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub struct InstallOptions {
    /// None: keep the installed choice (upgrade) — required for a new install.
    pub controller: Option<ControllerKind>,
    pub core_version: Option<String>,
    pub controller_version: Option<String>,
    pub admin_users: Vec<String>,
    pub webui_password: Option<String>,
    /// Remove 3.0.x Tauri packages after a successful migration.
    pub remove_legacy_packages: Option<bool>,
}

/// First-start password file of the native WebUI (deleted once it ran).
fn initial_password_file(os: Os) -> PathBuf {
    layout::webui_config_dir(os).join("initial-password")
}

/// Install, upgrade, or migrate from 3.0.x.
pub fn install(ctx: &Ctx, opts: InstallOptions) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let existing = InstallState::load(&ctx.layout)?;
    if existing.as_ref().is_some_and(|s| s.deployment == Deployment::Docker) {
        bail!("当前为 Docker 部署，请使用 Docker 模式升级（或先卸载再以原生方式安装）。");
    }
    if let Some(p) = existing.as_ref().and_then(|s| s.pending.as_ref()) {
        ctx.ui.warn(&format!("上次的操作（{} → {}）未完成；本次操作会重新写入服务配置。", p.action, p.target));
    }
    let legacy = if existing.is_none() { legacy::detect_native(&ctx.layout) } else { None };
    if let Some(l) = &legacy {
        ctx.ui.section("检测到 MSST-Net 3.0.x");
        for d in l.describe() {
            ctx.ui.info(&d);
        }
    }

    let controller = match (opts.controller, &existing) {
        (Some(c), _) => c,
        (None, Some(s)) => s.controller,
        (None, None) => bail!("请选择控制器（--controller desktop|webui|none）"),
    };
    let webui = controller == ControllerKind::Webui;

    let bundle = Bundle::resolve(ctx, opts.core_version.as_deref(), opts.controller_version.as_deref(), controller != ControllerKind::None)?;
    let versions = bundle.versions();
    let target = versions.id();
    let action = match &existing {
        Some(s) => {
            let old = Versions::parse_id(&s.version);
            if s.version == target && opts.controller.is_none_or(|c| c == s.controller) {
                ctx.ui.info(&format!("已安装 {}，无需升级。文件损坏或服务异常请使用「修复」。", versions.label()));
                return Ok(());
            }
            if versions.is_downgrade_from(&old) {
                ctx.ui.warn(&format!("目标 {} 低于已安装的 {}（降级）。", versions.label(), old.label()));
                if !ctx.ui.confirm("确定降级？", false)? {
                    return Ok(());
                }
                "downgrade"
            } else {
                "upgrade"
            }
        }
        None if legacy.is_some() => "migrate",
        None => "install",
    };

    let core_m = &bundle.core.manifest;
    let mut core_arts = vec![core_m.require(Component::Core, ctx.os, ctx.arch, Format::Binary)?];
    let wintun = (ctx.os == Os::Windows).then(|| core_m.find(Component::Wintun, ctx.os, ctx.arch, Format::Dll)).flatten();
    core_arts.extend(wintun);
    let ctl_m = bundle.controller.as_ref().map(|r| &r.manifest);
    let webui_art = match ctl_m {
        Some(m) if webui => Some(m.require(Component::Webui, ctx.os, ctx.arch, Format::Binary)?),
        _ => None,
    };
    let desktop_art = match ctl_m {
        Some(m) if controller == ControllerKind::Desktop => Some(desktop::select(m, ctx.os, ctx.arch)?),
        _ => None,
    };
    let artifacts: Vec<&Artifact> = core_arts.iter().copied().chain(webui_art).collect();

    let admin_users = choose_admin_users(ctx, &opts.admin_users, existing.as_ref())?;
    let needs_password = webui && !layout::webui_config_dir(ctx.os).join("passwd").exists();
    let import_from = if needs_password { find_webui_config_to_import() } else { None };
    let password = match (&opts.webui_password, &import_from) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(_)) => None,
        (None, None) if needs_password => Some(ctx.ui.new_password("设置 WebUI 访问密码", "--admin-password-file")?),
        _ => None,
    };

    // ---- plan ----
    let total: u64 = artifacts.iter().chain(desktop_art.iter()).map(|a| a.size).sum();
    let mut steps = vec![format!(
        "下载 {} 个文件（{}）到暂存目录并逐一校验 SHA-256；失败则中止，现有安装不受影响",
        artifacts.len() + usize::from(desktop_art.is_some()),
        fsutil::human_size(total)
    )];
    if ctx.os == Os::Windows && wintun.is_none() {
        steps.push("从 wintun.net 下载 Wintun（固定哈希校验）".into());
    }
    if legacy.is_some() {
        steps.push(format!("备份 3.0.x 身份与配置数据到 {}", ctx.layout.backups_dir().display()));
        steps.push("停止 3.0.x 服务，将 3.0.x 程序文件移入备份（不删除，可还原）".into());
    }
    steps.push(format!("安装到 {}", ctx.layout.version_dir(&target).display()));
    steps.push(format!(
        "确保管理员组 {}{}",
        accounts::admin_group(ctx.os),
        if admin_users.is_empty() { String::new() } else { format!("，加入用户：{}", admin_users.join("、")) }
    ));
    if webui {
        if ctx.os == Os::Linux {
            steps.push(format!("创建 WebUI 系统用户 {}（非特权，属于 msst-net 组）", accounts::WEBUI_USER));
        }
        if let Some(src) = &import_from {
            steps.push(format!("沿用 3.0.x WebUI 的密码与配置（{}）", src.display()));
        }
    }
    let rollback_to = existing.as_ref().map(|s| s.version.clone());
    let to = versions.label();
    steps.push(match (rollback_to.as_deref().map(|p| Versions::parse_id(p).label()), legacy.is_some()) {
        (Some(prev), _) => format!("切换服务 {prev} → {to}，启动并做健康检查；失败自动回滚到 {prev}"),
        (None, true) => format!("以 3.1 配置服务 msst-net（{to}），启动并做健康检查；失败自动还原 3.0.x"),
        (None, false) => format!("配置并启动 Core 服务（{to}），做健康检查"),
    });
    if webui {
        steps.push("配置并启动 WebUI 服务 msst-net-webui，做健康检查".into());
    } else if existing.as_ref().is_some_and(|s| s.webui_service) {
        steps.push("移除 WebUI 服务（不再选择 WebUI 控制器）".into());
    }
    if let Some(a) = desktop_art {
        steps.push(format!("安装桌面控制器（{}）", a.name));
    }
    let keep: Vec<String> = layout::data_locations(ctx.os, &ctx.home)
        .into_iter()
        .filter(|l| l.path.exists())
        .map(|l| format!("{}", l.path.display()))
        .collect();
    if !keep.is_empty() {
        steps.push(format!("保留现有数据：{}", keep.join("；")));
    }
    let title = match action {
        "migrate" => format!("迁移 3.0.x → {}", versions.label()),
        "install" => format!("安装 MSST-Net（{}）", versions.label()),
        _ => format!("升级到 {}", versions.label()),
    };
    if !ctx.ui.plan(&title, &steps, ctx.dry_run)? {
        return Ok(());
    }

    // ---- stage: nothing installed is touched until this succeeds ----
    ctx.ui.section("下载与校验");
    let staging = ctx.staging()?;
    let staged = stage(ctx, &bundle, &core_arts, webui_art, wintun.is_none(), &staging);
    if let Err(e) = staged {
        let _ = fsutil::remove_any(&staging);
        return Err(e.context("下载或校验失败，现有安装未做任何更改"));
    }

    // ---- 3.0.x: back up and move aside ----
    let mut backup = None;
    if let Some(l) = &legacy {
        ctx.ui.section("备份 3.0.x");
        match retire_legacy(ctx, l) {
            Ok(b) => backup = Some(b),
            Err(e) => {
                let _ = fsutil::remove_any(&staging);
                return Err(e.context("备份 3.0.x 失败"));
            }
        }
    }

    // ---- commit files ----
    let result = (|| -> Result<InstallState> {
        if existing.as_ref().is_some_and(|s| s.version == target) {
            // Replacing the running version's directory (controller change):
            // Windows can't move a directory whose executable is running.
            service::stop(ctx.os, Service::Webui);
            service::stop(ctx.os, Service::Core);
        }
        commit_version(&ctx.layout, &staging, &target)?;
        let mut state = existing.clone().unwrap_or_else(|| InstallState::new(Deployment::Native, controller, &target, &ctx.source.key()));
        state.begin(&ctx.layout, action, &target)?;

        ctx.ui.section("账户与权限");
        prepare_accounts(ctx, &admin_users, &mut state)?;
        if webui {
            prepare_webui(ctx, import_from.as_deref(), password.as_deref())?;
        }

        ctx.ui.section("启动服务");
        let password_file = Some(initial_password_file(ctx.os)).filter(|p| p.exists());
        activate(ctx, &target, webui, password_file.as_deref())?;
        Ok(state)
    })();

    let mut state = match result {
        Ok(s) => s,
        Err(e) => {
            ctx.ui.warn(&format!("{e:#}"));
            recover(ctx, existing.as_ref(), backup.as_ref(), &target);
            return Err(e.context(format!("{} 未能启动，已撤销本次更改", versions.label())));
        }
    };

    // ---- commit state ----
    if let Some(prev) = &rollback_to
        && prev != &target
    {
        state.previous = Some(prev.clone());
    }
    state.version = target.clone();
    state.controller = controller;
    state.source = ctx.source.key();
    state.webui_service = webui;
    if let Some(b) = &backup {
        state.legacy_backup = Some(b.dir.clone());
    }
    state.pending = None;
    state.record(action, &target, true, None);
    state.save(&ctx.layout)?;

    // ---- finish (failures here don't undo a healthy Core) ----
    if webui {
        finish_webui_password(ctx, &target)?;
    }
    if let Some(art) = desktop_art {
        ctx.ui.section("桌面控制器");
        match install_desktop(ctx, bundle.controller.as_ref().expect("controller release"), art, &state) {
            Ok(d) => {
                ctx.ui.success(&format!("桌面控制器已安装：{}", d.location));
                state.desktop = Some(d);
            }
            Err(e) => ctx.ui.warn(&format!("桌面控制器安装失败：{e:#}\n    Core 已正常运行；稍后可执行「修复」重试。")),
        }
    } else if let Some(d) = state.desktop.take() {
        ctx.ui.info("不再选择桌面控制器，正在移除…");
        if let Err(e) = desktop::uninstall(&d) {
            ctx.ui.warn(&format!("移除桌面控制器失败：{e:#}"));
            state.desktop = Some(d);
        }
    }
    state.save(&ctx.layout)?;
    if let Some(l) = &legacy {
        offer_remove_legacy_packages(ctx, l, opts.remove_legacy_packages)?;
    }
    prune(ctx, &state);
    let _ = fsutil::remove_any(&ctx.layout.staging_dir());

    summary(ctx, &state, backup.as_ref());
    Ok(())
}

fn stage(ctx: &Ctx, bundle: &Bundle, core_arts: &[&Artifact], webui: Option<&Artifact>, fetch_wintun: bool, dir: &Path) -> Result<()> {
    ctx.download(&bundle.core, core_arts, dir)?;
    if let (Some(a), Some(rel)) = (webui, &bundle.controller) {
        ctx.download(rel, &[a], dir)?;
    }
    for a in core_arts.iter().copied().chain(webui) {
        let renamed = dir.join(installed_name(a, ctx.os));
        std::fs::rename(dir.join(&a.name), &renamed)?;
        if a.format == Format::Binary {
            fsutil::set_mode(&renamed, 0o755)?;
        }
    }
    if ctx.os == Os::Windows && fetch_wintun {
        ctx.ui.step("下载 Wintun（wintun.net）");
        release::wintun::fetch(&ctx.http, ctx.arch, &dir.join("wintun.dll"))?;
    }
    bundle.core.save_manifest(dir)?;
    if let Some(c) = &bundle.controller {
        c.save_manifest(dir)?;
    }
    health::preflight(&dir.join(format!("msst-net-core{}", ctx.os.exe_suffix())), &bundle.core.manifest.version)?;
    ctx.ui.success("Core 自检通过");
    if let (Some(_), Some(ctl)) = (webui, &bundle.controller) {
        health::preflight(&dir.join(format!("msst-net-webui{}", ctx.os.exe_suffix())), &ctl.manifest.version)?;
        ctx.ui.success("WebUI 自检通过");
    }
    Ok(())
}

/// The file name an artifact has inside `versions/<v>/`.
fn installed_name(a: &Artifact, os: Os) -> String {
    match a.component {
        Component::Core => format!("msst-net-core{}", os.exe_suffix()),
        Component::Webui => format!("msst-net-webui{}", os.exe_suffix()),
        Component::Wintun => "wintun.dll".into(),
        Component::Desktop => a.name.clone(),
    }
}

/// Move a verified staging directory to `versions/<version>`.  An existing
/// directory of that version (reinstall) is set aside first and removed
/// only after the move succeeded.
fn commit_version(layout: &Layout, staging: &Path, version: &str) -> Result<()> {
    let dest = layout.version_dir(version);
    std::fs::create_dir_all(layout.versions_dir())?;
    let aside = dest.with_file_name(format!("{version}.replaced-{}", fsutil::stamp()));
    let had = dest.exists();
    if had {
        std::fs::rename(&dest, &aside)?;
    }
    if let Err(e) = std::fs::rename(staging, &dest) {
        if had {
            let _ = std::fs::rename(&aside, &dest);
        }
        return Err(e).context("无法移动到版本目录");
    }
    // The staging directory is private while downloads are unverified; the
    // version directory must be traversable by the unprivileged WebUI user.
    fsutil::set_mode(&dest, 0o755)?;
    if had {
        let _ = fsutil::remove_any(&aside);
    }
    Ok(())
}

fn retire_legacy(ctx: &Ctx, l: &LegacyNative) -> Result<Backup> {
    let mut b = Backup::create(&ctx.layout, "3.0.x")?;
    for p in legacy::snapshot_data(ctx.os, &ctx.home, &mut b)? {
        ctx.ui.step(&format!("已备份 {}", p.display()));
    }
    legacy::retire_native(ctx.os, l, &mut b)?;
    ctx.ui.success(&format!("3.0.x 已停止，程序文件已移入 {}", b.dir.display()));
    Ok(b)
}

/// Point the services at `version`, start them, and wait until healthy.
fn activate(ctx: &Ctx, version: &str, webui: bool, password_file: Option<&Path>) -> Result<()> {
    let os = ctx.os;
    service::stop(os, Service::Webui);
    service::stop(os, Service::Core);
    service::install(os, Service::Core, &service::definition_for_core(&ctx.layout.core_exe(version), &ctx.home))?;
    let core = Versions::parse_id(version).core;
    ctx.ui.step(&format!("启动 Core {core}"));
    service::start(os, Service::Core)?;
    health::wait_core(os)?;
    ctx.ui.success(&format!("Core {core} 运行正常（IPC {} 可连接）", health::core_ipc_path(os).display()));
    if webui {
        service::install(os, Service::Webui, &webui_definition(ctx, version, password_file))?;
        ctx.ui.step("启动 WebUI");
        service::start(os, Service::Webui)?;
        health::wait_webui(os, &ctx.layout.webui_exe(version))?;
        ctx.ui.success("WebUI 运行正常");
    } else if service::exists(os, Service::Webui) {
        service::remove(os, Service::Webui)?;
    }
    Ok(())
}

fn webui_definition(ctx: &Ctx, version: &str, password_file: Option<&Path>) -> Definition {
    let config = layout::webui_config_dir(ctx.os);
    Definition {
        exe: ctx.layout.webui_exe(version),
        home: if ctx.os == Os::Linux { config.clone() } else { ctx.home.clone() },
        webui_config: Some(config),
        password_file: password_file.map(Path::to_path_buf),
    }
}

/// Undo a failed activation: back to the previous version, or to 3.0.x, or
/// (fresh install) leave nothing running.
fn recover(ctx: &Ctx, existing: Option<&InstallState>, backup: Option<&Backup>, target: &str) {
    ctx.ui.section("回滚");
    let os = ctx.os;
    if let Some(s) = existing {
        match activate(ctx, &s.version, s.webui_service, None) {
            Ok(()) => ctx.ui.success(&format!("已回滚到 {}", Versions::parse_id(&s.version).label())),
            Err(e) => ctx.ui.warn(&format!("回滚到 {} 也失败了：{e:#}\n    请执行「修复」。", Versions::parse_id(&s.version).label())),
        }
        let mut s = s.clone();
        s.pending = None;
        s.record("upgrade", target, false, None);
        let _ = s.save(&ctx.layout);
    } else {
        let _ = service::remove(os, Service::Webui);
        match backup {
            Some(b) => match legacy::restore_native(os, b) {
                Ok(()) => ctx.ui.success("已还原 3.0.x 并重新启动"),
                Err(e) => ctx.ui.warn(&format!("还原 3.0.x 失败：{e:#}\n    备份位于 {}", b.dir.display())),
            },
            None => {
                let _ = service::remove(os, Service::Core);
            }
        }
        let _ = fsutil::remove_any(&ctx.layout.state_file());
    }
    if existing.is_none_or(|s| s.version != target && s.previous.as_deref() != Some(target)) {
        let _ = fsutil::remove_any(&ctx.layout.version_dir(target));
    }
    let _ = fsutil::remove_any(&ctx.layout.staging_dir());
}

fn choose_admin_users(ctx: &Ctx, given: &[String], existing: Option<&InstallState>) -> Result<Vec<String>> {
    if !given.is_empty() || !ctx.ui.interactive || existing.is_some() {
        return Ok(given.to_vec());
    }
    let Some(me) = crate::platform::invoking_user() else { return Ok(Vec::new()) };
    let q = format!("将用户 {me} 加入 {} 组（可通过控制器管理网络；否则只能查看）？", accounts::admin_group(ctx.os));
    Ok(if ctx.ui.choose_yn(&q, true)? { vec![me] } else { Vec::new() })
}

fn prepare_accounts(ctx: &Ctx, users: &[String], state: &mut InstallState) -> Result<()> {
    accounts::ensure_admin_group(ctx.os)?;
    ctx.ui.success(&format!("管理员组 {} 已就绪", accounts::admin_group(ctx.os)));
    for u in users {
        match accounts::add_admin(ctx.os, u) {
            Ok(()) => {
                ctx.ui.success(&format!("{u} 已加入 {}", accounts::admin_group(ctx.os)));
                if !state.admin_users.contains(u) {
                    state.admin_users.push(u.clone());
                }
            }
            Err(e) => ctx.ui.warn(&format!("{e:#}")),
        }
    }
    Ok(())
}

/// Config directory, service user, and the first-start password.
fn prepare_webui(ctx: &Ctx, import_from: Option<&Path>, password: Option<&str>) -> Result<()> {
    let dir = layout::webui_config_dir(ctx.os);
    std::fs::create_dir_all(&dir)?;
    fsutil::set_mode(&dir, 0o700)?;
    if let Some(src) = import_from {
        for f in ["passwd", "config.toml", "tls-cert.pem", "tls-key.pem"] {
            if src.join(f).exists() && !dir.join(f).exists() {
                std::fs::copy(src.join(f), dir.join(f))?;
            }
        }
        ctx.ui.success(&format!("已沿用 {} 中的 WebUI 密码与配置", src.display()));
    }
    if let Some(pw) = password {
        fsutil::atomic_write(&initial_password_file(ctx.os), pw.as_bytes(), 0o600)?;
    }
    if ctx.os == Os::Linux {
        accounts::ensure_webui_user(&dir)?;
        accounts::chown_webui(&dir)?;
        ctx.ui.success(&format!("WebUI 将以 {} 身份运行", accounts::WEBUI_USER));
    }
    if ctx.os == Os::Windows {
        // ProgramData is readable by every user; the password hash and the
        // TLS key must not be.
        crate::cmd::run_quiet("icacls", &[
            dir.as_os_str(),
            "/inheritance:r".as_ref(),
            "/grant:r".as_ref(),
            "*S-1-5-18:(OI)(CI)F".as_ref(),
            "*S-1-5-32-544:(OI)(CI)F".as_ref(),
        ])
        .context("无法限制 WebUI 配置目录的访问权限")?;
        ctx.ui.success("WebUI 配置目录仅 SYSTEM 与管理员可访问");
    }
    Ok(())
}

/// After the WebUI stored its password hash, delete the plain-text file and
/// drop it from the service definition.
fn finish_webui_password(ctx: &Ctx, version: &str) -> Result<()> {
    let file = initial_password_file(ctx.os);
    if !file.exists() {
        return Ok(());
    }
    if layout::webui_config_dir(ctx.os).join("passwd").exists() {
        fsutil::remove_any(&file)?;
        // Not restarted: only the (now unneeded) PASSWORD_FILE setting goes
        // away.  (launchd would need a reload, i.e. a restart; the variable
        // pointing at a deleted file is harmless there.)
        if ctx.os != Os::Macos {
            service::install(ctx.os, Service::Webui, &webui_definition(ctx, version, None))?;
        }
    } else {
        ctx.ui.warn(&format!("WebUI 尚未保存密码，{} 暂时保留", file.display()));
    }
    Ok(())
}

/// A 3.0.x WebUI's config (password hash, port, allowlist, certificate) in
/// the invoking user's or root's home.
fn find_webui_config_to_import() -> Option<PathBuf> {
    let mut homes = Vec::new();
    #[cfg(unix)]
    if let Some(u) = crate::platform::invoking_user() {
        homes.extend(crate::platform::home_of(&u));
    }
    homes.extend(std::env::var_os("HOME").map(PathBuf::from));
    homes.into_iter().map(|h| h.join(".config/msst-net-webui")).find(|d| d.join("passwd").is_file())
}

fn install_desktop(ctx: &Ctx, release: &Release, art: &Artifact, state: &InstallState) -> Result<crate::state::DesktopInstall> {
    let dir = ctx.staging()?;
    let result = (|| {
        ctx.download(release, &[art], &dir)?;
        desktop::install(&ctx.layout, art, &dir.join(&art.name), &release.manifest.version, state.desktop.as_ref())
    })();
    let _ = fsutil::remove_any(&dir);
    result
}

fn offer_remove_legacy_packages(ctx: &Ctx, l: &LegacyNative, choice: Option<bool>) -> Result<()> {
    if l.packages.is_empty() {
        return Ok(());
    }
    let remove = match choice {
        Some(c) => c,
        None => ctx.ui.choose_yn(&format!("卸载 3.0.x 桌面控制器软件包（{}）？", l.packages.join("、")), true)?,
    };
    if remove {
        for p in &l.packages {
            let r = if crate::cmd::exists("dpkg") {
                crate::cmd::run("dpkg", &["-r", p])
            } else {
                crate::cmd::run("rpm", &["-e", p])
            };
            if let Err(e) = r {
                ctx.ui.warn(&format!("卸载 {p} 失败：{e:#}"));
            }
        }
    }
    Ok(())
}

/// Keep the current and previous versions; delete older ones.
fn prune(ctx: &Ctx, state: &InstallState) {
    let keep = [Some(state.version.as_str()), state.previous.as_deref()];
    for dir in fsutil::list_dir(&ctx.layout.versions_dir()) {
        let name = fsutil::file_name(&dir);
        if !keep.contains(&Some(name.as_str())) {
            let _ = fsutil::remove_any(&dir);
        }
    }
    let desktop_keep = state.desktop.as_ref().map(|d| d.version.clone());
    for dir in fsutil::list_dir(&ctx.layout.root.join("desktop")) {
        if Some(fsutil::file_name(&dir)) != desktop_keep {
            let _ = fsutil::remove_any(&dir);
        }
    }
}

fn summary(ctx: &Ctx, state: &InstallState, backup: Option<&Backup>) {
    ctx.ui.section("完成");
    ctx.ui.success(&format!("MSST-Net 已就绪（{}）", Versions::parse_id(&state.version).label()));
    ctx.ui.info(&format!("Core：{}（服务 {}）", ctx.layout.core_exe(&state.version).display(), Service::Core.name(ctx.os)));
    ctx.ui.info(&format!("数据目录：{}", layout::effective_core_data_dir(ctx.os, &ctx.home).display()));
    match state.controller {
        ControllerKind::Webui => {
            ctx.ui.info("WebUI：https://<本机地址>:18080（首次访问需信任自签名证书）");
        }
        ControllerKind::Desktop if state.desktop.is_some() => ctx.ui.info("桌面控制器：在应用菜单中打开「MSST-Net」"),
        _ => {}
    }
    if !state.admin_users.is_empty() {
        ctx.ui.info(&format!("管理员：{}（{} 组）", state.admin_users.join("、"), accounts::admin_group(ctx.os)));
        if ctx.os == Os::Windows {
            ctx.ui.info("Windows 上组成员身份需重新登录后生效");
        }
    }
    if let Some(prev) = &state.previous {
        ctx.ui.info(&format!("可回滚到：{}", Versions::parse_id(prev).label()));
    }
    if let Some(b) = backup {
        ctx.ui.info(&format!("3.0.x 备份：{}（可用 rollback --legacy 还原）", b.dir.display()));
    }
}

// ---------------------------------------------------------------------------
// repair / rollback / uninstall / status
// ---------------------------------------------------------------------------

/// The verified manifests saved in a version directory.
struct Saved {
    core: Manifest,
    controller: Option<Manifest>,
}

impl Saved {
    fn of(&self, p: Product) -> Option<&Manifest> {
        match p {
            Product::Core => Some(&self.core),
            Product::Controller => self.controller.as_ref(),
        }
    }
}

fn saved_manifests(ctx: &Ctx, id: &str) -> Result<Saved> {
    let v = Versions::parse_id(id);
    let dir = ctx.layout.version_dir(id);
    let read = |p: Product, want: &str| -> Result<Manifest> {
        let (mname, sname) = release::saved_names(p);
        let bytes = std::fs::read(dir.join(&mname)).with_context(|| format!("版本目录中缺少 {mname}"))?;
        let sig = std::fs::read(dir.join(sname)).ok();
        let (m, _) = manifest::verify(p, &bytes, sig.as_deref(), &ctx.trust)?;
        if m.version != want {
            bail!("{mname} 的版本为 {}，应为 {want}", m.version);
        }
        Ok(m)
    };
    Ok(Saved { core: read(Product::Core, &v.core)?, controller: v.controller.as_deref().map(|c| read(Product::Controller, c)).transpose()? })
}

/// What `versions/<id>/` should contain, from its saved manifests.
fn expected_files(ctx: &Ctx, id: &str, saved: &Saved, webui: bool) -> Vec<(Product, Artifact, PathBuf)> {
    let dir = ctx.layout.version_dir(id);
    let mut wanted = vec![(Product::Core, Component::Core, Format::Binary)];
    if ctx.os == Os::Windows {
        wanted.push((Product::Core, Component::Wintun, Format::Dll));
    }
    if webui {
        wanted.push((Product::Controller, Component::Webui, Format::Binary));
    }
    wanted
        .into_iter()
        .filter_map(|(p, c, f)| saved.of(p)?.find(c, ctx.os, ctx.arch, f).map(|a| (p, a.clone(), dir.join(installed_name(a, ctx.os)))))
        .collect()
}

/// A file of an installed version that is missing or damaged.
enum Broken {
    /// A manifest artifact of a product, installed at the path.
    Artifact(Product, Artifact, PathBuf),
    /// wintun.dll of a Core release that doesn't ship it (from wintun.net).
    Wintun(PathBuf),
}

impl Broken {
    fn path(&self) -> &Path {
        match self {
            Broken::Artifact(_, _, p) | Broken::Wintun(p) => p,
        }
    }
}

/// Files of `versions/<id>/` that are missing or don't match the manifests.
fn broken_files(ctx: &Ctx, id: &str, saved: &Saved, webui: bool) -> Vec<Broken> {
    let mut broken: Vec<_> = expected_files(ctx, id, saved, webui)
        .into_iter()
        .filter(|(_, a, p)| !release::matches(p, a))
        .map(|(prod, a, p)| Broken::Artifact(prod, a, p))
        .collect();
    let wintun = ctx.layout.version_dir(id).join("wintun.dll");
    if ctx.os == Os::Windows && saved.core.find(Component::Wintun, ctx.os, ctx.arch, Format::Dll).is_none() && !wintun.exists() {
        broken.push(Broken::Wintun(wintun));
    }
    broken
}

pub fn repair(ctx: &Ctx, state: InstallState) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let id = state.version.clone();
    let versions = Versions::parse_id(&id);
    let webui = state.controller == ControllerKind::Webui;

    // Releases are fetched only when needed: a missing/damaged saved
    // manifest, damaged files, or a missing desktop Controller.
    let mut releases: Vec<Release> = Vec::new();
    let saved = match saved_manifests(ctx, &id) {
        Ok(s) => s,
        Err(e) => {
            ctx.ui.warn(&format!("本地清单不可用（{e:#}），从发布源重新获取"));
            let b = Bundle::resolve(ctx, Some(&versions.core), versions.controller.as_deref(), versions.controller.is_some())?;
            let s = Saved { core: b.core.manifest.clone(), controller: b.controller.as_ref().map(|r| r.manifest.clone()) };
            releases.push(b.core);
            releases.extend(b.controller);
            s
        }
    };
    let broken = broken_files(ctx, &id, &saved, webui);
    let desktop_missing = state.controller == ControllerKind::Desktop && state.desktop.as_ref().is_none_or(|d| !desktop::is_installed(d));

    let mut steps = Vec::new();
    if broken.is_empty() {
        steps.push(format!("程序文件完整（{} 已通过 SHA-256 校验）", versions.label()));
    } else {
        for b in &broken {
            steps.push(format!("重新下载并替换 {}", b.path().display()));
        }
    }
    steps.push(format!("确保管理员组 {}{}", accounts::admin_group(ctx.os), if webui && ctx.os == Os::Linux { "与 WebUI 系统用户" } else { "" }));
    steps.push("重写服务配置，重启并做健康检查".into());
    if desktop_missing {
        steps.push("重新安装桌面控制器".into());
    }
    if !ctx.ui.plan(&format!("修复 MSST-Net（{}）", versions.label()), &steps, ctx.dry_run)? {
        return Ok(());
    }

    let mut needed: Vec<Product> = broken.iter().filter_map(|b| if let Broken::Artifact(p, ..) = b { Some(*p) } else { None }).collect();
    if desktop_missing {
        needed.push(Product::Controller);
    }
    for p in needed {
        if !releases.iter().any(|r| r.manifest.product == p) {
            let v = if p == Product::Core { Some(versions.core.as_str()) } else { versions.controller.as_deref() };
            releases.push(ctx.resolve(p, v)?);
        }
    }
    let release_of = |p: Product| releases.iter().find(|r| r.manifest.product == p);

    // Download everything first: a failed download changes nothing.
    let staging = ctx.staging()?;
    if !broken.is_empty() {
        ctx.ui.section("下载与校验");
        let result = (|| -> Result<()> {
            for b in &broken {
                match b {
                    Broken::Artifact(p, a, _) => ctx.download(release_of(*p).expect("resolved"), &[a], &staging)?,
                    Broken::Wintun(_) => {
                        ctx.ui.step("下载 Wintun（wintun.net）");
                        release::wintun::fetch(&ctx.http, ctx.arch, &staging.join("wintun.dll"))?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fsutil::remove_any(&staging);
            return Err(e.context("下载或校验失败，未做任何更改"));
        }
    }

    let mut state = state;
    state.begin(&ctx.layout, "repair", &id)?;
    ctx.ui.section("修复");
    service::stop(ctx.os, Service::Webui);
    service::stop(ctx.os, Service::Core);
    let vdir = ctx.layout.version_dir(&id);
    std::fs::create_dir_all(&vdir)?;
    fsutil::set_mode(&vdir, 0o755)?;
    for b in &broken {
        let (staged, executable) = match b {
            Broken::Artifact(_, a, _) => (staging.join(&a.name), a.format == Format::Binary),
            Broken::Wintun(_) => (staging.join("wintun.dll"), false),
        };
        let dest = b.path();
        fsutil::remove_any(dest)?;
        std::fs::rename(&staged, dest)?;
        if executable {
            fsutil::set_mode(dest, 0o755)?;
        }
        ctx.ui.success(&format!("已替换 {}", dest.display()));
    }
    for r in &releases {
        if !vdir.join(release::saved_names(r.manifest.product).0).exists() {
            r.save_manifest(&vdir)?;
        }
    }
    let _ = fsutil::remove_any(&staging);

    let users = state.admin_users.clone();
    prepare_accounts(ctx, &users, &mut state)?;
    if webui {
        prepare_webui(ctx, None, None)?;
    }
    let password_file = Some(initial_password_file(ctx.os)).filter(|p| p.exists());
    let result = activate(ctx, &id, webui, password_file.as_deref());
    state.pending = None;
    state.record("repair", &id, result.is_ok(), result.as_ref().err().map(|e| format!("{e:#}")));
    state.save(&ctx.layout)?;
    result?;
    if webui {
        finish_webui_password(ctx, &id)?;
    }

    if desktop_missing && let Some(rel) = release_of(Product::Controller) {
        ctx.ui.section("桌面控制器");
        match desktop::select(&rel.manifest, ctx.os, ctx.arch).and_then(|a| install_desktop(ctx, rel, a, &state)) {
            Ok(d) => {
                ctx.ui.success("桌面控制器已重新安装");
                state.desktop = Some(d);
                state.save(&ctx.layout)?;
            }
            Err(e) => ctx.ui.warn(&format!("桌面控制器安装失败：{e:#}")),
        }
    }
    ctx.ui.success("修复完成");
    Ok(())
}

pub fn rollback(ctx: &Ctx, state: InstallState) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let Some(prev) = state.previous.clone() else {
        bail!("没有可回滚的版本（只保留上一次升级前的版本）。");
    };
    let webui = state.controller == ControllerKind::Webui;
    let saved = saved_manifests(ctx, &prev).with_context(|| format!("版本 {prev} 的文件不可用，无法回滚"))?;
    let broken = broken_files(ctx, &prev, &saved, webui);
    if !broken.is_empty() {
        bail!("版本 {prev} 的文件已损坏或缺失（{}），无法回滚", broken.iter().map(|b| b.path().display().to_string()).collect::<Vec<_>>().join("、"));
    }
    let (from, to) = (Versions::parse_id(&state.version).label(), Versions::parse_id(&prev).label());
    let steps = vec![format!("切换服务 {from} → {to}，启动并做健康检查；失败则切回原版本"), "桌面控制器不随之回滚".into()];
    if !ctx.ui.plan(&format!("回滚到 {to}"), &steps, ctx.dry_run)? {
        return Ok(());
    }
    let mut state = state;
    let current = state.version.clone();
    state.begin(&ctx.layout, "rollback", &prev)?;
    ctx.ui.section("回滚");
    if let Err(e) = activate(ctx, &prev, webui, None) {
        ctx.ui.warn(&format!("{e:#}"));
        let back = activate(ctx, &current, webui, None);
        state.pending = None;
        state.record("rollback", &prev, false, Some(format!("{e:#}")));
        state.save(&ctx.layout)?;
        back.context("切回原版本也失败了，请执行「修复」")?;
        bail!("{to} 未能启动，已切回 {from}");
    }
    state.version = prev.clone();
    state.previous = Some(current);
    state.pending = None;
    state.record("rollback", &prev, true, None);
    state.save(&ctx.layout)?;
    ctx.ui.success(&format!("已回滚到 {to}"));
    Ok(())
}

/// Put the 3.0.x installation back from the migration backup.
pub fn rollback_legacy(ctx: &Ctx, state: InstallState) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let dir = state.legacy_backup.clone().context("没有 3.0.x 备份（该系统不是从 3.0.x 迁移的）")?;
    let backup = Backup::open(&dir)?;
    let mut steps = vec![
        "停止并移除 3.1 服务".into(),
        format!("从 {} 还原 3.0.x 程序文件、服务配置与迁移前的数据快照", dir.display()),
        "启动 3.0.x 服务".into(),
        "删除 3.1 程序文件与安装记录（备份保留）".into(),
    ];
    if state.desktop.is_some() {
        steps.push("3.1 桌面控制器保留，可手动卸载".into());
    }
    if !ctx.ui.plan("还原到 3.0.x", &steps, ctx.dry_run)? {
        return Ok(());
    }
    service::remove(ctx.os, Service::Webui)?;
    legacy::restore_native(ctx.os, &backup)?;
    ctx.ui.success("3.0.x 已还原并启动");
    let _ = std::fs::copy(ctx.layout.state_file(), dir.join("installer-state.3.1.json"));
    fsutil::remove_any(&ctx.layout.versions_dir())?;
    fsutil::remove_any(&ctx.layout.state_file())?;
    Ok(())
}

pub fn uninstall(ctx: &Ctx, state: InstallState, purge: bool, keep_desktop: bool) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let data: Vec<_> = layout::data_locations(ctx.os, &ctx.home).into_iter().filter(|l| l.path.exists()).collect();
    let mut steps = vec!["停止并移除 Core 服务".to_string()];
    if state.webui_service {
        steps.push("停止并移除 WebUI 服务".into());
    }
    if let Some(d) = &state.desktop
        && !keep_desktop
    {
        steps.push(format!("卸载桌面控制器（{}）", d.location));
    }
    steps.push(format!("删除程序文件 {}", ctx.layout.versions_dir().display()));
    if purge {
        for l in &data {
            steps.push(format!("删除 {}：{}", l.label, l.path.display()));
        }
        steps.push(format!("删除备份 {}、管理员组 {} 与 WebUI 系统用户", ctx.layout.backups_dir().display(), accounts::admin_group(ctx.os)));
    } else {
        for l in &data {
            steps.push(format!("保留 {}：{}", l.label, l.path.display()));
        }
        steps.push(format!("保留管理员组 {} 与备份（重新安装后身份与配置照常使用）", accounts::admin_group(ctx.os)));
    }
    if !ctx.ui.plan(if purge { "彻底卸载（含身份与配置）" } else { "卸载（保留身份与配置）" }, &steps, ctx.dry_run)? {
        return Ok(());
    }
    if purge {
        ctx.ui.warn("彻底卸载会删除设备身份：重新安装后这台设备将成为一台新设备（新的 ID 与虚拟 IP）。");
        if ctx.ui.interactive && !ctx.ui.assume_yes {
            let typed = ctx.ui.input("输入 purge 确认", "", "--yes")?;
            if typed.trim() != "purge" {
                ctx.ui.info("已取消。");
                return Ok(());
            }
        }
    }

    service::remove(ctx.os, Service::Webui)?;
    service::remove(ctx.os, Service::Core)?;
    ctx.ui.success("服务已移除");
    if let Some(d) = &state.desktop
        && !keep_desktop
    {
        match desktop::uninstall(d) {
            Ok(()) => ctx.ui.success("桌面控制器已卸载"),
            Err(e) => ctx.ui.warn(&format!("卸载桌面控制器失败：{e:#}")),
        }
    }
    for p in [ctx.layout.versions_dir(), ctx.layout.staging_dir(), ctx.layout.root.join("desktop"), ctx.layout.state_file()] {
        fsutil::remove_any(&p)?;
    }
    if purge {
        for l in &data {
            fsutil::remove_any(&l.path)?;
        }
        if ctx.os == Os::Linux {
            accounts::remove_webui_user();
        }
        accounts::remove_admin_group(ctx.os);
        fsutil::remove_any(&ctx.layout.backups_dir())?;
        drop(_lock);
        fsutil::remove_any(&ctx.layout.root)?;
        ctx.ui.success("已彻底卸载");
    } else {
        ctx.ui.success("卸载完成；以下数据已保留：");
        for l in &data {
            ctx.ui.info(&format!("{}：{}", l.label, l.path.display()));
        }
    }
    Ok(())
}

pub fn status(ctx: &Ctx, state: Option<&InstallState>) {
    ctx.ui.section("安装状态");
    match state {
        None => {
            ctx.ui.info("未安装 MSST-Net 3.1");
            if let Some(l) = legacy::detect_native(&ctx.layout) {
                ctx.ui.info("检测到 3.0.x：");
                for d in l.describe() {
                    ctx.ui.info(&format!("  {d}"));
                }
            }
        }
        Some(s) => {
            ctx.ui.info(&format!(
                "版本：{}{}",
                Versions::parse_id(&s.version).label(),
                s.previous.as_ref().map(|p| format!("（可回滚到 {}）", Versions::parse_id(p).label())).unwrap_or_default()
            ));
            ctx.ui.info(&format!(
                "部署方式：{}；控制器：{}",
                if s.deployment == Deployment::Docker { "Docker" } else { "原生（系统服务）" },
                s.controller.label()
            ));
            if let Some(p) = &s.pending {
                ctx.ui.warn(&format!("未完成的操作：{} → {}（请执行修复）", p.action, p.target));
            }
            if s.deployment == Deployment::Native {
                let core = service::is_running(ctx.os, Service::Core);
                ctx.ui.info(&format!("Core 服务：{}", if core { "运行中" } else { "未运行" }));
                if s.webui_service {
                    ctx.ui.info(&format!("WebUI 服务：{}", if service::is_running(ctx.os, Service::Webui) { "运行中" } else { "未运行" }));
                }
                if let Ok(saved) = saved_manifests(ctx, &s.version) {
                    let broken = broken_files(ctx, &s.version, &saved, s.controller == ControllerKind::Webui);
                    if broken.is_empty() {
                        ctx.ui.info("程序文件：完整（SHA-256 一致）");
                    } else {
                        ctx.ui.warn(&format!("程序文件：{} 个损坏或缺失（请执行修复）", broken.len()));
                    }
                }
            }
            if let Some(d) = &s.desktop {
                ctx.ui.info(&format!("桌面控制器：{} {}", d.version, d.location));
            }
            if !s.admin_users.is_empty() {
                ctx.ui.info(&format!("管理员：{}", s.admin_users.join("、")));
            }
            if let Some(b) = &s.legacy_backup {
                ctx.ui.info(&format!("3.0.x 备份：{}", b.display()));
            }
        }
    }
    ctx.ui.section("数据位置");
    for l in layout::data_locations(ctx.os, &ctx.home) {
        if l.path.exists() {
            ctx.ui.info(&format!("{}：{}", l.label, l.path.display()));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn committed_version_dir_is_traversable() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { os: Os::Linux, root: d.path().into() };
        for _ in 0..2 {
            let staging = d.path().join("staging");
            std::fs::create_dir_all(&staging).unwrap();
            fsutil::set_mode(&staging, 0o700).unwrap();
            commit_version(&layout, &staging, "3.1.0+3.1.1").unwrap();
            let mode = std::fs::metadata(layout.version_dir("3.1.0+3.1.1")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}

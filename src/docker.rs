//! Docker deployment (Linux): Core and the WebUI Controller as two
//! containers sharing Core's IPC socket directory (controller
//! `docs/DOCKER.md`).  Offered when the WebUI or the command-line
//! Controller is chosen; with the latter only Core runs in a container and
//! `msst-net-cli` runs on the host (see cli_entry.rs), connecting to
//! `./run/ipc.sock` as root.
//!
//! ```text
//! /opt/msst-net-docker/        (3.0.x's directory: ./data keeps the device identity)
//!   docker-compose.yml         managed template (Core + WebUI, or Core only); images come from .env
//!   .env                       MSST_CORE_IMAGE / MSST_WEBUI_IMAGE / MSST_WEBUI_BIND  (0600)
//!   cli/<controller version>/  msst-net-cli + the Controller's manifest (CLI only; current and previous)
//!   .env.previous              the images before the last upgrade (rollback)
//!   secrets/admin_password     first-start WebUI password, deleted once the WebUI is healthy
//!   .docker/                   registry credentials used by the installer only (0700)
//!   data/  run/  webui/        Core data (identity), IPC socket, WebUI config (password hash, TLS)
//! ```
//!
//! Upgrades pull the new images *before* touching the running containers,
//! and roll back to the previous images if the new ones aren't healthy.
//!
//! Images: `abjust/msstnet31x-core:<Core version>` and
//! `abjust/msstnet31x-webui:<Controller version>` unless a release manifest
//! names (pins) its image.
//!
//! A native installation's device identity (3.0.x, or a 3.1 native install
//! that was uninstalled) is copied into `./data`, so switching to Docker
//! keeps the device.

use crate::bundle::{Bundle, Versions};
use crate::cli_entry::{self, Target};
use crate::cmd;
use crate::context::Ctx;
use crate::fsutil;
use crate::layout;
use crate::health;
use crate::legacy::{self, Backup};
use crate::manifest::{self, Component, Format, Images, Product};
use crate::platform::Os;
use crate::release::{self, Release};
use crate::service::{self, Service};
use crate::state::{ControllerKind, Deployment, InstallState, Lock};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const COMPOSE: &str = include_str!("../docker/docker-compose.yml");
/// Core only (command-line Controller on the host).
const COMPOSE_CORE: &str = include_str!("../docker/docker-compose.core.yml");
const CORE_CONTAINER: &str = "msst-net-core";
const WEBUI_CONTAINER: &str = "msst-net-ctrl-webui";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_BIND: &str = "0.0.0.0:18080";

pub struct DockerOptions {
    /// `Webui` or `Cli`; None: keep the installed choice (WebUI for a new
    /// deployment).
    pub controller: Option<ControllerKind>,
    pub core_version: Option<String>,
    pub controller_version: Option<String>,
    pub images: Option<Images>,
    pub webui_password: Option<String>,
    pub webui_bind: Option<String>,
    pub registry: Option<RegistryLogin>,
}

pub struct RegistryLogin {
    pub server: String,
    pub user: String,
    pub password: String,
}

#[derive(Clone, Copy)]
enum Compose {
    V2,
    V1,
}

struct Env {
    dir: PathBuf,
    compose: Compose,
}

impl Env {
    fn new(ctx: &Ctx) -> Result<Env> {
        Ok(Env { dir: ctx.layout.docker_dir(), compose: check_docker()? })
    }

    fn docker_config(&self) -> PathBuf {
        self.dir.join(".docker")
    }

    /// `docker …` with the installer's own credential store.
    fn docker(&self) -> Command {
        let mut c = Command::new("docker");
        if self.docker_config().join("config.json").exists() {
            c.env("DOCKER_CONFIG", self.docker_config());
        }
        c
    }

    fn compose(&self, args: &[&str]) -> Result<()> {
        let mut c = match self.compose {
            Compose::V2 => {
                let mut c = self.docker();
                c.arg("compose");
                c
            }
            Compose::V1 => Command::new("docker-compose"),
        };
        let status = c.current_dir(&self.dir).args(args).status().context("无法执行 docker compose")?;
        if !status.success() {
            bail!("docker compose {} 失败（退出码 {:?}）", args.join(" "), status.code());
        }
        Ok(())
    }

    fn pull(&self, image: &str) -> Result<()> {
        let out = self.docker().args(["pull", image]).stdin(Stdio::null()).output().context("无法执行 docker pull")?;
        if !out.status.success() {
            bail!("拉取镜像 {image} 失败：{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn login(&self, r: &RegistryLogin) -> Result<()> {
        std::fs::create_dir_all(self.docker_config())?;
        fsutil::set_mode(&self.docker_config(), 0o700)?;
        let mut child = Command::new("docker")
            .env("DOCKER_CONFIG", self.docker_config())
            .args(["login", &r.server, "--username", &r.user, "--password-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("无法执行 docker login")?;
        use std::io::Write;
        child.stdin.take().unwrap().write_all(r.password.as_bytes())?;
        if !child.wait()?.success() {
            bail!("登录镜像仓库 {} 失败", r.server);
        }
        Ok(())
    }

    fn write_env(&self, images: &Images, bind: &str) -> Result<()> {
        let mut text = format!("# Managed by msst-net-installer.\nMSST_CORE_IMAGE={}\n", images.core);
        if let Some(w) = &images.webui {
            text.push_str(&format!("MSST_WEBUI_IMAGE={w}\n"));
        }
        // Kept without the WebUI too: switching back keeps the address.
        text.push_str(&format!("MSST_WEBUI_BIND={bind}\n"));
        fsutil::atomic_write(&self.dir.join(".env"), text.as_bytes(), 0o600)
    }

    fn read_bind(&self) -> String {
        std::fs::read_to_string(self.dir.join(".env"))
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("MSST_WEBUI_BIND=").map(str::to_string)))
            .unwrap_or_else(|| DEFAULT_BIND.into())
    }

    fn write_compose(&self, webui: bool) -> Result<()> {
        for d in ["data", "run"] {
            std::fs::create_dir_all(self.dir.join(d))?;
        }
        if webui {
            std::fs::create_dir_all(self.dir.join("webui"))?;
            fsutil::set_mode(&self.dir.join("webui"), 0o700)?;
            std::fs::create_dir_all(self.dir.join("secrets"))?;
            fsutil::set_mode(&self.dir.join("secrets"), 0o700)?;
        }
        let template = if webui { COMPOSE } else { COMPOSE_CORE };
        fsutil::atomic_write(&self.dir.join("docker-compose.yml"), template.as_bytes(), 0o644)
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/ipc.sock")
    }

    fn secret_file(&self) -> PathBuf {
        self.dir.join("secrets/admin_password")
    }

    fn write_secret(&self, password: &str) -> Result<()> {
        let dir = self.dir.join("secrets");
        std::fs::create_dir_all(&dir)?;
        fsutil::set_mode(&dir, 0o700)?;
        fsutil::atomic_write(&self.secret_file(), password.as_bytes(), 0o600)
    }

    fn has_password(&self) -> bool {
        self.dir.join("webui/passwd").exists()
    }

    /// Wait until the containers (Core, and the WebUI if `webui`) run and
    /// report healthy, and Core's socket accepts connections from the host.
    fn wait_healthy(&self, webui: bool) -> Result<()> {
        let start = Instant::now();
        let socket = self.socket();
        let mut last = String::new();
        while start.elapsed() < HEALTH_TIMEOUT {
            let core = self.container_status(CORE_CONTAINER);
            let webui = if webui { self.container_status(WEBUI_CONTAINER) } else { "running healthy".into() };
            if core == "running healthy" && webui == "running healthy" && socket_accepts(&socket) {
                return Ok(());
            }
            if core.starts_with("exited") || webui.starts_with("exited") || core.starts_with("restarting") && start.elapsed() > Duration::from_secs(30) {
                last = format!("Core：{core}；WebUI：{webui}");
                break;
            }
            last = format!("Core：{core}；WebUI：{webui}");
            std::thread::sleep(Duration::from_secs(2));
        }
        let logs = |c: &str| {
            cmd::output("docker", &["logs", "--tail", "15", c]).unwrap_or_default()
        };
        bail!("容器未就绪（{last}）\nCore 日志：\n{}\nWebUI 日志：\n{}", logs(CORE_CONTAINER).trim(), logs(WEBUI_CONTAINER).trim())
    }

    fn container_status(&self, name: &str) -> String {
        cmd::output("docker", &["inspect", "-f", "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}", name])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "不存在".into())
    }
}

#[cfg(unix)]
fn socket_accepts(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn socket_accepts(_path: &Path) -> bool {
    false
}

/// Docker Engine running, Compose available, and a setup Core can work in.
fn check_docker() -> Result<Compose> {
    if !cmd::succeeds("docker", &["info"]) {
        bail!("未检测到运行中的 Docker Engine，请先安装并启动 Docker。");
    }
    let security = cmd::output("docker", &["info", "--format", "{{json .SecurityOptions}}"]).unwrap_or_default();
    if security.contains("rootless") || security.contains("userns") {
        bail!("不支持 rootless Docker 或用户命名空间重映射：容器内的 root 不是主机 root，Core 会拒绝 WebUI 的连接（见 DOCKER.md）。");
    }
    if !Path::new("/dev/net/tun").exists() {
        bail!("主机没有 /dev/net/tun，Core 容器无法创建虚拟网卡。");
    }
    if cmd::succeeds("docker", &["compose", "version"]) {
        Ok(Compose::V2)
    } else if cmd::succeeds("docker-compose", &["version"]) {
        Ok(Compose::V1)
    } else {
        bail!("未找到 docker compose 或 docker-compose，请安装 Docker Compose。")
    }
}

/// A 3.0.x Docker deployment (installed by the 3.0.x installer).
pub fn detect_legacy(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("docker-compose.yml")).is_ok_and(|t| t.contains("abjust/msstnet-"))
}

/// The version id and images to deploy: from the Core and Controller
/// releases, or given explicitly (`--core-image` / `--webui-image`).  With
/// the command-line Controller the Controller release is always resolved
/// (it carries the CLI) and returned.
fn images_for(ctx: &Ctx, opts: &DockerOptions, cli: bool) -> Result<(String, Images, Option<Release>)> {
    match &opts.images {
        Some(i) if cli && i.webui.is_some() => bail!("命令行控制器不使用 WebUI 镜像（去掉 --webui-image）"),
        Some(i) if !cli && i.webui.is_none() => bail!("WebUI 部署需同时指定 --core-image 与 --webui-image"),
        Some(i) if !cli && opts.core_version.is_none() && opts.controller_version.is_none() => {
            return Ok(("custom".into(), i.clone(), None));
        }
        _ => {}
    }
    let b = Bundle::resolve(ctx, opts.core_version.as_deref(), opts.controller_version.as_deref(), true)?;
    let ctl = b.controller.expect("controller release");
    let images = opts.images.clone().unwrap_or_else(|| Images {
        core: b.core.manifest.image(),
        webui: (!cli).then(|| ctl.manifest.image()),
    });
    let id = Versions { core: b.core.manifest.version.clone(), controller: Some(ctl.manifest.version.clone()) }.id();
    Ok((id, images, cli.then_some(ctl)))
}

/// The host CLI of a Docker deployment: `cli/<controller version>/msst-net-cli`.
fn cli_exe(ctx: &Ctx, controller_version: &str) -> PathBuf {
    ctx.layout.docker_cli_dir(controller_version).join("msst-net-cli")
}

/// Download and self-test the CLI into a staging directory (with the
/// Controller's manifest, for repair).  Nothing installed is touched.
fn stage_cli(ctx: &Ctx, rel: &Release) -> Result<PathBuf> {
    let art = rel.manifest.require(Component::Cli, ctx.os, ctx.arch, Format::Binary)?;
    let dir = ctx.staging()?;
    let result = (|| -> Result<()> {
        ctx.download(rel, &[art], &dir)?;
        let exe = dir.join("msst-net-cli");
        std::fs::rename(dir.join(&art.name), &exe)?;
        fsutil::set_mode(&exe, 0o755)?;
        rel.save_manifest(&dir)?;
        health::preflight(&exe, &rel.manifest.version)?;
        ctx.ui.success("命令行控制器自检通过");
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fsutil::remove_any(&dir);
        return Err(e);
    }
    Ok(dir)
}

/// Move a staged CLI to `cli/<version>/` (replacing it).
fn commit_cli(ctx: &Ctx, staged: &Path, version: &str) -> Result<()> {
    let dest = ctx.layout.docker_cli_dir(version);
    fsutil::remove_any(&dest)?;
    std::fs::create_dir_all(dest.parent().expect("cli dir"))?;
    fsutil::move_path(staged, &dest)?;
    fsutil::set_mode(&dest, 0o755)
}

/// Point `msst-net-cli` at the CLI of `controller_version`.
fn point_cli(ctx: &Ctx, env: &Env, controller_version: &str) -> Result<()> {
    let exe = cli_exe(ctx, controller_version);
    cli_entry::point_to(&ctx.layout, Target::Docker { exe: &exe, socket: &env.socket() })?;
    ctx.ui.success(&format!("命令 msst-net-cli 已指向 {}（需以 root 运行：sudo msst-net-cli）", exe.display()));
    Ok(())
}

/// Remove the host CLI (entry and files), e.g. after switching to the WebUI.
fn remove_cli(ctx: &Ctx) -> Result<()> {
    cli_entry::remove(&ctx.layout)?;
    fsutil::remove_any(&ctx.layout.docker_dir().join("cli"))
}

/// Keep the CLI of the current and previous versions.
fn prune_cli(ctx: &Ctx, state: &InstallState) {
    let keep: Vec<String> =
        [Some(&state.version), state.previous.as_ref()].into_iter().flatten().filter_map(|id| Versions::parse_id(id).controller).collect();
    for dir in fsutil::list_dir(&ctx.layout.docker_dir().join("cli")) {
        if !keep.contains(&fsutil::file_name(&dir)) {
            let _ = fsutil::remove_any(&dir);
        }
    }
}

/// Whether the installed CLI of `controller_version` matches its saved,
/// verified manifest.
fn cli_intact(ctx: &Ctx, controller_version: &str) -> bool {
    let dir = ctx.layout.docker_cli_dir(controller_version);
    let (mname, sname) = release::saved_names(Product::Controller);
    let Ok(bytes) = std::fs::read(dir.join(&mname)) else { return false };
    let sig = std::fs::read(dir.join(sname)).ok();
    let Ok((m, _)) = manifest::verify(Product::Controller, &bytes, sig.as_deref(), &ctx.trust) else { return false };
    m.version == controller_version
        && m.find(Component::Cli, ctx.os, ctx.arch, Format::Binary).is_some_and(|a| release::matches(&dir.join("msst-net-cli"), a))
}

/// A native data directory with a device identity that `./data` lacks.
fn identity_to_import(ctx: &Ctx, env: &Env) -> Option<PathBuf> {
    if env.dir.join("data/identity.json").exists() {
        return None;
    }
    Some(layout::effective_core_data_dir(ctx.os, &ctx.home)).filter(|d| d.join("identity.json").is_file())
}

fn label(id: &str) -> String {
    if id == "custom" { "自定义镜像".into() } else { Versions::parse_id(id).label() }
}

/// Install, upgrade, or migrate a 3.0.x Docker deployment.
pub fn install(ctx: &Ctx, opts: DockerOptions) -> Result<()> {
    if ctx.os != Os::Linux {
        bail!("Docker 部署仅支持 Linux（Core 容器需要主机网络、NET_ADMIN 与 /dev/net/tun；Docker Desktop 不支持）。");
    }
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let existing = InstallState::load(&ctx.layout)?;
    if existing.as_ref().is_some_and(|s| s.deployment == Deployment::Native) {
        bail!("当前为原生部署；两种部署会争用同一端口和虚拟网卡。请先卸载原生部署（身份与配置会保留，Docker 部署时自动沿用）。");
    }
    let env = Env::new(ctx)?;
    let legacy = existing.is_none() && detect_legacy(&env.dir);
    let legacy_native = if existing.is_none() { legacy::detect_native(&ctx.layout) } else { None };
    if let Some(l) = &legacy_native {
        ctx.ui.section("检测到 MSST-Net 3.0.x（原生安装）");
        for d in l.describe() {
            ctx.ui.info(&d);
        }
    }
    let controller = opts.controller.or(existing.as_ref().map(|s| s.controller)).unwrap_or(ControllerKind::Webui);
    if !matches!(controller, ControllerKind::Webui | ControllerKind::Cli) {
        bail!("Docker 部署的控制器只能是 WebUI 或命令行（--controller webui|cli）");
    }
    let (webui, cli) = (controller == ControllerKind::Webui, controller == ControllerKind::Cli);
    if cli {
        cli_entry::check_free(&ctx.layout)?;
    }
    let (version, images, cli_release) = images_for(ctx, &opts, cli)?;
    let cli_version = cli_release.as_ref().map(|r| r.manifest.version.clone());
    if let Some(s) = &existing
        && s.controller == controller
        && s.images.as_ref() == Some(&images)
        && (!cli || s.version == version)
    {
        ctx.ui.info(&format!("已在使用这些镜像（{}），无需升级。容器异常请使用「修复」。", label(&version)));
        return Ok(());
    }
    if let Some(s) = &existing
        && version != "custom"
        && s.version != "custom"
        && Versions::parse_id(&version).is_downgrade_from(&Versions::parse_id(&s.version))
    {
        ctx.ui.warn(&format!("目标 {} 低于已安装的 {}（降级）。", label(&version), label(&s.version)));
        if !ctx.ui.confirm("确定降级？", false)? {
            return Ok(());
        }
    }
    let import = if existing.is_none() && !legacy { identity_to_import(ctx, &env) } else { None };
    let import = match import {
        Some(d) if legacy_native.is_some() || ctx.ui.choose_yn(&format!("沿用本机原生安装的设备身份（{}）？", d.display()), true)? => Some(d),
        _ => None,
    };
    let bind = opts.webui_bind.clone().unwrap_or_else(|| if existing.is_some() { env.read_bind() } else { DEFAULT_BIND.into() });

    let can_import = webui && legacy && cmd::succeeds("docker", &["inspect", WEBUI_CONTAINER]);
    let password = match &opts.webui_password {
        _ if !webui => None,
        Some(p) => Some(p.clone()),
        None if env.has_password() || can_import => None,
        None => Some(ctx.ui.new_password("设置 WebUI 访问密码", "--admin-password-file")?),
    };

    let mut steps: Vec<String> =
        [&images.core].into_iter().chain(&images.webui).map(|i| format!("拉取镜像 {i}")).collect();
    if let Some(last) = steps.last_mut() {
        last.push_str("（拉取失败则中止，正在运行的容器不受影响）");
    }
    if let Some(r) = &cli_release {
        steps.push(format!("下载命令行控制器 {} 并校验 SHA-256、自检（在主机上运行）", r.manifest.version));
    }
    if opts.registry.is_some() {
        steps.insert(0, format!("登录镜像仓库（凭据仅保存在 {}，权限 0700）", env.docker_config().display()));
    }
    if legacy_native.is_some() {
        steps.push(format!("备份 3.0.x 身份与配置数据到 {}", ctx.layout.backups_dir().display()));
        steps.push("停止并移除 3.0.x 原生服务，将程序文件移入备份（不删除，失败自动还原）".into());
    }
    if let Some(d) = &import {
        steps.push(format!("将设备身份从 {} 复制到 {}/data", d.display(), env.dir.display()));
    }
    if legacy {
        steps.push(format!("备份 3.0.x 的 compose 文件与 ./data（设备身份）到 {}", ctx.layout.backups_dir().display()));
        if can_import {
            steps.push("尝试沿用 3.0.x WebUI 容器中的密码与配置".into());
        }
        steps.push("停止 3.0.x 容器（镜像保留，失败时可还原）".into());
    }
    steps.push(format!("写入 {}/docker-compose.yml 与 .env", env.dir.display()));
    steps.push(match &existing {
        Some(s) => format!("重建容器并做健康检查；失败自动回滚到 {}", label(&s.version)),
        None => "启动容器并做健康检查".into(),
    });
    if webui {
        steps.push(format!("WebUI 监听 {bind}；保留 ./data（身份）与 ./webui（密码、证书）"));
    } else {
        steps.push(format!(
            "命令 msst-net-cli（{}）指向主机上的命令行控制器；Core 只接受 root 的连接（sudo msst-net-cli）",
            ctx.layout.cli_entry().display()
        ));
        if existing.as_ref().is_some_and(|s| s.controller == ControllerKind::Webui) {
            steps.push("移除 WebUI 容器（./webui 中的密码与证书保留）".into());
        }
    }
    if webui && existing.as_ref().is_some_and(|s| s.controller == ControllerKind::Cli) {
        steps.push("移除命令 msst-net-cli 与主机上的命令行控制器".into());
    }
    if !ctx.ui.plan(&format!("Docker 部署 MSST-Net（{}）", label(&version)), &steps, ctx.dry_run)? {
        return Ok(());
    }

    std::fs::create_dir_all(&env.dir)?;
    ctx.ui.section("拉取镜像");
    if let Some(r) = &opts.registry {
        env.login(r)?;
        ctx.ui.success(&format!("已登录 {}", r.server));
    }
    env.pull(&images.core)?;
    if let Some(w) = &images.webui {
        env.pull(w)?;
    }
    ctx.ui.success("镜像已拉取");
    let cli_staged = match &cli_release {
        Some(r) => {
            ctx.ui.section("命令行控制器");
            Some(stage_cli(ctx, r).context("下载或校验失败，现有部署未做任何更改")?)
        }
        None => None,
    };

    let mut backup = None;
    let mut native_backup = None;
    if let Some(l) = &legacy_native {
        ctx.ui.section("备份 3.0.x");
        let mut b = Backup::create(&ctx.layout, "3.0.x-to-docker")?;
        let r = legacy::snapshot_data(ctx.os, &ctx.home, &mut b)
            .and_then(|_| legacy::retire_native(ctx.os, l, &mut b))
            // The definition is in the backup; a disabled-but-present unit
            // would try to start the moved binary at boot.
            .and_then(|_| service::remove(ctx.os, Service::Core));
        if let Err(e) = r {
            let _ = legacy::restore_native(ctx.os, &b);
            return Err(e.context("备份 3.0.x 失败，已还原"));
        }
        ctx.ui.success(&format!("3.0.x 已停止，备份：{}", b.dir.display()));
        native_backup = Some(b);
    }
    if let Some(d) = &import {
        let n = fsutil::copy_dir(d, &env.dir.join("data"))?;
        ctx.ui.success(&format!("已复制设备身份（{n} 个文件）"));
        let config = layout::data_locations(ctx.os, &ctx.home).into_iter().find(|l| l.path.join("core.toml").exists());
        if let Some(c) = config.filter(|c| c.path != *d) {
            ctx.ui.warn(&format!("{} 未迁移；如有自定义设置（如协调服务器），请写入 {}/data/core.toml", c.path.join("core.toml").display(), env.dir.display()));
        }
    }
    if legacy {
        ctx.ui.section("备份 3.0.x");
        let mut b = Backup::create(&ctx.layout, "3.0.x-docker")?;
        b.copy(&env.dir.join("docker-compose.yml"))?;
        if env.dir.join("data").is_dir() {
            b.copy(&env.dir.join("data"))?;
        }
        if can_import {
            import_legacy_webui(ctx, &env);
        }
        env.compose(&["down"])?;
        ctx.ui.success(&format!("3.0.x 容器已停止；备份：{}", b.dir.display()));
        backup = Some(b);
    }

    let mut state = existing.clone().unwrap_or_else(|| InstallState::new(Deployment::Docker, controller, &version, &ctx.source.key()));
    state.begin(&ctx.layout, if existing.is_some() { "upgrade" } else { "install" }, &version)?;
    let had_env = env.dir.join(".env").exists() && existing.is_some();
    let result = (|| -> Result<()> {
        if had_env {
            std::fs::copy(env.dir.join(".env"), env.dir.join(".env.previous"))?;
        }
        env.write_compose(webui)?;
        env.write_env(&images, &bind)?;
        if let Some(pw) = &password {
            env.write_secret(pw)?;
        }
        ctx.ui.section("启动容器");
        env.compose(&["up", "-d", "--remove-orphans"])?;
        env.wait_healthy(webui)?;
        // Core is healthy: switch the host CLI last.
        if let (Some(staged), Some(v)) = (&cli_staged, &cli_version) {
            commit_cli(ctx, staged, v)?;
            point_cli(ctx, &env, v)?;
        }
        Ok(())
    })();
    if let Some(staged) = &cli_staged {
        let _ = fsutil::remove_any(staged);
    }

    if let Err(e) = result {
        ctx.ui.warn(&format!("{e:#}"));
        ctx.ui.section("回滚");
        let prev_webui = existing.as_ref().is_none_or(|s| s.controller == ControllerKind::Webui);
        let restored = if had_env {
            std::fs::copy(env.dir.join(".env.previous"), env.dir.join(".env"))
                .map_err(anyhow::Error::from)
                .and_then(|_| env.write_compose(prev_webui))
                .and_then(|_| env.compose(&["up", "-d", "--remove-orphans"]))
                .and_then(|_| env.wait_healthy(prev_webui))
        } else if let Some(b) = &backup {
            let _ = env.compose(&["down"]);
            b.restore().and_then(|_| {
                let _ = std::fs::remove_file(env.dir.join(".env"));
                env.compose(&["up", "-d"])
            })
        } else {
            env.compose(&["down"])
        };
        let restored = match &native_backup {
            Some(b) => restored.and_then(|_| legacy::restore_native(ctx.os, b)),
            None => restored,
        };
        match restored {
            Ok(()) => ctx.ui.success("已恢复到更改前的状态"),
            Err(r) => ctx.ui.warn(&format!("恢复失败：{r:#}")),
        }
        match existing {
            Some(mut s) => {
                s.pending = None;
                s.record("upgrade", &version, false, Some(format!("{e:#}")));
                s.save(&ctx.layout)?;
            }
            None => {
                if cli {
                    let _ = remove_cli(ctx);
                }
                fsutil::remove_any(&ctx.layout.state_file())?
            }
        }
        return Err(e.context("Docker 部署失败，已撤销本次更改"));
    }

    if webui {
        finish_secret(ctx, &env);
    }
    if let Some(prev) = &state.images
        && existing.is_some()
    {
        state.previous_images = Some(prev.clone());
        state.previous = Some(state.version.clone());
    }
    state.images = Some(images);
    state.version = version.clone();
    state.controller = controller;
    state.source = ctx.source.key();
    state.legacy_backup = backup.as_ref().or(native_backup.as_ref()).map(|b| b.dir.clone()).or(state.legacy_backup.take());
    state.pending = None;
    state.record(if existing.is_some() { "upgrade" } else { "install" }, &version, true, None);
    state.save(&ctx.layout)?;
    if cli {
        prune_cli(ctx, &state);
    } else if let Err(e) = remove_cli(ctx) {
        ctx.ui.warn(&format!("移除命令行控制器失败：{e:#}"));
    }

    ctx.ui.section("完成");
    ctx.ui.success(&format!("MSST-Net（{}，Docker）已就绪", label(&version)));
    ctx.ui.info(&format!("部署目录：{}", env.dir.display()));
    if webui {
        ctx.ui.info(&format!("WebUI：https://<本机地址>:{}", bind.rsplit(':').next().unwrap_or("18080")));
    } else {
        ctx.ui.info("命令行控制器：sudo msst-net-cli --help（Core 在容器中，只接受 root 的连接）");
    }
    Ok(())
}

/// The 3.0.x WebUI container kept its password inside the container; copy
/// it out before the container is removed (best effort).
fn import_legacy_webui(ctx: &Ctx, env: &Env) {
    let dest = env.dir.join("webui");
    let _ = std::fs::create_dir_all(&dest);
    for src in ["/root/.config/msst-net-webui/.", "/var/lib/msst-webui/."] {
        let from = format!("{WEBUI_CONTAINER}:{src}");
        if cmd::succeeds("docker", &["cp", &from, &dest.to_string_lossy()]) && dest.join("passwd").exists() {
            ctx.ui.success("已沿用 3.0.x WebUI 的密码与配置");
            return;
        }
    }
    ctx.ui.warn("未能从 3.0.x WebUI 容器取得密码，将使用新设置的密码。");
}

/// Delete the first-start password once the WebUI stored its hash.
fn finish_secret(ctx: &Ctx, env: &Env) {
    if env.secret_file().exists() {
        if env.has_password() {
            let _ = std::fs::remove_file(env.secret_file());
        } else {
            ctx.ui.warn(&format!("WebUI 尚未保存密码，{} 暂时保留", env.secret_file().display()));
        }
    }
}

pub fn repair(ctx: &Ctx, mut state: InstallState) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let env = Env::new(ctx)?;
    let images = state.images.clone().context("安装记录中没有镜像信息")?;
    let webui = state.controller == ControllerKind::Webui;
    let cli_version = (state.controller == ControllerKind::Cli)
        .then(|| Versions::parse_id(&state.version).controller.context("安装记录中没有控制器版本"))
        .transpose()?;
    if cli_version.is_some() {
        cli_entry::check_free(&ctx.layout)?;
    }
    let cli_broken = cli_version.as_deref().is_some_and(|v| !cli_intact(ctx, v));
    let mut steps = vec![
        format!("重写 docker-compose.yml 与 .env（保留 ./data{}）", if webui { "、./webui" } else { "" }),
        "缺失的镜像重新拉取".into(),
        "重建容器并做健康检查".into(),
    ];
    if let Some(v) = &cli_version {
        if cli_broken {
            steps.push(format!("重新下载命令行控制器 {v}（文件缺失或损坏）"));
        }
        steps.push(format!("重建命令 msst-net-cli（{}）", ctx.layout.cli_entry().display()));
    }
    if !ctx.ui.plan(&format!("修复 Docker 部署（{}）", label(&state.version)), &steps, ctx.dry_run)? {
        return Ok(());
    }
    for img in [&images.core].into_iter().chain(&images.webui) {
        if !cmd::succeeds("docker", &["image", "inspect", img]) {
            env.pull(img)?;
        }
    }
    let cli_staged = match &cli_version {
        Some(v) if cli_broken => Some(stage_cli(ctx, &ctx.resolve(Product::Controller, Some(v))?)?),
        _ => None,
    };
    let bind = env.read_bind();
    env.write_compose(webui)?;
    env.write_env(&images, &bind)?;
    if webui && !env.has_password() && !env.secret_file().exists() {
        let pw = ctx.ui.new_password("WebUI 尚未设置密码，请设置", "--admin-password-file")?;
        env.write_secret(&pw)?;
    }
    state.begin(&ctx.layout, "repair", &state.version.clone())?;
    let result = env.compose(&["up", "-d", "--force-recreate", "--remove-orphans"]).and_then(|_| env.wait_healthy(webui)).and_then(|_| {
        let Some(v) = &cli_version else { return Ok(()) };
        if let Some(staged) = &cli_staged {
            commit_cli(ctx, staged, v)?;
        }
        point_cli(ctx, &env, v)
    });
    if let Some(staged) = &cli_staged {
        let _ = fsutil::remove_any(staged);
    }
    state.pending = None;
    state.record("repair", &state.version.clone(), result.is_ok(), result.as_ref().err().map(|e| format!("{e:#}")));
    state.save(&ctx.layout)?;
    result?;
    if webui {
        finish_secret(ctx, &env);
    }
    ctx.ui.success("修复完成");
    Ok(())
}

pub fn rollback(ctx: &Ctx, mut state: InstallState) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let prev = state.previous_images.clone().context("没有可回滚的镜像版本")?;
    let prev_version = state.previous.clone().unwrap_or_else(|| "custom".into());
    let webui = state.controller == ControllerKind::Webui;
    if prev.webui.is_some() != webui {
        bail!("上一版本使用的是另一种控制器，无法回滚；请用 install --controller 重新选择。");
    }
    let cli_version = if webui { None } else { Some(Versions::parse_id(&prev_version).controller.context("上一版本没有命令行控制器")?) };
    if let Some(v) = &cli_version
        && !cli_intact(ctx, v)
    {
        bail!("命令行控制器 {v} 的文件已损坏或缺失（{}），无法回滚", cli_exe(ctx, v).display());
    }
    let env = Env::new(ctx)?;
    let images: Vec<&String> = [&prev.core].into_iter().chain(&prev.webui).collect();
    let mut steps = vec![format!(
        "切换到 {}，重建容器并做健康检查；失败则切回 {}",
        images.iter().map(|i| i.as_str()).collect::<Vec<_>>().join(" / "),
        state.version
    )];
    if let Some(v) = &cli_version {
        steps.push(format!("命令 msst-net-cli 切换到 {}", cli_exe(ctx, v).display()));
    }
    if !ctx.ui.plan(&format!("回滚到 {}", label(&prev_version)), &steps, ctx.dry_run)? {
        return Ok(());
    }
    for img in images {
        if !cmd::succeeds("docker", &["image", "inspect", img]) {
            env.pull(img)?;
        }
    }
    let current = state.images.clone().context("安装记录中没有镜像信息")?;
    let bind = env.read_bind();
    state.begin(&ctx.layout, "rollback", &prev_version)?;
    env.write_env(&prev, &bind)?;
    let result = env
        .compose(&["up", "-d", "--remove-orphans"])
        .and_then(|_| env.wait_healthy(webui))
        .and_then(|_| cli_version.as_deref().map_or(Ok(()), |v| point_cli(ctx, &env, v)));
    if let Err(e) = result {
        env.write_env(&current, &bind)?;
        let back = env.compose(&["up", "-d", "--remove-orphans"]).and_then(|_| env.wait_healthy(webui));
        state.pending = None;
        state.record("rollback", &prev_version, false, Some(format!("{e:#}")));
        state.save(&ctx.layout)?;
        back.context("切回原版本也失败了，请执行「修复」")?;
        bail!("{} 未能正常运行（{e:#}），已切回 {}", label(&prev_version), label(&state.version));
    }
    let cur_version = std::mem::replace(&mut state.version, prev_version.clone());
    state.previous = Some(cur_version);
    state.previous_images = Some(current);
    state.images = Some(prev);
    state.pending = None;
    state.record("rollback", &prev_version, true, None);
    state.save(&ctx.layout)?;
    ctx.ui.success(&format!("已回滚到 {}", label(&prev_version)));
    Ok(())
}

pub fn uninstall(ctx: &Ctx, state: InstallState, purge: bool) -> Result<()> {
    let _lock = (!ctx.dry_run).then(|| Lock::acquire(&ctx.layout)).transpose()?;
    let dir = ctx.layout.docker_dir();
    let mut steps = vec!["停止并删除容器".to_string(), "删除 compose 文件、.env 与安装器保存的仓库凭据".into()];
    if state.controller == ControllerKind::Cli || cli_entry::exists(&ctx.layout) {
        steps.push(format!("删除命令 msst-net-cli（{}）与主机上的命令行控制器", ctx.layout.cli_entry().display()));
    }
    if purge {
        steps.push(format!("删除 {}（含设备身份 ./data 与 WebUI 配置 ./webui）及镜像", dir.display()));
    } else {
        steps.push(format!("保留 {}/data（设备身份）与 {}/webui（密码、证书）", dir.display(), dir.display()));
    }
    if !ctx.ui.plan(if purge { "彻底卸载 Docker 部署" } else { "卸载 Docker 部署（保留身份与配置）" }, &steps, ctx.dry_run)? {
        return Ok(());
    }
    if purge && ctx.ui.interactive && !ctx.ui.assume_yes && ctx.ui.input("输入 purge 确认", "", "--yes")?.trim() != "purge" {
        ctx.ui.info("已取消。");
        return Ok(());
    }
    if dir.join("docker-compose.yml").exists() {
        let env = Env::new(ctx)?;
        let mut args = vec!["down", "--remove-orphans"];
        if purge {
            args.extend(["--rmi", "all"]);
        }
        env.compose(&args)?;
    }
    for f in ["docker-compose.yml", ".env", ".env.previous", "secrets", ".docker", "run"] {
        fsutil::remove_any(&dir.join(f))?;
    }
    remove_cli(ctx)?;
    fsutil::remove_any(&ctx.layout.state_file())?;
    if purge {
        fsutil::remove_any(&dir)?;
        if let Some(b) = &state.legacy_backup {
            fsutil::remove_any(b)?;
        }
        ctx.ui.success("已彻底卸载");
    } else {
        ctx.ui.success(&format!("卸载完成；{}/data 与 {}/webui 已保留，重新安装后自动沿用", dir.display(), dir.display()));
    }
    Ok(())
}

pub fn status(ctx: &Ctx, state: &InstallState) {
    let Ok(env) = Env::new(ctx) else {
        ctx.ui.warn("Docker 不可用");
        return;
    };
    if let Some(i) = &state.images {
        ctx.ui.info(&format!("镜像：{}", [&i.core].into_iter().chain(&i.webui).map(String::as_str).collect::<Vec<_>>().join(" / ")));
    }
    ctx.ui.info(&format!("Core 容器：{}", env.container_status(CORE_CONTAINER)));
    if state.controller == ControllerKind::Webui {
        ctx.ui.info(&format!("WebUI 容器：{}", env.container_status(WEBUI_CONTAINER)));
    }
    ctx.ui.info(&format!("部署目录：{}", env.dir.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_template_contract() {
        // The shared-socket layout of controller docs/DOCKER.md.
        assert!(COMPOSE.contains("CORE_SOCKET_PATH: /run/msst/core.sock"));
        assert!(COMPOSE.contains("CORE_SOCKET: /run/msst/core.sock"));
        assert!(COMPOSE.contains("- ./data:/var/lib/msst"));
        assert!(COMPOSE.contains("- ./webui:/var/lib/msst-webui"));
        assert!(COMPOSE.contains("${MSST_CORE_IMAGE:?"));
        assert!(!COMPOSE.contains("ADMIN_PASSWORD"), "the password goes through a secret file, not the environment");
        // Core only: the same Core service, no WebUI.
        let core_service = |t: &str| t[t.find("  msst-net-core:").unwrap()..].split("\n\n").next().unwrap().trim_end().to_string();
        assert_eq!(core_service(COMPOSE_CORE), core_service(COMPOSE));
        assert!(!COMPOSE_CORE.contains("msst-net-ctrl-webui"));
    }

    #[test]
    fn legacy_detection() {
        let d = tempfile::tempdir().unwrap();
        assert!(!detect_legacy(d.path()));
        std::fs::write(d.path().join("docker-compose.yml"), "image: abjust/msstnet-core:latest").unwrap();
        assert!(detect_legacy(d.path()));
        std::fs::write(d.path().join("docker-compose.yml"), COMPOSE).unwrap();
        assert!(!detect_legacy(d.path()));
    }
}

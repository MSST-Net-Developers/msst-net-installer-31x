//! MSST-Net 3.1.x installer.
//!
//! Run without arguments for the interactive wizard, or with a subcommand
//! for unattended use (`install --controller webui --yes …`).  See
//! docs/DEPLOYMENT_CONTRACT.md for the release and service contract.

mod accounts;
mod bundle;
mod cli_entry;
mod cmd;
mod context;
mod desktop;
mod docker;
mod fsutil;
mod health;
mod layout;
mod legacy;
mod manifest;
mod native;
mod platform;
mod release;
mod release_tools;
mod service;
mod state;
mod ui;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use context::Ctx;
use layout::Layout;
use manifest::{Images, Product, TrustPolicy};
use platform::{Arch, Os};
use release::Source;
use state::{ControllerKind, Deployment, InstallState};
use std::io::IsTerminal;
use std::path::PathBuf;
use ui::Ui;

#[derive(Parser)]
#[command(name = "msst-net-installer", version, about = "MSST-Net 3.1.x 安装程序")]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args)]
struct Global {
    /// 下载源
    #[arg(long, global = true, value_enum)]
    mirror: Option<Mirror>,
    /// 从本地目录安装（含 manifest.json、签名与安装文件；离线安装、发布测试）
    #[arg(long, global = true, value_name = "DIR", conflicts_with = "mirror")]
    from_dir: Option<PathBuf>,
    /// 接受未签名的发布清单（仍校验每个文件的 SHA-256）
    #[arg(long, global = true)]
    allow_unsigned: bool,
    /// 对所有确认回答“是”（无人值守）
    #[arg(short, long, global = true)]
    yes: bool,
    /// 从不提示；缺少必要选项时报错
    #[arg(long, global = true)]
    non_interactive: bool,
    /// 只显示将执行的计划
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mirror {
    Gitee,
    Github,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum ControllerArg {
    Desktop,
    Webui,
    /// 命令行控制器 msst-net-cli
    Cli,
    None,
}

impl From<ControllerArg> for ControllerKind {
    fn from(c: ControllerArg) -> Self {
        match c {
            ControllerArg::Desktop => ControllerKind::Desktop,
            ControllerArg::Webui => ControllerKind::Webui,
            ControllerArg::Cli => ControllerKind::Cli,
            ControllerArg::None => ControllerKind::None,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// 安装（检测到 3.0.x 时迁移）
    Install(InstallArgs),
    /// 升级到最新或指定版本（失败自动回滚）
    Upgrade(UpgradeArgs),
    /// 修复：校验并替换损坏的文件，重写服务配置，重启并做健康检查
    Repair,
    /// 回滚到上一版本
    Rollback {
        /// 还原迁移前的 3.0.x 安装
        #[arg(long)]
        legacy: bool,
    },
    /// 卸载（默认保留设备身份与配置）
    Uninstall {
        /// 同时删除设备身份、配置、备份与管理员组
        #[arg(long)]
        purge: bool,
        /// 保留桌面控制器
        #[arg(long)]
        keep_desktop: bool,
    },
    /// 查看安装状态
    Status,
    /// 发布工具（CI 使用）
    #[command(subcommand, hide = true)]
    Release(ReleaseCmd),
}

#[derive(Args)]
struct InstallArgs {
    /// 控制器类型
    #[arg(long, value_enum)]
    controller: Option<ControllerArg>,
    /// 使用 Docker 部署（仅 Linux）：Core 与 WebUI 均为容器（--controller webui），或仅 Core 为容器、命令行控制器在主机上（--controller cli）
    #[arg(long, conflicts_with = "native")]
    docker: bool,
    /// 以系统服务方式部署（默认）
    #[arg(long)]
    native: bool,
    #[command(flatten)]
    common: CommonArgs,
    /// 加入管理员组的用户（可重复）
    #[arg(long = "admin-user", value_name = "USER")]
    admin_users: Vec<String>,
    /// 迁移后卸载 3.0.x 桌面控制器软件包
    #[arg(long)]
    remove_legacy_packages: Option<bool>,
}

#[derive(Args)]
struct UpgradeArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args)]
struct CommonArgs {
    /// Core 目标版本（默认最新正式版；预发布版本需指定）
    #[arg(long)]
    core_version: Option<String>,
    /// 控制器目标版本（默认最新正式版；预发布版本需指定）
    #[arg(long)]
    controller_version: Option<String>,
    /// WebUI 初始访问密码文件（至少 6 个字符）
    #[arg(long, env = "MSST_ADMIN_PASSWORD_FILE", value_name = "FILE")]
    admin_password_file: Option<PathBuf>,
    /// WebUI 监听地址（Docker）
    #[arg(long, value_name = "ADDR:PORT")]
    webui_bind: Option<String>,
    /// 覆盖 Core 镜像（Docker；WebUI 部署需同时指定 --webui-image）
    #[arg(long)]
    core_image: Option<String>,
    /// 覆盖 WebUI 镜像（Docker）
    #[arg(long, requires = "core_image")]
    webui_image: Option<String>,
    /// 镜像仓库地址（需要登录时）
    #[arg(long, requires_all = ["registry_user", "registry_password_file"])]
    registry: Option<String>,
    #[arg(long)]
    registry_user: Option<String>,
    #[arg(long, value_name = "FILE")]
    registry_password_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum ReleaseCmd {
    /// 生成签名密钥
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// 为目录中的发布文件生成 manifest.json
    Manifest {
        #[arg(long, value_enum)]
        product: ProductArg,
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        ipc_protocol: Option<String>,
        /// 本发布的容器镜像（默认 abjust/msstnet31x-core|webui:<版本>）
        #[arg(long)]
        image: Option<String>,
        /// 控制器：要求的最低 Core 版本
        #[arg(long)]
        min_core: Option<String>,
        #[arg(long)]
        min_installer: Option<String>,
    },
    /// 签名 manifest.json
    Sign {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// 按安装器的规则验证发布目录
    Verify {
        #[arg(long, value_enum)]
        product: ProductArg,
        #[arg(long)]
        dir: PathBuf,
        #[arg(long = "pubkey")]
        pubkeys: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ProductArg {
    Core,
    Controller,
}

impl From<ProductArg> for Product {
    fn from(p: ProductArg) -> Self {
        match p {
            ProductArg::Core => Product::Core,
            ProductArg::Controller => Product::Controller,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!();
        eprintln!("  {} {:#}", console::style("✗").red().bold(), e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let g = cli.global;
    if let Some(Command::Release(r)) = cli.command {
        return release_cmd(r, g.allow_unsigned);
    }

    let interactive = !g.non_interactive && std::io::stdin().is_terminal();
    let ui = Ui { interactive, assume_yes: g.yes };
    ui.banner();

    let os = Os::current();
    let arch = Arch::current().context("不支持的 CPU 架构（仅支持 x86_64 与 aarch64）")?;
    let read_only = matches!(cli.command, Some(Command::Status)) || g.dry_run;
    if !read_only && !platform::is_elevated() {
        bail!("请以 root（Linux/macOS: sudo）或管理员（Windows）身份运行。");
    }
    let layout = Layout::new(os);
    let state = InstallState::load(&layout)?;

    let source = match (&g.from_dir, g.mirror) {
        (Some(d), _) => Source::Dir(d.clone()),
        (None, Some(Mirror::Github)) => Source::GitHub,
        (None, Some(Mirror::Gitee)) => Source::Gitee,
        (None, None) => state.as_ref().and_then(|s| Source::from_key(&s.source)).unwrap_or(Source::Gitee),
    };
    let mut ctx = Ctx {
        os,
        arch,
        layout,
        ui,
        http: release::http_client()?,
        trust: TrustPolicy::builtin(g.allow_unsigned)?,
        source,
        dry_run: g.dry_run,
        home: platform::service_account_home(os),
    };
    if (ctx.trust.core.is_empty() || ctx.trust.controller.is_empty()) && !g.allow_unsigned && !matches!(cli.command, Some(Command::Status) | Some(Command::Uninstall { .. })) {
        ui.warn("此安装器构建未内置（全部）发布公钥：缺少公钥的组件只能经 --allow-unsigned 明确允许后安装。");
    }
    ui.info(&format!("系统：{} {}", os.display_name(), arch.as_str()));

    match cli.command {
        None if interactive => wizard(&mut ctx, state, g.mirror.is_none() && g.from_dir.is_none()),
        None => bail!("无人值守模式需要子命令（install / upgrade / repair / rollback / uninstall / status）"),
        Some(Command::Install(a)) => install_cmd(&ctx, state, a),
        Some(Command::Upgrade(a)) => {
            let state = state.context("尚未安装 MSST-Net 3.1（3.0.x 迁移请用 install）")?;
            upgrade(&ctx, state, a.common)
        }
        Some(Command::Repair) => repair(&ctx, state),
        Some(Command::Rollback { legacy }) => {
            let state = state.context("尚未安装 MSST-Net 3.1")?;
            rollback(&ctx, state, legacy)
        }
        Some(Command::Uninstall { purge, keep_desktop }) => {
            let state = state.context("尚未安装 MSST-Net 3.1")?;
            uninstall(&ctx, state, purge, keep_desktop)
        }
        Some(Command::Status) => {
            native::status(&ctx, state.as_ref());
            if let Some(s) = state.as_ref().filter(|s| s.deployment == Deployment::Docker) {
                docker::status(&ctx, s);
            }
            Ok(())
        }
        Some(Command::Release(_)) => unreachable!(),
    }
}

fn read_password(path: &Option<PathBuf>) -> Result<Option<String>> {
    let Some(p) = path else { return Ok(None) };
    let pw = std::fs::read_to_string(p).with_context(|| format!("无法读取密码文件 {}", p.display()))?;
    let pw = pw.trim_end_matches(['\r', '\n']).to_string();
    if pw.chars().count() < 6 {
        bail!("WebUI 密码至少 6 个字符");
    }
    Ok(Some(pw))
}

fn docker_options(c: &CommonArgs) -> Result<docker::DockerOptions> {
    let registry = match (&c.registry, &c.registry_user, &c.registry_password_file) {
        (Some(server), Some(user), Some(file)) => Some(docker::RegistryLogin {
            server: server.clone(),
            user: user.clone(),
            password: std::fs::read_to_string(file)
                .with_context(|| format!("无法读取 {}", file.display()))?
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        }),
        _ => None,
    };
    Ok(docker::DockerOptions {
        core_version: c.core_version.clone(),
        controller_version: c.controller_version.clone(),
        controller: None,
        images: c.core_image.clone().map(|core| Images { core, webui: c.webui_image.clone() }),
        webui_password: read_password(&c.admin_password_file)?,
        webui_bind: c.webui_bind.clone(),
        registry,
    })
}

fn install_cmd(ctx: &Ctx, state: Option<InstallState>, a: InstallArgs) -> Result<()> {
    if let Some(s) = &state {
        ctx.ui.info(&format!("已安装 {}；install 将按升级处理", s.version));
    }
    let docker_legacy = state.is_none() && docker::detect_legacy(&ctx.layout.docker_dir());
    let docker = a.docker || state.as_ref().is_some_and(|s| s.deployment == Deployment::Docker) || (docker_legacy && !a.native);
    if docker {
        if a.controller.is_some_and(|c| !matches!(c, ControllerArg::Webui | ControllerArg::Cli)) {
            bail!("Docker 部署的控制器只能是 WebUI 或命令行（--controller webui|cli）");
        }
        let mut opts = docker_options(&a.common)?;
        opts.controller = a.controller.map(Into::into);
        return docker::install(ctx, opts);
    }
    native::install(
        ctx,
        native::InstallOptions {
            controller: a.controller.map(Into::into),
            core_version: a.common.core_version,
            controller_version: a.common.controller_version,
            admin_users: a.admin_users,
            webui_password: read_password(&a.common.admin_password_file)?,
            remove_legacy_packages: a.remove_legacy_packages,
        },
    )
}

fn upgrade(ctx: &Ctx, state: InstallState, c: CommonArgs) -> Result<()> {
    match state.deployment {
        Deployment::Docker => docker::install(ctx, docker_options(&c)?),
        Deployment::Native => native::install(
            ctx,
            native::InstallOptions {
                controller: None,
                core_version: c.core_version,
                controller_version: c.controller_version,
                admin_users: Vec::new(),
                webui_password: read_password(&c.admin_password_file)?,
                remove_legacy_packages: None,
            },
        ),
    }
}

fn repair(ctx: &Ctx, state: Option<InstallState>) -> Result<()> {
    let Some(state) = state else {
        if legacy::detect_native(&ctx.layout).is_some() {
            bail!("检测到 3.0.x 安装；请使用 install 迁移到 3.1。");
        }
        bail!("尚未安装 MSST-Net 3.1，无可修复的内容。");
    };
    match state.deployment {
        Deployment::Docker => docker::repair(ctx, state),
        Deployment::Native => native::repair(ctx, state),
    }
}

fn rollback(ctx: &Ctx, state: InstallState, legacy: bool) -> Result<()> {
    match (state.deployment, legacy) {
        (Deployment::Native, true) => native::rollback_legacy(ctx, state),
        (Deployment::Native, false) => native::rollback(ctx, state),
        (Deployment::Docker, false) => docker::rollback(ctx, state),
        (Deployment::Docker, true) => bail!(
            "Docker 部署还原 3.0.x：备份位于 {}，请用其中的 docker-compose.yml 手动启动。",
            state.legacy_backup.map(|b| b.display().to_string()).unwrap_or_else(|| "（无）".into())
        ),
    }
}

fn uninstall(ctx: &Ctx, state: InstallState, purge: bool, keep_desktop: bool) -> Result<()> {
    match state.deployment {
        Deployment::Docker => docker::uninstall(ctx, state, purge),
        Deployment::Native => native::uninstall(ctx, state, purge, keep_desktop),
    }
}

fn release_cmd(r: ReleaseCmd, allow_unsigned: bool) -> Result<()> {
    match r {
        ReleaseCmd::Keygen { out } => release_tools::keygen(&out),
        ReleaseCmd::Manifest { product, dir, version, ipc_protocol, image, min_core, min_installer } => {
            release_tools::generate(product.into(), &dir, &version, release_tools::Extra { ipc_protocol, image, min_core, min_installer })
        }
        ReleaseCmd::Sign { dir, key } => release_tools::sign(&dir, key.as_deref()),
        ReleaseCmd::Verify { product, dir, pubkeys } => release_tools::verify(product.into(), &dir, &pubkeys, allow_unsigned),
    }
}

// ---------------------------------------------------------------------------
// Interactive wizard
// ---------------------------------------------------------------------------

fn pick_source(ctx: &mut Ctx) -> Result<()> {
    let i = ctx.ui.select("请选择下载源：", &["Gitee（国内镜像，推荐）", "GitHub（官方源）"], 0, "--mirror")?;
    ctx.source = if i == 0 { Source::Gitee } else { Source::GitHub };
    Ok(())
}

/// Target versions (empty answer = latest stable release).
fn pick_versions(ctx: &Ctx, controller: bool) -> Result<CommonArgs> {
    let ask = |what: &str, flag: &str| -> Result<Option<String>> {
        let v = ctx.ui.input(&format!("{what}版本（留空 = 最新正式版）"), "", flag)?;
        Ok(Some(v.trim().to_string()).filter(|v| !v.is_empty()))
    };
    let mut c = CommonArgs::empty();
    c.core_version = ask("Core ", "--core-version")?;
    if controller {
        c.controller_version = ask("控制器", "--controller-version")?;
    }
    Ok(c)
}

fn wizard(ctx: &mut Ctx, state: Option<InstallState>, ask_source: bool) -> Result<()> {
    let ui = ctx.ui;
    if let Some(s) = state {
        ui.info(&format!(
            "已安装 MSST-Net {}（{}，控制器：{}）",
            s.version,
            if s.deployment == Deployment::Docker { "Docker" } else { "原生" },
            s.controller.label()
        ));
        if let Some(p) = &s.pending {
            ui.warn(&format!("上次的操作（{} → {}）未完成，建议执行「修复」。", p.action, p.target));
        }
        let mut items = vec![("升级", 0), ("修复", 1)];
        if s.previous.is_some() {
            items.push(("回滚到上一版本", 2));
        }
        if s.legacy_backup.is_some() && s.deployment == Deployment::Native {
            items.push(("还原到 3.0.x", 3));
        }
        items.extend([("卸载", 4), ("查看状态", 5), ("退出", 6)]);
        let labels: Vec<&str> = items.iter().map(|(l, _)| *l).collect();
        let choice = items[ui.select("请选择操作：", &labels, 0, "")?].1;
        if ask_source && matches!(choice, 0 | 1) {
            pick_source(ctx)?;
        }
        let ctx = &*ctx;
        return match choice {
            0 => {
                let c = pick_versions(ctx, s.controller != ControllerKind::None)?;
                upgrade(ctx, s, c)
            }
            1 => repair(ctx, Some(s)),
            2 => rollback(ctx, s, false),
            3 => rollback(ctx, s, true),
            4 => {
                let purge = ui.select(
                    "卸载方式：",
                    &["保留设备身份与配置（推荐，重新安装后沿用）", "彻底删除（含设备身份）"],
                    0,
                    "--purge",
                )? == 1;
                uninstall(ctx, s, purge, false)
            }
            5 => {
                native::status(ctx, Some(&s));
                if s.deployment == Deployment::Docker {
                    docker::status(ctx, &s);
                }
                Ok(())
            }
            _ => Ok(()),
        };
    }

    // Not installed (3.1): fresh install or migration.
    let legacy_native = legacy::detect_native(&ctx.layout);
    let legacy_docker = docker::detect_legacy(&ctx.layout.docker_dir());
    if let Some(l) = &legacy_native {
        ui.section("检测到 MSST-Net 3.0.x（原生安装）");
        for d in l.describe() {
            ui.info(&d);
        }
        ui.info("迁移会先下载并校验 3.1，再备份 3.0.x 的身份与配置；3.0.x 文件移入备份而不删除，失败自动还原。");
        if !ui.confirm("迁移到 3.1？", true)? {
            return Ok(());
        }
    } else if legacy_docker {
        ui.section("检测到 MSST-Net 3.0.x（Docker 部署）");
        ui.info(&format!("{}：将沿用 ./data 中的设备身份。", ctx.layout.docker_dir().display()));
        if !ui.confirm("迁移到 3.1（Docker）？", true)? {
            return Ok(());
        }
        if ask_source {
            pick_source(ctx)?;
        }
        let c = pick_versions(ctx, true)?;
        return docker::install(ctx, docker_options(&c)?);
    }

    let controller = match ui.select(
        "请选择控制器：",
        &[
            "桌面版（Compose Desktop）— 图形界面应用",
            "WebUI                    — 浏览器访问，适合服务器",
            "命令行（msst-net-cli）    — 终端中使用，适合无图形界面的设备",
            "不安装控制器              — 仅核心",
        ],
        0,
        "--controller",
    )? {
        0 => ControllerKind::Desktop,
        1 => ControllerKind::Webui,
        2 => ControllerKind::Cli,
        _ => ControllerKind::None,
    };
    let use_docker = ctx.os == Os::Linux
        && match controller {
            ControllerKind::Webui => ui.choose_yn("使用 Docker 部署？（Core 与 WebUI 均以容器运行，需已安装 Docker 与 Compose）", false)?,
            ControllerKind::Cli => ui.choose_yn(
                "使用 Docker 部署 Core？（Core 以容器运行，msst-net-cli 装在主机上且需以 root 运行；需已安装 Docker 与 Compose）",
                false,
            )?,
            _ => false,
        };
    if ask_source {
        pick_source(ctx)?;
    }
    let c = pick_versions(ctx, controller != ControllerKind::None)?;
    if use_docker {
        let mut opts = docker_options(&c)?;
        opts.controller = Some(controller);
        return docker::install(ctx, opts);
    }
    native::install(
        ctx,
        native::InstallOptions {
            controller: Some(controller),
            core_version: c.core_version,
            controller_version: c.controller_version,
            admin_users: Vec::new(),
            webui_password: None,
            remove_legacy_packages: None,
        },
    )
}

impl CommonArgs {
    fn empty() -> CommonArgs {
        CommonArgs {
            core_version: None,
            controller_version: None,
            admin_password_file: None,
            webui_bind: None,
            core_image: None,
            webui_image: None,
            registry: None,
            registry_user: None,
            registry_password_file: None,
        }
    }
}

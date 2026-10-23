# MSST-Net 3.1.x 部署契约

本文档定义 3.1.x 的构建产物、组件关系与服务部署方式，是发布流水线（Core、
Controller 仓库）与安装器之间的契约。安装器中的 `manifest.rs`（命名与清单）、
`service.rs`（服务）、`layout.rs`（路径）是本文档的实现；`release` 子命令
（`msst-net-installer release manifest|sign|verify`）供发布流水线使用，保证
生成方与使用方不会各自演化。

## 1. 组件关系

```text
               ┌──────────────── 特权 ────────────────┐
               │ msst-net-core（系统服务，root/SYSTEM）│
               │  数据：身份、core.toml、缓存          │
               └──────────────┬───────────────────────┘
                              │ IPC（Unix socket / 命名管道）
                              │ 角色由 Core 按对端身份判定：
                              │   admin = root/SYSTEM/管理员/msst-net 组成员
                              │   viewer = 其他已登录用户（只读）
          ┌───────────────────┼──────────────────┬───────────────────────┐
          │                   │                  │                       │
 桌面控制器（Compose Desktop） msst-net-webui     msst-net-cli            Android（内置 Core，不经安装器）
 普通用户运行，JNI → IPC       系统服务，用户      普通用户在终端
                              msst-webui         中运行，IPC
                              （主组 msst-net → admin）
```

- Core 是唯一的网络状态来源；控制器只通过 IPC 请求操作（Core `AGENTS.md`）。
- 控制器四选一：桌面版、WebUI、命令行（`msst-net-cli`）、不安装（仅 Core）。
  选“仅 Core”时不安装任何控制器，CLI 也不装。3.0.x 的 Tauri 控制器由
  Compose Desktop 取代。
- 命令行控制器 `msst-net-cli` 由普通用户在终端中运行，经 IPC 连接 Core，不是服务；
  适合没有图形界面、又不需要远程管理的设备。权限与桌面控制器相同。
- WebUI 与命令行控制器可选 Docker 部署（仅 Linux）：
  - WebUI：Core 与 WebUI 均以容器运行，共享 IPC socket 目录（controller `docs/DOCKER.md`）；
  - 命令行：只有 Core 以容器运行，`msst-net-cli` 装在主机上，经 `run/ipc.sock` 连接。
    容器中的 Core 不认识主机的用户与组，只接受 root 的连接，所以要用 `sudo msst-net-cli`。
- Core 与控制器是两个开源仓库，各自发布、各自定版本号。安装器分别从两个仓库
  的 Release 下载，把（Core 版本，控制器版本）作为一个整体安装、保留与回滚
  （`versions/<core>+<controller>/`）。

## 2. 发布产物

### 2.0 发布仓库

| | GitHub | Gitee（同步） | 覆盖用环境变量 |
|---|---|---|---|
| Core | `MSST-Net-Developers/msst-net-core-31x` | `Abjust/msst-net-core-31x` | `MSST_CORE_GITHUB_REPO` / `MSST_CORE_GITEE_REPO` |
| 控制器 | `MSST-Net-Developers/msst-net-controller-31x` | `Abjust/msst-net-controller-31x` | `MSST_CONTROLLER_GITHUB_REPO` / `MSST_CONTROLLER_GITEE_REPO` |

Tag 为 `v<版本>`。每个 Release 只包含本仓库的产物与本仓库的清单。

### 2.1 命名

`<组件>-<os>-<arch><扩展名>`，os ∈ `linux` `windows` `macos`，arch ∈ `x86_64`
`aarch64`（3.0.x 中 `arm64`/`aarch64` 混用，3.1 统一为 `aarch64`）。

| 组件 | 文件 | 来源 |
|---|---|---|
| Core（core 仓库） | `msst-net-core-<os>-<arch>[.exe]` | core 仓库 `cargo build --release --bin msst-net-core` |
| WebUI（controller 仓库） | `msst-net-controller-webui-<os>-<arch>[.exe]` | controller 仓库 `--features webui --bin msst-net-webui`（先构建 wasm 前端） |
| 命令行控制器（controller 仓库） | `msst-net-controller-cli-<os>-<arch>[.exe]` | controller 仓库 `cargo build --release --bin msst-net-cli`（六个平台；Linux 为 musl 静态链接） |
| 桌面控制器（controller 仓库） | `msst-net-controller-desktop-linux-<arch>.deb` / `.rpm` / `.tar.gz` | `packageReleaseDeb` / `packageReleaseRpm` / `createReleaseDistributable` 打包为 tar.gz |
| | `msst-net-controller-desktop-windows-<arch>.msi` | `packageReleaseMsi` |
| | `msst-net-controller-desktop-macos-<arch>.dmg` | `packageReleaseDmg`（**当前 build.gradle.kts 未配置**） |
| Wintun（可选，core 仓库） | `wintun-windows-<arch>.dll` | 未提供时安装器从 wintun.net 下载 0.14.1（固定 SHA-256） |
| 清单（两个仓库各一份） | `manifest.json`、`manifest.json.sig` | `msst-net-installer release manifest/sign` |

控制器仓库的文件统一以 `msst-net-controller-<type>-` 开头（`type` ∈ `webui` `cli`
`desktop` `android`；Android APK 不进清单）。控制器 3.1.8 及更早的 Release 使用旧名称
`msst-net-webui-…` / `msst-net-cli-…`，安装器读取时仍然接受（回滚、`--controller-version`）。
安装后的可执行文件、服务与命令名称不变：`msst-net-webui`、`msst-net-cli`。

不符合命名的文件不会进入清单，安装器也不会使用。3.0.x 的
`msst-net-client-*` 文件名与 3.1 不冲突，3.0.x 安装器遇到 3.1 发布会因找不到
文件而在写入任何内容之前失败。

### 2.2 清单（manifest.json）

```json
{
  "schema": 1,
  "product": "msst-net-controller",
  "version": "3.1.0",
  "ipc_protocol": "0.4",
  "min_core": "3.1.0",
  "min_installer": "3.1.0",
  "artifacts": [
    { "name": "msst-net-controller-webui-linux-x86_64", "component": "webui", "os": "linux",
      "arch": "x86_64", "format": "binary", "size": 9437184, "sha256": "…" }
  ],
  "image": "abjust/msstnet31x-webui:3.1.0@sha256:…"
}
```

- `product`：`msst-net-core` 或 `msst-net-controller`；Core 清单只能列 core/wintun，
  控制器清单只能列 webui/desktop/cli（cli 的 `format` 只能是 `binary`）。用一个仓库的
  签名清单冒充另一个仓库的会被拒绝。
- 安装器不认识的 `component`（以后新增的组件）会被跳过，不会让整份清单失败。
  3.1.4 之前的安装器既不认识 `cli`，也不认识 `msst-net-controller-webui|cli-…` 这样的新名称，
  所以使用新名称或列出 `cli` 的控制器 Release 应把 `min_installer` 设为 `3.1.4` 或更新；
  生成清单所用的安装器也必须是 3.1.4 或更新，否则新名称的文件会被当作不符合命名约定而跳过。
- `min_core`（仅控制器）：所需的最低 Core 版本，不满足时安装器拒绝。
  `ipc_protocol` 两边不同时只给出警告。
- `image`：本发布的容器镜像，可省略；默认 `abjust/msstnet31x-core:<Core 版本>`
  与 `abjust/msstnet31x-webui:<控制器版本>`。建议带 `@sha256:` 摘要固定内容。

- `manifest.json.sig`：对 `manifest.json` 原始字节的 Ed25519 签名（base64）。
- 两个仓库各用一把签名密钥。安装器构建时内置两组公钥：
  `MSST_CORE_RELEASE_PUBKEYS` 与 `MSST_CONTROLLER_RELEASE_PUBKEYS`（逗号分隔，可多个
  以便轮换）。某个仓库的清单只认该仓库的公钥；签名缺失或无效即中止。`--allow-unsigned` 只接受“没有签名”，不接受
  “签名无效”，用于本地测试。
- 每个文件按清单的大小与 SHA-256 校验。
- 安装器只接受 `3.1.x` 版本的清单；`min_installer` 高于自身版本时拒绝。

### 2.3 发布流程（两个仓库的 GitHub Actions 各自执行）

```sh
# core 仓库：产物放进 dist/ 后
msst-net-installer release manifest --product core --dir dist --version 3.1.0 --ipc-protocol 0.4
# controller 仓库
msst-net-installer release manifest --product controller --dir dist --version 3.1.0 \
    --ipc-protocol 0.4 --min-core 3.1.0
# 两边相同：
MSST_RELEASE_SIGNING_KEY=${{ secrets.MSST_RELEASE_SIGNING_KEY }} msst-net-installer release sign --dir dist
msst-net-installer release verify --product <core|controller> --dir dist --pubkey <公钥>
# 上传 dist/* 到本仓库的 Release，并同步到 Gitee；镜像推送到 abjust/msstnet31x-core|webui:<版本>
```

每个仓库只持有自己的私钥（仓库机密 `MSST_RELEASE_SIGNING_KEY`）。离线安装目录的结构为
`<目录>/core/` 与 `<目录>/controller/`，各放一份发布产物及其清单。

GitHub 的 `releases/latest` 不含预发布版本；安装 alpha/beta 需 `--version`。

## 3. 安装布局

| | Linux | macOS | Windows |
|---|---|---|---|
| 程序根目录 | `/opt/msst-net` | `/usr/local/msst-net` | `%ProgramFiles%\MSST-Net` |
| 版本目录 | `<根>/versions/<版本>/` | 同左 | 同左 |
| Core 数据（root/SYSTEM） | 3.0.x 目录 `~/MSST-Net`（有 identity.json 时），否则 `~/.local/share/msst-net` | `~/MSST-Net` 或 `~/Library/Application Support/msst-net` | `systemprofile\MSST-Net` 或 `systemprofile\AppData\Roaming\msst-net` |
| Core 配置 | `~/.config/msst-net/core.toml` | 与数据目录相同 | 与数据目录相同 |
| WebUI 配置 | `/var/lib/msst-net-webui` | `/Library/Application Support/MSST-Net/webui` | `%ProgramData%\MSST-Net\webui`（仅 SYSTEM 与管理员可访问；日志 `webui.log`） |
| `msst-net-cli` | `<根>/versions/<版本>/msst-net-cli` | 同左 | `<根>\versions\<版本>\msst-net-cli.exe` |
| CLI 命令入口 | 符号链接 `/usr/local/bin/msst-net-cli` → 当前版本 | 同左 | `<根>\bin\msst-net-cli.exe`（切换版本时复制），`<根>\bin` 加入系统 `PATH` |
| Docker | `/opt/msst-net-docker`（`data/` `webui/` `run/`；命令行控制器：`cli/<控制器版本>/`） | 不支持 | 不支持 |

原生安装改为 Docker 部署时（3.0.x 原生安装，或已卸载的 3.1 原生安装），设备身份
（Core 实际使用的数据目录）复制到 `data/`，设备保持不变；3.0.x 的程序文件与服务
定义先移入备份，Docker 部署失败时自动还原。3.1 原生 `core.toml` 不自动迁移
（其中可能有只适用于本机的路径），会提示手动写入 `data/core.toml`。

- 升级与回滚时，CLI 命令入口和服务定义一起切换到新版本（§6 第 5 步）。控制器之间
  互相切换时（例如从 CLI 改为 WebUI），原控制器连同它的命令入口一起移除。
- Docker + 命令行控制器：`/usr/local/bin/msst-net-cli` 是安装器管理的脚本，设置
  `MSST_IPC_PATH=/opt/msst-net-docker/run/ipc.sock` 后运行 `cli/<控制器版本>/msst-net-cli`。
  非 root 用户连接不上时，脚本会提示改用 sudo。`docker-compose.yml` 改用只含 Core 的模板。
- `/usr/local/bin/msst-net-cli` 已存在但不是安装器创建的时，安装器在改动任何内容之前拒绝继续。

`~` 为服务账户的主目录（root：`/root`、`/var/root`）。数据路径完全沿用 Core
3.1 自己的解析规则（`config.rs`），安装器不改变它们，只负责备份与保留。

## 4. 服务

| | Linux（systemd） | macOS（launchd） | Windows（SCM） |
|---|---|---|---|
| Core | `msst-net.service`，root，`--foreground` | `net.msst.client`，root | `msst-net`，LocalSystem，自动启动，崩溃后重启 |
| WebUI | `msst-net-webui.service`，用户 `msst-webui`，主组 `msst-net`，沙箱化 | `net.msst.webui`，root | `msst-net-webui`，LocalSystem，依赖 `msst-net`，崩溃后重启 |

- 服务名沿用 3.0.x，迁移后系统中只有一个 Core 服务。
- 服务定义指向 `versions/<版本>/` 下的可执行文件；升级、回滚 = 重写定义 +
  重启。Windows 服务路径带引号（3.0.x 未加引号，存在服务路径劫持风险）。
- 定义文件由安装器管理，修复会重写；本地修改请用 systemd drop-in。
- Windows 上 WebUI 以 LocalSystem 运行：Core 的命名管道只允许 SYSTEM、管理员与
  交互式用户连接，虚拟服务账户会被拒绝。环境变量（配置目录、首次密码文件）
  通过服务注册表项的 `Environment` 值传入；首次启动保存密码后，安装器删除
  密码文件并移除该变量。

## 5. 权限模型

- 管理员组：Unix `msst-net`，Windows `MSST-Net Administrators`。安装器创建该组，
  并把选定的用户（默认是执行 sudo 的用户）加入。Core 每次连接时查询组数据库，
  Linux/macOS 上加入后立即生效；Windows 需重新登录。
- 不放宽 socket 权限，不以 root 运行 WebUI（Linux）。
- 安装器自身必须以 root/管理员运行；`status` 和 `--dry-run` 不需要。

## 6. 事务与数据保护

1. 解析发布、验证清单签名。
2. 全部产物下载到 `<根>/staging/<时间戳>/`，逐一校验大小与 SHA-256，并运行
   `msst-net-core --version` 自检（选择 WebUI / CLI 时，同样检查 `msst-net-webui` /
   `msst-net-cli --version` 的输出是否包含控制器版本号）。**此前不触碰已安装的任何内容。**
3. （3.0.x 迁移）将 Core 数据、配置目录复制到 `<根>/backups/<时间戳>-3.0.x/`，
   停止 3.0.x，将 3.0.x 程序文件、Tauri 启动脚本**移入**备份。
4. 暂存目录原子改名为 `versions/<版本>/`；状态文件标记 `pending`。
5. 重写服务定义、启动、健康检查（服务持续运行 3 秒以上且 IPC 可连接；WebUI
   通过 `msst-net-webui healthcheck`）。CLI 不是服务，只检查 Core；Core 通过后再切换
   CLI 命令入口，失败回滚时入口随上一版本一起恢复。
6. 失败：切回上一版本 / 还原 3.0.x / 移除新服务；成功：提交状态，保留当前与
   上一版本，删除更旧的版本。

状态文件 `installer-state.json` 原子写入；`pending` 未清除说明上次中断，
`repair` 会重新校验文件并重写服务。同一时间只允许一个安装器运行（文件锁）。

## 7. 卸载

总是删除 CLI 命令入口（Linux/macOS 的符号链接或脚本；Windows 的 `<根>\bin` 及其
`PATH` 条目）。默认保留：Core 数据与配置（设备身份）、WebUI 配置、`backups/`、管理员组；
Docker 保留 `data/` 与 `webui/`。重新安装后设备仍是同一台设备。
`--purge` 删除以上全部（交互模式需输入 `purge` 确认）。

## 8. 无人值守

```sh
msst-net-installer install --controller webui --admin-user alice \
    --admin-password-file /root/pw --mirror gitee --yes --non-interactive
msst-net-installer install --controller webui --docker --admin-password-file /root/pw --yes
msst-net-installer install --controller cli --admin-user alice --yes --non-interactive
msst-net-installer install --controller cli --docker --yes   # Core 在容器中，sudo msst-net-cli
msst-net-installer upgrade --core-version 3.1.2 --controller-version 3.1.1 --yes
msst-net-installer repair --yes
msst-net-installer rollback --yes            # 上一版本；--legacy 还原 3.0.x
msst-net-installer uninstall --yes           # --purge 彻底删除
msst-net-installer install --from-dir ./dist # 离线安装（dist/core、dist/controller）
```

无人值守模式不会提示；缺少必要选项时报错并指出对应参数，确认项需要 `--yes`。

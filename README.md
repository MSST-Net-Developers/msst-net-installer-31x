# MSST-Net 3.1.x 安装程序

MSST-Net 3.1.x 的命令行安装程序，支持 Linux、macOS 和 Windows（x86_64 与 aarch64）。它负责安装、升级、修复、回滚和卸载 MSST-Net 客户端：Core 服务以及可选的 WebUI 控制器、桌面控制器或命令行控制器（`msst-net-cli`）。WebUI 可以选择原生安装，也可以用 Docker 部署。选择命令行控制器时也可以用 Docker 部署 Core，这时 `msst-net-cli` 装在主机上，需要以 root 运行（`sudo msst-net-cli`）。

**完整文档和使用教程请访问文档站：<https://msst-net.msst2031.cn>**

## 下载

从 Releases 页面下载对应平台的安装程序：

- GitHub：<https://github.com/MSST-Net-Developers/msst-net-installer-31x/releases>
- Gitee（国内镜像）：<https://gitee.com/Abjust/msst-net-installer-31x/releases>

## 快速开始

```sh
# Linux / macOS（需要 root 权限）
chmod +x msst-net-installer-linux-x86_64
sudo ./msst-net-installer-linux-x86_64
```

在 Windows 上，请用管理员身份运行 `msst-net-installer-windows-x86_64.exe`。

不带参数运行时会进入交互式向导。常用命令：

| 命令 | 作用 |
| --- | --- |
| `install` | 安装（检测到 3.0.x 时会迁移） |
| `upgrade` | 升级到最新或指定版本，失败时自动回滚 |
| `repair` | 校验并替换损坏的文件，重写服务配置 |
| `rollback` | 回滚到上一版本 |
| `uninstall` | 卸载，默认保留设备身份与配置 |
| `status` | 查看安装状态 |

运行 `msst-net-installer --help` 可以查看全部选项。

## 从源码构建

```sh
cargo build --release
```

正式发布版内置了发布签名公钥。构建时通过环境变量 `MSST_CORE_RELEASE_PUBKEYS` 和 `MSST_CONTROLLER_RELEASE_PUBKEYS` 提供公钥；不提供的话，构建出的安装程序只能配合 `--allow-unsigned` 使用。

## 许可证

[MIT](LICENSE)。第三方依赖的许可证见 [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt)。

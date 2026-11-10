# 安装与升级

RDrive 0.8.0 的推荐起点是从源码构建 CLI。原生伴随 App 为可选组件，提供 Finder / File Provider 集成；CLI 文件操作、缓存、同步及 MCP 无需安装它。

## 系统要求

- macOS：当前主要开发和验证平台。原生 App 构建目标为 macOS 13.0 或更新版本，实际兼容性以[验证记录](TESTING.md)中的系统和硬件为准。
- Rust / Cargo 1.90 或更新版本：`Cargo.toml` 声明的最低 Rust 版本为 1.90。
- macOS 命令行开发工具：提供链接器及 Apple SDK。原生组件还需要可用的 Swift 编译器和 Python 3。
- 可访问的 WebDAV 或 SMB 服务器及相应账户；匿名 WebDAV 不需要密码。

产品运行不依赖 Docker、Samba 服务端或 FUSE。Docker 仅用于部分隔离 SMB 测试。未将其他操作系统或所有 NAS 型号作为已验证平台。

可先检查本机工具：

```sh
rustc --version
cargo --version
xcode-select -p
```

没有命令行开发工具时，可运行 `xcode-select --install` 并完成 macOS 安装对话框。安装 Rust 可参考 [Rust 官方安装指南](https://www.rust-lang.org/tools/install)。

## 构建 CLI

```sh
git clone https://github.com/lorian7392/RDrive.git
cd RDrive
cargo build --release --locked
./target/release/rdrive --version
./target/release/rdrive --help
```

`--locked` 要求使用仓库内的 `Cargo.lock`，避免无意改变依赖解析。`vendor/smb` 和 `vendor/smb-msg` 是构建所需的本地补丁源码；不要只复制 `src/`。补丁说明及上游许可证位于各目录的 `RDRIVE-PATCH.md` 和许可证文件中。

首次构建需要下载尚未缓存的依赖。`--offline` 仅适用于已经具备完整依赖缓存的环境，不是首次安装步骤。网络受限时，按你的环境配置 Cargo 源；项目不要求更改系统代理或使用某个镜像。

在仓库中临时使用：

```sh
export PATH="$PWD/target/release:$PATH"
rdrive --help
```

或安装到 Cargo 的用户可执行文件目录：

```sh
cargo install --path . --locked
rdrive --version
```

后者通常安装到 `~/.cargo/bin`，该目录需要在 `PATH` 中。源码树中的 `dist/`、`target/`、`macos/build/` 是本地构建输出，不能假设克隆后存在。项目的开发构建记录不代表存在可下载、已签名或公证的发行包。

## 状态目录与初始配置

默认状态目录为 `~/Library/Application Support/rdrive`，保存连接配置、缓存索引、上传快照、同步基线和恢复记录。可用 `RDRIVE_HOME` 或全局 `--home` 指定专用目录：

```sh
rdrive --home "$HOME/Library/Application Support/rdrive-demo" profile list
```

不要把状态目录放进同步源目录、共享目录或公开仓库。不同状态目录的连接与任务独立；切换目录不会迁移已有任务。相同状态目录的操作会通过锁协调，旧版和新版后台程序不能同时写入它。

CLI 不接受密码参数。连接示例、隐藏输入和钥匙串设置见[快速上手](QUICKSTART.md#认证与连接)。完整目录布局和环境变量见[配置参考](reference/CONFIGURATION.md)。

## 可选：构建 macOS 伴随 App

`macos/` 包含菜单栏伴随 App、replicated File Provider、File Provider 操作界面和 Finder Sync 扩展。构建本身不会安装、注册或启用扩展，也不会替你创建可用的远端连接。

先构建 release CLI，再选择一个专用、当前用户所有且权限为 `0700` 的开发 IPC 目录。以下示例在当前用户的临时目录下创建唯一目录，不复用其他实例：

```sh
RDRIVE_DEV_IPC="$(mktemp -d "${TMPDIR%/}/rdrive-ipc.XXXXXX")"
chmod 700 "$RDRIVE_DEV_IPC"
python3 macos/build.py \
  --cli target/release/rdrive \
  --development-ipc-root "$RDRIVE_DEV_IPC"
macos/build/RDrive.app/Contents/MacOS/RDriveCompanion --diagnose
```

保存这里选择的 IPC 路径；后续桥接服务和 App 必须使用同一路径。不要用用户主目录、整个临时目录或已被其他实例占用的目录。不要在仍使用它时删除该目录。

默认 `--identity -` 生成 **ad-hoc 本地开发构建**，通过显式 IPC 路径进行开发。它不具有生产 App Group 授权，也不等于通过 Gatekeeper、完成公证或获得所有扩展运行许可。macOS 是否接受应用、是否启用扩展和实际 Finder 行为需要分别确认。

生产签名构建接受以下参数；使用你实际获授权的身份、Team 与 App Group：

```sh
python3 macos/build.py \
  --cli target/release/rdrive \
  --identity 'YOUR_SIGNING_IDENTITY' \
  --team-id YOUR_TEAM_ID \
  --app-group YOUR_AUTHORIZED_APP_GROUP
```

脚本的签名验证不执行完整发行流程。正式分发仍需要匹配的授权、适当的签名与公证等 Apple 发行步骤；不要将开发包描述为已完成这些步骤。CLI 与原生扩展分别更新，重新编译 CLI 不会自动替换已安装 App 内的副本。

启用与注册流程见[macOS 集成](MACOS_INTEGRATION.md)，Smart 域和桥接配置见[Smart 使用说明](SMART_SYNCHRONIZATION.md)。其中历史记录的版本、路径和进程状态仅描述当时验证，不应复制为新安装环境的现成配置。

## 升级

1. 查看[变更记录](../CHANGELOG.md)以及目标版本的验证范围。
2. 检查 `queue list`、`smart status`、`smart list DOMAIN` 和 `transfers list`，导出需要额外保护的未上传修改。
3. 停止使用该状态目录的 `watch`、桥接服务、MCP 和伴随 App；已安装用户服务可先运行 `rdrive service stop`。确认没有旧程序继续写入。
4. 保留状态目录和需要的本地部分文件；完成备份后更新源码并用 `cargo build --release --locked` 重建。
5. 如使用原生集成，重新构建并按集成指南切换 App、桥接和通知助手。确认 `--version`、App 内嵌版本、状态目录和 IPC 路径匹配。
6. 先检查只读的连接、任务和域状态，再恢复后台处理。

SQLite 迁移会保留旧 JSON 作历史副本；不要用旧程序继续写入已经升级的状态目录。降级兼容性未作一般保证，直接替换旧可执行文件不能回退数据库和已发生的远端操作。

## 停用与卸载

先确认未上传内容和恢复副本已经处理，再停止相关进程。用户服务可通过 `service uninstall` 移除其服务配置；命令不会替你完成所有原生域或系统缓存的处理。原生域的移除与保留下载数据行为见[macOS 集成](MACOS_INTEGRATION.md)。

删除 CLI、源码目录或开发 App 不等于删除远端数据，也不等于清理队列、File Provider 域或钥匙串条目。状态目录包含恢复所需内容，只有确认不再需要后才由你单独处理；不要将整个状态目录作为“清缓存”步骤删除。

下一步：[快速上手](QUICKSTART.md) · [配置参考](reference/CONFIGURATION.md) · [故障排查](TROUBLESHOOTING.md)

# RDrive

**面向 macOS 的 WebDAV / SMB 文件与同步客户端，使用 Rust 构建。**

RDrive 将远程文件操作、可恢复传输、离线缓存和受版本保护的同步放在一个命令行工具中。可选的 macOS 伴随 App 通过 File Provider 提供 Finder 占位文件、按需下载和离线编辑；需要多个文件一起发布时，可使用托管快照仓库。

当前源码版本为 **0.8.0**，采用 [MIT 许可证](LICENSE)。CLI 可独立运行，无需 FUSE、伴随 App 或 Docker。协议兼容性与原生扩展状态见[测试与验证](docs/TESTING.md)；版本号和开发构建不代表正式签名发行包。

[安装](docs/INSTALLATION.md) · [快速上手](docs/QUICKSTART.md) · [完整功能](docs/FEATURES.md) · [命令参考](docs/reference/CLI.md) · [配置参考](docs/reference/CONFIGURATION.md) · [全部文档](docs/README.md)

2026-11-10 源码验收：Rust、Swift、Python 的生产行覆盖率均为 **100%**，详见[范围、结果和逐文件报告](docs/COVERAGE.md)。

## 可以做什么

- **操作远程文件**：浏览、搜索列表、读写、复制、移动、删除，以及受版本保护的编辑。
- **传输与恢复**：流式大文件、校验过的下载检查点、SMB 上传续传、显式配置的 Nextcloud 分块上传、持久上传队列。
- **控制传输**：进度、JSON 事件、共享限速与最多 64 个并行队列任务。
- **离线使用**：显式缓存、固定保留、LRU 容量管理、清理预览和缓存目录安全迁移。
- **同步目录**：先预览后执行的双向/单向同步、共同基线、冲突检测、显式删除传播和恢复副本。
- **在 Finder 使用**：原生 SMB/WebDAV 网络卷，或另行配置的 Smart File Provider 按需视图。
- **管理 Smart**：离线编辑队列、文件与目录操作、SQLite 索引、本地名称/路径搜索、过滤规则、版本/冲突策略和用户后台服务。
- **保留所选元数据**：Finder 文件标签和允许的用户扩展属性；托管模式将内容和所选元数据随快照发布。
- **发布多文件快照**：托管仓库事务、并发提交保护和不明提交结果恢复。
- **连接 AI 工具**：默认只读的 MCP stdio 服务，以及文件操作、Smart 同步、托管事务三个配套 Skills。

支持范围及限制集中在[功能说明](docs/FEATURES.md)。

## 从源码安装

需要 **Rust 1.90 或更新版本**及 macOS 命令行开发工具。以下命令在仓库根目录运行；使用提交的 `Cargo.lock` 和 `vendor/` 补丁源码。

```sh
git clone https://github.com/lorian7392/RDrive.git
cd RDrive
cargo build --release --locked
./target/release/rdrive --version
./target/release/rdrive --help
```

将编译结果放入当前终端的搜索路径后，即可使用下文的 `rdrive`：

```sh
export PATH="$PWD/target/release:$PATH"
```

可选 App 的构建、签名及升级步骤见[安装指南](docs/INSTALLATION.md)。源码克隆不包含本机开发产物、账户配置或运行服务。

## 第一次连接

用你自己的服务地址和用户名替换示例。以下密码输入方式适用于 macOS 默认的 **zsh 交互终端**，输入不会回显，也不会把密码字面量写入历史记录。

```sh
read -rs 'RDRIVE_PASSWORD?Password: '
printf '\n'
export RDRIVE_PASSWORD

rdrive profile add dav --protocol webdav \
  --url https://dav.example.com/files/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info dav
rdrive ls dav
```

SMB 连接使用共享路径；先重新输入该服务器对应的密码，或改用单独的密码变量：

```sh
rdrive profile add nas --protocol smb \
  --url smb://nas.example.com/share/folder \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info nas
rdrive ls nas
```

连接配置只记录密码环境变量名。也可通过 macOS 钥匙串提供密码，见[认证配置](docs/QUICKSTART.md#认证与连接)。不要将密码放入 URL、命令参数或配置文件。WebDAV 默认要求 HTTPS；SMB 默认要求传输加密。

## 常用操作

远程路径相对于连接 URL，使用 `/`。默认下载、上传、复制和移动均不覆盖已有目标。

```sh
rdrive get dav reports/report.pdf ./report.pdf
rdrive put dav ./notes.txt notes.txt
rdrive get nas large.zip ./large.zip --resume
rdrive put nas ./large.zip uploads/large.zip --resume --queue
rdrive queue run nas
```

准备离线副本，并在断网后读取：

```sh
rdrive cache config --max-bytes 5368709120
rdrive cache fetch dav notes.txt --pin
rdrive cache get dav notes.txt ./offline-notes.txt
```

同步先查看计划；确认后再加 `--apply`：

```sh
mkdir -p ./work
rdrive mkdir dav work
rdrive sync dav ./work work
rdrive sync dav ./work work --apply
```

挂载、覆盖、目录删除、后台服务和冲突恢复均有单独说明。完整操作流程见[快速上手](docs/QUICKSTART.md)，遇到问题先看[故障排查](docs/TROUBLESHOOTING.md)。

## 使用前了解

普通文件操作和目录同步的多个请求不构成跨文件事务；托管仓库提供合作客户端之间的完整快照可见性。覆盖保护依赖服务器正确执行修订条件或 SMB 保护句柄。恢复副本与自定义版本保存在同一服务器，不能代替独立备份。

本地缓存、上传快照及离线编辑内容以明文保存。RDrive 不提供 Cryptomator、公开分享/文件请求、远程 ACL 编辑、完整 macOS 元数据备份或完整 GUI 连接管理。原生网络卷的缓存由 macOS 管理，与 RDrive 的清洁缓存上限分开。

## 文档与参与

- 入门：[安装](docs/INSTALLATION.md)、[快速上手](docs/QUICKSTART.md)、[故障排查](docs/TROUBLESHOOTING.md)。
- 查询：[全部文档](docs/README.md)、[功能说明](docs/FEATURES.md)、[CLI 参数](docs/reference/CLI.md)、[配置与状态](docs/reference/CONFIGURATION.md)。
- 专题：[挂载](docs/MOUNT.md)、[Smart](docs/SMART_SYNCHRONIZATION.md)、[缓存](docs/CACHE_MANAGEMENT.md)、[MCP](docs/MCP.md)、[托管仓库](docs/REPOSITORY.md)。
- 项目：[变更记录](CHANGELOG.md)、[贡献指南](CONTRIBUTING.md)、[安全说明](SECURITY.md)、[测试说明](docs/TESTING.md)。

发现问题时，请提供脱敏后的版本、命令、错误及复现步骤。报告方法见[贡献指南](CONTRIBUTING.md)；包含凭据或未公开漏洞的信息请按[安全说明](SECURITY.md)处理。

# 快速上手

本文从可独立使用的 CLI 开始。需要 Rust 1.90+，先按[安装指南](INSTALLATION.md)完成构建。所有示例在 macOS 默认 zsh 中执行；将 `rdrive` 替换成 `./target/release/rdrive` 也可以。

示例域名、账户和文件是占位内容。每次连接都使用你有权访问的服务器；修改操作请先在自己的测试目录中尝试。完整参数见[CLI 参考](reference/CLI.md)。

## 认证与连接

### 在当前终端输入密码

交互读取并导出环境变量，输入不回显：

```sh
read -rs 'RDRIVE_PASSWORD?Password: '
printf '\n'
export RDRIVE_PASSWORD
```

不要把密码字面量写进命令、URL 或脚本。环境变量存在于当前终端及其子进程中，退出终端后需要重新准备；它不等于已保存到钥匙串，也不会自动进入 LaunchAgent 或另一个 MCP 客户端的环境。

连接配置记录变量名，运行连接命令时才读取其值。使用不同账户时，分别输入对应密码，或为各连接选择不同的密码变量名。

### 添加 WebDAV

```sh
rdrive profile add dav --protocol webdav \
  --url https://dav.example.com/files/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive profile list
rdrive info dav
rdrive ls dav
```

URL 应是账户的 DAV 文件根目录，不是网站登录页面。默认使用 HTTPS 和证书验证。仅在明确接受风险的 HTTP 测试服务上添加 `--allow-http`。当前认证支持 Basic，不支持 OAuth、Digest、客户端证书或自定义 CA 配置。

### 添加 SMB

为 SMB 服务器准备好对应密码后执行：

```sh
rdrive profile add nas --protocol smb \
  --url smb://nas.example.com/share/folder \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info nas
rdrive ls nas
```

`share` 为共享名，后面的 `folder` 是可选子目录。默认要求 SMB 传输加密，服务端不支持时会拒绝连接。`--allow-unencrypted-smb` 仅用于你明确接受的可信兼容性测试，不会自动降级。当前不提供 Kerberos/SSO、SMB1、DFS 或服务发现。

连接名只能使用字母、数字、`-` 和 `_`，同名连接不能直接覆盖。添加连接时可用 `--timeout 300` 将默认 120 秒超时改为 300 秒。若连接需修改，先查看[配置参考](reference/CONFIGURATION.md)中的连接删除限制，不要直接删除状态文件。

### 改用 macOS 钥匙串

在“钥匙串访问”中添加通用密码条目，服务名为 `rdrive:连接名`、账户名与 `--username` 完全相同，密码通过系统界面输入。创建连接时省略 `--password-env`，RDrive 才会查询该条目。例如连接名为 `dav-keychain` 时，服务名就是 `rdrive:dav-keychain`。

已配置 `--password-env` 的连接会优先且明确要求该变量存在；变量缺失不会自动改用钥匙串。无用户名且未指定密码变量的 WebDAV 可以匿名访问。钥匙串读取可能需要系统授权。

原生 SMB 挂载使用 macOS 自己的凭据或终端提示，与 CLI 传输的密码环境变量独立。后台服务也不会替你保存终端密码。

## 浏览和传输

远程路径相对于连接 URL，根目录写作空路径；不接受绝对路径、`..` 或反斜杠。包含空格的路径用引号括起来。

```sh
rdrive ls dav
rdrive ls dav documents --recursive --contains .pdf
rdrive stat dav documents/report.pdf
rdrive get dav documents/report.pdf ./report.pdf
rdrive cat dav notes.txt --max-bytes 1048576
rdrive url dav documents/report.pdf
```

`url` 返回资源地址，不改变权限，也不创建公开或签名分享链接。`cat` 向标准输出写文件字节，适合文本或重定向，避免直接向终端输出未知二进制。

下面流程使用一个新建的测试目录；若目录已存在，可跳过对应 `mkdir`：

```sh
printf 'Hello from RDrive\n' > ./hello.txt
rdrive mkdir dav rdrive-demo
rdrive put dav ./hello.txt rdrive-demo/hello.txt
rdrive cp dav rdrive-demo/hello.txt rdrive-demo/hello-copy.txt
rdrive mv dav rdrive-demo/hello-copy.txt rdrive-demo/hello-renamed.txt
rdrive get dav rdrive-demo/hello.txt ./downloaded-hello.txt
```

下载、上传、复制和移动默认不覆盖已有目标。明确替换时使用 `--overwrite`；内容确认、版本保护和恢复机制仍按操作及协议执行，不等于无条件安全备份。

删除文件和目录分别需要：

```sh
rdrive rm dav rdrive-demo/hello-renamed.txt
rdrive rm dav rdrive-demo --recursive
```

普通 `rm` 不提供通用远端回收站。需要保留内容时先使用下载、自定义版本或 Smart 目录的可恢复逻辑删除流程；不要把所有删除方式视作同一种保证。

具备可靠修订的文件可以通过编辑器修改：

```sh
EDITOR=/usr/bin/vi rdrive edit dav notes.txt
```

`EDITOR` 只能是单个可执行文件，不能包含附加参数。远端变化会停止发布并保留本地结果，详见[冲突恢复](TROUBLESHOOTING.md#同步或编辑发生冲突)。

## 大文件、队列和进度

下载断线后重新执行相同 `get --resume`，会先核验部分数据和远端版本：

```sh
rdrive get dav large.zip ./large.zip --resume
rdrive transfers list
```

SMB 上传支持检查点续传，可直接运行或排队：

```sh
rdrive put nas ./large.zip uploads/large.zip --resume --queue
rdrive queue list
rdrive --jobs 3 --limit 1048576 queue run nas
rdrive status
rdrive history --limit 50
```

上传目标父目录需要预先存在。`--queue` 成功表示独立上传快照已保存，只有服务器确认后任务才标为 `done`。未设置 `--resume` 的任务重试会从头传输。失败及不明覆盖重试需要显式选项，先按[故障排查](TROUBLESHOOTING.md#上传失败或结果不明)核对状态。

`--limit` 单位是字节/秒，为当前 Rust 进程共享的文件载荷限速；`--jobs` 范围为 1–64，默认 4，仅控制队列调度。它们不控制原生网络卷中其他应用的 I/O。

```sh
rdrive --progress auto get nas large.zip ./large.zip --resume
rdrive --json --progress json queue run nas 2>progress.jsonl
```

结果写入 stdout，进度写入 stderr。默认结构化输出为格式化 JSON，`--json` 使用单行 JSON。`--progress off` 关闭进度；终端提示及系统工具诊断有独立输出。

普通 WebDAV 没有通用上传续传。Nextcloud 需要添加连接时明确设置同服务的 v2 上传端点：

```sh
rdrive profile add nextcloud --protocol webdav \
  --url https://cloud.example.com/remote.php/dav/files/demo/ \
  --nextcloud-upload-url https://cloud.example.com/remote.php/dav/uploads/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive put nextcloud ./large.zip large.zip --resume
```

该实现有本地协议测试，真实 Nextcloud 部署兼容性需单独核对。最终发布后会完整读取目标核验 SHA-256，需考虑额外流量。

## 准备离线缓存

```sh
rdrive cache config --max-bytes 5368709120
rdrive cache fetch dav documents/report.pdf --pin
rdrive cache fetch dav documents --recursive --pin
rdrive cache status
rdrive cache list
```

断网后导出已缓存文件，不连接服务器也不需要密码：

```sh
rdrive cache get dav documents/report.pdf ./offline-report.pdf
```

这会核验本地缓存 SHA-256，但不保证它是服务器最新版本。`get` 不会自动固定缓存；`cache fetch` 才是显式准备缓存的入口。

```sh
rdrive cache unpin dav documents --recursive
rdrive cache prune --max-bytes 1073741824
rdrive cache clear
```

`prune` 会执行符合条件的容量回收；`cache clear` 默认仅预览，确认后加 `--apply`。固定、在用和未上传内容不会作为普通清洁缓存删除。缓存目录迁移、系统下载副本与共享容量的关系见[缓存管理](CACHE_MANAGEMENT.md)。

## 同步本地目录

两端根目录必须预先存在。先在测试目录中建立关系并查看计划：

```sh
mkdir -p ./work
rdrive mkdir dav work
rdrive sync dav ./work work
```

审核计划后选择需要的方向：

```sh
rdrive sync dav ./work work --apply
rdrive sync dav ./work work --direction push --apply
rdrive sync dav ./work work --direction pull --apply
```

默认 `both` 为双向同步；`push` 和 `pull` 仍检查基线及冲突，不是无条件镜像。首次同步不会根据“另一端没有”推断删除。两端同一路径无共同基线时，只有核验内容相同且版本稳定才建立关系，否则报告冲突。

需要传播已确认删除时明确加 `--delete`。本地被替换或删除的文件保存在同步根的 `.rdrive-versions/`；远端保护能力不足时会停止。普通同步不递归删除远端空目录，也不保证跨文件原子性。

```sh
rdrive sync dav ./work work --delete
rdrive sync dav ./work work --delete --apply
rdrive watch dav ./work work --apply --interval 30
```

`watch` 在前台轮询，Ctrl-C 停止。未加 `--apply` 时持续预览。单次 `sync`、`put` 或 `queue run` 退出后不会留下隐藏服务。

标签和所选扩展属性需明确加入 `--metadata`。同步完整规则、类型/名称冲突及服务器边界见[功能说明](FEATURES.md)。

## 在 Finder 中使用

原生网络卷直接使用 macOS 的 SMB/WebDAV 文件系统：

```sh
mkdir -m 700 "$HOME/RDriveMount"
rdrive mount nas "$HOME/RDriveMount" --read-only --dry-run
rdrive mount nas "$HOME/RDriveMount" --read-only
rdrive mounts
rdrive unmount "$HOME/RDriveMount"
```

挂载点必须预先存在、为空、属于当前用户且权限为 `0700`。移除 `--read-only` 才请求可写模式；添加 `--browse` 可请求在 Finder 中显示。SMB 需要系统已有凭据或终端密码提示，`--no-prompt` 只尝试已有系统凭据。详见[挂载说明](MOUNT.md)。

需要占位文件、按需下载、Keep Offline 和离线编辑时，另行配置 Smart File Provider。它需要可选 App、系统接受和启用扩展、同一 IPC 路径的桥接以及通知助手；运行 `smart setup` 本身不等于已经出现 Finder 域。按[Smart 指南](SMART_SYNCHRONIZATION.md)完成配置，后台常驻、过滤、版本和冲突策略见[Smart 管理](SMART_OPERATIONS.md)。

## 后续功能

- [托管仓库](REPOSITORY.md)：多个文件与所选元数据通过完整快照发布，已有普通目录不会自动导入。
- [目录索引](DIRECTORY_INDEXING.md)：后台元数据预建、离线名称/路径搜索、扫描深度和刷新策略；搜索不是全文内容搜索。
- [MCP 与 Skills](MCP.md)：默认只读；写入工具需明确启用并限制本地目录。生成配置时指定 `--binary ./target/release/rdrive`，不要假设存在分发产物。
- [配置参考](reference/CONFIGURATION.md)：状态目录、认证、缓存和后台服务的完整设置。
- [故障排查](TROUBLESHOOTING.md)：认证、连接、冲突、上传结果不明及恢复步骤。

当前终端不再需要连接凭据时，可执行 `unset RDRIVE_PASSWORD`；这不删除钥匙串条目或改变已运行子进程的环境。

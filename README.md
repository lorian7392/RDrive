# rdrive

面向 macOS 的 Rust WebDAV / SMB 客户端。0.8.0 提供远程文件操作、原生网络卷挂载、按协议断点续传、离线缓存、受保护同步、持久上传队列、进度与限速、并行队列、文件标签及选定用户扩展属性，以及可一次发布多个文件的托管快照仓库。可选伴随 App 提供 Finder / File Provider 集成，实际验证状态见对应文档。

0.8 新增缓存清理预览/执行，以及自定义缓存目录与安全迁移；保护固定、正在读取和未上传内容，可恢复到默认目录。提供对应 CLI / MCP 入口，见 [缓存清理与目录管理](docs/CACHE_MANAGEMENT.md)。

0.7 补齐 Smart 文件/目录重命名、移动与可恢复删除、全局及连接过滤规则、用户后台服务、自动版本保留和显式冲突决定，以及离线 SQLite 文件名/路径搜索。所有入口提供 CLI 与对应 MCP 控制。使用方式及安全边界见 [Smart 文件操作与管理](docs/SMART_OPERATIONS.md)。

0.6 新增 SQLite 目录索引和连接后的后台元数据预建：首页先发布，后台逐层发现未打开的目录；目录打开按父目录索引查询，并在后台按间隔刷新。可配置扫描深度、关闭预建或请求局部递归刷新。迁移保留旧 JSON，已有上传回执和恢复状态继续保存。详见 [目录索引与增量刷新](docs/DIRECTORY_INDEXING.md)。

0.5 新增 MCP stdio 服务与三个配套 AI 操作 Skills。MCP 使用现有 CLI 的传输和同步逻辑，提供独立的查询、文件、挂载、缓存、Smart 和托管事务工具；默认只开放查询，启用写入和本地路径范围后可执行用户授权的操作。启动、客户端配置生成与技能安装见 [MCP 与 Skills](docs/MCP.md)。

0.4 新增 Smart Synchronization 的离线目录、持久编辑队列、重连上传和递归离线保留，以及命令行缓存与 Finder 下载文件共享的容量上限。Finder 原生状态标记和操作菜单通过 File Provider 提供；独立 FinderSync 扩展的系统限制另有说明。WebDAV 保留对目录空大小和 macOS 系统代理的兼容。

## 构建和启动

从 Git 仓库获取后，按下面步骤从源码构建。缓存、私有运行状态及编译产物不纳入仓库。本机开发目录另有经过验证的 Apple Silicon 可执行文件 `./dist/rdrive`，其校验和见 `dist/SHA256SUMS`。

需要 Rust 1.90 或更新版本、macOS 命令行开发工具。

```sh
cargo build --release --locked
./target/release/rdrive --help
```

下面示例中的 `rdrive` 可以替换为 `./target/release/rdrive`。CLI 可独立使用，无需安装伴随 App、Finder 扩展或 FUSE。`mount` 创建 macOS 原生 SMB/WebDAV 网络卷，普通应用可以访问挂载目录；`get`、`put` 等由 Rust 客户端直接传输。原生卷的缓存由 macOS 管理，与本程序的 `cache`、队列和 `sync` 分别管理。可选 File Provider 扩展通过系统占位文件按需获取内容，需要另外构建、注册和启用。

默认状态目录为 `~/Library/Application Support/rdrive`，其中保存连接配置、缓存、上传快照和同步基线。可用全局 `--home /absolute/path` 或 `RDRIVE_HOME` 指定其他目录。不要把状态目录放在待同步目录内。相同状态目录的操作会串行互斥，防止两个进程同时改写任务记录；`watch` 在两次轮询间释放锁。

## 连接和认证

密码不接受命令行参数，也不写入配置。使用环境变量，或在 macOS 钥匙串中保存服务名为 `rdrive:<连接名>`、账户名为用户名的通用密码。未配置 `--password-env` 时，程序读取该钥匙串条目。无用户名的 WebDAV 可匿名访问。

```sh
# zsh：交互输入，不把密码字面量写进历史记录。
read -s 'RDRIVE_PASSWORD?Password: '
export RDRIVE_PASSWORD

rdrive profile add dav --protocol webdav \
  --url https://example.com/remote.php/dav/files/neo/ \
  --username neo --password-env RDRIVE_PASSWORD

rdrive profile add nas --protocol smb \
  --url smb://nas.example.com/share/folder \
  --username neo --password-env RDRIVE_PASSWORD

rdrive profile list
rdrive info dav
rdrive info nas
```

本机 `localhost` 和字面回环 IP 直连，避免经系统代理发送本机服务凭据。其他目标保留系统及环境变量代理支持；当前底层库不完整实现 macOS 代理例外表和 PAC，需明确绕过的目标可配置 `NO_PROXY`。

WebDAV 默认要求 HTTPS，验证服务器证书；本地测试 HTTP 必须明确 `--allow-http`。初版支持 Basic 认证，不支持 Digest、OAuth、客户端证书或自定义 CA 配置。不要将账号、密码、签名参数放入连接 URL。重定向通常拒绝，仅允许 WebDAV 同一资源补末尾 `/` 的有限重定向。

SMB 使用纯 Rust SMB2/3 客户端，默认要求传输加密。为可信环境中的兼容性测试，可在新增连接时明确使用 `--allow-unencrypted-smb`。当前使用用户名/密码认证，不提供 Kerberos/SSO、SMB1、DFS、域浏览或服务发现。已实现读取的 RH lease、durable-v2 重连和校验后重新打开，以及检查点上传的重连恢复；服务器可以拒绝租约或 durable 请求。这不包含持久 CA 句柄、透明故障转移或服务器断电保证。能力与实际测试见 [SMB 生命周期](docs/SMB_LIFECYCLE.md)。

`--timeout 300` 可在新增连接时调整超时。大文件或慢链路需要相应放宽超时。连接名只能包含字母、数字、`-` 和 `_`。已有连接不允许直接覆盖；删除前必须处理未完成上传、缓存。

## 命令行挂载

```sh
mkdir -m 700 "$HOME/NetworkStorage"
rdrive mount nas "$HOME/NetworkStorage" --dry-run
rdrive mount nas "$HOME/NetworkStorage"
rdrive mounts
rdrive unmount "$HOME/NetworkStorage"
```

挂载点必须预先存在、为空、属于当前用户且权限为 `0700`。`--read-only` 请求真正的只读卷；默认不显示在桌面，`--browse` 可显示。程序检查实际系统挂载表、来源完整路径、所属用户和只读标记后才报告成功。SMB 支持连接 URL 中的子目录，并按连接设置要求会话和共享加密。

原生 SMB 使用 macOS 自己的凭据或终端密码提示，连接的 `password-env` 不会自动提供原生 SMB 密码。`--no-prompt` 仅尝试系统已有凭据。WebDAV 可使用连接凭据，通过已取消文件名的临时文件描述符交给系统程序，密码不进入命令行参数。原生挂载的认证、证书、超时和缓存行为由 macOS 控制，详见 [挂载说明](docs/MOUNT.md)。

## 文件操作

所有远程路径均相对于连接 URL 的根目录，使用 `/`。不接受绝对路径、`..`、反斜杠。包含空格的路径用 shell 引号包围。

```sh
rdrive ls nas
rdrive ls dav documents --recursive --contains .pdf
rdrive stat dav documents/report.pdf
rdrive mkdir dav projects/demo --parents
rdrive get dav documents/report.pdf ./report.pdf
rdrive cat dav notes.txt
rdrive put dav ./report.pdf documents/report.pdf
rdrive cp dav notes.txt notes-copy.txt
rdrive mv dav notes-copy.txt archived-notes.txt
rdrive rm dav archived-notes.txt
rdrive rm dav obsolete-folder --recursive
rdrive url dav documents/report.pdf
```

下载先写同目录临时文件，核对远程版本和长度，再提交。默认不覆盖已有本地文件。上传先保存独立、校验过的快照，再发送；默认只能创建新文件。需要覆盖时明确传 `--overwrite`。`cp`、`mv` 同样默认不覆盖。根目录不可删除，目录删除需要 `--recursive`。SMB 会拒绝能够识别的远端重解析点/符号链接；WebDAV 没有统一的远端符号链接属性，服务器透明解析的链接无法由客户端保证识别。本地同步目录中的符号链接被拒绝。

SMB 文件上传和复制先写同目录的独立临时文件，完成后再用服务器重命名发布；失败可能留下 `.rdrive-upload-<UUID>` 或 `.rdrive-copy-<UUID>`，同步会排除这些内部文件，暂不自动清理。目录复制失败可能留下部分目标目录。

大文件通过流式传输处理；同步和缓存校验会读取文件计算 SHA-256，增加磁盘 I/O。WebDAV 的目录元数据单次最多接受 16 MiB，超出会停止，避免把不完整列表解释成删除。

`url` 只生成资源地址，不改变权限、不生成公开链接，也不生成临时签名链接。普通 WebDAV/SMB 没有统一的公开分享或文件请求接口。

对于具备强 ETag 的 WebDAV 文件，可以安全编辑：

```sh
EDITOR=/usr/bin/vi rdrive edit dav notes.txt
```

`EDITOR` 必须是单个可执行文件，不包含附加参数。保存后只有远程版本仍匹配时才上传；失败时编辑结果保留在上传队列。编辑器失败时会打印保留的本地文件路径。SMB 使用内容 SHA-256、文件身份和服务器拒写/删句柄保护覆盖，旧内容保存在远端恢复副本中。

## 离线缓存

```sh
rdrive cache config --max-bytes 5368709120
rdrive cache status
rdrive cache fetch dav documents/report.pdf --pin
rdrive cache fetch dav documents --recursive --pin
rdrive cache list

# 完全不连接服务器，也不需要密码：
rdrive cache get dav documents/report.pdf ./offline-report.pdf

rdrive cache unpin dav documents --recursive
rdrive cache prune --max-bytes 1073741824
```

容量设置持久保存，单位是字节；`cache config --unlimited` 解除限制。新下载在传输前预留空间，未固定文件按最近使用时间回收，正在读取或固定保留的文件不会被删除。缩小容量而受保护内容无法容纳时，保持原设置并报错。容量合计命令行清洁缓存和本客户端报告的 macOS 下载副本；上传快照、未上传修改及传输临时副本单独保存，不因清洁缓存上限而删除。`cache fetch` 会核对远程版本，复用有效副本；`cache get` 读取已有副本并验证 SHA-256，不保证它是服务器最新版本。缓存是显式命令管理的，不会因普通 `get` 自动固定。

缓存与待上传快照分开保存，缓存清理不会删除上传快照。磁盘内容为明文；私有目录和权限不等于静态加密。此版不实现 Cryptomator，也不兼容它的保险库格式。

## Smart Synchronization

0.4.1 改为按浏览目录逐层建立索引，首页无需等待整棵远端目录树扫描。尚未浏览的目录按需载入；离线保留的目录会继续发现其后代。

```sh
rdrive smart setup cloud-smart dav --display-name "RDrive Smart"
rdrive smart status
rdrive smart pause
rdrive smart resume
rdrive smart list cloud-smart
rdrive smart export cloud-smart <mutation-id> ./recovered.txt
```

Smart 域与原生网络卷挂载分别配置。创建域后，运行桥接服务并注册 File Provider 才会出现 Finder 按需视图；详细命令见 [Smart 使用说明](docs/SMART_SYNCHRONIZATION.md)。后台同步随 `provider serve` 或管理该服务的伴随 App 运行，单次 CLI 退出不会留下隐藏服务。

目录与文件信息存入本地索引，浏览不必等待网络。Finder 打开在线文件时才下载，系统已下载的内容可离线打开；固定文件夹的策略由后代继承并覆盖新发现文件。离线编辑先保存独立副本，重连自动尝试受版本保护的上传；服务器确认后才完成系统回调。冲突保留修改副本，可检查、导出或明确重试，重试仍保留原有版本条件。

## 上传队列及恢复

```sh
rdrive put dav ./large.zip uploads/large.zip --queue
rdrive queue list
rdrive queue run
rdrive queue run dav --retry-failed
rdrive queue cancel <job-id>
rdrive status
rdrive history --limit 50
```

入队成功意味着上传内容的独立副本已落盘，之后修改或删除原文件不会改变队列内容。服务器确认成功后才标为 `done`。重启会把未确认的 `running` 标记为 `interrupted`；`queue run` 重试待处理任务和不覆盖远端的中断任务，失败任务需明确 `--retry-failed`。曾使用 `--overwrite` 的中断/失败任务，还需要 `--retry-ambiguous-overwrite` 才会重试。未使用 `--resume` 的任务从文件开头重传。使用 `--resume` 后，SMB 按已验证字节偏移继续上传，显式配置的 Nextcloud 按已确认的数据块继续上传。

网络中断可能发生在服务器已经提交、客户端尚未收到响应时。默认“只能新建”的重试会报告冲突并保留快照，不会悄悄覆盖。`--overwrite` 明确允许覆盖；状态不明时，显式使用 `--retry-ambiguous-overwrite` 重试可能再次覆盖同一路径，服务器也可能已保存上一次内容。取消队列任务会删除其本地快照；若有上传检查点则保留，供检查远端临时文件和恢复记录，取消不会删除远端恢复副本。

普通 CLI 命令退出后不会自动建立后台同步。`watch` 和 `provider serve` 在前台持续运行；可选伴随 App 可以管理桥接进程，启动与退出行为见原生集成说明。

## 进度、限速与并行队列

```sh
# 每秒 1 MiB 文件载荷，进度写入 stderr。
rdrive --limit 1048576 --progress auto get nas large.zip ./large.zip --resume

rdrive put dav ./a.bin uploads/a.bin --queue
rdrive put dav ./b.bin uploads/b.bin --queue
rdrive --jobs 3 queue run

# 结果仍在 stdout，逐行进度事件单独保存。
rdrive --json --progress json --limit 1048576 --jobs 3 queue run 2>progress.jsonl
rdrive --progress off status
```

`--limit` 接受正整数，单位为字节/秒，由同一 Rust 进程的连接和任务共享。它限制流式文件载荷，也计算重试、前缀核验、内容哈希和发布后校验产生的额外读取；不会把已续传的前缀再计为新流量。统计不含 HTTP/SMB 头、TLS/SMB 加密开销或 TCP 重传。系统原生挂载中应用的 I/O 由 macOS 执行，不受这套限速控制。

`--progress auto` 在终端显示进度条、速度和预计剩余时间，非终端输出逐行 JSON；`json` 始终输出 JSON，`off` 关闭进度。事件包含单任务和汇总计数，速度使用最近两秒的窗口。传输字节增加与服务器确认提交分别记录，失败任务不会被进度条标成提交成功。

`--jobs` 的范围是 1–64，默认 4，用于 `queue run`。不同远端目标可以同时传输；相同连接和路径的任务仍按队列顺序处理。并发任务由一个协调器保存状态记录。该参数不会把 `sync` 或托管事务内部的所有步骤自动并行化。

## 断点续传

```sh
rdrive get dav large.zip ./large.zip --resume
# 断线后重新执行相同命令，从已验证的部分继续。
rdrive get dav large.zip ./large.zip --resume
rdrive get nas large.zip ./large.zip --resume
rdrive put nas ./large.zip uploads/large.zip --resume
rdrive put nas ./large.zip uploads/large.zip --resume --queue
rdrive transfers list
rdrive transfers discard <download-transfer-id>
```

下载检查点定期保存已经落盘的前缀长度与 SHA-256；重启先重新核验前缀，丢弃未写入检查点的尾部，再继续请求剩余字节。远端版本变化、范围响应不正确、已有本地文件在下载期间被改动时均停止，保留部分数据。`--overwrite` 替换本地文件时保留旧副本，其位置在结果的 `previous_local` 中。

SMB 上传重连后验证远端临时文件的身份，并逐字节核对已上传前缀再续写。重新执行相同的 `put --resume` 会复用内容和参数匹配的未完成队列任务；源文件内容改变则建立新任务。`queue run` 也能继续已经标记为续传的任务。

普通 WebDAV 不提供统一的上传续传协议，本程序不会用危险的分段 PUT 猜测服务器能力。Nextcloud 用户可明确配置上传端点：

```sh
rdrive profile add nextcloud --protocol webdav \
  --url https://cloud.example.com/remote.php/dav/files/neo/ \
  --nextcloud-upload-url https://cloud.example.com/remote.php/dav/uploads/neo/ \
  --username neo --password-env RDRIVE_PASSWORD
rdrive put nextcloud ./large.zip large.zip --resume
```

Nextcloud 按分块恢复，只跳过有确认记录、源内容哈希及未变化的强校验标识的数据块；没有可靠校验标识的块会单独重传。发布时使用目标条件保护，发布后完整下载核对 SHA-256，因此会增加一次完整读取。结果不明时先核对目标，不会盲目重发覆盖。服务器可能使过期分块会话失效；真实 Nextcloud 兼容性尚需实测。

下载检查点在状态目录的 `transfers/`，部分数据在目标目录旁私有的 `.rdrive-partial-<UUID>/`。上传检查点在 `spool/<job-id>.upload.json`。检查点、源快照和部分数据都不能被当作普通缓存清理。上传取消后远端临时数据可能保留。

## 同步

两端根目录必须预先存在。默认只输出计划，不执行：

```sh
mkdir -p ./work
rdrive mkdir dav work
rdrive sync dav ./work work
rdrive sync dav ./work work --apply
rdrive sync dav ./work work --direction push --apply
rdrive sync dav ./work work --direction pull --apply
rdrive sync dav ./work work --apply --delete
rdrive watch dav ./work work --apply --interval 30
rdrive sync dav ./work work --metadata --apply
```

- `both` 为默认双向，`push` 只向远端，`pull` 只向本地。不是无条件镜像。
- 首次同步不根据“另一端没有”推断删除。同一路径两端已有文件且没有共同基线时，会下载远端到临时文件并核对 SHA-256；内容相同且两端版本稳定时可建立基线，否则报告冲突。不会只根据大小、时间认定相同。
- 后续按本地内容哈希、远端版本和共同基线比较。两端都改动时报告冲突，整份计划不执行。需要用户明确整理两端后重新同步。
- 删除传播需 `--delete`。不支持原子保护的远端删除会失败并保留旧基线；远端空目录会保留，避免递归删除新出现的内容。SMB 文件删除使用核验内容与身份的受保护句柄；目录仍不执行普通同步递归删除。
- 替换或删除的本地文件会保存在同步根的 `.rdrive-versions/<id>/original`，该内部目录不再同步。自定义远程版本也使用 `.rdrive-versions`，是另一套数据，见下一节。
- 执行中的错误不会回滚已经确认的网络操作；旧基线保留，下一次运行可能报告无共同基线或版本冲突，需要检查输出后处理。
- SMB 支持已有文件的受保护覆盖：核对 SHA-256 和文件身份，持有服务器拒写/删句柄，将旧文件移到恢复副本，再通过不替换已有目标的重命名发布完整新文件。它有短暂的路径空缺，不是跨崩溃的原子替换；发生中断时保留恢复副本、临时文件及收据。未完成收据会阻止同步把路径空缺当作用户删除。WebDAV 覆盖仍要求强 ETag。
- `put --overwrite --version` 首次保存的目标修订会持久保存，重试复用同一修订保护。SMB 和具备强 ETag 的 WebDAV 均可使用。
- SMB 扫描读取文件内容计算 SHA-256，并暂时持有拒写/删句柄；大型目录会增加读取和网络开销，正在被其他客户端写入的文件可能使本轮同步停止。
- 文件/目录类型冲突、大小写或 Unicode 规范化名称冲突、符号链接、未完成/失败的目录扫描都会停止处理。

同步不是跨多个文件的事务。同步过程中外部编辑、服务器改动可能导致中止；已下载的新内容和本地旧内容副本会保留。状态目录的丢失也会丢失共同基线，因此需要重新建立初始关系。

## 文件标签与用户扩展属性

```sh
rdrive metadata capture ./report.pdf >report-metadata.json
rdrive metadata apply ./identical-copy.pdf ./report-metadata.json
rdrive metadata push dav ./report.pdf documents/report.pdf
rdrive metadata pull dav documents/report.pdf ./report.pdf
rdrive sync dav ./work work --metadata
rdrive sync dav ./work work --metadata --apply
```

当前 CLI 选择 Finder 标签 `com.apple.metadata:_kMDItemUserTags`、`com.apple.FinderInfo`、安全的 `com.apple.metadata:*` 和 `user.*`。二进制标签 plist 与其他属性值原样保存为十六进制，标签不转换成有损文本。此范围针对普通文件，目录标签尚不支持。资源叉、ACL、隔离标记、签名及系统安全属性不会复制；未选择或排除的属性会出现在报告中，不能把它当作完整 macOS 备份。

普通 WebDAV/SMB 使用 `.rdrive-metadata/` 中的便携记录，绑定远程相对路径、强修订和内容 SHA-256，不直接改写服务器的原生 xattrs、ACL 或 SMB 附加数据流。推送前验证源和远端内容一致，更新使用修订条件保护；不会仅根据大小附加标签。接收策略未管理的属性会保留。元数据同步有独立基线，两端都改动时保留双方并报告冲突；无可靠修订或内容尚未一致时会停止或延后元数据操作。

本地属性更新先记录原值，发生错误会回滚；macOS 没有跨多个 xattr 的原子系统调用。需要持久恢复位置时使用专用的 `0700` 目录：

```sh
mkdir -m 700 ./metadata-recovery
rdrive metadata apply ./report.pdf ./report-metadata.json \
  --journal ./metadata-recovery/report.json
# 仅在上次更新中断且留下日志时使用：
rdrive metadata recover ./report.pdf ./metadata-recovery/report.json
```

恢复会核对文件身份及内容，拒绝把旧属性恢复到替换后的另一个文件。托管仓库把所选元数据放在文件快照内，与文件树一起发布。普通目录的内容和便携元数据分别确认，跨文件事务请使用托管模式。

## 托管快照与多文件事务

```sh
rdrive mkdir dav managed
rdrive repo init dav managed
rdrive repo begin dav managed
# 从输出复制事务 id，下面用 <TX> 表示。
rdrive repo mkdir <TX> documents
rdrive repo put <TX> ./report.pdf documents/report.pdf
rdrive repo put <TX> ./notes.txt notes.txt
rdrive repo status <TX>
rdrive repo commit <TX>
rdrive repo ls dav managed
rdrive repo get dav documents/report.pdf ./downloaded.pdf --root managed
```

托管模式使用不可变内容对象、完整文件树清单和禁止覆盖的提交槽。一个事务可同时添加、替换、重命名、移动或删除多个逻辑文件；合作客户端看到完整旧快照或完整新快照。文件稳定身份及所选标签/扩展属性随快照发布。它需要专用存储格式，已有普通目录不会自动导入，直接挂载该服务器目录会看到内部存储结构。

进程退出或回执丢失后保留原状态目录，重放同一个 `repo commit <TX>` 可核对已经发布的事务；并发写者不能覆盖另一个写者的提交。`repo abort` 只允许尚未准备发布的事务。保证针对正确提供原子禁止覆盖重命名的服务器及合作客户端，不等于验证过服务器断电持久性。没有垃圾回收、历史压缩或自动合并；旧版本、不可达内容和失败暂存文件会继续占用空间。完整规则见 [托管仓库说明](docs/REPOSITORY.md)。

## SMB 通知与恢复记录

```sh
rdrive notify nas documents --recursive
```

通知来自真实 CHANGE_NOTIFY，包括创建、修改、删除及重命名提示。溢出、断线或取消不能解释成“没有变化”；需要重新扫描，必要时重新订阅。`watch` 同时保留周期性核对。租约、durable-v2、重连及实际 Samba 验证结果见 [SMB 生命周期](docs/SMB_LIFECYCLE.md)。

每次受保护覆盖都会保留远端 `.rdrive-backup-<UUID>` 及 `.rdrive-recovery-<UUID>.json`，成功发布还写入完成标记。它们与上传临时文件均被同步排除，没有自动清理策略。

若进程或网络在两次重命名之间中断，原路径可能暂时不存在。重新执行有检查点的上传会核对身份后恢复；普通同步会对未完成收据停止处理，即使指定 `--delete` 也不会把它当作用户删除。

没有可用上传检查点时，先用 `cat` 查看错误中指明的收据及 `ls`/`stat` 检查目标、恢复副本和新临时文件。可以明确选择用 `mv` 将恢复副本或完整临时文件移回原位置；默认不覆盖新出现的目标。确认目标内容和恢复状态后，再明确删除那一份未完成收据。服务器故障和同权限的其他客户端仍能删除恢复文件；这些副本不是异地备份。

## 自定义远程版本

```sh
rdrive versions save dav report.pdf
rdrive put dav ./report.pdf report.pdf --overwrite --version
rdrive versions list dav report.pdf
rdrive versions restore dav report.pdf <version-id> --overwrite
rdrive versions prune dav report.pdf --keep 10
rdrive versions prune dav report.pdf --keep 10 --apply
```

版本内容保存在同一服务器的 `.rdrive-versions/<路径哈希>/<版本ID>/content`，另有包含 SHA-256 的清单，恢复前核对内容。与 Mountain Duck 的 `.duckversions` 不兼容。它不是离线、异地或不可变备份；删除整个服务器目录仍会一起丢失。仅支持文件版本，经过本地临时快照下载、再上传保存，不依赖服务器 COPY。默认不自动清理；`prune` 默认列计划，`--apply` 才删除旧版本。

版本目录创建、内容传输、清单写入和后续上传不是同一事务；失败可能留下不完整版本目录，列表只显示有有效清单的版本。

## WebDAV 文件锁

```sh
rdrive lock dav report.pdf --seconds 600
# 将上一步返回 token 保存到变量；避免写入 shell 历史中的字面量。
read 'RDRIVE_WEBDAV_LOCK_TOKEN?Lock token: '
export RDRIVE_WEBDAV_LOCK_TOKEN
rdrive put dav ./report.pdf report.pdf --overwrite
rdrive unlock dav report.pdf
unset RDRIVE_WEBDAV_LOCK_TOKEN
```

要求服务器支持 WebDAV LOCK/UNLOCK。程序会向修改请求附带 token。锁期限由服务器决定，客户端不自动续约，不承诺跨多个源/目标的锁组合。普通 SMB 打开句柄和 WebDAV 文件锁不同，此版没有 SMB 锁管理命令。

## 输出和验证

普通结构化命令输出可读 JSON；`--json` 输出单行 JSON，错误写入标准错误并返回非零退出码。`cat` 直接输出文件字节。`watch --json` 每次轮询输出一个 JSON 记录，错误仍写标准错误。交互式原生挂载会显示系统提示；脚本应使用 `--no-prompt` 并准备可用系统凭据，系统工具诊断可能另外写入标准错误。

```sh
cargo test --locked
python3 tests/cli_workflows.py target/debug/rdrive
python3 tests/advanced_workflows.py target/debug/rdrive
```

测试内容及实际运行结果见 [docs/VALIDATION.md](docs/VALIDATION.md)。测试使用临时目录与本机服务器，不读取你的 NAS、云盘或真实文件。

## macOS 原生扩展开发构建

`macos/` 包含伴随 App、Finder Sync 菜单/图标扩展和真正的 replicated File Provider 扩展。构建不会自动安装或启用扩展；编译、实际注册、按需下载及界面验证的状态和操作说明见 [docs/MACOS_INTEGRATION.md](docs/MACOS_INTEGRATION.md)。

```sh
python3 macos/build.py --development-ipc-root /private/tmp/rdrive-native-ipc-501 --cli target/release/rdrive
macos/build/RDrive.app/Contents/MacOS/RDriveCompanion --diagnose
macos/build/RDrive.app/Contents/MacOS/RDriveCompanion --list-domains
```

开发 IPC 路径必须是专用的私有目录；示例 UID 应按实际用户调整。生产 App Group 构建使用授权的开发者签名。菜单控制由伴随 App 提供，扩展只有在 macOS 实际接受并启用后才会工作。

0.4 的 File Provider 使用系统支持的状态装饰与操作菜单；真实 Finder 已显示离线状态图标、标签及操作。独立 FinderSync 的五项兼容菜单已在普通映射测试目录显示，独立角标仍未确认；没有用真实 123 云盘挂载验证该兼容扩展。具体界面证据与开发签名边界见集成说明。

## 功能边界

本版提供上述 CLI 功能、可选 Finder/File Provider 集成、所选文件元数据，以及托管快照事务。原生扩展的构建、系统注册、按需下载和实际验证状态分别记录在 [macOS 集成说明](docs/MACOS_INTEGRATION.md)，不要把编译通过等同已经部署到系统。

尚不提供公开分享/文件请求、Cryptomator 保险库、远程 ACL 编辑、目录标签、完整资源叉/安全属性复制、独立 Spotlight 搜索界面、正式发行的原生扩展安装包或完整 GUI 连接管理。文件原始修改时间也不保证全部保留。普通目录同步不是跨文件事务；托管仓库需要通过合作客户端访问。协议与 NAS 互操作性以实际验证记录为准，客户端的版本条件与恢复日志不能替代服务器正确性。

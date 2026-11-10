# 架构与源码导航

RDrive 将远端协议、本机恢复状态、同步策略和 macOS 集成分别实现。
功能语义见 [FEATURES.md](FEATURES.md)，全部用户设置见 [配置参考](reference/CONFIGURATION.md)。
本文用于定位源码与理解修改时需要维持的保护条件。

## 命令、后端与传输

[`src/main.rs`](../src/main.rs) 定义 Clap 命令树与调度，结构化结果通过 serde 输出。
普通命令打开状态目录、取得私有锁，再连接已有 profile。
`watch` 在两轮间释放锁；索引只读查询走独立本地读取，不等待网络工作者。
MCP、桥接、通知、服务及原生挂载有各自长生命周期或系统调用入口，不能认为所有命令一直持有 Store。

[`src/config.rs`](../src/config.rs) 定义连接/缓存/索引配置、URL 验证和认证查找；
[`src/factory.rs`](../src/factory.rs) 根据 protocol 创建 WebDAV 或 SMB 后端并附共享传输上下文。
[`src/backend.rs`](../src/backend.rs) 的 `Backend` trait 统一列表、读取、写入、重命名、复制、锁及通知。
路径在协议边界再次验证，根是空字符串。默认 trait 方法在能力不足时失败，不无条件降级。

[`src/webdav.rs`](../src/webdav.rs) 实现 HTTP/DAV、强 ETag 条件、有限重定向、范围读、
LOCK/UNLOCK 和显式 Nextcloud 上传状态机。
[`src/smb.rs`](../src/smb.rs) 实现 SMB 协商与加密、身份+哈希修订、受保护句柄、
暂存发布、恢复收据、CHANGE_NOTIFY、读 lease 与 durable-v2/verified reopen。
[`vendor/smb`](../vendor/smb) 与 [`vendor/smb-msg`](../vendor/smb-msg) 保留上游代码及局部补丁说明；
不要把协议消息定义的存在当作上层工作流已支持。

[`src/transfer.rs`](../src/transfer.rs) 提供同进程共享限速、传输计数和 stderr 进度，
区分载荷字节完成与服务器确认提交。后端验证流量也使用该上下文。
[`src/scheduler.rs`](../src/scheduler.rs) 按连接+目标分组调度持久任务，同目标保序，
不同目标受并发上限控制，协调器统一保存状态。

## 内容操作、续传与普通同步

[`src/operations.rs`](../src/operations.rs) 负责验证下载、本地提交、不可变源快照、上传任务及自定义远端版本。
[`src/resume.rs`](../src/resume.rs) 管理持久下载检查点、已落盘前缀哈希、本地旧副本与恢复。
上传续传由后端与队列持久记录共同完成，不能只依赖“已发送字节数”。

[`src/sync.rs`](../src/sync.rs) 对本地树、远端树与共同基线做三方比较，先形成完整计划再执行。
计划遇到冲突不执行；执行阶段错误不回滚已经确认的操作，旧基线保留。
需要保护的替换/删除使用后端条件能力，扫描不完整不能推断删除。
本地旧内容与远端 SMB recovery receipt 负责保存中断后的证据；它们不会把普通同步变成跨文件事务。

三种写入条件是 `CreateNew`、`Match(Entry)`、`Any`。
默认创建使用 `CreateNew`，受保护编辑使用匹配到的具体修订，只有用户明确许可的覆盖才使用 `Any`。
弱索引 token 用于显示和刷新，不能代替强内容修订。
SMB 强 `stat/list` 需要读取正文建立 SHA-256+身份，metadata-only 列举明确保留弱 token。

## 本地状态、缓存与迁移

[`src/state.rs`](../src/state.rs) 维护 `Store`、配置/任务/缓存记录、原子写与进程锁。
状态目录要求当前用户所有、私有 `0700`，内部记录通常 `0600`；不允许通过符号链接替换已管理目录。
重启时未确认的 `running` 任务变为 `interrupted`，不能直接标完成。

[`src/cache.rs`](../src/cache.rs) 管理显式内容缓存、SHA-256、固定、活跃读者、容量预留及联合预算。
[`src/cache_directory.rs`](../src/cache_directory.rs) 将自定义根绑定状态目录和目录身份，
通过复制校验、持久迁移意图、配置提交与旧副本清理完成可恢复迁移。
这两个文件的内容存储不等于 File Provider 的系统存储。

重要路径包括：

- `config.json`：连接与持久策略；`state.json`：普通任务、缓存与历史记录；`client.lock`：Store 排他锁。
- `cache/` 或显式迁移后的外部根：可回收的 Rust 清洁载荷；marker 防止收编其他用户目录。
- `spool/`：上传/事务源快照和上传检查点；`transfers/`：下载恢复记录。
- `tmp/`：操作暂存；`baselines/`：普通内容与元数据同步的共同基线。
- `transactions/`：managed repository 的本机阶段记录。
- `provider-domains.json`、`provider/`：域配置、SQLite、旧 JSON、后台需求、回执和控制状态。
- `service/`：显式安装的监督配置、运行状态、有限日志与默认 IPC 路径。

这些目录用途不能按名称互换。清洁缓存删除不能触及未上传正文，
普通本地文件命令不能通过对象路径绕过账户归属，恢复日志不能作为“缓存垃圾”删除。
明文存储与私有权限不等于磁盘静态加密。

## Smart 域与索引

[`src/provider.rs`](../src/provider.rs) 是本地 File Provider bridge：域管理、请求验证、
内容/元数据回调、在线与离线请求、稳定项目身份、回执及后台调度入口。
它将域绑定到协议、账户、远端根和仓库模式，拒绝旧域静默指向新数据源。
桥接消息需要匹配版本、私有 IPC 与受控 staging 文件，不执行任意本地路径。

[`src/provider/index.rs`](../src/provider/index.rs) 保存 SQLite 项目、变更 anchor、控制和恢复记录，
支持事务迁移、按父目录读取、路径查找及 FTS5/LIKE 名称搜索。
[`src/provider/index_scheduler.rs`](../src/provider/index_scheduler.rs) 管理目录发现与刷新调度。
[`src/provider/namespace.rs`](../src/provider/namespace.rs) 保存普通目录移动/逻辑删除计划与清单，
[`src/provider/resolution.rs`](../src/provider/resolution.rs) 处理持久正文冲突决定。

[`src/smart.rs`](../src/smart.rs) 保存离线修改的独立快照、版本条件、顺序及重试/取消/导出状态。
后台重放只在服务器确认后完成回执，连续编辑的条件只能由前一次本客户端确认结果推进。
已有结果不明的请求不能与后续编辑随意合并。

[`src/filtering.rs`](../src/filtering.rs) 提供相对 glob 选择、临时文件规则和连接覆盖，
[`src/policies.rs`](../src/policies.rs) 提供 Smart 自动版本、保留数量和显式冲突策略。
排除是可见性/选择规则，不授予远端删除许可；自动 replace 仍依赖一次持久决定的修订条件。

## 元数据与托管仓库

[`src/metadata.rs`](../src/metadata.rs) 捕获所选用户 xattr，按内容哈希绑定，应用前记录旧值以便回滚/恢复。
[`src/metadata_remote.rs`](../src/metadata_remote.rs) 将普通 WebDAV/SMB 的属性存为受保护便携记录，
维护独立三方基线。正文和普通侧边属性是分步确认，无法保证同时可见。

[`src/repository.rs`](../src/repository.rs) 实现不可变内容对象、完整文件树、校验父链和不可变提交槽。
本机事务先持久化阶段再发布；重放相同事务可核对已完成提交。
并发提交竞争同一只创建槽位，不覆盖另一写者，不自动合并。
只有合作客户端的逻辑视图获得整树旧/新快照语义；服务器正确性和断电持久性另有边界。

## MCP、后台与原生层

[`src/mcp.rs`](../src/mcp.rs) 提供有界 stdio JSON-RPC，
[`src/mcp/catalog.rs`](../src/mcp/catalog.rs) 定义工具 schema、只读/写分类与 CLI 参数映射。
每项操作由同一可执行文件子进程完成，复用现有恢复条件；默认不暴露写工具。
本地范围验证约束参数，取消/超时终止子进程，不能撤销已经完成的远端提交。

[`src/service.rs`](../src/service.rs) 创建每状态目录独立的 macOS 用户 LaunchAgent，
监督 bridge 和可选通知助手，验证所管理路径与进程身份。
不负责保存密码、系统域注册或结束任意其他进程，卸载保留数据。

[`src/mount.rs`](../src/mount.rs) 调用系统 helper，验证挂载点、来源、所有者和只读标记。
它与 Rust 后端传输分开。原生 SMB 认证使用系统凭据/提示，原生 WebDAV 通过专用临时描述符交付认证。

[`macos/`](../macos) 包含伴随 App、FinderSync、File Provider 和共享桥接/标签转换代码。
系统占位和 eviction 通过原生 API 实现；Rust 负责协议操作与持久恢复。
原生标签 keyed archive 转换与允许的 `#S` 属性范围在 Swift 层另外校验。
编译不自动注册、启用或部署；见 [macOS 集成](MACOS_INTEGRATION.md)。

## 修改与验证入口

协议或恢复行为变化应补充隔离 fixture 或状态机回归；配置字段变化同步本页引用的配置参考。
CLI 文档由帮助树自动生成，不手改帮助块：

```sh
cargo build --locked
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive
python3 tests/test_documentation.py target/debug/rdrive
```

文档检查会独立比较源代码 Subcommand 枚举与实际帮助树，并验证完整配置 JSON 示例覆盖真实结构字段。
测试分层与外部依赖见 [TESTING.md](TESTING.md)，历史报告保留其版本与测试范围。

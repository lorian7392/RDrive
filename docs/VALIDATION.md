# RDrive 0.6.0 验证记录

日期：2026-11-10。新增权威 SQLite 目录索引、后台元数据预建、按目录的增量刷新，以及 CLI / MCP 控制；版本与最终 release SHA-256 记录在下方结构化报告中。

167 项 Rust 检查通过，4 项专用环境检查维持忽略状态。其中新增 9 项 SQLite 存储及迁移测试、17 项目录调度测试；旧有 WebDAV、同步、元数据、缓存、恢复和参数检查继续通过。详见 [CORE_VALIDATION_0.6.0.json](CORE_VALIDATION_0.6.0.json)。

最终发布程序通过 50 组独立工作流：

- 目录索引 10 组：[INDEX_VALIDATION.json](INDEX_VALIDATION.json)。覆盖未浏览目录自动预建、慢请求期间读取本地目录、正文零下载、TTL、局部变更、失败保留、深度及一次性递归刷新、重启与旧 JSON 迁移，以及索引查询计划。
- Smart 15 组：[SMART_VALIDATION_0.6.0.json](SMART_VALIDATION_0.6.0.json)。使用关闭一般预建的配置继续验证按需行为，包含离线编辑、重复回执、连续保存、冲突、配额和递归离线策略。
- MCP 14 组：[MCP_VALIDATION_0.6.0.json](MCP_VALIDATION_0.6.0.json)。运行时提供 62 个工具、24 个只读工具和 5 个资源，新增索引策略持久化与刷新请求完整调用。
- 并发回执及真实进程崩溃 7 组：[PROVIDER_RECEIPTS_VALIDATION.0.6.0.json](PROVIDER_RECEIPTS_VALIDATION.0.6.0.json)。保留独立客户端修改、丢失响应、SIGKILL 后重放与版本归属保护。
- 加密 SMB 4 组：[SMB_PROVIDER_VALIDATION_0.6.0.json](SMB_PROVIDER_VALIDATION_0.6.0.json)。普通和 Smart 各 2 组，验证 SQLite 中的强修订及确认回执、连续正文/属性修改、外部修改后的拒绝覆盖；临时 Samba 容器已清理。

10,100 条本机合成索引、每目录 100 项、30 次采样中，SQLite 子目录查询与项目解码中位数约 0.84 ms，实际目录 RPC 中位数约 12.89 ms。后台远端深目录被阻塞时，根目录 RPC 约 2.20 ms。查询计划明确使用 items_parent_id。保留 JSON 的 Python 全量解析筛选对照约 33.51 ms，仅用于说明测试方法，不作为 Rust 旧版/新版倍数或真实云盘速度声明。

三个配套 Skill 格式检查、Python 脚本编译检查和 Git 差异检查通过。所有新增操作测试使用临时本机服务、状态和文件，未重启或迁移真实账户的后台服务，未进行真实云盘写入；macOS File Provider 激活和实际 Finder 展示未作为 0.6 的再次实测声明。原生构建脚本已更新为 0.6.0/build8，已运行的原生包需要按升级步骤另行切换。

使用及升级见 [目录索引说明](DIRECTORY_INDEXING.md)。旧 JSON 在迁移后仅为历史副本，不能与旧版本后台服务并行写同一状态目录。历史版本证据保留在 [VALIDATION_0.5.0.md](VALIDATION_0.5.0.md) 和更早的版本记录中。

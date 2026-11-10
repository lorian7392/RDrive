# RDrive 文档

RDrive 0.8.0 的文档分为入门、参数参考、专题和验证记录。新用户从 CLI 开始；Finder 按需视图、后台服务和托管仓库分别按需启用。

## 从这里开始

1. [安装与升级](INSTALLATION.md)：工具链、源码构建、可选原生 App、签名边界与状态迁移。
2. [快速上手](QUICKSTART.md)：认证、连接、文件传输、队列、离线缓存、同步和原生网络卷。
3. [完整功能](FEATURES.md)：各模块实现范围、协议区别、保护机制与当前限制。
4. [故障排查与恢复](TROUBLESHOOTING.md)：认证、冲突、检查点、不明提交结果、缓存及 Finder 问题。

## 参数与配置

- [命令行参考](reference/CLI.md)：命令层级、位置参数、开关、默认值与调用示例。
- [MCP 全参数参考](reference/MCP.md)：每项工具的权限、完整 JSON Schema、必填项、默认值与约束。
- [架构与代码导航](ARCHITECTURE.md)：模块职责、数据流和扩展入口。
- [配置参考](reference/CONFIGURATION.md)：连接、认证、全局选项、环境变量、持久策略和状态目录。
- [测试与验证](TESTING.md)：复现命令、环境要求、覆盖率口径、已运行与未运行的边界。
- [生产覆盖率验收](COVERAGE.md)：2026-11-10 的逐语言 100% 结果、逐文件源码哈希与行为检查记录。

`rdrive --help`、`rdrive COMMAND --help` 和 `rdrive mcp tools --allow-write` 可直接查询当前可执行文件的接口。运行多个版本时，先用 `--version` 确认所查版本。

## 按使用场景查阅

- [原生网络卷挂载](MOUNT.md)：SMB/WebDAV 系统卷、挂载点要求、只读、认证和系统行为。
- [Smart Synchronization](SMART_SYNCHRONIZATION.md)：File Provider 占位文件、离线编辑、固定与共享缓存预算。
- [Smart 操作与管理](SMART_OPERATIONS.md)：重命名、移动、可恢复删除、过滤、用户服务、自动版本、冲突决定与搜索。
- [缓存清理与目录管理](CACHE_MANAGEMENT.md)：安全清理、保护内容、自定义清洁载荷目录与迁移恢复。
- [目录索引与增量刷新](DIRECTORY_INDEXING.md)：SQLite、后台元数据预建、深度、刷新和迁移。
- [托管快照仓库](REPOSITORY.md)：专用格式、多文件事务、并发提交、快照读者和崩溃恢复。
- [SMB 生命周期](SMB_LIFECYCLE.md)：CHANGE_NOTIFY、RH lease、durable-v2、受保护覆盖与检查点恢复。
- [macOS 原生集成](MACOS_INTEGRATION.md)：伴随 App、File Provider、Finder Sync、桥接协议、实际系统验证与限制。
- [MCP 与配套 Skills](MCP.md)：stdio 接口、默认只读、写入权限、本地范围、客户端配置与技能安装。

普通挂载、Smart File Provider 和本地目录同步是不同入口。选择方式时先看[功能说明](FEATURES.md)，不要将一种入口的缓存、认证或验证结果套用到另一种。

## 项目维护

- [变更记录](../CHANGELOG.md)：源码版本和有证据支持的历史变化。
- [贡献指南](../CONTRIBUTING.md)：提交问题、开发流程、测试与补丁范围。
- [安全说明](../SECURITY.md)：凭据、本地数据和漏洞报告注意事项。
- [MIT 许可证](../LICENSE)。
- SMB 补丁来源：[smb](../vendor/smb/RDRIVE-PATCH.md)、[smb-msg](../vendor/smb-msg/RDRIVE-PATCH.md)。

## 版本验证记录

[测试说明](TESTING.md)解释当前验证方法；[原有验证入口](VALIDATION.md)和以下版本文件保留当时结果。历史通过计数不能当成当前覆盖率，历史构建/激活也不能当成新克隆已安装。

- [0.8.0](VALIDATION_0.8.0.md)：缓存清理和目录迁移。
- [0.7.0](VALIDATION_0.7.0.md)：Smart 操作、过滤、服务、版本/冲突策略、离线搜索。
- [0.6.0](VALIDATION_0.6.0.md)：SQLite 索引、元数据预建和增量刷新。
- [0.5.0](VALIDATION_0.5.0.md)：MCP 与配套 Skills。
- [0.4.1](VALIDATION_0.4.1.md)：逐目录加载和首次目录显示修复。
- [0.4.0](VALIDATION_0.4.0.md)：Smart 离线编辑、共享预算和原生状态验证。
- [0.3.1](VALIDATION_0.3.1.md)：WebDAV 目录大小与系统代理兼容修复。
- [0.3.0](VALIDATION_0.3.0.md)：元数据、托管事务、进度/并行和原生集成。
- [0.2.0](VALIDATION_0.2.md)：续传、原生挂载、SMB 保护与恢复。

这些文件及下列 JSON 是历史证据，可能引用当时的本机测试产物或路径；它们不是首次安装步骤，也不代表相关产物随源码分发。真实账户的只读验证不扩展为真实账户写入验证。

<details>
<summary>全部结构化验证报告（JSON）</summary>

- [CACHE_DIRECTORY_VALIDATION_0.8.0.json](CACHE_DIRECTORY_VALIDATION_0.8.0.json)
- [CORE_VALIDATION.json](CORE_VALIDATION.json)
- [CORE_VALIDATION_0.3.0.json](CORE_VALIDATION_0.3.0.json)
- [CORE_VALIDATION_0.3.1.json](CORE_VALIDATION_0.3.1.json)
- [CORE_VALIDATION_0.4.0.json](CORE_VALIDATION_0.4.0.json)
- [CORE_VALIDATION_0.5.0.json](CORE_VALIDATION_0.5.0.json)
- [CORE_VALIDATION_0.6.0.json](CORE_VALIDATION_0.6.0.json)
- [CORE_VALIDATION_0.7.0.json](CORE_VALIDATION_0.7.0.json)
- [CORE_VALIDATION_0.8.0.json](CORE_VALIDATION_0.8.0.json)
- [INDEX_VALIDATION.json](INDEX_VALIDATION.json)
- [INDEX_VALIDATION_0.6.0.json](INDEX_VALIDATION_0.6.0.json)
- [INDEX_VALIDATION_0.7.0.json](INDEX_VALIDATION_0.7.0.json)
- [INDEX_VALIDATION_0.8.0.json](INDEX_VALIDATION_0.8.0.json)
- [MCP_VALIDATION.json](MCP_VALIDATION.json)
- [MCP_VALIDATION_0.6.0.json](MCP_VALIDATION_0.6.0.json)
- [MCP_VALIDATION_0.7.0.json](MCP_VALIDATION_0.7.0.json)
- [MCP_VALIDATION_0.8.0.json](MCP_VALIDATION_0.8.0.json)
- [NAMESPACE_VALIDATION.0.7.0.json](NAMESPACE_VALIDATION.0.7.0.json)
- [NATIVE_ACTIVATION_0.8.0.json](NATIVE_ACTIVATION_0.8.0.json)
- [NATIVE_BUILD_VALIDATION_0.7.0.json](NATIVE_BUILD_VALIDATION_0.7.0.json)
- [POLICY_VALIDATION_0.7.0.json](POLICY_VALIDATION_0.7.0.json)
- [PROVIDER_RECEIPTS_VALIDATION.0.6.0.json](PROVIDER_RECEIPTS_VALIDATION.0.6.0.json)
- [PROVIDER_RECEIPTS_VALIDATION.0.7.0.json](PROVIDER_RECEIPTS_VALIDATION.0.7.0.json)
- [PROVIDER_RECEIPTS_VALIDATION.0.8.0.json](PROVIDER_RECEIPTS_VALIDATION.0.8.0.json)
- [PROVIDER_RECEIPTS_VALIDATION.json](PROVIDER_RECEIPTS_VALIDATION.json)
- [SEARCH_VALIDATION_0.7.0.json](SEARCH_VALIDATION_0.7.0.json)
- [SEARCH_VALIDATION_0.8.0.json](SEARCH_VALIDATION_0.8.0.json)
- [SERVICE_VALIDATION_0.7.0.json](SERVICE_VALIDATION_0.7.0.json)
- [SMART_VALIDATION.json](SMART_VALIDATION.json)
- [SMART_VALIDATION_0.6.0.json](SMART_VALIDATION_0.6.0.json)
- [SMART_VALIDATION_0.7.0.json](SMART_VALIDATION_0.7.0.json)
- [SMART_VALIDATION_0.8.0.json](SMART_VALIDATION_0.8.0.json)
- [SMB_PROVIDER_RAW_VALIDATION.0.7.0.json](SMB_PROVIDER_RAW_VALIDATION.0.7.0.json)
- [SMB_PROVIDER_SMART_VALIDATION.0.7.0.json](SMB_PROVIDER_SMART_VALIDATION.0.7.0.json)
- [SMB_PROVIDER_VALIDATION.json](SMB_PROVIDER_VALIDATION.json)
- [SMB_PROVIDER_VALIDATION_0.4.1.json](SMB_PROVIDER_VALIDATION_0.4.1.json)
- [SMB_PROVIDER_VALIDATION_0.6.0.json](SMB_PROVIDER_VALIDATION_0.6.0.json)
- [SMB_VALIDATION.json](SMB_VALIDATION.json)
- [SMB_WORKFLOWS_VALIDATION.0.7.0.json](SMB_WORKFLOWS_VALIDATION.0.7.0.json)

</details>

返回[项目首页](../README.md)。

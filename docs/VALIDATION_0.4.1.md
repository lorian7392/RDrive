# RDrive 0.4.1 验证记录

日期：2026-11-10。修复真实 123pan Smart 视图首次一直为空的问题：此前每轮要求五秒内完成整棵目录树扫描；深目录或慢网络会反复取消，无法发布首页索引。现在按浏览需求逐目录加载并原子合并，working set 不触发递归，已固定目录逐层发现后代。

135 项 Rust 测试通过：93 项库测试、1 项真实 HTTP 缓存测试、3 项挂载 API、8 项 Smart 日志测试、30 项 WebDAV 协议测试。4 项环境相关测试保持单独运行设置。

最终 release 通过 15 组 Smart 工作流与 7 组并发/崩溃回执工作流。新增慢深目录首先在旧代码复现失败，再验证首页在五秒内显示；还覆盖按需子目录、无关目录保留、失败扫描、父目录确认删除、固定目录递归，以及未完成原子临时文件的忽略。原有离线编辑、重启、重连、冲突、容量上限及回执保护全部保留。

Smart 模式另经加密 Samba 4.21.9 两组验证，覆盖逐目录列表、创建后连续内容/属性修改、弱列表与强覆盖保护，以及外部客户端修改后的冲突拒绝。结果见 [SMB_PROVIDER_VALIDATION_0.4.1.json](SMB_PROVIDER_VALIDATION_0.4.1.json)。

真实 123pan 已重新挂载：原生 WebDAV 卷目录可读；新版 Smart 根目录发布 23 个项目，Finder 中可见 Online only、未下载云图标、子目录以及五项自定义菜单。缓存上限为 5 GiB，挂载验证时清洁文件载荷为零。未执行真实云盘上传、重命名或删除测试。实际连接及凭据运行状态只保留在私有 .runtime/，不进入开发包；后台与通知助手按用户要求保持运行。

原生包为 0.4.1/build7，重新嵌入最终 release 并验证严格签名。原生 Swift 行为未改变，0.4 的 13 组宿主与 11 组完整 File Provider 流程作为历史验证保留，详见 [0.4.0 记录](VALIDATION_0.4.0.md) 与 [原生说明](MACOS_INTEGRATION.md)。0.4.1 本次额外核对真实 Finder 展示、文件系统目录、活动进程和缓存配额。

结构化结果见 [CORE_VALIDATION.json](CORE_VALIDATION.json)、[SMART_VALIDATION.json](SMART_VALIDATION.json)、[PROVIDER_RECEIPTS_VALIDATION.json](PROVIDER_RECEIPTS_VALIDATION.json)。开发签名、清洁缓存容量范围、普通网络卷与 Smart 视图差异等边界继续适用。

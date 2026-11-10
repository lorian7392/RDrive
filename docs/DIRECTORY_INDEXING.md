# 目录索引与增量刷新

0.6 将 Smart / File Provider 的目录与变更记录迁入 SQLite。后台服务连上 WebDAV 或 SMB 后，先列出并发布根目录，再按层扫描其他目录。默认逐步覆盖完整目录树，打开文件夹会提高该目录的优先级；目录扫描只读取元数据，不会把文件下载为离线副本。

## 查询与刷新

Finder 的目录、项目和变化查询直接读取 SQLite。普通目录按 `parent_id` 查询其直接子项，不再为每次打开而反序列化整棵 JSON；单个项目按主键或路径查询，变化按持久 anchor 查询。网络列表、标签补充和上传重放在后台执行，读取已索引目录不获取网络工作者的状态锁。

默认后台目录刷新间隔为 300 秒，已浏览或固定离线目录为 30 秒。每个目录有独立的失败退避；当前浏览、待建索引和旧目录刷新公平调度。正在处理的单次网络请求仍需要完成或超时，因此慢目录可能推迟后续后台任务，但不阻塞已有 SQLite 目录读取。后台复用同一账户配置的连接，连接安全设置发生变化时重新创建客户端。

这里的增量刷新是按目录列举、比较和发布：只更新改变的 SQLite 行，向 Finder 提交新增、变更和删除事件。普通 WebDAV 仍需要一次完整的该目录列表，不要求服务端实现全盘变化日志。未扫描、失败或不完整的列表不会被当成目录已清空。写入和覆盖仍使用当前服务器版本保护，本地显示索引不替代写入校验。

## 操作命令

下面的 `cloud-smart` 是已有 Smart 域 ID，可从 `provider domains` 查看。

```sh
rdrive index status
rdrive index status cloud-smart

# 默认策略：后台逐步预建整棵目录树。
rdrive index config --enable --all

# 根目录深度为 0；扫描到深度 2 的目录，其直接子项也会显示。
rdrive index config --enable --depth 2
rdrive index config --refresh-seconds 300 --active-refresh-seconds 30

# 关闭一般预建；根目录、浏览需求和离线固定目录仍可刷新。
rdrive index config --disable

# 一次性请求刷新；命令只入队，provider serve 在后台执行。
rdrive index refresh cloud-smart documents
rdrive index refresh cloud-smart documents --recursive
```

递归刷新也可超出一般预建的深度限制。完成当前已知子树的扫描后，其一次性请求会被持久标记为完成；再次执行命令会创建新的请求。普通浏览不会覆盖未完成的手动刷新。目标路径相对于域根目录，需要先取得该目录自身的索引；尚未建立时，请先刷新最近的已知父目录，根目录使用空路径或省略路径。

`index status` 离线显示策略及各域的项目、目录、已扫描目录和待扫描数量。`pending` 是已发现但尚未成功扫描的目录数量，受深度限制、禁用预建或访问失败影响，不等于正在执行的任务数。命令显示 `queued` 仅代表请求已保存；需要保持 `provider serve` 运行才能继续扫描。单独执行 `info` 或建立普通原生网络卷不会启动该后台服务。

## 存储与升级

每个域的数据库保存在 `RDRIVE_HOME/provider/<域哈希>.sqlite3`，域绑定包含连接身份、服务器根和仓库模式。数据库使用本地 WAL 和事务，项目、变化 anchor、扫描记录、上传回执与恢复控制状态一起提交；失败事务不会发布半个目录。数据库及其辅助文件属于私有状态，不应提交到 Git 或放入同步目录。

首次使用会在私有锁保护下原子迁移原有 `<域哈希>.json`，保留其原始内容。SQLite 建立后为唯一权威来源，旧 JSON 是迁移前的历史副本。损坏、未知格式或账户绑定不匹配会报错并保留数据，不会静默重新导入旧 JSON、清空任务或重建写入回执。备份正在使用的数据库应使用 SQLite 备份方式，不能只复制主文件而遗漏 WAL。

升级时先停止使用同一状态目录的旧版后台服务，再使用新版启动；不要让旧版继续写 JSON，或将已迁移的状态目录交给旧版使用。现有 File Provider 桥接消息格式不变；新构建的伴随 App 可嵌入新版 CLI。此优化针对 Smart 视图，普通 `rdrive mount` 的目录缓存仍由 macOS 管理。

## 验证

```sh
cargo test --locked
python3 tests/index_workflows.py ./target/release/rdrive
```

验证使用临时状态和本机合成 WebDAV，包括自动预建、正文零下载、慢扫描时读取目录、增量变化、失败保留、深度配置、递归刷新、迁移恢复与索引查询计划。性能数据的样本和计时方法随验证报告记录；本机目录查询耗时不代表真实云盘首次列举速度。

SQLite 的读写隔离与事务行为见[官方 WAL 说明](https://sqlite.org/wal.html)及[事务说明](https://sqlite.org/lang_transaction.html)。

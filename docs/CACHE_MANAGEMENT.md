# 清理缓存与缓存目录（0.8）

RDrive 有两类清洁内容：Rust 客户端缓存，以及 macOS File Provider 管理的 Finder 下载副本。两者计入已有的联合容量预算。以下 `cache clear` 和 `cache directory` 管理 Rust 缓存；不会移动系统管理的 Finder 存储，不会迁移或删除待上传快照、离线修改、目录索引、连接配置和版本历史。

## 清理缓存

```sh
# 只预览，不移除文件。
rdrive cache clear

# 清理可回收的内容。
rdrive cache clear --apply
rdrive cache status
```

清理只处理状态库记录的缓存对象。固定离线的文件、正在通过 RDrive 读取的文件和未记录文件保持原样。即使一个文件的名称长得像缓存 UUID，也不会因此被删除。结果给出候选文件及字节数、实际清理数量，以及跳过的固定/在用对象。

需要回收固定内容时，先明确解除对应文件的固定，再清理：

```sh
rdrive cache unpin dav documents --recursive
rdrive cache clear --apply
```

这不删除远端文件。缓存中的唯一离线副本被回收后，下一次访问需要重新下载。Finder 内的已下载文件由系统保护；可使用 Finder 的 Allow Automatic Removal / Remove Download，或已有的系统回收入口：

```sh
# 请求系统回收可移除的 Finder 下载副本，固定或未上传内容仍受保护。
rdrive provider native -- --trim-cache 0
```

应对当前已注册的伴随 App 执行该命令；使用其他包时显式指定 `--app /absolute/path/RDrive.app`。系统可能拒绝回收正在使用、已固定或修改未上传的文件，命令返回结果才是本次实际回收情况。不要手工删除 `~/Library/CloudStorage` 内的文件来模拟缓存清理。

## 修改缓存路径

```sh
# 显示当前路径。
rdrive cache directory

# 新路径的父目录应已存在；新目录可以不存在，或是空的 0700 私有目录。
rdrive cache directory /absolute/path/RDriveCache --dry-run
rdrive cache directory /absolute/path/RDriveCache

# 将现有内容迁回默认状态目录下的 cache。
rdrive cache directory --default --dry-run
rdrive cache directory --default
```

相对路径、符号链接、非私有目录、非空目标，以及与状态目录、已知同步目录或系统 File Provider 存储重叠的位置会被拒绝。不同状态目录不能使用同一个缓存根。每个自定义根带有绑定到状态目录和目录身份的私有标记，不会把用户已有文件夹直接收编为缓存。

迁移包含固定内容，保持文件版本、哈希、固定策略和连接归属。开始前检查活跃下载/读取；有活跃使用时保持原设置，待任务结束后重试。迁移先复制并逐个核验 SHA-256，完成后一次保存新配置，最后清理旧副本。因此跨磁盘也可迁移，但目标盘需要容纳全部缓存的空间；中途不能依赖移动操作立即腾出源空间。

迁移计划持久保存在状态目录。进程在配置提交前退出时，下一次启动清理此次未完成的复制并继续使用旧路径；配置已经提交则验证新副本后完成旧副本清理。出现损坏、非预期文件或身份改变时保留数据并停止，不能通过删除迁移计划来“修复”。目录索引、上传队列与原生 Finder 存储的位置保持独立。

源目录出现未记录文件时，迁移会停止并保留所有内容。先自行整理这些文件；清理缓存不会删除它们。恢复默认路径同样需要目标没有非预期内容。

## MCP 与 Skills

- `rdrive_cache_directory`：只读查询当前路径。
- `rdrive_cache_set_directory`：传入 `directory`，或 `default: true` 恢复默认；可加 `dry_run: true`。
- `rdrive_cache_clear`：默认预览，`apply: true` 才执行。

写入工具需要 MCP `--allow-write`；新目录需要位于 `--local-root` 授权范围。普通上传、下载和导出入口禁止访问缓存根及迁移涉及的目录，即使它们处于一个更大的授权本地目录中。使用 `cache get` / 对应工具按连接和远端路径读取自己的缓存；不要从内部对象名绕过连接归属。

旧版本后台程序不能识别新的缓存路径配置。启用此功能前，将使用同一状态目录的桥接服务升级到 0.8；保留原状态，正常停止旧进程后再启动新版。`cache directory` 不设置 Finder 系统下载目录，也不是把整个 RDrive 状态目录迁到另一块磁盘。

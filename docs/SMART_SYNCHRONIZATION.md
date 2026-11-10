# Smart Synchronization

RDrive 0.4.1 的 Smart 模式使用 macOS File Provider 管理占位文件、已下载内容和离线修改，通过 Rust 桥接服务连接 WebDAV 或 SMB。普通 `rdrive mount` 使用系统网络文件系统，不会自动转换成 Smart 模式，也不受 RDrive 清洁缓存上限管理。

## 启用

先准备连接，再创建一个独立域。创建 Smart 域时，当前没有数值上限（包括此前选择 `--unlimited`）会设为 5 GiB；已有数值上限保持不变。如果需要不限容量，在创建域后再次运行 `cache config --unlimited`。

```sh
rdrive smart setup cloud-smart dav --display-name "RDrive Smart"
rdrive cache config --max-bytes 5368709120
rdrive cache status
```

开发包的 IPC 目录在构建时明确指定。使用同一路径启动桥接服务；凭据由连接设置、环境变量或钥匙串提供，不写入命令参数。

```sh
rdrive provider serve --socket /private/tmp/rdrive-native-ipc-501/bridge.sock \
  --staging /private/tmp/rdrive-native-ipc-501/Transfers --development-tcp
```

另一个终端注册原生域，并保持命令行通知助手运行：

```sh
rdrive provider native -- --register-domain cloud-smart --display-name "RDrive Smart"
rdrive provider native -- --domain-root cloud-smart
rdrive provider native -- --watch-domains
```

`--watch-domains` 在前台通知 Finder 刷新，使用它无需启动菜单栏 App；保持桥接服务与通知助手运行，远端新增文件及固定文件夹的新子文件才能持续出现。Ctrl-C 停止助手后，已有下载副本仍可使用，新的变化可能延后显示。运行完整伴随 App 时由 App 承担这项通知工作。

`provider native` 的参数直接传给伴随程序；部分 shell/CLI 版本可省略上面的 `--`。伴随 App 也能管理服务，但其默认状态目录必须与这里的 RDRIVE_HOME 相同。同一 IPC 目录只允许一个服务。退出或停止桥接服务会停止后台上传。开发构建必须经过系统接受并启用；生产分发仍需开发者签名与公证。

托管仓库模式需要提前初始化仓库，再给 `smart setup` 添加 `--repository`，普通服务目录不能直接作为托管仓库读取。

## 日常行为

- 目录按浏览需求逐层枚举并保存到本地索引，首页不会等待整棵远端目录树。未浏览的目录不会预先递归扫描；固定离线目录会逐层发现后代。离线可浏览已经枚举的目录、文件大小和状态；从未取得的远端目录内容无法凭空列出。
- 文件内容按需下载。系统下载副本可离线打开；未下载文件仍需要网络。RDrive 不用空的普通文件冒充占位文件。
- Keep Offline 对文件夹及其后代生效，新发现的后代继承策略。取消父文件夹的固定不会取消对子项目单独设置的固定策略。
- 普通下载内容可在空间不足时回收，固定保留、正在读取或未上传修改受到保护。允许自动回收是解除固定策略，不会承诺立即清空内容。
- 本地修改先保存独立、校验过的不可变快照。桥接服务自动尝试上传；进程中断后可重新载入。仅在服务器确认完成后返回成功回执，系统还会保留未成功上传的本地修改。
- 并发远端修改触发冲突，保留本地快照。不会通过重试自动强制覆盖远端。可导出后另存一个远端文件，或者根据业务需要人工合并。

```sh
rdrive smart status
rdrive smart list cloud-smart
rdrive smart pause
rdrive smart resume
rdrive smart retry cloud-smart <mutation-id>
rdrive smart export cloud-smart <mutation-id> ./recovered.txt
```

`smart run` 可执行一次重连同步；持续自动同步由 `provider serve` 提供。暂停只停止后台发布，不撤销已经发生的远端操作。取消任务保留本地快照；导出不会修改远端文件。

多次从未开始上传、基于相同版本的修改可以在覆盖旧修改内容的前提下合并；已开始或状态不明的修改保留各自快照，避免丢失不同版本。连续本地保存按顺序发布；后续修改只能依据前一次已经确认的本客户端写入回执重新建立版本条件，任何外部修改都会停止这条重放链。新建文件反复保存同样在确认前保留每次副本，并在首次创建确认后转为受保护修改。已确认上传的传输副本自动回收，回执、哈希和版本条件继续保留，系统重试仍可复用回执。被取消或被替代但未确认的副本作为恢复数据保留，可用 `smart discard` 明确清理。回执按内容、元数据和原始版本识别，系统更换请求 UUID 不会重复已经确认的上传。

## 缓存容量

```sh
rdrive cache config --max-bytes 10737418240
rdrive cache status
rdrive cache prune --max-bytes 5368709120
rdrive provider native -- --trim-cache 5368709120
rdrive cache config --max-bytes 5368709120
```

上限合计命令行清洁缓存、File Provider 清洁下载副本和已批准的下载预留。下载开始前预留容量；Rust 缓存按 LRU 回收，原生文件调用系统 eviction 接口。不会通过截断或删除 Finder 文件来模拟回收。

降低上限不能容纳受保护内容时会失败并保留原配置。命令行缓存更新固定文件时，会在新版本校验完成前保留旧副本，因此可能需要额外余量；正在读取的旧版本也会继续受到保护。先取消不需要的离线固定，再清理两类缓存；系统仍可能因为正在使用或未上传修改而拒绝回收。`cache config --unlimited` 明确解除限制。

此设置约束清洁载荷和预留，不是磁盘分区配额或预分配空间。数据库、传输暂存副本、系统开销、保留的修改与恢复副本额外占用磁盘。空间不足会停止接收新的修改，已有修改不会为了满足缓存上限被丢弃。

原生用量来自系统物化集合和实际文件状态；每个域单独报告。最后一次已用空间保守保留，正在下载的预留需要心跳续期。移除域应先清空其系统下载副本并报告用量，避免遗留用量被误当成可用空间。

## 验证范围

自动化结果见 [SMART_VALIDATION.json](SMART_VALIDATION.json)，真实 Finder / File Provider 结果见 [MACOS_INTEGRATION.md](MACOS_INTEGRATION.md)。并发回执及删除崩溃恢复另见 [PROVIDER_RECEIPTS_VALIDATION.json](PROVIDER_RECEIPTS_VALIDATION.json)。这些流程使用本机合成服务，不更改真实云盘内容。

普通 WebDAV/SMB 中，文件内容和独立属性记录分步提交，无法保证二者同时对其他客户端可见。需要多个文件、文件内容与元数据一起原子发布时，使用托管仓库模式；服务器确认和版本条件仍依赖服务端正确实现。

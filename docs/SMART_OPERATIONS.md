# Smart 文件操作、规则、版本与后台服务（0.7）

本页针对 Smart / File Provider 域。普通 `mount` 网络卷使用 macOS 的文件系统行为；普通 `sync` 仍采用已有的保守同步规则。以下新策略不会改变显式 `get`、`put` 或普通同步的含义。

## 文件与目录操作

Finder 的 Smart 域支持新建文件/目录、修改正文和标签，以及普通 WebDAV / SMB 文件及目录的重命名、移动、删除。已有项目标识保持稳定，目录移动会更新已知后代路径及离线固定策略。正文、名称、父目录和标签同时改变时，桥接服务保留各步回执，再确认最终结果；普通远端目录没有跨多个协议请求的原子事务。

移动默认不覆盖目标。WebDAV 使用源资源的强版本条件和 `Overwrite: F`；没有可保护的版本时停止。SMB 通过打开的源句柄核验身份及内容，并请求服务器拒绝并发写入/删除，再使用不覆盖重命名发布。服务器不支持这些保护时不会降级为无条件覆盖。

目录删除使用可恢复的逻辑删除：持久保存计划与目录清单，将源目录受保护地移动到同目录下唯一的 `.rdrive-trash-<UUID>`，核验整棵树后才确认。发现额外变化时尝试无覆盖恢复，否则保留恢复路径并报告冲突。文件删除使用协议支持的受保护 DELETE；目录删除保留恢复位置。恢复备份不会出现在 Smart 列表里。

```sh
rdrive smart recovery cloud-smart
```

恢复清单包含原路径、备份路径、阶段及 `space_reclaimed: false`。备份实际保留在服务器，**不释放服务器空间**，本版不自动物理清空。不要使用普通同步删除内部恢复目录。离线重命名、移动或删除显示待同步状态；保留本地原始记录直到远端结果确认。冲突时可取消操作或导出未上传内容。

## 文件过滤

```sh
# 改动指定的规则列表；其他列表保持不变。参数需要 shell 引号。
rdrive filter config --exclude 'private/' --exclude '*.bak'
rdrive filter config --include '*.txt' --include 'docs/**/*.md'
rdrive filter config --profile dav --exclude 'archive/**'
rdrive filter status --profile dav
rdrive filter test dav documents/report.txt
rdrive filter test dav archive --directory
rdrive filter config --clear-include --clear-exclude
rdrive filter reset dav
```

无 `/` 的 glob 匹配任意层的文件名；含 `/` 的规则从域根匹配，`*` 不跨 `/`，`**` 可跨层。末尾 `/` 匹配目录，排除目录会同时排除后代。排除优先于包含。目录仍可遍历，以找到包含规则选中的文件。规则区分大小写。各连接可分别覆盖全局的包含列表、排除列表和临时文件开关，`reset` 恢复继承。

默认隐藏 `.DS_Store`、AppleDouble `._*`、`*.tmp`、`*.swp`、`*.swo`、`*.part`、`~$*` 和 `.~lock.*`。可用 `--include-temporary` 关闭，或 `--exclude-temporary` 恢复。内部版本、暂存及恢复目录始终按内部规则保护。

规则控制 Smart 列表、后台发现与新写入的目标。修改规则不删除远端文件，不清除已索引的恢复记录或待上传快照，也不会把排除项作为需要传播的删除。排除会使已排队操作暂停，恢复允许后可继续；请检查 `smart list` 的实际状态。显式 CLI 文件操作仍可访问这些路径。

## 自动版本和冲突策略

```sh
rdrive policy status
rdrive policy config --enable-versions --keep 5
rdrive policy config --profile dav --include '*.docx' --include '*.txt' --exclude 'scratch/**'
rdrive policy config --profile dav --conflicts stop
rdrive policy reset dav
```

自动版本默认关闭，默认冲突策略为 `stop`。开启后，Smart 覆盖内容前先保存此次比较条件指定的原修订，只有覆盖确认后才按保留数量裁剪旧版本。保存失败或新内容尚未确认时不会先删旧版本；其他写入者尚未确认发布的备份也不裁剪，因此未完成任务可能使保留数量暂时超过设置。版本保存在远端的 RDrive 内部版本存储，按远端路径归档；重命名后旧路径历史仍保留，不自动跟随新路径。这不是服务商自己的历史版本。显式 `put --version`、`versions list/save/restore/prune` 保持原有行为。

`stop` 保留冲突等待决定；`keep-both` 把本地内容另存为带稳定冲突标识的文件，使用只创建条件，保留远端原件；`replace` 先归档实际观察到的远端版本，再按该版本条件替换。本地决策在写入前持久化。若决定后远端再次改变，操作重新进入冲突，自动策略不会反复追赶并覆盖新的修改。

```sh
rdrive smart list cloud-smart
rdrive smart resolve cloud-smart MUTATION_ID --strategy keep-both
rdrive smart resolve cloud-smart MUTATION_ID --strategy replace
rdrive smart resolve cloud-smart MUTATION_ID --strategy remote
rdrive smart export cloud-smart MUTATION_ID ./recovered.txt
```

`remote` 取消本地发布，但独立本地恢复快照继续保留，后续可导出。决定只适用于保留了文件正文的新建/修改冲突。受保护替换用于原路径正文冲突；同时重命名或移动的复合冲突需要人工处理命名意图，仍可用 keep-both 保存正文。目录及缺失的目标需要按具体状态处理。托管仓库使用已有不可变快照历史，普通目录的文件版本不等同于多文件事务。

## 后台常驻

```sh
# 安装只保存配置，不启动；可先审阅 dry-run 结果。
rdrive service install --ipc-root /private/tmp/rdrive-user-ipc --dry-run
rdrive service install --ipc-root /private/tmp/rdrive-user-ipc
rdrive service start
rdrive service status
rdrive service restart
rdrive service stop
rdrive service uninstall
```

macOS 每个状态目录使用独立的用户 LaunchAgent。启动后监督自身的 Rust 桥接服务，崩溃后退避重启；若安装时传入 `--app /absolute/path/RDrive.app`，还监督原生变化通知助手。使用开发 TCP IPC 的包需要同时设置 `--development-tcp`，IPC 根必须与该包构建时记录的目录一致。IPC 路径应较短，以符合 Unix socket 路径长度限制。

服务不自动注册 File Provider 域，也不替用户启用系统扩展。只控制属于当前状态目录的固定任务；不会扫描并结束其他 RDrive 进程。`service run` 是供 launchd 使用的前台监督模式。卸载保留缓存、索引、连接和恢复内容。

配置与日志使用私有权限。服务配置只保存路径，不保存密码或环境变量。后台登录服务不会继承当前终端中的临时密码环境；实际后台连接建议使用 macOS 钥匙串。`service status` 的存活状态与连接认证成功是两件事，需同时检查 Smart 和索引进度。日志有限轮转，只记录子进程退出等监督事件。

## 索引搜索与 MCP

```sh
rdrive index search cloud-smart '报告'
rdrive index search cloud-smart 'report' --scope documents --kind file --page-size 50 --offset 0
```

搜索文件名与路径，完全读取本地 SQLite，不发起远端递归搜索或正文下载。三字符及以上使用 FTS5 trigram 子串索引；一至两个字符使用转义后的 SQLite LIKE，支持短中文名称和字面量 `%`、`_`。查询按字面内容解释，不执行用户提供的 FTS 语法。结果只覆盖已建立索引的项目，包含 `anchor`、`more` 和下一页偏移；跨页期间索引变化可能改变分页结果。过滤规则及待删除状态同样适用。

0.7 首次读取会事务升级旧 SQLite 的派生搜索索引，保留项目、离线修改、版本、固定策略和回执；未知格式或损坏会失败并保留原数据。升级前停止使用同一状态目录的旧版后台服务，勿混用旧版本。

MCP 增加 `rdrive_index_search`、`rdrive_filter_*`、`rdrive_policy_*`、`rdrive_smart_resolve/recovery` 和 `rdrive_service_*`。查询默认可用，配置、冲突决定及服务变更需要 `--allow-write`。规则列表使用字符串数组。服务 App/IPC 路径同样接受服务器本地路径授权范围校验。工具实际参数以 `rdrive mcp tools --allow-write` 为准。

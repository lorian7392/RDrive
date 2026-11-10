# CLI 完整参考

本文件由 `scripts/generate-cli-docs.py` 递归读取当前可执行文件的 `--help`
生成。每个入口都保留完整用法、位置参数、长短选项、默认值、可选值和继承的全局选项。
修改 CLI 后重新生成，并用 `--check` 检查文档与实际帮助是否一致。

```sh
cargo build --locked
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --check
```

生成过程只执行 `--help` 和 `--version`，不建立状态目录、不读取认证、不连接服务器。
`rdrive help COMMAND [SUBCOMMAND]` 与对应的 `--help` 等价；它由 Clap 自动提供，
没有在下方重复展开。`provider native` 的末尾参数交给可选原生伴随 App，
该 App 的使用方式另见 [macOS 集成](../MACOS_INTEGRATION.md)。

## 通用约定

- `<NAME>` 是必需值；`[NAME]` 是可省略值；`...` 表示可重复。
- `PROFILE` 是已有连接名；`DOMAIN` 是已有 File Provider / Smart 域 ID；
  `TRANSACTION`、`MUTATION`、`ID` 使用相应创建或列表命令返回的标识。
- `REMOTE`、`ROOT`、远端 `PATH`、`FROM`、`TO` 相对于连接或仓库根；空字符串表示根。
  使用 `/` 分隔，禁止开头 `/`、`..`、反斜杠和 NUL；重复 `/` 与 `.` 段会规范化。
  本地 `LOCAL`、`RECORD`、`JOURNAL`、`STAGING` 等是本地路径。
- 大小、容量和速率均使用字节；时间参数以秒为单位。路径或 glob 含空格、`*`
  等 shell 特殊字符时需要引号。布尔开关只需写选项名，不写 `true`。
- 结构化结果默认是缩进 JSON，`--json` 改为紧凑单行 JSON，错误通常在 stderr。
  `cat` 在 stdout 输出原始字节；`notify` 输出逐行 JSON 事件；MCP stdio 使用 JSON-RPC。
  进度在 stderr。操作失败返回非零状态；参数解析失败、非法限速可能在执行前失败。

## 全局选项与执行约束

- `--home PATH`：优先于 `RDRIVE_HOME`，未设置则使用
  `~/Library/Application Support/rdrive`。各状态目录互相独立。
- `--json`：改变结构化结果与通常的运行错误输出格式；不是内容编码选项。
- `--limit BYTES_PER_SECOND`：同一 Rust 进程所有传输共享的正整数载荷速率；
  `0` 被拒绝。不含协议头、加密和 TCP 重传，也不控制 macOS 原生卷 I/O。
- `--progress auto|off|json`：默认 `auto`，终端为进度条，重定向时为 JSON 事件；
  `json` 始终使用 JSON，`off` 禁用。速度窗口为最近两秒。
- `--jobs N`：队列并行目标数，默认 `4`，运行队列时要求 `1..=64`。
  相同连接和路径的任务保持顺序；不会自动并行化每一种命令。
- `-h, --help`：显示当前入口帮助；根入口的 `-V, --version` 显示版本。

`history --limit N` 是历史**条数**，默认 `20`，覆盖该入口继承的同名速率参数。
不要使用 `history --limit` 设置传输速率。

## 帮助之外的参数要求

以下要求来自实现中的验证；下方每个帮助块还给出全部位置参数与选项：

- `profile add` 的 `--protocol`、`--url` 必填，协议是 `webdav|smb`；连接名仅 ASCII
  字母、数字、`-`、`_`，不能为空或以 `-` 开头。`--timeout` 默认 `120` 且必须大于 `0`。
  HTTPS 为 WebDAV 默认要求；HTTP 需要 `--allow-http`。SMB 默认加密，
  `--allow-unencrypted-smb` 显式关闭。URL 不允许内嵌认证、查询或片段；
  `--nextcloud-upload-url` 只用于同源 WebDAV。凭据要求见 [配置参考](CONFIGURATION.md)。
- `get`、`cache get`、`repo get`、`smart export` 默认不覆盖本地文件；
  `put`、`cp`、`mv` 默认不覆盖远端目标。明确使用该入口的 `--overwrite` 才允许覆盖。
  `put --queue` 只保存持久快照和任务，`--resume` 需要协议支持。
  `cat --max-bytes N` 在下载前拒绝超过限制的文件。
- `rm --recursive` 才允许删除目录；根目录不能删除。
  `mkdir --parents` 才递归创建缺失祖先。`sync` 和 `versions prune` 默认只列计划，
  `--apply` 才执行；`cache clear` 也是 `--apply` 才清理。
- `sync` / `watch --direction` 为 `push|pull|both`，默认 `both`；
  `--delete` 只在成功基线后传播观察到的删除，`--metadata` 同步选定属性。
  `watch --interval` 默认 `30` 且必须大于 `0`。
- `cache config` 必须提供 `--max-bytes N` 或 `--unlimited`，两者互斥。
  `cache directory PATH` 与 `--default` 互斥；没有二者时查询当前目录，
  `--dry-run` 预览迁移。`cache prune --max-bytes` 默认 `0`，仍保护固定/在用内容。
- `index config --enable` / `--disable` 互斥，`--depth` / `--all` 互斥；
  两种刷新间隔均为 `1..=31536000`。`index search --kind` 为 `file|directory`，
  `--page-size` 默认 `100` 且要求 `1..=1000`，`--offset` 默认 `0`，最大 `1000000`。
  搜索词去除首尾空白后要求 `1..=512` 个可打印字符；搜索范围相对域根。
- `filter config --exclude-temporary` / `--include-temporary` 互斥；
  `--clear-include` / `--include` 互斥，`--clear-exclude` / `--exclude` 互斥。
  `--include`、`--exclude` 可以重复，替换提供的整个列表，未提供列表保持原值。
  `--profile` 只影响指定连接；`filter reset PROFILE` 移除该连接覆盖。
- `policy config --enable-versions` / `--disable-versions` 互斥；两种 `--clear-*`
  与对应列表互斥。`--keep` 是 `1..=100000`，冲突策略为 `stop|keep-both|replace`。
  规则、继承和默认值见 [配置参考](CONFIGURATION.md)。
- `smart setup --app` 要求同时传入 `--register`；默认不会注册原生扩展。
  `smart resolve --strategy` 必填，可取 `keep-both|replace|remote`。
  `smart retry` 可省略 mutation，重试整个域可重试的保留工作。
  `smart cancel` 保留恢复快照，`smart discard` 只丢弃已结束工作的恢复快照。
- `mcp serve --timeout` 默认 `300` 且必须大于 `0`；`--local-root` 可重复。
  `--allow-write` 才开放写工具，本地读写仍受授权根约束。
  具体 MCP 工具参数用 `mcp tools [--allow-write]` 查询，见 [MCP](../MCP.md)。
- `queue run --retry-failed` 明确重试失败任务；曾无条件覆盖且结果不明的任务
  还需要 `--retry-ambiguous-overwrite`。`transfers discard` 只丢弃下载检查点，
  保留已有本地恢复副本。
- `mount` 要求预先存在、为空、当前用户所有的 `0700` 私有目录；`--dry-run`
  只生成计划。`--browse` 允许系统显示卷，`--no-prompt` 仅使用系统已有凭据，
  `--read-only` 请求真正只读卷。`unmount --force` 使用系统强制卸载选项。
- `provider serve --socket` 与 `--staging` 必填；`--development-tcp` 仅用于对应开发包。
  `service install` 默认只安装用户服务配置，需 `service start` 启动。
  安装参数保存的路径和开发 IPC 模式必须与所用 App 相符。
- `lock --seconds` 默认 `600`，实际期限由 WebDAV 服务器决定，不自动续约。
  修改和 `unlock` 通过 `RDRIVE_WEBDAV_LOCK_TOKEN` 读取 token，无 token 参数。

关于条件写入、恢复副本、协议能力和限制，见 [功能说明](../FEATURES.md)。

## 可选原生伴随 App 参数

`provider native [--app /absolute/path/RDrive.app] -- ARGS...` 将末尾参数原样传给
`Contents/MacOS/RDriveCompanion`，不是另一个 Clap 命令组。未给 `--app` 时使用
CLI 编译时源码目录的 `macos/build/RDrive.app`；移动安装位置后宜显式指定 App。
无 ARGS 时启动菜单栏 App，报告进程已启动，不代表服务器认证或系统注册成功。

原生程序没有统一 `--help`；下面按 `macos/Companion/main.swift` 当前实现列出全部控制选项，
没有执行这些系统操作来生成文档。每次只选择一项操作，避免多个动作依实现分支优先级执行。

- `--group-directory PATH`：覆盖伴随程序的开发 IPC/group 目录；要求已存在的绝对私有目录，
  当前用户所有。常规使用构建包内的匹配路径，不能用它自动改写扩展配置。
- `--diagnose`：输出 bundle、Finder 扩展开关和 group 路径/错误；不是连接测试。
- `--list-domains`：查询 macOS 已注册 File Provider 域。
- `--register-domain DOMAIN [--display-name NAME]`：向系统注册域，显示名默认域 ID；
  应先有匹配的 RDrive 本地域配置。
- `--remove-domain DOMAIN`：请求系统移除域，使用保留已下载用户数据的模式，结果包含保留位置。
- `--domain-root DOMAIN`：查询该域的用户可见根路径。
- `--resolve-file PATH`：由系统可见文件路径查询域与项目 ID。
- `--signal DOMAIN`：请求刷新域的 working-set 枚举器。
- `--download DOMAIN ITEM_ID`：请求系统下载项目，不是修改离线固定策略。
- `--evict DOMAIN ITEM_ID`：请求系统逐出项目，受系统固定/在用/未上传保护限制。
- `--pin DOMAIN ITEM_ID` / `--unpin DOMAIN ITEM_ID`：预检容量并设置/解除固定策略，
  由 bridge 和系统内容策略完成；解除固定不保证立即逐出。
- `--check-cache DOMAIN ITEM_ID`：预检并释放临时容量预留，不固定或下载内容。
- `--trim-cache BYTES`：请求回收可移除的系统清洁副本，接受非负整数；
  它不保存 `cache.max_bytes` 配置，也不移动系统目录。
- `--set-tags PATH [TAG...]`：通过 macOS 标准 URL resource tagNames 设置标签；没有 TAG 时清空。
  PATH 后的全部参数都作为标签名，不要在后面追加其他选项。
- `--watch-domains`：前台运行原生变化通知助手，约每 3 秒轮询，SIGINT/SIGTERM 停止。
- `--enable-finder`：打开系统扩展管理界面，用户仍需在系统界面实际启用。
- `--show-status DOMAIN ITEM_ID`：启动菜单栏 App 并打开项目状态窗口。
- `--show-message TEXT`：启动菜单栏 App 并显示消息对话框。

除持续通知/GUI 外，异步系统控制等待上限通常为 30 秒；超时不代表系统操作被撤回。
GUI 启动可能管理 bridge，不应将其作为无副作用的帮助命令。
平台、签名、启用及实际验证范围见 [macOS 集成](../MACOS_INTEGRATION.md)。

## 全部命令帮助

生成版本：`rdrive 0.8.0`。共 **111 个命令入口**（含命令组，不含根入口和自动 `help`）、**97 个可执行叶子命令**。下方保留 **788 条选项声明**（包含各入口继承的全局选项，仅清理帮助文字的行尾空白）。

### `rdrive`

```text
WebDAV and SMB files, offline cache, durable uploads, and conservative sync

Usage: rdrive [OPTIONS] <COMMAND>

Commands:
  mcp        Expose existing operations to AI clients over MCP standard input/output
  profile    Add/list/remove connection profiles. Never accepts a password argument
  info       Test connection and show backend capabilities
  ls         List remote children or recursively search the tree
  stat       Read remote type, size, timestamp and revision metadata
  get        Download through a verified temporary file, then commit locally
  cat        Print remote file bytes to stdout (status goes to stderr)
  put        Snapshot a local file to the durable queue, then upload it
  mkdir      Create a remote directory; --parents creates missing ancestors
  mv         Rename or move within one connection, without overwrite by default
  cp         Copy within one connection, without overwrite by default
  rm         Explicit remote deletion. Directories require --recursive
  url        Copyable location URL. Does not create a public sharing link
  edit       Download, edit with an executable from EDITOR, then queue a guarded upload
  cache      Fetch, pin, read offline and prune cached files
  queue      Inspect, run or cancel durable upload jobs
  sync       Plan by default; --apply commits. Conflicts stop the entire plan
  watch      Repeat sync in the foreground; stop with Ctrl-C
  notify     Stream server change notifications (SMB); overflow/disconnect requires rescan
  repo       Atomic managed snapshot repository and durable transactions
  provider   Finder/File Provider bridge and domain configuration
  smart      Configure on-demand offline domains and manage retained local edits
  index      Configure background metadata indexing and request scoped refreshes
  filter     Smart inclusion/exclusion rules; never deletes excluded remote files
  policy     Automatic version retention and conflict resolution policy
  service    Install and control the per-user background supervisor
  metadata   Capture, apply or synchronize selected user extended attributes/Finder tags
  versions   Save, inspect, restore and prune custom remote file versions
  lock       Acquire a WebDAV write lock; save its token for later writes/unlock
  unlock     Uses RDRIVE_WEBDAV_LOCK_TOKEN; tokens are not accepted as arguments
  status     Local queue/cache summary; works offline
  mount      Mount through macOS native SMB/WebDAV helpers
  mounts     List mounts reported by the operating system
  unmount    Unmount a native SMB/WebDAV filesystem
  transfers  Inspect resumable transfers or discard a saved download prefix
  history    Show locally recorded completed operations
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
  -V, --version              Print version
```

### `rdrive mcp`

```text
Expose existing operations to AI clients over MCP standard input/output

Usage: rdrive mcp [OPTIONS] <COMMAND>

Commands:
  tools  Print the MCP tool catalog as JSON without starting a server
  serve  Run a JSON-RPC stdio server; remote/local changes require --allow-write
  help   Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mcp tools`

```text
Print the MCP tool catalog as JSON without starting a server

Usage: rdrive mcp tools [OPTIONS]

Options:
      --allow-write
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mcp serve`

```text
Run a JSON-RPC stdio server; remote/local changes require --allow-write

Usage: rdrive mcp serve [OPTIONS]

Options:
      --allow-write
      --home <HOME>              State/config directory (also RDRIVE_HOME)
      --json                     Compact machine-readable JSON. Errors are JSON on stderr
      --local-root <LOCAL_ROOT>  Permit local file access inside this directory (repeatable)
      --limit <LIMIT>            Aggregate payload bytes/second across all jobs (headers excluded)
      --timeout <TIMEOUT>        Maximum duration of each underlying CLI operation, in seconds [default: 300]
      --progress <PROGRESS>      Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>              Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                     Print help
```

### `rdrive profile`

```text
Add/list/remove connection profiles. Never accepts a password argument

Usage: rdrive profile [OPTIONS] <COMMAND>

Commands:
  add
  list
  remove
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive profile add`

```text
Usage: rdrive profile add [OPTIONS] --protocol <PROTOCOL> --url <URL> <NAME>

Arguments:
  <NAME>

Options:
      --home <HOME>
          State/config directory (also RDRIVE_HOME)
      --protocol <PROTOCOL>
          [possible values: webdav, smb]
      --json
          Compact machine-readable JSON. Errors are JSON on stderr
      --url <URL>

      --limit <LIMIT>
          Aggregate payload bytes/second across all jobs (headers excluded)
      --username <USERNAME>

      --password-env <PASSWORD_ENV>

      --progress <PROGRESS>
          Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --allow-http

      --jobs <JOBS>
          Concurrent queue targets. Jobs for the same target retain order [default: 4]
      --allow-unencrypted-smb
          Permit SMB without encryption (for trusted compatibility testing)
      --timeout <TIMEOUT>
          [default: 120]
      --nextcloud-upload-url <NEXTCLOUD_UPLOAD_URL>
          Explicit Nextcloud v2 uploads URL, e.g. https://host/remote.php/dav/uploads/user/
  -h, --help
          Print help
```

### `rdrive profile list`

```text
Usage: rdrive profile list [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive profile remove`

```text
Usage: rdrive profile remove [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive info`

```text
Test connection and show backend capabilities

Usage: rdrive info [OPTIONS] <PROFILE>

Arguments:
  <PROFILE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive ls`

```text
List remote children or recursively search the tree

Usage: rdrive ls [OPTIONS] <PROFILE> [REMOTE]

Arguments:
  <PROFILE>
  [REMOTE]   [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
  -r, --recursive
      --contains <CONTAINS>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive stat`

```text
Read remote type, size, timestamp and revision metadata

Usage: rdrive stat [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive get`

```text
Download through a verified temporary file, then commit locally

Usage: rdrive get [OPTIONS] <PROFILE> <REMOTE> <LOCAL>

Arguments:
  <PROFILE>
  <REMOTE>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --resume               Continue an interrupted download from its durable verified prefix
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cat`

```text
Print remote file bytes to stdout (status goes to stderr)

Usage: rdrive cat [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --max-bytes <MAX_BYTES>  Refuse larger files before downloading; used for bounded MCP text reads
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                   Print help
```

### `rdrive put`

```text
Snapshot a local file to the durable queue, then upload it

Usage: rdrive put [OPTIONS] <PROFILE> <LOCAL> <REMOTE>

Arguments:
  <PROFILE>
  <LOCAL>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --version
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --queue
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --resume               Use a durable resumable staging session (SMB or configured Nextcloud)
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mkdir`

```text
Create a remote directory; --parents creates missing ancestors

Usage: rdrive mkdir [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
  -p, --parents
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mv`

```text
Rename or move within one connection, without overwrite by default

Usage: rdrive mv [OPTIONS] <PROFILE> <FROM> <TO>

Arguments:
  <PROFILE>
  <FROM>
  <TO>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cp`

```text
Copy within one connection, without overwrite by default

Usage: rdrive cp [OPTIONS] <PROFILE> <FROM> <TO>

Arguments:
  <PROFILE>
  <FROM>
  <TO>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive rm`

```text
Explicit remote deletion. Directories require --recursive

Usage: rdrive rm [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
  -r, --recursive
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive url`

```text
Copyable location URL. Does not create a public sharing link

Usage: rdrive url [OPTIONS] <PROFILE> [REMOTE]

Arguments:
  <PROFILE>
  [REMOTE]   [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive edit`

```text
Download, edit with an executable from EDITOR, then queue a guarded upload

Usage: rdrive edit [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache`

```text
Fetch, pin, read offline and prune cached files

Usage: rdrive cache [OPTIONS] <COMMAND>

Commands:
  clear      Preview eligible clean cache removal; --apply evicts only unpinned/inactive payloads
  directory  Show or safely migrate the Rust content cache (system Finder storage is separate)
  status     Inspect the persistent clean-cache limit and actual usage, without networking
  config     Persist an aggregate clean-cache maximum (bytes); --unlimited removes it
  fetch      Fetch files online; --pin protects copies from cache prune
  get        Read an already cached file without contacting the server
  list
  unpin
  prune      Evict least recently used unpinned files until below this size
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache clear`

```text
Preview eligible clean cache removal; --apply evicts only unpinned/inactive payloads

Usage: rdrive cache clear [OPTIONS]

Options:
      --apply
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache directory`

```text
Show or safely migrate the Rust content cache (system Finder storage is separate)

Usage: rdrive cache directory [OPTIONS] [PATH]

Arguments:
  [PATH]

Options:
      --default
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --dry-run
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache status`

```text
Inspect the persistent clean-cache limit and actual usage, without networking

Usage: rdrive cache status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache config`

```text
Persist an aggregate clean-cache maximum (bytes); --unlimited removes it

Usage: rdrive cache config [OPTIONS]

Options:
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --max-bytes <MAX_BYTES>
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --unlimited
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                   Print help
```

### `rdrive cache fetch`

```text
Fetch files online; --pin protects copies from cache prune

Usage: rdrive cache fetch [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --pin
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
  -r, --recursive
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache get`

```text
Read an already cached file without contacting the server

Usage: rdrive cache get [OPTIONS] <PROFILE> <REMOTE> <LOCAL>

Arguments:
  <PROFILE>
  <REMOTE>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache list`

```text
Usage: rdrive cache list [OPTIONS] [PROFILE]

Arguments:
  [PROFILE]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache unpin`

```text
Usage: rdrive cache unpin [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
  -r, --recursive
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive cache prune`

```text
Evict least recently used unpinned files until below this size

Usage: rdrive cache prune [OPTIONS]

Options:
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --max-bytes <MAX_BYTES>  [default: 0]
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                   Print help
```

### `rdrive queue`

```text
Inspect, run or cancel durable upload jobs

Usage: rdrive queue [OPTIONS] <COMMAND>

Commands:
  list
  run
  cancel
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive queue list`

```text
Usage: rdrive queue list [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive queue run`

```text
Usage: rdrive queue run [OPTIONS] [PROFILE]

Arguments:
  [PROFILE]

Options:
      --home <HOME>                State/config directory (also RDRIVE_HOME)
      --retry-failed
      --json                       Compact machine-readable JSON. Errors are JSON on stderr
      --retry-ambiguous-overwrite  Explicitly retry an interrupted/failed unconditional overwrite
      --limit <LIMIT>              Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>        Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>                Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                       Print help
```

### `rdrive queue cancel`

```text
Usage: rdrive queue cancel [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive sync`

```text
Plan by default; --apply commits. Conflicts stop the entire plan

Usage: rdrive sync [OPTIONS] <PROFILE> <LOCAL> [REMOTE]

Arguments:
  <PROFILE>
  <LOCAL>
  [REMOTE]   [default: ]

Options:
      --direction <DIRECTION>  [default: both] [possible values: push, pull, both]
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --apply
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --delete                 Propagate observed deletions after a successful baseline exists
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --metadata
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                   Print help
```

### `rdrive watch`

```text
Repeat sync in the foreground; stop with Ctrl-C

Usage: rdrive watch [OPTIONS] <PROFILE> <LOCAL> [REMOTE]

Arguments:
  <PROFILE>
  <LOCAL>
  [REMOTE]   [default: ]

Options:
      --direction <DIRECTION>  [default: both] [possible values: push, pull, both]
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --apply
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --delete
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --interval <INTERVAL>    [default: 30]
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
      --metadata
  -h, --help                   Print help
```

### `rdrive notify`

```text
Stream server change notifications (SMB); overflow/disconnect requires rescan

Usage: rdrive notify [OPTIONS] <PROFILE> [REMOTE]

Arguments:
  <PROFILE>
  [REMOTE]   [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --recursive
      --count <COUNT>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo`

```text
Atomic managed snapshot repository and durable transactions

Usage: rdrive repo [OPTIONS] <COMMAND>

Commands:
  init
  begin
  put
  mkdir
  rm
  mv
  commit
  status
  abort
  transactions
  ls
  get
  help          Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo init`

```text
Usage: rdrive repo init [OPTIONS] <PROFILE> [ROOT]

Arguments:
  <PROFILE>
  [ROOT]     [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo begin`

```text
Usage: rdrive repo begin [OPTIONS] <PROFILE> [ROOT]

Arguments:
  <PROFILE>
  [ROOT]     [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo put`

```text
Usage: rdrive repo put [OPTIONS] <TRANSACTION> <LOCAL> <PATH>

Arguments:
  <TRANSACTION>
  <LOCAL>
  <PATH>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo mkdir`

```text
Usage: rdrive repo mkdir [OPTIONS] <TRANSACTION> <PATH>

Arguments:
  <TRANSACTION>
  <PATH>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo rm`

```text
Usage: rdrive repo rm [OPTIONS] <TRANSACTION> <PATH>

Arguments:
  <TRANSACTION>
  <PATH>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --recursive
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo mv`

```text
Usage: rdrive repo mv [OPTIONS] <TRANSACTION> <FROM> <TO>

Arguments:
  <TRANSACTION>
  <FROM>
  <TO>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo commit`

```text
Usage: rdrive repo commit [OPTIONS] <TRANSACTION>

Arguments:
  <TRANSACTION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo status`

```text
Usage: rdrive repo status [OPTIONS] <TRANSACTION>

Arguments:
  <TRANSACTION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo abort`

```text
Usage: rdrive repo abort [OPTIONS] <TRANSACTION>

Arguments:
  <TRANSACTION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo transactions`

```text
Usage: rdrive repo transactions [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo ls`

```text
Usage: rdrive repo ls [OPTIONS] <PROFILE> [ROOT]

Arguments:
  <PROFILE>
  [ROOT]     [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive repo get`

```text
Usage: rdrive repo get [OPTIONS] <PROFILE> <PATH> <LOCAL>

Arguments:
  <PROFILE>
  <PATH>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --root <ROOT>          [default: ]
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --overwrite
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive provider`

```text
Finder/File Provider bridge and domain configuration

Usage: rdrive provider [OPTIONS] <COMMAND>

Commands:
  serve
  domains
  domain-add
  domain-remove
  native         Invoke the built native companion (build first with macos/build.py)
  help           Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive provider serve`

```text
Usage: rdrive provider serve [OPTIONS] --socket <SOCKET> --staging <STAGING>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --socket <SOCKET>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --staging <STAGING>
      --development-tcp
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive provider domains`

```text
Usage: rdrive provider domains [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive provider domain-add`

```text
Usage: rdrive provider domain-add [OPTIONS] <ID> <PROFILE> [REMOTE]

Arguments:
  <ID>
  <PROFILE>
  [REMOTE]   [default: ]

Options:
      --home <HOME>                  State/config directory (also RDRIVE_HOME)
      --repository
      --json                         Compact machine-readable JSON. Errors are JSON on stderr
      --smart
      --limit <LIMIT>                Aggregate payload bytes/second across all jobs (headers excluded)
      --local-root <LOCAL_ROOT>
      --display-name <DISPLAY_NAME>
      --progress <PROGRESS>          Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>                  Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                         Print help
```

### `rdrive provider domain-remove`

```text
Usage: rdrive provider domain-remove [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive provider native`

```text
Invoke the built native companion (build first with macos/build.py)

Usage: rdrive provider native [OPTIONS] [ARGS]...

Arguments:
  [ARGS]...

Options:
      --app <APP>
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart`

```text
Configure on-demand offline domains and manage retained local edits

Usage: rdrive smart [OPTIONS] <COMMAND>

Commands:
  setup     Save a Smart domain; configure a 5 GiB clean-content budget if none exists
  status    Show local Smart state, durable mutations and clean-cache accounting
  list      List retained mutations, optionally restricted to one domain
  pause
  resume
  retry     Mark retained work retryable without changing its original version guards
  cancel    Cancel pending work while preserving its independent local snapshot
  resolve   Resolve a conflict using a durable decision; remote retains a local recovery copy
  recovery  List retained remote namespace recovery backups (space is retained)
  export    Export a retained local edit to a separate real file for recovery
  discard   Discard only a completed/cancelled/superseded recovery snapshot
  run       Perform one reconnect/replay pass; provider serve also runs automatically
  help      Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart setup`

```text
Save a Smart domain; configure a 5 GiB clean-content budget if none exists

Usage: rdrive smart setup [OPTIONS] <ID> <PROFILE> [REMOTE]

Arguments:
  <ID>
  <PROFILE>
  [REMOTE]   [default: ]

Options:
      --home <HOME>                  State/config directory (also RDRIVE_HOME)
      --repository
      --display-name <DISPLAY_NAME>
      --json                         Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>                Aggregate payload bytes/second across all jobs (headers excluded)
      --register                     Register with the already built macOS File Provider companion
      --app <APP>
      --progress <PROGRESS>          Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>                  Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                         Print help
```

### `rdrive smart status`

```text
Show local Smart state, durable mutations and clean-cache accounting

Usage: rdrive smart status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart list`

```text
List retained mutations, optionally restricted to one domain

Usage: rdrive smart list [OPTIONS] [DOMAIN]

Arguments:
  [DOMAIN]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart pause`

```text
Usage: rdrive smart pause [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart resume`

```text
Usage: rdrive smart resume [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart retry`

```text
Mark retained work retryable without changing its original version guards

Usage: rdrive smart retry [OPTIONS] <DOMAIN> [MUTATION]

Arguments:
  <DOMAIN>
  [MUTATION]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart cancel`

```text
Cancel pending work while preserving its independent local snapshot

Usage: rdrive smart cancel [OPTIONS] <DOMAIN> <MUTATION>

Arguments:
  <DOMAIN>
  <MUTATION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart resolve`

```text
Resolve a conflict using a durable decision; remote retains a local recovery copy

Usage: rdrive smart resolve [OPTIONS] --strategy <STRATEGY> <DOMAIN> <MUTATION>

Arguments:
  <DOMAIN>
  <MUTATION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --strategy <STRATEGY>  [possible values: keep-both, replace, remote]
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart recovery`

```text
List retained remote namespace recovery backups (space is retained)

Usage: rdrive smart recovery [OPTIONS] <DOMAIN>

Arguments:
  <DOMAIN>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart export`

```text
Export a retained local edit to a separate real file for recovery

Usage: rdrive smart export [OPTIONS] <DOMAIN> <MUTATION> <LOCAL>

Arguments:
  <DOMAIN>
  <MUTATION>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart discard`

```text
Discard only a completed/cancelled/superseded recovery snapshot

Usage: rdrive smart discard [OPTIONS] <DOMAIN> <MUTATION>

Arguments:
  <DOMAIN>
  <MUTATION>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive smart run`

```text
Perform one reconnect/replay pass; provider serve also runs automatically

Usage: rdrive smart run [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --staging <STAGING>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive index`

```text
Configure background metadata indexing and request scoped refreshes

Usage: rdrive index [OPTIONS] <COMMAND>

Commands:
  search   Search cached file names/paths locally; works without network access
  status   Inspect local index progress without waiting for a network operation
  config   Persist background crawl and refresh policy. None of these options downloads files
  refresh  Queue a metadata refresh; provider serve performs it asynchronously
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive index search`

```text
Search cached file names/paths locally; works without network access

Usage: rdrive index search [OPTIONS] <DOMAIN> <QUERY>

Arguments:
  <DOMAIN>
  <QUERY>

Options:
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --scope <SCOPE>          [default: ]
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --kind <KIND>            [possible values: file, directory]
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --page-size <PAGE_SIZE>  [default: 100]
      --offset <OFFSET>        [default: 0]
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                   Print help
```

### `rdrive index status`

```text
Inspect local index progress without waiting for a network operation

Usage: rdrive index status [OPTIONS] [DOMAIN]

Arguments:
  [DOMAIN]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive index config`

```text
Persist background crawl and refresh policy. None of these options downloads files

Usage: rdrive index config [OPTIONS]

Options:
      --enable

      --home <HOME>
          State/config directory (also RDRIVE_HOME)
      --disable

      --json
          Compact machine-readable JSON. Errors are JSON on stderr
      --depth <DEPTH>

      --limit <LIMIT>
          Aggregate payload bytes/second across all jobs (headers excluded)
      --all

      --progress <PROGRESS>
          Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>
          Concurrent queue targets. Jobs for the same target retain order [default: 4]
      --refresh-seconds <REFRESH_SECONDS>

      --active-refresh-seconds <ACTIVE_REFRESH_SECONDS>

  -h, --help
          Print help
```

### `rdrive index refresh`

```text
Queue a metadata refresh; provider serve performs it asynchronously

Usage: rdrive index refresh [OPTIONS] <DOMAIN> [REMOTE]

Arguments:
  <DOMAIN>
  [REMOTE]  [default: ]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --recursive
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive filter`

```text
Smart inclusion/exclusion rules; never deletes excluded remote files

Usage: rdrive filter [OPTIONS] <COMMAND>

Commands:
  status
  config  Replace supplied rule lists; omitted lists stay unchanged
  reset   Remove a profile override and inherit global rules
  test
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive filter status`

```text
Usage: rdrive filter status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --profile <PROFILE>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive filter config`

```text
Replace supplied rule lists; omitted lists stay unchanged

Usage: rdrive filter config [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --profile <PROFILE>
      --exclude-temporary
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --include-temporary
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --include <INCLUDE>
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --exclude <EXCLUDE>
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
      --clear-include
      --clear-exclude
  -h, --help                 Print help
```

### `rdrive filter reset`

```text
Remove a profile override and inherit global rules

Usage: rdrive filter reset [OPTIONS] <PROFILE>

Arguments:
  <PROFILE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive filter test`

```text
Usage: rdrive filter test [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --directory
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive policy`

```text
Automatic version retention and conflict resolution policy

Usage: rdrive policy [OPTIONS] <COMMAND>

Commands:
  status
  config
  reset
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive policy status`

```text
Usage: rdrive policy status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --profile <PROFILE>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive policy config`

```text
Usage: rdrive policy config [OPTIONS]

Options:
      --home <HOME>            State/config directory (also RDRIVE_HOME)
      --profile <PROFILE>
      --enable-versions
      --json                   Compact machine-readable JSON. Errors are JSON on stderr
      --disable-versions
      --limit <LIMIT>          Aggregate payload bytes/second across all jobs (headers excluded)
      --keep <KEEP>
      --progress <PROGRESS>    Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --conflicts <CONFLICTS>  [possible values: stop, keep-both, replace]
      --jobs <JOBS>            Concurrent queue targets. Jobs for the same target retain order [default: 4]
      --include <INCLUDE>
      --exclude <EXCLUDE>
      --clear-include
      --clear-exclude
  -h, --help                   Print help
```

### `rdrive policy reset`

```text
Usage: rdrive policy reset [OPTIONS] <PROFILE>

Arguments:
  <PROFILE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service`

```text
Install and control the per-user background supervisor

Usage: rdrive service [OPTIONS] <COMMAND>

Commands:
  install
  start
  stop
  restart
  uninstall
  status
  run        Foreground supervisor used by the LaunchAgent. Stop with Ctrl-C
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service install`

```text
Usage: rdrive service install [OPTIONS]

Options:
      --home <HOME>
          State/config directory (also RDRIVE_HOME)
      --ipc-root <IPC_ROOT>

      --app <APP>

      --json
          Compact machine-readable JSON. Errors are JSON on stderr
      --development-tcp

      --limit <LIMIT>
          Aggregate payload bytes/second across all jobs (headers excluded)
      --launch-agents-dir <LAUNCH_AGENTS_DIR>

      --progress <PROGRESS>
          Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --dry-run

      --jobs <JOBS>
          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help
          Print help
```

### `rdrive service start`

```text
Usage: rdrive service start [OPTIONS]

Options:
      --dry-run
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service stop`

```text
Usage: rdrive service stop [OPTIONS]

Options:
      --dry-run
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service restart`

```text
Usage: rdrive service restart [OPTIONS]

Options:
      --dry-run
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service uninstall`

```text
Usage: rdrive service uninstall [OPTIONS]

Options:
      --dry-run
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service status`

```text
Usage: rdrive service status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive service run`

```text
Foreground supervisor used by the LaunchAgent. Stop with Ctrl-C

Usage: rdrive service run [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata`

```text
Capture, apply or synchronize selected user extended attributes/Finder tags

Usage: rdrive metadata [OPTIONS] <COMMAND>

Commands:
  capture
  apply
  recover
  push
  pull
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata capture`

```text
Usage: rdrive metadata capture [OPTIONS] <LOCAL>

Arguments:
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata apply`

```text
Usage: rdrive metadata apply [OPTIONS] <LOCAL> <RECORD>

Arguments:
  <LOCAL>
  <RECORD>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --journal <JOURNAL>
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata recover`

```text
Usage: rdrive metadata recover [OPTIONS] <LOCAL> <JOURNAL>

Arguments:
  <LOCAL>
  <JOURNAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata push`

```text
Usage: rdrive metadata push [OPTIONS] <PROFILE> <LOCAL> <REMOTE>

Arguments:
  <PROFILE>
  <LOCAL>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive metadata pull`

```text
Usage: rdrive metadata pull [OPTIONS] <PROFILE> <REMOTE> <LOCAL>

Arguments:
  <PROFILE>
  <REMOTE>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive versions`

```text
Save, inspect, restore and prune custom remote file versions

Usage: rdrive versions [OPTIONS] <COMMAND>

Commands:
  save
  list
  restore
  prune
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive versions save`

```text
Usage: rdrive versions save [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive versions list`

```text
Usage: rdrive versions list [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive versions restore`

```text
Usage: rdrive versions restore [OPTIONS] <PROFILE> <REMOTE> <ID>

Arguments:
  <PROFILE>
  <REMOTE>
  <ID>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --overwrite
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive versions prune`

```text
Usage: rdrive versions prune [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --keep <KEEP>          [default: 10]
      --apply
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive lock`

```text
Acquire a WebDAV write lock; save its token for later writes/unlock

Usage: rdrive lock [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --seconds <SECONDS>    [default: 600]
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive unlock`

```text
Uses RDRIVE_WEBDAV_LOCK_TOKEN; tokens are not accepted as arguments

Usage: rdrive unlock [OPTIONS] <PROFILE> <REMOTE>

Arguments:
  <PROFILE>
  <REMOTE>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive status`

```text
Local queue/cache summary; works offline

Usage: rdrive status [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mount`

```text
Mount through macOS native SMB/WebDAV helpers

Usage: rdrive mount [OPTIONS] <PROFILE> <LOCAL>

Arguments:
  <PROFILE>
  <LOCAL>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --read-only
      --browse
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --no-prompt
      --dry-run
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive mounts`

```text
List mounts reported by the operating system

Usage: rdrive mounts [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive unmount`

```text
Unmount a native SMB/WebDAV filesystem

Usage: rdrive unmount [OPTIONS] <LOCAL>

Arguments:
  <LOCAL>

Options:
      --force
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive transfers`

```text
Inspect resumable transfers or discard a saved download prefix

Usage: rdrive transfers [OPTIONS] <COMMAND>

Commands:
  list
  discard  Discard a download checkpoint; any prior-local recovery copy is retained
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive transfers list`

```text
Usage: rdrive transfers list [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive transfers discard`

```text
Discard a download checkpoint; any prior-local recovery copy is retained

Usage: rdrive transfers discard [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --limit <LIMIT>        Aggregate payload bytes/second across all jobs (headers excluded)
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

### `rdrive history`

```text
Show locally recorded completed operations

Usage: rdrive history [OPTIONS]

Options:
      --home <HOME>          State/config directory (also RDRIVE_HOME)
      --limit <limit>        [default: 20]
      --json                 Compact machine-readable JSON. Errors are JSON on stderr
      --progress <PROGRESS>  Live stderr progress; auto uses terminal bars or JSON when redirected [default: auto] [possible values: auto, off, json]
      --jobs <JOBS>          Concurrent queue targets. Jobs for the same target retain order [default: 4]
  -h, --help                 Print help
```

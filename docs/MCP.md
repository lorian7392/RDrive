# MCP 与 AI 操作技能

RDrive 0.5.0 在原有 CLI 上提供本地 MCP stdio 服务。每个工具通过独立子进程调用同一个 RDrive 可执行文件，复用原有 WebDAV、SMB、缓存、同步基线和事务实现；服务等待请求期间不持有状态目录锁。Finder 中正在运行的 0.4.1 伴随 App 无需因接入 MCP 而重新挂载。

## 启动

```sh
# 默认只开放查询工具。
rdrive --home '/absolute/path/state' mcp serve

# 开放写入工具，允许读取/写入指定本地工作目录。
rdrive --home '/absolute/path/state' mcp serve \
  --allow-write --local-root '/absolute/path/workspace' --timeout 300

# 无需开启会话，查看当前工具名、说明、参数及行为标记。
rdrive mcp tools
rdrive mcp tools --allow-write
```

全局 `--limit` 和 `--jobs` 会传递给底层操作，例如 `rdrive --limit 10485760 --jobs 4 --home '/absolute/path/state' mcp serve --allow-write`。MCP 同一时间运行一项操作，队列内部仍可按 `--jobs` 调度。传输期间仍响应 ping 和取消；取消或超时会终止并回收当前 CLI 子进程，但不能撤销已经完成的远端修改，应先检查队列或事务状态再重试。

服务从标准输入读取 JSON-RPC，每行一条消息；标准输出只写协议消息，不输出进度条或 CLI 文本。输入消息上限为 1 MiB，子进程 stdout / stderr 分别限制为 8 MiB；`read_text` 限制为 1 MiB 的 UTF-8 文本，并在下载前检查远端大小。客户端关闭输入后服务退出。此接口使用带初始化握手的 2025-11-25 协议，同时支持 2025-06-18 和 2025-03-26，协商版本以 `initialize` 返回值为准。使用方式见 [MCP 初始化规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) 和 [stdio 传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)。这不是 HTTP / SSE 服务。

认证沿用已有 profile 的密码环境变量或 macOS 钥匙串。`--home` 必须指向保存所需 profile 的状态目录；仅仅看见 Finder 挂载目录不代表 MCP 已获得该账户的认证。环境变量必须存在于 MCP 服务进程中；另一个后台进程拥有密码并不意味着新进程也有。接口不接受密码、任意 shell 命令、编辑器或原生伴随 App 的执行参数。

## 接入客户端

生成实际绝对路径的配置，不会改写客户端设置，也不会读取或输出密码。分发包默认使用 `dist/rdrive`；从源码仓库构建时，使用 `--binary ./target/release/rdrive` 指定已构建的程序：

```sh
python3 scripts/mcp-config.py --home '/absolute/path/state'
python3 scripts/mcp-config.py --home '/absolute/path/state' \
  --allow-write --local-root '/absolute/path/workspace' --format toml
python3 scripts/mcp-config.py --binary ./target/release/rdrive \
  --home '/absolute/path/state' --format toml
```

JSON 输出供使用 `mcpServers` 配置结构的客户端使用；TOML 输出使用 `[mcp_servers.rdrive]`、`command`、`args`。将输出合并到相应客户端的配置，保留其他已有服务。[Codex 官方 MCP 配置说明](https://developers.openai.com/codex/mcp) 中还提供了环境变量转发和超时设置：如果 profile 使用环境变量，可以只转发名称，例如 `env_vars = ["RDRIVE_PASSWORD"]`；不要把密码值写入示例或版本控制。客户端工具超时应不小于服务端 `--timeout`，需要时配置 `tool_timeout_sec = 330`。

服务端 `--allow-write` 决定哪些能力可调用；具体操作仍应遵循用户已授权的任务范围。未启用时，写入工具不出现在 `tools/list` 中，直接调用也会被拒绝。本地文件参数必须位于 `--local-root` 声明的现有目录内，状态目录受到独立保护。此范围限制用于这些本地路径参数；进程仍需要读取自己的状态、凭据，以及使用系统挂载接口。路径校验防止误访问和已存在的符号链接绕过，不是针对其他本地进程并发替换路径的操作系统隔离。

## 工具与资源

共 59 个工具，默认只开放其中 23 个查询工具。所有工具以 `rdrive_` 开头。权威参数定义来自 `rdrive mcp tools --allow-write`，不接受额外或类型错误的字段。

- 连接与查询：`profiles`、`status`、`info`、`list`、`stat`、`read_text`、`url`、`history`。
- 文件传输和远端修改：`download`、`upload`、`mkdir`、`copy`、`move`、`remove`。覆盖、递归删除、断点续传沿用 CLI 的显式参数和协议能力。
- 原生挂载：`mounts`、`mount_plan`、`mount`、`unmount`。实际挂载禁止交互密码提示，SMB 需系统已有凭据；远端连接密码仍按 CLI 的协议区别处理。
- 同步、队列及传输检查点：`sync_plan`、`sync_apply`、`queue_list`、`queue_run`、`queue_cancel`、`transfers`。计划报告与冲突报告保留结构化结果。
- 清洁缓存：`cache_status`、`cache_list`、`cache_fetch`、`cache_get`、`cache_config`、`cache_unpin`、`cache_prune`。
- Smart 与域：`domains`、`smart_status`、`smart_list`、`smart_pause`、`smart_resume`、`smart_retry`、`smart_cancel`、`smart_export`、`smart_run`。
- 托管快照事务：`repo_list`、`repo_transactions`、`repo_status`、`repo_init`、`repo_begin`、`repo_put`、`repo_mkdir`、`repo_remove`、`repo_move`、`repo_commit`、`repo_abort`、`repo_get`。
- 标签及文件版本：`metadata_capture`、`metadata_push`、`metadata_pull`、`versions_list`、`versions_save`、`versions_restore`、`versions_prune`。

只读资源 `rdrive://status`、`rdrive://profiles`、`rdrive://domains`、`rdrive://cache` 提供对应 JSON 状态。它们不是远端任意文件读取接口。

MCP 传输不改变原有能力边界：普通 WebDAV 的覆盖保护依赖可靠修订，普通多文件同步不具备跨文件原子性；跨崩溃的目录快照发布使用托管仓库。错误时需检查 `isError` 及返回报告，不能仅因为生成了计划、入队、注册了域就报告同步或挂载完成。远端文本和文件名应作为用户数据处理。

## 配套 Skills

仓库提供三个独立技能：

- `rdrive-files`：浏览、下载上传、原生挂载、续传和队列。
- `rdrive-smart-sync`：同步计划与冲突、固定缓存容量、Smart 离线保留与恢复。
- `rdrive-transactions`：托管快照、多文件提交和中断恢复。

它们既可使用 MCP 工具，也可使用已有 CLI。安装方式：

```sh
python3 scripts/install-skills.py --dry-run
python3 scripts/install-skills.py
```

默认安装到 `$CODEX_HOME/skills`，未设置 `CODEX_HOME` 时使用 `~/.codex/skills`。已有同名目录不会被覆盖；其他支持 SKILL.md 的客户端可用 `--target` 指定其技能目录。安装技能不等于注册 MCP 服务。重启或重新加载客户端后，确认工具列表及技能发现情况。

## 验证

```sh
cargo test --locked
python3 tests/mcp_workflows.py ./target/debug/rdrive
```

MCP 黑盒工作流使用临时状态目录和本机认证 WebDAV 测试服务，不使用真实云盘文件。既有 SMB、同步和崩溃恢复验证仍见 [验证记录](VALIDATION.md)；新增 MCP 测试不应被解释为对所有真实服务器或客户端的兼容性认证。

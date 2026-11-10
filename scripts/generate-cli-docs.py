#!/usr/bin/env python3
"""Generate the complete CLI reference using only read-only --help/--version.

Build the CLI first, then run:
    python3 scripts/generate-cli-docs.py --binary target/debug/rdrive
    python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --check

No server, profile, credential, state directory or third-party Python package is
needed. The command tree is discovered from Clap's public Commands sections.
"""

import argparse
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "docs/reference/CLI.md"

PRELUDE = """# CLI 完整参考

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

"""


def invoke(binary, arguments):
    environment = dict(os.environ, NO_COLOR="1", CLICOLOR="0", TERM="dumb", COLUMNS="100")
    completed = subprocess.run(
        [str(binary), *arguments], cwd=ROOT, env=environment,
        text=True, capture_output=True, timeout=20, check=False,
    )
    if completed.returncode:
        raise RuntimeError(
            f"{' '.join(arguments)} failed ({completed.returncode}): {completed.stderr.strip()}"
        )
    if completed.stderr.strip():
        raise RuntimeError(f"unexpected stderr from {' '.join(arguments)}: {completed.stderr}")
    return "\n".join(line.rstrip() for line in completed.stdout.replace("\r\n", "\n").splitlines()).rstrip() + "\n"


def children(help_text):
    commands = []
    in_commands = False
    for line in help_text.splitlines():
        if line == "Commands:":
            in_commands = True
            continue
        if in_commands and line and not line.startswith(" "):
            break
        if in_commands:
            match = re.match(r"^  ([a-z][a-z0-9-]*)(?:\s|$)", line)
            if match and match.group(1) != "help":
                commands.append(match.group(1))
    return commands


def collect(binary):
    records = []

    def visit(path):
        help_text = invoke(binary, [*path, "--help"])
        if not re.search(r"^Usage: rdrive(?: |$)", help_text, re.MULTILINE):
            raise RuntimeError(f"missing Clap usage in {' '.join([*path, '--help'])}")
        records.append((path, help_text))
        for command in children(help_text):
            visit([*path, command])

    visit([])
    return records


def render(binary):
    version = invoke(binary, ["--version"]).strip()
    records = collect(binary)
    command_count = len(records) - 1
    leaf_count = sum(not children(help_text) for _, help_text in records)
    option_count = sum(
        len(re.findall(r"^\s+(?:-[a-zA-Z], )?--[a-z][a-z-]*", help_text, re.MULTILINE))
        for _, help_text in records
    )
    output = PRELUDE + (
        f"生成版本：`{version}`。共 **{command_count} 个命令入口**（含命令组，"
        f"不含根入口和自动 `help`）、**{leaf_count} 个可执行叶子命令**。"
        f"下方保留 **{option_count} 条选项声明**（包含各入口继承的全局选项，仅清理帮助文字的行尾空白）。\n\n"
    )
    for path, help_text in records:
        name = " ".join(["rdrive", *path])
        output += f"### `{name}`\n\n```text\n{help_text}```\n\n"
    return output.rstrip() + "\n"


def render_mcp(binary):
    version = invoke(binary, ["--version"]).strip()
    tools = json.loads(invoke(binary, ["--json", "mcp", "tools", "--allow-write"]))["tools"]
    readonly = json.loads(invoke(binary, ["--json", "mcp", "tools"]))["tools"]
    names = [tool["name"] for tool in tools]
    read_names = {tool["name"] for tool in readonly}
    if not tools or len(set(names)) != len(names) or read_names != {tool["name"] for tool in tools if tool["annotations"]["readOnlyHint"]}:
        raise ValueError("inconsistent MCP tool catalogs")
    output = ("# MCP 工具与完整参数\n\n"
              "<!-- Generated by scripts/generate-cli-docs.py --mcp. Do not edit schemas by hand. -->\n\n"
              f"生成版本：`{version}`。共 **{len(tools)}** 项工具，其中默认只读模式提供 **{len(read_names)}** 项。\n\n"
              "本页原样保留运行时 JSON Schema。它描述结构约束；工具还执行下列语义检查及所调用 CLI 的业务校验。\n"
              "写工具需要启动服务时显式 `--allow-write`，涉及本地文件的工具还需配置 `--local-root`。\n"
              "工具调用、权限及资源/提示词用法见 [MCP 指南](../MCP.md)；CLI 对应入口见 [命令参考](CLI.md)。\n\n"
              "更新与校验：`python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --mcp [--check]`。\n\n"
              "## 运行时附加约束与默认行为\n\n"
              "- 字符串最多 16384 UTF-8 字节，禁止 NUL、控制字符和开头的 `-`；除远端路径外不得为空。`profile` 仅允许 ASCII 字母、数字、`-`、`_`。\n"
              "- 远端路径相对于连接或域，禁止绝对路径和 `..`。可选 `remote` / `root` 默认空字符串；具体写入、删除及文件命令仍拒绝不适用的根操作。\n"
              "- 本地路径须绝对且位于显式 `--local-root` 内，禁止符号链接、遍历、RDrive 内部存储及不允许的重叠。缓存迁移另验证私有空目录、所属状态和目录身份。\n"
              "- 数值须为 `0..9007199254740991` 的整数。`depth` 另限 `u32`，索引刷新间隔为 `1..31536000` 秒，`page_size` 为 `1..1000`；版本保留等业务范围另见 CLI / 配置参考。\n"
              "- 规则数组最多 256 项；每项是非空、最多 4096 字节的相对 glob，禁止开头的 `-`、绝对路径、反斜杠、控制字符及 `.` / `..` 段。空 `include` / `exclude` 数组表示清空该列表。\n"
              "- 可选布尔值默认为 `false`；其余未提供的选项采用对应 CLI 默认值或当前持久策略。`cache_config` 在 `max_bytes` 与 `unlimited=true` 中选一；`cache_set_directory` 在 `directory` 与 `default=true` 中选一。\n"
              "- `enable` / `disable`、`exclude_temporary` / `include_temporary`、`enable_versions` / `disable_versions` 不可同时为 `true`；`depth` 与 `all=true` 互斥；`clear_include=true` / `clear_exclude=true` 与对应规则数组互斥。\n"
              "- `smart_resolve.strategy` 为 `keep-both`、`replace`、`remote`；`conflicts` 为 `stop`、`keep-both`、`replace`；搜索 `kind` 为 `file` 或 `directory`。\n"
              "- 所有调用拒绝未知字段。取消和超时终止本机子进程，不能撤销已确认的远端写入；未确认结果按对应传输、回执或事务恢复规则处理。\n\n")
    for tool in tools:
        access = "默认只读模式可用" if tool["name"] in read_names else "需要 --allow-write"
        output += f"## `{tool['name']}`\n\n{tool['description']}\n\n访问：**{access}**。\n\n```json\n{json.dumps(tool['inputSchema'], ensure_ascii=False, indent=2, sort_keys=True)}\n```\n\n"
    return output.rstrip() + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, help="built rdrive executable; defaults to target/debug/rdrive")
    parser.add_argument("--output", type=Path, help="reference Markdown path (CLI.md, or MCP.md with --mcp)")
    parser.add_argument("--mcp", action="store_true", help="generate the complete MCP tool schema reference")
    parser.add_argument("--check", action="store_true", help="fail if the committed reference differs; do not write")
    args = parser.parse_args()
    args.output = args.output or (ROOT / "docs/reference/MCP.md" if args.mcp else OUTPUT)
    label = "MCP" if args.mcp else "CLI"
    binary = (args.binary or ROOT / "target/debug/rdrive").resolve()
    if not binary.is_file():
        parser.error(f"rdrive executable not found: {binary}; build with cargo build --locked or pass --binary")
    try:
        expected = render_mcp(binary) if args.mcp else render(binary)
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        parser.exit(2, f"Unable to read RDrive help: {error}\n")
    if args.check:
        actual = args.output.read_text(encoding="utf-8") if args.output.exists() else ""
        if actual != expected:
            difference = list(difflib.unified_diff(actual.splitlines(), expected.splitlines(), fromfile=str(args.output), tofile="generated"))
            print(f"{label} reference is out of date; regenerate with the same --binary.", file=sys.stderr)
            print("\n".join(difference[:100]), file=sys.stderr)
            return 1
        print(f"{label} reference matches {invoke(binary, ['--version']).strip()}.")
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(expected, encoding="utf-8")
        print(f"Generated {args.output}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

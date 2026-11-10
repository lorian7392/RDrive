# 配置完整参考

本页按 `src/config.rs`、`src/filtering.rs`、`src/policies.rs` 的实际序列化字段与默认值说明。
命令参数见 [CLI 完整参考](CLI.md)，使用流程见 [快速开始](../QUICKSTART.md)，
恢复与协议边界见 [功能说明](../FEATURES.md)。

## 状态目录与配置保存

路径选择顺序为全局 `--home PATH`、环境变量 `RDRIVE_HOME`、
`$HOME/Library/Application Support/rdrive`。`--home` 不会改变 shell 的 `HOME`。
配置文件是状态目录下的 `config.json`，UTF-8 JSON，无注释。文件不存在时使用完整默认配置；
已有 JSON 必须包含 `profiles` 对象，其余四个顶层对象可省略并采用默认值。

优先使用 `profile`、`cache`、`index`、`filter`、`policy` 命令保存配置。手动编辑前停止使用
同一状态目录的 RDrive 进程，保留原文件，并保持状态目录私有且归当前用户所有（`0700`）。
程序使用锁防止普通并发写入，配置以临时文件与重命名方式提交。
不要把状态目录放在普通同步根、内容缓存或 `~/Library/CloudStorage` 内。
配置文件不是全部恢复状态；单独复制配置不能恢复队列、离线修改与同步基线。

## 顶层 Config 字段

- `profiles`：连接名到 `Profile` 的对象，空配置为 `{}`。已有配置文件中必填。
- `cache`：`CachePolicy`，默认 `{"max_bytes":null,"directory":null}`。
- `index`：`IndexPolicy`，默认启用完整元数据预建，刷新间隔 `300` / `30` 秒。
- `filters`：全局过滤与连接覆盖，默认排除常见临时文件，无自定义包含/排除规则。
- `policies`：自动版本与冲突处理，默认自动版本关闭，冲突停止。

这些对象使用下面列出的字段；传输限速、并发数、MCP 本地范围不是 `config.json` 字段。

## profiles：连接 Profile

- `protocol`：必填字符串，`"webdav"` 或 `"smb"`，小写。
- `url`：必填字符串，作为该连接的远端根。
  WebDAV 使用 `https://host/path/`；只有 `allow_http: true` 时才允许 `http://`。
  SMB 使用 `smb://server/share[/folder]`，共享路径不能为空。
  两种协议都要求主机，不允许 URL 用户名、密码、查询参数或片段。
- `username`：字符串或 `null`，默认 `null`。没有用户名的 WebDAV 可匿名访问。
  SMB 的实际认证能力由服务端与 Rust 客户端协商决定，不提供 Kerberos/SSO 开关。
- `password_env`：字符串或 `null`，默认 `null`。字符串是**环境变量名**，不是密码。
  `profile add --password-env` 只接受非空 ASCII 字母、数字、`_` 组成的变量名。
  配置此字段后，变量缺失直接报错，不会回退到钥匙串。
- `allow_http`：布尔，默认 `false`。只改变 WebDAV HTTP 的显式许可，不禁用 HTTPS 证书验证。
- `timeout_secs`：正整数秒，默认 `120`。WebDAV 总请求超时使用此值，连接超时取
  `min(timeout_secs, 30)`。SMB 也使用连接配置的超时。原生 macOS 挂载由系统控制超时。
- `smb_encryption`：布尔，默认 `true`。Rust SMB 客户端要求加密；
  `profile add --allow-unencrypted-smb` 将其保存为 `false`。
- `nextcloud_upload_url`：字符串或 `null`，默认 `null`。仅用于显式启用 Nextcloud 分块续传，
  例如 `https://cloud.example.com/remote.php/dav/uploads/account/`。
  必须是 WebDAV，且与 `url` 同 origin（协议、主机与有效端口一致），
  不允许内嵌凭据、查询或片段。设置它不会把所有上传自动改成续传，仍需 `put --resume`。

CLI 的对应名称为 `--protocol`、`--url`、`--username`、`--password-env`、
`--allow-http`、`--timeout`、`--allow-unencrypted-smb`、`--nextcloud-upload-url`。
连接名不能为空、不能以 `-` 开头，只接受 ASCII 字母、数字、`-`、`_`。
同名连接不能直接覆盖；删除前要完成/取消未完成上传并处理该连接缓存。
更换账户或服务器不能把旧缓存、域与上传回执自动当成新账户的数据，内部绑定会核验身份。

## 认证与环境变量

Rust 文件操作的密码查找顺序：

1. `password_env` 非空时读取指定环境变量，缺失即失败。
2. 未指定时，在 macOS 用 `/usr/bin/security` 读取通用密码；
   service 为 `rdrive:<连接名>`，account 为 `username`（无用户名时为空）。
3. 未找到条目且没有用户名时，使用空密码；有用户名时，报告未找到凭据。

程序不接受密码命令行参数，不将密码写入连接配置。`profile list` 会显示用户名与环境变量名。
后台 LaunchAgent 不继承当前终端临时导出的密码变量，持久后台连接宜使用钥匙串；
`service status` 只报告变量在当前检查进程是否存在，不会显示其值。

原生 SMB `mount` 使用 macOS 已有认证或系统终端提示，不自动使用 `password_env`。
原生 WebDAV 可通过匿名临时文件描述符向系统 helper 交付连接凭据；具体规则见 [挂载](../MOUNT.md)。

运行期环境变量包括：

- `RDRIVE_HOME`：默认状态路径，被 `--home` 覆盖。
- `HOME`：未设置 `RDRIVE_HOME` 时的默认状态目录，以及用户 LaunchAgents 的基础路径。
- 连接指定的 `password_env` 名称：只有对应连接需要凭据时才读取。
- `EDITOR`：`edit` 使用的单个可执行文件，默认 `/usr/bin/vi`，不解析附加参数或 shell 命令。
- `RDRIVE_WEBDAV_LOCK_TOKEN`：已取得的 WebDAV 锁 token，用于修改及 `unlock`。
  token 不通过 CLI 参数传入，不自动续约。
- `HTTP_PROXY` / `http_proxy`、`HTTPS_PROXY` / `https_proxy`、`ALL_PROXY` / `all_proxy`、
  `NO_PROXY` / `no_proxy`：WebDAV HTTP 依赖栈的代理设置，详见下一节。

测试代码中的 `RDRIVE_TEST_SMB_URL`、`RDRIVE_TEST_DOCKER`、`RDRIVE_TEST_SMB_CONTAINER`
只用于隔离测试 fixture，普通 RDrive 操作不会读取它们。构建工具与系统 helper
自身还可能使用标准工具链环境，不能把这些环境当成 RDrive 配置字段。

## 代理、TLS 与协议能力

当前依赖启用了 reqwest `system-proxy`。非回环 WebDAV 目标使用底层库的环境变量及系统代理发现；
`localhost`（忽略大小写）和字面 IPv4/IPv6 回环地址强制直连。私网地址不因“位于局域网”而自动绕过代理。
按域名解析后才得到回环地址的其他主机名不在此显式检查范围内。

依赖中的环境变量读取先检查大写，再检查小写；HTTP / HTTPS 专用设置优先于 `ALL_PROXY`。
`NO_PROXY` 是逗号分隔的绕过列表，可包含主机、域名与 IP/CIDR，`*` 表示全部绕过。
底层库在 CGI 环境（存在 `REQUEST_METHOD`）对 HTTP 代理使用有额外保护。
这不是 RDrive 自建的代理引擎；当前 macOS 代理发现不完整支持系统例外表与 PAC。
需明确直连的目标可通过 `NO_PROXY` 指定。SMB 与 macOS 原生卷不使用 Rust WebDAV 这条 HTTP 代理路径。

HTTPS 证书验证默认启用；没有 `insecure`、自定义 CA、客户端证书、OAuth、Digest 或自定义请求头配置项。
重定向默认拒绝，仅允许实现中同一 WebDAV 资源补末尾 `/` 的有限情况。
普通 WebDAV 没有通用分段 PUT 续传，`nextcloud_upload_url` 只明确选择 Nextcloud 协议。

## cache：CachePolicy

- `max_bytes`：无符号整数字节或 `null`，默认 `null`（不限制清洁内容预算）。
  `0` 是有效的零预算，不等于无限。使用 `cache config --max-bytes N` 或 `--unlimited` 修改。
  `smart setup` 在当前值为 `null` 时将它设为 `5368709120`（5 GiB）；
  这也包括此前显式选择 unlimited 的情况，需要无限时在建域后重新设置。
- `directory`：路径字符串或 `null`，默认 `null`，使用状态目录下的 `cache/`。
  自定义值必须通过 `cache directory PATH` 安全迁移后保存，使用绝对真实私有目录。
  不要直接编辑此字段绕过缓存根绑定、活动读取检查和迁移日志。

预算合计 Rust 清洁缓存、客户端报告的 File Provider 清洁副本与已批准下载预留。
固定、正在读取、未上传内容受保护；持久上传快照、Smart 本地修改、部分下载、索引与恢复备份不属于可逐出内容。
降低预算无法容纳受保护内容时保持原配置并报错。迁移包含固定缓存但不迁移系统 Finder 下载目录、队列和索引。
详细目录条件、迁移中断恢复见 [缓存管理](../CACHE_MANAGEMENT.md)。

## index：IndexPolicy

- `enabled`：布尔，默认 `true`，启用一般后台元数据预建；`false` 仍允许根目录、浏览需求和离线固定目录刷新。
- `prefetch_depth`：`u32` 非负整数或 `null`，默认 `null`，逐步遍历完整树。
  根目录深度为 `0`；扫描某目录也会得到它的直接子项。CLI `--depth N` 设置，`--all` 恢复 `null`。
- `refresh_secs`：整数秒，默认 `300`，范围 `1..=31536000`，一般目录刷新间隔。
- `active_refresh_secs`：整数秒，默认 `30`，相同范围，已浏览或离线固定目录刷新间隔。

这些值是后台元数据调度策略，不下载文件、不授予离线固定，不等于硬实时定时器。
保持 `provider serve` 或管理它的后台服务运行才能执行；`index refresh` 只保存一次性需求。
手动递归刷新可超出一般预建深度。托管仓库按完整根快照刷新，不接受非空子路径刷新。
失败目录退避，不完整列表不会被解释成删除。见 [目录索引](../DIRECTORY_INDEXING.md)。

## filters：FilterConfig 与 ProfileFilter

全局字段：

- `temporary_files`：布尔，默认 `true`，排除 `.DS_Store`、`._*`、`*.tmp`、`*.swp`、
  `*.swo`、`*.part`、`~$*`、`.~lock.*`。
- `include`：字符串数组，默认 `[]`（文件不限包含范围）。
- `exclude`：字符串数组，默认 `[]`（无额外排除）。
- `profiles`：连接名到覆盖对象的映射，默认 `{}`。

每个连接覆盖对象有三个可选字段：`temporary_files` 为布尔或 `null`，
`include`、`exclude` 为字符串数组或 `null`。`null` / 省略表示逐字段继承全局值，
显式 `[]` 表示空列表并覆盖全局列表，列表不会拼接。`filter reset PROFILE` 删除整个覆盖。

glob 区分大小写：无 `/` 的模式匹配任意深度 basename；有 `/` 的模式从域根匹配，
`*` 不跨 `/`、`**` 可跨层；尾 `/` 只匹配目录，排除目录同时排除后代。
排除优先于包含，目录遍历仍可继续寻找允许的文件。每个列表最多 `256` 个模式；
模式不能为空、超过 `4096` 字节、以 `/` 开头、含反斜杠/控制字符或 `.` / `..` 路径段。

规则用于 Smart 列表、后台发现、搜索与新写入目标；不自动删除被排除的远端文件，
不会把排除解释为同步删除，不清除恢复记录与快照。已排队操作可能因规则暂停。
普通显式 `get` / `put`、原生卷及普通 `sync` 不因此改变行为。

## policies：PolicyConfig、VersionPolicy 与 ProfilePolicy

全局字段：

- `versions`：`VersionPolicy`，默认 `{"enabled":false,"keep":5,"include":["*"],"exclude":[]}`。
- `conflicts`：`"stop"`、`"keep-both"` 或 `"replace"`，默认 `"stop"`。
- `profiles`：连接名到 `ProfilePolicy` 的映射，默认 `{}`。

`VersionPolicy` 的全部字段：

- `enabled`：布尔，默认 `false`，启用 Smart 正文覆盖前的自动远端版本保存。
- `keep`：整数，默认 `5`，范围 `1..=100000`。
- `include`：字符串数组，默认 `["*"]`，没有匹配项时不自动保存。
- `exclude`：字符串数组，默认 `[]`，匹配即排除，优先于 include。

两个版本模式列表合计最多 `512` 项，每项非空、最多 `4096` 字节、无控制字符，
使用与过滤器相同的相对 glob 语法。自动裁剪只发生在新内容已确认发布后，
未确认发布的其他写者备份受保护，因此实际保留数可暂时超过 `keep`。
重命名不自动迁移旧路径的版本历史。

每个 `ProfilePolicy` 有 `versions`（完整 `VersionPolicy` 或 `null`）与 `conflicts`
（策略字符串或 `null`）。省略 / `null` 继承全局值；`versions` 对象整体覆盖，
对象内省略字段采用 `VersionPolicy` 自身默认值，不是逐项继承全局版本策略。
连接覆盖名长度 `1..=120` 字节且不可含控制字符。`policy reset PROFILE` 恢复全局继承。

`stop` 保留冲突等待处理；`keep-both` 使用稳定冲突文件名及只创建条件保留两份；
`replace` 先归档观察到的远端修订，再按该修订条件替换。决定后远端再次改变仍会停止。
`smart resolve --strategy remote` 是单条冲突决定，保留本地恢复快照，
**不是**这里允许的持久自动策略值。策略针对 Smart 正文冲突，不把普通 `sync` 改成自动覆盖。

## 无凭据的完整 JSON 示例

下面是可解析的 `config.json` 示例：保留所有默认字段并展示 WebDAV、SMB 和连接覆盖。
`example.invalid` 是占位地址；示例不含密码、token、环境变量值或真实账户。
使用时通过 CLI 配置自己的连接。`directory: null` 不需要手动迁移。

```json
{
  "profiles": {
    "dav": {
      "protocol": "webdav",
      "url": "https://example.invalid/files/",
      "username": null,
      "password_env": null,
      "allow_http": false,
      "timeout_secs": 120,
      "smb_encryption": true,
      "nextcloud_upload_url": null
    },
    "nas": {
      "protocol": "smb",
      "url": "smb://example.invalid/share/",
      "username": null,
      "password_env": null,
      "allow_http": false,
      "timeout_secs": 120,
      "smb_encryption": true,
      "nextcloud_upload_url": null
    }
  },
  "cache": {
    "max_bytes": null,
    "directory": null
  },
  "index": {
    "enabled": true,
    "prefetch_depth": null,
    "refresh_secs": 300,
    "active_refresh_secs": 30
  },
  "filters": {
    "temporary_files": true,
    "include": [],
    "exclude": [],
    "profiles": {
      "dav": {
        "temporary_files": null,
        "include": ["*.txt", "docs/**/*.md"],
        "exclude": ["private/"]
      }
    }
  },
  "policies": {
    "versions": {
      "enabled": false,
      "keep": 5,
      "include": ["*"],
      "exclude": []
    },
    "conflicts": "stop",
    "profiles": {
      "dav": {
        "versions": null,
        "conflicts": null
      }
    }
  }
}
```

最小空配置为 `{"profiles":{}}`，其余对象会采用上述默认值。

## 域、后台服务及会话参数

这些数据由专门命令维护，不嵌入 `config.json`：

`provider-domains.json` 保存域数组。每个 Domain 包含 `id`、`profile`、`remote`、
`display_name`，以及默认 `false` 的 `repository` / `smart`、默认 `null` 的 `local_root`。
`remote` 相对于连接根，`repository` 必须指向已初始化仓库；`local_root` 是兼容 Finder 映射的绝对真实本地路径。
`smart setup` 保存 `smart: true`，display name 默认域 ID，local root 为 `null`。
没有域文件时，`provider domains` 从连接派生普通域；保存显式域配置后使用该数组。
移除本地域配置不等于注销系统 File Provider、回收系统下载副本或清空恢复快照。

`service install` 保存 `ServiceOptions`：`home`、当前可执行文件 `executable`、
`ipc_root`（默认状态目录下 `service/ipc`）、可选 `app`（默认无）、
`development_tcp`（默认 `false`）、可选 `launch_agents_dir`
（默认 `~/Library/LaunchAgents`）。服务运行于 macOS 用户 launchd，
IPC 路径及 App 必须是匹配的真实私有路径；Unix socket 完整路径需短于实现的 `104` 字节限制。
安装不自动启动/注册域，卸载保留用户数据。不要手动改生成的安装记录与 plist。

`--limit`、`--progress`、`--jobs` 只作用于当前 CLI / MCP 会话，不保存到 Config；
`mcp serve` 的 `--allow-write`、`--local-root`、`--timeout` 同样是启动参数。
`smart run --staging` 默认状态目录下的 `SmartTransfers/`。
元数据选择策略当前固定为 CLI 内建安全范围，`MetadataPolicy` 不是用户配置入口。
索引数据库、上传收据、检查点、cache marker 与迁移日志是恢复状态，不是供手改的配置项。

## 辅助脚本的全部参数

这些脚本的 `--help` 由 Python argparse 提供。它们不增加生产 CLI 参数。
原生伴随程序的全部透传控制参数见 [CLI 参考](CLI.md#可选原生伴随-app-参数)。

`scripts/mcp-config.py` 只向 stdout 生成客户端配置，不修改客户端设置、不读取密码：

- `--binary PATH`：已有 RDrive 可执行文件。源码构建请明确传 `target/release/rdrive`；
  当前脚本默认查找项目 `dist/rdrive`，源码 Git checkout 不要求包含分发产物。
- `--home PATH`：必填，配置使用的状态目录，可以尚未创建；输出解析后的绝对路径。
- `--local-root PATH`：可重复，必须为已有目录，默认没有本地文件授权根。
- `--allow-write`：生成启用写工具的启动参数，默认不启用。
- `--timeout SECONDS`：正整数，默认 `300`。
- `--format json|toml`：默认 `json`，分别输出 `mcpServers` 或 `[mcp_servers.rdrive]` 配置。
- `-h, --help`：显示脚本帮助。

`scripts/install-skills.py` 复制仓库内三个技能，不覆盖已有同名目录：

- `--target PATH`：目标 skills 目录，默认 `$CODEX_HOME/skills`，未设 `CODEX_HOME` 时为 `~/.codex/skills`。
- `--skill NAME`：可重复选择 `rdrive-files`、`rdrive-smart-sync`、`rdrive-transactions`，默认全部。
- `--dry-run`：只输出安装计划，不创建目录。
- `-h, --help`：显示脚本帮助。安装技能不等于配置/注册 MCP。

`macos/build.py` 构建可选原生 App，不自动注册/启用系统扩展：

- `--identity IDENTITY`：codesign 身份，默认 `-` 为 ad-hoc 开发审阅签名。
- `--team-id TEAM`：开发者团队标识，默认无。
- `--app-group GROUP`：显式 App Group；未给时，有团队 ID 使用 `TEAM.io.rdrive.shared`，
  否则使用 `group.io.rdrive.shared`。真实分发需要与签名/授权匹配。
- `--development-ipc-root PATH`：开发包显式私有 IPC 根，默认无；
  要求专用绝对路径，当前用户所有且 `0700`，禁止直接使用 `/`、用户 home 或 `/private/tmp`。
  设置时构建开发模式，使用窄范围目录例外而非生产 App Group。
- `--cli PATH`：嵌入的已有 Rust CLI，默认 `target/debug/rdrive`。
- `--output PATH`：输出目录，默认 `macos/build`，最终 App 为其下 `RDrive.app`。
- `-h, --help`：显示脚本帮助。构建工具可能创建输出/开发目录并调用 Xcode 工具与 codesign。

`scripts/generate-cli-docs.py` 只读取可执行文件帮助，生成本仓库 CLI 参考：

- `--binary PATH`：默认 `target/debug/rdrive`，须预先构建。
- `--output PATH`：默认 `docs/reference/CLI.md`。
- `--check`：比较实际帮助与文档，漂移时非零退出，不写文件。
- `-h, --help`：显示脚本帮助。

测试脚本、覆盖率及其隔离环境选项另见 [测试指南](../TESTING.md)。

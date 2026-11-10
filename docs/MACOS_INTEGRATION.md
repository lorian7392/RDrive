# macOS 原生集成

当前源码版本为 **RDrive 0.8.0**。可选伴随 App 包含菜单栏宿主和三个 macOS 扩展：replicated File Provider 提供元数据、系统占位文件、按需下载和文件操作；File Provider UI 显示所选项目的同步状态；独立 Finder Sync 扩展为显式映射的普通目录提供菜单。

占位文件由 macOS 创建；应用读取正文时，系统调用 File Provider 下载内容。CLI 文件操作和普通系统 SMB/WebDAV 网络卷与这套集成分别运行。构建、签名核验、扩展启用、注册域和实际 Finder 显示是不同步骤，源码检出不会自动完成这些步骤。

从源码构建和签名选择见[安装指南](INSTALLATION.md#可选构建-macos-伴随-app)，功能区别见[完整功能](FEATURES.md)。本页后半部分保留历史构建与系统观察；其中的版本、运行进程和本机路径描述当时的验证环境，不代表当前机器或新克隆已经部署。

## 构建与签名

Command Line Tools, Swift, and the macOS SDK are sufficient to compile these
targets; the build does not require a full Xcode installation:

```sh
cargo build --release --locked
```

然后选择下节的显式开发 IPC，或使用获授权的签名身份与 App Group 构建。构建结果为 `macos/build/RDrive.app`，包含：

- `Contents/MacOS/RDriveCompanion`: native menu-bar app and domain controller.
- `Contents/PlugIns/RDriveFinder.appex`: Finder Sync extension.
- `Contents/PlugIns/RDriveProvider.appex`: replicated File Provider extension.
- `Contents/PlugIns/RDriveStatus.appex`: native File Provider status action UI.
- `Contents/Resources/rdrive`: the Rust backend executable copied at build time.

The build compiles the actual SDK protocol methods, links each extension through
`NSExtensionMain`, signs the bundles, and runs `codesign --verify --deep --strict`.
It builds in a separate staging directory and replaces the final app only after
verification succeeds. It never installs or activates the app automatically.
`macos/build/build-report.json` records the SDK, architecture, signing mode,
entitlements mode, CLI version, original CLI SHA256 before signing the copied
executable, embedded CLI SHA256 after signing, and whether activation occurred.

默认身份为 ad-hoc 开发签名。具备 App Group 授权的签名构建可以指定 `--identity` 和 `--team-id`；宿主和三个扩展必须使用同一个获授权的 App Group：

```sh
python3 macos/build.py --cli target/release/rdrive \
  --identity 'YOUR_SIGNING_IDENTITY' --team-id YOURTEAMID
```

The default production group is `<TeamID>.io.rdrive.shared`. `--app-group` can
override it. An ad-hoc signature passing local code verification does not imply
App Group authorization, notarization, or distribution readiness.

## 显式开发 IPC

The development Mac accepted ad-hoc extension registration and a synthetic File
Provider domain. However, its container manager rejected access to the protected
App Group because the requesting signature did not identify an authorized team.
The development build option scopes a temporary sandbox file exception to one
explicit, user-owned private directory:

```sh
RDRIVE_DEV_IPC="$(mktemp -d "${TMPDIR%/}/rdrive-native-ipc.XXXXXX")"
chmod 700 "$RDRIVE_DEV_IPC"
python3 macos/build.py --cli target/release/rdrive \
  --development-ipc-root "$RDRIVE_DEV_IPC"
```

保留本次选择的专用目录，后续宿主与桥接必须使用同一路径，不要复用另一实例的 IPC。此模式省略 App Group entitlement，并在各包中嵌入 `RDriveDevelopmentIPCRoot`。扩展保留 App Sandbox，只获得该专用目录的读写例外和网络客户端权限；不添加主目录访问例外，不要求 Full Disk Access，也不启用系统级 File Provider 测试模式或修改安全设置。目录必须仅允许当前所有者访问。

An actual sandboxed provider test showed that the file exception alone does not
permit an outbound Unix socket outside an authorized App Group: `connect`
returned `EPERM`. Therefore explicit development mode uses an authenticated
loopback TCP connection. The service binds only `127.0.0.1` and writes a fresh
private endpoint descriptor with a random IPC nonce. The extension accepts only
that loopback host, checks descriptor ownership/permissions and regular-file
type, and includes the nonce in each request. The descriptor is not shown in the
UI or copied into a request journal. Remote passwords remain in the private Rust
credential flow. Production App Group mode keeps Unix sockets and peer-UID
checks. The current production Unix-socket client uses the 104-byte
`sockaddr_un.sun_path` field; it does not implement Darwin's longer socket address
format.

This is a local development build, not a distributable production entitlement
configuration. The build report identifies it accordingly.

## 控制命令与生命周期

For CLI-only use, run the Rust service and the foreground native watcher in
separate terminals. Use the authorized App Group directory in production, or
the exact private IPC directory embedded in a development build:

```sh
NATIVE_IPC="$RDRIVE_DEV_IPC"
rdrive provider serve --socket "$NATIVE_IPC/bridge.sock" \
  --staging "$NATIVE_IPC/Transfers" --development-tcp
rdrive provider native -- --group-directory "$NATIVE_IPC" --watch-domains
```

`--development-tcp` is only for the explicit development build described above;
production uses its authorized App Group Unix socket. The foreground watcher
uses a Foundation run loop without starting a menu-bar GUI. It polls the local
Smart catalog every three seconds, signals the working set and changed parent
directories, requests downloads for pinned descendants, and refreshes native
cache accounting. This is necessary for reliable updates while macOS has
unloaded an idle provider extension. It rereads the IPC descriptor for every
request, so a restarted broker's fresh nonce is picked up. Ctrl-C or SIGTERM
stops it; an already scheduled OS download may continue. Keep both processes
running for background Smart synchronization. These foreground commands do not
install a login item or LaunchAgent. Optional `rdrive service` supervision is
configured separately; see [Smart 操作与管理](SMART_OPERATIONS.md).

The optional menu-bar app starts its own bridge subprocess and reuses the same
watcher. Quitting stops only that owned service. Existing profile and credential
storage remains private to Rust. The shared IPC directory contains transfer
staging files, cache accounting and watched-root metadata, not remote passwords
or credential configuration.

The native controller is:

```sh
APP=macos/build/RDrive.app
CONTROL="$APP/Contents/MacOS/RDriveCompanion"
"$CONTROL" --diagnose
"$CONTROL" --list-domains
"$CONTROL" --register-domain PROFILE --display-name 'My Storage'
"$CONTROL" --domain-root PROFILE
"$CONTROL" --signal PROFILE
"$CONTROL" --download PROFILE ITEM_ID
"$CONTROL" --evict PROFILE ITEM_ID
"$CONTROL" --pin PROFILE ITEM_ID
"$CONTROL" --unpin PROFILE ITEM_ID
"$CONTROL" --check-cache PROFILE ITEM_ID
"$CONTROL" --trim-cache MAX_TOTAL_BYTES
"$CONTROL" --watch-domains
"$CONTROL" --enable-finder
"$CONTROL" --remove-domain PROFILE
```

Registration requires macOS to know the containing app and its extensions. A
missing registration produces the native `providerNotFound` error; it does not
by itself prove a signing problem. `--enable-finder` opens Apple's extension
management interface. It does not assert that Finder has enabled the extension.
`--download` acknowledges a scheduled system download; completion of hydration
must be checked by reading the item or its materialization state. `--evict`
finishes after the operating system has attempted eviction. Dirty or pinned
items can reject eviction.

`--remove-domain` uses Apple's preserve-downloaded-user-data mode, so macOS may
return a preserved directory. It does not silently delete downloaded user data.
Control requests return JSON and time out if the system service does not reply.

`--group-directory PATH` is an explicit host development override. Sandboxed
extensions use only the path embedded in their reviewed bundle or their
authorized App Group. Passing a different host override does not grant an
extension access to that directory.

## Finder 动作与可用性

The native File Provider menu provides Show Sync Status, Keep Offline, Allow
Automatic Removal and Download Now; macOS also provides Remove Download. These
menus and the pinned badge/label were observed in the historical system runs
recorded below. The separate Finder Sync menu has all five commands and was
observed in an ordinary mapped synthetic folder in those runs. Custom Finder Sync badge rendering in ordinary folders
remains unverified. File Provider and Finder Sync are separate mechanisms.

Native item/domain IDs come from public File Provider URL/identifier APIs.
File Provider UI can receive an OS-private document ID rather than the backend
item ID; the status action resolves it through the user-visible URL and verifies
the returned domain before querying status. It does not parse document IDs or
guess remote paths. Ordinary local or mounted paths require an explicit Rust
root mapping. No unrelated local directory is silently uploaded.

Keep Offline persists a pin through the Rust service. The provider exposes the
native `downloadEagerlyAndKeepDownloaded` content policy, which asks macOS to
download the item and retain it. Folder pins apply to known descendants and
new descendants discovered later. On macOS versions supporting namespace policy,
pinned directories also ask the OS to materialize their namespace eagerly.
Unpinning restores inherited/lazy policy. Download Now and Remove Download use
native download/eviction APIs. A native network mount is not an offline store:
Finder Sync refuses pin/unpin/download/evict for an ordinary mapped path without
actual File Provider provenance and directs the user to the Smart Finder view.
It can still query the mapped item's status.

File Provider items expose online, cached, pinned, syncing, queued, conflict and
error decorations/labels. macOS owns the standard cloud/download indicators.
Dirty state comes from the durable Rust mutation queue; neither a kernel-mounted
file's existence nor legacy OS upload flags prove an offline copy is available.
Finder Sync separately queries visible items and suppresses duplicate in-flight
badge requests. Its custom badge visibility depends on native Finder behavior
and is not inferred from successful compilation or menu rendering.

The CLI watcher or optional companion watches registered Smart File Provider
domains. An ordinary local or mounted root additionally requires watched-root
metadata and a matching `provider domain-add ... --local-root PATH` mapping.
That mapping alone does not start a folder synchronization loop or turn a raw
SMB/WebDAV mount into a placeholder-based Smart view.

## 清洁缓存联合预算

The configured budget covers completed clean Rust cache payloads and their
reservations plus reported native materialized payloads and native reservations.
A native fetch uses OS materialization as its persistent cache and does not keep
a second completed Rust copy. Pending edits, durable upload/recovery snapshots
and temporary transfer copies are reported separately and excluded. Consequently
this is a managed clean-payload limit, not a filesystem partition hard cap;
temporary copies, filesystem allocation and protected dirty content can use
additional space.

Before fetching, a cross-process locked coordinator scans registered Smart
domains through the native materialized-set enumerator and user-visible URLs.
It checks `SF_DATALESS` on regular files and counts physical materialized clean
logical bytes. An unavailable domain manager or failed scan is an error, never
used-zero. It evicts the least recently used eligible native items through
`NSFileProviderManager.evictItem`, then obtains a combined atomic reservation.
It never truncates or unlinks user-visible content. Pins, pending edits,
conflicts, active/error states and native refusal protect items from eviction.

Unique reservation owners prevent an old release from deleting a newer lease.
An actual fetch consumes a matching pin preflight; concurrent scheduling does
not duplicate the reservation. Growing versions reserve the additional final
size over an existing resident. Active leases renew every 30 seconds, expire
after 120 seconds without renewal, and cancel on failed renewal. Reported used
bytes never expire merely because the broker or watcher stopped. They clear
only after a verified scan/eviction or confirmed domain removal.

Finite-budget folder pinning preflights the known subtree and refuses the policy
change when it cannot fit. A newly discovered pinned child is admitted using the
same fetch budget and may report insufficient quota rather than exceed it.
Oversized files fail before a content GET. Dirty or pinned data may prevent a
requested trim or a lower limit.

For an explicit lower limit, prune eligible Rust cache first, trim eligible
native materializations, then change the configured limit:

```sh
rdrive cache prune --max-bytes MAX_TOTAL_BYTES
"$CONTROL" --trim-cache MAX_TOTAL_BYTES
rdrive cache config --max-bytes MAX_TOTAL_BYTES
```

Review each result. Protected data can make these commands refuse the requested
target; a failed lowering does not silently change the configured setting.

## File Provider 行为

Metadata enumeration and content transfer are separate. Directory enumeration
returns real remote item names, sizes, versions, parents, and capabilities.
Stable provider IDs come from the Rust journal rather than from mutable paths.
Enumeration pages reuse one metadata snapshot; working-set changes deliver
updated items, deletion IDs, and durable anchors. A failed network listing is an
error, never an empty directory. Invalidated enumerators stop emitting results.

When macOS requests contents, the Rust service downloads actual bytes into the
private transfer directory. The extension checks that the result is a regular
file inside that directory, copies it to the File Provider manager's same-volume
temporary directory, and hands ownership to the system. Cancelled deliveries are
cleaned up. Old extension-owned temporary deliveries can be cleaned after a
crash. The native framework handles user-visible dataless files and hydration.

Create, modify, and delete callbacks call the real Rust backend. Edits include
base content/metadata versions; creates include Apple's stable replay identifier.
An unsupported or conflicting mutation reports an error and does not acknowledge
an unconfirmed remote operation. Capabilities omit unsafe operations. Ordinary
WebDAV/SMB profiles depend on their atomic revision guards; managed repository
domains use their manifest transaction model. Symbolic links and unconfirmed
dataless reimports are refused. A remote trash is not implemented.

Finder tag data travels as optional `tag_data_hex` metadata. Native callbacks
can supply Apple's abstract keyed archive instead of the on-disk Finder xattr
format. The Swift layer securely decodes the known version-1 name/color schema
and converts it to a binary plist of Finder name/color strings before sending
it to Rust. Existing raw plist bytes pass through unchanged. Unsupported
archives, invalid names/colors, and payloads above 64 KiB report errors; they are
not silently written as an invalid xattr. A missing/empty callback value clears
the tags. This conversion is applied to both create and modify callbacks.

Native extended attributes are restricted to selected safe `user.*` and
`com.apple.metadata:*` names ending in `#S`, with a combined 32 KiB bound. Finder
tags use the dedicated tag field. FinderInfo, quarantine, security attributes,
and resource forks are not exposed through this native attribute dictionary;
unexposed attributes are preserved by metadata-only updates. This does not
claim that every filesystem attribute is supported by File Provider.

A pin is a native
content policy, separate from file bytes. Socket cancellation interrupts pending
requests, and progress frames update `NSProgress`. The Rust side preserves the
record of already confirmed operations when a caller disconnects. The OS retains
pending local changes when the provider reports errors and retries according to
File Provider's synchronization rules.

## 桥接协议

Each Unix-socket connection sends one newline-delimited JSON request containing
`v: 1`, `request_id`, `op`, and operation fields. Replies are `{ok:true,...}` or
`{ok:false,error:{code,message}}`. Optional progress frames precede the final
reply: `{progress:{completed,total}}`. The Rust service validates peer UID,
socket/staging permissions, domain scope, revision guards, and local paths.
The explicit development TCP variant uses the same framing, adds `auth_token`,
and validates the fresh local IPC nonce before handling a request.

Operations include `domains`, `resolve`, `item`, `list`, `changes`, `fetch`, `create`,
`modify`, `delete`, `pin`, `status`, `cache_budget`, and `native_error`. Items contain `id`, `parent_id`, `name`,
`kind`, `size`, optional `modified`, `content_version`, `metadata_version`,
`pinned`, `capabilities`, optional `tag_data_hex`, and `extended_attributes`
(name-to-hex dictionary), plus `smart`, `sync_state`, `sync_message`,
`has_unsynced_changes` and `native_materialized`. Cached-version observations
are version/domain scoped, chunked at 512 items, and do not replace physical
byte accounting. Native versions are bounded to 128 UTF-8 bytes. The special
wire IDs are `root` and `working_set`.

## 当前测试方式与适用边界

原生宿主测试使用真实 SDK 数据对象与回调接口，并在测试进程内替换域管理、通知、界面和进程边界；所有生产 Swift 源都保留在覆盖率范围内。测试使用私有临时 Unix/TCP 服务，不注册真实域、不显示窗口、不启动应用事件循环，也不请求真实内容驱逐：

```sh
python3 macos/tests/run.py
python3 macos/tests/run.py --coverage --output-dir target/native-tests-coverage
```

第二条命令输出 `coverage.lcov`、`coverage.json`、`coverage-manifest.json` 和 `coverage-summary.json`。manifest 记录十个生产源的 SHA-256；覆盖率按 LCOV 唯一 `(文件, 行)` 计算，不能直接把泛型/闭包重复计数的 LLVM summary 当作物理行数。完整口径与逐语言验收命令见[测试说明](TESTING.md)。

这些测试证明客户端逻辑、参数和错误处理按预期调用接口，不证明系统已接受某个签名包或 Finder 已实际显示菜单、图标和占位文件。`tests/provider_native_workflows.py`、`macos/tests/activation_probe.py` 会接触真实扩展和域；仅在独立测试账户或专用环境、明确待测 App 与 IPC 路径后执行。历史命令中的测试路径不能直接套用到正在运行的个人域。

## 历史构建与系统观察

以下保留对应版本当时的验证事实、产物路径和限制。测试组数不是当前源码的覆盖率；“保持运行”“已经升级”等状态只属于对应记录，不代表当前仍在运行，也不是首次安装步骤。历史 JSON/版本验证文件保持独立。

<details>
<summary>展开 0.4.0–0.8.0 的历史记录</summary>

### 0.4.0/build6

The 0.4.0/build6 development app embeds `rdrive 0.4.0`. Its original
Rust executable SHA256 is
`e51818bb29b81e638bdaecadd48aed99fa1375c08ce6b2e8eca7ab2ddb1f5215`.
The embedded executable is re-signed, so its SHA256 differs; both hashes are
recorded in `macos/build/build-report.json`. Swift 6.4.2 and the selected macOS
27.2 SDK compiled an Apple Silicon macOS 13+ target. All three extensions and
the host passed deep, strict code-signature verification. This remains an
ad-hoc local development build, without notarized production distribution.

```sh
python3 macos/tests/run.py
python3 tests/provider_native_workflows.py --binary target/release/rdrive \
  --app macos/build/RDrive.app --ipc /private/tmp/rdrive-native-ipc-501 \
  --smart --cache-bytes 1048576 --extended-smart \
  --output macos/build/provider-native-smart-report.json
```

The native host suite passed 13 groups across Unix and authenticated development
TCP transport. It covers strict archive/raw Finder-tag conversion,
metadata/policy/attribute bounds, fragmented replies, progress/cancellation,
actual SDK enumeration observers, paging, snapshot anchors, parent-scoped
changes, deletions, invalidation and regular-file staging.

The final 0.4.0 app and release CLI passed eleven actual OS workflows against an
authenticated disposable loopback WebDAV fixture, with the foreground CLI
watcher and no GUI host:

1. Finder enumeration created real `SF_DATALESS` files with zero content GETs.
2. Opening a placeholder hydrated its exact bytes through Swift and Rust.
3. Native eviction restored the dataless state.
4. Keep Offline eagerly hydrated content and native eviction returned the
   specific non-evictable error.
5. Unpinning permitted eviction and another open re-downloaded content.
6. Finder tags and safe syncable attribute set/remove reached metadata sidecars
   without losing tags.
7. Native create, revision-guarded modification and delete reached the server.
8. An exact file-size cache limit admitted content through native LRU eviction,
   protected pins, rejected oversized content before GET, and reclaimed combined
   usage to zero after unpin/evict.
9. Finite-budget folder pinning hydrated known children and a new child without
   opening it. A same-size remote replacement also downloaded automatically;
   reading the updated bytes needed no additional GET.
10. The native catalog stayed available offline; an offline edit survived a
    broker restart and replayed automatically after reconnection.
11. A materialized file stayed readable with the broker stopped.

`macos/build/provider-native-smart-report.json` records the successful final run,
including the same-size update assertion and zero combined usage after reclaim.
Append `--hold-for-ui --hold-seconds 1200` for a disposable Finder inspection;
press Enter to clean up early. The earlier 0.3.0 eight-workflow report and
0.3.1 rebuild remain historical evidence, separate from this new validation.

Actual Finder observations are recorded in
`macos/build/finder-ui-report-0.4.json`. The Smart File Provider view showed its
context actions, an orange pinned badge and `Kept offline` label. Show Sync
Status successfully displayed `c.txt: Kept offline · Smart Synchronization`.
The ordinary mapped-folder Finder Sync menu also displayed all five commands
after a reversible registration of the final build in a uniquely named
Applications test copy. These observations establish rendering; they do not
isolate which registration/build change corrected the earlier missing menu.
No confidently visible independent Finder Sync custom badge was observed in
ordinary icon view. Do not claim that raw SMB/WebDAV mount badges or menus on
the existing 123pan mount were tested. Native Smart status decorations are
verified separately.

Cleanup restored only RDrive's test extension election/registrations, closed the
synthetic Finder window, removed test domains with preserve-data mode, and
deleted only verified test bytes plus Finder's metadata in those disposable
folders. No owned companion/broker/extension processes, fixture CloudStorage
directories, private test IPC directory, temporary Applications copy, or RDrive
plugin records remained. After unregistering the provider, the native domain
query correctly became unavailable; known test domains had already been
removed. `macos/build/native-cleanup-report-0.4.json` records the observed state.
The user's real 123pan mount, profile and routes were unchanged. Permanent
extension enablement is not claimed.

### 0.4.1 real-service reconnection

The 0.4.1/build7 bundle re-embeds the final Rust CLI. Native Swift behavior is unchanged. The Rust broker now indexes ordinary Smart domains one requested directory at a time, so a large remote tree no longer prevents the first namespace snapshot from appearing.

The user's real 123pan connection was restored and its original native WebDAV volume retained. The additional Smart domain displayed 23 root items, a browsed child directory, Online only labels, dataless cloud buttons and the five custom actions in actual Finder. The final release broker and foreground native watcher remain running as requested. Cache capacity is 5 GiB; no real remote mutation test was performed. Live connection reports and all credentials remain in ignored private runtime state and are excluded from artifacts.

The synthetic cleanup report above describes the completed 0.4 test fixtures. It does not mean this newly requested real domain or its services should be removed. The separate FinderSync badge limitation remains unchanged.

### 0.7 构建验证

0.7.0/build9 已在隔离目录编译并通过 ad-hoc 签名检查，嵌入同一份最终 CLI。未注册或激活该包，现有 Finder 实例仍使用原已安装版本。新命名操作的桥接协议测试通过不等同于 Finder 实机回调验证。详见 [0.7 验证](VALIDATION_0.7.0.md)及[Smart 操作与服务管理](SMART_OPERATIONS.md)。

### 0.8 实际升级

0.8.0/build10 已沿用原域和 IPC 路径切换现有原生包、Rust 桥接与通知助手。核对了扩展注册版本、新进程及原域目录；使用现有凭据完成远端只读根目录验证。0.7 阶段已实际观察 Finder 文件列表和在线云图标。缓存目录保持默认，没有对真实云盘做文件写入/删除测试；原生下载位置仍由 macOS 管理。详见 [0.8 激活记录](NATIVE_ACTIVATION_0.8.0.json)。

</details>

## Apple 接口文档

- [Replicated File Provider extension](https://developer.apple.com/documentation/fileprovider/replicated-file-provider-extension)
- [NSFileProviderReplicatedExtension](https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension)
- [NSFileProviderManager](https://developer.apple.com/documentation/fileprovider/nsfileprovidermanager)
- [Finder Sync programming guide](https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/Finder.html)
- [App Groups entitlement and IPC requirements](https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.security.application-groups)
- Installed SDK headers: `NSFileProviderReplicatedExtension.h`,
  `NSFileProviderItem.h`, `NSFileProviderEnumerating.h`, and `FinderSync.h`.

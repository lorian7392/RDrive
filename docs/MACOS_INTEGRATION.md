# Native macOS integration

The optional companion contains a menu-bar host and three macOS extensions. Replicated File Provider supplies metadata, system placeholders, on-demand downloads, and file actions. File Provider UI displays item synchronization status. A separate Finder Sync extension supplies actions for explicitly mapped ordinary folders.

macOS creates the placeholders and requests content when an application reads them. CLI operations and ordinary SMB/WebDAV network volumes are separate workflows. Building, verifying signatures, enabling extensions, registering domains, and observing Finder are distinct steps; cloning the repository does not perform them.

See [Installation](INSTALLATION.md#optional-native-companion) for build and signing options, and [Features](FEATURES.md) for workflow differences. Historical deployment observations are retained in the repository archive and do not describe a fresh installation.

## Build and signing

Command Line Tools, Swift, and the macOS SDK are sufficient to compile these
targets; the build does not require a full Xcode installation:

```sh
cargo build --release --locked
```

Choose explicit development IPC below, or build with an authorized signing identity and App Group. The output is `macos/build/RDrive.app`, containing:

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

The build probes the selected SDK for `NSFileProviderNamespacePolicy` instead
of assuming that a newer runtime makes the API available to the compiler. It
enables eager namespace materialization only when that SDK supports the API
and the app runs on macOS 27 or later. Older SDKs retain the existing
`NSFileProviderContentPolicy` behavior for pinned files and folders. The build
report and native coverage summary record this capability as
`sdk_namespace_policy`.

The default identity is an ad-hoc development signature. For an authorized App Group build, set `--identity` and `--team-id`; the host and all three extensions must share the same authorized group:

```sh
python3 macos/build.py --cli target/release/rdrive \
  --identity 'YOUR_SIGNING_IDENTITY' --team-id YOURTEAMID
```

The default production group is `<TeamID>.io.rdrive.shared`. `--app-group` can
override it. An ad-hoc signature passing local code verification does not imply
App Group authorization, notarization, or distribution readiness.

## Explicit development IPC

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

Keep this dedicated directory for the matching host and bridge; do not reuse another instance's IPC. This mode omits the App Group entitlement and embeds `RDriveDevelopmentIPCRoot` in each bundle. Extensions retain App Sandbox, with a read/write exception only for that directory and network-client permission. It grants no home-directory exception or Full Disk Access and does not enable system-wide File Provider testing or change security settings. The directory must be private to its owner.

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

## Controls and lifecycle

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
configured separately; see [Smart operations and management](SMART_OPERATIONS.md).

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
"$CONTROL" --retry-sync PROFILE
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

## Finder actions and availability

RDrive adds Show Sync Status, Keep Offline and Allow Automatic Removal to the
File Provider menu. Right-click one Smart folder and choose **Refresh Index** to queue
a refresh of its immediate contents. The menu-bar companion offers **Refresh
Connection Index** for each Smart connection, which recursively refreshes its tree.
This menu is available in the full companion app; the resident notification helper
runs without a menu bar icon. For a managed repository, the command refreshes
its entire snapshot. The
background service applies the refresh and notifies Finder when the catalog
changes. Use Finder's cloud download button or open an online-only
item to download it. Finder offers Remove Download for eligible local copies.
For a conflicted or blocked Smart folder, choose **Repair Sync Issue…**, or open
**Show Sync Status**. The status window offers **Repair Sync Issue** when
RDrive can safely cancel the exact stale request while retaining local contents
and other pending edits. Repair queues a folder refresh. Requests containing
local file content, Finder tags, extended attributes, or a recorded remote
transaction show a review reason and keep their recovery data.
After a confirmed repair, RDrive asks macOS to retry requests held by cached
synchronization or connection errors in this domain. The native
`--retry-sync DOMAIN` command provides the same retry notification after a
connection is restarted. This resets system backoff; every retried operation
keeps its existing revision checks and retained content. A retry notification
does not confirm that the remote connection has recovered.
An item blocked by filtering is labeled **Excluded by sync rules**. A stale-folder
repair explains the selected rule. An existing temporary file under a folder does not
block an unrelated metadata update to that folder; moving or deleting the folder
still checks the affected descendants.
For one Smart file or folder, **View File History…** opens retained versions,
including deleted descendants when a folder is selected. It offers read-only
preview, verified export to a new destination, restore as a copy, and explicitly
confirmed file replacement guarded by the observed current revision. The same
window configures shared retention settings. See [file history and recovery](HISTORY.md)
for recovery steps and optional APFS checkpoints.

The separate Finder Sync menu also includes Download Now and Remove Download.
The historical system runs in the repository archive observed that menu in an
ordinary mapped synthetic folder and observed the File Provider pinned badge
and label. Custom Finder Sync badge rendering in ordinary folders remains
unverified.

Native item/domain IDs come from public File Provider URL/identifier APIs.
File Provider UI can receive an OS-private document ID rather than the backend
item ID; the status action resolves it through the user-visible URL and verifies
the returned domain before querying status. It does not parse document IDs or
guess remote paths. Ordinary local or mounted paths require an explicit Rust
root mapping. No unrelated local directory is silently uploaded.

Keep Offline persists a pin through the Rust service. The provider exposes the
native `downloadEagerlyAndKeepDownloaded` content policy, which asks macOS to
download the item and retain it. Folder pins apply to known descendants and
new descendants discovered later. With a supporting SDK and macOS 27 or later,
pinned directories also ask the OS to materialize their namespace eagerly.
Unpinning restores inherited/lazy policy. The companion's `--download` and
`--evict` commands use the system File Provider manager. A native network mount is not an offline store:
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

## Shared clean-cache budget

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

## File Provider behavior

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
requests, and progress frames update `NSProgress`. File transfers report their
direction, payload bytes, and available speed and time estimates. Preflight and
verification traffic do not advance the file's payload counter. Reaching the
payload size does not mark the operation complete until its final result is
confirmed and a download has been handed to the system.

The companion also publishes file progress for background Smart upload attempts
after resolving the item's real user-visible URL through File Provider. Progress
is removed on failure, cancellation, or a missing attempt; a new attempt gets a
new progress object. Unknown lengths remain indeterminate. Finder controls the
appearance of its progress indicators and the fields it displays; publication
does not add a custom Finder column or guarantee a numeric percentage in every
view.

The Rust side preserves the
record of already confirmed operations when a caller disconnects. The OS retains
pending local changes when the provider reports errors and retries according to
File Provider's synchronization rules.

## Bridge protocol

Each Unix-socket connection sends one newline-delimited JSON request containing
`v: 1`, `request_id`, `op`, and operation fields. Replies are `{ok:true,...}` or
`{ok:false,error:{code,message}}`. Optional progress frames precede the final
reply: `{progress:{completed,total}}`, with optional transfer direction, speed,
and estimated time. A null total means that the payload length is unknown. The
local `transfer_status` request exposes scoped active and recent transfer
attempts to the companion without waiting for a remote operation or index scan.
The Rust service validates peer UID,
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

## Testing and validation boundaries

Native host tests use actual SDK data objects and callbacks, with injected domain-manager, notification, UI, and process boundaries. All production Swift sources remain in coverage scope. Tests use private temporary Unix/TCP peers without registering real domains, displaying windows, running the application event loop, or requesting real eviction:

```sh
python3 macos/tests/run.py
python3 macos/tests/run.py --coverage --output-dir target/native-tests-coverage
```

Coverage mode produces `coverage.lcov`, `coverage.json`, `coverage-manifest.json`, and `coverage-summary.json`. The manifest records SHA-256 values for all ten production sources. Unique LCOV `(file, line)` entries define the line metric; LLVM summaries can double-count generic or closure instantiations. See [Testing](TESTING.md) for the complete scope and per-language gate.

Mock tests validate client logic, parameters, and error handling. They do not establish that macOS accepted a signed bundle or that Finder displayed menus, badges, or placeholders. `tests/provider_native_workflows.py` and `macos/tests/activation_probe.py` access real extensions and domains; use a separate test account or dedicated environment with an explicit App and IPC path. Do not copy historical fixture paths into an active personal domain.

## Historical evidence

The [native integration archive](https://github.com/lorian7392/RDrive/blob/main/docs/archive/MACOS_INTEGRATION_HISTORY.md) preserves the original version-specific observations. Those records are not installation instructions or evidence of a currently running deployment. Structured validation files remain unchanged in the source repository.

## Apple API references

- [Replicated File Provider extensions](https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension)
- [Finder Sync extensions](https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/Finder.html)
- [App Group entitlements](https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.security.application-groups)
- Installed SDK headers: `NSFileProviderReplicatedExtension.h`, `NSFileProviderItem.h`, `NSFileProviderEnumerating.h`, and `FinderSync.h`.

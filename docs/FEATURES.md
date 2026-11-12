# Features, guarantees, and limits

This guide describes the implemented behavior. Use the [CLI reference](reference/CLI.md) for every command, argument, and default; [configuration](reference/CONFIGURATION.md) for persistent settings and environment variables; and [testing](TESTING.md) and [validation records](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION.md) for evidence.

An implemented capability, an isolated passing test, a verified server deployment, and a system-accepted native extension are different results.

## Access modes

**Rust CLI operations** connect directly to WebDAV or SMB. `ls/stat/get/cat/put/mkdir/cp/mv/rm/url/edit` work without FUSE or the companion App. RDrive manages its own cache, queue, baselines, checkpoints, and recovery records.

**Native `mount` network volumes** use macOS SMB/WebDAV helpers. Applications access the system network filesystem with the server's ordinary directory layout. macOS controls authentication, certificates, caching, timeouts, and file I/O. RDrive's `--limit`, clean-cache budget, Smart queue, and ordinary `sync` do not control application I/O on these volumes.

**File Provider / Smart domains** combine the optional companion, system extensions, and Rust bridge. macOS manages real placeholders and downloaded copies. SQLite retains discovered metadata; offline changes have separate persistent snapshots. Configuration, compilation, domain registration, system enablement, and usable Finder behavior require separate steps.

**Managed repositories** use a dedicated storage format accessible through the CLI or File Provider's logical view. Immutable objects and complete snapshots give cooperating clients a complete old or new tree. Ordinary mounts, file commands, and `sync` do not inherit these transactions. Mounting the physical repository directory exposes its internal storage rather than the logical tree.

## Profiles, authentication, routing, and diagnostics

`profile add/list/remove` manages connections. `info` performs an authenticated connection and reports backend capabilities and diagnostics. An existing profile cannot be silently replaced; removal checks pending uploads and cache ownership to prevent redirecting old state to new storage.

Passwords come from an explicit environment variable or macOS Keychain. They are not command arguments or configuration values. WebDAV without a username can connect anonymously. Background processes need their own usable credential source.

WebDAV requires HTTPS and verifies certificates by default. HTTP needs explicit permission. The Rust backend currently supports Basic authentication; Digest, OAuth, client certificates, custom CA configuration, and custom request headers are not provided. URLs reject embedded credentials, query parameters, and fragments. Redirects are rejected except for a limited same-resource trailing-slash case. The default timeout is 120 seconds; connection timeout is capped at 30 seconds while total requests use the configured value.

WebDAV retains environment/system proxy support. `localhost` and literal loopback addresses always connect directly; other private-network targets retain normal proxy routing. macOS proxy discovery does not fully support PAC or exception lists. Use `NO_PROXY` for explicit direct destinations. See [configuration](reference/CONFIGURATION.md).

SMB uses a Rust SMB2/3 client with required encryption by default and an explicit unencrypted compatibility option. Username/password authentication is supported; SMB1, Kerberos/SSO, DFS, domain browsing, and service discovery are not provided. Rust SMB diagnostics and native system mounts are independent implementations.

## Paths and file operations

Remote paths are UTF-8 and relative to the profile URL. The root is `""` and the separator is `/`. Absolute paths, `..`, backslashes, and NUL are rejected; repeated separators and `.` components normalize. Backends also validate decoded server paths and reject escaping children or directory cycles.

`ls --recursive --contains TEXT` walks server directories and filters paths by a substring. It is distinct from offline `index search`. `stat` reports type, length, modification time, and revision.

`get` writes a temporary file beside the destination, verifies remote length/revision, and then publishes locally. It preserves existing files by default. `cat` writes file bytes directly; `--max-bytes` bounds the advertised size before download. `put` retains an independent verified source snapshot and creates a new file unless `--overwrite` is explicit.

`cp` and `mv` also preserve existing destinations. `mkdir --parents` creates ancestors. `rm` rejects the root and requires `--recursive` for a directory.

SMB file upload/copy writes an independent sibling staging file before server-side rename publication. Failure may leave `.rdrive-upload-<UUID>` or `.rdrive-copy-<UUID>`; directory copy can leave a partial destination tree. Ordinary sync excludes these internal names; they are not automatically reclaimed.

Recognized SMB reparse points/symlinks are rejected. WebDAV has no universal remote-link attribute, and server-transparent links may be impossible to identify. Local sync rejects symlinks.

Large payloads stream rather than loading into memory. Hashing, strong SMB revision scans, and synchronization may read content again and add disk/network cost. WebDAV directory XML is limited to 16 MiB; an oversized or incomplete listing is not treated as empty. Original modification times are not universally preserved.

`url` returns a resource address without changing permissions or creating a public/signed link. There is no uniform sharing, file-request, or remote ACL editor.

## Protected editing

`edit PROFILE REMOTE` downloads a file, invokes `EDITOR`, and queues a revision-protected upload after modification. `EDITOR` names one executable, without extra arguments; the default is `/usr/bin/vi`.

WebDAV needs a reliable strong ETag. SMB verifies identity and SHA-256 and uses protected handles. Remote changes stop publication while retaining the local result. An editor failure reports its retained local path. This is not a GUI editor launcher or a multi-file editing transaction.

## Persistent upload queue and history

`put --queue` retains a persistent snapshot and job. Later modification or removal of the original source does not change queued content. `queue list/run/cancel` inspects, executes, or cancels jobs. `status` summarizes local queue/cache state offline. `history --limit N` shows local completion events, with a default of 20; it is not a server audit log.

A job becomes `done` only after server confirmation. Restart changes unconfirmed `running` work to `interrupted`. `queue run` retries pending work and interrupted work that does not overwrite remote content. Failed work needs `--retry-failed`.

An uncertain overwrite also needs `--retry-ambiguous-overwrite`, which may overwrite the path again. A create-only retry conflicts with an existing destination and preserves its snapshot instead of silently becoming an overwrite.

Jobs created without `--resume` retransmit from the beginning. Ordinary queue cancellation removes the local upload snapshot, retains an existing upload checkpoint, and does not remove remote staging/recovery copies. Smart cancellation separately preserves local recovery snapshots. A one-shot CLI command does not leave a background synchronization process.

## Progress, rate limits, and parallel jobs

`--limit N` is a positive shared payload-bytes-per-second budget within one Rust process. It accounts for actual traffic from retry, prefix verification, strong-revision hashing, copy, and post-publication validation. A resumed prefix is not counted again as newly completed logical content.

The budget excludes protocol headers, encryption overhead, and TCP retransmissions. It does not control native network-volume I/O.

`--progress auto` shows a progress bar, speed, and ETA in a terminal and line-delimited JSON when redirected. `json` always emits JSON; `off` disables progress. Progress uses stderr; results use stdout. Events include job and aggregate counters with a two-second speed window, separating payload completion from server-confirmed publication.

`--jobs 1..64`, default 4, controls concurrent remote targets in `queue run`. Jobs for the same profile and target remain ordered; a coordinator saves state. It does not parallelize every step of ordinary sync or managed transactions.

## Resumable downloads and uploads

`get --resume` persists flushed prefix length and SHA-256. Restart verifies the prefix, truncates unrecorded tail bytes, and requests the suffix under a protected remote revision. Remote changes, incorrect range replies, or concurrent local modification stop resumption and preserve data. A local overwrite retains the old file and reports its `previous_local` path.

SMB `put --resume` verifies the private staging file's identity and every uploaded prefix byte before continuing. Repeating the same content and arguments can reuse unfinished work; changed source content creates a new job. Running checkpointed uploads can perform bounded authenticated reconnects. They do not rely on durable write handles or dirty write caching.

Ordinary WebDAV has no generic upload-resume protocol; RDrive does not guess with partial PUT. An explicit same-origin `nextcloud_upload_url` enables Nextcloud chunking. A chunk is skipped only when confirmed, bound to the source hash, and still protected by an unchanged strong validator. Other chunks retransmit.

Final publication protects the destination and is followed by a full SHA-256 read. An ambiguous result is inspected rather than blindly overwritten. Servers may expire sessions. Isolated protocol tests do not establish compatibility with every real Nextcloud deployment.

Download records live in `transfers/`, with partial content beside the target in `.rdrive-partial-<UUID>/`. Upload checkpoints use `spool/<job-id>.upload.json`. `transfers list/discard` manages download records while preserving prior local recovery copies. These files, upload snapshots, and remote staging are not clean cache.

## Clean cache, pinning, and relocation

`cache fetch [--recursive] [--pin]` downloads and verifies content, reusing valid copies. `cache get` is entirely offline and verifies SHA-256 without establishing server freshness. `cache list/status` are local queries; `unpin` removes a pin; `prune --max-bytes` reclaims eligible unpinned content by LRU. Ordinary `get` does not pin content.

`cache config --max-bytes N` persists the combined clean budget; `--unlimited` removes the limit. Accounting includes Rust cache, reported clean File Provider copies, and download reservations. Downloads reserve capacity first. Pins and active readers are protected. An impossible reduction fails without changing the previous limit.

Upload snapshots, unsent edits, temporary transfers, databases, recovery content, and system overhead consume additional space. This is a clean-payload budget, not a partition quota or preallocated disk reservation.

`cache clear` previews by default. `--apply` removes reclaimable Rust objects recorded by the state database; an unknown file is not removed merely because its name looks like a UUID.

`cache directory` reports the path. `PATH [--dry-run]` migrates it; `--default` returns to the default root. Migration copies and verifies each object before committing configuration and cleaning old copies, requiring temporary capacity for both. Recovery checks old/new identity and stops on corruption or unknown files while preserving data.

A custom root must be absolute, real, private, empty, and disjoint from state, sync, and system File Provider storage. An ownership marker binds it to its state/directory identity; separate state directories cannot share it. Migration includes pinned Rust cache but does not move Finder downloads, queues, or indexes.

System copies must be reclaimed through native eviction APIs, which may refuse. Do not remove CloudStorage files manually to imitate cache cleanup. All cache/snapshot content is currently plaintext. See [cache management](CACHE_MANAGEMENT.md).

## Ordinary directory synchronization and watch

`sync` and `watch` require existing roots on both sides. Directions are `both` (default), `push`, and `pull`. The default is preview; `--apply` executes. Direction does not authorize unconditional mirroring.

`watch` runs in the foreground at an interval, default 30 seconds, releasing the state lock between passes. Ctrl-C stops it. SMB notifications prompt checks while periodic scanning remains enabled.

Without a common baseline, absence on one side does not establish deletion. Existing files at one path require a temporary remote download and SHA-256 comparison. Only equal content and stable revisions establish the initial baseline; size and time are insufficient.

Subsequent plans compare local hashes, remote revisions, and the common baseline. Concurrent changes on both sides stop the complete plan. File/directory conflicts, case/Unicode-normalization collisions, symlinks, and failed/incomplete scans also stop it.

Deletion propagation additionally requires `--delete` and a successful baseline. An insufficiently protected remote deletion fails and retains the file. Empty remote directories remain rather than being recursively deleted on an assumption of emptiness. SMB file deletion verifies identity/content and uses a protected handle.

Replaced/deleted local content is retained at `.rdrive-versions/<id>/original` beneath the sync root and excluded from synchronization. This is separate from custom versions stored on the server.

WebDAV overwrite requires a strong ETag. SMB overwrite verifies SHA-256 and identity, holds a deny-write/delete handle, moves the old file to a recovery copy, and publishes the new file with no-overwrite rename. The two renames have a brief path gap; this is not crash-atomic replacement. An unfinished recovery receipt prevents interpreting that gap as a user deletion.

Strong SMB scans read complete contents and may add substantial traffic or stop while another client writes. Ordinary synchronization has no cross-file transaction: a mid-run failure retains the old baseline and prior copies but does not undo confirmed operations. Subsequent runs may require reconciliation. Losing state also loses the common baseline. `--metadata` adds an independent attribute baseline.

## Finder tags and selected extended attributes

`metadata capture/apply/recover/push/pull` and `sync --metadata` handle selected attributes on ordinary files. CLI policy includes Finder tags (`com.apple.metadata:_kMDItemUserTags`), `com.apple.FinderInfo`, safe `com.apple.metadata:*`, and `user.*`. Binary values use hexadecimal encoding and preserve the tag plist.

Limits are 1 MiB per value, 4 MiB total, 1,024 attributes, and 64 MiB for an input record file. Reports identify exclusions. Attributes outside the selection or receiving policy remain untouched. Directory tags, resource forks, ACLs, quarantine, signatures, and system security attributes are outside this scope.

Ordinary WebDAV/SMB uses portable `.rdrive-metadata/` sidecars bound to relative path, strong revision, and content SHA-256. Push verifies equal local/remote content before conditional update. It does not directly edit server-native xattrs, ACLs, or SMB ADS.

Changes on both sides preserve content and report a metadata conflict. Missing reliable revisions or unequal content stop/defer the operation. Content and sidecars confirm separately. Managed repositories instead publish selected metadata in the same snapshot.

Local application records prior values and rolls back after failure; macOS has no atomic multi-xattr syscall. `metadata apply --journal` saves a recovery journal in a dedicated private directory. `metadata recover` verifies file identity and content before restoring values and refuses a replacement file.

File Provider bridges native `tagData` and `extendedAttributes` callbacks. Finder may supply an abstract keyed archive, which the companion validates and converts to a standard tag plist rather than writing the archive directly as an xattr. Native xattrs have a narrower policy: permitted `user.*` / `com.apple.metadata:*` names must end with `#S`, with a 32 KiB combined limit. Tags use a separate field.

Compilation and actual bidirectional Finder tag callbacks are separate validation. See [macOS integration](MACOS_INTEGRATION.md).

## Smart on-demand access and offline recovery

`smart setup` creates an independent domain without converting an ordinary network volume. If the current clean budget is unlimited, setup assigns 5 GiB; an existing numeric limit remains. Reconfigure afterward if unlimited storage is intentional.

`--register` asks a built companion to register the domain. The bridge, notification helper, and enabled system extensions still need matching settings. `provider domain-add` also creates ordinary or managed domains; `provider domains/domain-remove` manages local configuration. Removing local configuration does not unregister a system domain or release all disk usage.

The first directory page is persisted in SQLite before background breadth-first discovery. Browsing an indexed directory does not wait for a complete scan. Content downloads when opened. Materialized system copies work offline; unmaterialized content needs a connection.

Keep Offline pins a directory and descendants, including newly discovered children. Unpinning a parent preserves explicit child pins. Allow Automatic Removal removes pinning but does not promise immediate eviction.

Offline changes retain independent verified snapshots. Reconnect uploads use the original revision conditions and complete receipts only after server confirmation. `smart status/list` inspects work; `pause/resume` controls publication; `run` performs one pass; `retry` marks eligible work without weakening its revision; `cancel` retains recovery copies; `export` writes a real file; and `discard` cleans snapshots only after completion, cancellation, or supersession.

Pause/cancel cannot undo a completed remote operation. Unstarted edits against the same base may coalesce; started or uncertain edits keep their snapshots. Later edits advance conditions only through this client's prior confirmed receipt. External changes stop the replay chain. Repeated saves of a newly created file become protected modifications after initial confirmation.

Confirmed transfer payloads can be reclaimed while receipts, hashes, and conditions remain. A changed request UUID does not repeat a confirmed write.

Smart ordinary directories support file/directory creation, content/tags, rename, move, and recoverable deletion. Moves preserve destinations and update stable IDs, descendant paths, and pin policies. Directory deletion records a manifest, moves the tree to `.rdrive-trash-<UUID>` under protection, and verifies the tree. A concurrent change restores it or preserves a recovery path and reports conflict.

`smart recovery` reports original/backup paths and phases. `space_reclaimed: false` means server space was not freed. This version does not automatically purge remote recovery copies. Combined content/name/tag operations have stepwise receipts; ordinary directories have no cross-request atomicity. See [Smart synchronization](SMART_SYNCHRONIZATION.md) and [management](SMART_OPERATIONS.md).

## Filters, automatic versions, and conflicts

`filter status/config/reset/test` manages global and profile rules. Common temporary files are excluded by default. Rules support repeated include/exclude globs, empty-list clearing, temporary-file switches, and per-field inheritance.

Exclusion wins over inclusion. Directory rules apply to descendants, and matching is case-sensitive. Filters affect Smart listings, discovery/search, and new writes; they do not delete remote data or recovery snapshots or infer deletion. Excluded pending work may pause and continue when allowed again. Explicit CLI access and ordinary sync keep their own rules. See [configuration](reference/CONFIGURATION.md).

`policy status/config/reset` controls Smart automatic versions and content-conflict strategy. Versions are off by default. When enabled, overwrite archives the observed revision first and prunes only after confirmed publication; the default retention is 5. Failed backup or uncertain publication does not prune first. Unconfirmed recovery versions from other writers remain protected, so counts can exceed the limit temporarily. History is path-based and does not follow rename automatically.

The default conflict strategy is `stop`. `keep-both` uses a stable suffix and creates only a new file. `replace` saves the actual remote revision before conditional replacement. Decisions persist before writes; a further remote change still stops publication.

`smart resolve --strategy keep-both|replace|remote` explicitly handles retained-content conflicts. `remote` abandons local publication while preserving the recovery snapshot. Directory, missing-target, and simultaneous move/rename conflicts may need manual handling. These policies do not grant ordinary `sync` automatic-overwrite permission.

## SQLite indexes and offline search

`index config/status/refresh` controls background metadata prebuilding, depth, refresh, and one-shot demands. Defaults are the full tree, 300-second ordinary refresh, and 30-second browsed/pinned refresh; root depth is 0.

Disabling general prebuilding still permits root, browse, and offline demands. A manual recursive request can exceed the configured depth. Requests persist locally and need `provider serve` or the resident service to run. One-shot `info` and ordinary mounts do not start a background worker.

Enumeration reads metadata. SMB's weak metadata token is not an overwrite condition. Workers reuse connections with backoff and fair scheduling. Up to four connections perform metadata reads concurrently; within one connection, a slow request can delay its next scan. Existing SQLite directory reads do not wait for remote networking. Finder’s **Refresh Index** scans a selected Smart folder; the full companion’s **Refresh Connection Index** requests a recursive connection refresh. Native notifications bootstrap large catalogs in bounded pages and isolate connection failures. Complete directory comparison publishes changes transactionally; a failed listing preserves old items. `pending` counts discovered directories not successfully scanned, not currently executing jobs.

`index search DOMAIN QUERY` reads indexed names/paths only. It does not search content or contact the server. Queries of at least three characters use FTS5 trigram; short queries use escaped LIKE. `%` and `_` are literal, and user FTS syntax is not executed.

Search supports scope, type, and pagination with current filters and pending-deletion state. Index changes can affect later pages. Results do not establish that the entire server has been scanned.

Each domain's WAL SQLite database retains transactional state and anchors bound to protocol, account, root, and repository mode. Migration preserves legacy JSON and makes SQLite authoritative. Derived search schemas upgrade atomically. Corrupt, unknown-version, or mismatched state fails without silently clearing receipts.

Back up a live database through a WAL-aware SQLite backup procedure. Do not mix older writers with upgraded state. See [directory indexing](DIRECTORY_INDEXING.md).

## Managed snapshots and multi-file transactions

`repo init/begin/put/mkdir/rm/mv/status/commit/abort/transactions/ls/get` manages a dedicated logical tree. `repo put` retains a verified source snapshot; changing the original later does not change the transaction. A transaction can add, replace, rename, move, or delete multiple logical items and publish stable IDs and selected tags with the complete tree. Ordinary directories are not imported automatically.

`.rdrive-repository/` stores its format, immutable SHA-256 objects, commit slots, and staged manifests. Publication verifies objects/the full manifest, then competes for the next immutable sequence slot through no-overwrite rename. It does not rely on mutable HEAD or a stranded remote lock.

Readers validate parent chains, content binding, UUIDs, directories, and name conflicts. One concurrent writer wins; the other conflicts and retains staging. There is no automatic merge: read the latest tree and start a new transaction.

For an uncertain result, retain the original state and replay `repo commit TX` to inspect that transaction's publication. Do not create duplicate commits. `repo abort` accepts only transactions not yet prepared for publication.

Guarantees require correct same-filesystem atomic no-overwrite rename. They cover cooperating-client snapshot visibility and client-process crash recovery, not malicious servers or persistence through server power loss.

Repository GC, history compaction, branch merging, automatic migration, and automatic conflict merging are not implemented. Old commits, unreachable objects, failed staging, and local snapshots consume space. Reads validate complete history and commits verify referenced objects, so costs grow with history/content. Repository control files are not cache. See [managed repositories](REPOSITORY.md).

## SMB notifications, leases, and reconnects

`notify` uses CHANGE_NOTIFY for create/change/delete/rename hints. `--recursive` requests descendants; `--count` counts non-ready events. Notifications are not a complete journal, and rename hints are not paired transactions.

Overflow, invalid paths, disconnect, and cancellation require rescan/resubscription. A disconnect emits `rescan_required` and ends the subscription. Changes may be lost between subscriptions, so `watch` retains periodic verification.

SMB3 reads request RH leases and durable-v2 handles; the server may decline or adjust timeouts. SMB2 uses ordinary protected opens. The client handles lease breaks/ACKs without dirty write caching or retaining closed application handles.

Read recovery authenticates under the same identity/encryption policy, tries durable reconnect, and, if needed, verifies complete identity plus SHA-256 before reopening. Equal content with a changed identity is still rejected. There are at most 3 read recoveries and 5 bounded-backoff attempts per authentication.

Client restart uses application download checkpoints; protocol durable tokens are not persisted. These features do not provide persistent CA handles, transparent failover, retained durable state after server restart, or power-loss guarantees. Diagnostics distinguish requests, grants, reconnect, verified reopen, and authentication failures. See [SMB lifecycle](SMB_LIFECYCLE.md).

## Local file history and Finder recovery

Optional per-connection history preserves verified before-images before supported replacements and deletions, independently of clean-cache eviction. Retained files and directory trees are account-bound, and recorded item identities and aliases preserve supported rename continuity. Prepared entries remain protected until publication is durably confirmed. Defaults are 20 versions, 30 days, and 5 GiB when enabled; existing configurations default to disabled.

Supported Smart changes, `put` and queued overwrites, explicit deletion, copy/move overwrite destinations, and guarded remote file overwrites/deletions from ordinary `sync`/`watch` use this history. Local synchronization recovery keeps its existing local `.rdrive-versions` behavior.

Finder **View File History…** provides paged entries, read-only preview, verified export to a new destination, restore as a copy, and explicitly confirmed file replacement. Replacement plans bind the selected entry and observed current revision; newer changes stop a stale replacement. Directory recovery creates a copy. Shared history settings are configurable through the program.

Optional macOS APFS checkpoints retain local state payloads on compatible volumes. They verify actual system snapshot identities and export selected verified payloads without replaying old queues or replacing the live database. System-managed snapshots may disappear; forgetting a local record does not remove the snapshot. See [file history and recovery](HISTORY.md) for scope, limits, and recovery procedures.

## Remote recovery markers and versions

Protected SMB overwrite can retain `.rdrive-backup-<UUID>` and `.rdrive-recovery-<UUID>.json`, with a completion marker after publication. Sync excludes them and `.rdrive-upload-*` / `.rdrive-copy-*`.

An unfinished receipt prevents interpreting a missing path as deletion. Without a usable checkpoint, inspect the target, receipt, complete staging, and prior copy before choosing recovery and removing the corresponding unfinished receipt. These copies have no automatic reclamation and are not independent backups.

`versions save/list/restore/prune` manages custom file history. `put --overwrite --version` archives the target and persists retry conditions before replacement. It supports SMB and strong-ETag WebDAV.

Payloads live at `.rdrive-versions/<path-hash>/<version-id>/content` with SHA-256 manifests verified before restore. Versions are file-only and transfer through local temporary storage rather than requiring server COPY. The format is incompatible with Mountain Duck's `.duckversions`.

Pruning previews unless `--apply` is explicit. A failed multi-step save may leave an incomplete version directory; listing does not treat it as valid. Versions reside beside server data, not in an offline, off-site, or immutable backup.

## WebDAV locks

`lock --seconds` requests 600 seconds by default and requires LOCK/UNLOCK support. The server chooses the actual duration. Later mutations and `unlock` read `RDRIVE_WEBDAV_LOCK_TOKEN` and attach the condition.

Locks do not renew automatically or provide general combined source/destination locking. SMB protected opens/share modes are a different mechanism; there is no SMB lock-management command.

## Resident services and native macOS integration

`service install/start/stop/restart/status/uninstall/run` manages an opt-in per-user macOS supervisor. Installation saves configuration. Starting supervises this state directory's Rust bridge with crash backoff; `--app` also supervises the native notification helper.

Development TCP mode, IPC root, and bundle must match. The service controls its own fixed roles rather than scanning/killing arbitrary RDrive processes. It does not register domains or enable extensions. Uninstall retains profiles, caches, indexes, and recovery content.

Private configuration contains no passwords, and logs have bounded rotation. A running supervisor does not establish server authentication or successful synchronization.

`macos/` provides the companion, compatibility Finder Sync menus/badges, and replicated File Provider. Compilation, development signing, registration, enablement, Finder display, and on-demand reads/writes require separate checks. Development builds are not notarized distribution installers.

File Provider uses native state decorations/actions. Independent Finder Sync remains subject to system path/badge restrictions. See [macOS integration](MACOS_INTEGRATION.md).

## MCP and AI clients

`mcp tools` exposes exact runtime JSON schemas; `mcp tools --allow-write` includes the write-tool catalog. `mcp serve` provides local JSON-RPC stdio by running the same executable as a child, reusing CLI semantics. Queries are exposed by default; `--allow-write` enables write tools.

Local files must fall beneath repeated existing absolute `--local-root` directories. Paths reject symlinks, traversal, internal state, and cache escape. This prevents accidental access and is not OS isolation against concurrent replacement by another local process.

One tool runs at a time, while queue jobs may still run in parallel. Waiting requests do not hold the Store lock. The server supports initialization, tools/resources, ping, cancellation, timeouts, and bounded UTF-8 text reads.

Defaults/limits are 300 seconds per operation, 1 MiB input frames, 8 MiB each for stdout/stderr, and 1 MiB text content. Cancellation/timeouts terminate the child but cannot undo confirmed remote writes; inspect queue/transaction state.

Stdout contains protocol messages only. This is not HTTP/SSE, and it exposes no arbitrary shell, password argument, or editor execution interface.

Tools cover files, sync, mounts, queues, cache, Smart, indexes, rules, services, versions, and repositories. The [MCP guide](MCP.md) describes `rdrive-files`, `rdrive-smart-sync`, `rdrive-transactions`, configuration, and installation. Installing a Skill does not register an MCP server. A plan, queued job, or local domain is not a completed transfer, mount, or deployment.

## Not implemented

Public sharing/file requests, Cryptomator, encryption at rest, remote ACL editing, directory tags, complete resource-fork/security-attribute backup, a standalone Spotlight search interface, full GUI connection management, and production native installers are not provided.

Ordinary-directory cross-file transactions, global automatic conflict merging, repository GC, and server power-loss persistence are outside current guarantees. Server interoperability is limited to versioned evidence; client conditions and recovery records depend on correct servers.

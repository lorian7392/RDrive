# Architecture

RDrive separates remote protocols, persistent recovery state, synchronization policy, and macOS integration. This guide maps those responsibilities to source files and the invariants a change must preserve. See [features](FEATURES.md) for user-visible behavior and [configuration](reference/CONFIGURATION.md) for settings.

## Commands, backends, and transfers

[`src/main.rs`](https://github.com/lorian7392/RDrive/blob/main/src/main.rs) defines the Clap command tree and dispatch. Serde provides structured output. Ordinary commands open state, acquire its private lock, and connect an existing profile. `watch` releases the lock between passes. Read-only index queries use an independent local read and do not wait for the network worker.

MCP, the bridge, notifications, services, and native mounts have their own long-lived or system-call entry points. They do not all hold `Store` continuously.

[`src/config.rs`](https://github.com/lorian7392/RDrive/blob/main/src/config.rs) defines profile/cache/index settings, URL validation, and credential lookup. [`src/factory.rs`](https://github.com/lorian7392/RDrive/blob/main/src/factory.rs) creates the WebDAV or SMB backend and attaches the shared transfer context. [`src/backend.rs`](https://github.com/lorian7392/RDrive/blob/main/src/backend.rs) defines the `Backend` trait for listing, reads, writes, rename, copy, locks, and notifications. Paths are validated again at protocol boundaries; the root is an empty string. Unsupported default trait operations fail instead of silently weakening conditions.

[`src/webdav.rs`](https://github.com/lorian7392/RDrive/blob/main/src/webdav.rs) implements HTTP/DAV, strong ETag conditions, limited redirects, range reads, LOCK/UNLOCK, and the explicit Nextcloud upload state machine.

[`src/smb.rs`](https://github.com/lorian7392/RDrive/blob/main/src/smb.rs) implements negotiation/encryption, identity-and-hash revisions, protected handles, staged publication, recovery receipts, CHANGE_NOTIFY, read leases, and durable-v2/verified reopen. [`vendor/smb`](https://github.com/lorian7392/RDrive/blob/main/vendor/smb) and [`vendor/smb-msg`](https://github.com/lorian7392/RDrive/blob/main/vendor/smb-msg) retain upstream sources and local patch notes. A defined wire message does not by itself establish an implemented application workflow.

[`src/transfer.rs`](https://github.com/lorian7392/RDrive/blob/main/src/transfer.rs) supplies shared in-process rate limiting, counters, and stderr progress. It distinguishes payload completion from server-confirmed commit and accounts for verification traffic too.

[`src/scheduler.rs`](https://github.com/lorian7392/RDrive/blob/main/src/scheduler.rs) groups persistent jobs by profile and target. Jobs for one target remain ordered; different targets share the concurrency limit. A coordinator persists the results.

## Content operations, resumption, and ordinary sync

[`src/operations.rs`](https://github.com/lorian7392/RDrive/blob/main/src/operations.rs) manages verified downloads, local publication, immutable upload-source snapshots, jobs, and custom remote file versions. [`src/resume.rs`](https://github.com/lorian7392/RDrive/blob/main/src/resume.rs) manages persistent download checkpoints, flushed-prefix hashes, prior local copies, and recovery. Upload resumption depends on backend checks and durable job state rather than a sent-byte counter alone.

[`src/sync.rs`](https://github.com/lorian7392/RDrive/blob/main/src/sync.rs) compares the local tree, remote tree, and common baseline. It prepares the full plan before execution and does not apply a conflicting plan. Execution errors do not roll back already-confirmed network operations; the old baseline is retained. Protected replacements/deletions require backend conditions. Incomplete scans cannot establish deletions.

Prior local content and remote SMB recovery receipts retain evidence after interruption. They do not turn ordinary sync into a cross-file transaction.

The three write conditions are `CreateNew`, `Match(Entry)`, and `Any`. Default creation uses `CreateNew`; protected edits match a specific revision. `Any` requires explicit overwrite permission. Weak metadata-index tokens support display and refresh and cannot replace strong content revisions. Strong SMB `stat/list` reads content to establish SHA-256 and identity; metadata-only enumeration explicitly retains a weak token.

## Local state, cache, and migration

[`src/state.rs`](https://github.com/lorian7392/RDrive/blob/main/src/state.rs) maintains `Store`, configuration/jobs/cache records, atomic writes, and process locking. State is private, current-user-owned, and mode `0700`; records are generally `0600`. Managed directories cannot be replaced through symbolic links. On restart, unconfirmed `running` jobs become `interrupted` rather than `done`.

[`src/cache.rs`](https://github.com/lorian7392/RDrive/blob/main/src/cache.rs) manages explicit content caching, hashes, pinning, active readers, reservations, and the combined clean budget. [`src/cache_directory.rs`](https://github.com/lorian7392/RDrive/blob/main/src/cache_directory.rs) binds custom roots to the state directory and directory identity. Migration verifies copied objects, retains durable intent, commits configuration, and reclaims old copies recoverably. This payload store is separate from the system's File Provider storage.

Important paths:

- `config.json` contains profiles/policies; `state.json` contains ordinary jobs/cache/history; `client.lock` is the exclusive Store lock.
- `cache/`, or an explicitly migrated external root, contains reclaimable Rust clean payloads. An ownership marker prevents adopting an unrelated directory.
- `spool/` retains upload/transaction source snapshots and upload checkpoints; `transfers/` retains download recovery records.
- `tmp/` contains temporary operation files; `baselines/` holds common content and metadata baselines.
- `transactions/` contains local managed-repository transaction phases.
- `provider-domains.json` and `provider/` contain domain configuration, SQLite, legacy JSON, demands, receipts, and control state.
- `service/` contains opt-in supervisor configuration, runtime state, bounded logs, and default IPC paths.

These locations are not interchangeable. Clean-cache reclamation cannot remove unsent content; local file commands cannot use internal object paths to bypass account scope; recovery records are not cache garbage. Plaintext storage plus private permissions does not provide encryption at rest.

## Smart domains and indexing

[`src/provider.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider.rs) implements the local File Provider bridge: domains, request validation, content/metadata callbacks, online/offline requests, stable item identities, receipts, and background scheduling. Domains bind protocol, account, remote root, and repository mode so an existing domain cannot silently point to another data source. Messages require the supported version, private IPC, and controlled staging files rather than arbitrary local paths.

[`src/provider/index.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider/index.rs) stores SQLite items, change anchors, control, and recovery records, with transactional migration, per-parent reads, path lookup, and FTS5/LIKE search. [`src/provider/index_scheduler.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider/index_scheduler.rs) schedules directory discovery and refresh. [`src/provider/namespace.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider/namespace.rs) retains ordinary-directory move/logical-deletion plans and manifests. [`src/provider/resolution.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider/resolution.rs) handles persistent content-conflict decisions.

[`src/smart.rs`](https://github.com/lorian7392/RDrive/blob/main/src/smart.rs) retains independent offline-edit snapshots, revision conditions, sequence, retry/cancel/export state, and durable results. Replay completes only after server confirmation. A later edit can advance its base only through an earlier confirmed result from this client. An uncertain request cannot be freely coalesced with a subsequent edit.

[`src/filtering.rs`](https://github.com/lorian7392/RDrive/blob/main/src/filtering.rs) implements relative glob selection, temporary-file rules, and profile overrides. [`src/policies.rs`](https://github.com/lorian7392/RDrive/blob/main/src/policies.rs) implements Smart versions, retention, and explicit conflict policy. Exclusion controls visibility/selection; it does not authorize remote deletion. Automatic replacement still uses the revision established by a persistent decision.

Metadata indexing uses up to four concurrent connection workers in [`src/provider/background_index.rs`](https://github.com/lorian7392/RDrive/blob/main/src/provider/background_index.rs). Each worker plans under the shared coordinator, releases `Store` before connecting or reading the server, and reacquires the coordinator to publish against the latest validated domain, configuration, and catalog. Stale results are discarded. Remote mutations keep serialized replay and their existing publication receipts. The one-shot `smart run` path remains sequential. Projection and metadata selection build lookup maps once per pass rather than scanning every retained request for every file. An unchanged overlay skips index publication; derived-state normalization and real queue, cache, or filter changes still persist.

The native notification helper reads an initial catalog in pages of at most 256 items. It keeps the first page’s anchor and consumes intervening changes after the final page; expired anchors restart that connection’s bootstrap. Page replies omit metadata that notification and download scheduling do not need. Failure in one connection does not end reconciliation for the others.

## Metadata and managed repositories

[`src/metadata.rs`](https://github.com/lorian7392/RDrive/blob/main/src/metadata.rs) captures selected user xattrs bound to a content hash. It records prior values before application for rollback and recovery. [`src/metadata_remote.rs`](https://github.com/lorian7392/RDrive/blob/main/src/metadata_remote.rs) stores portable, protected metadata records for ordinary WebDAV/SMB and maintains an independent three-way baseline. Content and sidecars are confirmed in separate steps and need not become visible together.

[`src/repository.rs`](https://github.com/lorian7392/RDrive/blob/main/src/repository.rs) implements immutable content objects, complete file trees, validated parent chains, and immutable commit slots. The local transaction persists its phase before publication; replaying it can confirm an already-completed commit. Concurrent writers compete for the same create-only slot, without replacing another writer or merging automatically.

Whole-tree old/new snapshot visibility applies to cooperating clients' logical views. Server correctness and persistence through power loss are separate boundaries.

## MCP, services, and native integration

[`src/mcp.rs`](https://github.com/lorian7392/RDrive/blob/main/src/mcp.rs) provides bounded stdio JSON-RPC. [`src/mcp/catalog.rs`](https://github.com/lorian7392/RDrive/blob/main/src/mcp/catalog.rs) defines tool schemas, read/write classification, and CLI mappings. Operations use the same executable as a child process and retain its recovery conditions. Writes are not exposed by default. Scope checks constrain local arguments; cancellation/timeouts stop the child and cannot undo a confirmed remote commit.

[`src/service.rs`](https://github.com/lorian7392/RDrive/blob/main/src/service.rs) creates a macOS user LaunchAgent per state directory. It supervises the bridge and optional notification helper and validates managed paths/process identity. It does not store passwords, register system domains, or terminate arbitrary processes. Uninstall retains user data.

[`src/mount.rs`](https://github.com/lorian7392/RDrive/blob/main/src/mount.rs) invokes system helpers and verifies mountpoints, sources, ownership, and read-only flags. Native mounts are separate from Rust backend transfers. SMB uses system credentials/prompts; WebDAV can receive credentials through a dedicated temporary descriptor.

[`macos/`](https://github.com/lorian7392/RDrive/blob/main/macos) contains the companion, Finder Sync, File Provider, and shared bridge/tag conversion. Native APIs manage placeholders and eviction; Rust manages protocol operations and persistent recovery. Swift separately validates keyed-archive tag conversion and the permitted `#S` attributes. Compilation does not register, enable, or deploy the extensions. See [macOS integration](MACOS_INTEGRATION.md).

## Change and validation entry points

Protocol and recovery changes need isolated fixtures or state-machine regressions. Configuration changes must also update the reference. Generate CLI help instead of editing help blocks:

~~~sh
cargo build --locked
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive
python3 tests/test_documentation.py target/debug/rdrive
~~~

Documentation contracts independently compare source subcommand declarations against the executable's help tree and check that the complete configuration JSON covers actual structure fields. See [testing](TESTING.md) for layers and external dependencies; historical reports retain their original version and scope.

# Changelog

Changes are organized from source history and versioned validation records. Dates identify the recorded changes or checks; they do not imply that a signed distribution was published. Historical test counts and deployment observations remain in their original reports and do not establish current coverage or installation status.

## 0.8.1 — 2026-11-10

- Account for credits granted by SMB `STATUS_PENDING` responses before waiting for final completion, so interim grants remain available to other requests.
- Preserve a queued pending response when the same message ID receives its final response before the caller drains the queue.
- Add deterministic protocol regressions for both conditions and run the vendored SMB library tests explicitly in CI using the root lockfile and the client's async/sign/encrypt feature set.
- Keep ordinary builds and tests scoped to RDrive through workspace default members; update the native development bundle to version 0.8.1/build 11.

## 0.8.0 — 2026-11-10

- Publish English project, installation, quick-start, feature, architecture, configuration, and troubleshooting documentation.
- Add a GitHub Pages documentation site and main-branch build/test and version-tag workflows.
- Generate the CLI help tree and complete MCP schemas from the current executable, with drift checks.
- Expand Rust, Swift, and Python regression tests and injectable system boundaries; add per-language production-coverage collection and strict gates.
- Keep reproducible current checks separate from immutable historical validation evidence.
- Prevent macOS path aliases or linked ancestors from bypassing the requirement that synchronization baselines remain outside synchronized trees, for both content and metadata synchronization.
- Validate transaction metadata completely before writing; invalid input preserves the existing transaction.
- Compare metadata selections independently of order and duplicates, and check actual xattr read lengths to reject changing values.
- Return diagnostic errors for command-output failures instead of panicking when a pipe closes.
- Require configured Finder roots to be real directories rather than ordinary files.

- Add clean-cache clearing with preview by default and explicit application, protecting pinned, active, and non-clean content.
- Support custom Rust clean-payload directories, verified migration, return to the default directory, and persistent migration recovery.
- Add cache directory, migration, and clearing commands to CLI and MCP. Internal-storage boundaries also protect relocated cache directories.
- Update the native development bundle to build 10 and record signature checks and read-only activation; distinguish development signing from notarized distribution.

See [cache management](docs/CACHE_MANAGEMENT.md) and the [0.8 validation record](docs/VALIDATION_0.8.0.md).

## 0.7.0 — 2026-11-10

- Add Smart file/directory rename, move, persistent namespace recovery, recoverable deletion, and stable item/descendant identities.
- Add global and profile filters, user LaunchAgent management, file-version retention policies, and explicit conflict decisions.
- Add offline filename/path search over SQLite, with scope, type, pagination, and MCP access.
- Expand CLI/MCP management while retaining explicit limits for ordinary multi-request operations and remote recovery copies.

See [Smart management](docs/SMART_OPERATIONS.md) and the [0.7 validation record](docs/VALIDATION_0.7.0.md).

## 0.6.0 — 2026-11-10

- Make SQLite the authoritative Smart directory index, with per-parent reads and stable identities.
- Prebuild remote metadata breadth-first in the background, with separate active-directory refresh, depth, and manual recursive requests.
- Preserve legacy JSON history, confirmed upload receipts, pinning, and recovery state. Add CLI/MCP index controls.

See [directory indexing](docs/DIRECTORY_INDEXING.md) and the [0.6 validation record](docs/VALIDATION_0.6.0.md).

## 0.5.0 — 2026-11-10

- Add a read-only-by-default MCP stdio server, explicit write enablement, local-directory scopes, and a client-configuration generator.
- Provide file, Smart synchronization, and managed-transaction Skills with an installer.
- Fix command-argument conflicts and check text-file size before downloading oversized content.

See [MCP](docs/MCP.md) and the [0.5 validation record](docs/VALIDATION_0.5.0.md).

## 0.4.1 — 2026-11-10

- Replace whole-tree scanning before initial display with incremental per-directory loading and merging.
- Keep working-set enumeration nonrecursive, discover pinned descendants incrementally, and preserve the existing index after scan failure.

See the [0.4.1 validation record](docs/VALIDATION_0.4.1.md).

## 0.4.0 — 2026-11-10

- Add persistent Smart directories, retained offline-edit snapshots, reconnect uploads, confirmed-receipt reuse, and recursive pin policies.
- Combine Rust clean-cache accounting with reported File Provider copies, reservations, LRU reclamation, and protected-content handling.
- Add a File Provider status/action UI; document separate Finder Sync badge limitations.
- Force loopback WebDAV to connect directly while preserving proxy support for other destinations.

See [Smart synchronization](docs/SMART_SYNCHRONIZATION.md) and the [0.4 validation record](docs/VALIDATION_0.4.0.md).

## Earlier versions

These features were present in the first repository commit. The summaries below come from their validation records and do not imply independent Git tags or published distributions.

### 0.3.1

Accept empty directory `getcontentlength` values from compatible WebDAV servers while requiring reliable lengths for ordinary files. Enable macOS system-proxy discovery and retain certificate and redirect protection. See the [0.3.1 validation record](docs/VALIDATION_0.3.1.md).

### 0.3.0

Add selected tags/xattrs, managed multi-file snapshots, shared rate limiting, parallel upload jobs, and progress. Expand SMB notifications, leases, recovery, and native File Provider integration. See the [0.3 validation record](docs/VALIDATION_0.3.0.md) and [macOS integration](docs/MACOS_INTEGRATION.md).

### 0.2.0

Expand native SMB/WebDAV mounting, persistent download checkpoints, resumable SMB uploads, protected overwrite recovery, and explicitly configured Nextcloud chunked uploads. The CLI already included profiles, file operations, cache, queues, synchronization, versions, and WebDAV locks. See the [0.2 validation record](docs/VALIDATION_0.2.md).

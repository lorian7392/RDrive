---
name: rdrive-smart-sync
description: Operate RDrive Smart Synchronization, clean-cache limits, retained offline edits, and conservative directory sync. Use for RDrive cache pressure, offline availability, synchronization conflicts, and Smart recovery.
---

# RDrive Smart and sync

Preserve the user's selected profile, domain, and `RDRIVE_HOME` or explicit `--home`. Prefer connected RDrive MCP tools after inspecting their actual schemas; CLI fallback supports `--json --progress off`. Do not restart an existing bridge or register a second domain to solve a missing credential in a different client process.

## Inspect the right system

```sh
rdrive --json smart status
rdrive --json smart list cloud-smart
rdrive --json provider domains
rdrive --json cache status
rdrive --json cache list
rdrive --json queue list
rdrive --json index status
```

MCP inspection tools: `rdrive_smart_status`, `rdrive_smart_list`, `rdrive_domains`, `rdrive_cache_status`, `rdrive_cache_list`, `rdrive_queue_list`. The ordinary upload queue and Smart retained-mutation journal are separate. Distinguish their identifiers and cancellation behavior.

Ordinary `rdrive mount` is a native network volume. Smart uses a registered macOS File Provider domain and running Rust bridge; it browses a local SQLite index, downloads on access, and retains offline changes. A newly configured domain is not proof that Finder registration or the background service is running. Directory indexes build progressively in the background; an unvisited subtree is not evidence of deletion. The installed service supervisor can run the broker and optional watcher; inspect `rdrive_service_status` before starting it. Service installation does not register a native domain.

For slow folder discovery, inspect `rdrive_index_status` / `index status`. Background preindex is enabled by default and reads metadata only. Configure it with `rdrive_index_config` or `index config --enable --all`, `--depth 2`, or `--disable`; active/background intervals use `--active-refresh-seconds` and `--refresh-seconds`. Request an asynchronous one-shot subtree refresh with `rdrive_index_refresh` or `index refresh cloud-smart documents --recursive`. A queued refresh is not completed indexing. The worker must be running; ordinary native mounts do not consume this index. Preserve the SQLite database, its recovery records and original migration JSON; never delete them as clean cache, and stop an older broker before upgrading the same state directory.

## Cache capacity and offline files

```sh
rdrive --json cache config --max-bytes 5368709120
rdrive --json cache fetch dav documents --recursive --pin
rdrive --json cache get dav documents/report.pdf ./offline-report.pdf
rdrive --json cache unpin dav documents --recursive
rdrive --json cache prune --max-bytes 1073741824
```

MCP uses `rdrive_cache_config`, `rdrive_cache_fetch`, `rdrive_cache_get`, `rdrive_cache_unpin`, and `rdrive_cache_prune`. Mutations and local exports need MCP `--allow-write`; local paths must be under a configured `--local-root`. Existing authorization remains applicable.

The byte budget covers clean CLI cache, reported File Provider clean downloads, and approved download reservations. It excludes queued source snapshots, unuploaded edits, recovery files, metadata, and system overhead. It is not a disk quota or preallocated partition. Pinned or active content is protected; reducing the limit can fail without changing the old setting. Never delete pending edits to satisfy a clean-cache target.

`cache get` is an offline read of an existing verified cached copy, not proof it is the latest server version. Finder's Keep Offline and Allow Automatic Removal affect File Provider policies; CLI cache pinning is not a substitute for setting those native policies. Allow Automatic Removal does not promise immediate eviction.

For conflicts, paused work, file history, APFS payload recovery, retry, export, and cleanup, read [retained edit recovery](references/recovery.md).

## Ordinary folder synchronization

```sh
# Both roots must already exist. The first command only plans changes.
rdrive --json sync dav ./work work --direction both
rdrive --json sync dav ./work work --direction both --apply
```

Use `rdrive_sync_plan`, review actions/conflicts, then `rdrive_sync_apply` when applying the requested sync is authorized. Pass the same profile, roots, direction, deletion policy, and metadata policy. A plan is not a reservation: apply rechecks current revisions and may stop after concurrent changes.

`both`, `push`, and `pull` are conservative synchronization, not unconditional mirroring. No baseline means missing files cannot be inferred as deletions. Divergent changes or unsupported conditional deletes stop processing; adding overwrite/retry flags does not resolve this safely. Propagate deletion only when requested, with CLI `--delete` or matching tool argument. SMB file deletion validates content and identity on a protected handle; ordinary sync still refuses directory deletion. Ordinary sync can partially complete before an error and does not roll back confirmed network operations; it is not a multi-file transaction.

Metadata is opt-in (`--metadata`), limited to supported Finder tags and selected user attributes. Ordinary remote content and portable metadata sidecars commit separately. Use managed repositories for atomic publication of content, selected metadata, and multiple logical files.

## Rules, versions, search and resident supervision

Use `rdrive_index_search` for local offline filename/path search; it covers only indexed entries. CLI uses `index search DOMAIN QUERY --scope PATH --page-size 100 --offset 0`, separate from network `ls --recursive`. Search is literal, not an FTS query language.

Inspect `rdrive_filter_status` and test rules with `rdrive_filter_test` before changing selection. `rdrive_filter_config` takes include/exclude arrays and per-profile overrides. Exclusion never authorizes deleting remote files or retained edits. Slashless globs match basenames; slash patterns are rooted in the domain; directory exclusions cover descendants.

Inspect `rdrive_policy_status`; defaults are versions disabled and conflicts stopped. `rdrive_smart_resolve` accepts keep-both, replace or remote. Replace archives the observed remote revision and retains a CAS guard; a second change must stop again. Remote cancels publication while preserving the local recovery snapshot. Configure automatic policy only to match the user’s requested behavior. `smart recovery DOMAIN` reports retained remote trash backups, which still consume remote space.

Use `rdrive_service_install/start/stop/restart/uninstall` only for this home’s supervisor. Install accepts dry-run and does not start. Native App IPC must match the installed service; Keychain credentials are preferable to terminal-only environment variables. A running supervisor is not proof authentication or Finder registration succeeded. These tools require MCP write mode; native App/IPC paths require an allowed local root.

## Clear or relocate Rust cache

Use `rdrive_cache_clear` without apply to inspect eligible clean objects, then apply when cleanup is authorized. Pinned, active, unmanaged and unsent files remain protected; do not invent force-delete behavior. Finder download eviction uses the existing native trim/removal APIs separately.

Inspect `rdrive_cache_directory`; configure with `rdrive_cache_set_directory` using an absolute private empty directory (or default=true), optionally dry_run=true. Migration verifies content and commits the new location only after all copies pass. Active readers/downloads prevent migration; source files remain in place on refusal. Preserve cache-directory-migration.json and both roots after an interruption; normal startup reconciles the durable intent. Untracked files block migration and must not be deleted as cache.

Use RDrive 0.8 or newer for every broker sharing that state before changing the path. The change covers Rust clean payloads, not File Provider storage or offline edits. Generic MCP file tools intentionally reject cache roots, including migration roots; use profile-bound cache get rather than exporting internal object files.

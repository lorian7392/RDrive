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
```

MCP inspection tools: `rdrive_smart_status`, `rdrive_smart_list`, `rdrive_domains`, `rdrive_cache_status`, `rdrive_cache_list`, `rdrive_queue_list`. The ordinary upload queue and Smart retained-mutation journal are separate. Distinguish their identifiers and cancellation behavior.

Ordinary `rdrive mount` is a native network volume. Smart uses a registered macOS File Provider domain and running Rust bridge; it browses a local index, downloads on access, and retains offline changes. A newly configured domain is not proof that Finder registration or the background service is running. Directory indexes load progressively; an unvisited subtree is not evidence of deletion. MCP does not launch the native companion or a long-running provider service.

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

For conflicts, paused work, retry, export, and cleanup, read [retained edit recovery](references/recovery.md).

## Ordinary folder synchronization

```sh
# Both roots must already exist. The first command only plans changes.
rdrive --json sync dav ./work work --direction both
rdrive --json sync dav ./work work --direction both --apply
```

Use `rdrive_sync_plan`, review actions/conflicts, then `rdrive_sync_apply` when applying the requested sync is authorized. Pass the same profile, roots, direction, deletion policy, and metadata policy. A plan is not a reservation: apply rechecks current revisions and may stop after concurrent changes.

`both`, `push`, and `pull` are conservative synchronization, not unconditional mirroring. No baseline means missing files cannot be inferred as deletions. Divergent changes or unsupported conditional deletes stop processing; adding overwrite/retry flags does not resolve this safely. Propagate deletion only when requested, with CLI `--delete` or matching tool argument. SMB lacks atomic conditional deletion. Ordinary sync can partially complete before an error and does not roll back confirmed network operations; it is not a multi-file transaction.

Metadata is opt-in (`--metadata`), limited to supported Finder tags and selected user attributes. Ordinary remote content and portable metadata sidecars commit separately. Use managed repositories for atomic publication of content, selected metadata, and multiple logical files.

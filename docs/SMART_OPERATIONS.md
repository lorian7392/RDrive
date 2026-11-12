# Smart operations and management

This guide covers Smart / File Provider domains and the management features introduced in 0.7. Ordinary `mount` volumes follow macOS filesystem behavior, and ordinary `sync` keeps its conservative synchronization rules. These policies do not change explicit `get`, `put`, or ordinary synchronization commands.

## File and directory operations

Finder Smart domains support file/directory creation, content and tag edits, and renaming, moving, and deleting ordinary WebDAV/SMB files and directories. Existing item identities stay stable. Moving a directory updates known descendant paths and offline pin policies. When content, name, parent, and tags change together, the bridge retains receipts for each step before confirming the final result. Ordinary folders do not provide an atomic transaction across several protocol requests.

Moves do not replace destinations by default. WebDAV uses a strong source revision and `Overwrite: F`, and stops if no usable guard exists. SMB verifies identity and content through an open source handle, requests denial of concurrent writes/deletes, then publishes with a no-replace rename. Unsupported protection never falls back to an unconditional replacement.

Directory deletion is recoverable logical deletion. RDrive persists the plan and tree manifest, moves the source under protection to a unique sibling `.rdrive-trash-<UUID>`, then verifies the whole tree before confirmation. Unexpected changes trigger an attempted no-replace restoration; otherwise the recovery path is retained and a conflict reported. File deletion uses a protocol-supported guarded DELETE. Directory deletion retains a recovery location, hidden from Smart listings.

```sh
rdrive smart recovery cloud-smart
```

Recovery entries report original path, backup path, phase, and `space_reclaimed: false`. Backups remain on the server and **do not release server space**; this version does not automatically purge them physically. Do not delete internal recovery directories through ordinary synchronization. Offline renames, moves, and deletes appear as pending changes, and original local records remain until the remote result is confirmed. Conflicting operations can be cancelled, or their unsent content exported.

## Filters

```sh
# Replace only the specified lists; other lists remain unchanged. Quote patterns for the shell.
rdrive filter config --exclude 'private/' --exclude '*.bak'
rdrive filter config --include '*.txt' --include 'docs/**/*.md'
rdrive filter config --profile dav --exclude 'archive/**'
rdrive filter status --profile dav
rdrive filter test dav documents/report.txt
rdrive filter test dav archive --directory
rdrive filter config --clear-include --clear-exclude
rdrive filter reset dav
```

A glob without `/` matches filenames at any depth. Patterns containing `/` match from the domain root; `*` does not cross a slash, while `**` can span levels. A trailing `/` matches directories, and excluding a directory excludes its descendants. Excludes take priority over includes. Directories may still be traversed to discover files selected by include rules. Matching is case-sensitive. Each profile can override the global include list, exclude list, and temporary-file option independently; `reset` restores inheritance.

The defaults hide `.DS_Store`, AppleDouble `._*`, `*.tmp`, `*.swp`, `*.swo`, `*.part`, `~$*`, and `.~lock.*`. Use `--include-temporary` to disable this behavior or `--exclude-temporary` to restore it. Internal version, staging, and recovery directories always retain their internal protection.

Filters control Smart listings, background discovery, and destinations of new writes. Changing a rule does not delete remote files, remove indexed recovery records or pending upload snapshots, or interpret excluded entries as deletions to propagate. Exclusion pauses queued operations; they may resume when allowed again. Inspect `smart list` for actual state. Explicit CLI file operations can still access these paths.

Finder labels retained operations blocked by these rules **Excluded by sync rules**. Open **Show Sync Status** for the blocked path; an eligible stale-folder repair also identifies the matching rule. A built-in exclusion such as `*.part` is a local policy decision and can leave the pending edit retained. An existing excluded child does not block a metadata-only update of its containing directory. Renaming, moving, or deleting a directory still checks its descendants, so those operations cannot bypass exclusions.

## Automatic versions and conflict policy

```sh
rdrive policy status
rdrive policy config --enable-versions --keep 5
rdrive policy config --profile dav --include '*.docx' --include '*.txt' --exclude 'scratch/**'
rdrive policy config --profile dav --conflicts stop
rdrive policy reset dav
```

Automatic versions are disabled by default, and the default conflict policy is `stop`. When enabled, Smart archives the original revision named by the comparison condition before replacing content. Older versions are pruned to the retention count only after replacement is confirmed. Failed archives and unconfirmed replacements do not delete old versions first. Backups belonging to other writers' unconfirmed publications are not pruned either, so unfinished work can temporarily exceed the count.

Versions live in RDrive's internal remote version storage, keyed by remote path. Renaming retains history at the old path; it does not automatically follow the new name. This is separate from provider-native version history. Explicit `put --version` and `versions list/save/restore/prune` keep their existing behavior.

`stop` retains a conflict for a decision. `keep-both` saves local content under a stable conflict name with create-only conditions and leaves the remote original intact. `replace` archives the actually observed remote revision, then replaces under that revision's condition. Decisions persist before writes. A later remote change creates another conflict; automatic policy does not repeatedly chase and overwrite newer changes.

```sh
rdrive smart list cloud-smart
rdrive smart resolve cloud-smart MUTATION_ID --strategy keep-both
rdrive smart resolve cloud-smart MUTATION_ID --strategy replace
rdrive smart resolve cloud-smart MUTATION_ID --strategy remote
rdrive smart export cloud-smart MUTATION_ID ./recovered.txt
```

`remote` cancels local publication but retains the independent recovery snapshot for later export. Decisions apply only to create/modify conflicts with retained file content. Guarded replacement handles content conflicts at the original path. Compound rename/move conflicts require a person to reconcile the naming intent; keep-both can still preserve content. Directories and missing targets require handling appropriate to their state. Managed repositories use existing immutable snapshot history; ordinary file versions do not provide multi-file transactions.

## Refresh a stale Finder directory conflict

Use `rdrive smart list DOMAIN` to find the mutation ID when Finder reports that the item changed since its system snapshot.

```sh
rdrive smart refresh-conflict DOMAIN MUTATION_ID
```

For an obsolete directory update whose canonical name and parent are unchanged, the Finder status dialog offers **Repair Sync Issue**. This includes a contentless directory callback blocked by an excluded descendant. Its eligibility check reads the local catalog and retained queue, so it remains available while child uploads are running. The repair rechecks the exact mutation ID, saves a permanent cancellation marker, and queues a metadata refresh of that directory. It works offline and preserves other queued operations, their original version conditions, and all retained contents. It does not change filtering rules or delete excluded files. A queued refresh is complete only after the provider successfully reads the directory again. Actual Finder tag or extended-attribute edits require a separate decision and are excluded from this repair.

Older versions can report `unsupported: fetch this file before editing metadata` for an unchanged directory, especially with SMB metadata identities. A directory has no file contents to fetch. **Repair Sync Issue** is also available from **Needs attention** for this exact obsolete request: it verifies that any saved provider intent is unpublished and matches the recorded replay before clearing it. New unchanged-directory callbacks complete without a content fetch. Explicit tag or extended-attribute changes, including clearing them, remain retained and are never treated as empty callbacks; directory tag and attribute synchronization is unsupported.

If another active request directly targets the same directory, the status window explains that it needs review. A cancelled request from an earlier repair cannot hide a newer conflict or make a repeated repair report success for it. Child uploads can continue independently.

The command also handles these obsolete updates locally. Directory renames, moves, and deletions continue to require a successful authoritative parent listing before cancellation. Those namespace repairs stop when related pending work or a recorded publication intent makes cancellation ambiguous. Reopen the parent in Finder and repeat an intended namespace operation against the refreshed item if it is still appropriate.

For a request that reached namespace publication, inspect `rdrive smart recovery DOMAIN` and the server before deciding how to proceed.

## Resident service

```sh
# Installation saves configuration without starting the service. Review the plan first.
rdrive service install --ipc-root /private/tmp/rdrive-user-ipc --dry-run
rdrive service install --ipc-root /private/tmp/rdrive-user-ipc
rdrive service start
rdrive service status
rdrive service restart
rdrive service stop
rdrive service uninstall
```

macOS uses a separate user LaunchAgent for each state directory. It supervises its Rust bridge and restarts it with backoff after a crash. Installing with `--app /absolute/path/RDrive.app` also supervises the native change-notification helper. Development TCP IPC bundles require `--development-tcp` and an IPC root matching the path recorded when the bundle was built. Keep paths short enough for Unix socket limits.

The service does not register File Provider domains or enable system extensions automatically. It controls only its fixed job for the current state directory, without searching for and terminating other RDrive processes. `service run` is the foreground supervisor used by launchd. Uninstalling preserves caches, indexes, connections, and recovery content.

Configuration and logs have private permissions. Service configuration stores paths without passwords or environment values. A login service does not inherit a terminal's temporary password environment; use macOS Keychain for persistent background authentication. A live `service status` does not prove successful connection authentication; also check Smart state and index progress. Bounded rotating logs record supervisor events such as child exits.

## Index search and MCP

```sh
rdrive index search cloud-smart '报告'
rdrive index search cloud-smart 'report' --scope documents --kind file --page-size 50 --offset 0
```

Search reads names and paths entirely from local SQLite, without remote recursive search or content downloads. At least three characters use the FTS5 trigram substring index. One or two characters use escaped SQLite LIKE, including short Chinese names and literal `%` or `_`. Queries are literal text rather than user-supplied FTS syntax. Results cover indexed items and include `anchor`, `more`, and the next offset. Index changes between pages can change pagination. Current filters and pending-deletion state apply.

First use of the 0.7 search format transactionally upgrades the derived search index in an older SQLite database, preserving items, offline edits, versions, pins, and receipts. Unknown formats or corruption fail without replacing original data. Stop older background processes sharing the directory before upgrading; do not mix versions.

MCP provides `rdrive_index_search`, `rdrive_filter_*`, `rdrive_policy_*`, `rdrive_smart_resolve/recovery`, and `rdrive_service_*`. Queries are enabled by default; configuration, conflict decisions, and service changes require `--allow-write`. Rule lists use string arrays. Service App/IPC paths must also satisfy authorized local-path scopes. Inspect `rdrive mcp tools --allow-write` for authoritative parameters.

# Troubleshooting and recovery

Start with the version, state directory, and exact error. A generated plan, queued job, built App, or registered domain describes a stage, not the final operation result. Preserve checkpoints and recovery copies while diagnosing a failure.

## Collect diagnostics

~~~sh
rdrive --version
rdrive profile list
rdrive status
rdrive status --details
# Explicit authenticated checks; may access configured credentials.
rdrive status --check-connections
rdrive queue list
rdrive transfers list
rdrive history --limit 20
~~~

Use the same `--home` on every command if state is customized. Output may contain usernames, server addresses, and local/remote paths; redact them before opening an issue. Do not submit passwords, lock tokens, complete private state directories, or user-content snapshots.

Use `rdrive COMMAND --help` and the [CLI reference](reference/CLI.md) for options. See [testing](TESTING.md) for validation and interoperability scope.

## Command missing or an old version runs

Source builds produce `target/release/rdrive`; a checkout does not guarantee `dist/rdrive`.

~~~sh
./target/release/rdrive --version
command -v rdrive
rdrive --version
~~~

Different versions mean `PATH` selects another executable. Rebuilding the CLI does not update an App's embedded copy, a running bridge/MCP process, or registered extensions. Follow the [upgrade procedure](INSTALLATION.md#upgrade).

## Authentication fails

`credential environment variable ... is missing` means the current RDrive process did not receive that variable. Read and export it again through hidden terminal input; do not print it into a log.

Keychain is queried only when `--password-env` is absent. Its service and account must match the profile. If a terminal works but a background process fails, inspect the credential source first. LaunchAgents do not inherit temporary terminal exports, and the service installer does not write passwords into service files. Prepare appropriate Keychain credentials as described in [configuration](reference/CONFIGURATION.md).

For `401`, `403`, or SMB login/permission errors, verify the account, DAV file root, share name, and server permissions. Weakening local permissions or transport encryption does not resolve an account error.

Native SMB mounting uses macOS credentials or a terminal prompt. Successful CLI password-environment authentication does not establish system mount credentials. `--no-prompt` cannot bypass authentication.

## Timeout, proxy, or certificate errors

- Confirm the protocol endpoint: a DAV file root or `smb://host/share/subtree`, rather than a website login page.
- WebDAV validates HTTPS certificates. Check the hostname, clock, and certificate chain. There is no general certificate-verification bypass or custom CA option.
- Increase `--timeout` when adding a profile for a slow link. It does not fix a wrong address, blocked route, or failed authentication.
- WebDAV supports environment and macOS system proxies. Literal loopback addresses and `localhost` connect directly. PAC/exception-table support is incomplete in the proxy library; use `NO_PROXY` for explicit direct destinations when appropriate.
- Native mounts use a different system client with independent proxy, certificate, cache, and timeout behavior.
- SMB requires encryption by default. `--allow-unencrypted-smb` is an explicit trusted compatibility-test option, not a general failure fallback.

Diagnose actual requests before changing a global proxy, VPN, route, or server security policy. Real-server interoperability needs endpoint-specific evidence.

## Invalid path, existing destination, or permission error

Remote paths are relative to the profile root. Absolute paths, `..`, and backslashes are rejected. Case, Unicode-normalization, and file/directory conflicts stop operations. Do not bypass a failure by editing internal indexes.

No-overwrite behavior is intentional. Inspect the target through `stat` or `ls`, and use `--overwrite` only for an intended replacement. Upload parents must exist; `mkdir ... --parents` can create them. Profile names accept letters, digits, `-`, and `_`.

Local sync symlinks and recognized SMB reparse points are rejected. Some servers transparently resolve links, so remote no-follow behavior also depends on server policy.

## State directory is busy

Mutations sharing a state directory are serialized. Check for active transfers, `watch`, bridge/MCP processes, or old services. Stop an instance you no longer need and retry. Do not delete locks or state files while an owning process is running.

A separate `--home` isolates a test but does not inherit the original profiles, snapshots, or baselines.

## Upload failure or an uncertain result

Inspect `queue list`, `history`, and remote `stat` first. A job's retained snapshot may be the only unpublished copy.

- `queue run` handles pending and eligible interrupted work.
- Failed jobs need `queue run PROFILE --retry-failed`.
- An uncertain job created with `--overwrite` also needs `--retry-ambiguous-overwrite`. This can overwrite the path again; inspect current content and job intent first.
- `--resume` requires matching protocol support. An upload created without it does not gain a checkpoint through an ordinary retry.
- `queue cancel JOB_ID` removes the local upload snapshot. Existing upload checkpoints and remote recovery files may remain. Cancellation cannot undo an already-completed server operation.

Enqueue success means the snapshot was retained. A job becomes `done` only after server confirmation. Do not conceal uncertain work with a new unconditional overwrite.

## Download resumption is rejected

Checkpoints retain flushed prefix bytes and SHA-256. A changed remote revision, corrupt prefix, invalid Range response, or concurrent destination edit stops resumption. Retain the partial data and inspect the server/destination before choosing a fresh filename or restarting.

`transfers list` inspects records. `transfers discard ID` explicitly discards the corresponding checkpoint/partial transfer while preserving prior local recovery copies. `transfers/`, `.rdrive-partial-*`, and `spool/` are not clean cache. An overwrite result's `previous_local` identifies its prior local copy.

SMB upload resumption also verifies remote staging identity and the complete uploaded prefix. A Nextcloud chunk without a reliable unchanged validator may retransmit. Extra verification reads do not mean all payload bytes were uploaded again.

## Synchronization or edit conflicts

Stop repeatedly applying the same plan and preserve both sides and recovery copies. Sync stops on concurrent changes, differing content without a common baseline, incomplete scans, or name/type conflicts. It does not automatically choose the newer timestamp.

Use `rdrive conflict list` to inspect ordinary sync conflicts. For files with a common baseline, [preview and apply a repair](CONFLICTS.md) while keeping a recovery copy. Initial absence does not prove deletion. Losing state loses the common baseline. Execution failures do not roll back already-confirmed network operations; the old baseline remains, and later manual reconciliation may be necessary.

A failed `edit` upload remains in the queue. If the editor itself fails, the error reports the retained local file. Export/copy the result before resolving remote revision changes.

Smart conflicts use a separate persistent queue:

~~~sh
rdrive smart list DOMAIN
rdrive smart export DOMAIN MUTATION_ID ./recovered.txt
~~~

Choose `keep-both`, `replace`, or `remote` through [Smart management](SMART_OPERATIONS.md). Replacement remains revision-protected. `remote` abandons local publication while retaining an exportable recovery snapshot. `smart retry` preserves original conditions; `smart discard` explicitly reclaims an eligible recovery snapshot.

If a Smart folder shows **Needs attention** with `unsupported: fetch this file before editing metadata`, open **Show Sync Status** or **Repair Sync Issue…** in Finder. Older versions could mistake an unchanged directory callback for a file metadata edit. The repair is offered only when the same folder has no content, tag, attribute, rename, or move intent and no evidence of publication. It clears that obsolete request and queues a folder refresh while preserving other retained edits. Downloading a directory cannot resolve this message. A genuine directory tag or extended-attribute edit remains unsupported and requires separate review.

## SMB recovery files or a temporarily missing path

Protected overwrite may leave `.rdrive-backup-*`, `.rdrive-upload-*`, and `.rdrive-recovery-*.json`. An interruption between renames can leave the original path temporarily absent. Sync stops on the unfinished receipt rather than treating this as deletion.

With an upload checkpoint, resume the original job so the client verifies identity and publication phase. Otherwise:

1. Read the receipt named by the error with `cat` and inspect the target, prior copy, and staging through `ls` / `stat`.
2. Export needed content and identify the intended version.
3. If manual recovery is necessary, explicitly `mv` a verified prior copy or complete staging file back to the path. The default preserves a newly appeared destination.
4. Only consider removing that specific unfinished receipt after confirming content and recovery state.

Do not bulk-delete `.rdrive-*`. These files have no universal automatic GC and are not independent backups. See [SMB lifecycle](SMB_LIFECYCLE.md).

## Cache full, cleanup refused, or migration blocked

Inspect `cache status`, `cache list`, and the `cache clear` preview. Pins, active readers, unsent edits, conflicts, and refused system eviction can prevent reclamation. An impossible limit reduction preserves the previous setting. Unpin unneeded content, then reclaim Rust payloads and native copies through their respective interfaces.

The clean-cache budget is not a hard quota for the entire state directory or disk. Databases, snapshots, unsent edits, recovery copies, temporary transfers, and system overhead are additional. Native SMB/WebDAV network-volume caches are separate.

A migration destination must meet private-directory rules and fit the full new copy. Active reads/downloads, unknown files, symlinks, changed identity, or failed hashes stop migration and preserve content. Use the preview/recovery procedure in [cache management](CACHE_MANAGEMENT.md), rather than manually moving active data.

`cache get` reads a local copy without establishing server freshness. `cache clear --apply` does not remove upload snapshots, unsent edits, remote versions, or recovery backups.

## Native mount missing or unexpected read-only behavior

Run `mount ... --dry-run` first. The mountpoint must exist, be empty, belong to the current user, and have mode `0700`. Inspect `mounts` to confirm the actual kernel mount rather than relying only on the helper's exit status. Use `--browse` to request visibility; mounts are hidden from the desktop by default.

`--read-only` requests a genuinely read-only volume. Writable WebDAV also depends on macOS and server LOCK support. Mounts are not offline replicas and do not acquire RDrive pinning semantics. Follow [mounting](MOUNT.md) before forcing a detach while applications may still write.

## Finder domain, status, or search does not update

Smart requires a profile, domain configuration, accepted/enabled native extensions, the bridge, and the notification helper. `smart setup` saves configuration; compilation or registration alone does not establish correct Finder display.

Inspect `provider domains`, `smart status`, `index status DOMAIN`, and companion `--diagnose` / `--list-domains`. Verify the same state/IPC paths and ensure an old embedded CLI is not still running after upgrade. See [macOS integration](MACOS_INTEGRATION.md).

For an empty or stale Smart directory, inspect `rdrive index status DOMAIN`. `pending` includes discovered directories that have not completed their first successful scan; it does not mean all of them are currently running. An existing SQLite index can therefore be incomplete.

Select a Smart folder in Finder and choose **Refresh Index**, or run `rdrive index refresh DOMAIN --recursive` for the connection tree. The full companion app also offers **Refresh Connection Index**; its headless resident notification helper does not show the menu bar UI. A queued acknowledgment means the request was saved. The owned service must be running and synchronization unpaused, and filters and retry backoff still apply.

`index refresh` retains a request for the background worker. Offline search reads acquired filename/path metadata without fetching content or contacting the server. Unscanned directories, paused services, and filters affect results. See [indexing](DIRECTORY_INDEXING.md).

Independent Finder Sync menus/badges and native File Provider decorations are different mechanisms. Mapping an ordinary directory or network volume does not make it a Smart domain.

## MCP tools or local paths are unavailable

MCP exposes queries by default. Writes require `mcp serve --allow-write`, and local file arguments must fall beneath declared existing `--local-root` directories. Internal state/cache paths have additional protection. Do not default to granting your entire home directory to resolve a path error.

The configuration generator does not install a service or edit client settings. For a source build, pass `--binary ./target/release/rdrive`. The client must select state containing the intended profiles and receive their credentials. Another connected process does not establish this MCP session's access.

Cancellation/timeouts end the current operation but cannot undo earlier confirmed writes. Inspect `isError`, reports, and queue/transaction state before retrying. See [MCP](MCP.md).

## Uncertain managed commit

Retain the original state directory and transaction ID. Inspect `repo status TRANSACTION_ID`, then replay `repo commit TRANSACTION_ID` to determine whether that transaction published. Do not create a new transaction to bypass an uncertain receipt or abort a prepared publication.

Managed directories have an internal storage format. Ordinary `ls` and native mounts expose that physical structure. Use `repo ls` / `repo get` or a managed File Provider domain for the logical tree. Concurrent conflicts do not merge automatically. See [managed repositories](REPOSITORY.md) for protocol and retained-space limits.

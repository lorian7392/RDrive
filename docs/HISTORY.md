# File history and recovery

RDrive can preserve verified copies before replacing or deleting remote content. The retained history is separate from the clean cache: automatic cache eviction does not remove it. Finder exposes the same history as the command-line client, with read-only preview, export, and guarded recovery.

History starts when enabled. It does not reconstruct older server changes or files that RDrive never observed. Keep independent backups; the local history and optional APFS checkpoints are recovery aids on the same Mac.

## Enable history for a connection

History settings belong to the shared RDrive configuration. Configure them through the program rather than editing the state directory or maintaining a second Finder configuration. Each connection can enable history and set a version count, maximum age, and space budget. Defaults are **20 versions per item, 30 days, and 5 GiB** when history is enabled. A blank space limit in Finder means unlimited.

For example, enable history for an existing connection named `nas`:

```sh
rdrive history config --profile nas --enabled true \
  --keep 20 --max-age-days 30 --max-bytes 5368709120
rdrive history status nas
```

Use `--unlimited` instead of `--max-bytes` to remove the history byte target. Omit `--profile` when changing the default policy. An explicit connection policy overrides the default.

History is disabled in older configurations until explicitly enabled. Settings apply to future changes; disabling capture does not silently delete existing retained content.

In Finder, select one Smart file or folder, choose **View File History…**, adjust **Preserve history for this connection**, **Versions**, **Days**, and **MiB**, then choose **Save History Settings**. This updates the shared connection policy. The command-line settings use the same policy.

## What is retained

Before a protected replacement or supported deletion, RDrive stores immutable content and a manifest in the private state directory. Files are identified by SHA-256 and verified when exported or recovered. Directory recovery records include descendant files and empty directories. Selected synchronized metadata is retained with the content.

Retained content is plaintext inside the private state directory. RDrive validates ownership and private paths; it does not provide a separate encrypted history vault. Protect the Mac and its backups accordingly.

A history entry identifies its connection, path, item, capture reason, date, size, and publication state:

- **Prepared** means the before-image was retained but the associated remote publication has not been durably confirmed. An interrupted operation does not justify deleting this recovery copy.
- **Confirmed** means RDrive recorded the successful operation. Retention may reclaim confirmed entries according to the connection policy.

Histories are bound to the connection's remote account. Changing an endpoint or account cannot silently apply another account's old history. Stable item identities and recorded path aliases allow history to follow supported rename operations. Selecting a folder includes retained descendants, including items that have since been deleted.

Automatic local capture covers supported Smart changes, immediate and queued `put` overwrites, `rm`, and guarded `cp`/`mv` overwrite destinations. Explicit legacy version restores preserve the current target before replacement. Ordinary `sync` and `watch` preserve the remote before-image before guarded file replacements and propagated remote file deletions. Their local overwrite/deletion recovery continues to use the existing local `.rdrive-versions` behavior; it is separate from this retained remote history. Directory deletion is not automatically propagated by ordinary synchronization.

Operations performed by another client, directly on a native network mount, or directly on the server are outside RDrive's capture path. Managed repositories have their own immutable snapshot format; ordinary file history does not turn multiple remote writes into a single transaction.

## Browse and preview in Finder

Select one file or folder in an RDrive Smart connection and choose **View File History…**. The table shows the saved date, reason, original path, size, state, and source. Folder history includes retained deleted children. Use **Previous** and **Next** to browse additional pages.

Select a retained entry and choose **Preview**. RDrive exports a verified temporary copy into its private transfer area. The preview is read-only and never opens the current Finder file for writing. Text previews show at most 256 KiB; binary files require an exported copy. Folder previews list up to 200 entries.

**Export Copy…** saves a verified file or directory to a new destination. RDrive checks file hashes, sizes, and directory members and refuses symbolic links, unsafe paths, unrecorded members, or an existing destination. An export does not modify the server. Temporary preview/export content is cleaned after the operation; the retained history remains available.

The CLI reads the same retained content:

```sh
rdrive history files nas
rdrive history files nas Documents --offset 0 --page-size 100
rdrive history export nas VERSION_ID ./historical-copy.txt
```

Use `--item-id` to select a stable item identity instead of relying on its current path. Export destinations must be new; RDrive does not overwrite an existing local file.

## Recover a version safely

**Restore Copy** is the default recovery action. RDrive prepares a destination and creates a separate remote item, preserving the current one. Use this to inspect or compare historical content before deciding how to merge it.

**Replace Current…** is available for files. RDrive first prepares a plan bound to the selected history entry, account, destination, and current remote revision. The confirmation shows the destination. Only after confirmation does RDrive apply the plan. The current version is retained before replacement, and a newer remote change causes the operation to stop rather than overwrite that change.

The CLI also requires a preview before applying a prepared plan:

```sh
rdrive history restore nas VERSION_ID --to Documents/recovered-copy.txt
rdrive history restore nas VERSION_ID --plan PLAN_ID --apply
```

The first command prints the plan identifier and destination. Use the returned identifier and the same connection/version when applying. The saved plan fixes its target and mode, so omit `--to` and `--replace` from the apply command. Omit `--to` to let RDrive choose a separate copy name. To replace a file, use `--replace` in the preview command and review the current version before applying.

A stale plan must be prepared again. Recovery never accepts a client-provided replacement revision as authority. Directory replacement is intentionally unavailable; restore a directory as a copy and review it before reorganizing the current tree.

A successful recovery is a new remote operation. It does not replay an old queue, roll back the index, restore an old state database, or assume that an uncertain earlier upload failed. Keep the retained recovery copies while resolving an ambiguous server result.

## Retention and space

The history budget limits logical retained content bytes per connection/account; deduplicated blobs can occupy less physical space. It excludes SQLite, metadata, receipts and control-file overhead and is not a hard filesystem quota. The history budget is independent of the [clean-cache budget](CACHE_MANAGEMENT.md). Retained history, unsent edits, transfer staging, indexes, and optional system snapshots all consume space in addition to clean cached files.

Retention considers the version count, age, and space policy. Prepared entries remain protected until the publication is resolved, so actual usage may temporarily exceed the configured target. Reclamation occurs after confirmation, never before the destructive operation has succeeded. Review a prune preview before applying explicit cleanup.

```sh
rdrive history prune nas
rdrive history prune nas --apply
rdrive history clear nas
rdrive history clear nas --apply
```

`prune` applies the retention policy; `clear` selects eligible confirmed entries regardless of their age. Both preview by default and retain protected prepared entries.

If the history budget cannot hold the required before-image, RDrive reports `insufficient_quota` and preserves both the remote item and the pending local edit. Smart Synchronization marks that edit **Needs attention** and stops automatic retries. This is a local history budget problem; reconnecting does not free the required space.

Use the shared history settings to increase the budget, or preview and apply cleanup of eligible confirmed history. Prepared entries remain protected. Then find the retained request and explicitly retry it:

```sh
rdrive history status nas
rdrive history config --profile nas --max-bytes 10737418240
rdrive history prune nas
rdrive history prune nas --apply
rdrive smart list
rdrive smart retry DOMAIN_ID MUTATION_ID
```

Replace `DOMAIN_ID` and `MUTATION_ID` with the identifiers from `smart list`. Increasing the budget or pruning history does not silently restart failed work. The explicit retry retains the original local payload and remote version conditions; a newer remote change still causes a conflict. A resident service processes the retry normally. With the service stopped, `rdrive smart run` performs one replay pass.

Do not remove history blobs, manifests, SQLite files, or retained Smart snapshots by hand. A manifest and its verified contents form one recovery record; deleting pieces makes recovery impossible. Use the program's configuration, history, and cleanup commands.

## Optional APFS checkpoints

APFS checkpoints capture local RDrive state on a compatible macOS volume. They are optional and disabled by default. They supplement per-file history by retaining local payloads such as offline edits that have not reached the server.

RDrive reports whether the state directory is on a supported APFS volume and records the exact volume and snapshot identities. Snapshot creation and read-only recovery may require macOS authorization. A prepared local checkpoint is confirmed only after RDrive observes the new system snapshot and verifies its recorded content.

```sh
rdrive checkpoint status
rdrive checkpoint list
rdrive checkpoint config --enabled true --max-records 64
# Authorized OS operations run in your terminal, with the resident service stopped.
rdrive service stop
rdrive checkpoint create --authorize
rdrive service start
rdrive checkpoint inspect CHECKPOINT_ID
```

`--authorize` permits a terminal authorization prompt for the fixed snapshot or read-only mount command. The background service cannot receive a terminal password. Stop the service through `rdrive service stop` before an authorized operation, and start it again with `rdrive service start` afterward, including after a failed operation. Neither command deletes retained edits. An unavailable or unauthorized snapshot is reported explicitly.

`--max-records` limits local checkpoint records; it does not delete Time Machine snapshots. Inspect a record to identify the exact retained payload before exporting. Checkpoint export requires a new absolute destination:

```sh
rdrive checkpoint export CHECKPOINT_ID smart/DOMAIN/dirty/PAYLOAD.data \
  "$HOME/Desktop/recovered-edit.data" --authorize
rdrive history import nas Documents/recovered-edit.txt "$HOME/Desktop/recovered-edit.data"
```

Use the imported history entry with the normal restore preview/apply flow. `checkpoint reconcile CHECKPOINT_ID --authorize` can verify a prepared record after an uncertain system command. `checkpoint forget CHECKPOINT_ID` previews local metadata cleanup; add `--apply` to forget that record.

Recovery exports a recorded local payload to a new location and verifies its size and SHA-256. It does **not** replace the live state directory or replay old queues. A recovered file can subsequently be restored through a new guarded operation after reviewing the current remote version.

macOS manages snapshot storage and may remove snapshots. A local record alone does not prove that its snapshot remains available. RDrive reports an unavailable or unconfirmed checkpoint instead of assuming recovery is possible. Forgetting a local record does not delete a system snapshot. There is no whole-volume rollback command or automatic Time Machine snapshot pruning.

## Existing version and conflict tools

`history --limit N` remains the local **completion-event log**. It is distinct from retained file contents.

`versions save/list/restore/prune` manages the older **server-side file version format** under `.rdrive-versions`. That format remains file-only and separate from local file history. Existing automatic Smart version policy and remote recovery markers retain their documented behavior.

[Conflict repair](CONFLICTS.md) resolves pending edits and revision conflicts. File history provides prior content for inspection and recovery; it does not choose a merge strategy or weaken the conflict guard. [Managed repositories](REPOSITORY.md) provide a separate complete-snapshot model for cooperating clients.

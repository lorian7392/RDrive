# Directory indexing and incremental refresh

RDrive 0.6 moved Smart / File Provider directory and change records into SQLite. After connecting to WebDAV or SMB, the background service lists and publishes the root first, then scans other directories breadth first. It gradually indexes the complete tree by default, with higher priority for folders the user opens. Scans read metadata; they do not download file contents for offline use.

## Queries and refresh

Finder directory, item, and change queries read SQLite directly. A failed best-effort browse hint does not discard an otherwise valid cached listing; an explicit refresh still reports a failure to save its request. A folder query selects immediate children by `parent_id` instead of deserializing a whole-tree JSON document. Item queries use their primary key or path, and change queries use a persistent anchor. Network listings, metadata enrichment, and upload replay run in the background. Reading an indexed folder does not acquire the network worker's state lock.

The default refresh interval is 300 seconds, or 30 seconds for browsed or pinned directories. Each directory has independent failure backoff. Scheduling balances current browsing, undiscovered subtrees, and old-directory refresh. Up to four connections run metadata reads concurrently without holding the global mutation coordinator. A slow or unreachable connection no longer blocks the complete indexing loop; additional connections wait for a read slot. Publications recheck the current configuration and catalog before committing, and uploads retain their serialized recovery and revision checks. Within one connection, a request still needs to finish or time out before its next scan. Background work reuses connections for the same account configuration and recreates clients when connection security settings change.

Incremental refresh means listing, comparing, and publishing one directory at a time. Only changed SQLite rows are updated, and Finder receives added, changed, and deleted events. Ordinary WebDAV still requires a complete listing of that directory; a server-wide change journal is not required. Unscanned, failed, or incomplete listings are never interpreted as an empty folder. Writes and replacements still check current server revisions; the display index does not authorize a write.

## Commands

The examples use an existing Smart domain named `cloud-smart`. List domains with `provider domains`.

```sh
rdrive index status
rdrive index status cloud-smart

# Gradually prebuild the entire directory tree in the background.
rdrive index config --enable --all

# Root depth is 0. Scanning depth-2 directories also discovers their immediate children.
rdrive index config --enable --depth 2
rdrive index config --refresh-seconds 300 --active-refresh-seconds 30

# Disable general prebuilding; root, browsing demands, and pinned directories can still refresh.
rdrive index config --disable

# Queue a one-time request; provider serve performs the actual refresh.
rdrive index refresh cloud-smart documents
rdrive index refresh cloud-smart documents --recursive
```

In Finder, select one Smart folder and choose **Refresh Index** from its context menu. This scans that folder’s immediate children. To refresh the entire connection, open the full RDrive companion app and choose **Refresh Connection Index** in its menu bar menu, or run the recursive CLI command above with no path. The resident service’s notification helper runs without a menu bar icon; the connection menu belongs to the full app. Sidebar and blank-space menus may not expose a File Provider custom action. For a managed repository, the root action refreshes its current snapshot.

Finder and the CLI submit a request for the provider service to process in the background. A manual request bypasses the normal freshness interval. An exact request for an unscanned folder takes priority over inherited recursive work on normal priority turns; fair refresh/crawl turns and retry backoff still apply. The service still respects synchronization filters, pause, and retry backoff. An ordinary folder refresh reads the metadata for its immediate children. Use the CLI’s `--recursive` option when you need to scan a subtree. The companion’s connection action submits a recursive root request. Cached contents remain readable while a scan is pending; an unscanned directory still requires its first successful listing.

A recursive refresh can exceed the general prebuilding depth limit. Once the currently known subtree has been scanned, its one-time request is persistently marked complete. Running the command again creates a new request; ordinary browsing does not overwrite an unfinished manual refresh. Paths are relative to the domain root, and the target directory itself must already be indexed. If it is not, refresh the nearest known parent first. Omit the path, or use an empty path, for the root.

`index status` works offline and reports policy plus item, directory, scanned-directory, and pending-directory counts for each domain. `pending` counts discovered directories not yet successfully scanned, including those blocked by depth policy, disabled prebuilding, or errors. It is not a count of active tasks. `queued` means only that a request was saved. Keep `provider serve` running to continue scanning; running `info` or mounting an ordinary native volume does not start this service.

SMB servers supporting full-identity directory enumeration return names, sizes, timestamps, and 128-bit file IDs in bulk. RDrive uses that metadata to build the same weak revision tokens as an individual metadata lookup. Servers explicitly rejecting that information class use the compatible per-child fallback, with at most eight complete metadata lookups in flight. Started lookups finish their cleanup before an error is returned. Authentication, permission, and network failures remain errors; directory metadata tokens never replace strong content verification or guarded overwrite checks.

The native notification helper bootstraps each connection in bounded pages and then follows its change anchor. Concurrent changes are reconciled after the final page. A failed connection does not prevent notifications for another connection.

## Storage and upgrades

Each domain uses `RDRIVE_HOME/provider/<domain-hash>.sqlite3`. Its binding includes connection identity, server root, and repository mode. SQLite uses local WAL and transactions. Items, change anchors, scan records, upload receipts, and recovery control state commit together; a failed transaction cannot publish half a directory. The database and its auxiliary files are private state and must not be committed to Git or placed in a synchronized directory.

First use atomically migrates the old `<domain-hash>.json` under a private lock and preserves its original bytes. SQLite then becomes authoritative; the JSON file is a historical copy from before migration. Corruption, unknown formats, or account-binding mismatches return errors and retain data. RDrive does not silently reimport old JSON, discard tasks, or reconstruct write receipts. Back up an active database using SQLite's backup facilities rather than copying only its main file and omitting WAL.

Stop older background processes sharing the state directory before upgrading. Do not let an old process keep writing JSON or reopen migrated state with an old version. File Provider bridge messages keep their existing format, and a newly built companion can embed the updated CLI. This optimization applies to Smart views. macOS still manages directory caching for ordinary `rdrive mount` volumes.

## Validation

```sh
cargo test --locked
python3 tests/index_workflows.py ./target/release/rdrive
```

The temporary-state, local synthetic WebDAV fixture covers automatic prebuilding without content downloads, folder reads during slow scans, incremental changes, failure preservation, depth policy, recursive refresh, migration recovery, and query plans. Validation reports record performance samples and timing methods. Local database query latency does not describe the time required for a real cloud service's first listing.

See SQLite's [WAL documentation](https://sqlite.org/wal.html) and [transaction documentation](https://sqlite.org/lang_transaction.html) for isolation and transaction behavior.

## Offline index search

Introduced in 0.7, `index search DOMAIN QUERY --scope PATH --kind file --page-size 100 --offset 0` searches local names and paths without traversing the server. Queries of at least three characters use FTS5 trigram; shorter queries use escaped SQLite LIKE. File contents are not searched. Current filters apply, results cover only indexed data, and queries work offline. Schema 2 adds derived search rows transactionally when opening schema 1, preserving all control and recovery state. See [Smart operations](SMART_OPERATIONS.md).

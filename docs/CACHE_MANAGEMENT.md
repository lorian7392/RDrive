# Cache management

RDrive accounts for two kinds of clean content: the Rust client's cache and downloaded Finder copies managed by macOS File Provider. Both contribute to the shared capacity budget. The `cache clear` and `cache directory` commands introduced in 0.8 manage the Rust cache. They do not move Finder's system-managed storage, upload snapshots, offline edits, directory indexes, connection profiles, or version history.

## Clear the cache

```sh
# Preview without removing files.
rdrive cache clear

# Remove reclaimable content.
rdrive cache clear --apply
rdrive cache status
```

Clearing considers only cache objects recorded in the state database. Pinned files, files being read through RDrive, and unrecorded files remain untouched. A filename that resembles a cache UUID is not sufficient authority to delete it. Results list candidate files and bytes, actual removals, and skipped pinned or active objects.

To reclaim pinned content, explicitly unpin the relevant files first:

```sh
rdrive cache unpin dav documents --recursive
rdrive cache clear --apply
```

This does not delete remote files. Reclaiming the only offline copy means that the next access needs a download. For system-managed Finder copies, use Finder's Allow Automatic Removal / Remove Download actions or the native trim command:

```sh
# Ask macOS to reclaim removable Finder copies. Pinned and unsent content stays protected.
rdrive provider native -- --trim-cache 0
```

Use the companion App associated with the currently registered domains. To select another bundle, pass `--app /absolute/path/RDrive.app`. macOS can refuse eviction for files that are open, pinned, or contain unsent changes; inspect the returned result for what was actually reclaimed. Do not manually delete files under `~/Library/CloudStorage` to simulate cache clearing.

## Change the cache directory

```sh
# Show the current path.
rdrive cache directory

# The parent must exist. The destination may be absent or an empty private 0700 directory.
rdrive cache directory /absolute/path/RDriveCache --dry-run
rdrive cache directory /absolute/path/RDriveCache

# Move existing content back to cache under the default state directory.
rdrive cache directory --default --dry-run
rdrive cache directory --default
```

RDrive rejects relative paths, symbolic links, non-private directories, nonempty destinations, and locations overlapping its state directory, known synchronization roots, or system File Provider storage. Separate state directories cannot share a cache root. Each custom root has a private ownership marker bound to its state directory and directory identity; an existing user folder is never silently adopted as a cache.

Migration includes pinned content and preserves revisions, hashes, pin policies, and connection ownership. Active downloads or readers prevent the change and leave the existing configuration intact; retry after they finish. RDrive copies and verifies every payload with SHA-256, saves the new configuration once, then removes old copies. Migration can cross disks, but the destination needs room for the entire cache. Do not rely on it to free source space while copying.

The migration plan is persisted in the state directory. If the process exits before configuration commit, the next startup removes this migration's incomplete copies and keeps the old path. After configuration commit, it verifies the new copies before completing old-copy cleanup. Corruption, unexpected files, or changed directory identity stop recovery and preserve data. Deleting the migration plan is not a repair procedure. Directory indexes, upload queues, and native Finder storage keep their own locations.

Unrecorded files in the source stop migration without deleting any content. Review those files separately; cache clearing will not remove them. Returning to the default path also requires a destination free of unexpected content.

## MCP and Skills

- `rdrive_cache_directory` queries the current path without writing.
- `rdrive_cache_set_directory` accepts `directory`, or `default: true` to restore the default; `dry_run: true` previews the change.
- `rdrive_cache_clear` previews by default and requires `apply: true` to remove content.

Write tools require MCP `--allow-write`, and the destination must fall within an authorized `--local-root`. General upload, download, and export tools reject the cache root and migration directories even when they lie inside a broader authorized local directory. Read cached content by profile and remote path with `cache get` or its MCP equivalent; internal object names must not bypass connection ownership.

Older background processes do not understand custom cache paths. Before enabling this feature, upgrade every bridge using the same state directory to 0.8, stop older processes normally, and retain the existing state. `cache directory` does not configure Finder's download location or relocate the whole RDrive state directory.

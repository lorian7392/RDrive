# Interrupted transfers and native mounts

## Transfer recovery

Keep the same state directory and exact transfer source/destination. `get --resume` verifies the retained on-disk prefix and remote revision before requesting the remainder. A revision or local-identity mismatch stops recovery; preserve the partial data and report the conflict.

SMB resumable uploads verify the remote staging identity and uploaded prefix before continuing. WebDAV uploads resume only for an explicitly configured Nextcloud upload endpoint. Ordinary WebDAV does not have a universal safe segmented PUT protocol; do not claim `--resume` can resume every WebDAV upload.

```sh
rdrive --json transfers list
rdrive --json queue list
rdrive --json queue run nas --retry-failed
```

Queued source snapshots are independent copies. A successful enqueue does not mean remote publication succeeded. Interrupted or failed unconditional overwrites need the additional CLI `--retry-ambiguous-overwrite` because the previous write might already have reached the server. This option is deliberately not exposed through MCP. Inspect the destination and retained job first; do not choose this option to suppress a version conflict.

`queue cancel` deletes its local upload snapshot; it differs from Smart cancellation, which retains its snapshot. Do not cancel merely to clear an error display when the retained copy may be the user's only data.

SMB protected replacement checks content and identity, saves the original under a recovery name, and publishes the complete new file. There can be a short missing-path interval; ordinary SMB replacement is not a cross-crash atomic transaction. Incomplete recovery receipts block treating that interval as a user deletion. Preserve `.rdrive-backup-*`, `.rdrive-recovery-*`, and staging files during diagnosis. Use managed transactions if the requested guarantee is atomic visibility across multiple files.

## Native macOS mount

```sh
# The mountpoint must already exist, be empty, owned by the user, and mode 0700.
rdrive --json mount dav ./mountpoint --dry-run --no-prompt
rdrive --json mount dav ./mountpoint --browse --no-prompt
rdrive --json mounts
rdrive --json unmount ./mountpoint
```

Use `rdrive_mount_plan`, `rdrive_mount`, `rdrive_mounts`, and `rdrive_unmount` through MCP. Mount calls are noninteractive. Native SMB relies on credentials available to macOS; it does not consume a profile's password environment automatically. Profile credentials can be passed privately to the native WebDAV helper. If credentials are unavailable, report that condition without embedding a password in the command.

Verify the returned operating-system mount details and actual mountpoint before claiming success. `--dry-run` only returns a plan. `--read-only` requests a real read-only volume. Avoid force-unmount unless requested or required by a clearly authorized recovery action.

Native SMB/WebDAV mounts use macOS network filesystems and operating-system caches. They do not automatically create Smart File Provider placeholders, use RDrive's clean-cache budget, or obey Rust transfer rate limits. Finder Smart behavior requires an independently configured and running File Provider domain.

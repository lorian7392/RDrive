# Quick start

Start with the standalone CLI. Follow [installation](INSTALLATION.md) to build with Rust 1.90+. Examples use macOS's default interactive zsh; substitute `./target/release/rdrive` for `rdrive` if needed.

Replace example endpoints and accounts with your own. Try modifying commands in a dedicated test directory first. The [CLI reference](reference/CLI.md) lists all arguments.

## Authentication and connections

### Read a password in the current terminal

~~~sh
read -rs 'RDRIVE_PASSWORD?Password: '
printf '\n'
export RDRIVE_PASSWORD
~~~

The input is hidden. Do not put password literals into commands, URLs, or scripts. The variable exists in this terminal and its children; it is not a Keychain entry and is not automatically available to a LaunchAgent or another MCP client.

A profile records the variable name and reads its value when connecting. Enter the correct password for each account, or use a separate variable for each profile.

### Add WebDAV

~~~sh
rdrive profile add dav --protocol webdav \
  --url https://dav.example.com/files/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive profile list
rdrive info dav
rdrive ls dav
~~~

Use the account's DAV file root, not a website login page. HTTPS and certificate verification are the defaults. `--allow-http` permits an explicitly trusted HTTP test service. Authentication currently supports Basic; OAuth, Digest, client certificates, and custom CA configuration are not provided.

### Add SMB

Enter the SMB account's password, then create its profile:

~~~sh
rdrive profile add nas --protocol smb \
  --url smb://nas.example.com/share/folder \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info nas
rdrive ls nas
~~~

`share` is the share name; `folder` is an optional subtree. SMB transport encryption is required by default. `--allow-unencrypted-smb` is an explicit compatibility-test option, not an automatic fallback. SMB1, Kerberos/SSO, DFS, and service discovery are not provided.

Profile names accept letters, digits, `-`, and `_`. Existing names cannot be overwritten. `--timeout 300` changes the default 120-second timeout when adding a profile. To change a profile, read the [removal restrictions](reference/CONFIGURATION.md) first; do not delete state files directly.

### Use macOS Keychain

In Keychain Access, create a generic-password item with service `rdrive:PROFILE_NAME` and an account exactly matching `--username`. Enter the password through the system UI. Create the profile without `--password-env` to use that item. For profile `dav-keychain`, the service is `rdrive:dav-keychain`.

A profile with `--password-env` explicitly requires that variable; a missing variable does not trigger a Keychain fallback. WebDAV without a username or password variable can connect anonymously. Reading Keychain may require system authorization.

Native SMB mounts use macOS credentials or a terminal prompt independently of CLI transfer credentials. Background services do not save terminal passwords for you.

## Browse and transfer files

Remote paths are relative to the profile URL. Use an empty path for the root and `/` as the separator. Absolute paths, `..`, and backslashes are rejected. Quote paths containing spaces.

~~~sh
rdrive ls dav
rdrive ls dav documents --recursive --contains .pdf
rdrive stat dav documents/report.pdf
rdrive get dav documents/report.pdf ./report.pdf
rdrive cat dav notes.txt --max-bytes 1048576
rdrive url dav documents/report.pdf
~~~

`url` returns a resource address; it does not change permissions or create a public/signed share. `cat` writes file bytes to stdout. Use it for known text or redirect its output rather than printing unknown binary data to a terminal.

Use a new test directory for this example. Skip `mkdir` if that directory already exists.

~~~sh
printf 'Hello from RDrive\n' > ./hello.txt
rdrive mkdir dav rdrive-demo
rdrive put dav ./hello.txt rdrive-demo/hello.txt
rdrive cp dav rdrive-demo/hello.txt rdrive-demo/hello-copy.txt
rdrive mv dav rdrive-demo/hello-copy.txt rdrive-demo/hello-renamed.txt
rdrive get dav rdrive-demo/hello.txt ./downloaded-hello.txt
~~~

Downloads, uploads, copies, and moves preserve an existing destination by default. Explicit replacement uses `--overwrite` and retains the operation's protocol-specific conditions and recovery behavior; it is not an unconditional backup guarantee.

File and directory deletion are explicit:

~~~sh
rdrive rm dav rdrive-demo/hello-renamed.txt
rdrive rm dav rdrive-demo --recursive
~~~

Ordinary `rm` has no universal remote trash. Retain content first through a download, custom version, or recoverable Smart namespace deletion as appropriate.

Files with a reliable revision can be edited locally:

~~~sh
EDITOR=/usr/bin/vi rdrive edit dav notes.txt
~~~

`EDITOR` must name one executable without additional arguments. Remote changes stop publication and preserve the local result. See [conflict recovery](TROUBLESHOOTING.md#synchronization-or-edit-conflicts).

## Large files, queues, and progress

Repeat the same `get --resume` after interruption. RDrive checks the partial data and remote revision before continuing:

~~~sh
rdrive get dav large.zip ./large.zip --resume
rdrive transfers list
~~~

SMB uploads support checkpointed resumption, directly or through the persistent queue:

~~~sh
rdrive put nas ./large.zip uploads/large.zip --resume --queue
rdrive queue list
rdrive --jobs 3 --limit 1048576 queue run nas
rdrive status
rdrive history --limit 50
~~~

The destination parent must exist. `--queue` confirms that an independent upload snapshot was retained; `done` requires server confirmation. A job created without `--resume` restarts its payload on retry. Failed and ambiguous-overwrite retries need explicit options; inspect [the recovery procedure](TROUBLESHOOTING.md#upload-failure-or-an-uncertain-result) first.

`--limit` is shared payload bytes per second for the Rust process. `--jobs` controls queue scheduling from 1 to 64 targets, with a default of 4. Neither controls other applications' I/O through native network mounts.

~~~sh
rdrive --progress auto get nas large.zip ./large.zip --resume
rdrive --json --progress json queue run nas 2>progress.jsonl
~~~

Results go to stdout and progress to stderr. Structured results use formatted JSON by default; `--json` selects single-line JSON. `--progress off` disables transfer progress. Terminal prompts and system tools have separate diagnostics.

Ordinary WebDAV has no general upload-resume protocol. For Nextcloud, explicitly configure a same-service v2 uploads endpoint:

~~~sh
rdrive profile add nextcloud --protocol webdav \
  --url https://cloud.example.com/remote.php/dav/files/demo/ \
  --nextcloud-upload-url https://cloud.example.com/remote.php/dav/uploads/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive put nextcloud ./large.zip large.zip --resume
~~~

The protocol is tested locally; deployment compatibility needs separate verification. Final publication is followed by a full target read for SHA-256 verification, which adds traffic.

## Prepare an offline cache

~~~sh
rdrive cache config --max-bytes 5368709120
rdrive cache fetch dav documents/report.pdf --pin
rdrive cache fetch dav documents --recursive --pin
rdrive cache status
rdrive cache list
~~~

Export an existing cache copy offline, without contacting the server or requiring its password:

~~~sh
rdrive cache get dav documents/report.pdf ./offline-report.pdf
~~~

RDrive verifies the cached SHA-256; this does not establish that it is the server's latest revision. Ordinary `get` does not pin content. Use `cache fetch` to prepare explicit cached copies.

~~~sh
rdrive cache unpin dav documents --recursive
rdrive cache prune --max-bytes 1073741824
rdrive cache clear
~~~

`prune` performs eligible capacity reclamation. `cache clear` previews by default and needs `--apply` to remove content. Pinned, active, and unsent content is protected. See [cache management](CACHE_MANAGEMENT.md) for migration, native downloaded copies, and the shared budget.

## Synchronize a local directory

Both roots must already exist. Establish the relationship in a test directory and preview its plan:

~~~sh
mkdir -p ./work
rdrive mkdir dav work
rdrive sync dav ./work work
~~~

Review the plan, then choose a direction:

~~~sh
rdrive sync dav ./work work --apply
rdrive sync dav ./work work --direction push --apply
rdrive sync dav ./work work --direction pull --apply
~~~

The default `both` is two-way synchronization. `push` and `pull` still check baselines and conflicts; they are not unconditional mirrors. An initial missing counterpart does not establish deletion. Without a common baseline, existing files at the same path establish a relationship only after verified equal content and a stable revision.

Add `--delete` to propagate confirmed deletions. Local replaced/deleted files are retained under the sync root's `.rdrive-versions/`. Operations stop if remote protection is insufficient. Ordinary sync does not recursively remove remote empty directories and does not provide cross-file atomicity.

~~~sh
rdrive sync dav ./work work --delete
rdrive sync dav ./work work --delete --apply
rdrive watch dav ./work work --apply --interval 30
~~~

`watch` polls in the foreground until Ctrl-C. Without `--apply` it keeps previewing. A one-shot `sync`, `put`, or `queue run` does not leave a hidden service behind.

Add `--metadata` explicitly for selected tags and extended attributes. See [features](FEATURES.md) for complete synchronization, naming, type-conflict, and server limits.

## Use Finder

Native network volumes use macOS's SMB/WebDAV filesystem:

~~~sh
mkdir -m 700 "$HOME/RDriveMount"
rdrive mount nas "$HOME/RDriveMount" --read-only --dry-run
rdrive mount nas "$HOME/RDriveMount" --read-only
rdrive mounts
rdrive unmount "$HOME/RDriveMount"
~~~

The mountpoint must exist, be empty, be owned by the current user, and have mode `0700`. Remove `--read-only` to request writes; add `--browse` to request Finder visibility. SMB uses existing system credentials or a terminal prompt. `--no-prompt` only tries existing credentials. See [native mounting](MOUNT.md).

For placeholders, on-demand downloads, Keep Offline, and offline edits, configure a Smart File Provider separately. It needs the companion, accepted/enabled extensions, a bridge using the same IPC directory, and the notification helper. `smart setup` alone does not establish a visible Finder domain. Follow [Smart setup](SMART_SYNCHRONIZATION.md); see [Smart management](SMART_OPERATIONS.md) for services, filters, versions, and conflicts.

## Next steps

- [Managed repositories](REPOSITORY.md): publish complete snapshots of multiple files and selected metadata; ordinary directories are not imported automatically.
- [Directory indexing](DIRECTORY_INDEXING.md): metadata prebuilding, offline filename/path search, depth, and refresh policy. Search does not inspect file contents.
- [MCP and Skills](MCP.md): read-only by default, with explicit write enablement and local scopes. Pass `--binary ./target/release/rdrive` when generating a source-build configuration.
- [Configuration](reference/CONFIGURATION.md): complete state, authentication, cache, and background settings.
- [Troubleshooting](TROUBLESHOOTING.md): connection, conflict, ambiguous-upload, and recovery procedures.

When this terminal no longer needs the password, run `unset RDRIVE_PASSWORD`. This does not delete Keychain items or change an already-running child's environment.

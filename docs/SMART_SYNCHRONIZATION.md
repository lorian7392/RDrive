# Smart Synchronization

Smart mode uses macOS File Provider to manage placeholders, downloaded content, and offline edits. A Rust bridge connects the domain to WebDAV or SMB. Directory metadata lives in SQLite and is prebuilt breadth first, then refreshed incrementally. Ordinary `rdrive mount` volumes use the system network filesystem: they do not become Smart domains or use RDrive's clean-cache capacity limit.

## Enable Smart mode

Configure a connection, then create a separate domain. Creating a Smart domain sets a 5 GiB limit if no numeric cache limit exists, including when `--unlimited` was previously selected. An existing numeric limit is preserved. To use unlimited capacity, run `cache config --unlimited` again after creating the domain.

```sh
rdrive smart setup cloud-smart dav --display-name "RDrive Smart"
rdrive cache config --max-bytes 5368709120
rdrive cache status
```

Development bundles record an explicit IPC directory at build time. Start the bridge using that same private path; replace the example below with the path recorded in your bundle. Credentials come from the connection's environment or Keychain configuration, never from command arguments.

```sh
rdrive provider serve --socket /private/tmp/rdrive-user-ipc/bridge.sock \
  --staging /private/tmp/rdrive-user-ipc/Transfers --development-tcp
```

In another terminal, register the native domain and keep the notification helper running:

```sh
rdrive provider native -- --register-domain cloud-smart --display-name "RDrive Smart"
rdrive provider native -- --domain-root cloud-smart
rdrive provider native -- --watch-domains
```

`--watch-domains` notifies Finder in the foreground without requiring the menu-bar App. Keep both the bridge and notification helper running so remote additions and newly discovered children of pinned folders continue to appear. After Ctrl-C stops the helper, existing downloaded copies remain usable, but new changes can take longer to appear. The full companion App performs this notification work itself.

`provider native` passes arguments directly to the companion; some shell/CLI versions also accept these arguments without the separating `--`. The App can manage the service, but its default state directory must match this `RDRIVE_HOME`. Only one service may own an IPC directory. Stopping the bridge stops background uploads. macOS must accept and enable a development build; production distribution requires developer signing and notarization.

Managed repository mode requires an initialized repository and `smart setup --repository`. An ordinary remote folder cannot be read directly as a managed repository.

## Everyday behavior

- The root is published to local SQLite first. Background work prebuilds other directory metadata, while folder reads use parent-index queries. Scans do not download contents. Open folders get priority, and unopened folders are gradually indexed too. Previously indexed data can be browsed offline; unscanned directories still require a connection. Configure depth and intervals with `index config`; see [directory indexing](DIRECTORY_INDEXING.md).
- File contents download on demand. Downloaded system copies can open offline; undownloaded files need a connection. RDrive does not represent placeholders as empty ordinary files.
- Keep Offline applies to a folder and its descendants, including descendants discovered later. Unpinning a parent does not remove separately selected pins on its children.
- Ordinary downloaded content may be reclaimed under space pressure. Pinned files, active readers, and unsent edits remain protected. Allow Automatic Removal removes a pin policy; it does not promise immediate eviction.
- Local edits are first retained as independent, verified immutable snapshots. The bridge attempts upload automatically and reloads pending work after interruption. Success receipts follow server confirmation; macOS also retains local changes that have not uploaded successfully.
- Concurrent remote changes stop publication by default and preserve the local snapshot. Explicit choices are keep both, archive then perform a guarded replacement, or accept the remote version. Automatic policies require separate configuration.

```sh
rdrive smart status
rdrive smart list cloud-smart
rdrive smart pause
rdrive smart resume
rdrive smart retry cloud-smart <mutation-id>
rdrive smart export cloud-smart <mutation-id> ./recovered.txt
```

`smart run` performs one reconnect pass. `provider serve` provides continuous automatic synchronization. Pausing stops background publication without undoing remote operations that already happened. Cancelling a task retains its local snapshot; exporting does not alter remote files.

Edits based on the same revision that have never started uploading may be coalesced when the newer change covers the older content. Started or uncertain edits retain separate snapshots. Consecutive local saves publish in order: later edits can adopt only a preceding, confirmed write receipt from this client, and external changes stop the replay chain. Repeated saves of a newly created file also retain copies until confirmation, then become guarded modifications.

Confirmed transfer payloads are reclaimed automatically, while receipts, hashes, and version conditions remain available for system retries. Cancelled or superseded but unconfirmed copies remain recovery data until explicitly removed with `smart discard`. Receipts identify content, metadata, and original versions, so a new system request UUID does not repeat a confirmed upload.

## Cache capacity

```sh
rdrive cache config --max-bytes 10737418240
rdrive cache status
rdrive cache prune --max-bytes 5368709120
rdrive provider native -- --trim-cache 5368709120
rdrive cache config --max-bytes 5368709120
```

The limit covers clean CLI cache payloads, clean File Provider downloads, and approved download reservations. Capacity is reserved before downloading. Rust payloads use LRU reclamation; native copies use the system eviction API. RDrive does not truncate or delete Finder files to simulate eviction.

Reducing the limit fails and preserves the old configuration if protected content cannot fit. Updating a pinned CLI cache item keeps the old copy until the new revision is verified, which may require extra headroom. Readers also protect the old revision. Unpin content you no longer need, then reclaim both cache types; macOS may still refuse to evict open files or unsent changes. `cache config --unlimited` explicitly removes the limit.

This is a clean-payload and reservation budget, not a partition quota or preallocated disk space. Databases, transfer staging, system overhead, retained edits, and recovery copies consume additional space. Insufficient space prevents acceptance of new edits; existing edits are not discarded to satisfy the cache limit.

Native usage is derived from the system's materialized set and actual file state, reported per domain. Last-known used bytes are retained conservatively; active download reservations require heartbeat renewal. Before removing a domain, clear its system downloads and report usage so stale accounting is not mistaken for free space.

## Validation and boundaries

See [SMART_VALIDATION.json](https://github.com/lorian7392/RDrive/blob/main/docs/SMART_VALIDATION.json) for automated results, [macOS integration](MACOS_INTEGRATION.md) for real Finder / File Provider observations, and [PROVIDER_RECEIPTS_VALIDATION.json](https://github.com/lorian7392/RDrive/blob/main/docs/PROVIDER_RECEIPTS_VALIDATION.json) for concurrent receipts and deletion crash recovery. These fixtures use local synthetic services and do not modify real cloud-drive content.

In ordinary WebDAV/SMB folders, content and separate attribute records commit in separate steps; other clients may observe one before the other. Use a managed repository to publish several files, content, and metadata atomically. Server confirmation and revision conditions still rely on correct server behavior.

See [Smart operations and management](SMART_OPERATIONS.md) for file/directory renaming, moving, recoverable deletion, filtering, resident services, and version/conflict policies.

# Managed repositories and crash recovery

A managed repository stores file content as immutable objects and stores the directory tree, stable file identities, and selected tags/extended attributes in complete snapshots. One transaction can add, replace, move, and delete several logical files. Readers see the previous snapshot before publication and the complete new snapshot after a successful commit. Ordinary remote-folder `put`, `sync`, and native SMB/WebDAV mounts do not acquire these transaction semantics.

## Usage

Create a dedicated remote directory first. Existing ordinary files are not automatically imported:

```sh
rdrive mkdir dav managed
rdrive repo init dav managed
rdrive repo begin dav managed
# Copy the transaction ID from the output; <TX> stands for that ID below.
rdrive repo mkdir <TX> documents
rdrive repo put <TX> ./report.pdf documents/report.pdf
rdrive repo put <TX> ./notes.txt notes.txt
rdrive repo status <TX>
rdrive repo commit <TX>
rdrive repo ls dav managed
rdrive repo get dav documents/report.pdf ./downloaded.pdf --root managed
```

Transactions also support `repo mv <TX> FROM TO` and `repo rm <TX> PATH --recursive`. `repo put` copies and verifies local source content when staging it; later source edits cannot change that transaction. `repo transactions` lists locally persisted records.

If commit fails, the process exits, or its success receipt is lost, keep the original state directory and retry **the same** `repo commit <TX>`. RDrive checks immutable commit slots and transaction identity to recover an already published result without creating another commit. `repo abort <TX>` is allowed only before commit preparation. A prepared transaction may already be visible to other clients, so its result must be reconciled first.

When two clients start from the same snapshot, only one can claim the next sequence number. The other reports a conflict and retains its staged content. It does not merge automatically or overwrite the winner. Read the latest snapshot, inspect conflicts, and create a new transaction before reapplying changes.

## Publication and recovery

- `.rdrive-repository/` contains `FORMAT.json`, `objects/`, `commits/`, and `staging/`.
- SHA-256 determines content paths. Before publication, RDrive downloads and verifies content objects and the staged manifest, then publishes the next commit slot using a server-enforced no-replace rename.
- Commit slots increase monotonically and include the previous commit's checksum. Readers validate the complete parent chain, content bindings, stable UUIDs, parent directories, and case/Unicode name collisions.
- There is no mutable HEAD that an older client can overwrite, and no reliance on remote locks that can remain after a crash. Concurrent commits compete for the same immutable slot.
- Local `transactions/` stores phase records and `spool/` stores source snapshots. If publication finishes before the local record is updated, replay recovers the committed identity from the server.
- File Provider version checks, reads, and new transactions use the same pinned snapshot. A later commit cannot attach new bytes to an old version marker or silently replace a newer commit.

The guarantee is **atomic visibility across client process crashes**, assuming correct same-filesystem atomic rename and no-replace behavior on the server. It does not establish recovery from server power loss, disk-controller caches, corrupt or malicious servers, or direct edits to control files outside the repository protocol.

## File Provider integration

```sh
rdrive provider domain-add managed-domain dav managed --repository --display-name 'Managed Documents'
rdrive provider domains
```

Register the same domain as described in [macOS integration](MACOS_INTEGRATION.md). This logical view exposes repository filenames and atomic snapshots in Finder. A direct native mount of the remote directory exposes the internal storage layout instead.

Managed File Provider domains support stable file/directory identities, creation, content and selected metadata updates, moves, renames, and deletion. Ordinary Smart folders also support protected moves, renames, and recoverable logical deletion through persistent plans, per-item revision checks, and recovery directories. Those are multi-step operations rather than one complete-tree snapshot publication. See [Smart operations](SMART_OPERATIONS.md).

## Limitations

There is no garbage collection, history compaction, automatic import/migration, branching, or automatic conflict merging. Old commits, unreachable content, failed stages, and local source snapshots consume space. Do not treat the control directory as an ordinary cache. Reads currently validate the complete commit history, and commits verify every referenced object, so overhead grows with history and content.

File tags and allowed extended attributes publish atomically with the snapshot. Directory tags, resource forks, ACLs, quarantine flags, and security attributes are outside this metadata scope. Local updates to several extended attributes have a separate recovery journal; macOS provides no atomic multi-attribute system call.

File Provider indexes and replay records bind to connection protocol, address, username, remote root, and repository mode. Repointing an existing domain ID at different storage is rejected while preserving its records. Create a new domain ID for new storage.

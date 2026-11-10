---
name: rdrive-transactions
description: Publish and recover RDrive managed-repository transactions with atomic snapshot visibility. Use for multi-file commits, transaction conflicts, or uncertain commit receipts; ordinary WebDAV or SMB folders do not have these semantics.
---

# RDrive managed transactions

Use this workflow when the user requests atomic publication of multiple logical files or recovery of an existing managed transaction. Preserve the selected connection, repository root, transaction identifier, and state directory. Managed format is separate from ordinary remote folders; do not initialize an existing collection expecting automatic import.

Prefer connected MCP `rdrive_repo_*` tools after inspecting their schemas. CLI fallback uses `rdrive --json --progress off ...`. MCP write operations require server `--allow-write`; paths reading or writing local files additionally require a configured `--local-root`. These settings do not add authorization beyond the requested operation.

## Inspect or prepare

```sh
rdrive --json repo transactions
rdrive --json repo status TX
rdrive --json repo ls dav managed
```

MCP inspection tools: `rdrive_repo_transactions`, `rdrive_repo_status`, `rdrive_repo_list`. Replace `TX`, `dav`, and `managed` with the actual selected values. If continuing an existing operation, load its recorded transaction instead of starting a new one.

For a newly requested repository, first create an empty dedicated remote directory if needed, then initialize it. The remote parent must exist; local source content is not imported implicitly.

```sh
rdrive --json mkdir dav managed
rdrive --json repo init dav managed
rdrive --json repo begin dav managed
```

Use `rdrive_mkdir`, `rdrive_repo_init`, and `rdrive_repo_begin` accordingly. Extract the actual transaction ID from the response and use it for all staging and commit calls.

## Stage and publish

```sh
rdrive --json repo mkdir TX documents
rdrive --json repo put TX ./report.pdf documents/report.pdf
rdrive --json repo put TX ./notes.txt notes.txt
rdrive --json repo mv TX old.txt archived.txt
rdrive --json repo rm TX obsolete.txt
rdrive --json repo status TX
rdrive --json repo commit TX
rdrive --json repo ls dav managed
```

MCP staging tools are `rdrive_repo_mkdir`, `rdrive_repo_put`, `rdrive_repo_move`, `rdrive_repo_remove`; publish with `rdrive_repo_commit`. The examples are independent actions: inspect actual entries before moving or removing existing paths. Staging logical removals or replacements is part of the requested transaction, not a live ordinary-file operation. Use recursive removal only for the directory scope the user requested. Staging content makes a verified independent snapshot; later changes to the local source do not update staged content.

Review staged paths before committing. `repo commit` publishes one complete new tree containing content objects, stable file identities, and selected tags/attributes. Describe success only after server publication is confirmed, not after staging. Download a logical repository file with:

```sh
rdrive --json repo get dav documents/report.pdf ./downloaded.pdf --root managed
```

The MCP equivalent is `rdrive_repo_get`. A native network mount of the raw repository root exposes control files; it is not the logical managed file view.

## Recover uncertainty or conflict

If a commit times out, the process exits, or the receipt is lost, preserve local state and **replay the same `repo commit TX`**. Publication uses immutable commit slots and transaction identity to recognize an already published commit. Starting a replacement transaction or manually editing the control files can obscure this recovery.

Concurrent writers from the same parent snapshot race for the next commit slot. A losing transaction reports conflict and retains its staged content; it does not overwrite the winning commit. Read the current repository, inspect the actual changes, and create a new transaction only when the requested resolution is clear. There is no automatic merge.

`repo abort TX` / `rdrive_repo_abort` works only before the transaction is prepared for publication. A prepared transaction might already be visible remotely, so inspect and replay its commit instead of assuming abort can undo it.

## Guarantee and storage boundaries

Atomic visibility applies to cooperating repository clients and servers correctly implementing same-filesystem atomic no-overwrite rename. It is a client-crash recovery guarantee, not evidence of server power-loss durability. Ordinary `put`, `sync`, SMB replacement, and metadata sidecars do not gain multi-file atomicity from this feature.

Do not manipulate `.rdrive-repository/`, immutable objects, commit slots, or transaction spool files as ordinary cache. There is no garbage collection, historical compaction, automatic import, or automatic conflict merge. Unreachable objects and failed staging can occupy space. Selected file tags and safe user attributes are supported; directory tags, resource forks, ACLs, and security attributes are outside this snapshot metadata scope.

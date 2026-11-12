# Retained edit recovery

Inspect `smart status` and the domain's `smart list` before changing recovery state. Keep the same state directory and domain binding; reusing a domain identifier for a different connection/root is not a recovery technique.

```sh
rdrive --json smart pause
rdrive --json smart list cloud-smart
rdrive --json smart export cloud-smart MUTATION ./recovered.txt
rdrive --json smart retry cloud-smart MUTATION
rdrive --json smart resume
rdrive --json smart run
```

Replace `cloud-smart` and `MUTATION` with inspected identifiers. MCP tools: `rdrive_smart_pause`, `rdrive_smart_export`, `rdrive_smart_retry`, `rdrive_smart_resume`, `rdrive_smart_run`.

- Pause stops later background publication; it does not undo already confirmed server operations.
- Export writes a separate real local file and does not change the remote file. It requires an allowed local root in MCP. Refuse local overwrite unless requested.
- Retry makes retained work eligible again while preserving its original version guards. It does not force an overwrite of an externally changed file.
- Resume re-enables processing. `smart run` performs one replay pass across configured Smart domains and may publish retained uploads or deletions; inspect affected queued work and stay within the requested recovery scope. It does not leave a new hidden background service running. The bridge may already be processing work.
- After a conflict, export local content and inspect the current remote revision. Merge or save under a new requested destination; preserve both versions until the user-directed resolution is complete.

Smart cancellation retains its independent local snapshot. CLI `smart discard DOMAIN MUTATION` explicitly removes a recovery snapshot only when the mutation is completed, cancelled, or superseded. Do not discard a recovery copy merely because an error was acknowledged. The MCP subset does not expose discard.

## Space pressure

Start with the clean-cache usage, pinned usage, and retained-edit totals. Unpin or allow automatic removal of content the user no longer needs offline, then request clean-cache eviction. File Provider may reject eviction of active or modified content. Preserve unsent modifications even if total application disk usage stays over the clean-cache number.

`cache prune` evicts eligible CLI cache entries; native File Provider eviction is requested through its system integration. Never truncate or delete Finder files to imitate placeholders or cloud-only state. If native eviction is needed, use the registered domain's menu or documented companion support, and report actual returned status instead of assuming the files disappeared.

## Missing Finder items or status

Check the existing domain identity, native registration, bridge, and notification helper separately. A live native mount does not prove a Smart domain is connected. Initial directory listings do not recursively scan the entire account. Failed or unvisited listings must not be treated as authoritative empty subtrees. Diagnose without deleting domain state, remote files, or recovery snapshots.

## File history and local checkpoints

`rdrive_history` is the completion-event log; it does not contain recoverable file bytes. Inspect `rdrive_history_status` and `rdrive_history_files` for account-bound retained contents. Automatic history is opt-in and uses a budget independent of clean-cache eviction. Pending Smart snapshots and prepared before-images remain protected during history cleanup.

Prefer exporting (`rdrive_history_export`) or restoring a separate copy. `rdrive_history_restore_prepare` saves a plan for a copy or explicit replacement; review the returned destination and message, then use `rdrive_history_restore_apply` with the same profile, version and plan and `apply:true`. A plan fixes its mode and destination. Changed remote content/metadata or overlapping pending edits stop replacement; never reprepare repeatedly to chase another writer. The current replacement version is retained first. An uncertain publication requires inspecting its destination and retained decision, not an unconditional retry.

Finder's **View File History…** exposes the same entries, policy, preview, export and two-stage restore. History starts when enabled; online-only remote bytes and edits made outside RDrive are not reconstructed retroactively. Legacy remote `versions` and managed-repository snapshots are separate formats.

Optional `rdrive_checkpoint_status/list/inspect` read APFS capability and owned checkpoint records. Creation and read-only recovery can require authorization; the background broker has no terminal and cannot read a sudo password. Checkpoint export permits only recorded local payloads to a new destination. Import verified recovered payloads using `rdrive_history_import`, then preview a new guarded restore. Never replace the live database or old queues with snapshot copies. macOS can expire system snapshots; a record is not proof its snapshot exists. Forgetting a record does not delete an OS snapshot.

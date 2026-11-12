# State directory and migration

RDrive keeps connection profiles, directory indexes, clean cache metadata,
transfer checkpoints, retained edits, and recovery receipts in one private state
directory. The CLI and Finder can share it through
`~/Library/Application Support/rdrive/shared-home.json`. The locator contains a
path; passwords remain in the connection's environment or existing Keychain item.

Inspect the active configuration and service before relocating it:

```sh
rdrive config show
rdrive service status
```

Preview a migration to the standard state location:

```sh
rdrive config migrate --to "$HOME/Library/Application Support/rdrive/state"
```

Apply the same operation:

```sh
rdrive config migrate --to "$HOME/Library/Application Support/rdrive/state" --apply
```

The command coordinates the state-directory locks and the existing managed
service. It pauses new synchronization, waits for active work, copies and verifies
the retained state, updates the shared locator, and configures the service for
the new directory. It preserves the service's application and IPC paths and the
previous synchronization pause setting. An unmanaged active writer prevents the
operation instead of being terminated.

Existing File Provider registrations and CloudStorage folders keep their domain
identifiers. Their placeholders and downloaded system copies remain under
macOS management. Directory indexes, queue identities, original revision guards,
and retained file contents are carried forward. A custom cache outside the state
directory remains at its configured location. Migration updates its ownership
reference when needed; it does not turn cached files into ordinary Finder files.

The destination must be a new private directory outside the source. RDrive refuses
an occupied destination, symbolic links, unsafe ownership or permissions, and
overlapping locations. It retains a recovery copy of the source and prevents the
old location from becoming a second writable state directory. The result reports
the backup path and verification totals.

An interrupted migration records a durable journal next to the shared locator.
Recover it through the program:

```sh
rdrive config recover
```

Recovery does not depend on resolving a possibly interrupted shared locator. It
uses the recorded phase to preserve the source or finish the committed switch.
Keep the journal and both locations until recovery completes; no manual JSON
editing is needed.

Verify the selected state and service after migration:

```sh
rdrive config show
rdrive profile list
rdrive service status
rdrive ls nas
```

An explicit `--home` or `RDRIVE_HOME` continues to take precedence over the shared
locator. A command aimed at the old location reports that it has moved. Use the
shared default or update the explicit selection to the reported destination.

See [cache management](CACHE_MANAGEMENT.md) to move only clean cached payloads,
and [Smart operations](SMART_OPERATIONS.md#resident-service) for service controls.

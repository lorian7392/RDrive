# Status overview

`rdrive status` shows the shared connection state, resident service, directory indexes, clean cache, file history, APFS checkpoints, retained work and conflicts in one report.

```sh
rdrive status
```

The default report reads local observations and existing state. It remains available while the background provider owns the state writer lock. It does not contact remote servers, look up a password, recover interrupted work, initialize missing databases, migrate indexes, mount an APFS image, or create a checkpoint.

## Common commands

```sh
# Overview of every configured connection.
rdrive status

# Scope account-owned data to one connection.
rdrive status --profile pan123

# Show a bounded set of issue details and retained request identifiers.
rdrive status --details --max-items 20

# Authenticate and check each selected connection's root metadata.
rdrive status --check-connections --timeout 10

# Read current capacity and record information from the existing checkpoint backend.
rdrive status --check-checkpoints

# One structured report, including explicit live checks.
rdrive status --json --check-connections --check-checkpoints --details
```

Use the profile names shown by `rdrive profile list`. Examples such as `nas` and `pan123` refer to existing profiles; `status` never creates a new connection or selects a different state directory to make an unknown name work. The normal `--home`, `RDRIVE_HOME` and shared-home selection rules apply.

## Options

| Option | Behavior |
| --- | --- |
| `--profile NAME` | Select one configured profile. Account-owned connection observations, index data, history, upload counts and Smart issues follow this scope. Shared service and checkpoint information retain their explicit global scope. |
| `--check-connections` | Perform an authenticated, read-only root metadata check for every selected profile and save the resulting local connection observation. It does not enumerate a remote tree, download file contents, or change remote files. |
| `--timeout SECONDS` | Limit each explicit connection check, including credential lookup and remote I/O. The accepted range is 1–120 seconds; the default is 10. Checks use bounded parallelism. The inherited `--jobs` option selects concurrency, capped at four. |
| `--check-checkpoints` | Inspect the configured checkpoint backend now. An existing APFS file vault may be mounted read-only to obtain current internal usage and record counts. This option does not create or delete a checkpoint. |
| `--details` | Include bounded issue examples and additional component information in the human report. JSON always includes bounded examples. Retained request identifiers help locate a specific item with the normal recovery commands. |
| `--max-items N` | Limit the displayed examples. The accepted range is 1–100; the default is 10. Totals remain the full observed totals. |
| `--json` | Print one JSON report for scripts. It includes section status, source, observation time, data and any inspection error. |

`--check-connections` uses the profile's configured environment or Keychain reference. A Keychain access request may occur during an explicit check if the item has not been authorized. Passive status never starts credential lookup. A timed-out check terminates and reaps its own helper process; it does not restart the provider or change synchronization settings.

## Reading connection state

Connection state comes from actual authenticated observations, including explicit checks and successful remote reads performed by the CLI or background provider. The report shows when the observation occurred, the last successful observation and the most recent failure reason.

- **Online**: a recent authenticated observation succeeded.
- **Offline**: the latest applicable connection observation failed; its safe error category explains the failure.
- **Unknown**: no applicable observation exists for the current account or the check could not establish reachability.
- **Stale**: the stored observation is older than the 300-second freshness window.

Read the observation timestamp alongside the state. A successful observation applies to its recorded time. A running service, saved profile or cached listing does not establish that the server is currently reachable. Changing the account identity, credential reference or other connection settings invalidates evidence recorded for the earlier configuration.

An operation error has context. For example, a missing remote file is not evidence that the entire connection has gone offline. The probe deadline also covers credential lookup and process startup, so a deadline by itself leaves reachability unknown. Credential and server error text is reduced to safe categories so a server cannot echo a password or authorization header into status output.

## Components

The configuration section identifies the selected state home, profile settings and effective policy references. Credentials are represented by their configured source; password values and authorization tokens are excluded.

The service section reports the observed supervisor and worker state. Service observations are separate from remote connection observations. A stopped or uninstalled resident service can coexist with valid retained local data.

The index section reports initialized databases, file and directory counts, pending scans, revision anchors and the latest observed successful scan time. Old or unsupported index schemas are reported for attention. Reading status does not migrate them or turn an unindexed directory into an empty remote folder.

The cache section separates indexed clean content, pinned content, active reservations and native File Provider accounting. Read its accounting label and source time alongside the byte counts; the fast overview does not hash file contents or treat an indexed entry as proof that its payload is still present. Native usage or reservations that cannot be attributed to an individual profile retain a global label. The cache budget excludes file history, queued source snapshots, unuploaded edits, checkpoint images and filesystem overhead; it is not a quota for the entire state directory.

The history section shows each selected profile's effective retention policy, logical retained bytes, entry count, prepared entries and latest recorded version time. History uses a separate budget from clean-cache eviction. Protected prepared or inflight content may prevent retention from meeting its target.

The checkpoint section shows the selected backend, configured capacity and retention policy, image identity information, retained creation plan, last completed checkpoint, next due time and recorded error. The default view uses local image and scheduler metadata. Internal filesystem usage that has not been freshly inspected remains identified as unknown or previously observed. `--check-checkpoints` requests a fresh backend inspection. The fixed image capacity is separate from ordinary history, cache, tiny outside scheduling records and independent system backups. See [file history and checkpoints](HISTORY.md).

The conflict and queue sections distinguish active Smart mutations, interrupted or failed uploads, retained recovery payloads and ordinary folder synchronization observations. They preserve the stored running state and original version conditions. Reading the overview does not retry, cancel, discard, rebase or resolve anything.

Ordinary folder synchronization reports the **last observed plan** for each recorded local/remote pair, with its time, phase and bounded conflict reasons. It does not silently rerun a synchronization plan. After repairing an ordinary conflict, run the normal conflict inspection or synchronization command to produce an updated observation. Old plans remain historical observations rather than a claim that a conflict is still active.

## JSON and partial reports

```sh
rdrive status --json
rdrive status --json --profile nas --details --max-items 5
```

The report includes `schema_version`, `observed_at` and `state_directory`. Component sections use an envelope containing `status`, `scope`, `source`, `observed_at`, `data` and an error when inspection cannot complete. The previous top-level `profiles`, `queued`, `cache_files`, `cache_bytes` and `pinned` metrics remain available when their source ledger can be read; profile-scoped reports apply the selected profile to account-owned metrics.

Components are sampled independently. The overview can report a busy, changing or malformed component while continuing to show unrelated sections. It does not replace missing or invalid evidence with a healthy zero. Existing SQLite metadata is read from verified temporary copies, including committed WAL data (bounded to 2 GiB per database or WAL file), without creating live sidecars or acquiring the application's writer lock. A concurrent change can make that component temporarily unavailable; a later status query can inspect a new snapshot.

Use the specific section and its observation time when scripting a health decision. A complete JSON document can contain a connection failure or a component requiring attention.

## Inspecting and resolving an issue

The overview supplies counts, reasons and bounded examples. The existing commands expose the complete selected records and their explicit recovery actions:

```sh
rdrive smart list DOMAIN_ID
rdrive queue list
rdrive index status DOMAIN_ID
rdrive history status PROFILE
rdrive checkpoint status
rdrive checkpoint list
rdrive conflict list PROFILE /absolute/local/root REMOTE_ROOT
```

Use [conflict repair](CONFLICTS.md) for the supported repair choices. Use [file history](HISTORY.md) to inspect or recover prior contents. Status does not weaken current revision checks or choose a conflict strategy automatically.

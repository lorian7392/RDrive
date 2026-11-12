# Complete configuration reference

This reference follows the actual serialized fields and defaults in
`src/config.rs`, `src/filtering.rs`, `src/policies.rs`, `src/history.rs`, and `src/checkpoints.rs`. See the [complete CLI
reference](CLI.md) for command arguments, [Quickstart](../QUICKSTART.md) for
workflows, and [Features](../FEATURES.md) for recovery and protocol boundaries.

## State directory and configuration persistence

Path precedence is the global `--home PATH` option, then `RDRIVE_HOME`, then an
existing shared-home locator, then `$HOME/Library/Application Support/rdrive`.
The optional owner-only file `~/Library/Application Support/rdrive/shared-home.json`
contains `{"version":1,"home":"/absolute/private/existing/state"}`. It references
the existing state directory; it does not duplicate profiles, credentials, queues,
or caches. The referenced state must exist and remain private. Explicit overrides
do not consult this locator. Remove the locator to return to the ordinary default.
The locator's parent and referenced state directory must be real directories,
owned by the current user, with no group or other permissions (normally `0700`).
The locator and the referenced `config.json` must be regular files owned by the
same user, with no group or other permissions (normally `0600`), and no symbolic
or hard links. The locator accepts only `version` and `home`, has a 4096-byte
limit, and requires an absolute existing target with an existing configuration.
A missing locator preserves ordinary default behavior without creating state.
An invalid locator or unavailable target reports an error instead of silently
selecting a different connection set. It never migrates or initializes the
referenced state. Do not replace the state directory with a symbolic link.
`--home` does not change the shell's
`HOME`. Configuration is stored as UTF-8 JSON without comments in `config.json`
inside that directory. A missing file uses all defaults. An existing JSON file
must contain a `profiles` object; the other top-level objects may be omitted
and then receive their defaults.

Use `rdrive config show` to inspect the active state and service. To relocate
the complete state, use `rdrive config migrate --to PATH`, review the preview,
then repeat with `--apply`. The program preserves retained contents, updates
the shared locator and managed service, and keeps a recovery copy. Use
`rdrive config recover` after an interrupted migration. See
[state directory migration](../STATE_DIRECTORY.md); moving `config.json` alone
does not relocate a connection safely.

Prefer the `profile`, `cache`, `index`, `filter`, `policy`, `history config`, and `checkpoint config` commands for saving
configuration. Before editing manually, stop RDrive processes using the same
state directory, preserve the original file, and keep the directory private,
owned by the current user, with mode `0700`. A lock prevents ordinary concurrent
writers; configuration is committed through a temporary file and rename. Do not
place the state directory inside an ordinary synchronization root, the content
cache, or `~/Library/CloudStorage`. Configuration is only part of recovery state:
copying it alone cannot restore queues, offline edits, or synchronization baselines.

## Top-level Config fields

- `profiles`: an object mapping connection names to `Profile`; an empty
  configuration uses `{}`. Required in an existing configuration file.
- `profile_admin_receipts`: internal replay receipts for profile changes,
  defaulting to `{}` and omitted when empty. RDrive maintains this field; do not
  edit or delete it to retry a command. Each receipt records an argument
  fingerprint, the original result, and its expiration time, without credential
  values. See [profile administration](#profile-administration-and-safe-retries).
- `cache`: `CachePolicy`, defaulting to `{"max_bytes":null,"directory":null}`.
- `index`: `IndexPolicy`; full metadata prefetch is enabled by default, with
  refresh intervals of `300` / `30` seconds.
- `filters`: global filters and connection overrides; common temporary files
  are excluded by default, with no custom include/exclude patterns.
- `policies`: automatic versioning and conflict handling; automatic versioning
  is off and conflicts stop synchronization by default.
- `history`: `HistoryConfig`, containing a default local history policy and
  connection overrides. Capture is disabled by default; limits default to 20
  versions, 30 days, and 5 GiB when enabled.
- `checkpoints`: optional APFS checkpoint `Policy`, disabled by default with a
  maximum of 64 local records. This is separate from resumable transfer checkpoints.

The following sections describe these objects. Transfer rate limits, job
concurrency, and MCP local roots are not `config.json` fields.

## profiles: connection Profile

- `protocol`: required lowercase string, either `"webdav"` or `"smb"`.
- `url`: required string defining the connection's remote root. WebDAV uses
  `https://host/path/`; `http://` requires `allow_http: true`. SMB uses
  `smb://server/share[/folder]`, with a nonempty share path. Both require a host
  and reject URL usernames, passwords, query parameters, and fragments.
- `username`: a string or `null`, defaulting to `null`. WebDAV without a username
  can use anonymous access. Actual SMB authentication is negotiated by the
  server and Rust client; there is no Kerberos/SSO switch.
- `password_env`: a string or `null`, defaulting to `null`. The string is an
  **environment-variable name**, not a password. `profile add --password-env`
  accepts a nonempty name containing only ASCII letters, digits, and `_`.
  Once configured, a missing variable is an error; it does not fall back to Keychain.
- `keychain_internet`: a boolean, defaulting to `false` and omitted when false.
  On macOS, `true` selects an existing Internet password in Keychain using the
  connection URL's host and the configured username. It is mutually exclusive
  with `password_env`. RDrive reads that same item without copying its secret
  into another Keychain item, an environment file, or the connection configuration.
  A missing username selects the empty account. A missing item or denied access
  is an error, even for anonymous connections; it never falls back to a generic
  item or an empty password. Other operating systems reject this credential source.
- `allow_http`: a boolean, defaulting to `false`. It explicitly permits WebDAV
  HTTP; it does not disable HTTPS certificate validation.
- `timeout_secs`: positive integer seconds, defaulting to `120`. WebDAV's total
  request timeout uses this value; its connection timeout is
  `min(timeout_secs, 30)`. SMB also uses the connection profile's timeout. Native
  macOS mounts use system-controlled timeouts.
- `smb_encryption`: a boolean, defaulting to `true`; the Rust SMB client requires
  encryption. `profile add --allow-unencrypted-smb` saves it as `false`.
- `nextcloud_upload_url`: a string or `null`, defaulting to `null`. It explicitly
  enables Nextcloud resumable chunk uploads, for example
  `https://cloud.example.com/remote.php/dav/uploads/account/`. It requires WebDAV
  and the same origin as `url` (scheme, host, and effective port). Embedded
  credentials, queries, and fragments are rejected. Setting it does not make
  every upload resumable; `put --resume` is still required.

The corresponding CLI names are `--protocol`, `--url`, `--username`,
`--password-env`, `--keychain-internet`, `--allow-http`, `--timeout`, `--allow-unencrypted-smb`, and
`--nextcloud-upload-url`. A connection name cannot be empty or start with `-`;
only ASCII letters, digits, `-`, and `_` are accepted. Existing names cannot be
overwritten directly. Before removing a connection, finish or cancel unfinished
uploads and handle its cache. Changing an account or server does not make old
cache, domains, or upload receipts belong to the new account: internal bindings
verify their identity.

## Profile administration and safe retries

`profile add` and `profile remove` can use the same state directory as a running
provider bridge. When that bridge is active, the CLI submits the change through
its private Unix-socket administration broker. The broker verifies the same
user, canonical state directory, and running instance before applying it. When
no broker is active, the CLI obtains the state writer lock and applies the
same operation directly. Both paths update the original `config.json`; they do
not create another profile store or copy a Keychain secret.

Administration runs between durable background synchronization phases. A
read-only background scan may yield to a pending profile change; uploads and
other remote mutations finish through their existing receipt and recovery
paths. `--wait-timeout SECONDS` defaults to `300` and accepts `1..3600`. It bounds
the CLI's wait, not the duration of a remote upload. A timeout or lost reply can
leave the outcome unconfirmed; it does not establish that the change failed.

Every change has a request ID, generated automatically unless `--request-id`
is supplied. Successful results include `request_id`; failures also identify
the ID needed for a retry. Repeat the complete original operation with
`--request-id '<unix_seconds>:<UUID>'`, substituting that exact reported ID and
keeping the profile name and all add options unchanged. Within 24 hours of its
timestamp, the same ID and arguments return the original committed result,
including after a restart. Reusing an ID with different arguments is rejected.
An ID must use a canonical Unix-seconds timestamp and lowercase UUID, must not
be more than 30 seconds in the future, and must not be expired. Invalid or
expired IDs are rejected; inspect `profile list` before issuing a new request
when the old outcome is uncertain.

The profile change and its receipt are saved in one atomic configuration
write. Up to 1024 unexpired receipts are retained. Once that limit is reached,
new requests are rejected until receipts expire; retrying an existing request
still returns its result. Expired receipts are pruned when a new change is
saved. Do not clear receipts to bypass the limit or retry an expired ID.

Removal refuses profiles with unfinished upload jobs, any cached files,
configured Finder domains, retained file history, retained download transfers, or open/prepared
repository transactions. Finish or cancel uploads, unpin and prune cache,
unregister and remove domains, and recover or explicitly discard retained work
through the appropriate commands first. Invalid or unrecognized retained
Smart state also blocks removal so that recovery data is preserved. Adding a
profile only saves connection configuration; it does not automatically add or
register a Finder domain.

## Authentication and environment variables

Password lookup for Rust file operations follows this order:

Setting both `password_env` and `keychain_internet` is rejected before any
credential lookup.

1. If `password_env` is set, read that variable and fail if it is missing.
2. If `keychain_internet` is true, look up the existing macOS Internet password
   for the URL host and configured username, and fail if it cannot be read.
3. Otherwise, on macOS use `/usr/bin/security` to look up a generic password
   whose service is `rdrive:<connection-name>` and account is `username`
   (an empty account when no username is configured).
4. If no entry exists and no username was supplied, use an empty password.
   With a username, report missing credentials.

There is no password command-line argument, and passwords are not written to
connection configuration. `profile list` displays usernames and variable names.
A background LaunchAgent does not inherit passwords temporarily exported in the
current terminal; use Keychain for persistent background connections.
The same applies to an already running provider bridge: profile administration
sends only the environment-variable name or Keychain reference, not the secret
value or the terminal's environment. Adding a profile does not make a newly
exported variable available to that bridge.
`service status` reports whether a variable exists in the checking process,
never its value.

Native SMB `mount` uses macOS credentials or a system terminal prompt; it does
not automatically use `password_env`. Native WebDAV can pass credentials to the
system helper through an anonymous temporary file descriptor. See [Mounting](../MOUNT.md).

Runtime environment variables include:

- `RDRIVE_HOME`: the default state path, overridden by `--home`.
- `HOME`: the base for the default state directory when `RDRIVE_HOME` is absent,
  and for user LaunchAgents.
- The variable named by a profile's `password_env`: read only when that
  connection needs credentials.
- `EDITOR`: a single executable used by `edit`, defaulting to `/usr/bin/vi`.
  Additional arguments and shell commands are not parsed.
- `RDRIVE_WEBDAV_LOCK_TOKEN`: an acquired WebDAV lock token, used for mutations
  and `unlock`. It is not passed as a CLI argument or renewed automatically.
- `HTTP_PROXY` / `http_proxy`, `HTTPS_PROXY` / `https_proxy`, `ALL_PROXY` /
  `all_proxy`, and `NO_PROXY` / `no_proxy`: proxy settings used by the WebDAV HTTP
  dependency stack, as described below.

`RDRIVE_TEST_SMB_URL`, `RDRIVE_TEST_DOCKER`, and `RDRIVE_TEST_SMB_CONTAINER` in test
code are isolated-fixture controls; ordinary RDrive operations do not read them.
Build tools and system helpers may also use their standard toolchain environment;
those variables are not additional RDrive configuration fields.

## Proxies, TLS, and protocol capabilities

The current reqwest dependency enables `system-proxy`. Non-loopback WebDAV
hosts use the underlying library's environment and system-proxy discovery.
Case-insensitive `localhost` and literal IPv4/IPv6 loopback addresses explicitly
connect directly. Private addresses do not bypass a proxy simply because they
are on a local network. Other hostnames that resolve to loopback addresses are
outside this explicit check.

The dependency checks uppercase environment variables before lowercase ones;
HTTP / HTTPS settings take precedence over `ALL_PROXY`. `NO_PROXY` is a
comma-separated bypass list accepting hosts, domains, and IP/CIDR entries; `*`
bypasses all proxies. The underlying library applies additional HTTP-proxy
protection in CGI environments where `REQUEST_METHOD` exists. This is not a
separate RDrive proxy engine. Current macOS proxy discovery does not fully
support the system exceptions list or PAC. Use `NO_PROXY` for explicit direct
connections. SMB and native macOS volumes do not use the Rust WebDAV HTTP-proxy path.

HTTPS certificate validation is enabled by default. There are no configuration
options for `insecure`, custom CAs, client certificates, OAuth, Digest, or custom
request headers. Redirects are rejected except for the implementation's limited
same-resource WebDAV trailing-`/` handling. Generic WebDAV has no universal
resumable segmented PUT; `nextcloud_upload_url` explicitly selects Nextcloud's protocol.

## cache: CachePolicy

- `max_bytes`: unsigned integer bytes or `null`, defaulting to `null` (no clean
  content-budget limit). `0` is a valid zero budget, not unlimited. Change it
  with `cache config --max-bytes N` or `--unlimited`. When its current value is
  `null`, `smart setup` sets it to `5368709120` (5 GiB), including when unlimited
  was explicitly chosen earlier; set unlimited again after creating the domain
  if that is required.
- `directory`: a path string or `null`, defaulting to `null` for `cache/` under
  the state directory. A custom value must be saved by safely migrating with
  `cache directory PATH`, using an absolute real private directory. Do not edit
  this field to bypass cache-root binding, active-reader checks, or the migration journal.

The budget combines Rust clean cache, client-reported File Provider clean copies,
and approved download reservations. Pinned content, active readers, and content
awaiting upload are protected. Durable upload snapshots, Smart local edits,
partial downloads, indexes, and recovery backups are not evictable content.
Reducing the budget below protected usage fails without changing the prior
configuration. Migration includes pinned Rust cache, but does not move Finder's
system download directory, queues, or indexes. See [Cache management](../CACHE_MANAGEMENT.md)
for directory requirements and interrupted-migration recovery.

## index: IndexPolicy

- `enabled`: a boolean, defaulting to `true`; enables general background metadata
  prefetch. `false` still allows root, browse-demand, and pinned-directory refreshes.
- `prefetch_depth`: a nonnegative `u32` or `null`, defaulting to `null` for gradual
  traversal of the full tree. Root depth is `0`; scanning a directory also finds
  its immediate children. CLI `--depth N` sets it; `--all` restores `null`.
- `refresh_secs`: integer seconds, defaulting to `300`, within `1..=31536000`;
  the general directory-refresh interval.
- `active_refresh_secs`: integer seconds, defaulting to `30`, with the same
  range; the interval for browsed or offline-pinned directories.

These values schedule background metadata work. They neither download files nor
grant offline pinning, and are not hard real-time timers. Keep `provider serve`
or its managing background service running to execute the work; `index refresh`
only saves a one-time demand. Manual recursive refresh can exceed the general
prefetch depth. Managed repositories refresh a complete root snapshot and reject
nonempty subpath refreshes. Failed directories back off; incomplete listings are
not treated as deletions. See [Directory indexing](../DIRECTORY_INDEXING.md).

## filters: FilterConfig and ProfileFilter

Global fields:

- `temporary_files`: a boolean, defaulting to `true`; excludes `.DS_Store`, `._*`,
  `*.tmp`, `*.swp`, `*.swo`, `*.part`, `~$*`, and `.~lock.*`.
- `include`: an array of strings, defaulting to `[]` (no include restriction on files).
- `exclude`: an array of strings, defaulting to `[]` (no additional exclusions).
- `profiles`: a map of connection names to override objects, defaulting to `{}`.

Each connection override has three optional fields: `temporary_files` is a
boolean or `null`; `include` and `exclude` are string arrays or `null`. Omission
or `null` inherits the corresponding global field. Explicit `[]` overrides a
global list with an empty one; lists are not concatenated. `filter reset PROFILE`
removes the entire override.

Globs are case-sensitive. A pattern without `/` matches basenames at any depth;
a pattern with `/` is relative to the domain root. `*` does not cross `/`; `**`
can cross directory levels. Trailing `/` matches only directories; excluding a
directory also excludes its descendants. Exclusion takes precedence over
inclusion, while directory traversal can continue to find allowed files. Each
list permits at most `256` patterns. Patterns cannot be empty, exceed `4096`
bytes, start with `/`, contain backslashes or control characters, or contain
`.` / `..` path components.

Rules apply to Smart listings, background discovery, searches, and new mutation
targets. They do not delete excluded remote files, interpret exclusion as a
synchronized deletion, or remove recovery records and snapshots. Already queued
operations can pause because of changed rules. Ordinary explicit `get` / `put`,
native volumes, and ordinary `sync` are unaffected.

## policies: PolicyConfig, VersionPolicy, and ProfilePolicy

Global fields:

- `versions`: `VersionPolicy`, defaulting to
  `{"enabled":false,"keep":5,"include":["*"],"exclude":[]}`.
- `conflicts`: `"stop"`, `"keep-both"`, or `"replace"`, defaulting to `"stop"`.
- `profiles`: a map of connection names to `ProfilePolicy`, defaulting to `{}`.

All `VersionPolicy` fields:

- `enabled`: a boolean, defaulting to `false`; enables automatic remote version
  saving before Smart overwrites file contents.
- `keep`: an integer, defaulting to `5`, within `1..=100000`.
- `include`: a string array, defaulting to `["*"]`; unmatched files are not saved automatically.
- `exclude`: a string array, defaulting to `[]`; a match excludes the file and
  takes precedence over include patterns.

The two version-pattern lists permit at most `512` entries combined. Each must
be nonempty, contain no control characters, and be at most `4096` bytes, using
the same relative-glob syntax as filters. Automatic pruning happens only after
new content is confirmed published. Backups belonging to other unconfirmed
writers are protected, so the actual retained count can temporarily exceed
`keep`. Renaming a file does not migrate its old path's version history automatically.

Each `ProfilePolicy` contains `versions` (a complete `VersionPolicy` or `null`)
and `conflicts` (a strategy string or `null`). Omission / `null` inherits the
global value. A `versions` object replaces the global version policy as a whole;
omitted fields inside it use `VersionPolicy`'s own defaults, not per-field global
inheritance. Override connection names must be `1..=120` bytes without control
characters. `policy reset PROFILE` restores global inheritance.

`stop` retains conflicts for a decision. `keep-both` uses a stable conflict
filename and create-only conditions to retain both copies. `replace` archives
the observed remote revision before a revision-conditional replacement; another
remote change after the decision still stops it. `smart resolve --strategy remote`
is an individual conflict decision that preserves the local recovery snapshot;
it is **not** an allowed persistent automatic policy. Policies apply to Smart
content conflicts, not automatic overwriting in ordinary `sync`.

## history: HistoryConfig and HistoryPolicy

`history` controls local, account-bound file history. It does not change the
older server-side `policies.versions` format or the completion-event log.

- `default`: a `HistoryPolicy` used when a connection has no override.
- `profiles`: a map of connection names to complete `HistoryPolicy` overrides,
  defaulting to `{}`. An override replaces the default policy as a whole.

Every history policy has four fields:

- `enabled`: a boolean, defaulting to `false`. When true, supported destructive
  operations preserve verified before-images before publication. Turning it off
  stops future automatic capture without silently clearing existing content.
- `keep`: integer versions per item, defaulting to `20`, within `1..=100000`.
- `max_age_days`: integer days, defaulting to `30`, within `1..=36500`.
- `max_bytes`: a positive unsigned byte limit or `null`, defaulting to
  `5368709120` (5 GiB). `null` means unlimited; zero is rejected. This budget is
  independent of the clean-cache limit. Prepared or unresolved recovery entries
  stay protected, so actual disk use can temporarily exceed the retention target.

Use `history config --profile NAME` for a connection override, or omit
`--profile` for the default policy. `--enabled true|false`, `--keep`,
`--max-age-days`, and `--max-bytes` set the corresponding fields. `--unlimited`
sets `max_bytes` to `null` and conflicts with `--max-bytes`. Finder's history
settings update the same connection override through the authenticated broker.
There is no second Finder-only history configuration.

Automatic local capture covers supported Smart mutations, immediate and queued
`put` overwrites, `rm`, and guarded `cp`/`mv` overwrite destinations. Explicit
legacy version restores force preservation of the current target before
replacement. Ordinary `sync`/`watch` retain remote before-images before guarded
file overwrites and propagated remote file deletions. Local synchronization
recovery keeps its existing local `.rdrive-versions` behavior. Ordinary sync
does not propagate directory deletion. Raw native mounts and writes from other
clients are outside this automatic local history path. Metadata and remote
publication must still meet their normal revision conditions. See
[file history and recovery](../HISTORY.md).

## checkpoints: optional APFS Policy

- `enabled`: boolean, default `false`. Enables creation with the selected backend.
- `max_records`: integer within `1..=4096`, default `64`. Limits owned checkpoint records. Managed automatic cleanup can remove eligible old file checkpoints; it never deletes native system snapshots.
- `storage`: `system_snapshot` (default) or `apfs_vault`. The CLI uses `system-snapshot` and `apfs-vault`.
- `interval_secs`: `null` (manual only, the default), or `60..=31536000` seconds between completed scheduled vault checkpoints. The resident service performs scheduling.
- `max_bytes`: `null` for legacy system snapshots. The APFS vault requires a fixed finite capacity of at least 256 MiB, fitting a signed 64-bit integer and divisible by 512. The image reserves this capacity, including its internal filesystem overhead. An initialized image rejects incompatible capacity changes. State relocation currently refuses retained vault data because the image and its manifests are bound to their original private state location; the original data remains untouched.
- `auto_prune`: boolean, default `false`. Enables reclamation of the vault's own eligible old file checkpoints while protecting the latest verified recovery point during replacement.

Scheduling, byte capacity and automatic cleanup require `storage: apfs_vault`. Old configurations keep their existing manual system-snapshot behavior until explicitly changed through the program. Use `checkpoint config` to select storage and limits, `--manual` to clear the interval, and `checkpoint status` to inspect the actual store and schedule. Small external scheduler/ownership records, ordinary cache/history and independent macOS snapshots are outside the fixed image's capacity.

Recovery exports verified payloads to new paths and never restores old queue/database state over the live state. Legacy system-snapshot creation and export may require `--authorize` in a terminal. See [APFS checkpoints](../HISTORY.md#optional-apfs-checkpoints).

## Complete credential-free JSON example

This parseable `config.json` example includes every default field, WebDAV and
SMB profiles, and connection overrides. `example.invalid` is a placeholder;
there are no passwords, tokens, environment-variable values, or real accounts.
Configure your own connections through the CLI. `directory: null` requires no
manual migration.

```json
{
  "profiles": {
    "dav": {
      "protocol": "webdav",
      "url": "https://example.invalid/files/",
      "username": null,
      "password_env": null,
      "keychain_internet": false,
      "allow_http": false,
      "timeout_secs": 120,
      "smb_encryption": true,
      "nextcloud_upload_url": null
    },
    "nas": {
      "protocol": "smb",
      "url": "smb://example.invalid/share/",
      "username": null,
      "password_env": null,
      "keychain_internet": false,
      "allow_http": false,
      "timeout_secs": 120,
      "smb_encryption": true,
      "nextcloud_upload_url": null
    }
  },
  "profile_admin_receipts": {},
  "cache": {
    "max_bytes": null,
    "directory": null
  },
  "index": {
    "enabled": true,
    "prefetch_depth": null,
    "refresh_secs": 300,
    "active_refresh_secs": 30
  },
  "filters": {
    "temporary_files": true,
    "include": [],
    "exclude": [],
    "profiles": {
      "dav": {
        "temporary_files": null,
        "include": ["*.txt", "docs/**/*.md"],
        "exclude": ["private/"]
      }
    }
  },
  "policies": {
    "versions": {
      "enabled": false,
      "keep": 5,
      "include": ["*"],
      "exclude": []
    },
    "conflicts": "stop",
    "profiles": {
      "dav": {
        "versions": null,
        "conflicts": null
      }
    }
  },
  "history": {
    "default": {
      "enabled": false,
      "keep": 20,
      "max_age_days": 30,
      "max_bytes": 5368709120
    },
    "profiles": {
      "dav": {
        "enabled": true,
        "keep": 20,
        "max_age_days": 30,
        "max_bytes": 5368709120
      }
    }
  },
  "checkpoints": {
    "enabled": false,
    "max_records": 64,
    "storage": "system_snapshot",
    "interval_secs": null,
    "max_bytes": null,
    "auto_prune": false
  }
}
```

The smallest empty configuration is `{"profiles":{}}`; the other objects use
the defaults above. The empty `profile_admin_receipts` object is included here
for completeness; RDrive omits it when saving an empty receipt set.

## Domains, resident services, and session options

Dedicated commands maintain these records separately from `config.json`.

`provider-domains.json` stores an array of domains. Each Domain has `id`,
`profile`, `remote`, and `display_name`, plus `repository` / `smart` (default
`false`) and `local_root` (default `null`). `remote` is relative to the connection
root; `repository` must reference an initialized repository. `local_root` is an
absolute real local path for a compatible Finder mapping. `smart setup` saves
`smart: true`, defaults the display name to the domain ID, and uses a `null`
local root. Without a domain file, `provider domains` derives ordinary domains
from profiles; once explicit domain configuration is saved, that array is used.
Removing local domain configuration does not unregister a system File Provider,
reclaim system-downloaded copies, or discard recovery snapshots.

`service install` saves `ServiceOptions`: `home`, the current CLI `executable`,
`ipc_root` (default `service/ipc` under the state directory), optional `app`
(default none), `development_tcp` (default `false`), and optional
`launch_agents_dir` (default `~/Library/LaunchAgents`). The service runs under
macOS user launchd. IPC paths and the App must be matching real private paths;
the complete Unix-socket path must be shorter than the implementation's `104`
byte limit. Installation does not start the service or register domains;
uninstallation preserves user data. Do not edit generated installation records
or plists manually.

`--limit`, `--progress`, and `--jobs` apply only to the current CLI / MCP session
and are not saved to Config. `mcp serve` options `--allow-write`, `--local-root`,
and `--timeout` are also startup arguments. `smart run --staging` defaults to
`SmartTransfers/` under the state directory. Metadata selection is currently the
CLI's built-in safe scope; `MetadataPolicy` is not a user-configurable entry
point. Index databases, upload receipts, checkpoints, cache markers, and
migration journals are recovery state, not configuration for manual editing.

## Complete helper-script options

Python argparse supplies these scripts' `--help`; they do not add production
CLI options. See the [CLI reference](CLI.md#optional-native-companion-options)
for every native companion passthrough control.

`scripts/mcp-config.py` prints client configuration to stdout without changing
client settings or reading passwords:

- `--binary PATH`: an existing RDrive executable. For a source build, explicitly
  provide `target/release/rdrive`; the script defaults to the project's
  `dist/rdrive`, which is not expected in a source checkout.
- `--home PATH`: required state directory; it may not yet exist. The generated
  configuration uses its resolved absolute path.
- `--local-root PATH`: repeatable existing local directories. By default there
  are no authorized local-file roots.
- `--allow-write`: includes startup arguments enabling write tools; off by default.
- `--timeout SECONDS`: a positive integer, defaulting to `300`.
- `--format json|toml`: defaults to `json`; produces `mcpServers` or
  `[mcp_servers.rdrive]` configuration respectively.
- `-h, --help`: shows script help.

`scripts/install-skills.py` copies the three repository skills without replacing
existing directories with the same name:

- `--target PATH`: destination skills directory, defaulting to
  `$CODEX_HOME/skills`, or `~/.codex/skills` when `CODEX_HOME` is unset.
- `--skill NAME`: repeatable selection from `rdrive-files`, `rdrive-smart-sync`,
  and `rdrive-transactions`; all three by default.
- `--dry-run`: prints the installation plan without creating directories.
- `-h, --help`: shows script help. Installing skills does not configure or
  register the MCP server.

`macos/build.py` builds the optional native App without registering or enabling
system extensions automatically:

- `--identity IDENTITY`: codesign identity, defaulting to `-` for an ad-hoc
  development review signature.
- `--team-id TEAM`: developer team identifier; none by default.
- `--app-group GROUP`: explicit App Group. Without it, a team ID selects
  `TEAM.io.rdrive.shared`; otherwise the value is `group.io.rdrive.shared`.
  Actual distribution requires matching signing and entitlements.
- `--development-ipc-root PATH`: an explicit development-only private IPC root;
  none by default. It must be a dedicated absolute, current-user-owned `0700`
  directory, not `/`, the user's home, or `/private/tmp`. This enables a
  development build with a narrow directory exception rather than a production App Group.
- `--cli PATH`: an existing Rust CLI to embed; defaults to `target/debug/rdrive`.
- `--output PATH`: output directory, defaulting to `macos/build`; the final App
  is `RDrive.app` inside it.
- `-h, --help`: shows script help. Building can create output/development
  directories and invokes Xcode tools and codesign.

`scripts/generate-cli-docs.py` reads executable help to generate the repository's
CLI and MCP references:

- `--binary PATH`: defaults to `target/debug/rdrive`; build it first.
- `--output PATH`: defaults to `docs/reference/CLI.md`, or
  `docs/reference/MCP.md` with `--mcp`.
- `--mcp`: generates the complete MCP tool schema reference instead of CLI help.
- `--check`: compares generated content with the reference and exits nonzero
  on drift without writing the file.
- `-h, --help`: shows script help.

See [Testing](../TESTING.md) for test scripts, coverage, and their isolated
environment options.

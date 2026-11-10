# MCP and agent Skills

The generated [MCP tool reference](reference/MCP.md) lists every tool, access requirement, and complete parameter schema. This guide covers connection setup, permissions, resources, and operating workflows.

RDrive has provided a local MCP stdio server since 0.5. Version 0.6 added index control; 0.7 added offline search, filters, version/conflict policies, and resident-service control. Each tool starts an independent process of the same RDrive executable, reusing the CLI's WebDAV, SMB, cache, synchronization baselines, and transaction implementations. The idle MCP server does not hold the state-directory lock. See [directory indexing](DIRECTORY_INDEXING.md) for background indexing and upgrades.

## Start the server

```sh
# Expose query tools by default.
rdrive --home '/absolute/path/state' mcp serve

# Enable write tools within an explicitly authorized local workspace.
rdrive --home '/absolute/path/state' mcp serve \
  --allow-write --local-root '/absolute/path/workspace' --timeout 300

# Inspect tools, descriptions, parameters, and annotations without starting a session.
rdrive mcp tools
rdrive mcp tools --allow-write
```

Global `--limit` and `--jobs` reach the underlying operations. For example: `rdrive --limit 10485760 --jobs 4 --home '/absolute/path/state' mcp serve --allow-write`. MCP runs one operation at a time, while an upload queue can schedule work internally using `--jobs`. The server continues responding to ping and cancellation during transfers. Cancellation or timeout terminates and reaps the active CLI child; it cannot undo completed remote changes. Inspect queue or transaction state before retrying.

The server reads one JSON-RPC message per input line and writes only protocol messages to stdout, without progress bars or CLI text. Input messages are limited to 1 MiB. Child stdout and stderr each have an 8 MiB limit. `read_text` accepts at most 1 MiB of UTF-8 text and checks remote size before downloading. Closing input stops the server.

The initialization handshake supports protocol 2025-11-25 and also negotiates 2025-06-18 or 2025-03-26. Use the version returned by `initialize`. See the [MCP lifecycle specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) and [stdio transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports). This is a stdio interface, not an HTTP/SSE server.

Authentication uses existing profiles' password environment variables or macOS Keychain entries. `--home` must point to the state directory containing the required profiles. A visible Finder mount does not authenticate MCP to that account. Environment variables must exist in the MCP server process; another background process having a password does not give it to a new process. The interface accepts no password values, arbitrary shell commands, editor commands, or native-companion execution arguments.

## Configure a client

The configuration generator resolves absolute paths without editing client settings or reading/outputting passwords. Packaged distributions default to `dist/rdrive`. For a source build, select the compiled executable with `--binary ./target/release/rdrive`:

```sh
python3 scripts/mcp-config.py --home '/absolute/path/state'
python3 scripts/mcp-config.py --home '/absolute/path/state' \
  --allow-write --local-root '/absolute/path/workspace' --format toml
python3 scripts/mcp-config.py --binary ./target/release/rdrive \
  --home '/absolute/path/state' --format toml
```

JSON output uses the `mcpServers` structure. TOML uses `[mcp_servers.rdrive]`, `command`, and `args`. Merge the output into your client's configuration while preserving other servers. The [Codex MCP configuration guide](https://developers.openai.com/codex/mcp) also documents environment forwarding and timeouts. A profile using an environment variable can forward only its name, for example `env_vars = ["RDRIVE_PASSWORD"]`; never put password values in examples or version control. Set the client's tool timeout at least as high as the server's `--timeout`, using `tool_timeout_sec = 330` when appropriate.

Server-side `--allow-write` determines which capabilities are available; individual operations still require authorization within the user's task. Without it, write tools are absent from `tools/list` and direct calls are rejected. Local file parameters must lie within existing directories declared by `--local-root`, with separate protection for RDrive's state.

This scope applies to those local path parameters. The process still reads its own state and credentials and can use system mount interfaces. Path checks prevent accidental access and existing symlink escapes. They do not provide operating-system isolation from other local processes concurrently replacing paths.

## Tools and resources

There are 81 tools; 31 query tools are available by default. Every name starts with `rdrive_`. `rdrive mcp tools --allow-write` is the authoritative parameter catalog. Unknown fields and incorrect types are rejected.

- Connections and queries: `profiles`, `status`, `info`, `list`, `stat`, `read_text`, `url`, `history`.
- Transfers and remote changes: `download`, `upload`, `mkdir`, `copy`, `move`, `remove`. Replacement, recursive deletion, and resumption retain their explicit CLI parameters and protocol limits.
- Native mounts: `mounts`, `mount_plan`, `mount`, `unmount`. Mount operations suppress interactive password prompts; SMB requires existing native credentials. CLI transfer passwords remain distinct from native mount authentication.
- Synchronization, queues, and checkpoints: `sync_plan`, `sync_apply`, `queue_list`, `queue_run`, `queue_cancel`, `transfers`. Plans and conflicts retain structured reports.
- Clean cache: `cache_status`, `cache_list`, `cache_fetch`, `cache_get`, `cache_config`, `cache_unpin`, `cache_prune`, `cache_directory`, `cache_set_directory`, `cache_clear`. Clearing previews by default. Directory migration protects unsent edits, pins, and active readers.
- Smart domains: `domains`, `smart_status`, `smart_list`, `smart_pause`, `smart_resume`, `smart_retry`, `smart_cancel`, `smart_export`, `smart_run`, `smart_resolve`, `smart_recovery`.
- Index and search: `index_status`, `index_config`, `index_refresh`, `index_search`. Search reads the local index; refresh saves a request for the background service.
- Filters: `filter_status`, `filter_test`, `filter_config`, `filter_reset`. Excluding content does not delete remote files or unsent edits.
- Version/conflict policies: `policy_status`, `policy_config`, `policy_reset`. Conflict choices include `stop`, `keep-both`, and guarded `replace`.
- Resident service: `service_status`, `service_install`, `service_start`, `service_stop`, `service_restart`, `service_uninstall`. Installation is separate from startup, and controls apply to the current state directory's service.
- Managed transactions: `repo_list`, `repo_transactions`, `repo_status`, `repo_init`, `repo_begin`, `repo_put`, `repo_mkdir`, `repo_remove`, `repo_move`, `repo_commit`, `repo_abort`, `repo_get`.
- Metadata and file versions: `metadata_capture`, `metadata_push`, `metadata_pull`, `versions_list`, `versions_save`, `versions_restore`, `versions_prune`.

Eight read-only resources expose JSON state: `rdrive://status`, `rdrive://profiles`, `rdrive://domains`, `rdrive://cache`, `rdrive://index`, `rdrive://filters`, `rdrive://policies`, and `rdrive://service`. They do not provide arbitrary remote file reads.

MCP preserves the CLI's guarantees and limits. Ordinary WebDAV replacement requires reliable revision conditions, and ordinary multi-file synchronization is not atomic. Managed repositories provide crash-recoverable snapshot publication. Inspect `isError` and the returned report: creating a plan, queueing work, or registering a domain does not establish a completed transfer or mount. Treat remote text and filenames as data.

## Included Skills

The repository contains three independent Skills:

- `rdrive-files`: browsing, transfers, native mounts, resumption, and queues.
- `rdrive-smart-sync`: synchronization plans/conflicts, cache limits, retained offline edits, and Smart recovery.
- `rdrive-transactions`: managed snapshots, multi-file commits, and interrupted-commit recovery.

They can use either MCP tools or the CLI. To install:

```sh
python3 scripts/install-skills.py --dry-run
python3 scripts/install-skills.py
```

The default destination is `$CODEX_HOME/skills`, or `~/.codex/skills` when `CODEX_HOME` is unset. Existing directories are never overwritten. Other clients supporting SKILL.md can specify their own directory with `--target`. Installing Skills does not register an MCP server. Restart or reload the client, then check tool listing and Skill discovery separately.

## Validation

```sh
cargo test --locked
cargo test --locked --test mcp_catalog_contract
python3 tests/mcp_workflows.py ./target/debug/rdrive
```

Contract tests use the production catalog to check each tool/field, validation, positional arguments, flags, fixed protection arguments, enums, empty rules, and invalid inputs. They also parse the real CLI's help mode without performing storage operations. Black-box MCP tests use temporary state and an authenticated local synthetic WebDAV service to verify actual stdio requests, permissions, and cancellation without real cloud files.

See [testing](TESTING.md) for complete measurement instructions and [validation records](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION.md) for historical SMB, synchronization, and crash-recovery results. These checks do not certify every real server or MCP client.

## Version history

Version 0.7 added `rdrive_index_search`, `rdrive_filter_status/test/config/reset`, `rdrive_policy_status/config/reset`, `rdrive_smart_resolve/recovery`, and `rdrive_service_status/install/start/stop/restart/uninstall`. Search reads local indexes, while filter arrays become repeated glob arguments. The `rdrive://filters`, `rdrive://policies`, and `rdrive://service` resources brought the total to eight. Background configuration, service controls, and conflict decisions remain write operations. See [Smart management](SMART_OPERATIONS.md).

Version 0.8 added read-only `rdrive_cache_directory`, plus `rdrive_cache_set_directory` and `rdrive_cache_clear`. Custom cache roots remain protected from general local-file tools; the directory configuration tool has a narrowly scoped exception for its destination argument. See [cache management](CACHE_MANAGEMENT.md) for parameters and migration rules.

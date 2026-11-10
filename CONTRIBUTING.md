# Contributing to RDrive

Reproducible bug reports, documentation improvements, and focused patches are welcome. RDrive is [MIT-licensed](LICENSE). Ensure that you can contribute the material and retain third-party licenses and attribution.

## Report a bug

Search [existing issues](https://github.com/lorian7392/RDrive/issues) first. Include:

- `rdrive --version`, the source commit when available, macOS version, and CPU architecture.
- The access path: CLI transfer, native mount, Smart File Provider, or MCP.
- The protocol and a shareable server implementation/version.
- Redacted commands, expected and actual behavior, the full error, and a minimal reproduction.
- Relevant custom state/cache directories, proxy or service settings, and whether the problem reproduces with temporary synthetic storage.

Follow the [security policy](SECURITY.md) for credentials and unpublished vulnerabilities. Do not attach private state directories, real business files, or upload snapshots containing user data.

## Set up a development environment

Use Rust 1.90+, the macOS command-line developer tools, and Python 3.11+. Native tests also require the Apple SDK and Swift compiler. Follow the [installation guide](docs/INSTALLATION.md), then build and test:

~~~sh
cargo build --locked
cargo test --locked
~~~

The lockfile and `vendor/` patches are part of the reproducible build. Avoid unrelated dependency upgrades. Changes to vendored SMB code must also update its `RDRIVE-PATCH.md` notes, upstream attribution, and licenses.

Tests must use dedicated temporary state directories, synthetic credentials, and disposable storage. They must not access a developer's real cloud storage, NAS, mounts, Keychain, or proxy configuration by default. Do not allow an old process to write an upgraded state directory.

## Run relevant checks

After the Rust checks, select the workflows relevant to the change:

~~~sh
cargo build --locked
python3 tests/cli_workflows.py target/debug/rdrive
python3 tests/advanced_workflows.py target/debug/rdrive
python3 tests/cache_directory_workflows.py target/debug/rdrive
python3 tests/mcp_workflows.py target/debug/rdrive
python3 tests/index_workflows.py target/debug/rdrive
python3 tests/search_workflows.py target/debug/rdrive
python3 tests/policy_workflows.py target/debug/rdrive
python3 tests/service_workflows.py target/debug/rdrive
~~~

These workflows cover different behavior; every small patch does not require every long-running check. Loopback servers need an environment that permits local listening sockets. The [testing guide](docs/TESTING.md) lists the complete matrix, report options, and Smart, namespace, and receipt workflows.

SMB fixtures and native integration are separate layers:

~~~sh
python3 tests/smb_workflows.py target/debug/rdrive
python3 tests/mount_workflows.py target/debug/rdrive
python3 macos/tests/run.py
~~~

SMB fixtures need Docker. Mount, LaunchAgent, and actual File Provider workflows call system interfaces and require the isolated environments described in the testing guide. Native system tests also require a built, explicitly selected App. Successful compilation does not establish registration, callback behavior, downloads, or Finder visibility.

Report passed, failed, ignored, and skipped checks separately. A test count or a collection of passing workflows is not a code-coverage measurement. Claim coverage only after generating and reviewing an actual report for the stated scope.

## Keep changes focused

- Preserve no-overwrite defaults, preview-before-apply behavior, revision conditions, state identity, and path boundaries.
- Test failures, process interruption, lost replies, and concurrent changes when adding recovery logic.
- Assert real requests, bytes, and persistent state. Distinguish queued, sent, server-confirmed, and system-visible results.
- Follow the surrounding style. Avoid unrelated whole-repository formatting or dependency updates.
- Update the feature guide and command/configuration references when behavior changes.
- Keep historical `docs/*VALIDATION*.json` reports and versioned evidence intact. Add a new dated report with the measured version and scope instead of rewriting old results.

Check whitespace and the working tree before submitting:

~~~sh
git diff --check
git status --short
~~~

Build output, caches, real account configuration, `.runtime/`, `.env`, upload payloads, and temporary test data must stay out of a patch.

## Documentation

The public documentation is written in English and published to [GitHub Pages](https://lorian7392.github.io/RDrive/). Markdown remains the editable source. Keep examples generic, preserve links, and describe the implemented behavior and its limits.

CLI and MCP reference pages are generated from the built executable:

~~~sh
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --mcp
python3 tests/test_documentation.py target/debug/rdrive
~~~

Do not edit generated command help or schemas by hand.

## Submit a pull request

Describe the concrete trigger, the resulting behavior, and why the change is needed. List the checks you actually ran and any relevant checks left unrun. For state migration, overwrite, deletion, or recovery changes, explain compatibility and retained data.

Attach only relevant, redacted logs. Do not claim publication, deployment, or production validation without corresponding evidence. See [release and version-tag instructions](docs/RELEASING.md) for the release process.

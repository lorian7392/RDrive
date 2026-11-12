# Testing and coverage

RDrive separates Rust behavior tests, isolated protocol workflows, native SDK/mock tests, and real-system acceptance. Passing tests does not establish that every production line ran. Passing mocks does not establish that macOS accepted an extension or Finder displayed a menu.

[Validation archives](README.md#historical-validation-records) preserve historical results. The [coverage snapshot](COVERAGE.md) records the measured 100% result delivered with commit `f831526`; it must not be attributed to later source changes. New revisions require fresh collection and a passing gate.

## Requirements and isolation

- Rust 1.90+, the committed `Cargo.lock`, and the `vendor/` patches.
- Python 3.11+ for the complete development suite. Pinned `coverage==7.17.2` requires Python 3.11 or newer; ordinary Python tests use the standard library.
- Xcode command-line tools, Swift, and the FileProvider, FinderSync, and AppKit SDKs for native tests.
- Permission for temporary loopback HTTP/TCP/Unix sockets for DAV/IPC workflows. They do not access real storage services.
- Docker and the fixture image/packages for Samba workflows. Containers, accounts, and shares use synthetic temporary data and are removed afterward.

Tests use temporary state directories and synthetic passwords. They do not query real Keychain credentials or alter proxies or existing connections. `macos/tests/run.py` writes to `target/native-tests` by default, leaving `macos/build/RDrive.app` untouched. Native mock tests can construct offscreen controls but do not show windows, run the application event loop, register/remove real domains, or request real content eviction.

## Quick checks

Run from the repository root:

```sh
cargo build --locked
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --check
python3 scripts/generate-cli-docs.py --binary target/debug/rdrive --mcp --check
python3 macos/tests/run.py
```

Documentation tests use `target/debug/rdrive` by default; set `RDRIVE_TEST_BINARY` to select another executable. CLI reference checks independently enumerate source commands and runtime help, then verify configuration examples, native parameters, links, and generator error handling. Python tests mock compilers, signing tools, and subprocesses while checking actual temporary bundle layouts, plists, hashes, permissions, and rollback.

The default-ignored Samba proofs need the fixture below. Ignored tests are not passes. Test the development AST scope helper separately:

```sh
cargo test --locked --manifest-path tools/coverage-scope/Cargo.toml
```

## Vendored SMB protocol regressions

The root workspace contains RDrive and the patched SMB SDK. Its default member
is RDrive, so ordinary `cargo build` and `cargo test` keep their application
scope. Run the SDK's library unit tests explicitly with the same async,
signing, and encryption features used by the client:

```sh
cargo test --locked --package smb --no-default-features \
  --features async,sign,encrypt --lib -- --nocapture
```

This uses the root `Cargo.lock`, including the SDK's test dependencies. It runs
the library regressions for interim `STATUS_PENDING` credit grants and queued
pending/final responses without an external server. `--lib` keeps upstream
integration targets out of this command; the isolated Samba fixture below is
the documented integration suite. The AST helper and `vendor/smb-msg` remain
outside workspace membership; the helper retains its independent lockfile,
while the SMB message crate continues to resolve through the root patch.

These tests supplement application coverage. Vendored dependencies remain
outside the first-party production line percentage.

## Isolated behavior workflows

Each script accepts the CLI executable under test. The DAV/IPC suite does not require Docker:

```sh
python3 tests/cli_workflows.py target/debug/rdrive
python3 tests/advanced_workflows.py target/debug/rdrive
python3 tests/smart_workflows.py target/debug/rdrive
python3 tests/namespace_workflows.py target/debug/rdrive
python3 tests/provider_receipts_workflows.py target/debug/rdrive
python3 tests/coverage_cli_workflows.py target/debug/rdrive
python3 tests/cache_directory_workflows.py target/debug/rdrive --report target/validation/cache.json
python3 tests/mcp_workflows.py target/debug/rdrive --report target/validation/mcp.json
python3 tests/mcp_startup_workflows.py target/debug/rdrive --report target/validation/mcp-startup.json
python3 tests/index_workflows.py target/debug/rdrive --output target/validation/index.json
python3 tests/search_workflows.py target/debug/rdrive --report target/validation/search.json
RDRIVE_HISTORY_REPORT=target/validation/history.json \
  python3 tests/history_workflows.py target/debug/rdrive
python3 tests/policy_workflows.py target/debug/rdrive --output target/validation/policy.json
python3 tests/service_workflows.py target/debug/rdrive --output target/validation/service.json
```

These workflows verify actual requests, bytes, persistent state, process termination, and independent-writer competition. Record the binary version/hash with results; do not attribute one executable's behavior to another. Scripts without report-path options print results, and some also write Git-ignored `docs/*.debug.json`. Do not overwrite committed version evidence.

The MCP startup workflow launches 128 real processes with each of one and two Tokio workers while four existing sessions and four temporary CPU workers compete. It checks every initialization using the ordinary 35-second response bound, verifies the existing sessions still respond, and records each case plus the binary hash. A timeout fails the workflow; on macOS the shared client captures the owned child's live stack before cleanup.

Run full SMB workflows and the raw/Smart bridges separately:

```sh
python3 tests/smb_workflows.py target/debug/rdrive
RDRIVE_SMB_PROVIDER_ONLY=1 python3 tests/smb_workflows.py target/debug/rdrive
RDRIVE_SMB_PROVIDER_ONLY=1 RDRIVE_SMB_SMART=1 \
  python3 tests/smb_workflows.py target/debug/rdrive
```

`RDRIVE_DOCKER` selects the Docker executable. With `RDRIVE_SMB_TEST_RUNNER` pointing to the compiled Rust library test executable, the fixture also runs the ignored guarded/resumable proofs. Set `RDRIVE_SMB_LIFECYCLE=1 RDRIVE_SMB_PROOF_ONLY=1` to run the lifecycle proof separately in that fixture. These variables control test scripts, not production client configuration.

If a concurrent managed-SMB commit exceeds its timeout, the fixture preserves
bounded partial stdout/stderr and records available transaction phases,
commit sequence information, and published slot names before temporary data
is cleaned up. The original timeout remains fatal; diagnostic collection does
not convert it into a pass.

## Test map

- Connections, paths, and backend capabilities: `tests/coverage_backend.rs`, `tests/webdav.rs`, and `src/config.rs` tests.
- Downloads/checkpoints, immutable upload snapshots, progress, limits, and concurrency: `src/resume.rs`, `src/operations.rs`, `src/transfer.rs`, `src/scheduler.rs`, plus CLI/advanced/SMB workflows.
- Cache budgets, pins, active readers, clearing, and directory migration: `tests/cache.rs`, `tests/cache_directory_workflows.py`, and `src/cache*.rs`.
- Ordinary synchronization and metadata: `src/sync.rs`, `src/metadata*.rs`, and CLI/advanced/SMB workflows.
- Account-bound file history, recycle/restore receipts, retention and APFS boundaries: `src/history.rs`, `src/history_control.rs`, `src/checkpoints.rs`, `tests/history_workflows.py`, and native history tests. The history workflow reads native APFS capability/status without creating system snapshots. The separate checkpoint workflow uses disposable, privately owned fixed-capacity APFS images for real vault and resident scheduling integration; Rust host mocks exercise platform errors and privileged legacy boundaries.
- Managed transactions and lost responses: `src/repository.rs` and advanced workflows, including competing writers and post-publication SIGKILL.
- Smart queues, namespace changes, and conflict decisions: `tests/smart.rs`, `src/provider/`, and Smart/namespace/policy/receipts workflows.
- SQLite, incremental discovery, and search: `src/provider/index*.rs` and index/search workflows.
- MCP: `src/mcp.rs`, `src/mcp/catalog.rs`, `tests/mcp_catalog_contract.rs`, and stdio workflows covering permissions, scopes, protocol, and cancellation.
- Mounts and services: the fake system in `tests/mount.rs` and service workflows. Real-system acceptance is separate.
- Swift: `macos/tests/NativeTests.swift` and `coverage_*.swift`, using real SDK data objects with injectable bridge, manager, notification, and UI boundaries.
- CLI contracts: `cli_contract` in `src/main.rs` traverses the actual Clap tree and checks all 120 leaf commands, 901 parameter declarations including positional/inherited arguments, enum values, numeric types, and conflicts. It currently generates 1,287 accepted-value cases and saves the inventory to `target/validation/cli-parameter-cases.json`.
- MCP field contracts: `tests/mcp_catalog_contract.rs` checks all 105 tools and 231 fields, schemas, valid/invalid input, positional/flag mapping, and real CLI help parsing: 490 valid and 2,985 invalid cases. Help parsing performs no storage operation and creates no state directory.
- Parameter documentation: `tests/test_documentation.py` checks help trees, configuration, and native parameters. The generated reference also includes help/version options and command groups, so its declaration count differs from the production argument contract. Contracts do not cover every semantic combination; module tests and protocol workflows verify behavior.

Run these contracts independently:

```sh
cargo test --locked --bin rdrive cli_contract::
cargo test --locked --test mcp_catalog_contract
python3 -m unittest discover -s tests -p 'test_documentation.py' -v
```

## Measure production line coverage

The acceptance target is **100% executable production lines for each language separately**. A combined total cannot hide a gap in one language.

Application scope is current-target Rust under `src/`, all macOS production Swift, and four distributed Python scripts: `macos/build.py`, `scripts/install-skills.py`, `scripts/mcp-config.py`, and `scripts/generate-cli-docs.py`. Dependencies, tests, generated files, and development test tools are outside application scope. Production adapters, error handling, recovery, and compiled platform paths remain included.

Rust and Swift use unique LLVM LCOV `(file, line)` execution records. For duplicate closure/generic/object mappings, take the maximum observed hit count for each physical line. Retain raw reports; duplicated Swift `LF` or LLVM summary counts are not physical source-line counts. Rust test-only spans come from a `syn` syntax tree, excluding only items guaranteed to require `cfg(test)` or test attributes, rather than guessing braces with regexes. Missing production mappings fail the gate instead of silently contributing zero lines. Stable Rust line instrumentation does not collect branch coverage; report it as unmeasured, never 100%.

Install tools and collect the complete Rust suite:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.1 --locked
python3 tests/source_manifest.py --output target/coverage/source-manifest.json
python3 tests/run_rust_coverage.py \
  --output-dir target/coverage/rust-acceptance --workflows all
```

`--output-dir` must not exist, ensuring fresh raw profiles. The collector reads instrumentation settings from `cargo-llvm-cov show-env` and freezes the exact CLI/test executables returned by Cargo JSON. It runs ordinary tests, then every DAV/IPC/Samba workflow above with that same CLI, including all three ignored SMB proofs inside their fixture. Real network-mount acceptance stays ignored and is not counted as passed.

Only these explicit objects are exported with Rust's bundled LLVM. Outputs include `coverage.lcov`, `test-spans.json`, per-command logs, and `manifest.json`. The manifest records source/object hashes. Changed inputs, failed commands, or missing profiles fail collection. This does not rely on automatic discovery of old objects in a Cargo coverage directory.

The library harness runs with `--test-threads=1` to isolate fixtures that exercise process-global signals, subprocess creation, and file-lock lifetimes. Every selected test still runs, and deliberate thread/process concurrency inside individual tests remains enabled. Separate integration-test executables keep their normal harness settings; the manifest records the exact commands.

Every executable and workflow phase receives a fixed, sanitized label in its
`LLVM_PROFILE_FILE` basename, followed by the unchanged `%p-%16m` process/merge-pool
pattern. `phases.json` records the exact command, profile pattern, start/end time,
exit code, and failure type, including failed or timed-out phases. Phase labels
identify provenance; they do not establish why a raw profile became invalid.

If `llvm-profdata merge` fails, collection still fails with the original error.
The runner preserves only known raw inputs explicitly named in LLVM error or
warning diagnostics, with size, modification time, SHA-256 and initial header
bytes. Evidence is bounded to eight profiles, 16 MiB for a complete file and
32 MiB of copied bytes overall. Oversized inputs retain a bounded header copy
plus full streamed hash/metadata; the report marks incomplete or unavailable
preservation and any omitted profiles. It never excludes a profile from the
merge, retries a failed merge, or substitutes a partial coverage result.
Both existing always-run CI artifact uploads include `phases.json` and the
`profile-merge-diagnostics/` directory, while valid raw-profile collections are
not uploaded wholesale. Original command logs remain available alongside them.

`--target-dir PATH` may reuse build caches while retaining fresh profiles and exact current Cargo artifacts. `--offline` disables Cargo networking, and `--cargo-llvm-cov PATH` selects an installed tool. `--workflows dav` collects DAV/IPC, `smb` collects Samba, and `none` runs only Rust tests; none of these subsets is complete acceptance. `--test-suite portable-smb` runs only SMB library tests and operations integration tests and requires `--workflows smb`. It supplements Linux protocol testing; macOS acceptance retains the default `all`.

Do not clear collection directories, remove `.profraw`, or replace objects before aggregation. Source changes require new collection; old line mappings cannot be reused for a new revision.

Collect Swift and Python separately. Apple and Rust LLVM versions may differ; do not merge their raw profiles:

```sh
python3 macos/tests/run.py --coverage --output-dir target/native-tests-coverage
python3 -m venv target/python-coverage-venv
target/python-coverage-venv/bin/python -m pip install coverage==7.17.2
COVERAGE_FILE=target/coverage/python.coverage \
RDRIVE_TEST_BINARY="$PWD/target/coverage/rust-acceptance/build/debug/rdrive" \
target/python-coverage-venv/bin/python -m coverage run --branch \
  --include='*/macos/build.py,*/scripts/*.py' \
  -m unittest discover -s tests -p 'test_*.py'
COVERAGE_FILE=target/coverage/python.coverage \
target/python-coverage-venv/bin/python -m coverage json -o target/coverage/python.json
python3 tests/source_manifest.py --output target/coverage/source-manifest.json --check
```

Generate the combined report and enforce the strict gate:

```sh
python3 tests/report_coverage.py \
  --rust-lcov target/coverage/rust-acceptance/coverage.lcov \
  --rust-test-spans target/coverage/rust-acceptance/test-spans.json \
  --swift-lcov target/native-tests-coverage/coverage.lcov \
  --python-json target/coverage/python.json \
  --source-manifest target/coverage/rust-acceptance/manifest.json \
  --source-manifest target/native-tests-coverage/coverage-manifest.json \
  --source-manifest target/coverage/source-manifest.json \
  --output target/coverage/production.json
```

`--min-line 100` is the default. Formal acceptance requires matching source-hash manifests for every production source; create the snapshot before testing and verify it afterward. Any language gap, missing mapping, mismatched manifest, or incomplete input returns failure while preserving file hashes and uncovered lines. An explicitly lower threshold is diagnostic only and cannot establish 100% acceptance. Repeat `--python-json` to union separate Python runs by actual hits without double-counting lines.

## Continuous integration

Three workflows keep compilation, validation, and the website separate:

- **Build** (`.github/workflows/build.yml`) runs on pushes to `main`, pull requests targeting `main`, and `v*` version tags. It builds the release CLI and optional native companion, checks version-tag consistency, and preserves build artifacts.
- **Checks** (`.github/workflows/checks.yml`) runs Rust, DAV/IPC, Swift, Python, and documentation contracts, plus disposable Linux Samba coverage. Its final job requires 100% production line coverage for each language.
- **Documentation** (`.github/workflows/docs.yml`) builds the documentation site and deploys GitHub Pages from `main`. Pull requests validate the site without deploying it.

Checks collects ordinary Rust/DAV/IPC, Swift, and Python on macOS, and Docker-backed Samba on Linux. Before the Samba workflows, the Linux job also runs every vendored SMB library unit test explicitly through the root locked workspace. Both Rust collectors freeze source and objects at the same Git revision. The final job checks every source hash and uses the first macOS result's executable-line inventory as the acceptance target. Linux executions contribute hits only to **those same macOS source lines**. Linux-only platform lines are outside macOS coverage, while the original Linux report is retained. This does not establish 100% coverage for a Linux client.

Merge with the tested helper, listing macOS before Linux:

```sh
python3 tests/merge_rust_coverage.py \
  --input-dir target/coverage/rust-macos \
  --input-dir target/coverage/rust-linux \
  --output target/coverage/rust.lcov
```

The first collection must come from macOS with all ordinary tests and DAV workflows. Different revisions, source inventories, or hashes are rejected. Manifests bind LCOV and test-span SHA-256 values, so replacing a report is detected even when source files remain unchanged. `report_coverage.py` applies the default 100% threshold separately to Rust, Swift, and Python; any gap fails CI. Raw LCOV, source/object manifests, workflow logs, and the final file-level report are retained as artifacts. A local pass does not establish a hosted Actions pass.

## Real-system validation

`tests/mount_workflows.py` actually mounts and unmounts test volumes. `service_workflows.py --launchd` manages a temporary user LaunchAgent. `tests/provider_native_workflows.py` and `macos/tests/activation_probe.py` use real extensions, registered domains, and system state.

These are not side-effect-free baseline CI. Read the script, identify the exact App/domain/IPC paths, and use a dedicated test account or environment. Do not share the default development IPC directory with active personal domains during tests.

Real Finder menus, status icons, placeholders, downloading, eviction, offline editing, and tag display each need observed system evidence. Mock coverage establishes client logic and interface calls, not server power-loss durability, every SMB/WebDAV implementation, or every macOS version.

## Large-catalog regression checks

The macOS Checks job also runs `provider::policy_projection_tests` in release mode with `--include-ignored`. Its ordinary differential cases run in the regular Rust suite; the two larger benchmarks are executed explicitly by this additional step. They compare complete projected records, metadata selection, and unchanged journals against the prior implementation, including request alias precedence and required normalization updates.

The fixtures use 108,675 synthetic catalog items, 529 retained requests, and 14,590 metadata demands. Timings are reported without a hardware-dependent pass threshold. The pipeline measurement separates first publication, control updates, reads, required normalization, and genuinely unchanged overlays. These measurements do not establish a real server’s network or first-scan latency. CI retains `catalog-benchmarks.log` with its validation artifacts.

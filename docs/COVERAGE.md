# Production coverage snapshot

On 2026-11-10, the source delivered in [commit `f831526`](https://github.com/lorian7392/RDrive/commit/f83152605c7a283bc7a09ba00f612344fc0d8467) passed **100% executable production line coverage for each language**:

- Rust: **15,803 / 15,803 lines**, covering first-party `src/` production code for the measured macOS target.
- Swift: **1,863 / 1,863 lines**, including all ten macOS production source files.
- Python: **291 / 291 lines**, covering all four distributed scripts, plus **94 / 94 measured branches**.

The [original file-level report and SHA-256 records](validation/production-coverage-2026-11-10.json) retain executable/covered lines, empty uncovered-line lists, source hashes, input checksums, and validation results. The original JSON is preserved unchanged.

This is a historical snapshot of `f831526`, not a coverage claim for later English documentation, generator changes, CI fixes, or other revisions. The report was produced from a frozen working tree before that commit. Its recorded base HEAD identifies the preceding commit; the per-file SHA-256 values identify the actual measured source delivered in `f831526`. Any later source change requires fresh collection and a passing gate. Do not attribute these counts or hashes to the current checkout without verification.

## Scope and counting

Dependencies, tests, generated files, and development test tools are outside application scope. Production system adapters, failure handling, recovery, and compiled platform paths were retained. No production coverage-exclusion markers reduced the denominator.

Rust and Swift count unique LLVM LCOV `(file, line)` records, taking the maximum actual hit count when closures, generics, or several objects map to the same physical line. A `syn` syntax tree identifies Rust inline and external test modules. Missing production mappings fail acceptance. Python uses actual `coverage.py` execution records.

**100% line coverage does not mean 100% branch coverage or every input combination.** Rust branches were unmeasured, and this record gives no Swift branch percentage. Command/parameter contracts, faults, and protocol workflows provide additional behavior checks.

## Checks in this snapshot

- Nine Rust test executables: **416 passed, 0 failed, 4 ignored**.
- Three default-ignored SMB guarded/resumable/lifecycle proofs were separately executed and passed in the disposable Samba fixture. The real native network-mount test remained ignored and was not counted as passed.
- **16 CLI/DAV/IPC/Samba workflow groups** passed: crash receipts, cache migration, SQLite indexing, search, filters/conflict policy, service plans, and raw/Smart SMB providers. Rust collection used 1,127 fresh profiles.
- Python: **59 passed**. Documentation contracts after final formatting: **13 passed**. Rust test-scope parser: **2 passed**.
- CLI contracts: **97 leaf commands, 727 parameter declarations, 1,034 accepted-value cases**. The help reference's 788 option declarations also include command groups and help/version entries, so the inventories use different counting rules.
- MCP contracts: **81 tools, 186 fields, 394 valid cases, 2,392 invalid cases**, with complete runtime schema consistency.
- Swift was rerun in a fresh directory using real SDK data objects, callbacks, and temporary Unix/TCP peers. All ten production sources were measured, and source hashes matched the reports.

Storage tests used temporary synthetic content. System management, UI, process, and failure boundaries used mocks where needed; default production implementations retained real interfaces. The run did not operate personal cloud/NAS files, real mounts, or registered domains, query real Keychain credentials, or request native cache eviction.

These results establish the tested code and assertions. They do not establish that Finder accepted a particular signed bundle, displayed real menus/status icons, or interoperates with every WebDAV/SMB server and macOS version. See [real-system validation](TESTING.md#real-system-validation) and the [validation archives](README.md#historical-validation-records) for the separate system and deployment evidence.

## Reproduce and enforce new results

The [testing guide](TESTING.md#measure-production-line-coverage) covers building, freezing inputs, collection, and aggregation. The Rust collector exports only exact artifacts returned by Cargo, uses fresh profiles, and checks source/object identity before and after the run. Its report also binds LCOV and test-span checksums. Strict aggregation requires matching source provenance for every production file.

Original profiles, objects, and complete LCOV remain in local ignored `target/` directories. The Git snapshot retains compact file-level results and input checksums. Current CI uploads reports/logs and requires fresh 100% results separately for Rust, Swift, and Python.

Use the Actions results for the specific commit to assess hosted CI. This local historical pass does not establish a hosted workflow result or waive the current coverage gate.

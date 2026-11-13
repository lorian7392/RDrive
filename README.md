# RDrive

[![Build](https://github.com/lorian7392/RDrive/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/lorian7392/RDrive/actions/workflows/build.yml)
[![Tests](https://github.com/lorian7392/RDrive/actions/workflows/checks.yml/badge.svg?branch=main)](https://github.com/lorian7392/RDrive/actions/workflows/checks.yml)
[![Documentation](https://github.com/lorian7392/RDrive/actions/workflows/docs.yml/badge.svg?branch=main)](https://github.com/lorian7392/RDrive/actions/workflows/docs.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> **Personal project under active development**
>
> RDrive is developed for personal use and may contain bugs that cause data loss, corruption, overwrites, or unintended deletion. Keep independent backups and test with disposable files before use. **Use at your own risk. No guarantees are provided for data safety, integrity, availability, or recovery.**

**A WebDAV and SMB file and synchronization client for macOS, built with Rust.**

RDrive combines resumable transfers, offline caching, revision-protected synchronization, and recoverable file operations in a standalone command-line tool. An optional macOS companion adds Finder placeholders, on-demand downloads, and offline editing through File Provider. Managed repositories provide complete multi-file snapshot publication for cooperating clients.

[Documentation](https://lorian7392.github.io/RDrive/) · [Quick start](docs/QUICKSTART.md) · [Features](docs/FEATURES.md) · [CLI reference](docs/reference/CLI.md) · [Configuration](docs/reference/CONFIGURATION.md)

## Features

- Inspect connections, background service, indexes, cache, file history, checkpoints and conflicts with one [status overview](docs/STATUS.md).
- Browse, read, upload, copy, move, delete, and edit remote files.
- Resume verified downloads, SMB uploads, and explicitly configured Nextcloud chunked uploads.
- Queue immutable upload snapshots, limit bandwidth, and run up to 64 parallel queue jobs.
- Keep selected files offline, enforce an LRU clean-cache budget, and safely clear or relocate cached payloads.
- Preview and apply two-way or one-way directory synchronization with shared baselines and conflict detection.
- Use native macOS network mounts or an optional Smart File Provider view.
- Manage Smart offline edits, file and directory operations, SQLite indexes, filename/path search, filters, versions, conflicts, and background services.
- Synchronize selected Finder tags and user extended attributes.
- Publish managed-repository transactions with concurrent-writer protection and uncertain-commit recovery.
- Connect AI clients through a read-only-by-default MCP server and three companion Skills.

See the [feature guide](docs/FEATURES.md) for protocol differences, protections, and limits.

## Install from source

Requires **Rust 1.90+** and the macOS command-line developer tools. Keep the committed `Cargo.lock` and patched `vendor/` dependencies.

~~~sh
git clone https://github.com/lorian7392/RDrive.git
cd RDrive
cargo build --release --locked
./target/release/rdrive --version
export PATH="$PWD/target/release:$PATH"
~~~

Apple Silicon users with repository access can also [install the CLI from a GitHub Release](docs/INSTALLATION.md#install-the-release). Check for updates with `rdrive update`.

The CLI runs without FUSE, Docker, or the companion App. See [installation](docs/INSTALLATION.md) for the optional native bundle, signing, and upgrades. A source checkout does not include local build artifacts or account configuration.

## Connect

Replace the example endpoint and username with your own. In an interactive **zsh** terminal, read the password without echoing it or putting its literal value into shell history:

~~~sh
read -rs 'RDRIVE_PASSWORD?Password: '
printf '\n'
export RDRIVE_PASSWORD

rdrive profile add dav --protocol webdav \
  --url https://dav.example.com/files/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info dav
rdrive ls dav
~~~

For SMB, enter that server's password first, or use a separate password variable:

~~~sh
rdrive profile add nas --protocol smb \
  --url smb://nas.example.com/share/folder \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info nas
~~~

Profiles store the environment-variable name, not its value. macOS Keychain is also supported. WebDAV requires HTTPS by default; SMB requires transport encryption. See [authentication](docs/QUICKSTART.md#authentication-and-connections).

## Common operations

Remote paths are relative to the profile URL. Transfers, copies, and moves preserve existing destinations by default.

~~~sh
rdrive get dav reports/report.pdf ./report.pdf
rdrive put dav ./notes.txt notes.txt
rdrive get nas large.zip ./large.zip --resume
rdrive put nas ./large.zip uploads/large.zip --resume --queue
rdrive queue run nas
~~~

Prepare a verified offline copy:

~~~sh
rdrive cache config --max-bytes 5368709120
rdrive cache fetch dav notes.txt --pin
rdrive cache get dav notes.txt ./offline-notes.txt
~~~

Preview synchronization before applying it:

~~~sh
mkdir -p ./work
rdrive mkdir dav work
rdrive sync dav ./work work
rdrive sync dav ./work work --apply
~~~

Continue with the [quick start](docs/QUICKSTART.md) or [troubleshooting guide](docs/TROUBLESHOOTING.md).

## Behavior and limitations

Ordinary file operations and directory synchronization use multiple requests and do not form cross-file transactions. Managed repositories provide complete snapshot visibility to cooperating clients. Overwrite protection depends on correct server revision conditions or protected SMB handles. Versions and recovery copies on the same server do not replace independent backups.

Local caches, upload snapshots, and retained edits are stored in plaintext. RDrive does not implement Cryptomator, public sharing or file requests, remote ACL editing, complete macOS metadata backup, or a full GUI connection manager. Native network-volume caches are managed by macOS and are separate from RDrive's clean-cache budget.

## Development

The current source version is **0.8.3**, licensed under [MIT](LICENSE).

The [2026-11-10 coverage snapshot](docs/COVERAGE.md) measured **100% production line coverage** for Rust, Swift, and Python. It records the measured source hashes and scope; it is distinct from the status of subsequent CI runs. System-boundary mocks and isolated DAV/Samba fixtures complement real-system validation.

- [Build and test](CONTRIBUTING.md)
- [Test methodology and coverage](docs/TESTING.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Changelog](CHANGELOG.md)
- [Security policy](SECURITY.md)
- [All documentation](docs/README.md)

Please include a redacted version, command, error, and minimal reproduction when [reporting an issue](https://github.com/lorian7392/RDrive/issues).

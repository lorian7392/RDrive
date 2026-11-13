# RDrive

> **Personal project under active development**
>
> RDrive is developed for personal use and may contain bugs that cause data loss, corruption, overwrites, or unintended deletion. Keep independent backups and test with disposable files before use. **Use at your own risk. No guarantees are provided for data safety, integrity, availability, or recovery.**

RDrive is a Rust command-line client for **WebDAV and SMB** on macOS. It combines remote file operations, recoverable transfers, offline caching, and guarded synchronization. An optional native companion adds a Finder view with system placeholders and on-demand downloads.

Start with the CLI. Add Smart Synchronization or a managed snapshot repository when your workflow needs it.

[Get started](QUICKSTART.md){ .md-button .md-button--primary }
[Install RDrive](INSTALLATION.md){ .md-button }

## Start here

1. **[Install](INSTALLATION.md)** the CLI from a release on Apple Silicon, or build it from source.
2. **[Connect](QUICKSTART.md#authentication-and-connections)** to your own WebDAV or SMB server using an environment variable or macOS Keychain credential.
3. **[Browse and transfer files](QUICKSTART.md#browse-and-transfer-files)**, then configure caching, synchronization, or Finder integration as needed.

```sh
rdrive profile add dav --protocol webdav \
  --url https://dav.example.com/files/demo/ \
  --username demo --password-env RDRIVE_PASSWORD
rdrive info dav
rdrive ls dav
```

Prepare `RDRIVE_PASSWORD` using the private prompt in the [quick start](QUICKSTART.md). Examples use reserved domains and synthetic paths; replace them with your own server details.

## Choose a workflow

| Workflow | What it provides | Guide |
| --- | --- | --- |
| CLI | Remote files, streaming transfers, resumable checkpoints, cache, and directory sync | [Quick start](QUICKSTART.md) |
| Native network volume | macOS-managed SMB/WebDAV mounting | [Mounting](MOUNT.md) |
| Smart Finder view | File Provider placeholders, offline edits, pinning, and background reconciliation | [Smart Synchronization](SMART_SYNCHRONIZATION.md) |
| Managed repository | Immutable content and atomic snapshot publication across multiple files | [Managed repositories](REPOSITORY.md) |
| MCP | Scoped AI access through the same CLI, read-only by default | [MCP and skills](MCP.md) |

See [features and limitations](FEATURES.md) for protocol differences, overwrite protection, and unsupported features. Ordinary directory sync and raw mounts do not provide the snapshot semantics of managed repositories.

## Find an answer

- **Status overview:** [connections, background service and retained state](STATUS.md).
- **History and recovery:** [retained file versions, Finder recovery, and optional APFS checkpoints](HISTORY.md).
- **File availability:** [cache limits, clearing, and migration](CACHE_MANAGEMENT.md); [Smart operations and recovery](SMART_OPERATIONS.md).
- **Browsing and discovery:** [SQLite indexing and incremental refresh](DIRECTORY_INDEXING.md), including local index search.
- **Native integration:** [macOS companion and extensions](MACOS_INTEGRATION.md); [SMB notifications and handle recovery](SMB_LIFECYCLE.md).
- **Commands and settings:** [complete CLI reference](reference/CLI.md), [configuration reference](reference/CONFIGURATION.md), and [all MCP tool schemas](reference/MCP.md).
- **Failures:** [troubleshooting](TROUBLESHOOTING.md), including retained edits, uncertain commit results, and Finder activation. See [conflict repair](CONFLICTS.md) for ordinary sync and Smart recovery.

## Develop and contribute

Read the [architecture](ARCHITECTURE.md), [testing guide](TESTING.md), [coverage evidence](COVERAGE.md), [version-tag procedure](RELEASING.md), and [website guide](WEBSITE.md).

The repository also contains [contribution guidelines](https://github.com/lorian7392/RDrive/blob/main/CONTRIBUTING.md), a [changelog](https://github.com/lorian7392/RDrive/blob/main/CHANGELOG.md), a [security policy](https://github.com/lorian7392/RDrive/blob/main/SECURITY.md), and the [MIT license](https://github.com/lorian7392/RDrive/blob/main/LICENSE).

## Historical validation records

Version-specific records describe the source, fixtures, and system observations at the time they were collected. They do not establish the state of a current installation or the coverage of a later commit. Raw records remain in the repository rather than being copied into the public website.

- [0.8.0: cache management and migration](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.8.0.md)
- [0.7.0: Smart operations, policies, services, and search](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.7.0.md)
- [0.6.0: SQLite indexing and incremental refresh](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.6.0.md)
- [0.5.0: MCP and skills](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.5.0.md)
- [0.4.1: progressive directory discovery](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.4.1.md)
- [0.4.0: Smart cache coordination](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.4.0.md)
- [0.3.1: WebDAV compatibility](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.3.1.md)
- [0.3.0: metadata, transactions, and native integration](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.3.0.md)
- [0.2: transfers, mounts, and guarded SMB writes](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION_0.2.md)
- [Original validation index](https://github.com/lorian7392/RDrive/blob/main/docs/VALIDATION.md)

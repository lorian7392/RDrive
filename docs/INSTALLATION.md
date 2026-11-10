# Installation and upgrades

Start with the standalone CLI. The optional native companion provides Finder and File Provider integration; CLI transfers, caching, synchronization, and MCP do not require it.

## Requirements

- **macOS** is the primary development and validation platform. The native App targets macOS 13.0+, with observed compatibility limited to the systems recorded in [testing](TESTING.md).
- **Rust and Cargo 1.90+**, as declared in `Cargo.toml`.
- **macOS command-line developer tools** for the linker and Apple SDK. Native builds also need Swift and Python 3.
- A reachable WebDAV or SMB server and an appropriate account. Anonymous WebDAV does not need a password.

Running RDrive does not require Docker, a local Samba server, or FUSE. Docker is used by isolated SMB tests. Other operating systems and all NAS models are not claimed as validated platforms.

Check the toolchain:

~~~sh
rustc --version
cargo --version
xcode-select -p
~~~

If needed, run `xcode-select --install` and complete the system installer. Install Rust through the [official Rust instructions](https://www.rust-lang.org/tools/install).

## Build the CLI

~~~sh
git clone https://github.com/lorian7392/RDrive.git
cd RDrive
cargo build --release --locked
./target/release/rdrive --version
./target/release/rdrive --help
~~~

`--locked` uses the committed `Cargo.lock`. Keep `vendor/smb` and `vendor/smb-msg` with the source; their patches are required to build, so copying only `src/` is insufficient. Each dependency directory includes `RDRIVE-PATCH.md`, upstream attribution, and licensing.

The first build downloads dependencies that are not already cached. `--offline` is appropriate only after the complete dependency cache is available. Configure Cargo for your own network as needed; no particular mirror or system-proxy change is required.

Use the build from the current shell:

~~~sh
export PATH="$PWD/target/release:$PATH"
rdrive --help
~~~

Or install into Cargo's user executable directory:

~~~sh
cargo install --path . --locked
rdrive --version
~~~

This normally installs into `~/.cargo/bin`, which must be in `PATH`. A checkout does not include `dist/`, `target/`, or `macos/build/`. Recorded development builds do not imply that downloadable signed or notarized distributions exist.

## State directory

The default state directory is `~/Library/Application Support/rdrive`. It retains profile configuration, cache indexes, upload snapshots, synchronization baselines, and recovery records. Override it with `RDRIVE_HOME` or global `--home`:

~~~sh
rdrive --home "$HOME/Library/Application Support/rdrive-demo" profile list
~~~

Keep state outside synchronization sources, shared directories, and public repositories. Separate state directories have independent profiles and jobs; changing `--home` does not migrate existing work. Locks coordinate operations sharing a state directory. Old and new background processes must not write the same state concurrently.

The CLI has no plaintext password argument. See [authentication](QUICKSTART.md#authentication-and-connections) for hidden input and Keychain setup, and the [configuration reference](reference/CONFIGURATION.md) for environment variables and directory layout.

## Optional native companion

`macos/` contains the menu-bar companion, replicated File Provider, File Provider action UI, and Finder Sync extension. Building does not install, register, or enable extensions, or create a remote connection.

Build the release CLI first. For a development bundle, create a dedicated current-user-owned IPC directory with mode `0700`:

~~~sh
RDRIVE_DEV_IPC="$(mktemp -d "$TMPDIR/rdrive-ipc.XXXXXX")"
chmod 700 "$RDRIVE_DEV_IPC"
python3 macos/build.py \
  --cli target/release/rdrive \
  --development-ipc-root "$RDRIVE_DEV_IPC"
macos/build/RDrive.app/Contents/MacOS/RDriveCompanion --diagnose
~~~

Retain the chosen IPC path; the bridge and App must use the same path. Do not use your home directory, the whole temporary directory, or another running instance's directory. Do not remove it while it is in use.

The default `--identity -` creates an **ad-hoc local development build**. An explicit development IPC path does not provide production App Group entitlement, notarization, Gatekeeper acceptance, or all extension runtime permissions. System acceptance, extension enablement, and Finder behavior require separate validation.

For a signed build, supply identities and entitlements you are authorized to use:

~~~sh
python3 macos/build.py \
  --cli target/release/rdrive \
  --identity 'YOUR_SIGNING_IDENTITY' \
  --team-id YOUR_TEAM_ID \
  --app-group YOUR_AUTHORIZED_APP_GROUP
~~~

The script verifies signatures but does not complete Apple's distribution process. Production distribution still requires suitable entitlements, signing, notarization, and other applicable steps. Rebuilding the CLI does not replace the copy embedded in an installed App.

Follow [macOS integration](MACOS_INTEGRATION.md) for enablement and registration, and [Smart synchronization](SMART_SYNCHRONIZATION.md) for domains and the bridge. Historical versions, paths, and process observations describe their original environments, not an installed state for a new checkout.

## Upgrade

1. Read the [changelog](https://github.com/lorian7392/RDrive/blob/main/CHANGELOG.md) and the target version's validation scope.
2. Check `queue list`, `smart status`, `smart list DOMAIN`, and `transfers list`. Export unsent edits that need an additional copy.
3. Stop the relevant `watch`, bridge, MCP, and companion processes. For an installed user service, use `rdrive service stop`. Confirm that no old process continues writing the state directory.
4. Preserve state and required partial downloads. Back them up, update the source, and rebuild with `cargo build --release --locked`.
5. Rebuild and switch the App, bridge, and notification helper if native integration is used. Verify the CLI version, embedded version, state directory, and IPC path.
6. Inspect profiles, jobs, and domains through read-only commands before resuming background work.

SQLite migration preserves the old JSON as history. Do not run older writers against upgraded state. Downgrade compatibility is not generally guaranteed: replacing a binary does not undo a database migration or a remote operation.

## Disable or uninstall

Handle unsent content and recovery copies before stopping processes. `service uninstall` removes the managed service configuration; it does not perform every native-domain or system-cache cleanup step. See [macOS integration](MACOS_INTEGRATION.md) for domain removal and retained downloads.

Deleting the CLI, source checkout, or development App does not delete remote data, clear queues, remove File Provider domains, or erase Keychain entries. State contains recovery content. Remove it separately only when it is no longer needed; deleting the whole state directory is not a cache-clearing procedure.

Next: [Quick start](QUICKSTART.md) · [Configuration](reference/CONFIGURATION.md) · [Troubleshooting](TROUBLESHOOTING.md)

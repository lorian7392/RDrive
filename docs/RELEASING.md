# Version tags and CI builds

RDrive uses `v<version>` Git tags, such as `v0.8.1`, for source versions. The tag must exactly match `package.version` in `Cargo.toml`, the compiled CLI's `--version`, and the optional native host and extension bundle versions.

A source tag does not create a GitHub Release, install an application, or certify a signed and notarized distribution. CI's companion build uses ad-hoc signing for build verification.

## What runs automatically

Pushes to `main`, pull requests targeting `main`, and pushes of `v*` tags run both workflows:

- **Build** compiles the optimized Rust CLI, builds and verifies the optional macOS companion in an isolated output directory, checks CLI/bundle/tag consistency, and uploads a CLI archive, SHA-256 checksums, and build reports as a short-lived Actions artifact. The archive preserves the CLI's executable permission.
- **Checks** runs the Rust, Python, native SDK/mock, DAV/IPC, and isolated Samba tests, then enforces the production coverage thresholds described in [Testing](TESTING.md).

Both workflows also support manual runs. Their results are separate: a successful compilation does not imply the tests or coverage gate have passed. The build artifact targets the architecture of the `macos-26` runner; it is not a universal macOS binary or a notarized application package.

## Prepare a version

1. Update `Cargo.toml` and the root package entry in `Cargo.lock` together. Update the native version fields in `macos/build.py` and the relevant version references and changelog.
2. Run the development checks in [Contributing](https://github.com/lorian7392/RDrive/blob/main/CONTRIBUTING.md) and [Testing](TESTING.md). Generate fresh coverage evidence when production code changes.
3. Push the version commit to `main` and wait for both **Build** and **Checks** to pass for that exact commit.
4. Create and push an annotated version tag. Never move a published version tag to a different commit; publish a new version for a correction.

For the current `0.8.1` version:

```sh
git switch main
git pull --ff-only
git tag -a v0.8.1 -m "RDrive 0.8.1"
git push origin refs/tags/v0.8.1
```

Use `git tag -s` instead of `git tag -a` when a signing key is configured. Replace the example version for later releases. A tag push reruns the workflows; a tag such as `v0.8.2` on source that still declares `0.8.1` fails version validation.

## Check a build locally

```sh
cargo build --release --locked
python3 macos/build.py --cli target/release/rdrive --output target/native-version-check
python3 tests/check_version.py \
  --binary target/release/rdrive \
  --app target/native-version-check/RDrive.app \
  --tag v0.8.1 \
  --output target/version-report.json
python3 -m unittest discover -s tests -p 'test_version.py' -v
```

The version checker only invokes `--version` and reads bundle metadata. It does not launch, register, or install the companion, and it does not access connection credentials. Omit `--tag` to validate a branch build; omit `--app` for a CLI-only check.

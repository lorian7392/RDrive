use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const CLI: &str = env!("CARGO_BIN_EXE_rdrive");
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASE: &str = "v100.0.0";
const NEW_CLI: &[u8] = b"#!/bin/sh\nprintf 'rdrive 100.0.0\\n'\n";

fn executable(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    programs: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let programs = root.path().join("programs");
        fs::create_dir(&programs).unwrap();
        executable(
            &programs.join("uname"),
            b"#!/bin/sh\ncase \"$1\" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n",
        );
        executable(
            &programs.join("gh"),
            br#"#!/bin/sh
set -eu
case "$1 $2" in
  "release view")
    case " $* " in
      *" --jq "*) printf '%s\n' "$RDRIVE_TEST_TAG" ;;
      *) printf '{"tagName":"%s","isDraft":false,"isPrerelease":false,"assets":[{"name":"rdrive-macos-arm64"},{"name":"SHA256SUMS"}]}\n' "$RDRIVE_TEST_TAG" ;;
    esac ;;
  "release download")
    while [ "$1" != "--dir" ]; do shift; done
    cp "$RDRIVE_TEST_RELEASE/rdrive-macos-arm64" "$RDRIVE_TEST_RELEASE/SHA256SUMS" "$2"
    ;;
  *) exit 71 ;;
esac
"#,
        );
        fs::write(root.path().join("rdrive-macos-arm64"), NEW_CLI).unwrap();
        fs::write(
            root.path().join("SHA256SUMS"),
            format!("{:x}  rdrive-macos-arm64\n", Sha256::digest(NEW_CLI)),
        )
        .unwrap();
        Self { root, programs }
    }

    fn command(&self, cli: impl AsRef<Path>, tag: &str) -> Command {
        let mut command = Command::new(cli.as_ref());
        command
            .env(
                "PATH",
                std::env::join_paths([
                    self.programs.as_path(),
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ])
                .unwrap(),
            )
            .env("RDRIVE_TEST_RELEASE", self.root.path())
            .env("RDRIVE_TEST_TAG", tag);
        command
    }

    fn update(&self, cli: impl AsRef<Path>, tag: &str, apply: bool) -> Output {
        let mut command = self.command(cli, tag);
        command
            .args(["--json", "--home"])
            .arg(self.root.path().join("unused state"))
            .arg("update");
        if apply {
            command.arg("--apply");
        }
        command.output().unwrap()
    }
}

fn report(result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[test]
fn standalone_update_is_explicit_checksum_verified_and_state_independent() {
    let fixture = Fixture::new();
    let bin = fixture.root.path().join("prefix with spaces/bin");
    fs::create_dir_all(&bin).unwrap();
    let cli = bin.join("rdrive");
    fs::copy(CLI, &cli).unwrap();
    let old_tag = "v0.0.0";
    let check = report(fixture.update(&cli, old_tag, true));
    assert_eq!(check["update_available"], false);
    assert_eq!(check["applied"], false);
    assert_eq!(check["current_version"], CURRENT);

    let check = report(fixture.update(&cli, RELEASE, false));
    assert_eq!(check["latest_version"], "100.0.0");
    assert_eq!(check["update_available"], true);
    assert_eq!(check["applied"], false);
    let original_checksum = fs::read(fixture.root.path().join("SHA256SUMS")).unwrap();
    fs::write(
        fixture.root.path().join("SHA256SUMS"),
        format!("{:064x}  rdrive-macos-arm64\n", 0),
    )
    .unwrap();
    let failure = fixture.update(&cli, RELEASE, true);
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("checksum mismatch"));
    assert_ne!(fs::read(&cli).unwrap(), NEW_CLI);
    fs::write(fixture.root.path().join("SHA256SUMS"), original_checksum).unwrap();

    let applied = report(fixture.update(&cli, RELEASE, true));
    assert_eq!(applied["applied"], true);
    assert_eq!(fs::read(&cli).unwrap(), NEW_CLI);
    assert!(!fixture.root.path().join("unused state").exists());
}

#[test]
fn update_refuses_an_embedded_cli() {
    let fixture = Fixture::new();
    let bin = fixture.root.path().join("RDrive.app/Contents/bin");
    fs::create_dir_all(&bin).unwrap();
    let cli = bin.join("rdrive");
    fs::copy(CLI, &cli).unwrap();
    let failure = fixture.update(&cli, RELEASE, true);
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("standalone CLI"));
    assert_ne!(fs::read(&cli).unwrap(), NEW_CLI);
}

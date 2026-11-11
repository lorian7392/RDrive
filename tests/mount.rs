#![cfg(target_os = "macos")]

use rdrive::{
    config::{Profile, Protocol},
    mount::{self, MountOptions},
};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

fn profile(protocol: Protocol) -> Profile {
    Profile {
        url: match protocol {
            Protocol::Webdav => "https://example.test/root/",
            Protocol::Smb => "smb://server/share/nested",
        }
        .to_owned(),
        protocol,
        username: Some("test user".to_owned()),
        password_env: Some("RDRIVE_MOUNT_TEST_PASSWORD".to_owned()),
        allow_http: false,
        timeout_secs: 10,
        smb_encryption: true,
        keychain_internet: false,
        nextcloud_upload_url: None,
    }
}

fn target() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("network volume");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    (temp, path)
}

#[test]
fn public_dry_run_has_no_side_effects_or_secrets() {
    let (_temp, path) = target();
    let before = mount::list().unwrap();
    let profile = profile(Protocol::Smb);
    let plan = mount::plan(
        &profile,
        &path,
        &MountOptions {
            read_only: true,
            no_browse: true,
            no_prompt: true,
        },
    )
    .unwrap();
    assert_eq!(plan.filesystem, "smbfs");
    assert!(plan.source.ends_with("/share/nested"));
    assert!(plan.source.contains("test%20user@"));
    assert!(plan.encryption_required);
    assert!(plan
        .args
        .iter()
        .any(|a| a.contains("sessionencrypt,shareencrypt")));
    assert_eq!(plan.args.last().unwrap(), path.to_str().unwrap());
    assert!(fs::read_dir(&path).unwrap().next().is_none());
    assert_eq!(before, mount::list().unwrap());
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("RDRIVE_MOUNT_TEST_PASSWORD"));
}

#[test]
fn public_validation_preserves_existing_data() {
    let (_temp, path) = target();
    fs::write(path.join("existing.txt"), "important data").unwrap();
    assert!(mount::plan(&profile(Protocol::Webdav), &path, &MountOptions::default()).is_err());
    assert_eq!(
        fs::read_to_string(path.join("existing.txt")).unwrap(),
        "important data"
    );
    assert!(mount::unmount(&path, false).is_err());
}

#[test]
fn native_smb_rejects_injected_password_argument() {
    let (_temp, path) = target();
    let error = mount::mount(
        &profile(Protocol::Smb),
        &path,
        &MountOptions {
            no_prompt: true,
            ..Default::default()
        },
        Some("never-show-this"),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot accept an rdrive password"));
    assert!(!error.to_string().contains("never-show-this"));
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Volume(PathBuf);
impl Drop for Volume {
    fn drop(&mut self) {
        let _ = mount::unmount(&self.0, false);
    }
}

/// An actual isolated mount, not a simulated helper exit. This deliberately fails
/// when the host denies native mount privileges; it never changes system sharing.
#[test]
#[ignore = "requires macOS native mount permission; mounts only an isolated loopback DAV fixture"]
fn native_webdav_readonly_roundtrip() {
    let (temp, path) = target();
    let fixture_root = temp.path().canonicalize().unwrap().join("server");
    fs::create_dir(&fixture_root).unwrap();
    fs::write(fixture_root.join("hello.txt"), "native mounted bytes\n").unwrap();
    let script = r#"
import http.server, importlib.util, pathlib, sys
spec = importlib.util.spec_from_file_location('fixture', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
class NativeDav(module.Dav):
    root = pathlib.Path(sys.argv[2])
    def reply(self, status, body=b'', **headers):
        if status == 401:
            headers['WWW_Authenticate'] = 'Basic realm="rdrive-native-fixture"'
        return super().reply(status, body, **headers)
    def do_OPTIONS(self):
        self.reply(200, DAV='1', Allow='OPTIONS,PROPFIND,GET,HEAD')
    def do_HEAD(self):
        resource = self.resource()
        if not resource.exists(): return self.reply(404)
        return self.reply(200, Content_Type='application/octet-stream')
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), NativeDav)
print(server.server_port, flush=True)
server.serve_forever()
"#;
    let script_path = temp.path().join("native_fixture.py");
    fs::write(&script_path, script).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cli_workflows.py");
    let mut server = Server(
        Command::new("python3")
            .arg(&script_path)
            .arg(fixture)
            .arg(&fixture_root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line
        .trim()
        .parse()
        .expect("fixture announced its loopback port");
    let mut profile = profile(Protocol::Webdav);
    profile.url = format!("http://127.0.0.1:{port}/dav/");
    profile.username = Some("user".to_owned());
    profile.allow_http = true;
    let result = mount::mount(
        &profile,
        &path,
        &MountOptions {
            read_only: true,
            no_browse: true,
            no_prompt: true,
        },
        Some("fixture-password"),
    );
    let mounted =
        result.expect("actual native WebDAV mount must succeed for this explicit integration test");
    let guard = Volume(path.clone());
    assert!(mounted.read_only);
    assert!(mount::list().unwrap().iter().any(|m| m.mountpoint == path));
    assert_eq!(
        fs::read_to_string(path.join("hello.txt")).unwrap(),
        "native mounted bytes\n"
    );
    assert!(fs::write(path.join("forbidden.txt"), "readonly").is_err());
    mount::unmount(&path, false).unwrap();
    std::mem::forget(guard);
    assert!(fs::read_dir(&path).unwrap().next().is_none());
    assert!(!mount::list().unwrap().iter().any(|m| m.mountpoint == path));
}

//! Discover stable CLI releases and apply them with the built-in installer.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const REPOSITORY: &str = "github.com/lorian7392/RDrive";
const INSTALLER: &str = include_str!("../scripts/install.sh");
const REQUIRED_ASSETS: [&str; 2] = ["rdrive-macos-arm64", "SHA256SUMS"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    tag_name: String,
    is_draft: bool,
    is_prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    current_version: String,
    latest_version: String,
    release_url: String,
    update_available: bool,
    applied: bool,
}

fn version(value: &str) -> Result<[u64; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    let [major, minor, patch] = parts.as_slice() else {
        bail!("expected a stable numeric release version");
    };
    let parse = |part: &str| {
        if !part.bytes().all(|b| b.is_ascii_digit()) || (part.len() > 1 && part.starts_with('0')) {
            bail!("expected a stable numeric release version");
        }
        part.parse::<u64>()
            .context("invalid release version number")
    };
    Ok([parse(major)?, parse(minor)?, parse(patch)?])
}

fn release_report(bytes: &[u8], current: &str) -> Result<Report> {
    let release: Release =
        serde_json::from_slice(bytes).context("invalid GitHub release metadata")?;
    if release.is_draft || release.is_prerelease {
        bail!("the GitHub release is not a published stable release");
    }
    let latest = release
        .tag_name
        .strip_prefix('v')
        .context("expected a vX.Y.Z release tag")?;
    let update_available = version(latest)? > version(current)?;
    for required in REQUIRED_ASSETS {
        if release
            .assets
            .iter()
            .filter(|asset| asset.name == required)
            .count()
            != 1
        {
            bail!(
                "release {} must contain exactly one {required} asset",
                release.tag_name
            );
        }
    }
    Ok(Report {
        current_version: current.into(),
        latest_version: latest.into(),
        release_url: format!("https://{REPOSITORY}/releases/tag/{}", release.tag_name),
        update_available,
        applied: false,
    })
}

fn install_prefix(executable: &Path) -> Result<&Path> {
    let bin = executable
        .parent()
        .context("cannot locate the CLI installation directory")?;
    if executable.file_name().is_none_or(|name| name != "rdrive")
        || bin.file_name().is_none_or(|name| name != "bin")
        || !executable.is_absolute()
        || executable.ancestors().any(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
    {
        bail!("install the standalone CLI before updating; update the Finder companion separately");
    }
    bin.parent()
        .context("cannot locate the CLI installation prefix")
}

async fn run_command(command: &mut Command, deadline: Duration, operation: &str) -> Result<Output> {
    let child = command
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .with_context(|| format!("could not start {operation}"))?;
    let group = child.id().context("update subprocess has no process ID")? as libc::pid_t;
    let output = match timeout(deadline, child.wait_with_output()).await {
        Ok(result) => result.with_context(|| format!("could not read {operation} output"))?,
        Err(_) => {
            // Cancel descendants too. The installer can still be waiting for gh.
            unsafe { libc::kill(-group, libc::SIGTERM) };
            bail!("{operation} timed out");
        }
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!("{operation} failed ({}): {}", output.status, detail.trim());
    }
    Ok(output)
}

pub async fn run(apply: bool) -> Result<Report> {
    update(
        apply,
        "gh".as_ref(),
        std::env::current_exe,
        env!("CARGO_PKG_VERSION"),
    )
    .await
}

async fn update(
    apply: bool,
    gh: &std::ffi::OsStr,
    current_exe: impl FnOnce() -> std::io::Result<std::path::PathBuf>,
    current: &str,
) -> Result<Report> {
    let output = run_command(
        Command::new(gh).args([
            "release",
            "view",
            "--repo",
            REPOSITORY,
            "--json",
            "tagName,isDraft,isPrerelease,assets",
        ]),
        Duration::from_secs(30),
        "GitHub release check (requires gh and access to the RDrive repository)",
    )
    .await?;
    let mut report = release_report(&output.stdout, current)?;
    if apply && report.update_available {
        let executable = current_exe().context("cannot locate the running CLI")?;
        let prefix = install_prefix(&executable)?;
        run_command(
            Command::new("/bin/bash")
                .args(["-c", INSTALLER, "rdrive-install", "--prefix"])
                .arg(prefix)
                .args([
                    "--version",
                    &format!("v{}", report.latest_version),
                    "--replace-version",
                    &format!("v{}", report.current_version),
                ]),
            Duration::from_secs(600),
            "RDrive installation",
        )
        .await?;
        report.applied = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_considers_complete_stable_releases_and_compares_components() {
        let mut release = json!({
            "tagName": "v0.10.1",
            "isDraft": false,
            "isPrerelease": false,
            "assets": [{"name": REQUIRED_ASSETS[0]}, {"name": REQUIRED_ASSETS[1]}]
        });
        let report = |release: &serde_json::Value, current| {
            release_report(&serde_json::to_vec(release).unwrap(), current)
        };
        assert!(report(&release, "0.9.99").unwrap().update_available);
        assert!(!report(&release, "0.10.1").unwrap().update_available);
        assert!(!report(&release, "1.0.0").unwrap().update_available);
        for flag in ["isDraft", "isPrerelease"] {
            release[flag] = json!(true);
            assert!(report(&release, "0.0.0").is_err());
            release[flag] = json!(false);
        }
        for tag in [
            "0.10.1",
            "v0.10",
            "v0.010.1",
            "v0.10.1-rc",
            "v99999999999999999999.0.0",
        ] {
            release["tagName"] = json!(tag);
            assert!(report(&release, "0.0.0").is_err(), "{tag}");
        }
        release["tagName"] = json!("v0.10.1");
        assert!(report(&release, "not a version").is_err());
        release["assets"] = json!([{"name": REQUIRED_ASSETS[0]}, {"name": REQUIRED_ASSETS[0]}]);
        assert!(report(&release, "0.0.0").is_err());
        assert!(release_report(b"{", "0.0.0").is_err());
    }

    #[test]
    fn resolves_only_standalone_cli_locations() {
        assert_eq!(
            install_prefix(Path::new("/opt/example/bin/rdrive")).unwrap(),
            Path::new("/opt/example")
        );
        for path in [
            "/",
            "/opt/example/bin/custom",
            "/source/target/release/rdrive",
            "bin/rdrive",
            "/RDrive.APP/Contents/bin/rdrive",
        ] {
            assert!(install_prefix(Path::new(path)).is_err(), "{path}");
        }
    }

    #[tokio::test]
    async fn subprocess_failures_are_bounded_and_reported() {
        let mut missing = Command::new("/missing-rdrive-test-command");
        assert!(run_command(&mut missing, Duration::from_secs(1), "fixture")
            .await
            .unwrap_err()
            .to_string()
            .contains("could not start"));
        let mut failure = Command::new("/bin/sh");
        failure.args(["-c", "echo fixture-error >&2; exit 7"]);
        assert!(run_command(&mut failure, Duration::from_secs(2), "fixture")
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture-error"));
        let mut stuck = Command::new("/bin/sh");
        stuck.args(["-c", "sleep 30 & wait"]);
        assert!(
            run_command(&mut stuck, Duration::from_millis(50), "fixture")
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }
}

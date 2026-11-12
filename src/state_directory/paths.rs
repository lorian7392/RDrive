//! Rebind the few local pointers whose meaning changes with a state home.
//! Mutation identities, remote names, SQLite catalogs and user roots are never
//! subject to a recursive string replacement.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

const MARKER: &str = ".rdrive-cache.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RebaseReport {
    /// Paths relative to the staged state whose contents were intentionally changed.
    pub changed_files: Vec<PathBuf>,
    /// Journal these before applying them. They belong to a custom external cache.
    pub external_markers: Vec<MarkerChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarkerChange {
    pub path: PathBuf,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheMarker {
    version: u32,
    home: String,
    device: u64,
    inode: u64,
}

fn directory(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("relocation metadata directories must be private, owned real directories");
    }
    Ok(metadata)
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let before = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o077 != 0
        || before.nlink() != 1
    {
        bail!("relocation metadata must be private, owned regular files without links");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let opened = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let after = fs::symlink_metadata(path)?;
    let stamp = |value: &fs::Metadata| {
        (
            value.dev(),
            value.ino(),
            value.len(),
            value.mtime(),
            value.mtime_nsec(),
            value.uid(),
            value.mode(),
            value.nlink(),
        )
    };
    if bytes.len() > 64 * 1024 * 1024
        || stamp(&before) != stamp(&opened)
        || stamp(&before) != stamp(&after)
    {
        bail!("relocation metadata changed during inspection or exceeds 64 MiB");
    }
    Ok(Some(bytes))
}

fn pointer(value: &Value, field: &str) -> Result<Option<PathBuf>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path))
            if Path::new(path).is_absolute()
                && !Path::new(path)
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir)) =>
        {
            Ok(Some(path.into()))
        }
        _ => bail!("invalid absolute local pointer in relocation metadata: {field}"),
    }
}

fn entries(path: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            directory(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    Ok(fs::read_dir(path)?
        .map(|item| item.map(|item| item.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .collect())
}

fn preserve_user_root(path: &Path, fields: &[&str], source: &Path) -> Result<()> {
    let bytes = read(path)?.context("relocation metadata disappeared")?;
    let value: Value = serde_json::from_slice(&bytes)?;
    for field in fields {
        if pointer(&value, field)?.is_some_and(|path| path.starts_with(source)) {
            bail!("a user sync root or download destination is inside the state directory; relocate or finish that operation first");
        }
    }
    Ok(())
}

fn marker(bytes: &[u8], home: &Path, metadata: &fs::Metadata) -> Result<CacheMarker> {
    let value: CacheMarker = serde_json::from_slice(bytes)?;
    if value.version != 1
        || value.home != crate::state::key(&home.to_string_lossy())
        || value.device != metadata.dev()
        || value.inode != metadata.ino()
    {
        bail!("cache ownership differs from the source home; cache and state were preserved");
    }
    Ok(value)
}

/// Run only on a verified copy while the source writer and cache configuration
/// are locked. External cache marker writes are planned, never performed here.
pub fn rebase_tree(staged: &Path, source: &Path, destination: &Path) -> Result<RebaseReport> {
    if [staged, source, destination].iter().any(|path| {
        !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    }) || staged.starts_with(source)
        || source.starts_with(staged)
        || source == destination
    {
        bail!("relocation rebasing needs distinct absolute state paths without traversal");
    }
    directory(staged)?;
    if fs::symlink_metadata(staged.join("cache-directory-migration.json")).is_ok() {
        bail!("finish the retained cache-directory migration before relocating state");
    }
    let config_path = staged.join("config.json");
    let mut config: Value =
        serde_json::from_slice(&read(&config_path)?.context("copied configuration is missing")?)?;
    let _: crate::config::Config = serde_json::from_value(config.clone())?;
    let configured = config
        .get("cache")
        .and_then(|value| value.get("directory"))
        .cloned()
        .unwrap_or(Value::Null);
    let selected = pointer(&serde_json::json!({"directory":configured}), "directory")?;
    let old_cache = selected.clone().unwrap_or_else(|| source.join("cache"));
    if old_cache.starts_with(source) && old_cache != source.join("cache") {
        bail!(
            "custom cache overlaps the state directory; preserve and correct it before relocation"
        );
    }
    if let Some(bytes) = read(&staged.join("provider-domains.json"))? {
        let domains: Vec<crate::provider::Domain> = serde_json::from_slice(&bytes)?;
        if domains.iter().any(|domain| {
            domain
                .local_root
                .as_ref()
                .is_some_and(|path| !path.is_absolute() || path.starts_with(source))
        }) {
            bail!("a Finder synchronization root is inside the state directory; preserve and relocate that root first");
        }
    }
    for path in entries(&staged.join("baselines"), ".json")? {
        preserve_user_root(&path, &["local_root"], source)?;
    }
    for path in entries(&staged.join("transfers"), ".json")? {
        preserve_user_root(&path, &["destination", "directory"], source)?;
    }
    let mut report = RebaseReport::default();
    for path in entries(&staged.join("spool"), ".upload.json")? {
        let mut value: Value =
            serde_json::from_slice(&read(&path)?.context("upload checkpoint disappeared")?)?;
        if let Some(old) = pointer(&value, "source_path")? {
            if let Ok(relative) = old.strip_prefix(source) {
                value["source_path"] = serde_json::to_value(destination.join(relative))?;
                crate::state::atomic_json(&path, &value)?;
                report.changed_files.push(path.strip_prefix(staged)?.into());
            }
        }
    }
    if old_cache == source.join("cache") {
        let copied_cache = staged.join("cache");
        if let Some(bytes) = read(&copied_cache.join(MARKER))? {
            marker(&bytes, source, &directory(&old_cache)?)?;
            let metadata = directory(&copied_cache)?;
            crate::state::atomic_json(
                &copied_cache.join(MARKER),
                &CacheMarker {
                    version: 1,
                    home: crate::state::key(&destination.to_string_lossy()),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            )?;
            report
                .changed_files
                .push(PathBuf::from("cache").join(MARKER));
        } else if selected.is_some() {
            bail!("configured cache ownership marker is missing");
        }
        if selected.is_some() {
            config["cache"]["directory"] = serde_json::to_value(destination.join("cache"))?;
            crate::state::atomic_json(&config_path, &config)?;
            report.changed_files.push("config.json".into());
        }
    } else {
        let metadata = directory(&old_cache)?;
        let path = old_cache.join(MARKER);
        let before = read(&path)?.context("external cache ownership marker is missing")?;
        let mut after = marker(&before, source, &metadata)?;
        after.home = crate::state::key(&destination.to_string_lossy());
        report.external_markers.push(MarkerChange {
            path,
            before,
            after: serde_json::to_vec_pretty(&after)?,
        });
    }
    Ok(report)
}

/// Compare-and-swap the sole ownership record under the source administration
/// lock. The journal makes an acknowledged or uncertain write reversible.
pub fn apply_external_markers(report: &RebaseReport, forward: bool) -> Result<()> {
    for change in &report.external_markers {
        let directory_path = change
            .path
            .parent()
            .context("cache marker requires a parent")?;
        let metadata = directory(directory_path)?;
        let before: CacheMarker = serde_json::from_slice(&change.before)?;
        let after: CacheMarker = serde_json::from_slice(&change.after)?;
        let valid_hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if change.path.file_name() != Some(std::ffi::OsStr::new(MARKER))
            || before.version != 1
            || after.version != 1
            || !valid_hash(&before.home)
            || !valid_hash(&after.home)
            || (before.device, before.inode) != (metadata.dev(), metadata.ino())
            || (after.device, after.inode) != (metadata.dev(), metadata.ino())
        {
            bail!("invalid external cache ownership transition; no file was overwritten");
        }
        let (expected, desired) = if forward {
            (&change.before, &change.after)
        } else {
            (&change.after, &change.before)
        };
        let current = read(&change.path)?.context("external cache ownership marker disappeared")?;
        if &current == desired {
            continue;
        }
        if &current != expected {
            bail!("external cache ownership changed concurrently; no file was overwritten");
        }
        crate::state::atomic_write(&change.path, desired)?;
    }
    Ok(())
}

/// Bind deserialized recovery instructions to the migration's two homes. Only
/// the specific pointer records below may differ from a verified state copy.
pub fn validate_report(report: &RebaseReport, source: &Path, destination: &Path) -> Result<()> {
    let mut unique = std::collections::BTreeSet::new();
    for path in &report.changed_files {
        let parts: Vec<_> = path.components().collect();
        let upload = parts.len() == 2
            && parts[0] == Component::Normal(std::ffi::OsStr::new("spool"))
            && path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .strip_suffix(".upload.json")
                    .is_some_and(|prefix| !prefix.is_empty())
            });
        if path.is_absolute()
            || parts
                .iter()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !unique.insert(path)
            || !(path == Path::new("config.json")
                || path == &Path::new("cache").join(MARKER)
                || upload)
        {
            bail!("invalid state pointer record in migration recovery report");
        }
    }
    let mut markers = std::collections::BTreeSet::new();
    for change in &report.external_markers {
        let before: CacheMarker = serde_json::from_slice(&change.before)?;
        let after: CacheMarker = serde_json::from_slice(&change.after)?;
        if !change.path.is_absolute()
            || change
                .path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || change.path.file_name() != Some(std::ffi::OsStr::new(MARKER))
            || change.path.starts_with(source)
            || change.path.starts_with(destination)
            || !markers.insert(&change.path)
            || before.version != 1
            || after.version != 1
            || before.home != crate::state::key(&source.to_string_lossy())
            || after.home != crate::state::key(&destination.to_string_lossy())
            || (before.device, before.inode) != (after.device, after.inode)
        {
            bail!("external cache recovery instructions belong to different state homes");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn private(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn write(path: &Path, value: Value) {
        private(path.parent().unwrap());
        crate::state::atomic_json(path, &value).unwrap();
    }
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("old state");
        let staged = root.join("staged");
        let destination = root.join("new state");
        private(&source);
        private(&staged);
        private(&source.join("cache"));
        private(&staged.join("cache"));
        write(&staged.join("config.json"), json!({"profiles":{}}));
        (temporary, source, staged, destination)
    }
    fn cache_marker(home: &Path, cache: &Path) -> Value {
        let m = fs::metadata(cache).unwrap();
        json!({"version":1,"home":crate::state::key(&home.to_string_lossy()),"device":m.dev(),"inode":m.ino()})
    }

    #[test]
    fn byte_preserves_immutable_journals_indexes_receipts_and_external_user_paths() {
        let (_temporary, source, staged, destination) = setup();
        let external = source.parent().unwrap().join("external destination");
        write(
            &staged.join("baselines/sync.json"),
            json!({"version":1,"local_root":external,"remote_root":source,"entries":{}}),
        );
        write(
            &staged.join("transfers/download.json"),
            json!({"id":"immutable","destination":external,"directory":external.parent().unwrap().join(".rdrive-partial-uuid"),"bytes":3,"sha256":"guard"}),
        );
        write(
            &staged.join("provider-domains.json"),
            json!([{"id":"domain","profile":"p","remote":"remote","display_name":"NAS","smart":true,"local_root":external}]),
        );
        write(
            &staged.join("smart/domain/queue.json"),
            json!({"mutations":{"id":{"request":{"name":source,"base_content_version":"guard"},"fingerprint":"immutable","snapshot":{"file":"id.data"}}}}),
        );
        private(&staged.join("provider"));
        fs::write(
            staged.join("provider/index.sqlite3"),
            b"opaque authoritative SQLite bytes",
        )
        .unwrap();
        write(
            &staged.join("spool/smb.upload.json"),
            json!({"source_sha256":"immutable","stage":"remote-name","remote":source}),
        );
        write(
            &staged.join("spool/external.upload.json"),
            json!({"source_path":external,"remote":source,"chunks":{"1":"immutable"}}),
        );
        write(
            &staged.join("spool/ignored.json"),
            json!({"source_path":source.join("do-not-touch")}),
        );
        let paths = [
            "config.json",
            "baselines/sync.json",
            "transfers/download.json",
            "provider-domains.json",
            "smart/domain/queue.json",
            "provider/index.sqlite3",
            "spool/smb.upload.json",
            "spool/external.upload.json",
            "spool/ignored.json",
        ];
        let before: Vec<_> = paths
            .iter()
            .map(|path| fs::read(staged.join(path)).unwrap())
            .collect();
        let report = rebase_tree(&staged, &source, &destination).unwrap();
        validate_report(&report, &source, &destination).unwrap();
        assert!(report.changed_files.is_empty() && report.external_markers.is_empty());
        for (path, bytes) in paths.into_iter().zip(before) {
            assert_eq!(fs::read(staged.join(path)).unwrap(), bytes);
        }
        apply_external_markers(&report, true).unwrap();
    }

    #[test]
    fn rebases_nextcloud_snapshot_pointer_without_changing_session_guards_or_remote_text() {
        let (_temporary, source, staged, destination) = setup();
        let mut checkpoint = json!({"source_path":source.join("spool/job"),"session":"immutable","remote":source,"chunks":{"1":{"sha256":"guard"}},"guard":{"etag":"original"}});
        let file = staged.join("spool/job.upload.json");
        write(&file, checkpoint.clone());
        let report = rebase_tree(&staged, &source, &destination).unwrap();
        assert_eq!(
            report.changed_files,
            [PathBuf::from("spool/job.upload.json")]
        );
        checkpoint["source_path"] = json!(destination.join("spool/job"));
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(file).unwrap()).unwrap(),
            checkpoint
        );
    }

    #[test]
    fn rebinds_optional_default_marker_and_explicit_default_config_to_copied_inode() {
        let (_temporary, source, staged, destination) = setup();
        let old = cache_marker(&source, &source.join("cache"));
        write(&staged.join("cache").join(MARKER), old.clone());
        let result = rebase_tree(&staged, &source, &destination).unwrap();
        assert_eq!(result.changed_files, [PathBuf::from("cache").join(MARKER)]);
        let actual: CacheMarker =
            serde_json::from_slice(&fs::read(staged.join("cache").join(MARKER)).unwrap()).unwrap();
        assert_eq!(
            actual.home,
            crate::state::key(&destination.to_string_lossy())
        );
        assert_eq!(
            actual.inode,
            fs::metadata(staged.join("cache")).unwrap().ino()
        );
        write(&staged.join("cache").join(MARKER), old.clone());
        let retained = fs::read(staged.join("cache").join(MARKER)).unwrap();
        fs::set_permissions(staged.join("cache"), fs::Permissions::from_mode(0o500)).unwrap();
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        assert_eq!(
            fs::read(staged.join("cache").join(MARKER)).unwrap(),
            retained
        );
        fs::set_permissions(staged.join("cache"), fs::Permissions::from_mode(0o700)).unwrap();
        write(&staged.join("cache").join(MARKER), old);
        write(
            &staged.join("config.json"),
            json!({"profiles":{},"cache":{"directory":source.join("cache"),"max_bytes":512},"unknown":{"remote":source}}),
        );
        let result = rebase_tree(&staged, &source, &destination).unwrap();
        assert!(result.changed_files.contains(&PathBuf::from("config.json")));
        let config: Value =
            serde_json::from_slice(&fs::read(staged.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            config["cache"]["directory"],
            json!(destination.join("cache"))
        );
        assert_eq!(config["unknown"]["remote"], json!(source));
    }

    #[test]
    fn external_cache_rebinding_is_planned_idempotent_reversible_and_rejects_concurrent_changes() {
        let (_temporary, source, staged, destination) = setup();
        let external = source.parent().unwrap().join("external cache");
        private(&external);
        let old = cache_marker(&source, &external);
        write(&external.join(MARKER), old);
        let original = fs::read(external.join(MARKER)).unwrap();
        write(
            &staged.join("config.json"),
            json!({"profiles":{},"cache":{"directory":external}}),
        );
        let config = fs::read(staged.join("config.json")).unwrap();
        let report = rebase_tree(&staged, &source, &destination).unwrap();
        assert!(report.changed_files.is_empty());
        assert_eq!(report.external_markers.len(), 1);
        validate_report(&report, &source, &destination).unwrap();
        assert!(validate_report(&report, &destination, &source).is_err());
        let mut invalid = report.clone();
        invalid.changed_files = vec!["../config.json".into()];
        assert!(validate_report(&invalid, &source, &destination).is_err());
        let mut invalid = report.clone();
        invalid.changed_files = vec!["config.json".into(), "config.json".into()];
        assert!(validate_report(&invalid, &source, &destination).is_err());
        let mut valid = report.clone();
        valid.changed_files = vec![
            "config.json".into(),
            "cache/.rdrive-cache.json".into(),
            "spool/job.upload.json".into(),
        ];
        validate_report(&valid, &source, &destination).unwrap();
        assert_eq!(fs::read(external.join(MARKER)).unwrap(), original);
        assert_eq!(fs::read(staged.join("config.json")).unwrap(), config);
        apply_external_markers(&report, true).unwrap();
        apply_external_markers(&report, true).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(external.join(MARKER)).unwrap()).unwrap()
                ["home"],
            json!(crate::state::key(&destination.to_string_lossy()))
        );
        apply_external_markers(&report, false).unwrap();
        apply_external_markers(&report, false).unwrap();
        assert_eq!(fs::read(external.join(MARKER)).unwrap(), original);
        write(
            &external.join(MARKER),
            cache_marker(&destination, &external),
        );
        assert!(apply_external_markers(&report, true).is_err());
        fs::remove_file(external.join(MARKER)).unwrap();
        assert!(apply_external_markers(&report, true).is_err());
    }

    #[test]
    fn unsafe_or_incomplete_local_operations_fail_before_any_copy_is_rebased() {
        let (_temporary, source, staged, destination) = setup();
        assert!(rebase_tree(&staged, &source, &source).is_err());
        assert!(rebase_tree(&source, &source, &destination).is_err());
        assert!(rebase_tree(Path::new("relative"), &source, &destination).is_err());
        assert!(rebase_tree(&staged, &source, &destination.join("../other")).is_err());
        let intent = staged.join("cache-directory-migration.json");
        write(&intent, json!({}));
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        fs::remove_file(intent).unwrap();
        write(
            &staged.join("config.json"),
            json!({"profiles":{},"cache":{"directory":source.join("spool")}}),
        );
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        write(&staged.join("config.json"), json!({"profiles":{}}));
        write(
            &staged.join("provider-domains.json"),
            json!([{"id":"domain","profile":"p","remote":"","display_name":"NAS","local_root":source.join("view")} ]),
        );
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        fs::remove_file(staged.join("provider-domains.json")).unwrap();
        for (relative, value) in [
            (
                "baselines/local.json",
                json!({"local_root":source.join("work")}),
            ),
            (
                "transfers/partial.json",
                json!({"destination":source.join("out")}),
            ),
        ] {
            write(&staged.join(relative), value);
            assert!(rebase_tree(&staged, &source, &destination).is_err());
            fs::remove_file(staged.join(relative)).unwrap();
        }
        write(
            &staged.join("config.json"),
            json!({"profiles":{},"cache":{"directory":source.join("cache")}}),
        );
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        write(
            &staged.join("cache").join(MARKER),
            cache_marker(&destination, &source.join("cache")),
        );
        assert!(rebase_tree(&staged, &source, &destination).is_err());
    }

    #[test]
    fn validates_private_metadata_types_bounded_reads_and_pointer_types() {
        let (_temporary, source, staged, destination) = setup();
        assert!(pointer(&json!({"p":"relative"}), "p").is_err());
        assert!(pointer(&json!({"p":source.join("../outside")}), "p").is_err());
        assert!(pointer(&json!({"p":4}), "p").is_err());
        assert_eq!(pointer(&json!({"p":null}), "p").unwrap(), None);
        let path = staged.join("private.json");
        write(&path, json!({}));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = staged.join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(read(&path).is_err());
        fs::remove_file(alias).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(staged.join("config.json"), &path).unwrap();
        assert!(read(&path).is_err());
        fs::remove_file(&path).unwrap();
        private(&path);
        assert!(read(&path).is_err());
        fs::remove_dir(&path).unwrap();
        write(&path, json!({}));
        assert!(read(&path.join("child")).is_err());
        assert!(entries(&path.join("child"), ".json").is_err());
        let link = staged.join("linked-dir");
        symlink(staged.join("cache"), &link).unwrap();
        assert!(directory(&link).is_err());
        fs::set_permissions(staged.join("cache"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(directory(&staged.join("cache")).is_err());
        fs::set_permissions(staged.join("cache"), fs::Permissions::from_mode(0o700)).unwrap();
        let big = OpenOptions::new().write(true).open(&path).unwrap();
        big.set_len(64 * 1024 * 1024 + 1).unwrap();
        assert!(read(&path).is_err());
        fs::remove_file(staged.join("config.json")).unwrap();
        assert!(rebase_tree(&staged, &source, &destination).is_err());
    }

    #[test]
    fn rejects_forged_external_marker_transitions_and_missing_ownership() {
        let (_temporary, source, staged, destination) = setup();
        let external = source.parent().unwrap().join("external");
        private(&external);
        write(
            &staged.join("config.json"),
            json!({"profiles":{},"cache":{"directory":external}}),
        );
        assert!(rebase_tree(&staged, &source, &destination).is_err());
        write(&external.join(MARKER), cache_marker(&source, &external));
        let report = rebase_tree(&staged, &source, &destination).unwrap();
        let mut forged = report.clone();
        let mut after: Value = serde_json::from_slice(&forged.external_markers[0].after).unwrap();
        after["inode"] = json!(0);
        forged.external_markers[0].after = serde_json::to_vec(&after).unwrap();
        assert!(apply_external_markers(&forged, true).is_err());
        let mut forged = report.clone();
        forged.external_markers[0].path = external.join("unrelated.json");
        assert!(apply_external_markers(&forged, true).is_err());
        let mut forged = report.clone();
        forged.external_markers[0].before = b"bad".to_vec();
        assert!(apply_external_markers(&forged, true).is_err());
        let mut forged = report;
        forged.external_markers[0].after = b"bad".to_vec();
        assert!(apply_external_markers(&forged, true).is_err());
        let path = staged.join("spool/bad.upload.json");
        write(&path, json!({"source_path":false}));
        assert!(rebase_tree(&staged, &source, &destination).is_err());
    }
}

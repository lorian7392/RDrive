//! Opt-in per-user background service. No credentials are written to launchd
//! property lists, service configuration, runtime status, or diagnostic logs.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceOptions {
    pub home: PathBuf,
    pub executable: PathBuf,
    pub ipc_root: PathBuf,
    pub app: Option<PathBuf>,
    pub development_tcp: bool,
    pub launch_agents_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Uninstall,
}

pub fn install(options: &ServiceOptions, dry_run: bool) -> Result<Value> {
    ensure_macos()?;
    #[cfg(unix)]
    {
        native::install(options, dry_run, &native::SystemLaunchctl)
    }
    #[cfg(not(unix))]
    {
        unreachable!()
    }
}
pub fn action(home: &Path, action: ServiceAction, dry_run: bool) -> Result<Value> {
    ensure_macos()?;
    #[cfg(unix)]
    {
        native::action(home, action, dry_run, &native::SystemLaunchctl)
    }
    #[cfg(not(unix))]
    {
        unreachable!()
    }
}
pub fn status(home: &Path) -> Result<Value> {
    ensure_macos()?;
    #[cfg(unix)]
    {
        native::status(home, &native::SystemLaunchctl)
    }
    #[cfg(not(unix))]
    {
        unreachable!()
    }
}
pub async fn run(home: &Path) -> Result<()> {
    ensure_macos()?;
    #[cfg(unix)]
    {
        native::run(home).await
    }
    #[cfg(not(unix))]
    {
        unreachable!()
    }
}
fn ensure_macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("resident services currently require macOS launchd");
    }
    Ok(())
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::{config, state};
    use anyhow::Context;
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
            process::ExitStatusExt,
        },
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    const FORMAT: u32 = 1;
    const LOG_LIMIT: u64 = 1024 * 1024;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct Installation {
        format: u32,
        label: String,
        options: ServiceOptions,
        plist: PathBuf,
    }

    pub(super) struct SystemLaunchctl;
    #[derive(Default)]
    pub(super) struct CommandOutput {
        pub code: i32,
        pub stdout: String,
    }
    pub(super) trait Launchctl {
        fn invoke(&self, args: &[String]) -> Result<CommandOutput>;
    }
    impl Launchctl for SystemLaunchctl {
        fn invoke(&self, args: &[String]) -> Result<CommandOutput> {
            let output = Command::new("/bin/launchctl")
                .args(args)
                .stdin(Stdio::null())
                .output()
                .context("cannot execute launchctl")?;
            // launchctl print can contain an environment section. Never expose,
            // log, or persist its unfiltered output or use it for credentials.
            Ok(CommandOutput {
                code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        }
    }
    fn uid() -> u32 {
        unsafe { libc::geteuid() }
    }
    fn exists(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    fn absolute(path: &Path) -> Result<PathBuf> {
        if path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("service paths cannot contain parent traversal");
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        if path
            .to_str()
            .is_none_or(|value| value.chars().any(|c| c.is_control()))
        {
            bail!("service paths must be UTF-8 without control characters");
        }
        if exists(&path)? {
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                bail!("service paths cannot be symbolic links");
            }
            return Ok(path.canonicalize()?);
        }
        let parent = absolute(path.parent().context("service path needs a parent")?)?;
        Ok(parent.join(path.file_name().context("service path needs a filename")?))
    }
    fn directory(path: &Path, create: bool, private: bool) -> Result<PathBuf> {
        let path = absolute(path)?;
        if !exists(&path)? {
            if !create {
                return Ok(path);
            }
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)?;
        }
        let metadata = fs::symlink_metadata(&path)?;
        let forbidden = if private { 0o077 } else { 0o022 };
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid()
            || metadata.mode() & forbidden != 0
        {
            bail!("service directories must be owned, real directories with safe permissions");
        }
        Ok(path)
    }
    fn private_file(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            bail!("service files must be private owned regular files without links");
        }
        Ok(())
    }
    fn executable(path: &Path) -> Result<PathBuf> {
        let path = absolute(path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid()
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            bail!("service executable must be an owned executable file without writable group/other permissions");
        }
        Ok(path)
    }
    fn read_bytes(path: &Path) -> Result<Vec<u8>> {
        private_file(path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if file.metadata()?.len() > 1024 * 1024 {
            bail!("service control file is too large");
        }
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
        Ok(serde_json::from_slice(&read_bytes(path)?)
            .context("invalid service control file; existing state was preserved")?)
    }
    fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
        if exists(path)? {
            private_file(path)?;
        }
        state::atomic_json(path, value)
    }
    fn service_dir(home: &Path, create: bool) -> Result<PathBuf> {
        directory(&directory(home, false, true)?.join("service"), create, true)
    }
    fn install_path(home: &Path) -> Result<PathBuf> {
        Ok(service_dir(home, false)?.join("installation.json"))
    }
    fn label(home: &Path) -> String {
        format!(
            "io.rdrive.service.{}.{}",
            uid(),
            &state::key(&home.to_string_lossy())[..20]
        )
    }
    fn target(installation: &Installation) -> String {
        format!("gui/{}/{}", uid(), installation.label)
    }
    fn lock(path: &Path, blocking: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        private_file(path)?;
        if blocking {
            file.lock()?;
        } else {
            file.try_lock().map_err(|_| {
                anyhow::anyhow!("an RDrive supervisor is already running for this state directory")
            })?;
        }
        Ok(file)
    }
    fn native_property(app: &Path, key: &str) -> Result<Option<String>> {
        let plist = app.join("Contents/Info.plist");
        let metadata = fs::symlink_metadata(&plist)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid()
            || metadata.mode() & 0o022 != 0
        {
            bail!("native app metadata must be an owned regular file");
        }
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist)
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(output.stdout)?.trim().into()))
    }
    fn normalized(options: &ServiceOptions, verify_artifacts: bool) -> Result<ServiceOptions> {
        let home = directory(&options.home, false, true)?;
        if !exists(&home)? {
            bail!("initialize a private RDrive state directory before installing its service");
        }
        // Management must remain possible after an upgrade moves an executable
        // or removes the companion app. Only operations that execute artifacts
        // require their executable permissions and signatures to remain valid.
        let binary = if verify_artifacts {
            executable(&options.executable)?
        } else {
            absolute(&options.executable)?
        };
        let ipc_root = directory(&options.ipc_root, false, true)?;
        if ipc_root.join("bridge.sock").as_os_str().len() >= 104 {
            bail!("service IPC path is too long for a Unix socket; choose a shorter --ipc-root");
        }
        let app=options.app.as_ref().map(|path|->Result<PathBuf>{
            let app=directory(path,false,false)?;
            if verify_artifacts {
                let _=executable(&app.join("Contents/MacOS/RDriveCompanion"))?;
                if native_property(&app,"CFBundleIdentifier")?.as_deref()!=Some("io.rdrive.native"){bail!("the native watcher must come from the RDrive companion app");}
                if !Command::new("/usr/bin/codesign").args(["--verify","--deep","--strict"]).arg(&app).output()?.status.success(){bail!("the RDrive companion app failed code-signature verification");}
                let development=native_property(&app,"RDriveDevelopmentIPCRoot")?;
                if options.development_tcp {
                    let root=development.context("--development-tcp requires a matching development native app")?;
                    if absolute(Path::new(&root))?!=ipc_root{bail!("service IPC root must match the native app's embedded development IPC root");}
                }else if development.is_some(){bail!("the selected development app requires explicit --development-tcp");}
            }
            Ok(app)
        }).transpose()?;
        let launch_agents_dir = if let Some(path) = &options.launch_agents_dir {
            Some(directory(path, false, true)?)
        } else {
            None
        };
        Ok(ServiceOptions {
            home,
            executable: binary,
            ipc_root,
            app,
            development_tcp: options.development_tcp,
            launch_agents_dir,
        })
    }
    fn planned(options: &ServiceOptions, verify_artifacts: bool) -> Result<Installation> {
        let options = normalized(options, verify_artifacts)?;
        let agents = if let Some(path) = &options.launch_agents_dir {
            path.clone()
        } else {
            let user = std::env::var_os("HOME")
                .context("HOME is unavailable for the per-user LaunchAgent location")?;
            directory(
                &PathBuf::from(user).join("Library/LaunchAgents"),
                false,
                false,
            )?
        };
        let label = label(&options.home);
        let plist = agents.join(format!("{label}.plist"));
        Ok(Installation {
            format: FORMAT,
            label,
            options,
            plist,
        })
    }
    fn load(home: &Path) -> Result<Installation> {
        let home = directory(home, false, true)?;
        let installation: Installation = read(&install_path(&home)?)?;
        if installation.format != FORMAT
            || installation.options.home != home
            || installation.label != label(&home)
        {
            bail!("service installation identity changed; existing state was preserved");
        }
        let expected = planned(&installation.options, false)?;
        if installation != expected {
            bail!("service installation paths changed; existing state was preserved");
        }
        Ok(installation)
    }
    fn ready(installation: &Installation) -> Result<()> {
        if planned(&installation.options, true)? != *installation {
            bail!("service installation paths changed; existing state was preserved");
        }
        Ok(())
    }
    fn xml(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
    fn arguments(installation: &Installation) -> Vec<String> {
        vec![
            installation.options.executable.display().to_string(),
            "--home".into(),
            installation.options.home.display().to_string(),
            "--progress".into(),
            "off".into(),
            "service".into(),
            "run".into(),
        ]
    }
    fn plist(installation: &Installation) -> String {
        let arguments = arguments(installation)
            .iter()
            .map(|value| format!("<string>{}</string>", xml(value)))
            .collect::<String>();
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{}</array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer><key>ExitTimeOut</key><integer>15</integer><key>Umask</key><integer>63</integer><key>ProcessType</key><string>Background</string><key>StandardOutPath</key><string>/dev/null</string><key>StandardErrorPath</key><string>/dev/null</string></dict></plist>\n",xml(&installation.label),arguments)
    }
    fn ensure_plist(installation: &Installation) -> Result<()> {
        let actual = read_bytes(&installation.plist)?;
        if actual != plist(installation).as_bytes() {
            bail!("refusing to manage an altered or unrelated LaunchAgent property list");
        }
        Ok(())
    }
    #[derive(Debug, Default, Serialize)]
    struct Job {
        loaded: bool,
        managed: bool,
        running: bool,
        pid: Option<u32>,
        state: Option<String>,
        last_exit_code: Option<i32>,
    }
    fn job(installation: &Installation, control: &dyn Launchctl) -> Result<Job> {
        let output = control.invoke(&["print".into(), target(installation)])?;
        if output.code != 0 {
            if matches!(output.code, 3 | 113) {
                return Ok(Job::default());
            }
            bail!(
                "launchctl could not inspect this per-user service (exit {})",
                output.code
            );
        }
        let mut values = BTreeMap::new();
        let mut depth = 0usize;
        let mut outer = false;
        let mut actual_arguments = vec![];
        let mut in_arguments = false;
        for line in output.stdout.lines() {
            let line = line.trim();
            let top = if outer { 1 } else { 0 };
            if line == "}" {
                if in_arguments && depth == top + 1 {
                    in_arguments = false;
                }
                depth = depth.saturating_sub(1);
                continue;
            }
            if in_arguments && depth == top + 1 {
                actual_arguments.push(line.trim_matches('"').to_owned());
            }
            if depth == top {
                if let Some((key, value)) = line.split_once(" = ") {
                    values.insert(key, value);
                    if key == "arguments" && value == "{" {
                        in_arguments = true;
                    }
                }
            }
            if line.ends_with('{') {
                if !outer
                    && depth == 0
                    && !line.starts_with("environment ")
                    && !line.starts_with("arguments ")
                {
                    outer = true;
                }
                depth += 1;
            }
        }
        let strip = |value: &&str| value.trim().trim_matches('"').to_owned();
        let path = values.get("path").map(strip);
        let program = values.get("program").map(strip);
        let managed = path.as_deref() == installation.plist.to_str()
            && program.as_deref() == installation.options.executable.to_str()
            && actual_arguments == arguments(installation);
        let pid = values.get("pid").and_then(|value| value.parse().ok());
        let state = values.get("state").map(strip);
        Ok(Job {
            loaded: true,
            managed,
            running: pid.is_some() && state.as_deref() == Some("running"),
            pid,
            state,
            last_exit_code: values
                .get("last exit code")
                .and_then(|value| value.parse().ok()),
        })
    }
    fn credential_diagnostics(home: &Path) -> Result<Value> {
        let config_path = home.join("config.json");
        if !exists(&config_path)? {
            return Ok(
                json!({"passwords_persisted":false,"environment_profiles":[],"note":"Service credentials must already be available through Keychain or the launchd bootstrap environment."}),
            );
        }
        let configuration: config::Config = read(&config_path)?;
        let profiles=configuration.profiles.iter().filter_map(|(name,profile)|profile.password_env.as_ref().map(|variable|json!({"profile":name,"variable":variable,"available_in_this_process":std::env::var_os(variable).is_some()}))).collect::<Vec<_>>();
        Ok(
            json!({"passwords_persisted":false,"environment_profiles":profiles,"note":"Availability above refers to the current process only. launchd does not inherit a terminal's exported variables. Values are never copied into service files. Profiles without password_env use existing Keychain lookup."}),
        )
    }
    fn plan(
        installation: &Installation,
        operation: &str,
        commands: Vec<Vec<String>>,
    ) -> Result<Value> {
        Ok(
            json!({"operation":operation,"dry_run":true,"label":installation.label,"home":installation.options.home,"ipc_root":installation.options.ipc_root,"native_watcher":installation.options.app,"plist_path":installation.plist,"plist":plist(installation),"commands":commands,"credentials":credential_diagnostics(&installation.options.home)?,"remote_files_modified":false}),
        )
    }
    pub(super) fn install(
        options: &ServiceOptions,
        dry_run: bool,
        control: &dyn Launchctl,
    ) -> Result<Value> {
        let installation = planned(options, true)?;
        if dry_run {
            return plan(&installation, "install", vec![]);
        }
        let service = service_dir(&installation.options.home, true)?;
        let _lock = lock(&service.join("management.lock"), true)?;
        let previous = if exists(&install_path(&installation.options.home)?)? {
            Some(load(&installation.options.home)?)
        } else {
            None
        };
        if let Some(previous) = &previous {
            if previous.plist != installation.plist {
                bail!("uninstall the existing service before changing its LaunchAgents directory");
            }
            if previous != &installation && (job(previous, control)?.loaded || active(&service)?) {
                bail!("stop the existing managed service before changing its installation options");
            }
        }
        if exists(&installation.plist)? {
            let actual = read_bytes(&installation.plist)?;
            let owned = actual == plist(&installation).as_bytes()
                || previous
                    .as_ref()
                    .is_some_and(|previous| actual == plist(previous).as_bytes());
            if !owned {
                bail!("refusing to overwrite an unrelated or altered LaunchAgent");
            }
        }
        directory(&installation.options.ipc_root, true, true)?;
        directory(&installation.options.ipc_root.join("Transfers"), true, true)?;
        directory(
            installation.plist.parent().unwrap(),
            true,
            installation.options.launch_agents_dir.is_some(),
        )?;
        state::atomic_write(&installation.plist, plist(&installation).as_bytes())?;
        write(&install_path(&installation.options.home)?, &installation)?;
        let mut result = status(&installation.options.home, control)?;
        result["operation"] = json!("install");
        result["started_by_install"] = json!(false);
        Ok(result)
    }
    fn command(control: &dyn Launchctl, args: Vec<String>) -> Result<()> {
        let output = control.invoke(&args)?;
        if output.code != 0 {
            bail!(
                "launchctl {} failed (exit {}); existing state was preserved",
                args.first().map(String::as_str).unwrap_or("operation"),
                output.code
            );
        }
        Ok(())
    }
    pub(super) fn action(
        home: &Path,
        operation: ServiceAction,
        dry_run: bool,
        control: &dyn Launchctl,
    ) -> Result<Value> {
        let installation = load(home)?;
        ensure_plist(&installation)?;
        if matches!(operation, ServiceAction::Start | ServiceAction::Restart) {
            // Validate before a restart can stop a healthy existing owner.
            ready(&installation)?;
        }
        let commands = match operation {
            ServiceAction::Start => vec![vec![
                "bootstrap".into(),
                format!("gui/{}", uid()),
                installation.plist.display().to_string(),
            ]],
            ServiceAction::Stop | ServiceAction::Uninstall => {
                vec![vec!["bootout".into(), target(&installation)]]
            }
            ServiceAction::Restart => vec![
                vec!["bootout".into(), target(&installation)],
                vec![
                    "bootstrap".into(),
                    format!("gui/{}", uid()),
                    installation.plist.display().to_string(),
                ],
            ],
        };
        let name = match operation {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Uninstall => "uninstall",
        };
        if dry_run {
            return plan(&installation, name, commands);
        }
        let service = service_dir(&installation.options.home, true)?;
        let _lock = lock(&service.join("management.lock"), true)?;
        let current = job(&installation, control)?;
        if current.loaded && !current.managed {
            bail!("refusing to stop or replace an unrelated launchd service with the same label");
        }
        if operation == ServiceAction::Start && !current.loaded && active(&service)? {
            bail!("an owned foreground supervisor is already running; stop it before starting launchd management");
        }
        if matches!(
            operation,
            ServiceAction::Stop | ServiceAction::Restart | ServiceAction::Uninstall
        ) {
            if current.loaded {
                command(control, vec!["bootout".into(), target(&installation)])?;
            }
            stop_foreground(&installation)?;
        }
        if matches!(operation, ServiceAction::Start | ServiceAction::Restart) {
            if !current.loaded || operation == ServiceAction::Restart {
                command(
                    control,
                    vec![
                        "bootstrap".into(),
                        format!("gui/{}", uid()),
                        installation.plist.display().to_string(),
                    ],
                )?;
            } else if !current.running {
                command(control, vec!["kickstart".into(), target(&installation)])?;
            }
        }
        if operation == ServiceAction::Uninstall {
            ensure_plist(&installation)?;
            fs::remove_file(&installation.plist)?;
            fs::remove_file(install_path(&installation.options.home)?)?;
            File::open(installation.plist.parent().unwrap())?.sync_all()?;
            File::open(&service)?.sync_all()?;
        }
        let mut result = status(&installation.options.home, control)?;
        result["operation"] = json!(name);
        result["offline_data_preserved"] = json!(true);
        Ok(result)
    }
    fn active(service: &Path) -> Result<bool> {
        let path = service.join("supervisor.lock");
        if !exists(&path)? {
            return Ok(false);
        }
        private_file(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(error) => Err(anyhow::anyhow!("cannot inspect supervisor lock: {error}")),
        }
    }
    pub(super) fn status(home: &Path, control: &dyn Launchctl) -> Result<Value> {
        let home = directory(home, false, true)?;
        let installation_path = install_path(&home)?;
        if !exists(&installation_path)? {
            return Ok(
                json!({"installed":false,"label":label(&home),"launchd":{"loaded":false,"running":false},"supervisor_running":false}),
            );
        }
        let installation = load(&home)?;
        ensure_plist(&installation)?;
        let service = service_dir(&home, false)?;
        let running = active(&service)?;
        let runtime = if exists(&service.join("status.json"))? {
            read::<Value>(&service.join("status.json"))?
        } else {
            Value::Null
        };
        Ok(
            json!({"installed":true,"label":installation.label,"plist_path":installation.plist,"home":home,"ipc_root":installation.options.ipc_root,"native_watcher":installation.options.app,"launchd":job(&installation,control)?,"supervisor_running":running,"runtime":runtime,"credentials":credential_diagnostics(&home)?}),
        )
    }
    fn stop_foreground(installation: &Installation) -> Result<()> {
        let service = service_dir(&installation.options.home, false)?;
        if !active(&service)? {
            return Ok(());
        }
        let runtime: Value = read(&service.join("status.json"))?;
        if runtime["label"] != installation.label {
            bail!("supervisor runtime identity mismatch");
        }
        let pid = runtime["supervisor_pid"]
            .as_u64()
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u64)
            .context("supervisor PID missing")?;
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            bail!("could not stop the owned foreground supervisor");
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        while active(&service)? {
            if Instant::now() >= deadline {
                bail!("owned supervisor has not stopped; no installation files were removed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
    fn event(service: &Path, value: Value) -> Result<()> {
        let log = service.join("events.log");
        let previous = service.join("events.previous.log");
        if exists(&log)? {
            private_file(&log)?;
            if fs::metadata(&log)?.len() >= LOG_LIMIT {
                if exists(&previous)? {
                    private_file(&previous)?;
                    fs::remove_file(&previous)?;
                }
                fs::rename(&log, &previous)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&log)?;
        private_file(&log)?;
        serde_json::to_writer(&mut file, &json!({"time":state::now(),"event":value}))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
    struct Role {
        name: &'static str,
        executable: PathBuf,
        args: Vec<String>,
        child: Option<tokio::process::Child>,
        starts: u64,
        next: Instant,
        started: Option<Instant>,
        delay: u64,
        last_exit: Option<i32>,
    }
    impl Role {
        fn snapshot(&self) -> Value {
            json!({"pid":self.child.as_ref().and_then(|child|child.id()),"starts":self.starts,"restarts":self.starts.saturating_sub(1),"last_exit_code":self.last_exit,"restart_in_seconds":self.next.saturating_duration_since(Instant::now()).as_secs()})
        }
        async fn tick(&mut self, service: &Path) -> Result<bool> {
            let mut changed = false;
            if let Some(child) = &mut self.child {
                if let Some(status) = child.try_wait()? {
                    let stable = self
                        .started
                        .is_some_and(|started| started.elapsed() >= Duration::from_secs(30));
                    self.delay = if stable {
                        1
                    } else {
                        self.delay.saturating_mul(2).clamp(1, 30)
                    };
                    self.last_exit = status
                        .code()
                        .or_else(|| status.signal().map(|signal| -signal));
                    self.child = None;
                    self.started = None;
                    self.next = Instant::now() + Duration::from_secs(self.delay);
                    event(
                        service,
                        json!({"kind":"child_exited","role":self.name,"exit_code":self.last_exit,"retry_seconds":self.delay}),
                    )?;
                    changed = true;
                }
            }
            if self.child.is_none() && Instant::now() >= self.next {
                match tokio::process::Command::new(&self.executable)
                    .args(&self.args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => {
                        let pid = child.id();
                        self.child = Some(child);
                        self.starts += 1;
                        self.started = Some(Instant::now());
                        event(
                            service,
                            json!({"kind":"child_started","role":self.name,"pid":pid,"starts":self.starts}),
                        )?;
                    }
                    Err(error) => {
                        self.delay = self.delay.saturating_mul(2).clamp(1, 30);
                        self.next = Instant::now() + Duration::from_secs(self.delay);
                        event(
                            service,
                            json!({"kind":"spawn_failed","role":self.name,"error_kind":format!("{:?}",error.kind()),"retry_seconds":self.delay}),
                        )?;
                    }
                }
                changed = true;
            }
            Ok(changed)
        }
        async fn stop(&mut self) {
            if let Some(mut child) = self.child.take() {
                if let Some(pid) = child.id() {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
        }
    }
    fn role(name: &'static str, executable: PathBuf, args: Vec<String>) -> Role {
        Role {
            name,
            executable,
            args,
            child: None,
            starts: 0,
            next: Instant::now(),
            started: None,
            delay: 0,
            last_exit: None,
        }
    }
    fn runtime(
        installation: &Installation,
        roles: &[Role],
        state: &str,
        credentials: &Value,
    ) -> Value {
        json!({"format":FORMAT,"label":installation.label,"supervisor_pid":std::process::id(),"state":state,"updated":state::now(),"children":roles.iter().map(|role|(role.name.to_owned(),role.snapshot())).collect::<BTreeMap<_,_>>(),"credentials":credentials})
    }
    pub(super) async fn run(home: &Path) -> Result<()> {
        let installation = load(home)?;
        ensure_plist(&installation)?;
        ready(&installation)?;
        if std::env::current_exe()?.canonicalize()? != installation.options.executable {
            bail!("run the installed service executable or reinstall after moving it");
        }
        let service = service_dir(&installation.options.home, true)?;
        let _lock = lock(&service.join("supervisor.lock"), false)?;
        if UnixStream::connect(installation.options.ipc_root.join("bridge.sock")).is_ok() {
            bail!("an existing broker owns this IPC endpoint; it was left running");
        }
        let client = installation.options.home.join("client.lock");
        if exists(&client)? {
            private_file(&client)?;
            let probe = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&client)?;
            if probe.try_lock().is_err() {
                bail!("another manual client or broker is using this state directory; it was left running");
            }
        }
        directory(&installation.options.ipc_root, true, true)?;
        directory(&installation.options.ipc_root.join("Transfers"), true, true)?;
        let mut broker = vec![
            "--home".into(),
            installation.options.home.display().to_string(),
            "--progress".into(),
            "off".into(),
            "provider".into(),
            "serve".into(),
            "--socket".into(),
            installation
                .options
                .ipc_root
                .join("bridge.sock")
                .display()
                .to_string(),
            "--staging".into(),
            installation
                .options
                .ipc_root
                .join("Transfers")
                .display()
                .to_string(),
        ];
        if installation.options.development_tcp {
            broker.push("--development-tcp".into());
        }
        let mut roles = vec![role(
            "broker",
            installation.options.executable.clone(),
            broker,
        )];
        if let Some(app) = &installation.options.app {
            roles.push(role(
                "native_watcher",
                app.join("Contents/MacOS/RDriveCompanion"),
                vec!["--watch-domains".into()],
            ));
        }
        let credentials = credential_diagnostics(&installation.options.home)?;
        event(
            &service,
            json!({"kind":"supervisor_started","pid":std::process::id()}),
        )?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        let mut heartbeat = Instant::now();
        write(
            &service.join("status.json"),
            &runtime(&installation, &roles, "running", &credentials),
        )?;
        loop {
            let mut changed = false;
            for role in &mut roles {
                changed |= role.tick(&service).await?;
            }
            if changed || heartbeat.elapsed() >= Duration::from_secs(2) {
                write(
                    &service.join("status.json"),
                    &runtime(&installation, &roles, "running", &credentials),
                )?;
                heartbeat = Instant::now();
            }
            tokio::select! {_=terminate.recv()=>break,_=&mut interrupt=>break,_=tokio::time::sleep(Duration::from_millis(250))=>{}}
        }
        for role in roles.iter_mut().rev() {
            role.stop().await;
        }
        write(
            &service.join("status.json"),
            &runtime(&installation, &roles, "stopped", &credentials),
        )?;
        event(
            &service,
            json!({"kind":"supervisor_stopped","pid":std::process::id()}),
        )?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            os::unix::fs::{symlink, PermissionsExt},
            sync::Mutex,
        };
        #[derive(Default)]
        struct Fake {
            output: Mutex<Option<String>>,
            calls: Mutex<Vec<Vec<String>>>,
        }
        impl Launchctl for Fake {
            fn invoke(&self, args: &[String]) -> Result<CommandOutput> {
                self.calls.lock().unwrap().push(args.to_vec());
                if args[0] == "print" {
                    return Ok(self
                        .output
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|value| CommandOutput {
                            code: 0,
                            stdout: value.clone(),
                        })
                        .unwrap_or(CommandOutput {
                            code: 113,
                            stdout: String::new(),
                        }));
                }
                if args[0] == "bootout" {
                    *self.output.lock().unwrap() = None;
                }
                Ok(CommandOutput::default())
            }
        }
        fn fixture() -> (tempfile::TempDir, ServiceOptions) {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let home = root.join("state");
            directory(&home, true, true).unwrap();
            let binary = root.join("rdrive");
            fs::write(&binary, b"fixture executable").unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            let options = ServiceOptions {
                home,
                executable: binary,
                ipc_root: root.join("ipc"),
                app: None,
                development_tcp: true,
                launch_agents_dir: Some(root.join("agents")),
            };
            (temporary, options)
        }
        #[test]
        fn dry_run_writes_nothing_and_plan_contains_no_environment_values() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            let result = install(&options, true, &fake).unwrap();
            assert_eq!(result["dry_run"], true);
            assert!(!options.home.join("service").exists());
            assert!(!options.ipc_root.exists());
            let xml = result["plist"].as_str().unwrap();
            assert!(!xml.contains("EnvironmentVariables"));
            assert!(xml.contains("KeepAlive"));
            assert!(xml.contains("service</string><string>run"));
            assert!(fake.calls.lock().unwrap().is_empty());
        }
        #[test]
        fn installation_is_private_idempotent_and_uninstall_preserves_offline_data() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let installation = load(&options.home).unwrap();
            let first = fs::read(&installation.plist).unwrap();
            install(&options, false, &fake).unwrap();
            assert_eq!(fs::read(&installation.plist).unwrap(), first);
            assert_eq!(
                fs::metadata(&installation.plist).unwrap().mode() & 0o777,
                0o600
            );
            let retained = options.home.join("retained-edit");
            fs::write(&retained, b"user bytes").unwrap();
            action(&options.home, ServiceAction::Uninstall, false, &fake).unwrap();
            assert_eq!(fs::read(retained).unwrap(), b"user bytes");
            assert!(!installation.plist.exists());
            assert!(!install_path(&options.home).unwrap().exists());
        }
        #[test]
        fn missing_executable_can_be_stopped_repaired_and_uninstalled_without_losing_identity() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let installation = load(&options.home).unwrap();
            let owned_job = format!(
                "state = running\npid = 123\npath = {}\nprogram = {}\narguments = {{\n{}\n}}\n",
                installation.plist.display(),
                options.executable.display(),
                arguments(&installation).join("\n")
            );
            let moved = options.executable.with_file_name("moved-rdrive");
            fs::rename(&options.executable, &moved).unwrap();
            *fake.output.lock().unwrap() = Some(owned_job.clone());
            assert_eq!(
                status(&options.home, &fake).unwrap()["launchd"]["managed"],
                true
            );
            for operation in [ServiceAction::Start, ServiceAction::Restart] {
                for dry_run in [false, true] {
                    assert!(action(&options.home, operation, dry_run, &fake).is_err());
                }
            }
            assert!(!fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| { matches!(args[0].as_str(), "bootstrap" | "bootout" | "kickstart") }));
            // A missing executable never weakens the loaded job's exact argv
            // and plist ownership checks.
            *fake.output.lock().unwrap() = Some(owned_job.replace("--progress", "--unrelated"));
            assert!(action(&options.home, ServiceAction::Stop, false, &fake).is_err());
            assert!(!fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| args[0] == "bootout"));
            *fake.output.lock().unwrap() = Some(owned_job);
            assert_eq!(
                action(&options.home, ServiceAction::Stop, false, &fake).unwrap()["launchd"]
                    ["loaded"],
                false
            );
            let mut replacement = options.clone();
            replacement.executable = moved.clone();
            install(&replacement, false, &fake).unwrap();
            let repaired = load(&options.home).unwrap();
            assert_eq!(repaired.options.executable, moved);
            assert_eq!(
                fs::read(&repaired.plist).unwrap(),
                plist(&repaired).as_bytes()
            );
            fs::remove_file(&replacement.executable).unwrap();
            let retained = options.home.join("retained-edit");
            fs::write(&retained, b"user bytes").unwrap();
            action(&options.home, ServiceAction::Uninstall, false, &fake).unwrap();
            assert!(!repaired.plist.exists());
            assert_eq!(fs::read(retained).unwrap(), b"user bytes");
        }
        #[test]
        fn removed_companion_app_does_not_block_status_or_uninstall() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let mut installation = load(&options.home).unwrap();
            // Model a previously validated app that has since been removed.
            installation.options.app = Some(options.home.parent().unwrap().join("removed.app"));
            write(&install_path(&options.home).unwrap(), &installation).unwrap();
            state::atomic_write(&installation.plist, plist(&installation).as_bytes()).unwrap();
            assert_eq!(status(&options.home, &fake).unwrap()["installed"], true);
            assert!(action(&options.home, ServiceAction::Start, false, &fake).is_err());
            assert!(action(&options.home, ServiceAction::Restart, true, &fake).is_err());
            action(&options.home, ServiceAction::Uninstall, false, &fake).unwrap();
            assert!(!installation.plist.exists());
        }
        #[test]
        fn changing_launch_agents_directory_preserves_the_existing_uninstallable_installation() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let installation = load(&options.home).unwrap();
            let old_plist = fs::read(&installation.plist).unwrap();
            let old_control = fs::read(install_path(&options.home).unwrap()).unwrap();
            let new_agents = options.home.parent().unwrap().join("other-agents");
            let mut replacement = options.clone();
            replacement.launch_agents_dir = Some(new_agents.clone());
            assert!(install(&replacement, false, &fake)
                .unwrap_err()
                .to_string()
                .contains("uninstall the existing service"));
            assert_eq!(fs::read(&installation.plist).unwrap(), old_plist);
            assert_eq!(
                fs::read(install_path(&options.home).unwrap()).unwrap(),
                old_control
            );
            assert_eq!(load(&options.home).unwrap(), installation);
            assert!(!new_agents.exists());
            action(&options.home, ServiceAction::Uninstall, false, &fake).unwrap();
            assert!(!installation.plist.exists());
            assert!(!install_path(&options.home).unwrap().exists());
        }
        #[test]
        fn unknown_job_and_altered_plist_are_never_stopped_or_replaced() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            *fake.output.lock().unwrap() = Some(
                "state = running\npid = 42\npath = /unrelated.plist\nprogram = /unrelated\n".into(),
            );
            assert!(action(&options.home, ServiceAction::Stop, false, &fake).is_err());
            assert!(!fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| args[0] == "bootout"));
            let installation = load(&options.home).unwrap();
            fs::write(&installation.plist, b"unrelated plist").unwrap();
            assert!(install(&options, false, &fake).is_err());
            assert!(action(&options.home, ServiceAction::Start, false, &fake).is_err());
            assert_eq!(fs::read(installation.plist).unwrap(), b"unrelated plist");
        }
        #[test]
        fn load_status_and_mutating_plans_validate_all_managed_paths() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let installation = load(&options.home).unwrap();
            for operation in [
                ServiceAction::Start,
                ServiceAction::Stop,
                ServiceAction::Restart,
                ServiceAction::Uninstall,
            ] {
                assert_eq!(
                    action(&options.home, operation, true, &fake).unwrap()["dry_run"],
                    true
                );
            }
            let state = status(&options.home, &fake).unwrap();
            assert_eq!(state["installed"], true);
            assert_eq!(state["supervisor_running"], false);
            assert_eq!(state["launchd"]["loaded"], false);
            let args = fake.calls.lock().unwrap();
            assert!(args.iter().all(|args| args[0] == "print"));
            drop(args);
            fs::remove_file(&installation.plist).unwrap();
            let outside = options.home.join("outside");
            fs::write(&outside, b"outside").unwrap();
            symlink(&outside, &installation.plist).unwrap();
            assert!(status(&options.home, &fake).is_err());
            assert!(action(&options.home, ServiceAction::Uninstall, false, &fake).is_err());
            assert_eq!(fs::read(outside).unwrap(), b"outside");
        }
        #[test]
        fn supervisor_lock_and_bounded_private_log_are_independent_of_store_lock() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let service = service_dir(&options.home, false).unwrap();
            let held = lock(&service.join("supervisor.lock"), false).unwrap();
            assert!(active(&service).unwrap());
            assert!(lock(&service.join("supervisor.lock"), false).is_err());
            drop(held);
            assert!(!active(&service).unwrap());
            let file = service.join("events.log");
            fs::write(&file, vec![b'x'; LOG_LIMIT as usize]).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            event(&service, json!({"kind":"test"})).unwrap();
            assert!(
                fs::metadata(service.join("events.previous.log"))
                    .unwrap()
                    .len()
                    >= LOG_LIMIT
            );
            assert!(fs::metadata(file).unwrap().len() < 1024);
        }
        #[test]
        fn labels_are_home_scoped_and_launchctl_environment_is_not_exposed() {
            let (_temporary, options) = fixture();
            let fake = Fake::default();
            install(&options, false, &fake).unwrap();
            let installation = load(&options.home).unwrap();
            assert_ne!(installation.label, label(&options.home.join("other")));
            *fake.output.lock().unwrap()=Some(format!("gui/501/test = {{\npath = {}\nprogram = {}\narguments = {{\n{}\n}}\nstate = running\npid = 123\nenvironment = {{\nTEST_PASSWORD = never-copy-this\npath = /do-not-parse-environment-fields\n}}\n}}\n",installation.plist.display(),options.executable.display(),arguments(&installation).join("\n")));
            let response = status(&options.home, &fake).unwrap();
            assert_eq!(response["launchd"]["managed"], true);
            assert_eq!(response["launchd"]["running"], true);
            assert!(!response.to_string().contains("never-copy-this"));
        }
    }
}

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rdrive::{
    backend::{self, Backend},
    config::{self, Profile, Protocol},
    operations as ops,
    state::{self, Store},
    sync,
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser)]
#[command(
    version,
    about = "WebDAV and SMB files, offline cache, durable uploads, and conservative sync"
)]
struct Cli {
    /// State/config directory (also RDRIVE_HOME).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Compact machine-readable JSON. Errors are JSON on stderr.
    #[arg(long, global = true)]
    json: bool,
    /// Aggregate payload bytes/second across all jobs (headers excluded).
    #[arg(long,global=true)]
    limit:Option<u64>,
    /// Live stderr progress; auto uses terminal bars or JSON when redirected.
    #[arg(long,global=true,value_enum,default_value="auto")]
    progress:ProgressMode,
    /// Concurrent queue targets. Jobs for the same target retain order.
    #[arg(long,global=true,default_value_t=4)]
    jobs:usize,
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, Copy, ValueEnum)]
enum Proto {
    Webdav,
    Smb,
}
#[derive(Clone, Copy, ValueEnum)]
enum SyncDirection {
    Push,
    Pull,
    Both,
}
#[derive(Clone,Copy,ValueEnum)]enum ProgressMode{Auto,Off,Json}
#[derive(Subcommand)]
enum Command {
    /// Check for a stable CLI release on GitHub.
    Update {
        /// Install the newer release over the standalone CLI.
        #[arg(long)]
        apply: bool,
    },
    /// Inspect and repair ordinary two-way sync conflicts.
    Conflict {
        #[command(subcommand)]
        command: ConflictCommand,
    },
    /// Expose existing operations to AI clients over MCP standard input/output.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Add/list/remove connection profiles. Never accepts a password argument.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Test connection and show backend capabilities.
    Info { profile: String },
    /// List remote children or recursively search the tree.
    Ls {
        profile: String,
        #[arg(default_value = "")]
        remote: String,
        #[arg(short, long)]
        recursive: bool,
        #[arg(long)]
        contains: Option<String>,
    },
    /// Read remote type, size, timestamp and revision metadata.
    Stat { profile: String, remote: String },
    /// Download through a verified temporary file, then commit locally.
    Get {
        profile: String,
        remote: String,
        local: PathBuf,
        #[arg(long)]
        overwrite: bool,
        /// Continue an interrupted download from its durable verified prefix.
        #[arg(long)]
        resume: bool,
    },
    /// Print remote file bytes to stdout (status goes to stderr).
    Cat {
        profile: String,
        remote: String,
        /// Refuse larger files before downloading; used for bounded MCP text reads.
        #[arg(long)]
        max_bytes: Option<u64>,
    },
    /// Snapshot a local file to the durable queue, then upload it.
    Put {
        profile: String,
        local: PathBuf,
        remote: String,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        version: bool,
        #[arg(long)]
        queue: bool,
        /// Use a durable resumable staging session (SMB or configured Nextcloud).
        #[arg(long)]
        resume: bool,
    },
    /// Create a remote directory; --parents creates missing ancestors.
    Mkdir {
        profile: String,
        remote: String,
        #[arg(short, long)]
        parents: bool,
    },
    /// Rename or move within one connection, without overwrite by default.
    Mv {
        profile: String,
        from: String,
        to: String,
        #[arg(long)]
        overwrite: bool,
    },
    /// Copy within one connection, without overwrite by default.
    Cp {
        profile: String,
        from: String,
        to: String,
        #[arg(long)]
        overwrite: bool,
    },
    /// Explicit remote deletion. Directories require --recursive.
    Rm {
        profile: String,
        remote: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Copyable location URL. Does not create a public sharing link.
    Url {
        profile: String,
        #[arg(default_value = "")]
        remote: String,
    },
    /// Download, edit with an executable from EDITOR, then queue a guarded upload.
    Edit { profile: String, remote: String },
    /// Fetch, pin, read offline and prune cached files.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Inspect, run or cancel durable upload jobs.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Plan by default; --apply commits. Conflicts stop the entire plan.
    Sync {
        profile: String,
        local: PathBuf,
        #[arg(default_value = "")]
        remote: String,
        #[arg(long, value_enum, default_value = "both")]
        direction: SyncDirection,
        #[arg(long)]
        apply: bool,
        /// Propagate observed deletions after a successful baseline exists.
        #[arg(long)]
        delete: bool,
        #[arg(long)]
        metadata:bool,
    },
    /// Repeat sync in the foreground; stop with Ctrl-C.
    Watch {
        profile: String,
        local: PathBuf,
        #[arg(default_value = "")]
        remote: String,
        #[arg(long, value_enum, default_value = "both")]
        direction: SyncDirection,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        delete: bool,
        #[arg(long, default_value_t = 30)]
        interval: u64,
        #[arg(long)]
        metadata:bool,
    },
    /// Stream server change notifications (SMB); overflow/disconnect requires rescan.
    Notify {profile:String,#[arg(default_value="")]remote:String,#[arg(long)]recursive:bool,#[arg(long)]count:Option<usize>},
    /// Atomic managed snapshot repository and durable transactions.
    Repo {#[command(subcommand)]command:RepoCommand},
    /// Finder/File Provider bridge and domain configuration.
    Provider {#[command(subcommand)]command:ProviderCommand},
    /// Configure on-demand offline domains and manage retained local edits.
    Smart {#[command(subcommand)]command:SmartCommand},
    /// Configure background metadata indexing and request scoped refreshes.
    Index {#[command(subcommand)]command:IndexCommand},
    /// Smart inclusion/exclusion rules; never deletes excluded remote files.
    Filter { #[command(subcommand)] command: FilterCommand },
    /// Automatic version retention and conflict resolution policy.
    Policy { #[command(subcommand)] command: PolicyCommand },
    /// Install and control the per-user background supervisor.
    Service { #[command(subcommand)] command: ServiceCommand },
    /// Capture, apply or synchronize selected user extended attributes/Finder tags.
    Metadata {#[command(subcommand)]command:MetadataCommand},
    /// Save, inspect, restore and prune custom remote file versions.
    Versions {
        #[command(subcommand)]
        command: VersionsCommand,
    },
    /// Acquire a WebDAV write lock; save its token for later writes/unlock.
    Lock {
        profile: String,
        remote: String,
        #[arg(long, default_value_t = 600)]
        seconds: u32,
    },
    /// Uses RDRIVE_WEBDAV_LOCK_TOKEN; tokens are not accepted as arguments.
    Unlock { profile: String, remote: String },
    /// Local queue/cache summary; works offline.
    Status,
    /// Mount through macOS native SMB/WebDAV helpers.
    Mount {
        profile: String,
        local: PathBuf,
        #[arg(long)]
        read_only: bool,
        #[arg(long)]
        browse: bool,
        #[arg(long)]
        no_prompt: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// List mounts reported by the operating system.
    Mounts,
    /// Unmount a native SMB/WebDAV filesystem.
    Unmount {
        local: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Inspect resumable transfers or discard a saved download prefix.
    Transfers {
        #[command(subcommand)]
        command: TransferCommand,
    },
    /// Show locally recorded completed operations.
    History {
        // Clap shares this public name with the inherited transfer flag. Keep
        // its id and numeric type identical to avoid duplicate flags/downcasts.
        #[arg(id = "limit", long = "limit", default_value_t = 20)]
        count: u64,
    },
}
#[derive(Subcommand)]
enum McpCommand {
    /// Print the MCP tool catalog as JSON without starting a server.
    Tools {
        #[arg(long)]
        allow_write: bool,
    },
    /// Run a JSON-RPC stdio server; remote/local changes require --allow-write.
    Serve {
        #[arg(long)]
        allow_write: bool,
        /// Permit local file access inside this directory (repeatable).
        #[arg(long)]
        local_root: Vec<PathBuf>,
        /// Maximum duration of each underlying CLI operation, in seconds.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
}
#[derive(Subcommand)]enum RepoCommand {
    Init{profile:String,#[arg(default_value="")]root:String},
    Begin{profile:String,#[arg(default_value="")]root:String},
    Put{transaction:String,local:PathBuf,path:String},
    Mkdir{transaction:String,path:String},
    Rm{transaction:String,path:String,#[arg(long)]recursive:bool},
    Mv{transaction:String,from:String,to:String},
    Commit{transaction:String},
    Status{transaction:String},
    Abort{transaction:String},
    Transactions,
    Ls{profile:String,#[arg(default_value="")]root:String},
    Get{profile:String,path:String,local:PathBuf,#[arg(long,default_value="")]root:String,#[arg(long)]overwrite:bool},
}
#[derive(Subcommand)]enum ProviderCommand {
    Serve{#[arg(long)]socket:PathBuf,#[arg(long)]staging:PathBuf,#[arg(long)]development_tcp:bool},
    Domains,
    DomainAdd{id:String,profile:String,#[arg(default_value="")]remote:String,#[arg(long)]repository:bool,#[arg(long)]smart:bool,#[arg(long)]local_root:Option<PathBuf>,#[arg(long)]display_name:Option<String>},
    DomainRemove{id:String},
    /// Invoke the built native companion (build first with macos/build.py).
    Native{#[arg(long)]app:Option<PathBuf>,#[arg(trailing_var_arg=true,allow_hyphen_values=true)]args:Vec<String>},
}
#[derive(Clone, Copy, ValueEnum)]
enum ConflictSide {
    Local,
    Remote,
}

#[derive(Subcommand)]
enum ConflictCommand {
    /// List conflicting paths from a read-only two-way sync plan.
    List {
        profile: String,
        local: PathBuf,
        #[arg(default_value = "")]
        remote: String,
    },
    /// Preview a file repair, or apply it using its current preview token.
    Repair {
        profile: String,
        local: PathBuf,
        remote: String,
        /// Relative path within the paired sync directories.
        path: String,
        #[arg(long, value_enum)]
        choose: ConflictSide,
        /// Back up the displaced file and apply the chosen side.
        #[arg(long, requires = "expect")]
        apply: bool,
        /// Token from a preview of this repair.
        #[arg(long, requires = "apply")]
        expect: Option<String>,
    },
}

#[derive(Subcommand)]
enum SmartCommand {
    /// Save a Smart domain; configure a 5 GiB clean-content budget if none exists.
    Setup { id:String, profile:String, #[arg(default_value="")]remote:String,
        #[arg(long)]repository:bool, #[arg(long)]display_name:Option<String>,
        /// Register with the already built macOS File Provider companion.
        #[arg(long)]register:bool, #[arg(long,requires="register")]app:Option<PathBuf> },
    /// Show local Smart state, durable mutations and clean-cache accounting.
    Status,
    /// List retained mutations, optionally restricted to one domain.
    List { domain:Option<String> },
    Pause,
    Resume,
    /// Mark retained work retryable without changing its original version guards.
    Retry { domain:String, mutation:Option<String> },
    /// Cancel pending work while preserving its independent local snapshot.
    Cancel { domain:String, mutation:String },
    /// Resolve a conflict using a durable decision; remote retains a local recovery copy.
    Resolve { domain:String, mutation:String, #[arg(long,value_parser=["keep-both","replace","remote"])] strategy:String },
    /// Refresh a directory after a stale Finder request that never reached the server.
    RefreshConflict { domain: String, mutation: String },
    /// List retained remote namespace recovery backups (space is retained).
    Recovery { domain:String },
    /// Export a retained local edit to a separate real file for recovery.
    Export { domain:String, mutation:String, local:PathBuf, #[arg(long)]overwrite:bool },
    /// Discard only a completed/cancelled/superseded recovery snapshot.
    Discard { domain:String, mutation:String },
    /// Perform one reconnect/replay pass; provider serve also runs automatically.
    Run { #[arg(long)]staging:Option<PathBuf> },
}
#[derive(Subcommand)]
enum IndexCommand {
    /// Search cached file names/paths locally; works without network access.
    Search { domain:String, query:String, #[arg(long,default_value="")] scope:String,
        #[arg(long,value_parser=["file","directory"])] kind:Option<String>,
        #[arg(long,default_value_t=100)] page_size:usize, #[arg(long,default_value_t=0)] offset:usize },
    /// Inspect local index progress without waiting for a network operation.
    Status { domain: Option<String> },
    /// Persist background crawl and refresh policy. None of these options downloads files.
    Config {
        #[arg(long,conflicts_with="disable")] enable: bool,
        #[arg(long,conflicts_with="enable")] disable: bool,
        #[arg(long,conflicts_with="all")] depth: Option<u32>,
        #[arg(long,conflicts_with="depth")] all: bool,
        #[arg(long,value_parser=clap::value_parser!(u64).range(1..=31_536_000))] refresh_seconds: Option<u64>,
        #[arg(long,value_parser=clap::value_parser!(u64).range(1..=31_536_000))] active_refresh_seconds: Option<u64>,
    },
    /// Queue a metadata refresh; provider serve performs it asynchronously.
    Refresh { domain:String, #[arg(default_value="")]remote:String, #[arg(long)]recursive:bool },
}
#[derive(Subcommand)]
enum FilterCommand {
    Status { #[arg(long)] profile:Option<String> },
    /// Replace supplied rule lists; omitted lists stay unchanged.
    Config { #[arg(long)] profile:Option<String>, #[arg(long,conflicts_with="include_temporary")] exclude_temporary:bool,
        #[arg(long,conflicts_with="exclude_temporary")] include_temporary:bool,
        #[arg(long)] include:Vec<String>, #[arg(long)] exclude:Vec<String>,
        #[arg(long,conflicts_with="include")] clear_include:bool, #[arg(long,conflicts_with="exclude")] clear_exclude:bool },
    /// Remove a profile override and inherit global rules.
    Reset { profile:String },
    Test { profile:String, remote:String, #[arg(long)] directory:bool },
}
#[derive(Subcommand)]
enum PolicyCommand {
    Status { #[arg(long)] profile:Option<String> },
    Config { #[arg(long)] profile:Option<String>, #[arg(long,conflicts_with="disable_versions")] enable_versions:bool,
        #[arg(long,conflicts_with="enable_versions")] disable_versions:bool, #[arg(long)] keep:Option<usize>,
        #[arg(long,value_parser=["stop","keep-both","replace"])] conflicts:Option<String>,
        #[arg(long)] include:Vec<String>, #[arg(long)] exclude:Vec<String>,
        #[arg(long,conflicts_with="include")] clear_include:bool, #[arg(long,conflicts_with="exclude")] clear_exclude:bool },
    Reset { profile:String },
}
#[derive(Subcommand)]
enum ServiceCommand {
    Install { #[arg(long)] ipc_root:Option<PathBuf>, #[arg(long)] app:Option<PathBuf>,
        #[arg(long)] development_tcp:bool, #[arg(long)] launch_agents_dir:Option<PathBuf>, #[arg(long)] dry_run:bool },
    Start { #[arg(long)] dry_run:bool }, Stop { #[arg(long)] dry_run:bool },
    Restart { #[arg(long)] dry_run:bool }, Uninstall { #[arg(long)] dry_run:bool }, Status,
    /// Foreground supervisor used by the LaunchAgent. Stop with Ctrl-C.
    Run,
}
#[derive(Subcommand)]enum MetadataCommand {
    Capture{local:PathBuf},
    Apply{local:PathBuf,record:PathBuf,#[arg(long)]journal:Option<PathBuf>},
    Recover{local:PathBuf,journal:PathBuf},
    Push{profile:String,local:PathBuf,remote:String},
    Pull{profile:String,remote:String,local:PathBuf},
}
#[derive(Subcommand)]
enum ProfileCommand {
    Add {
        name: String,
        #[arg(long, value_enum)]
        protocol: Proto,
        #[arg(long)]
        url: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password_env: Option<String>,
        /// Reuse the matching Internet Password in macOS Keychain.
        #[arg(long, conflicts_with = "password_env")]
        keychain_internet: bool,
        #[arg(long)]
        allow_http: bool,
        /// Permit SMB without encryption (for trusted compatibility testing).
        #[arg(long)]
        allow_unencrypted_smb: bool,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Explicit Nextcloud v2 uploads URL, e.g. https://host/remote.php/dav/uploads/user/.
        #[arg(long)]
        nextcloud_upload_url: Option<String>,
    },
    List,
    Remove {
        name: String,
    },
}
#[derive(Subcommand)]
enum TransferCommand {
    List,
    /// Discard a download checkpoint; any prior-local recovery copy is retained.
    Discard {
        id: String,
    },
}
#[derive(Subcommand)]
enum CacheCommand {
    /// Preview eligible clean cache removal; --apply evicts only unpinned/inactive payloads.
    Clear { #[arg(long)] apply:bool },
    /// Show or safely migrate the Rust content cache (system Finder storage is separate).
    Directory { path:Option<PathBuf>, #[arg(long="default",conflicts_with="path")] restore_default:bool,
        #[arg(long)] dry_run:bool },
    /// Inspect the persistent clean-cache limit and actual usage, without networking.
    Status,
    /// Persist an aggregate clean-cache maximum (bytes); --unlimited removes it.
    Config { #[arg(long,conflicts_with="unlimited",required_unless_present="unlimited")] max_bytes:Option<u64>, #[arg(long)] unlimited:bool },
    /// Fetch files online; --pin protects copies from cache prune.
    Fetch {
        profile: String,
        remote: String,
        #[arg(long)]
        pin: bool,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Read an already cached file without contacting the server.
    Get {
        profile: String,
        remote: String,
        local: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    List {
        profile: Option<String>,
    },
    Unpin {
        profile: String,
        remote: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Evict least recently used unpinned files until below this size.
    Prune {
        #[arg(long, default_value_t = 0)]
        max_bytes: u64,
    },
}
#[derive(Subcommand)]
enum QueueCommand {
    List,
    Run {
        profile: Option<String>,
        #[arg(long)]
        retry_failed: bool,
        /// Explicitly retry an interrupted/failed unconditional overwrite.
        #[arg(long)]
        retry_ambiguous_overwrite: bool,
    },
    Cancel {
        id: String,
    },
}
#[derive(Subcommand)]
enum VersionsCommand {
    Save {
        profile: String,
        remote: String,
    },
    List {
        profile: String,
        remote: String,
    },
    Restore {
        profile: String,
        remote: String,
        id: String,
        #[arg(long)]
        overwrite: bool,
    },
    Prune {
        profile: String,
        remote: String,
        #[arg(long, default_value_t = 10)]
        keep: usize,
        #[arg(long)]
        apply: bool,
    },
}

fn emit(json: bool, value: &impl Serialize) -> Result<()> {
    emit_to(json, value, &mut std::io::stdout().lock())
}
fn emit_to(json: bool, value: &impl Serialize, output: &mut impl std::io::Write) -> Result<()> {
    let text = if json { serde_json::to_string(value)? } else { serde_json::to_string_pretty(value)? };
    writeln!(output, "{text}").context("cannot write command output")
}
fn path(p: &str) -> Result<String> {
    backend::normalize(p)
}
fn require_guarded_edit(conditional_write: bool) -> Result<()> {
    if !conditional_write {
        bail!("this backend cannot guard edits against remote changes; use get/put --overwrite explicitly");
    }
    Ok(())
}
fn profile(store: &Store, name: &str) -> Result<Profile> {
    configured_profile(&store.config, &store.home, name)
}
fn configured_profile(config: &config::Config, home: &Path, name: &str) -> Result<Profile> {
    config
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("unknown profile {name} in {}; available profiles: [{}]; run rdrive profile list",
            home.display(), config.profiles.keys().map(String::as_str).collect::<Vec<_>>().join(", ")))
}
fn smart_queue(store:&Store,id:&str)->Result<rdrive::smart::Queue>{
    let domain=rdrive::provider::domains(&store.home)?.into_iter().find(|domain|domain.id==id&&domain.smart).context("Smart domain not found")?;
    rdrive::smart::Queue::open(store,id,&rdrive::provider::journal_binding(&store.config,&domain)?)
}
async fn connect(store: &Store, name: &str) -> Result<Arc<dyn Backend>> {
    rdrive::factory::connect(&profile(store,name)?,name,rdrive::transfer::global()).await
}

async fn read_connection(home: &Path, name: &str) -> Result<Arc<dyn Backend>> {
    let config = rdrive::client_read::config(home)?;
    rdrive::factory::connect(&configured_profile(&config, home, name)?, name, rdrive::transfer::global()).await
}

/// Read-only remote operations use one configuration snapshot. They never open
/// Store's writer lock or recover its unrelated upload/cache journals.
async fn read_command(cli: &Cli, home: &Path) -> Result<Option<serde_json::Value>> {
    let value = match &cli.command {
        Command::Profile { command: ProfileCommand::List } => {
            serde_json::to_value(rdrive::client_read::config(home)?.profiles)?
        }
        Command::Info { profile } => {
            let backend = read_connection(home, profile).await?;
            let root = backend.stat("").await?;
            serde_json::json!({"capabilities":backend.capabilities(),"root":root,"diagnostics":backend.diagnostics()})
        }
        Command::Ls { profile, remote, recursive, contains } => {
            let remote = path(remote)?;
            let backend = read_connection(home, profile).await?;
            let mut entries = if *recursive { backend::walk(backend.as_ref(), &remote).await? }
                else { backend.list(&remote).await? };
            if let Some(filter) = contains { entries.retain(|entry| entry.path.contains(filter)); }
            serde_json::to_value(entries)?
        }
        Command::Stat { profile, remote } => {
            let remote = path(remote)?;
            let backend = read_connection(home, profile).await?;
            serde_json::to_value(backend.stat(&remote).await?)?
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}
fn sync_options(direction: SyncDirection, apply: bool, delete: bool) -> sync::SyncOptions {
    sync::SyncOptions {
        apply,
        delete,
        direction: match direction {
            SyncDirection::Push => sync::Direction::Push,
            SyncDirection::Pull => sync::Direction::Pull,
            SyncDirection::Both => sync::Direction::Both,
        },
    }
}
fn sync_roots(
    store: &Store,
    name: &str,
    local: &Path,
    remote: &str,
) -> Result<(PathBuf, String, PathBuf)> {
    let remote = path(remote)?;
    if std::fs::symlink_metadata(local)?.file_type().is_symlink() {
        bail!("sync root cannot be a symbolic link");
    }
    let local = local
        .canonicalize()
        .context("local sync directory must exist")?;
    for cache in rdrive::cache::protected_directories(&store.home,&store.config)? {
        if rdrive::cache::paths_overlap(&local,&cache)?{bail!("sync root overlaps the RDrive content cache; choose a separate data directory");}
    }
    let identity = format!(
        "{}\0{}\0{}\0{}\0{}",
        name,
        profile(store, name)?.url,
        profile(store, name)?.username.unwrap_or_default(),
        local.display(),
        remote
    );
    let baseline = store
        .home
        .join("baselines")
        .join(format!("{}.json", state::key(&identity)));
    Ok((local, remote, baseline))
}

async fn do_sync(
    store: &mut Store,
    json: bool,
    name: &str,
    local: &Path,
    remote: &str,
    options: sync::SyncOptions,
    metadata:bool,
) -> Result<()> {
    let backend = connect(store, name).await?;
    let (local, remote, baseline) = sync_roots(store, name, local, remote)?;
    let report = sync::run_with_metadata(backend.as_ref(), &local, &remote, &baseline, &options, metadata).await?;
    if metadata&&report.conflicts.is_empty(){
        let meta=rdrive::metadata_remote::sync(backend.as_ref(),&local,&remote,&baseline.with_extension("metadata.json"),&rdrive::metadata_remote::SyncOptions{apply:options.apply,direction:options.direction}).await?;
        emit(json,&serde_json::json!({"content":report,"metadata":meta}))?;
        if !meta.conflicts.is_empty(){bail!("metadata sync has conflicts; conflicting attributes were preserved");}
    }else{emit(json, &report)?;}
    if !report.conflicts.is_empty() {
        bail!("sync has conflicts; no changes were applied");
    }
    if options.apply {
        store.event("sync", name, &remote, "sync engine completed")?;
    }
    Ok(())
}

async fn execute(cli: &Cli, store: &mut Store) -> Result<()> {
    let json = cli.json;
    match &cli.command {
        Command::Mcp { .. } | Command::Service { .. } | Command::Update { .. } => unreachable!(),
        Command::Conflict { command } => match command {
            ConflictCommand::List { profile, local, remote } => {
                let (local, remote, baseline) = sync_roots(store, profile, local, remote)?;
                let backend = connect(store, profile).await?;
                let options = sync_options(SyncDirection::Both, false, false);
                let report = sync::run(backend.as_ref(), &local, &remote, &baseline, &options).await?;
                emit(json, &report.conflicts)?;
            }
            ConflictCommand::Repair {
                profile,
                local,
                remote,
                path,
                choose,
                expect,
                ..
            } => {
                let (local, remote, baseline) = sync_roots(store, profile, local, remote)?;
                let backend = connect(store, profile).await?;
                let choice = match choose {
                    ConflictSide::Local => sync::ConflictChoice::Local,
                    ConflictSide::Remote => sync::ConflictChoice::Remote,
                };
                let report = sync::repair_conflict(
                    backend.as_ref(),
                    &local,
                    &remote,
                    &baseline,
                    path,
                    choice,
                    expect.as_deref(),
                )
                .await?;
                emit(json, &report)?;
            }
        },
        Command::Filter { command } => match command {
            FilterCommand::Status {profile} => {
                if let Some(name)=profile { self_profile(store,name)?; }
                emit(json,&serde_json::json!({"rules":store.config.filters,"profile":profile,"override":profile.as_ref().and_then(|p|store.config.filters.profiles.get(p))}))?;
            },
            FilterCommand::Test {profile,remote,directory} => {
                self_profile(store,profile)?;
                emit(json,&serde_json::json!({"allowed":rdrive::filtering::allowed(&store.config,profile,&path(remote)?,*directory)?}))?;
            },
            FilterCommand::Reset {profile} => { self_profile(store,profile)?;store.config.filters.profiles.remove(profile);store.save_config()?;emit(json,&store.config.filters)?; },
            FilterCommand::Config {profile,exclude_temporary,include_temporary,include,exclude,clear_include,clear_exclude} => {
                let mut rules=store.config.filters.clone();
                let temporary=if *exclude_temporary {Some(true)}else if *include_temporary {Some(false)}else{None};
                if let Some(name)=profile {
                    self_profile(store,name)?;
                    let local=rules.profiles.entry(name.clone()).or_default();
                    if temporary.is_some(){local.temporary_files=temporary;}
                    if !include.is_empty()||*clear_include{local.include=Some(include.clone());}
                    if !exclude.is_empty()||*clear_exclude{local.exclude=Some(exclude.clone());}
                }else{
                    if let Some(value)=temporary{rules.temporary_files=value;}
                    if !include.is_empty()||*clear_include{rules.include=include.clone();}
                    if !exclude.is_empty()||*clear_exclude{rules.exclude=exclude.clone();}
                }
                rules.validate()?;store.config.filters=rules;store.save_config()?;emit(json,&store.config.filters)?;
            },
        },
        Command::Policy {command} => match command {
            PolicyCommand::Status{profile} => {
                if let Some(name)=profile{self_profile(store,name)?;emit(json,&store.config.policies.effective(name))?;}
                else{emit(json,&store.config.policies)?;}
            },
            PolicyCommand::Reset{profile} => {self_profile(store,profile)?;store.config.policies.profiles.remove(profile);store.save_config()?;emit(json,&store.config.policies)?;},
            PolicyCommand::Config{profile,enable_versions,disable_versions,keep,conflicts,include,exclude,clear_include,clear_exclude} => {
                let mut policies=store.config.policies.clone();
                if let Some(name)=profile{self_profile(store,name)?;}
                let mut versions=profile.as_deref().map(|p|policies.effective(p).versions).unwrap_or_else(||policies.versions.clone());
                if *enable_versions{versions.enabled=true;}if *disable_versions{versions.enabled=false;}
                if let Some(n)=keep{versions.keep=*n;}
                if !include.is_empty()||*clear_include{versions.include=include.clone();}
                if !exclude.is_empty()||*clear_exclude{versions.exclude=exclude.clone();}
                let strategy=conflicts.as_ref().map(|s|s.parse::<rdrive::policies::ConflictStrategy>()).transpose()?;
                if let Some(name)=profile{
                    let local=policies.profiles.entry(name.clone()).or_default();
                    if *enable_versions||*disable_versions||keep.is_some()||!include.is_empty()||!exclude.is_empty()||*clear_include||*clear_exclude{local.versions=Some(versions);}
                    if strategy.is_some(){local.conflicts=strategy;}
                }else{policies.versions=versions;if let Some(strategy)=strategy{policies.conflicts=strategy;}}
                policies.validate()?;store.config.policies=policies;store.save_config()?;emit(json,&store.config.policies)?;
            },
        },
        Command::Index { command } => match command {
            IndexCommand::Config { enable, disable, depth, all, refresh_seconds, active_refresh_seconds } => {
                let mut policy=store.config.index.clone();
                if *enable { policy.enabled=true; }
                if *disable { policy.enabled=false; }
                if let Some(depth)=depth { policy.prefetch_depth=Some(*depth); }
                if *all { policy.prefetch_depth=None; }
                if let Some(seconds)=refresh_seconds { policy.refresh_secs=*seconds; }
                if let Some(seconds)=active_refresh_seconds { policy.active_refresh_secs=*seconds; }
                policy.validate()?;
                store.config.index=policy;
                store.save_config()?;
                emit(json,&store.config.index)?;
            },
            IndexCommand::Status { .. } | IndexCommand::Refresh { .. } | IndexCommand::Search { .. } => unreachable!(),
        },
        Command::Repo{command}=>match command {
            RepoCommand::Init{profile:name,root}=>{let b=connect(store,name).await?;emit(json,&rdrive::repository::init(store,b.as_ref(),root).await?)?;},
            RepoCommand::Begin{profile:name,root}=>{let b=connect(store,name).await?;emit(json,&rdrive::repository::begin(store,b.as_ref(),name,root).await?)?;},
            RepoCommand::Put{transaction,local,path}=>emit(json,&rdrive::repository::put(store,transaction,local,path)?)?,
            RepoCommand::Mkdir{transaction,path}=>emit(json,&rdrive::repository::mkdir(store,transaction,path)?)?,
            RepoCommand::Rm{transaction,path,recursive}=>emit(json,&rdrive::repository::remove(store,transaction,path,*recursive)?)?,
            RepoCommand::Mv{transaction,from,to}=>emit(json,&rdrive::repository::rename(store,transaction,from,to)?)?,
            RepoCommand::Commit{transaction}=>{let tx=rdrive::repository::load(store,transaction)?;let b=connect(store,&tx.profile).await?;emit(json,&rdrive::repository::commit(store,b.as_ref(),transaction).await?)?;},
            RepoCommand::Status{transaction}=>emit(json,&rdrive::repository::load(store,transaction)?)?,
            RepoCommand::Abort{transaction}=>emit(json,&rdrive::repository::abort(store,transaction)?)?,
            RepoCommand::Transactions=>emit(json,&rdrive::repository::transactions(store)?)?,
            RepoCommand::Ls{profile:name,root}=>{let b=connect(store,name).await?;emit(json,&rdrive::repository::read(store,b.as_ref(),root).await?)?;},
            RepoCommand::Get{profile:name,path,local,root,overwrite}=>{let b=connect(store,name).await?;emit(json,&rdrive::repository::get(store,b.as_ref(),root,path,local,*overwrite).await?)?;},
        },
        Command::Provider{command}=>match command {
            ProviderCommand::Domains=>emit(json,&rdrive::provider::domains(&store.home)?)?,
            ProviderCommand::DomainAdd{id,profile,remote,repository,smart,local_root,display_name}=>{rdrive::provider::add_domain(store,rdrive::provider::Domain{id:id.clone(),profile:profile.clone(),remote:path(remote)?,repository:*repository,smart:*smart,local_root:local_root.clone(),display_name:display_name.clone().unwrap_or_else(||id.clone())})?;emit(json,&rdrive::provider::domains(&store.home)?)?;},
            ProviderCommand::DomainRemove{id}=>{rdrive::provider::remove_domain(store,id)?;emit(json,&serde_json::json!({"removed":id}))?;},
            ProviderCommand::Serve{..}|ProviderCommand::Native{..}=>unreachable!(),
        },
        Command::Smart{command}=>match command {
            SmartCommand::Setup{id,profile:name,remote,repository,display_name,register,app}=>{
                profile(store,name)?;
                if id.is_empty()||id.len()>120||id.chars().any(char::is_control)||display_name.as_ref().is_some_and(|name|name.is_empty()){bail!("invalid Smart domain identity or display name");}
                if rdrive::provider::domains(&store.home)?.iter().any(|domain|domain.id==*id){bail!("provider domain already exists");}
                let remote=path(remote)?;
                let companion=if *register {
                    if !cfg!(target_os="macos"){bail!("native registration is supported on macOS only");}
                    let bundle=app.clone().unwrap_or_else(||PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/build/RDrive.app"));
                    let executable=bundle.join("Contents/MacOS/RDriveCompanion");
                    if !executable.is_file(){bail!("native companion is not built; build macos/build.py or pass --app");}
                    Some(executable)
                }else{None};
                if store.config.cache.max_bytes.is_none(){rdrive::cache::set_limit(store,Some(5*1024*1024*1024))?;}
                let domain=rdrive::provider::Domain{id:id.clone(),profile:name.clone(),remote,repository:*repository,smart:true,local_root:None,display_name:display_name.clone().unwrap_or_else(||id.clone())};
                rdrive::provider::add_domain(store,domain.clone())?;
                let binding=rdrive::provider::journal_binding(&store.config,&domain)?;
                drop(rdrive::smart::Queue::open(store,id,&binding)?);
                let registration=if let Some(executable)=companion {
                    let output=std::process::Command::new(executable).args(["--register-domain",id,"--display-name",&domain.display_name]).output()?;
                    let value:serde_json::Value=serde_json::from_slice(&output.stdout).context("native companion returned an invalid registration response")?;
                    if !output.status.success()||value.get("ok").and_then(|value|value.as_bool())!=Some(true){bail!("Smart domain was saved, but native registration failed: {value}");}Some(value)
                }else{None};
                emit(json,&serde_json::json!({"configured_domain":domain,"native_registration":registration,"provider_service_started":false,"cache":rdrive::cache::status(store)?,"next_step":"Run provider serve with the companion's shared IPC socket and staging paths; register/enable its File Provider domain to show on-demand files in Finder."}))?;
            },
            SmartCommand::Resolve{..} | SmartCommand::RefreshConflict{..}=>unreachable!(),
            SmartCommand::Recovery{domain}=>emit(json,&rdrive::provider::namespace_recovery(&store.home,domain)?)?,
            SmartCommand::Status=>emit(json,&rdrive::provider::smart_summary(store)?)?,
            SmartCommand::List{domain}=>{
                let mut result=Vec::new();for configured in rdrive::provider::domains(&store.home)?.into_iter().filter(|configured|configured.smart&&domain.as_ref().is_none_or(|name|name==&configured.id)){
                    let binding=rdrive::provider::journal_binding(&store.config,&configured)?;result.push(serde_json::json!({"domain":configured.id,"mutations":rdrive::smart::read_only(&store.home,&configured.id,&binding)?}));
                }if domain.is_some()&&result.is_empty(){bail!("Smart domain not found");}emit(json,&result)?;
            },
            SmartCommand::Pause=>emit(json,&rdrive::smart::set_paused(store,true)?)?,
            SmartCommand::Resume=>emit(json,&rdrive::smart::set_paused(store,false)?)?,
            SmartCommand::Retry{domain,mutation}=>{
                let mut queue=smart_queue(store,domain)?;let ids=if let Some(id)=mutation{vec![id.clone()]}else{queue.list().into_iter().filter(|item|matches!(item.state,rdrive::smart::MutationState::Conflict|rdrive::smart::MutationState::Failed|rdrive::smart::MutationState::Interrupted)).map(|item|item.id).collect()};
                let mut retried=Vec::new();for id in ids{retried.push(queue.retry(&id)?);}emit(json,&serde_json::json!({"retried":retried,"remote_confirmation":false}))?;
            },
            SmartCommand::Cancel{domain,mutation}=>emit(json,&smart_queue(store,domain)?.cancel(mutation)?)?,
            SmartCommand::Discard{domain,mutation}=>{smart_queue(store,domain)?.discard_snapshot(mutation)?;emit(json,&serde_json::json!({"discarded_snapshot":mutation}))?;},
            SmartCommand::Export{domain,mutation,local,overwrite}=>{
                use std::{io::{Read,Seek,SeekFrom},os::unix::fs::OpenOptionsExt};
                let queue=smart_queue(store,domain)?;let lease=queue.snapshot(mutation)?.context("Smart mutation has no retained file contents")?;
                let temporary=ops::destination_temp(local)?;
                let result=(||->Result<()>{let mut source=lease.try_clone_file()?;source.seek(SeekFrom::Start(0))?;let mut output=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;std::io::copy(&mut source.by_ref(),&mut output)?;rdrive::metadata::apply_file(&output,&lease.info.metadata,&Default::default())?;output.sync_all()?;if state::fingerprint(&temporary)?!=lease.info.sha256{bail!("Smart recovery copy checksum mismatch");}state::commit_download(&temporary,local,*overwrite)?;Ok(())})();
                if result.is_err(){let _=std::fs::remove_file(&temporary);}result?;emit(json,&serde_json::json!({"exported":local,"sha256":lease.info.sha256,"bytes":lease.info.size,"remote_changed":false}))?;
            },
            SmartCommand::Run{..}=>unreachable!(),
        },
        Command::Metadata{command}=>match command {
            MetadataCommand::Capture{local}=>emit(json,&rdrive::metadata::capture(local,&Default::default())?)?,
            MetadataCommand::Apply{local,record,journal}=>{if std::fs::metadata(record)?.len()>64*1024*1024{bail!("metadata record exceeds 64 MiB");}let record:rdrive::metadata::MetadataRecord=serde_json::from_slice(&std::fs::read(record)?)?;let report=if let Some(path)=journal{rdrive::metadata::apply_with_recovery(local,&record,&Default::default(),path)?}else{rdrive::metadata::apply(local,&record,&Default::default())?};emit(json,&report)?;},
            MetadataCommand::Recover{local,journal}=>emit(json,&rdrive::metadata::recover(local,journal)?)?,
            MetadataCommand::Push{profile:name,local,remote}=>{let b=connect(store,name).await?;let p=path(remote)?;let e=b.stat(&p).await?.context("remote file missing")?;emit(json,&rdrive::metadata_remote::save(b.as_ref(),local,&p,&e,&Default::default()).await?)?;},
            MetadataCommand::Pull{profile:name,remote,local}=>{let b=connect(store,name).await?;let p=path(remote)?;let e=b.stat(&p).await?.context("remote file missing")?;emit(json,&rdrive::metadata_remote::apply(b.as_ref(),&p,&e,local,&Default::default()).await?)?;},
        },
        Command::Notify{..}=>unreachable!(),
        Command::Profile { command } => {
            match command {
                ProfileCommand::Add {
                    name,
                    protocol,
                    url,
                    username,
                    password_env,
                    keychain_internet,
                    allow_http,
                    allow_unencrypted_smb,
                    timeout,
                    nextcloud_upload_url,
                } => {
                    if name.is_empty()
                        || name.starts_with('-')
                        || !name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        bail!("profile name must contain letters, numbers, hyphen or underscore");
                    }
                    if store.config.profiles.contains_key(name) {
                        bail!("profile already exists; remove it before replacing its destination");
                    }
                    if password_env.as_ref().is_some_and(|s| {
                        s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    }) {
                        bail!("invalid credential environment variable name");
                    }
                    let p = Profile {
                        protocol: match protocol {
                            Proto::Webdav => Protocol::Webdav,
                            Proto::Smb => Protocol::Smb,
                        },
                        url: url.clone(),
                        username: username.clone(),
                        password_env: password_env.clone(),
                        keychain_internet: *keychain_internet,
                        allow_http: *allow_http,
                        timeout_secs: *timeout,
                        smb_encryption: !*allow_unencrypted_smb,
                        nextcloud_upload_url: nextcloud_upload_url.clone(),
                    };
                    config::validate_profile(&p)?;
                    store.config.profiles.insert(name.clone(), p);
                    store.save_config()?;
                    emit(json, &serde_json::json!({"profile":name,"saved":true}))?;
                }
                ProfileCommand::List => emit(json, &store.config.profiles)?,
                ProfileCommand::Remove { name } => {
                    if store.db.jobs.iter().any(|j| {
                        j.profile == *name && j.status != "done" && j.status != "cancelled"
                    }) {
                        bail!("profile has unfinished uploads; finish or cancel them first");
                    }
                    if store.db.cache.values().any(|c| c.profile == *name) {
                        bail!("profile has cached files; unpin and prune them first");
                    }
                    if store.config.profiles.remove(name).is_none() {
                        bail!("unknown profile");
                    }
                    store.save_config()?;
                    emit(json, &serde_json::json!({"removed":name}))?;
                }
            }
        }
        Command::Info { .. } | Command::Ls { .. } | Command::Stat { .. } => {
            emit(json, &read_command(cli, &store.home).await?)?;
        }
        Command::Get {
            profile: name,
            remote,
            local,
            overwrite,
            resume,
        } => {
            let b = connect(store, name).await?;
            let remote = path(remote)?;
            if *resume {
                emit(
                    json,
                    &rdrive::resume::get(store, b.as_ref(), name, &remote, local, *overwrite)
                        .await?,
                )?;
            } else {
                let e = ops::get(b.as_ref(), &remote, local, *overwrite).await?;
                store.event("download", name, &remote, "verified local commit")?;
                emit(json, &e)?;
            }
        }
        Command::Cat {
            profile: name,
            remote,
            max_bytes,
        } => {
            let b = connect(store, name).await?;
            let remote = path(remote)?;
            let temp = store
                .home
                .join("tmp")
                .join(uuid::Uuid::new_v4().to_string());
            ops::verified_download_limited(b.as_ref(), &remote, &temp, *max_bytes).await?;
            let result = async {
                let mut f = tokio::fs::File::open(&temp).await?;
                tokio::io::copy(&mut f, &mut tokio::io::stdout()).await?;
                Result::<()>::Ok(())
            }
            .await;
            let _ = std::fs::remove_file(temp);
            result?;
        }
        Command::Put {
            profile: name,
            local,
            remote,
            overwrite,
            version,
            queue,
            resume,
        } => {
            if *resume {
                let p = profile(store, name)?;
                if matches!(p.protocol, Protocol::Webdav) && p.nextcloud_upload_url.is_none() {
                    bail!("resumable WebDAV uploads require a profile with --nextcloud-upload-url; ordinary WebDAV supports resumable downloads");
                }
            }
            let job = ops::enqueue_with_resume(
                store, name, local, remote, *overwrite, None, *version, *resume,
            )?;
            if *queue {
                emit(json, &job)?;
            } else {
                let b = connect(store, name).await?;
                emit(json, &ops::run_job(store, b.as_ref(), &job.id).await?)?;
            }
        }
        Command::Mkdir {
            profile: name,
            remote,
            parents,
        } => {
            let b = connect(store, name).await?;
            let remote = path(remote)?;
            if *parents {
                backend::mkdir_all(b.as_ref(), &remote).await?;
            } else {
                b.mkdir(&remote).await?;
            }
            emit(json, &serde_json::json!({"created":remote}))?;
        }
        Command::Mv {
            profile: name,
            from,
            to,
            overwrite,
        }
        | Command::Cp {
            profile: name,
            from,
            to,
            overwrite,
        } => {
            let b = connect(store, name).await?;
            let from = path(from)?;
            let to = path(to)?;
            let op = if matches!(&cli.command, Command::Mv { .. }) {
                b.rename(&from, &to, *overwrite).await?;
                "move"
            } else {
                b.copy(&from, &to, *overwrite).await?;
                "copy"
            };
            store.event(op, name, &from, &to)?;
            emit(
                json,
                &serde_json::json!({"operation":op,"from":from,"to":to}),
            )?;
        }
        Command::Rm {
            profile: name,
            remote,
            recursive,
        } => {
            let remote = path(remote)?;
            if remote.is_empty() {
                bail!("cannot delete the remote root");
            }
            let b = connect(store, name).await?;
            b.remove(&remote, *recursive).await?;
            store.event("delete", name, &remote, "explicit remote deletion")?;
            emit(json, &serde_json::json!({"deleted":remote}))?;
        }
        Command::Url {
            profile: name,
            remote,
        } => {
            let p = profile(store, name)?;
            let remote = path(remote)?;
            let mut url = url::Url::parse(&p.url)?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("URL cannot contain path segments"))?;
                segments.pop_if_empty();
                for part in remote.split('/').filter(|s| !s.is_empty()) {
                    segments.push(part);
                }
            }
            emit(
                json,
                &serde_json::json!({"url":url.as_str(),"public_link":false}),
            )?;
        }
        Command::Edit {
            profile: name,
            remote,
        } => {
            let b = connect(store, name).await?;
            require_guarded_edit(b.capabilities().conditional_write)?;
            let remote = path(remote)?;
            let temp = store
                .home
                .join("tmp")
                .join(uuid::Uuid::new_v4().to_string());
            let before = ops::verified_download(b.as_ref(), &remote, &temp).await?;
            let hash = state::fingerprint(&temp)?;
            let editor = std::env::var_os("EDITOR").unwrap_or_else(|| "/usr/bin/vi".into());
            let status = std::process::Command::new(editor)
                .arg(&temp)
                .status()
                .context("EDITOR must name a single executable, without arguments")?;
            if !status.success() {
                bail!("editor failed; edited file retained at {}", temp.display());
            }
            if state::fingerprint(&temp)? == hash {
                std::fs::remove_file(temp)?;
                emit(json, &serde_json::json!({"changed":false}))?;
            } else {
                let job = ops::enqueue(store, name, &temp, &remote, false, Some(before), false)?;
                std::fs::remove_file(temp)?;
                emit(json, &ops::run_job(store, b.as_ref(), &job.id).await?)?;
            }
        }
        Command::Cache { command } => match command {
            CacheCommand::Clear{apply}=>emit(json,&rdrive::cache::clear(store,*apply)?)?,
            CacheCommand::Directory{path,restore_default,dry_run}=>{
                if path.is_none()&&!*restore_default {emit(json,&rdrive::cache::directory_status(store)?)?;}
                else if *dry_run {emit(json,&rdrive::cache::plan_directory(store,path.as_deref())?)?;}
                else {emit(json,&rdrive::cache::set_directory(store,path.as_deref())?)?;}
            },
            CacheCommand::Status => emit(json, &rdrive::cache::status(store)?)?,
            CacheCommand::Config {max_bytes,..} => emit(json, &rdrive::cache::set_limit(store,*max_bytes)?)?,
            CacheCommand::Fetch {
                profile: name,
                remote,
                pin,
                recursive,
            } => {
                let remote = path(remote)?;
                let b = connect(store, name).await?;
                let root = b.stat(&remote).await?.context("remote does not exist")?;
                let paths = if root.is_dir {
                    if !recursive {
                        bail!("directory cache requires --recursive");
                    }
                    backend::walk(b.as_ref(), &remote)
                        .await?
                        .into_iter()
                        .filter(|e| !e.is_dir)
                        .map(|e| e.path)
                        .collect()
                } else {
                    vec![remote]
                };
                let mut cached = Vec::new();
                for p in paths {
                    cached.push(ops::cache_file(store, b.as_ref(), name, &p, *pin).await?);
                }
                emit(json, &cached)?;
            }
            CacheCommand::Get {
                profile: name,
                remote,
                local,
                overwrite,
            } => {
                let item = ops::cache_get(store, name, &path(remote)?, local, *overwrite)?;
                emit(json, &item)?;
            }
            CacheCommand::List { profile: name } => emit(
                json,
                &store
                    .db
                    .cache
                    .values()
                    .filter(|c| name.as_ref().is_none_or(|p| p == &c.profile))
                    .collect::<Vec<_>>(),
            )?,
            CacheCommand::Unpin {
                profile: name,
                remote,
                recursive,
            } => {
                let remote = path(remote)?;
                let mut count = 0;
                for c in store.db.cache.values_mut() {
                    if c.profile == *name
                        && (c.remote == remote
                            || (*recursive
                                && (remote.is_empty()
                                    || c.remote.starts_with(&format!("{remote}/")))))
                    {
                        c.pinned = false;
                        count += 1;
                    }
                }
                store.save()?;
                emit(json, &serde_json::json!({"unpinned":count}))?;
            }
            CacheCommand::Prune { max_bytes } => {
                emit(json, &rdrive::cache::prune(store,*max_bytes)?)?;
            }
        },
        Command::Queue { command } => match command {
            QueueCommand::List => emit(json, &store.db.jobs)?,
            QueueCommand::Run {profile:name,retry_failed,retry_ambiguous_overwrite}=>{
                let report=rdrive::scheduler::run(store,name.as_deref(),cli.jobs,*retry_failed,*retry_ambiguous_overwrite,rdrive::transfer::global()).await?;
                emit(json,&report.results)?;
                if report.failed>0{bail!("{} upload(s) failed; snapshots retained",report.failed);}
            }
            QueueCommand::Cancel { id } => {
                let j = store
                    .db
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == *id)
                    .context("unknown job")?;
                if j.status == "done" {
                    bail!("job already completed");
                }
                j.status = "cancelled".into();
                let snapshot = j.snapshot.clone();
                store.save()?;
                let p = store.home.join("spool").join(snapshot);
                if p.exists() {
                    std::fs::remove_file(p)?;
                }
                let checkpoint = store.home.join("spool").join(format!("{id}.upload.json"));
                emit(
                    json,
                    &serde_json::json!({"cancelled":id,"upload_checkpoint_retained":checkpoint.exists().then_some(checkpoint)}),
                )?;
            }
        },
        Command::Sync {
            profile: name,
            local,
            remote,
            direction,
            apply,
            delete,
            metadata,
        } => {
            do_sync(
                store,
                json,
                name,
                local,
                remote,
                sync_options(*direction, *apply, *delete),
                *metadata,
            )
            .await?
        }
        Command::Watch { .. } => unreachable!(),
        Command::Versions { command } => match command {
            VersionsCommand::Save {
                profile: name,
                remote,
            } => {
                let b = connect(store, name).await?;
                emit(
                    json,
                    &ops::archive(store, b.as_ref(), &path(remote)?).await?,
                )?;
            }
            VersionsCommand::List {
                profile: name,
                remote,
            } => {
                let b = connect(store, name).await?;
                emit(
                    json,
                    &ops::versions(store, b.as_ref(), &path(remote)?).await?,
                )?;
            }
            VersionsCommand::Restore {
                profile: name,
                remote,
                id,
                overwrite,
            } => {
                if path(id)? != *id || id.contains('/') || id.is_empty() {
                    bail!("invalid version id");
                }
                let remote = path(remote)?;
                let b = connect(store, name).await?;
                let versions = ops::versions(store, b.as_ref(), &remote).await?;
                let version = versions
                    .iter()
                    .find(|v| v.id == *id)
                    .context("version not found")?;
                ops::restore(store, b.as_ref(), version, *overwrite).await?;
                store.event("restore", name, &remote, id)?;
                emit(json, &serde_json::json!({"restored":id,"path":remote}))?;
            }
            VersionsCommand::Prune {
                profile: name,
                remote,
                keep,
                apply,
            } => {
                let remote = path(remote)?;
                let b = connect(store, name).await?;
                let all = ops::versions(store, b.as_ref(), &remote).await?;
                let removed = all.into_iter().skip(*keep).collect::<Vec<_>>();
                if *apply {
                    for v in &removed {
                        b.remove(&format!("{}/{}", ops::version_root(&remote), v.id), true)
                            .await?;
                    }
                }
                emit(
                    json,
                    &serde_json::json!({"applied":apply,"versions":removed}),
                )?;
            }
        },
        Command::Lock {
            profile: name,
            remote,
            seconds,
        } => {
            if *seconds == 0 {
                bail!("lock timeout must be positive");
            }
            let b = connect(store, name).await?;
            emit(
                json,
                &serde_json::json!({"token":b.lock(&path(remote)?,*seconds).await?,"requested_seconds":seconds}),
            )?;
        }
        Command::Unlock {
            profile: name,
            remote,
        } => {
            let token = std::env::var("RDRIVE_WEBDAV_LOCK_TOKEN")
                .context("set RDRIVE_WEBDAV_LOCK_TOKEN")?;
            let b = connect(store, name).await?;
            b.unlock(&path(remote)?, &token).await?;
            emit(json, &serde_json::json!({"unlocked":remote}))?;
        }
        Command::Status => emit(
            json,
            &serde_json::json!({"state_directory":store.home,"profiles":store.config.profiles.len(),"queued":store.db.jobs.iter().filter(|j|j.status!="done"&&j.status!="cancelled").count(),"cache_files":store.db.cache.len(),"cache_bytes":store.db.cache.values().map(|c|c.entry.size).sum::<u64>(),"pinned":store.db.cache.values().filter(|c|c.pinned).count()}),
        )?,
        Command::Transfers { command } => match command {
            TransferCommand::List => emit(json, &rdrive::resume::list(store)?)?,
            TransferCommand::Discard { id } => emit(
                json,
                &serde_json::json!({"discarded":id,"previous_local_retained":rdrive::resume::discard(store,id)?}),
            )?,
        },
        Command::Mount { .. } | Command::Mounts | Command::Unmount { .. } => unreachable!(),
        Command::History { count } => emit(
            json,
            &store
                .db
                .history
                .iter()
                .rev()
                .take(usize::try_from(*count).unwrap_or(usize::MAX))
                .collect::<Vec<_>>(),
        )?,
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let context=match rdrive::transfer::TransferContext::new(cli.limit,match cli.progress{ProgressMode::Auto=>rdrive::transfer::ProgressOutput::Auto,ProgressMode::Off=>rdrive::transfer::ProgressOutput::Off,ProgressMode::Json=>rdrive::transfer::ProgressOutput::Json}){Ok(c)=>c,Err(e)=>{eprintln!("error: {e}");std::process::exit(2);}};
    let _=rdrive::transfer::install_global(context.clone());
    let reporter=context.reporter();
    let result=run(&cli).await;reporter.abort();
    if let Err(e) = result {
        if cli.json {
            eprintln!("{}", serde_json::json!({"error":format!("{e:#}")}));
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(1);
    }
}
async fn run(cli: &Cli) -> Result<()> {
    run_with_native(cli, &NativeCalls::default()).await
}

struct NativeCalls {
    plan: fn(&Profile, &std::path::Path, &rdrive::mount::MountOptions) -> Result<rdrive::mount::MountPlan>,
    mount: fn(&Profile, &std::path::Path, &rdrive::mount::MountOptions, Option<&str>) -> Result<rdrive::mount::MountInfo>,
    list: fn() -> Result<Vec<rdrive::mount::MountInfo>>,
    unmount: fn(&std::path::Path, bool) -> Result<rdrive::mount::MountInfo>,
    password: fn(&Profile, &str) -> Result<String>,
}
impl Default for NativeCalls {
    fn default() -> Self {
        Self { plan: rdrive::mount::plan, mount: rdrive::mount::mount, list: rdrive::mount::list, unmount: rdrive::mount::unmount, password: config::password }
    }
}

async fn run_with_native(cli: &Cli, native: &NativeCalls) -> Result<()> {
    if let Command::Update { apply } = &cli.command {
        return emit(cli.json, &rdrive::update::run(*apply).await?);
    }
    let home = rdrive::shared_home::resolve(cli.home.as_deref())?;
    if let Some(value) = read_command(cli, &home).await? { return emit(cli.json, &value); }
    match &cli.command {
        Command::Service {command} => {
            use rdrive::service::{self,ServiceAction};
            let value=match command {
                ServiceCommand::Run=>return service::run(&home).await,
                ServiceCommand::Status=>service::status(&home)?,
                ServiceCommand::Install{ipc_root,app,development_tcp,launch_agents_dir,dry_run}=>service::install(&service::ServiceOptions{home:home.clone(),executable:std::env::current_exe()?,ipc_root:ipc_root.clone().unwrap_or_else(||home.join("service/ipc")),app:app.clone(),development_tcp:*development_tcp,launch_agents_dir:launch_agents_dir.clone()},*dry_run)?,
                ServiceCommand::Start{dry_run}=>service::action(&home,ServiceAction::Start,*dry_run)?,
                ServiceCommand::Stop{dry_run}=>service::action(&home,ServiceAction::Stop,*dry_run)?,
                ServiceCommand::Restart{dry_run}=>service::action(&home,ServiceAction::Restart,*dry_run)?,
                ServiceCommand::Uninstall{dry_run}=>service::action(&home,ServiceAction::Uninstall,*dry_run)?,
            };return emit(cli.json,&value);
        },
        Command::Smart {command:SmartCommand::Resolve{domain,mutation,strategy}}=>return emit(cli.json,&rdrive::provider::resolve_smart(&home,domain,mutation,strategy).await?),
        Command::Smart {
            command: SmartCommand::RefreshConflict { domain, mutation },
        } => {
            let report = rdrive::provider::refresh_stale_smart_directory(&home, domain, mutation).await?;
            return emit(cli.json, &report);
        },
        Command::Index {command:IndexCommand::Search{domain,query,scope,kind,page_size,offset}}=>return emit(cli.json,&rdrive::provider::index_search(&home,domain,query,scope,kind.as_deref(),*page_size,*offset)?),
        Command::Index { command:IndexCommand::Status { domain } } => {
            return emit(cli.json,&rdrive::provider::index_status(&home,domain.as_deref())?);
        },
        Command::Index { command:IndexCommand::Refresh { domain,remote,recursive } } => {
            return emit(cli.json,&rdrive::provider::index_refresh(&home,domain,remote,*recursive)?);
        },
        Command::Mcp { command: McpCommand::Tools { allow_write } } => {
            return emit(true, &rdrive::mcp::tools(*allow_write));
        },
        Command::Mcp {
            command: McpCommand::Serve { allow_write, local_root, timeout },
        } => {
            return rdrive::mcp::serve(rdrive::mcp::Options {
                home,
                allow_write: *allow_write,
                local_roots: local_root.clone(),
                timeout_secs: *timeout,
                jobs: cli.jobs,
                limit: cli.limit,
            }).await;
        },
        Command::Smart{command:SmartCommand::Run{staging}}=>{
            let staging=staging.clone().unwrap_or_else(||home.join("SmartTransfers"));
            return emit(cli.json,&rdrive::provider::run_smart_once(&home,&staging).await?);
        },
        Command::Notify{profile:name,remote,recursive,count}=>{
            let store=Store::open(home.clone())?;let p=profile(&store,name)?;drop(store);
            let b=rdrive::factory::connect(&p,name,rdrive::transfer::global()).await?;
            let cancel=tokio_util::sync::CancellationToken::new();let mut events=b.change_feed(&path(remote)?,*recursive,cancel.clone()).await?;let mut n=0;
            loop{tokio::select!{_=tokio::signal::ctrl_c()=>break,event=events.recv()=>{match event{Some(event)=>{let event=event?;emit(true,&event)?;if event.kind!="ready"{n+=1;}if count.is_some_and(|limit|n>=limit){break;}},None=>break}}}}
            cancel.cancel();return Ok(());
        },

        Command::Provider{command:ProviderCommand::Serve{socket,staging,development_tcp}}=>return rdrive::provider::serve(home,socket.clone(),staging.clone(),*development_tcp).await,
        Command::Provider{command:ProviderCommand::Native{app,args}}=>{
            let app=app.clone().unwrap_or_else(||PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/build/RDrive.app"));
            let executable=app.join("Contents/MacOS/RDriveCompanion");
            if !executable.is_file(){bail!("native companion is not built; run python3 macos/build.py or specify --app");}
            if args.is_empty(){let child=std::process::Command::new(executable).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;return emit(cli.json,&serde_json::json!({"pid":child.id(),"app":app}));}
            let status=std::process::Command::new(executable).args(args).status()?;if !status.success(){bail!("native companion command failed");}return Ok(());
        },
        Command::Mounts => return emit(cli.json, &(native.list)()?),
        Command::Unmount { local, force } => {
            return emit(cli.json, &(native.unmount)(local, *force)?)
        }
        Command::Mount {
            profile: name,
            local,
            read_only,
            browse,
            no_prompt,
            dry_run,
        } => {
            let store = Store::open(home)?;
            let p = profile(&store, name)?;
            drop(store);
            let options = rdrive::mount::MountOptions {
                read_only: *read_only,
                no_browse: !*browse,
                no_prompt: *no_prompt,
            };
            if *dry_run {
                return emit(cli.json, &(native.plan)(&p, local, &options)?);
            }
            let credential = if matches!(p.protocol, Protocol::Webdav)
                && (p.username.is_some() || p.password_env.is_some() || p.keychain_internet)
            {
                match (native.password)(&p, name) {
                    Ok(value) => Some(value),
                    Err(e) if p.password_env.is_some() || p.keychain_internet => return Err(e),
                    Err(_) => None,
                }
            } else {
                None
            };
            return emit(
                cli.json,
                &(native.mount)(&p, local, &options, credential.as_deref())?,
            );
        }
        _ => (),
    }
    if let Command::Watch {
        profile,
        local,
        remote,
        direction,
        apply,
        delete,
        interval,
        metadata,
    } = &cli.command
    {
        if *interval == 0 {
            bail!("watch interval must be positive");
        }
        let cancel=tokio_util::sync::CancellationToken::new();
        let mut feed=None;
        loop {
            if feed.is_none(){
                let snapshot=Store::open(home.clone()).and_then(|s|s.config.profiles.get(profile).cloned().context("unknown profile"));
                if let Ok(p)=snapshot{if matches!(p.protocol,Protocol::Smb){if let Ok(b)=rdrive::factory::connect(&p,profile,rdrive::transfer::global()).await{feed=b.change_feed(&path(remote)?,true,cancel.clone()).await.ok();}}}
            }
            // Release the state lock between polls so status/queue commands work.
            let result = async {
                let mut store = Store::open(home.clone())?;
                do_sync(
                    &mut store,
                    cli.json,
                    profile,
                    local,
                    remote,
                    sync_options(*direction, *apply, *delete),
                    *metadata,
                )
                .await
            }
            .await;
            if let Err(e) = result {
                if cli.json {
                    eprintln!("{}", serde_json::json!({"watch_error":format!("{e:#}")}));
                } else {
                    eprintln!("watch: {e:#}");
                }
            }
            tokio::select! { _=tokio::signal::ctrl_c()=>{cancel.cancel();return Ok(());}, _=tokio::time::sleep(std::time::Duration::from_secs(*interval))=>{}, _=notification(&mut feed)=>{} }
        }
    } else {
        let mut store = Store::open(home)?;
        execute(cli, &mut store).await
    }
}

async fn notification(feed:&mut Option<tokio::sync::mpsc::Receiver<anyhow::Result<rdrive::backend::ChangeEvent>>>) {
    if let Some(events)=feed.as_mut(){loop{match events.recv().await{Some(Ok(event))if event.kind=="ready"=>continue,Some(_)=>{tokio::time::sleep(std::time::Duration::from_millis(150)).await;while events.try_recv().is_ok(){}return;},None=>{*feed=None;return;}}}}
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn history_count_and_transfer_limit_parse_without_type_collision() {
        let cli = Cli::try_parse_from(["rdrive", "history", "--limit", "3"]).unwrap();
        assert!(matches!(cli.command, Command::History { count: 3 }));
        let cli = Cli::try_parse_from(["rdrive", "--limit", "1024", "status"]).unwrap();
        assert_eq!(cli.limit, Some(1024));
    }

    #[test]
    fn bounded_cat_keeps_ordinary_cat_and_explicit_transfer_limit() {
        let cli = Cli::try_parse_from(["rdrive", "--limit", "1024", "cat", "dav", "note.txt", "--max-bytes", "4096"]).unwrap();
        assert_eq!(cli.limit, Some(1024));
        assert!(matches!(cli.command, Command::Cat { max_bytes: Some(4096), .. }));
        let cli = Cli::try_parse_from(["rdrive", "cat", "dav", "note.txt"]).unwrap();
        assert!(matches!(cli.command, Command::Cat { max_bytes: None, .. }));
    }
}

fn self_profile(store:&Store,name:&str)->Result<()> { profile(store,name).map(|_|()) }

#[cfg(test)]
mod coverage_cli_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cli(home: &Path, args: &[&str]) -> Cli {
        let mut input = vec!["rdrive".to_owned(), "--json".into(), "--home".into(), home.to_string_lossy().into_owned()];
        input.extend(args.iter().map(|value| (*value).to_owned()));
        Cli::try_parse_from(input).unwrap()
    }

    #[tokio::test]
    async fn local_commands_persist_profiles_metadata_and_domain_intent() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        for args in [vec!["profile","add","dav","--protocol","webdav","--url","https://example.invalid/dav/"],
                     vec!["profile","add","smb","--protocol","smb","--url","smb://example.invalid/share"]] {
            execute(&cli(&home,&args), &mut store).await.unwrap();
        }
        assert_eq!(store.config.profiles.len(), 2);
        assert!(execute(&cli(&home,&["profile","add","dav","--protocol","webdav","--url","https://example.invalid/"]),&mut store).await.is_err());
        for name in ["", "bad!", "-bad"] {
            let mut input = cli(&home,&["profile","add","new","--protocol","webdav","--url","https://example.invalid/"]);
            if let Command::Profile {command:ProfileCommand::Add{name: value,..}} = &mut input.command { *value = name.into(); }
            assert!(execute(&input,&mut store).await.is_err());
        }
        for variable in ["", "bad-name"] {
            let input = cli(&home,&["profile","add","new","--protocol","webdav","--url","https://example.invalid/","--password-env",variable]);
            assert!(execute(&input,&mut store).await.is_err());
        }
        execute(&cli(&home,&["profile","remove","smb"]),&mut store).await.unwrap();
        assert!(!store.config.profiles.contains_key("smb"));
        assert!(execute(&cli(&home,&["profile","remove","missing"]),&mut store).await.is_err());
        let local = temp.path().join("data"); std::fs::write(&local,b"same contents").unwrap();
        let metadata = temp.path().join("metadata.json");
        state::atomic_json(&metadata,&rdrive::metadata::capture(&local,&Default::default()).unwrap()).unwrap();
        execute(&cli(&home,&["metadata","apply",local.to_str().unwrap(),metadata.to_str().unwrap()]),&mut store).await.unwrap();
        let recovery = temp.path().join("recovery");
        std::fs::create_dir(&recovery).unwrap();
        std::fs::set_permissions(&recovery,std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = recovery.join("recovery.json");
        execute(&cli(&home,&["metadata","apply",local.to_str().unwrap(),metadata.to_str().unwrap(),"--journal",journal.to_str().unwrap()]),&mut store).await.unwrap();
        assert!(execute(&cli(&home,&["metadata","recover",local.to_str().unwrap(),journal.to_str().unwrap()]),&mut store).await.is_err());
        let oversized = temp.path().join("oversized.json");
        std::fs::File::create(&oversized).unwrap().set_len(64*1024*1024+1).unwrap();
        assert!(execute(&cli(&home,&["metadata","apply",local.to_str().unwrap(),oversized.to_str().unwrap()]),&mut store).await.is_err());
        for args in [vec!["index","config","--disable"],vec!["index","config","--enable","--depth","1"],vec!["index","config","--all","--refresh-seconds","12","--active-refresh-seconds","3"],
                     vec!["provider","domain-add","ordinary","dav"],vec!["provider","domains"],vec!["provider","domain-remove","ordinary"],
                     vec!["smart","setup","smart-one","dav"],vec!["smart","list","smart-one"],vec!["smart","retry","smart-one"],vec!["smart","recovery","smart-one"],vec!["smart","status"],
                     vec!["transfers","list"],vec!["cache","list","dav"],vec!["cache","unpin","dav","missing","--recursive"],vec!["history","--limit","1"]] {
            execute(&cli(&home,&args),&mut store).await.unwrap();
        }
        let domain=rdrive::provider::domains(&home).unwrap().into_iter().find(|value|value.id=="smart-one").unwrap();
        let binding=rdrive::provider::journal_binding(&store.config,&domain).unwrap();
        let mutation={let mut queue=rdrive::smart::Queue::open(&store,"smart-one",&binding).unwrap();
            let mutation=queue.enqueue(serde_json::json!({"v":1,"op":"modify","domain":"smart-one","id":"file","request_id":"cli-recovery","base_content_version":"c","base_metadata_version":"m"}),Some(&std::fs::File::open(&local).unwrap())).unwrap();
            queue.start(&mutation.id).unwrap();queue.fail(&mutation.id,rdrive::smart::FailureKind::Other,"Synthetic failure").unwrap();mutation};
        execute(&cli(&home,&["smart","retry","smart-one"]),&mut store).await.unwrap();
        execute(&cli(&home,&["smart","retry","smart-one",&mutation.id]),&mut store).await.unwrap();
        let recovered=temp.path().join("exported");
        execute(&cli(&home,&["smart","export","smart-one",&mutation.id,recovered.to_str().unwrap()]),&mut store).await.unwrap();
        assert_eq!(std::fs::read(recovered).unwrap(),b"same contents");
        execute(&cli(&home,&["smart","cancel","smart-one",&mutation.id]),&mut store).await.unwrap();
        execute(&cli(&home,&["smart","discard","smart-one",&mutation.id]),&mut store).await.unwrap();
        assert!(execute(&cli(&home,&["smart","export","smart-one",&mutation.id,temp.path().join("missing-snapshot").to_str().unwrap()]),&mut store).await.is_err());
        let upload=ops::enqueue(&mut store,"dav",&local,"upload",false,None,false).unwrap();
        assert!(execute(&cli(&home,&["profile","remove","dav"]),&mut store).await.is_err());
        execute(&cli(&home,&["queue","cancel",&upload.id]),&mut store).await.unwrap();
        assert!(!home.join("spool").join(upload.snapshot).exists());
        execute(&cli(&home,&["queue","cancel",&upload.id]),&mut store).await.unwrap();
        store.db.jobs[0].status="done".into();
        assert!(execute(&cli(&home,&["queue","cancel",&upload.id]),&mut store).await.is_err());
        store.db.cache.insert("synthetic".into(),state::CacheItem{profile:"dav".into(),remote:"cached".into(),object:"not-a-real-payload".into(),entry:rdrive::backend::Entry{path:"cached".into(),is_dir:false,size:0,modified:None,etag:None},sha256:String::new(),pinned:false,accessed:state::now(),scope:None});
        assert!(execute(&cli(&home,&["profile","remove","dav"]),&mut store).await.is_err());
        store.db.cache.clear();
        assert!(execute(&cli(&home,&["smart","setup","smart-one","dav"]),&mut store).await.is_err());
        assert!(execute(&cli(&home,&["smart","list","missing"]),&mut store).await.is_err());
        assert!(execute(&cli(&home,&["smart","setup","invalid","dav","--display-name",""]),&mut store).await.is_err());
        assert!(execute(&cli(&home,&["lock","dav","file","--seconds","0"]),&mut store).await.is_err());
        assert!(execute(&cli(&home,&["transfers","discard","invalid"]),&mut store).await.is_err());
    }

    #[tokio::test]
    async fn fake_companion_is_scoped_to_test_bundle_and_never_registers_a_real_domain() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("state");
        let bundle = temp.path().join("Synthetic.app");
        let executable = bundle.join("Contents/MacOS/RDriveCompanion");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fn install(executable:&Path,body:&str) {std::fs::write(executable,format!("#!/bin/sh\n{body}\n")).unwrap();std::fs::set_permissions(executable,std::fs::Permissions::from_mode(0o700)).unwrap();}
        install(&executable,"printf '{\"ok\":true}'");
        let mut store = Store::open(home.clone()).unwrap();
        execute(&cli(&home,&["profile","add","dav","--protocol","webdav","--url","https://example.invalid/"]),&mut store).await.unwrap();
        execute(&cli(&home,&["smart","setup","registered-fake","dav","--register","--app",bundle.to_str().unwrap()]),&mut store).await.unwrap();
        install(&executable,"printf '{\"ok\":false}'; exit 1");
        assert!(execute(&cli(&home,&["smart","setup","failed-fake","dav","--register","--app",bundle.to_str().unwrap()]),&mut store).await.is_err());
        install(&executable,"printf 'invalid-json'");
        assert!(execute(&cli(&home,&["smart","setup","invalid-fake","dav","--register","--app",bundle.to_str().unwrap()]),&mut store).await.is_err());
        drop(store);
        install(&executable,"exit 0");
        run(&cli(&home,&["provider","native","--app",bundle.to_str().unwrap(),"--","--synthetic"])).await.unwrap();
        install(&executable,"exit 1");
        assert!(run(&cli(&home,&["provider","native","--app",bundle.to_str().unwrap(),"--","--synthetic"])).await.is_err());
        assert!(run(&cli(&home,&["provider","native","--app",temp.path().join("missing.app").to_str().unwrap()])).await.is_err());
        assert!(run(&cli(&home,&["watch","dav",temp.path().to_str().unwrap(),"--interval","0"])).await.is_err());
    }

    #[tokio::test]
    async fn notification_coalesces_changes_and_preserves_disconnected_state() {
        let (sender,receiver) = tokio::sync::mpsc::channel(8);
        let mut feed = Some(receiver);
        sender.send(Ok(rdrive::backend::ChangeEvent {kind:"ready".into(),path:String::new()})).await.unwrap();
        sender.send(Ok(rdrive::backend::ChangeEvent {kind:"modified".into(),path:"a".into()})).await.unwrap();
        sender.send(Err(anyhow::anyhow!("synthetic notification failure"))).await.unwrap();
        notification(&mut feed).await;
        assert!(feed.as_mut().unwrap().try_recv().is_err());
        drop(sender);
        notification(&mut feed).await;
        assert!(feed.is_none());
        assert!(tokio::time::timeout(std::time::Duration::from_millis(1), notification(&mut feed)).await.is_err());
    }

    #[tokio::test]
    async fn synchronous_dispatch_rejects_commands_owned_by_the_outer_runner() {
        for args in [vec!["mcp","tools"],vec!["service","status"],vec!["index","status"],vec!["provider","serve","--socket","/unused","--staging","/unused"],vec!["smart","run"],vec!["smart","resolve","d","m","--strategy","remote"],vec!["notify","d"],vec!["watch","d","/unused"],vec!["mounts"]] {
            let temp = tempfile::tempdir().unwrap();
            let home=temp.path().join("state");let input=cli(&home,&args);
            let task=tokio::spawn(async move {let mut store=Store::open(home).unwrap();execute(&input,&mut store).await});
            assert!(task.await.unwrap_err().is_panic(),"outer-runner command reached synchronous dispatcher without rejecting it");
        }
    }

    #[tokio::test]
    async fn service_previews_and_missing_state_fail_before_any_system_action() {
        let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
        let home = temporary.path().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        store.config.profiles.insert("smb".into(), serde_json::from_value(serde_json::json!({"protocol":"smb","url":"smb://example.invalid/share"})).unwrap());
        store.save_config().unwrap();
        // An absent source fails before connecting. SMB must pass the resume
        // protocol guard used to reject unconfigured WebDAV chunk uploads.
        let missing = temporary.path().join("absent-source");
        assert!(execute(&cli(&home, &["put", "smb", missing.to_str().unwrap(), "file", "--resume"]), &mut store).await.is_err());
        assert!(execute(&cli(&home, &["versions", "restore", "smb", "file", "nested/version"]), &mut store).await.unwrap_err().to_string().contains("invalid version id"));
        drop(store);
        run(&cli(&home, &["service", "status"])).await.unwrap();
        run(&cli(&home, &["service", "install", "--dry-run"])).await.unwrap();
        let ipc = temporary.path().join("ipc");
        let agents = temporary.path().join("agents");
        run(&cli(&home, &["service", "install", "--ipc-root", ipc.to_str().unwrap(), "--launch-agents-dir", agents.to_str().unwrap(), "--dry-run"])).await.unwrap();
        for operation in ["start", "stop", "restart", "uninstall"] {
            assert!(run(&cli(&home, &["service", operation, "--dry-run"])).await.is_err());
        }
        assert!(run(&cli(&home, &["service", "run"])).await.is_err());
        assert!(!home.join("service").exists());
        assert!(!ipc.exists());
        assert!(!agents.exists());
        assert!(run(&cli(&home, &["index", "search", "missing", "query"])).await.is_err());
        assert!(run(&cli(&home, &["smart", "resolve", "missing", "mutation", "--strategy", "remote"])).await.is_err());
        run(&cli(&home, &["mcp", "tools"])).await.unwrap();
    }

    #[tokio::test]
    async fn global_filters_profile_overrides_and_reset_preserve_declared_rules() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        execute(&cli(&home, &["profile", "add", "dav", "--protocol", "webdav", "--url", "https://example.invalid/"]), &mut store).await.unwrap();
        execute(&cli(&home, &["filter", "config", "--exclude-temporary", "--include", "*.txt", "--exclude", "private*"]), &mut store).await.unwrap();
        assert!(rdrive::filtering::allowed(&store.config, "dav", "notes.txt", false).unwrap());
        assert!(!rdrive::filtering::allowed(&store.config, "dav", "private-notes.txt", false).unwrap());
        execute(&cli(&home, &["filter", "test", "dav", "notes.txt"]), &mut store).await.unwrap();
        execute(&cli(&home, &["filter", "test", "dav", "folder", "--directory"]), &mut store).await.unwrap();
        execute(&cli(&home, &["filter", "config", "--profile", "dav", "--include-temporary", "--include", "*"]), &mut store).await.unwrap();
        assert!(rdrive::filtering::allowed(&store.config, "dav", "scratch.tmp", false).unwrap());
        execute(&cli(&home, &["filter", "reset", "dav"]), &mut store).await.unwrap();
        assert!(!store.config.filters.profiles.contains_key("dav"));
        assert!(!rdrive::filtering::allowed(&store.config, "dav", "scratch.tmp", false).unwrap());
        execute(&cli(&home, &["filter", "config", "--include-temporary", "--clear-include", "--clear-exclude"]), &mut store).await.unwrap();
        assert!(rdrive::filtering::allowed(&store.config, "dav", "scratch.tmp", false).unwrap());
        let original = std::fs::read(home.join("config.json")).unwrap();
        assert!(execute(&cli(&home, &["filter", "config", "--include", "../escape"]), &mut store).await.is_err());
        assert_eq!(std::fs::read(home.join("config.json")).unwrap(), original);
        assert!(matches!(sync_options(SyncDirection::Push, false, false).direction, sync::Direction::Push));
        assert!(matches!(sync_options(SyncDirection::Pull, false, false).direction, sync::Direction::Pull));
    }
}

#[cfg(test)]
mod cli_contract {
    use super::*;
    use clap::CommandFactory;

    fn value(arg: &clap::Arg) -> String {
        if let Some(values) = arg.get_value_parser().possible_values() {
            return values.into_iter().next().unwrap().get_name().into();
        }
        let kind = arg.get_value_parser().type_id();
        if kind == std::any::TypeId::of::<u64>() || kind == std::any::TypeId::of::<u32>() || kind == std::any::TypeId::of::<usize>() { return "2".into(); }
        if arg.get_id() == "strategy" { return "stop".into(); }
        "fixture".into()
    }
    fn append(args: &mut Vec<String>, arg: &clap::Arg) {
        if let Some(long) = arg.get_long() { args.push(format!("--{long}")); }
        else if let Some(short) = arg.get_short() { args.push(format!("-{short}")); }
        if arg.get_action().takes_values() { args.push(value(arg)); }
    }
    fn visit(command: &clap::Command, path: &[String], cases: &mut Vec<serde_json::Value>) {
        let children = command.get_subcommands().filter(|command| command.get_name() != "help").collect::<Vec<_>>();
        if !children.is_empty() {
            for child in children { let mut path=path.to_vec(); path.push(child.get_name().into()); visit(child, &path, cases); }
            return;
        }
        let args = command.get_arguments().filter(|arg| !["help", "version"].contains(&arg.get_id().as_str())).collect::<Vec<_>>();
        let mut required = args.iter().filter(|arg| arg.is_required_set()).copied().collect::<Vec<_>>();
        required.sort_by_key(|arg| arg.get_index().unwrap_or(0));
        let mut base = vec!["rdrive".into()]; base.extend(path.to_vec());
        for arg in &required { append(&mut base, arg); }
        let cache_config=path==["cache", "config"];
        if cache_config { base.push("--unlimited".into()); }
        Cli::try_parse_from(&base).unwrap_or_else(|error| panic!("baseline {base:?}: {error}"));
        for arg in args {
            let mut invocation = base.clone();
            if cache_config && arg.get_id()=="max_bytes" { invocation.retain(|argument|argument!="--unlimited"); }
            if arg.get_id()=="app" && command.get_arguments().any(|argument|argument.get_id()=="register") { invocation.push("--register".into()); }
            if path == ["conflict", "repair"] {
                match arg.get_id().as_str() {
                    "apply" => invocation.extend(["--expect".into(), "fixture-token".into()]),
                    "expect" => invocation.push("--apply".into()),
                    _ => {}
                }
            }
            if !arg.is_required_set() && !(cache_config && arg.get_id()=="unlimited") { append(&mut invocation, arg); }
            Cli::try_parse_from(&invocation).unwrap_or_else(|error| panic!("parameter {invocation:?}: {error}"));
            let mut values = 1;
            if arg.get_action().takes_values() {
                if let Some(possible) = arg.get_value_parser().possible_values() {
                    for possible in possible {
                        let mut choice=invocation.clone();
                        let index=arg.get_long().map(|long|choice.iter().position(|argument|argument==&format!("--{long}")).unwrap()+1).unwrap_or(choice.len()-1);
                        choice[index]=possible.get_name().into();
                        Cli::try_parse_from(&choice).unwrap_or_else(|error| panic!("enum {choice:?}: {error}")); values+=1;
                    }
                }
                let kind=arg.get_value_parser().type_id();
                if !arg.is_required_set() && (kind==std::any::TypeId::of::<u64>() || kind==std::any::TypeId::of::<u32>() || kind==std::any::TypeId::of::<usize>()) {
                    let mut invalid=invocation.clone(); *invalid.last_mut().unwrap()="not-an-integer".into();
                    assert!(Cli::try_parse_from(&invalid).is_err(), "numeric parameter accepted text: {invalid:?}");
                }
            }
            for conflicting in command.get_arg_conflicts_with(arg) {
                let mut invalid=invocation.clone(); append(&mut invalid,conflicting);
                assert!(Cli::try_parse_from(&invalid).is_err(),"conflicting arguments were accepted: {invalid:?}");
            }
            cases.push(serde_json::json!({"command":path.join(" "),"argument":arg.get_id().as_str(),"accepted_value_cases":values,"required":arg.is_required_set()}));
        }
    }
    #[test]
    fn every_leaf_and_declared_parameter_can_be_parsed_with_typed_values() {
        let mut command=Cli::command(); command.build(); let mut cases=Vec::new(); visit(&command, &[], &mut cases);
        let leaves=cases.iter().map(|case| case["command"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(leaves.len(),101,"update the public interface inventory when adding a command");
        assert!(cases.len()>600,"inherited and positional parameters must remain in the inventory");
        let directory=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/validation"); std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("cli-parameter-cases.json"),serde_json::to_vec_pretty(&serde_json::json!({"leaf_commands":leaves.len(),"parameter_cases":cases})).unwrap()).unwrap();
    }
    #[test]
    fn output_serialization_and_broken_writers_return_errors() {
        assert!(require_guarded_edit(true).is_ok());
        assert!(require_guarded_edit(false).unwrap_err().to_string().contains("cannot guard edits"));
        let value=serde_json::json!({"value":1}); let mut compact=Vec::new(); let mut pretty=Vec::new();
        emit_to(true,&value,&mut compact).unwrap(); emit_to(false,&value,&mut pretty).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&compact).unwrap(),value); assert!(pretty.len()>compact.len());
        struct Broken;
        impl std::io::Write for Broken { fn write(&mut self,_:&[u8])->std::io::Result<usize>{Err(std::io::ErrorKind::BrokenPipe.into())} fn flush(&mut self)->std::io::Result<()>{Ok(())} }
        assert!(emit_to(true,&value,&mut Broken).unwrap_err().to_string().contains("cannot write command output"));
        struct Invalid;
        impl Serialize for Invalid { fn serialize<S:serde::Serializer>(&self,_:S)->std::result::Result<S::Ok,S::Error>{Err(serde::ser::Error::custom("synthetic invalid value"))} }
        assert!(emit_to(true,&Invalid,&mut Vec::new()).is_err()); assert!(emit_to(false,&Invalid,&mut Vec::new()).is_err());
    }
    fn mounted(path: &std::path::Path) -> rdrive::mount::MountInfo {
        rdrive::mount::MountInfo { mountpoint:path.into(),source:"synthetic://fixture/share".into(),filesystem:"webdav".into(),read_only:true,no_browse:false,owner:unsafe{libc::geteuid()},fsid:[1,2] }
    }
    #[tokio::test]
    async fn native_dispatch_uses_explicit_credential_policy_without_os_mutations() {
        let temp=tempfile::tempdir().unwrap(); let home=temp.path().join("state"); let point=temp.path().join("mount"); std::fs::create_dir(&point).unwrap();
        let mut store=Store::open(home.clone()).unwrap();
        for (name,protocol,username,environment) in [
            ("credential",Protocol::Webdav,Some("credential"),Some("MOCKED_ENV")),
            ("env-failure",Protocol::Webdav,Some("env-failure"),Some("MOCKED_ENV")),
            ("keychain-failure",Protocol::Webdav,Some("keychain-failure"),None),
            ("anonymous",Protocol::Webdav,None,None),
            ("smb",Protocol::Smb,Some("smb"),None),
        ] {
            let url=if matches!(protocol,Protocol::Smb){"smb://fixture/share"}else{"https://fixture.invalid/root/"};
            store.config.profiles.insert(name.into(),Profile{protocol,url:url.into(),username:username.map(str::to_owned),password_env:environment.map(str::to_owned),keychain_internet:false,allow_http:false,timeout_secs:5,smb_encryption:true,nextcloud_upload_url:None});
        }
        let mut internet = store.config.profiles["credential"].clone();
        internet.password_env = None;
        internet.keychain_internet = true;
        store.config.profiles.insert("internet".into(), internet.clone());
        internet.username = None;
        store.config.profiles.insert("internet-failure".into(), internet);
        store.save_config().unwrap(); drop(store);
        let native=NativeCalls {
            password: |_,name| if name.ends_with("failure"){bail!("synthetic credential lookup failed")}else{Ok("mock credential".into())},
            mount: |profile,path,options,credential| {
                assert!(options.read_only&&options.no_prompt&&!options.no_browse);
                assert_eq!(credential,if profile.username.as_deref()==Some("credential"){Some("mock credential")}else{None});
                std::fs::write(path.join("observed"),profile.username.as_deref().unwrap_or("anonymous"))?;
                Ok(mounted(path))
            },
            plan: |_,point,options| Ok(rdrive::mount::MountPlan{program:"synthetic-helper".into(),args:vec![],mountpoint:point.into(),source:"synthetic://fixture/share".into(),filesystem:"webdav".into(),credential_mode:"mock".into(),read_only:options.read_only,encryption_required:true,notes:vec![]}),
            list: || Ok(vec![mounted(std::path::Path::new("/synthetic/fixture"))]),
            unmount: |path,force| { assert!(force); Ok(mounted(path)) },
        };
        for name in ["credential","internet","keychain-failure","anonymous","smb"] {
            let args: Vec<String>=vec!["rdrive".into(),"--home".into(),home.to_string_lossy().into(),"--json".into(),"mount".into(),name.into(),point.to_string_lossy().into(),"--read-only".into(),"--browse".into(),"--no-prompt".into()];
            run_with_native(&Cli::try_parse_from(args).unwrap(),&native).await.unwrap();
            assert_eq!(std::fs::read_to_string(point.join("observed")).unwrap(),if name=="internet"{"credential"}else{name});
        }
        let input=Cli::try_parse_from(["rdrive","--home",home.to_str().unwrap(),"mount","env-failure",point.to_str().unwrap(),"--no-prompt"]).unwrap();
        assert!(run_with_native(&input,&native).await.unwrap_err().to_string().contains("synthetic credential"));
        let input=Cli::try_parse_from(["rdrive","--home",home.to_str().unwrap(),"mount","internet-failure",point.to_str().unwrap(),"--no-prompt"]).unwrap();
        assert!(run_with_native(&input,&native).await.unwrap_err().to_string().contains("synthetic credential"));
        for args in [vec!["mounts"],vec!["unmount",point.to_str().unwrap(),"--force"],vec!["mount","credential",point.to_str().unwrap(),"--dry-run"]] {
            let mut arguments=vec!["rdrive","--home",home.to_str().unwrap(),"--json"]; arguments.extend(args);
            run_with_native(&Cli::try_parse_from(arguments).unwrap(),&native).await.unwrap();
        }
    }
}

#[cfg(test)]
mod readonly_connection_tests {
    use super::*;
    use std::{io::{Read, Write}, net::{TcpListener, TcpStream}, sync::{atomic::{AtomicBool, Ordering}, Mutex}, thread};

    struct Dav {
        address: std::net::SocketAddr,
        fresh: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        seen: Arc<Mutex<Vec<String>>>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Dav {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let fresh = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (available, stopping, requests) = (fresh.clone(), stop.clone(), seen.clone());
            let thread = thread::spawn(move || {
                for connection in listener.incoming() {
                    let mut connection = connection.unwrap();
                    if stopping.load(Ordering::SeqCst) { break; }
                    connection.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                    let mut bytes = Vec::new();
                    let headers_end = loop {
                        let mut buffer = [0; 4096];
                        let count = connection.read(&mut buffer).unwrap();
                        assert_ne!(count, 0, "request ended before headers");
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") { break position + 4; }
                    };
                    let headers = String::from_utf8(bytes[..headers_end].to_vec()).unwrap();
                    let length = headers.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("content-length")).map(|(_, value)| value.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    while bytes.len() < headers_end + length {
                        let mut buffer = [0; 4096];
                        let count = connection.read(&mut buffer).unwrap();
                        assert_ne!(count, 0);
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    let request = headers.lines().next().unwrap().to_owned();
                    requests.lock().unwrap().push(request.clone());
                    assert!(request.starts_with("PROPFIND "), "unexpected mutation request: {request}");
                    assert!(headers.lines().any(|line| line.eq_ignore_ascii_case("authorization: Basic dXNlcjpmaXh0dXJlLXBhc3N3b3Jk")));
                    let target = request.split_whitespace().nth(1).unwrap();
                    let depth_one = headers.lines().any(|line| line.eq_ignore_ascii_case("depth: 1"));
                    let mut rows = match target {
                        "/dav/" => vec![("", true, 0)],
                        "/dav/folder/" | "/dav/folder" => vec![("folder/", true, 0)],
                        "/dav/fresh.txt" if available.load(Ordering::SeqCst) => vec![("fresh.txt", false, 9)],
                        _ => Vec::new(),
                    };
                    if depth_one && target == "/dav/" {
                        rows.extend([("folder/", true, 0), ("old.txt", false, 3)]);
                        if available.load(Ordering::SeqCst) { rows.push(("fresh.txt", false, 9)); }
                    } else if depth_one && target.trim_end_matches('/') == "/dav/folder" {
                        rows.push(("folder/fresh-child.txt", false, 12));
                    }
                    let status = if rows.is_empty() { "404 Not Found" } else { "207 Multi-Status" };
                    let body = format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">{}</d:multistatus>", rows.into_iter().map(|(name, directory, size)| format!("<d:response><d:href>/dav/{name}</d:href><d:propstat><d:prop><d:resourcetype>{}</d:resourcetype><d:getcontentlength>{size}</d:getcontentlength><d:getetag>\"live\"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>", if directory { "<d:collection/>" } else { "" })).collect::<String>());
                    write!(connection, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            Self { address, fresh, stop, seen, thread: Some(thread) }
        }
    }
    impl Drop for Dav {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            TcpStream::connect(self.address).unwrap();
            self.thread.take().unwrap().join().unwrap();
        }
    }
    struct Environment(Option<std::ffi::OsString>);
    impl Environment {
        fn fixture() -> Self {
            let old = std::env::var_os("RDRIVE_READONLY_FIXTURE_PASSWORD");
            std::env::set_var("RDRIVE_READONLY_FIXTURE_PASSWORD", "fixture-password");
            Self(old)
        }
    }
    impl Drop for Environment {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("RDRIVE_READONLY_FIXTURE_PASSWORD", value),
                None => std::env::remove_var("RDRIVE_READONLY_FIXTURE_PASSWORD"),
            }
        }
    }
    fn cli(home: &Path, arguments: &[&str]) -> Cli {
        let mut values = vec!["rdrive", "--json", "--home", home.to_str().unwrap()];
        values.extend_from_slice(arguments);
        Cli::try_parse_from(values).unwrap()
    }
    fn names(home: &Path) -> std::collections::BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(home).unwrap().map(|row| row.unwrap().file_name()).collect()
    }

    #[tokio::test]
    async fn remote_reads_use_live_shared_profiles_without_taking_or_recovering_store() {
        let _environment = Environment::fixture();
        let dav = Dav::start();
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        store.config.profiles.insert("pan123".into(), Profile {
            protocol: Protocol::Webdav, url: format!("http://{}/dav/", dav.address), username: Some("user".into()),
            password_env: Some("RDRIVE_READONLY_FIXTURE_PASSWORD".into()), keychain_internet: false,
            allow_http: true, timeout_secs: 5, smb_encryption: true, nextcloud_upload_url: None,
        });
        store.config.filters.exclude.push("fresh*".into());
        store.save_config().unwrap();
        std::fs::write(home.join("state.json"), b"unrelated interrupted writer ledger").unwrap();
        let before = (std::fs::read(home.join("config.json")).unwrap(), std::fs::read(home.join("state.json")).unwrap(), names(&home));
        let first = read_command(&cli(&home, &["ls", "pan123"]), &home).await.unwrap().unwrap();
        assert_eq!(first.as_array().unwrap().len(), 2);
        dav.fresh.store(true, Ordering::SeqCst);
        let fresh = read_command(&cli(&home, &["ls", "pan123", "--contains", "fresh"]), &home).await.unwrap().unwrap();
        assert_eq!(fresh.as_array().unwrap().len(), 1);
        assert_eq!(fresh[0]["path"], "fresh.txt");
        let recursive = read_command(&cli(&home, &["ls", "pan123", "--recursive", "--contains", "fresh"]), &home).await.unwrap().unwrap();
        assert_eq!(recursive.as_array().unwrap().len(), 2);
        assert!(recursive.as_array().unwrap().iter().any(|item| item["path"] == "folder/fresh-child.txt"));
        let stat = read_command(&cli(&home, &["stat", "pan123", "fresh.txt"]), &home).await.unwrap().unwrap();
        assert_eq!(stat["size"], 9);
        let info = read_command(&cli(&home, &["info", "pan123"]), &home).await.unwrap().unwrap();
        assert_eq!(info["root"]["is_dir"], true);
        assert!(info["capabilities"].is_object());
        assert!(info["diagnostics"].is_object());
        let profiles = read_command(&cli(&home, &["profile", "list"]), &home).await.unwrap().unwrap();
        assert_eq!(profiles["pan123"]["url"], store.config.profiles["pan123"].url);
        for arguments in [vec!["ls", "pan123"], vec!["stat", "pan123", "fresh.txt"], vec!["info", "pan123"], vec!["profile", "list"]] {
            let command = cli(&home, &arguments);
            run_with_native(&command, &NativeCalls::default()).await.unwrap();
            execute(&command, &mut store).await.unwrap();
        }
        let unknown = read_command(&cli(&home, &["ls", "dav"]), &home).await.unwrap_err().to_string();
        assert!(unknown.contains("unknown profile dav") && unknown.contains("pan123") && unknown.contains(home.to_str().unwrap()));
        let missing = read_command(&cli(&home, &["stat", "pan123", "missing.txt"]), &home).await.unwrap().unwrap();
        assert_eq!(missing, serde_json::Value::Null);
        for arguments in [vec!["ls", "pan123", "../invalid"], vec!["stat", "pan123", "../invalid"], vec!["ls", "pan123", "missing"]] {
            assert!(read_command(&cli(&home, &arguments), &home).await.is_err(), "{arguments:?}");
        }
        assert!(read_command(&cli(&home, &["cache", "list", "pan123"]), &home).await.unwrap().is_none());
        assert_eq!((std::fs::read(home.join("config.json")).unwrap(), std::fs::read(home.join("state.json")).unwrap(), names(&home)), before);
        assert!(Store::open(home.clone()).err().unwrap().to_string().contains("another rdrive process"));
        assert!(dav.seen.lock().unwrap().iter().any(|request| request.contains("/dav/folder/")));
        let absent = temporary.path().join("absent");
        assert_eq!(read_command(&cli(&absent, &["profile", "list"]), &absent).await.unwrap().unwrap(), serde_json::json!({}));
        assert!(!absent.exists());
    }

    #[tokio::test]
    async fn profile_add_explicit_internet_keychain_source_is_persisted_and_exclusive() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("state");
        let mut store = Store::open(home.clone()).unwrap();
        execute(&cli(&home, &["profile", "add", "existing", "--protocol", "webdav", "--url", "https://example.invalid/dav/", "--username", "user", "--keychain-internet"]), &mut store).await.unwrap();
        assert!(store.config.profiles["existing"].keychain_internet);
        assert!(rdrive::client_read::config(&home).unwrap().profiles["existing"].keychain_internet);
        assert!(Cli::try_parse_from(["rdrive", "profile", "add", "invalid", "--protocol", "webdav", "--url", "https://example.invalid/dav/", "--keychain-internet", "--password-env", "PASSWORD"]).is_err());
    }
}

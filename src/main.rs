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
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}
fn path(p: &str) -> Result<String> {
    backend::normalize(p)
}
fn profile(store: &Store, name: &str) -> Result<Profile> {
    store
        .config
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("unknown profile {name}"))
}
fn smart_queue(store:&Store,id:&str)->Result<rdrive::smart::Queue>{
    let domain=rdrive::provider::domains(&store.home)?.into_iter().find(|domain|domain.id==id&&domain.smart).context("Smart domain not found")?;
    rdrive::smart::Queue::open(store,id,&rdrive::provider::journal_binding(&store.config,&domain)?)
}
async fn connect(store: &Store, name: &str) -> Result<Arc<dyn Backend>> {
    rdrive::factory::connect(&profile(store,name)?,name,rdrive::transfer::global()).await
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
        Command::Mcp { .. } | Command::Service { .. } => unreachable!(),
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
            SmartCommand::Resolve{..}=>unreachable!(),
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
        Command::Info { profile: name } => {
            let b = connect(store, name).await?;
            let root = b.stat("").await?;
            emit(
                json,
                &serde_json::json!({"capabilities":b.capabilities(),"root":root,"diagnostics":b.diagnostics()}),
            )?;
        }
        Command::Ls {
            profile: name,
            remote,
            recursive,
            contains,
        } => {
            let b = connect(store, name).await?;
            let remote = path(remote)?;
            let mut entries = if *recursive {
                backend::walk(b.as_ref(), &remote).await?
            } else {
                b.list(&remote).await?
            };
            if let Some(filter) = contains {
                entries.retain(|e| e.path.contains(filter));
            }
            emit(json, &entries)?;
        }
        Command::Stat {
            profile: name,
            remote,
        } => {
            let b = connect(store, name).await?;
            emit(json, &b.stat(&path(remote)?).await?)?;
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
            if !b.capabilities().conditional_write {
                bail!("this backend cannot guard edits against remote changes; use get/put --overwrite explicitly");
            }
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
    let home = cli.home.clone().unwrap_or_else(config::default_home);
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
        Command::Mounts => return emit(cli.json, &rdrive::mount::list()?),
        Command::Unmount { local, force } => {
            return emit(cli.json, &rdrive::mount::unmount(local, *force)?)
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
                return emit(cli.json, &rdrive::mount::plan(&p, local, &options)?);
            }
            let credential = if matches!(p.protocol, Protocol::Webdav)
                && (p.username.is_some() || p.password_env.is_some())
            {
                match config::password(&p, name) {
                    Ok(value) => Some(value),
                    Err(e) if p.password_env.is_some() => return Err(e),
                    Err(_) => None,
                }
            } else {
                None
            };
            return emit(
                cli.json,
                &rdrive::mount::mount(&p, local, &options, credential.as_deref())?,
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

//! Pure Rust SMB transport. No Finder mount or Samba library is involved.
use crate::{
    backend::{self, Backend, Capabilities, ChangeEvent, Entry, WriteCondition},
    config::Profile,
    transfer::{Transfer, TransferContext},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smb::binrw_util::prelude::FileTime;
use smb::connection::EncryptionMode;
use smb::{
    Client, ClientConfig, ConnectionConfig, CreateDisposition, CreateOptions, DirAccessMask,
    Directory, FileAccessMask, FileAttributes, FileCreateArgs, FileDispositionInformation,
    FileFsVolumeInformation, FileIdBothDirectoryInformation, FileIdInformation,
    FileInternalInformation, FileNetworkOpenInformation, FileRenameInformation, GetLen,
    NotifyAction, NotifyFilter, ReadAt, Resource, ResourceHandle, ShareAccessFlags, UncPath,
    WriteAt,
};
use std::{
    io::SeekFrom,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const CHUNK: usize = 64 * 1024;

#[derive(Default)]
struct LifecycleCounters {
    reconnects: std::sync::atomic::AtomicU64,
    authentication_failures: std::sync::atomic::AtomicU64,
    durable_grants: std::sync::atomic::AtomicU64,
    durable_reconnects: std::sync::atomic::AtomicU64,
    verified_reopens: std::sync::atomic::AtomicU64,
    leases: std::sync::Mutex<Vec<std::sync::Weak<smb::resource::LeaseInfo>>>,
}

/// Credentials stay private and are reused only with the original authentication
/// and encryption policy. Mutating requests are never replayed after an error.
struct ManagedClient {
    config: ClientConfig,
    share: UncPath,
    username: String,
    password: std::sync::Mutex<String>,
    active: tokio::sync::RwLock<Arc<Client>>,
    reconnect: tokio::sync::Mutex<()>,
    counters: Arc<LifecycleCounters>,
}

impl ManagedClient {
    fn new(client: Client, share: UncPath, username: &str, password: &str) -> Self {
        Self {
            config: client.config().clone(),
            share,
            username: username.into(),
            password: std::sync::Mutex::new(password.into()),
            active: tokio::sync::RwLock::new(Arc::new(client)),
            reconnect: tokio::sync::Mutex::new(()),
            counters: Default::default(),
        }
    }
    fn config(&self) -> &ClientConfig {
        &self.config
    }
    async fn get_connection(&self, server: &str) -> smb::Result<Arc<smb::connection::Connection>> {
        self.active.read().await.get_connection(server).await
    }
    async fn current(&self) -> smb::Result<Arc<Client>> {
        let client = self.active.read().await.clone();
        if client
            .get_connection(self.share.server())
            .await?
            .is_closed()
        {
            return self.reconnect(false).await;
        }
        Ok(client)
    }
    async fn reconnect(&self, force: bool) -> smb::Result<Arc<Client>> {
        use std::sync::atomic::Ordering;
        let _guard = self.reconnect.lock().await;
        let current = self.active.read().await.clone();
        if !force
            && !current
                .get_connection(self.share.server())
                .await?
                .is_closed()
        {
            return Ok(current);
        }
        let mut last = None;
        for attempt in 0..5 {
            if attempt != 0 {
                tokio::time::sleep(Duration::from_millis(200 * (1 << (attempt - 1)))).await;
            }
            let candidate = Arc::new(Client::new(self.config.clone()));
            let password = self
                .password
                .lock()
                .map_err(|_| smb::Error::LockError)?
                .clone();
            match candidate
                .share_connect(&self.share, &self.username, password)
                .await
            {
                Ok(()) => {
                    *self.active.write().await = candidate.clone();
                    self.counters.reconnects.fetch_add(1, Ordering::SeqCst);
                    return Ok(candidate);
                }
                Err(error) => {
                    if !reconnectable_smb(&error) {
                        self.counters
                            .authentication_failures
                            .fetch_add(1, Ordering::SeqCst);
                        return Err(error);
                    }
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or(smb::Error::ConnectionStopped))
    }
    async fn create_file(&self, path: &UncPath, args: &FileCreateArgs) -> smb::Result<Resource> {
        let client = self.current().await?;
        match client.create_file(path, args).await {
            Err(error)
                if args.disposition == CreateDisposition::Open
                    && args.lifecycle.as_ref().is_none_or(|request| {
                        matches!(
                            request.durable,
                            smb::resource::DurableContext::Request { .. }
                        )
                    })
                    && reconnectable_smb(&error) =>
            {
                self.reconnect(true).await?.create_file(path, args).await
            }
            result => result,
        }
    }
}

fn reconnectable_smb(error: &smb::Error) -> bool {
    match error {
        smb::Error::ConnectionStopped | smb::Error::OperationTimeout(..) => true,
        smb::Error::TransportError(
            smb::transport::TransportError::NotConnected
            | smb::transport::TransportError::Timeout(_),
        ) => true,
        smb::Error::TransportError(smb::transport::TransportError::IoError(error))
        | smb::Error::IoError(error) => reconnectable_io(error),
        smb::Error::ReceivedErrorMessage(status, _)
        | smb::Error::UnexpectedMessageStatus(status) => {
            matches!(*status, 0xC00000C9 | 0xC0000203 | 0xC000035C | 0xC0000128)
        }
        _ => false,
    }
}

fn reconnectable_io(error: &std::io::Error) -> bool {
    if let Some(error) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<smb::Error>())
    {
        return reconnectable_smb(error);
    }
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
    )
}

fn reconnectable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<smb::Error>()
            .is_some_and(reconnectable_smb)
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(reconnectable_io)
    })
}

fn durable_reopen_refused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| match cause.downcast_ref::<smb::Error>() {
            Some(
                smb::Error::ReceivedErrorMessage(status, _)
                | smb::Error::UnexpectedMessageStatus(status),
            ) => matches!(
                *status,
                0xC000000D | 0xC0000034 | 0xC0000008 | 0xC00000BB | 0xC0000128
            ),
            _ => false,
        })
}

/// All SMB file payloads, including validation reads and metadata files, use
/// the same caller-supplied aggregate limiter. Protocol headers are excluded.
struct SmbFile {
    raw: smb::File,
    context: TransferContext,
    path: String,
    read: std::sync::Mutex<Option<Transfer>>,
    write: std::sync::Mutex<Option<Transfer>>,
}

impl SmbFile {
    fn new(raw: smb::File, context: TransferContext, path: &str) -> Self {
        Self {
            raw,
            context,
            path: path.into(),
            read: Default::default(),
            write: Default::default(),
        }
    }
    fn meter(&self, writing: bool) -> Result<Transfer> {
        let slot = if writing { &self.write } else { &self.read };
        let mut slot = slot
            .lock()
            .map_err(|_| anyhow!("SMB transfer counter unavailable"))?;
        if slot.is_none() {
            *slot = Some(self.context.start(
                if writing {
                    "smb.write"
                } else {
                    "smb.read-and-verify"
                },
                &self.path,
                None,
                0,
            )?);
        }
        Ok(slot.as_ref().unwrap().clone())
    }
    async fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize> {
        let limit = self.context.chunk_size(buffer.len());
        let buffer = &mut buffer[..limit];
        let transfer = self.meter(false)?;
        transfer.acquire(buffer.len()).await?;
        match self.raw.read_at(buffer, offset).await {
            Ok(count) => {
                transfer.record(count as u64)?;
                Ok(count)
            }
            Err(error) => {
                transfer.fail(&error.to_string());
                Err(error.into())
            }
        }
    }
    async fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize> {
        let buffer = &buffer[..self.context.chunk_size(buffer.len())];
        let transfer = self.meter(true)?;
        transfer.acquire(buffer.len()).await?;
        match self.raw.write_at(buffer, offset).await {
            Ok(count) => {
                transfer.record(count as u64)?;
                Ok(count)
            }
            Err(error) => {
                transfer.fail(&error.to_string());
                Err(error.into())
            }
        }
    }
    async fn close(&self) -> smb::Result<()> {
        let result = self.raw.close().await;
        for slot in [&self.read, &self.write] {
            if let Ok(slot) = slot.lock() {
                if let Some(transfer) = slot.as_ref() {
                    if result.is_ok() {
                        transfer.finish();
                    } else {
                        transfer.fail("SMB close failed");
                    }
                }
            }
        }
        result
    }
}

impl std::ops::Deref for SmbFile {
    type Target = smb::File;
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecoveryReceipt {
    version: u32,
    target: String,
    stage: String,
    backup: String,
    receipt: String,
    expected: Entry,
    replacement_sha256: String,
    stage_etag: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryDone {
    version: u32,
    receipt_sha256: String,
    target: String,
    stage_etag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UploadIntent {
    overwrite: bool,
    expected: Option<Entry>,
}

impl UploadIntent {
    fn from_condition(condition: &WriteCondition) -> Self {
        Self {
            overwrite: matches!(condition, WriteCondition::Any),
            expected: if let WriteCondition::Match(entry) = condition {
                Some(entry.clone())
            } else {
                None
            },
        }
    }
    fn condition(&self) -> WriteCondition {
        if let Some(entry) = &self.expected {
            WriteCondition::Match(entry.clone())
        } else if self.overwrite {
            WriteCondition::Any
        } else {
            WriteCondition::CreateNew
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum UploadPhase {
    Prepared,
    Uploading,
    Staged,
    Publishing,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadSession {
    version: u32,
    scope: String,
    remote: String,
    stage: String,
    source_sha256: String,
    source_size: u64,
    intent: UploadIntent,
    phase: UploadPhase,
    stage_identity: Option<String>,
    stage_etag: Option<String>,
    confirmed_bytes: u64,
    recovery: Option<RecoveryReceipt>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum GuardPhase {
    Compared,
    BackedUp,
    UploadProgress,
    ResumeFirstWrite,
    DownloadFirstWrite,
    DownloadProgress,
    Published,
}

#[cfg(test)]
struct GuardTestHooks {
    phase: GuardPhase,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    offset: std::sync::atomic::AtomicU64,
}

pub struct Smb {
    client: Arc<ManagedClient>,
    share: UncPath,
    root: String,
    encryption_required: bool,
    scope: String,
    transfer: TransferContext,
    #[cfg(test)]
    guard_hooks: Option<Arc<GuardTestHooks>>,
    #[cfg(test)]
    notify_buffer: Option<u32>,
}

impl Smb {
    pub fn with_transfer_context(mut self, context: TransferContext) -> Self {
        self.transfer = context;
        self
    }

    pub async fn new(profile: &Profile, password: &str) -> Result<Self> {
        crate::config::validate_profile(profile)?;
        let (url, share_name, root) = parse_target(&profile.url)?;
        let connection = ConnectionConfig {
            port: url.port(),
            timeout: Some(Duration::from_secs(profile.timeout_secs)),
            min_dialect: Some(if profile.smb_encryption {
                smb::Dialect::Smb030
            } else {
                smb::Dialect::Smb0202
            }),
            max_dialect: Some(smb::Dialect::Smb0311),
            encryption_mode: if profile.smb_encryption {
                EncryptionMode::Required
            } else {
                EncryptionMode::Allowed
            },
            compression_enabled: false,
            ..ConnectionConfig::default()
        };
        connection.validate()?;
        let client = Client::new(ClientConfig {
            connection,
            dfs: false,
            ..ClientConfig::default()
        });
        let share = UncPath::from_str(&format!(
            r"\\{}\{}",
            url.host_str().context("missing SMB host")?,
            share_name
        ))?;
        client
            .share_connect(
                &share,
                profile.username.as_deref().unwrap_or(""),
                password.to_owned(),
            )
            .await
            .context("SMB connection or authentication failed")?;
        let result = Self {
            client: Arc::new(ManagedClient::new(
                client,
                share.clone(),
                profile.username.as_deref().unwrap_or(""),
                password,
            )),
            share,
            root,
            encryption_required: profile.smb_encryption,
            transfer: crate::transfer::global_context(),
            scope: crate::state::key(&format!(
                "{}\n{}\n{}\n{}",
                profile.url,
                profile.username.as_deref().unwrap_or(""),
                profile.smb_encryption,
                "SMB-v1"
            )),
            #[cfg(test)]
            guard_hooks: None,
            #[cfg(test)]
            notify_buffer: None,
        };
        result.check_ancestors("", true).await?;
        let entry = result
            .stat("")
            .await?
            .context("configured SMB root does not exist")?;
        if !entry.is_dir {
            bail!("configured SMB root is not a directory");
        }
        Ok(result)
    }

    fn full_path(&self, path: &str) -> Result<String> {
        let path = valid_path(path)?;
        Ok(match (self.root.is_empty(), path.is_empty()) {
            (true, _) => path,
            (false, true) => self.root.clone(),
            (false, false) => format!("{}/{path}", self.root),
        })
    }

    fn unc(&self, path: &str) -> Result<UncPath> {
        let full = self.full_path(path)?;
        Ok(if full.is_empty() {
            self.share.clone().with_no_path()
        } else {
            self.share.clone().with_path(&full.replace('/', "\\"))
        })
    }

    async fn raw_stat(&self, full: &str) -> Result<Option<FileNetworkOpenInformation>> {
        let unc = if full.is_empty() {
            self.share.clone().with_no_path()
        } else {
            self.share.clone().with_path(&full.replace('/', "\\"))
        };
        let mut args = FileCreateArgs::make_open_existing(
            FileAccessMask::new().with_file_read_attributes(true),
        );
        args.options = CreateOptions::new().with_open_reparse_point(true);
        let resource = match self.client.create_file(&unc, &args).await {
            Ok(r) => r,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let handle = handle(&resource);
        let result = handle
            .query_info::<FileNetworkOpenInformation>()
            .await
            .map_err(anyhow::Error::from);
        let closed = handle.close().await.map_err(anyhow::Error::from);
        let info = finish(result, closed)?;
        reject_reparse(info.file_attributes)?;
        Ok(Some(info))
    }

    /// Inspect every known ancestor without following its final reparse point.
    /// SMB path opens cannot atomically pin all ancestors against a concurrent rename.
    async fn ancestors_exist(&self, path: &str, include_leaf: bool) -> Result<bool> {
        let full = self.full_path(path)?;
        let mut parts: Vec<&str> = full.split('/').filter(|p| !p.is_empty()).collect();
        if !include_leaf {
            parts.pop();
        }
        let mut prefix = String::new();
        for part in parts {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let Some(info) = self.raw_stat(&prefix).await? else {
                return Ok(false);
            };
            if !info.file_attributes.directory() {
                bail!("SMB ancestor {prefix} is not a directory");
            }
        }
        Ok(true)
    }

    async fn check_ancestors(&self, path: &str, include_leaf: bool) -> Result<()> {
        if !self.ancestors_exist(path, include_leaf).await? {
            bail!("an SMB parent directory is missing");
        }
        Ok(())
    }

    async fn open_file(&self, path: &str, args: &FileCreateArgs) -> Result<SmbFile> {
        let resource = self.client.create_file(&self.unc(path)?, args).await?;
        let info = handle(&resource)
            .query_info::<FileNetworkOpenInformation>()
            .await;
        match info {
            Ok(info) if !info.file_attributes.reparse_point() => (),
            Ok(_) => {
                let _ = handle(&resource).close().await;
                bail!("SMB symlinks and reparse points are not supported");
            }
            Err(error) => {
                let _ = handle(&resource).close().await;
                return Err(error.into());
            }
        }
        match resource {
            Resource::File(file) => {
                use std::sync::atomic::Ordering;
                if file.durable_info().is_some() {
                    self.client
                        .counters
                        .durable_grants
                        .fetch_add(1, Ordering::SeqCst);
                }
                if let Some(lease) = file.lease_info() {
                    if let Ok(mut leases) = self.client.counters.leases.lock() {
                        leases.push(Arc::downgrade(&lease));
                    }
                }
                Ok(SmbFile::new(file, self.transfer.clone(), path))
            }
            other => {
                handle(&other).close().await?;
                bail!("SMB path is not a regular file");
            }
        }
    }

    fn write_args(condition: WriteCondition) -> Result<FileCreateArgs> {
        let disposition = match condition {
            WriteCondition::Any => CreateDisposition::OverwriteIf,
            WriteCondition::CreateNew => CreateDisposition::Create,
            WriteCondition::Match(_) => {
                bail!("SMB atomic revision comparison is unavailable; refusing guarded overwrite")
            }
        };
        Ok(FileCreateArgs {
            share_access: None,
            lifecycle: None,
            disposition,
            attributes: FileAttributes::new(),
            options: CreateOptions::new()
                .with_non_directory_file(true)
                .with_open_reparse_point(true),
            desired_access: FileAccessMask::new()
                .with_generic_write(true)
                .with_file_read_attributes(true),
        })
    }

    async fn open_protected(&self, path: &str, delete: bool) -> Result<SmbFile> {
        let mut args = FileCreateArgs::make_open_existing(
            FileAccessMask::new()
                .with_file_read_data(true)
                .with_file_read_attributes(true)
                .with_delete(delete),
        );
        args.options = CreateOptions::new()
            .with_non_directory_file(true)
            .with_open_reparse_point(true);
        args.share_access = Some(ShareAccessFlags::new().with_read(true));
        self.open_file(path, &args)
            .await
            .context("obtain server-enforced SMB deny-write/delete handle")
    }

    fn download_args() -> FileCreateArgs {
        let mut args = FileCreateArgs::make_open_existing(
            FileAccessMask::new()
                .with_file_read_data(true)
                .with_file_read_attributes(true),
        );
        args.options = CreateOptions::new()
            .with_non_directory_file(true)
            .with_open_reparse_point(true);
        args.share_access = Some(ShareAccessFlags::new().with_read(true));
        // There is no dirty write cache. The handle lease permits the server to
        // retain this read open on transient TCP loss; grants remain optional.
        args.lifecycle = Some(smb::resource::LifecycleRequest::read_handle(
            smb::Guid::generate(),
            smb::Guid::generate(),
            15_000,
        ));
        args
    }

    async fn open_download(&self, path: &str) -> Result<SmbFile> {
        let mut args = Self::download_args();
        let client = self.client.current().await?;
        if client
            .get_connection(self.share.server())
            .await?
            .conn_info()
            .is_none_or(|info| info.negotiation.dialect_rev < smb::Dialect::Smb030)
        {
            args.lifecycle = None;
        }
        self.open_file(path, &args).await
    }

    async fn recover_download(
        &self,
        path: &str,
        previous: &SmbFile,
        expected: &Entry,
    ) -> Result<SmbFile> {
        use std::sync::atomic::Ordering;
        let durable = previous.durable_info();
        previous.abandon();
        let client = self
            .client
            .reconnect(true)
            .await
            .context("SMB authenticated reconnect failed; partial download retained")?;
        let mut args = Self::download_args();
        let mut recovered = None;
        if let Some(durable) = durable {
            let server_matches = client
                .get_connection(self.share.server())
                .await?
                .conn_info()
                .is_some_and(|info| info.negotiation.server_guid == durable.server_guid);
            if server_matches {
                args.lifecycle = durable.reconnect_request();
                match self.open_file(path, &args).await {
                    Ok(file) => {
                        // Stable disk identity and content are checked below even
                        // after a successful protocol durable-handle reconnect.
                        self.client
                            .counters
                            .durable_reconnects
                            .fetch_add(1, Ordering::SeqCst);
                        recovered = Some(file);
                    }
                    Err(error) if durable_reopen_refused(&error) => (),
                    Err(error) => return Err(error.context("SMB durable reconnect failed")),
                }
            }
        }
        let reopened = recovered.is_none();
        let file = match recovered {
            Some(file) => file,
            None => self
                .open_download(path)
                .await
                .context("SMB durable state unavailable; protected reopen failed")?,
        };
        let result = self.protected_entry(path, &file).await;
        match result {
            Ok(current) if current.same_revision(expected) => {
                if reopened {
                    self.client
                        .counters
                        .verified_reopens
                        .fetch_add(1, Ordering::SeqCst);
                }
                Ok(file)
            }
            result => {
                let _ = file.close().await;
                match result {
                    Ok(_) => bail!(
                        "SMB remote identity or content changed after reconnect; partial download retained"
                    ),
                    Err(error) => Err(error.context(
                        "SMB reconnect revision verification failed; partial download retained",
                    )),
                }
            }
        }
    }

    async fn read_download_chunk(
        &self,
        path: &str,
        source: &mut SmbFile,
        expected: &Entry,
        buffer: &mut [u8],
        offset: u64,
        recoveries: &mut u8,
    ) -> Result<usize> {
        loop {
            match source.read_at(buffer, offset).await {
                Ok(count) => return Ok(count),
                Err(error) if *recoveries < 3 && reconnectable(&error) => {
                    *recoveries += 1;
                    *source = self.recover_download(path, source, expected).await?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn protected_entry(&self, path: &str, file: &SmbFile) -> Result<Entry> {
        let info = file.query_info::<FileNetworkOpenInformation>().await?;
        reject_reparse(info.file_attributes)?;
        let identity = stable_identity(file).await?;
        let hash = hash_remote(file, info.end_of_file).await?;
        let mut result = entry(path, &info);
        result.etag = Some(revision_token(&hash, &identity));
        Ok(result)
    }

    async fn rename_handle(&self, file: &SmbFile, to: &str) -> Result<()> {
        file.set_info(FileRenameInformation {
            replace_if_exists: false.into(),
            root_directory: 0,
            file_name: self.full_path(to)?.replace('/', "\\").into(),
        })
        .await?;
        Ok(())
    }

    async fn write_receipt(&self, path: &str, receipt: &RecoveryReceipt) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(receipt)?;
        self.write_metadata_once(path, &bytes).await
    }

    async fn write_metadata_once(&self, path: &str, bytes: &[u8]) -> Result<()> {
        if self.raw_stat(&self.full_path(path)?).await?.is_some() {
            let file = self.open_protected(path, false).await?;
            let result = async {
                let observed = self.protected_entry(path, &file).await?;
                if observed.size != bytes.len() as u64
                    || token_hash(observed.etag.as_deref())
                        != Some(format!("{:x}", Sha256::digest(&bytes)))
                {
                    bail!("SMB recovery metadata exists with different or incomplete contents");
                }
                Ok(())
            }
            .await;
            return finish(result, file.close().await.map_err(anyhow::Error::from));
        }
        let args = Self::write_args(WriteCondition::CreateNew)?;
        let file = self.open_file(path, &args).await?;
        let result = async {
            write_all_at(&file, bytes, 0).await?;
            file.flush().await?;
            Ok(())
        }
        .await;
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    async fn write_recovery_done(&self, receipt: &RecoveryReceipt) -> Result<()> {
        self.write_metadata_once(
            &recovery_done_path(&receipt.receipt)?,
            &serde_json::to_vec_pretty(&recovery_done(receipt)?)?,
        )
        .await
    }

    async fn read_receipt(&self, path: &str) -> Result<RecoveryReceipt> {
        if !reserved_uuid_name(path, ".rdrive-recovery-", ".json") {
            bail!("not an SMB recovery receipt path");
        }
        let file = self.open_protected(path, false).await?;
        let result = async {
            let size = file.get_len().await?;
            if size == 0 || size > 64 * 1024 {
                bail!("invalid SMB recovery receipt size");
            }
            let mut bytes = vec![0; size as usize];
            let mut offset = 0;
            while offset < bytes.len() {
                let count = file.read_at(&mut bytes[offset..], offset as u64).await?;
                if count == 0 {
                    bail!("incomplete SMB recovery receipt");
                }
                offset += count;
            }
            let receipt: RecoveryReceipt = serde_json::from_slice(&bytes)?;
            validate_receipt(&receipt, path)?;
            Ok(receipt)
        }
        .await;
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    async fn check_recovery_receipt(&self, path: &str) -> Result<()> {
        let receipt = self.read_receipt(path).await?;
        let done_path = recovery_done_path(path)?;
        if self.raw_stat(&self.full_path(&done_path)?).await?.is_some() {
            let expected = serde_json::to_vec_pretty(&recovery_done(&receipt)?)?;
            let file = self.open_protected(&done_path, false).await?;
            let result = async {
                let actual = self.protected_entry(&done_path, &file).await?;
                if actual.size != expected.len() as u64
                    || token_hash(actual.etag.as_deref())
                        != Some(format!("{:x}", Sha256::digest(expected)))
                {
                    bail!("SMB recovery completion marker is invalid; sync stopped");
                }
                Ok(())
            }
            .await;
            return finish(result, file.close().await.map_err(anyhow::Error::from));
        }
        if let Some(target) = self.stat(&receipt.target).await? {
            if target.etag.as_deref() == Some(&receipt.stage_etag)
                && token_hash(target.etag.as_deref()).as_deref()
                    == Some(&receipt.replacement_sha256)
            {
                // Planning is read-only. Explicit upload/recovery persists completion.
                return Ok(());
            }
        }
        bail!(
            "unfinished SMB replacement blocks synchronization: receipt={}, backup={}, stage={}; resume its upload checkpoint or explicitly recover the recorded files",
            receipt.receipt,
            receipt.backup,
            receipt.stage
        )
    }

    async fn publish_guarded(
        &self,
        stage: &str,
        remote: &str,
        expected: &Entry,
        replacement_sha256: &str,
        recovery: Option<&RecoveryReceipt>,
    ) -> Result<Entry> {
        require_strong_revision(expected)?;
        if expected.is_dir || expected.path != remote {
            bail!("SMB guarded replacement revision belongs to a different file");
        }
        let staged = self.open_protected(stage, true).await?;
        let staged_entry = match self.protected_entry(stage, &staged).await {
            Ok(entry)
                if token_hash(entry.etag.as_deref()).as_deref() == Some(replacement_sha256) =>
            {
                entry
            }
            Ok(_) => {
                let _ = staged.close().await;
                bail!("SMB staging content changed; original preserved");
            }
            Err(error) => {
                let _ = staged.close().await;
                return Err(error);
            }
        };
        if recovery.is_some_and(|receipt| Some(&receipt.stage_etag) != staged_entry.etag.as_ref()) {
            let _ = staged.close().await;
            bail!("SMB staging identity changed; recovery publication refused");
        }
        let original = match self.open_protected(remote, true).await {
            Ok(file) => file,
            Err(error) => {
                let _ = staged.close().await;
                return Err(error);
            }
        };
        let id = uuid::Uuid::new_v4();
        let parent = remote
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let owned_receipt;
        let receipt = if let Some(receipt) = recovery {
            receipt
        } else {
            owned_receipt = RecoveryReceipt {
                version: 1,
                target: remote.into(),
                stage: stage.into(),
                backup: backend::join(parent, &format!(".rdrive-backup-{id}"))?,
                receipt: backend::join(parent, &format!(".rdrive-recovery-{id}.json"))?,
                expected: expected.clone(),
                replacement_sha256: replacement_sha256.into(),
                stage_etag: staged_entry
                    .etag
                    .clone()
                    .context("missing staging identity")?,
            };
            &owned_receipt
        };
        let result = async {
            let observed = self.protected_entry(remote, &original).await?;
            if !observed.same_revision(expected) {
                bail!("SMB remote content or identity changed; original preserved");
            }
            #[cfg(test)]
            self.guard_test_pause(GuardPhase::Compared).await;
            self.write_receipt(&receipt.receipt, receipt).await?;
            self.rename_handle(&original, &receipt.backup)
                .await
                .context("move protected original to recovery backup")?;
            #[cfg(test)]
            self.guard_test_pause(GuardPhase::BackedUp).await;
            self.rename_handle(&staged, remote).await.context(
                "publish complete SMB stage without replacing any concurrently created target",
            )?;
            self.write_recovery_done(receipt).await?;
            let mut published = staged_entry;
            published.path = remote.into();
            Ok(published)
        }
        .await;
        let result = finish(result, staged.close().await.map_err(anyhow::Error::from));
        let result = finish(result, original.close().await.map_err(anyhow::Error::from));
        result.with_context(|| format!("SMB guarded replacement recovery: receipt={}, backup={}, stage={}; no automatic rollback or deletion", receipt.receipt, receipt.backup, receipt.stage))
    }

    async fn publish_staged(
        &self,
        stage: &str,
        remote: &str,
        condition: WriteCondition,
        replacement_sha256: &str,
        expected_stage_etag: Option<&str>,
    ) -> Result<Entry> {
        if let WriteCondition::Match(expected) = condition {
            return self
                .publish_guarded(stage, remote, &expected, replacement_sha256, None)
                .await;
        }
        let overwrite = matches!(condition, WriteCondition::Any);
        if overwrite {
            self.raw_stat(&self.full_path(remote)?).await?;
        }
        let file = self.open_protected(stage, true).await?;
        let result = async {
            let mut observed = self.protected_entry(stage, &file).await?;
            if expected_stage_etag
                .is_some_and(|expected| observed.etag.as_deref() != Some(expected))
            {
                bail!("SMB staging identity changed; publication refused");
            }
            if token_hash(observed.etag.as_deref()).as_deref() != Some(replacement_sha256) {
                bail!("SMB staging content changed; publication refused");
            }
            file.set_info(FileRenameInformation {
                replace_if_exists: overwrite.into(),
                root_directory: 0,
                file_name: self.full_path(remote)?.replace('/', "\\").into(),
            })
            .await?;
            observed.path = remote.into();
            Ok(observed)
        }
        .await;
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    #[cfg(test)]
    async fn guard_test_pause(&self, phase: GuardPhase) {
        self.guard_test_pause_at(phase, 0).await;
    }

    #[cfg(test)]
    async fn guard_test_pause_at(&self, phase: GuardPhase, offset: u64) {
        if let Some(hooks) = &self.guard_hooks {
            if hooks.phase == phase {
                hooks
                    .offset
                    .store(offset, std::sync::atomic::Ordering::SeqCst);
                hooks.reached.notify_one();
                hooks.resume.notified().await;
            }
        }
    }

    async fn open_staging(&self, path: &str, create: bool) -> Result<SmbFile> {
        let args = FileCreateArgs {
            disposition: if create {
                CreateDisposition::Create
            } else {
                CreateDisposition::Open
            },
            attributes: FileAttributes::new(),
            options: CreateOptions::new()
                .with_non_directory_file(true)
                .with_open_reparse_point(true),
            desired_access: FileAccessMask::new()
                .with_file_read_data(true)
                .with_file_write_data(true)
                .with_file_append_data(true)
                .with_file_read_attributes(true)
                .with_delete(true),
            share_access: Some(ShareAccessFlags::new().with_read(true)),
            lifecycle: None,
        };
        self.open_file(path, &args).await
    }

    async fn checked_staging_entry(&self, session: &UploadSession) -> Result<Entry> {
        let file = self.open_protected(&session.stage, false).await?;
        let result = async {
            let observed = self.protected_entry(&session.stage, &file).await?;
            if observed.size != session.source_size
                || token_hash(observed.etag.as_deref()).as_deref() != Some(&session.source_sha256)
                || token_identity(observed.etag.as_deref()) != session.stage_identity
            {
                bail!(
                    "SMB resumable stage identity, size or content changed; refusing publication"
                );
            }
            Ok(observed)
        }
        .await;
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    async fn reconcile_publication(&self, session: &UploadSession) -> Result<Option<Entry>> {
        let current = self.stat(&session.remote).await?;
        if let Some(current) = &current {
            if current.etag == session.stage_etag
                && current.size == session.source_size
                && token_hash(current.etag.as_deref()).as_deref() == Some(&session.source_sha256)
            {
                if let Some(receipt) = &session.recovery {
                    self.write_recovery_done(receipt).await?;
                }
                return Ok(Some(current.clone()));
            }
        }
        self.checked_staging_entry(session).await
            .context("publication outcome ambiguous: final target is not the recorded stage and stage is unavailable or changed")?;
        if let Some(receipt) = &session.recovery {
            if self
                .raw_stat(&self.full_path(&receipt.backup)?)
                .await?
                .is_some()
            {
                if current.is_some() {
                    bail!(
                        "SMB publication conflict: another target exists; recovery backup and stage preserved"
                    );
                }
                let backup = self.open_protected(&receipt.backup, true).await?;
                let result = async {
                    let old = self.protected_entry(&receipt.backup, &backup).await?;
                    if !old.same_revision(&receipt.expected) {
                        bail!("SMB recovery backup changed; refusing to publish");
                    }
                    self.write_receipt(&receipt.receipt, receipt).await?;
                    let stage = self.open_protected(&session.stage, true).await?;
                    let publication = async {
                        let mut observed = self.protected_entry(&session.stage, &stage).await?;
                        if observed.etag != session.stage_etag {
                            bail!("SMB stage changed during recovery");
                        }
                        self.rename_handle(&stage, &session.remote).await?;
                        self.write_recovery_done(receipt).await?;
                        observed.path = session.remote.clone();
                        Ok(observed)
                    }
                    .await;
                    finish(
                        publication,
                        stage.close().await.map_err(anyhow::Error::from),
                    )
                }
                .await;
                return finish(result, backup.close().await.map_err(anyhow::Error::from)).map(Some);
            }
        }
        Ok(None)
    }

    async fn resume_upload_session(
        &self,
        local: &Path,
        remote: &str,
        condition: WriteCondition,
        checkpoint: &Path,
    ) -> Result<Entry> {
        if let WriteCondition::Match(expected) = &condition {
            require_strong_revision(expected)?;
        }
        let remote = valid_path(remote)?;
        if remote.is_empty() {
            bail!("cannot replace configured SMB root");
        }
        self.check_ancestors(&remote, false).await?;
        no_local_symlinks(local)?;
        no_local_symlinks(checkpoint)?;
        if !std::fs::symlink_metadata(local)?.is_file() {
            bail!("SMB upload requires a regular local snapshot");
        }
        let source_size = std::fs::metadata(local)?.len();
        let source_sha256 = crate::state::fingerprint(local)?;
        let intent = UploadIntent::from_condition(&condition);
        let mut session = match read_upload_session(checkpoint)? {
            Some(session) => session,
            None => {
                let parent = remote
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
                    .unwrap_or("");
                let session = UploadSession {
                    version: 1,
                    scope: self.scope.clone(),
                    remote: remote.clone(),
                    stage: backend::join(
                        parent,
                        &format!(".rdrive-upload-{}", uuid::Uuid::new_v4()),
                    )?,
                    source_sha256: source_sha256.clone(),
                    source_size,
                    intent: intent.clone(),
                    phase: UploadPhase::Prepared,
                    stage_identity: None,
                    stage_etag: None,
                    confirmed_bytes: 0,
                    recovery: None,
                };
                save_upload_session(checkpoint, &session)?;
                session
            }
        };
        validate_upload_session(
            &session,
            &self.scope,
            &remote,
            &source_sha256,
            source_size,
            &intent,
        )?;
        if matches!(
            session.phase,
            UploadPhase::Publishing | UploadPhase::Complete
        ) {
            if let Some(done) = self.reconcile_publication(&session).await? {
                session.phase = UploadPhase::Complete;
                save_upload_session(checkpoint, &session)?;
                return Ok(done);
            }
            if session.phase == UploadPhase::Complete {
                bail!("completed SMB upload target changed; no overwrite attempted");
            }
        }
        if matches!(
            session.phase,
            UploadPhase::Prepared | UploadPhase::Uploading
        ) {
            let creating = session.phase == UploadPhase::Prepared;
            let file = self.open_staging(&session.stage, creating).await
                .context("open recorded private SMB upload stage; an unrecorded existing stage is never adopted")?;
            let result = async {
                let identity = file_identity(&file).await?;
                if creating {
                    session.stage_identity = Some(identity);
                    session.phase = UploadPhase::Uploading;
                    save_upload_session(checkpoint, &session)?;
                } else if session.stage_identity.as_deref() != Some(&identity) {
                    bail!("SMB resumable stage was replaced; refusing to append");
                }
                let actual = file
                    .query_info::<FileNetworkOpenInformation>()
                    .await?
                    .end_of_file;
                if actual > source_size {
                    bail!("SMB stage is larger than immutable source; refusing to truncate");
                }
                let mut input = open_local_input(local)?;
                let mut buffer = vec![0; CHUNK];
                let mut local_buffer = vec![0; CHUNK];
                let mut offset = 0;
                while offset < actual {
                    let count = file
                        .read_at(&mut buffer[..CHUNK.min((actual - offset) as usize)], offset)
                        .await?;
                    if count == 0 {
                        bail!("SMB stage prefix ended early");
                    }
                    input.read_exact(&mut local_buffer[..count]).await?;
                    if buffer[..count] != local_buffer[..count] {
                        bail!(
                            "SMB uploaded prefix differs from immutable snapshot; stage preserved"
                        );
                    }
                    offset += count as u64;
                }
                let mut last_flush = offset;
                let mut first_write = true;
                loop {
                    let count = input.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    if offset + count as u64 > source_size {
                        bail!("local SMB snapshot grew during upload");
                    }
                    if first_write {
                        #[cfg(test)]
                        self.guard_test_pause_at(GuardPhase::ResumeFirstWrite, offset)
                            .await;
                        first_write = false;
                    }
                    write_all_at(&file, &buffer[..count], offset).await?;
                    offset += count as u64;
                    if offset - last_flush >= 1024 * 1024 {
                        file.flush().await?;
                        session.confirmed_bytes = offset;
                        save_upload_session(checkpoint, &session)?;
                        #[cfg(test)]
                        self.guard_test_pause_at(GuardPhase::UploadProgress, offset)
                            .await;
                        last_flush = offset;
                    }
                }
                if offset != source_size {
                    bail!("local SMB snapshot shrank during upload");
                }
                file.flush().await?;
                session.confirmed_bytes = offset;
                save_upload_session(checkpoint, &session)?;
                Ok(())
            }
            .await;
            let closed = file.close().await.map_err(anyhow::Error::from);
            finish(result, closed)?;
            let staged = self.checked_staging_entry(&session).await?;
            session.stage_etag = staged.etag;
            session.phase = UploadPhase::Staged;
            save_upload_session(checkpoint, &session)?;
        }
        let staged = self.checked_staging_entry(&session).await?;
        if staged.etag != session.stage_etag {
            bail!("SMB staging revision changed after checkpoint");
        }
        if let Some(expected) = &session.intent.expected {
            if session.recovery.is_none() {
                let id = uuid::Uuid::new_v4();
                let parent = remote
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
                    .unwrap_or("");
                session.recovery = Some(RecoveryReceipt {
                    version: 1,
                    target: remote.clone(),
                    stage: session.stage.clone(),
                    backup: backend::join(parent, &format!(".rdrive-backup-{id}"))?,
                    receipt: backend::join(parent, &format!(".rdrive-recovery-{id}.json"))?,
                    expected: expected.clone(),
                    replacement_sha256: source_sha256.clone(),
                    stage_etag: session
                        .stage_etag
                        .clone()
                        .context("missing SMB stage revision")?,
                });
            }
        }
        session.phase = UploadPhase::Publishing;
        save_upload_session(checkpoint, &session)?;
        let published = if let Some(receipt) = &session.recovery {
            self.publish_guarded(
                &session.stage,
                &remote,
                &receipt.expected,
                &source_sha256,
                Some(receipt),
            )
            .await?
        } else {
            self.publish_staged(
                &session.stage,
                &remote,
                session.intent.condition(),
                &source_sha256,
                session.stage_etag.as_deref(),
            )
            .await?
        };
        #[cfg(test)]
        self.guard_test_pause(GuardPhase::Published).await;
        session.phase = UploadPhase::Complete;
        save_upload_session(checkpoint, &session)?;
        Ok(published)
    }

    async fn delete_one(&self, path: &str, is_dir: bool) -> Result<()> {
        self.check_ancestors(path, false).await?;
        let args = FileCreateArgs {
            share_access: None,
            lifecycle: None,
            disposition: CreateDisposition::Open,
            attributes: FileAttributes::new(),
            options: CreateOptions::new()
                .with_open_reparse_point(true)
                .with_directory_file(is_dir)
                .with_non_directory_file(!is_dir),
            desired_access: FileAccessMask::new()
                .with_delete(true)
                .with_file_read_attributes(true),
        };
        let resource = self
            .client
            .create_file(&self.unc(path)?, &args)
            .await
            .context("open SMB deletion handle")?;
        let h = handle(&resource);
        let result = async {
            reject_reparse(
                h.query_info::<FileNetworkOpenInformation>()
                    .await
                    .context("inspect SMB deletion handle")?
                    .file_attributes,
            )?;
            h.set_info(FileDispositionInformation::default())
                .await
                .context("mark SMB resource for deletion")?;
            Ok(())
        }
        .await;
        let closed = h.close().await;
        if result.is_ok() && is_dir && closed.as_ref().is_err_and(is_missing) {
            // Samba can acknowledge the disposition, remove the directory, and then
            // return NAME_NOT_FOUND for CLOSE. Only accept that exact outcome after
            // a fresh lookup confirms that the requested path is absent.
            if self.stat(path).await?.is_none() {
                return Ok(());
            }
        }
        finish(result, closed.context("close SMB deletion handle"))
    }

    async fn copy_file(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        self.check_ancestors(from, false).await?;
        self.check_ancestors(to, false).await?;
        // Stage copies so a hard-link or filename alias cannot truncate
        // the source while it is being read. Failed copies may leave this named staging file.
        if overwrite {
            self.raw_stat(&self.full_path(to)?).await?;
        }
        let parent = to.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        let staging = backend::join(parent, &format!(".rdrive-copy-{}", uuid::Uuid::new_v4()))?;
        let destination = &staging;
        let mut read_args =
            FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
        read_args.options = CreateOptions::new()
            .with_non_directory_file(true)
            .with_open_reparse_point(true);
        let source = self.open_file(from, &read_args).await?;
        let write_args = Self::write_args(WriteCondition::CreateNew)?;
        let target = match self.open_file(destination, &write_args).await {
            Ok(file) => file,
            Err(error) => {
                let _ = source.close().await;
                return Err(error);
            }
        };
        let result = async {
            let len = source.get_len().await?;
            let mut offset = 0;
            let mut buffer = vec![0; CHUNK];
            while offset < len {
                let count = source
                    .read_at(&mut buffer[..CHUNK.min((len - offset) as usize)], offset)
                    .await?;
                if count == 0 {
                    bail!("SMB source ended before its reported length");
                }
                write_all_at(&target, &buffer[..count], offset).await?;
                offset += count as u64;
            }
            target.flush().await?;
            Ok(())
        }
        .await;
        let target_closed = target.close().await.map_err(anyhow::Error::from);
        let source_closed = source.close().await.map_err(anyhow::Error::from);
        finish(finish(result, target_closed), source_closed).with_context(|| {
            format!("SMB copy failed; partial data may remain at {destination}")
        })?;
        self.rename(&staging, to, overwrite)
            .await
            .with_context(|| {
                format!("SMB copy publication failed; staging object may remain at {staging}")
            })?;
        Ok(())
    }
}

#[async_trait]
impl Backend for Smb {
    async fn change_feed(
        &self,
        path: &str,
        recursive: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChangeEvent>>> {
        let path = valid_path(path)?;
        self.check_ancestors(&path, true).await?;
        let mut args = FileCreateArgs::make_open_existing(
            DirAccessMask::new()
                .with_list_directory(true)
                .with_read_attributes(true)
                .into(),
        );
        args.options = CreateOptions::new()
            .with_directory_file(true)
            .with_open_reparse_point(true);
        let resource = self.client.create_file(&self.unc(&path)?, &args).await?;
        if let Err(error) = reject_reparse(
            handle(&resource)
                .query_info::<FileNetworkOpenInformation>()
                .await?
                .file_attributes,
        ) {
            let _ = handle(&resource).close().await;
            return Err(error);
        }
        let directory = match resource {
            Resource::Directory(directory) => Arc::new(directory),
            other => {
                handle(&other).close().await?;
                bail!("SMB notification source must be a directory");
            }
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        #[cfg(test)]
        let buffer_size = self.notify_buffer;
        #[cfg(not(test))]
        let buffer_size = None;
        tokio::spawn(async move {
            use smb::resource::DirectoryWatchResult;
            let mut ready = false;
            'watch: loop {
                let ids = Arc::new(smb::msg_handler::AsyncMessageIds::default());
                let watch = directory.watch_controlled(
                    notification_filter(),
                    recursive,
                    ids.clone(),
                    buffer_size,
                );
                tokio::pin!(watch);
                let mut tick = tokio::time::interval(Duration::from_millis(5));
                let result = loop {
                    tokio::select! {
                        result = &mut watch => break result,
                        _ = cancel.cancelled() => {
                            let _ = directory.send_cancel(&ids).await;
                            let _ = tokio::time::timeout(Duration::from_secs(2), &mut watch).await;
                            break 'watch;
                        },
                        _ = sender.closed() => {
                            let _ = directory.send_cancel(&ids).await;
                            let _ = tokio::time::timeout(Duration::from_secs(2), &mut watch).await;
                            break 'watch;
                        },
                        _ = tick.tick(), if !ready => {
                            if ids.async_id.load(std::sync::atomic::Ordering::SeqCst) != u64::MAX {
                                ready = true;
                                if !send_change(&sender, &cancel, Ok(ChangeEvent { path: path.clone(), kind: "ready".into() })).await { break 'watch; }
                            }
                        },
                    }
                };
                match result {
                    DirectoryWatchResult::Notifications(events) => {
                        for event in events {
                            let name = event.file_name.to_string().replace('\\', "/");
                            let relative = match valid_path(&name) {
                                Ok(name) if !name.is_empty() => name,
                                _ => {
                                    let _ = send_change(
                                        &sender,
                                        &cancel,
                                        Ok(ChangeEvent {
                                            path: path.clone(),
                                            kind: "rescan_required".into(),
                                        }),
                                    )
                                    .await;
                                    continue;
                                }
                            };
                            let kind = match event.action {
                                NotifyAction::Added => "added",
                                NotifyAction::Removed => "removed",
                                NotifyAction::Modified => "modified",
                                NotifyAction::RenamedOldName => "rename_old",
                                NotifyAction::RenamedNewName => "rename_new",
                                _ => "rescan_required",
                            };
                            let event_path = match backend::join(&path, &relative) {
                                Ok(path) => path,
                                Err(_) => break 'watch,
                            };
                            if !send_change(
                                &sender,
                                &cancel,
                                Ok(ChangeEvent {
                                    path: event_path,
                                    kind: kind.into(),
                                }),
                            )
                            .await
                            {
                                break 'watch;
                            }
                        }
                    }
                    DirectoryWatchResult::NotifyEnumDir { .. } => {
                        if !send_change(
                            &sender,
                            &cancel,
                            Ok(ChangeEvent {
                                path: path.clone(),
                                kind: "rescan_required".into(),
                            }),
                        )
                        .await
                        {
                            break;
                        }
                    }
                    DirectoryWatchResult::Cancelled if cancel.is_cancelled() => break,
                    DirectoryWatchResult::Error(error) => {
                        let _ = send_change(
                            &sender,
                            &cancel,
                            Ok(ChangeEvent {
                                path: path.clone(),
                                kind: "rescan_required".into(),
                            }),
                        )
                        .await;
                        let _ = send_change(&sender, &cancel, Err(anyhow!("SMB change notification disconnected; rescan and resubscribe: {error}"))).await;
                        break;
                    }
                    _ => {
                        let _ = send_change(
                            &sender,
                            &cancel,
                            Ok(ChangeEvent {
                                path: path.clone(),
                                kind: "rescan_required".into(),
                            }),
                        )
                        .await;
                        break;
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), directory.close()).await;
        });
        Ok(receiver)
    }

    async fn check_sync_recovery(&self, receipt: &str) -> Result<()> {
        self.check_recovery_receipt(&valid_path(receipt)?).await
    }
    fn content_sha256(&self, entry: &Entry) -> Option<String> {
        token_hash(entry.etag.as_deref())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: "smb", conditional_write: true, server_copy: false, locking: false,
            notes: vec![
                if self.encryption_required { "SMB 3.0–3.1.1 with required transport encryption".into() }
                    else { "SMB 2.0.2–3.1.1; encryption allowed but not required".into() },
                "Guarded replacement verifies SHA256+file identity under server deny-write/delete handles, retains a recovery backup and receipt, then publishes with no-replace rename; the namespace gap is not an atomic transaction".into(),
                "Transfers and copies use a 64 KiB client buffer; files publish by server rename; failed operations may leave .rdrive-upload/.rdrive-copy staging files or partial directory trees".into(),
                "Client-visible SMB reparse points rejected; server-transparent POSIX symlinks cannot be identified. DFS disabled; concurrent ancestor replacement cannot be atomically excluded".into(),
                "CHANGE_NOTIFY provides cancellable change hints and explicit rescan on overflow/disconnect. Read downloads request server RH leases and durable-v2 handles; after disconnect a fresh authenticated session reconnects or reopens and verifies full SHA256+identity before continuing. Grants are server-dependent; no persistent CA handles or write cache".into(),
                "Uploads retain prefix-verified resumable checkpoints; ambiguous namespace mutations are not blindly replayed. No byte-range lock API. Metadata-only enumeration uses weak revisions and reads no file content".into(),
                "File stat/list stream full content to compute strong revisions under server share-mode protection; direct server filesystem writes bypass SMB sharing modes".into(),
            ],
        }
    }

    fn diagnostics(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering;
        let counters = &self.client.counters;
        let (granted, breaks, acknowledgements) = counters
            .leases
            .lock()
            .map(|mut leases| {
                leases.retain(|lease| lease.strong_count() != 0);
                leases.iter().filter_map(|lease| lease.upgrade()).fold(
                    (0u64, 0u64, 0u64),
                    |sum, lease| {
                        (
                            sum.0 + u64::from(lease.granted()),
                            sum.1 + lease.breaks(),
                            sum.2 + lease.acknowledgements(),
                        )
                    },
                )
            })
            .unwrap_or_default();
        serde_json::json!({"authenticated_reconnects": counters.reconnects.load(Ordering::SeqCst),
            "reconnect_authentication_failures": counters.authentication_failures.load(Ordering::SeqCst),
            "durable_grants": counters.durable_grants.load(Ordering::SeqCst),
            "durable_reconnects": counters.durable_reconnects.load(Ordering::SeqCst),
            "verified_reopens": counters.verified_reopens.load(Ordering::SeqCst),
            "live_lease_grants": granted, "live_lease_breaks": breaks, "live_lease_acknowledgements": acknowledgements,
            "automatic_read_recovery_limit": 3, "automatic_write_replay": false,
            "payload_meter_excludes_protocol_headers": true})
    }

    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let path = valid_path(path)?;
        if !self.ancestors_exist(&path, false).await? {
            return Ok(None);
        }
        let Some(info) = self.raw_stat(&self.full_path(&path)?).await? else {
            return Ok(None);
        };
        if info.file_attributes.directory() {
            return Ok(Some(entry(&path, &info)));
        }
        let file = self.open_protected(&path, false).await?;
        let result = self.protected_entry(&path, &file).await.map(Some);
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    async fn stat_metadata(&self, path: &str) -> Result<Option<Entry>> {
        let path = valid_path(path)?;
        if !self.ancestors_exist(&path, false).await? {
            return Ok(None);
        }
        let mut args = FileCreateArgs::make_open_existing(
            FileAccessMask::new().with_file_read_attributes(true),
        );
        args.options = CreateOptions::new().with_open_reparse_point(true);
        let resource = match self.client.create_file(&self.unc(&path)?, &args).await {
            Ok(resource) => resource,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file = handle(&resource);
        let result = async {
            let info = file.query_info::<FileNetworkOpenInformation>().await?;
            reject_reparse(info.file_attributes)?;
            let identity = stable_identity(file).await?;
            let mut metadata = entry(&path, &info);
            // Deliberately weak: metadata enumeration must not download file
            // contents. It never authorizes a guarded overwrite/download.
            metadata.etag = Some(format!(
                "W/smb-meta:{:016x}:{:032x}:{}:{}:{}",
                identity.volume_serial_number,
                identity.file_id,
                info.end_of_file,
                info.last_write_time.since_epoch().as_nanos(),
                info.change_time.since_epoch().as_nanos()
            ));
            Ok(Some(metadata))
        }
        .await;
        finish(result, file.close().await.map_err(anyhow::Error::from))
    }

    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let mut entries = self.list_metadata(path).await?;
        for item in &mut entries {
            if !item.is_dir && !internal_artifact(&item.path) {
                *item = self
                    .stat(&item.path)
                    .await?
                    .context("SMB file disappeared during directory scan")?;
            }
        }
        Ok(entries)
    }

    async fn list_metadata(&self, path: &str) -> Result<Vec<Entry>> {
        let path = valid_path(path)?;
        self.check_ancestors(&path, true).await?;
        let mut args = FileCreateArgs::make_open_existing(
            DirAccessMask::new()
                .with_list_directory(true)
                .with_synchronize(true)
                .with_read_attributes(true)
                .into(),
        );
        args.options = CreateOptions::new()
            .with_directory_file(true)
            .with_open_reparse_point(true);
        let resource = self.client.create_file(&self.unc(&path)?, &args).await?;
        let dir = match resource {
            Resource::Directory(dir) => Arc::new(dir),
            other => {
                handle(&other).close().await?;
                bail!("SMB path is not a directory");
            }
        };
        let result = async {
            reject_reparse(
                dir.handle()
                    .query_info::<FileNetworkOpenInformation>()
                    .await?
                    .file_attributes,
            )?;
            let mut stream = Directory::query::<FileIdBothDirectoryInformation>(&dir, "*").await?;
            let mut entries = Vec::new();
            while let Some(item) = stream.next().await {
                let item = item?;
                let name = item.file_name.to_string();
                if name == "." || name == ".." {
                    continue;
                }
                valid_component(&name)?;
                reject_reparse(item.file_attributes)?;
                entries.push(Entry {
                    path: backend::join(&path, &name)?,
                    is_dir: item.file_attributes.directory(),
                    size: item.end_of_file,
                    modified: unix_ms(item.last_write_time),
                    etag: None,
                });
            }
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(entries)
        }
        .await;
        let mut entries = finish(
            result,
            dir.handle().close().await.map_err(anyhow::Error::from),
        )?;
        for item in &mut entries {
            if !internal_artifact(&item.path) {
                *item = self
                    .stat_metadata(&item.path)
                    .await?
                    .context("SMB file disappeared during directory scan")?;
            }
        }
        Ok(entries)
    }

    async fn download(&self, remote: &str, local: &Path) -> Result<()> {
        let expected = self
            .stat(remote)
            .await?
            .context("SMB download source is missing")?;
        self.download_resume(remote, local, &expected, 0).await
    }

    async fn upload(&self, local: &Path, remote: &str, condition: WriteCondition) -> Result<Entry> {
        if let WriteCondition::Match(expected) = &condition {
            require_strong_revision(expected)?;
        }
        let args = Self::write_args(WriteCondition::CreateNew)?;
        let remote = valid_path(remote)?;
        if remote.is_empty() {
            bail!("cannot replace the configured SMB root");
        }
        self.check_ancestors(&remote, false).await?;
        no_local_symlinks(local)?;
        if !std::fs::symlink_metadata(local)?.is_file() {
            bail!("upload source is not a regular file");
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        no_follow(&mut options);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let input = options.open(local).context("open local upload file")?;
        if !input.metadata()?.is_file() {
            bail!("upload source is not a regular file");
        }
        let mut input = tokio::fs::File::from_std(input);
        let parent = remote
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let staging = backend::join(parent, &format!(".rdrive-upload-{}", uuid::Uuid::new_v4()))?;
        let target = self.open_file(&staging, &args).await?;
        let result = async {
            let mut buffer = vec![0; CHUNK];
            let mut offset = 0;
            let mut hash = Sha256::new();
            loop {
                let count = input.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                write_all_at(&target, &buffer[..count], offset).await?;
                hash.update(&buffer[..count]);
                offset += count as u64;
            }
            target.flush().await?;
            Ok(format!("{:x}", hash.finalize()))
        }
        .await;
        let hash = finish(result, target.close().await.map_err(anyhow::Error::from)).with_context(
            || format!("SMB upload failed; staging object may remain at {staging}"),
        )?;
        self.publish_staged(&staging, &remote, condition, &hash, None)
            .await
            .with_context(|| {
                format!("SMB upload publication failed; staging object may remain at {staging}")
            })
    }

    async fn upload_resume(
        &self,
        local: &Path,
        remote: &str,
        condition: WriteCondition,
        checkpoint: &Path,
    ) -> Result<Entry> {
        let result = async {
            for attempt in 0..=3 {
                match self
                    .resume_upload_session(local, remote, condition.clone(), checkpoint)
                    .await
                {
                    Err(error) if attempt < 3 && reconnectable(&error) => {
                        // Re-enter the persisted state machine. It reconciles
                        // identity, actual length, every prefix byte and any
                        // publication receipt before issuing another mutation.
                        self.client
                            .reconnect(true)
                            .await
                            .context("SMB upload authenticated reconnect failed")?;
                    }
                    result => return result,
                }
            }
            unreachable!()
        }
        .await;
        result.with_context(|| format!("SMB upload checkpoint retained at {}; remote stages/backups are never deleted automatically", checkpoint.display()))
    }

    async fn download_checked(&self, remote: &str, local: &Path, expected: &Entry) -> Result<()> {
        self.download_resume(remote, local, expected, 0).await
    }

    async fn download_resume(
        &self,
        remote: &str,
        local: &Path,
        expected: &Entry,
        offset: u64,
    ) -> Result<()> {
        require_strong_revision(expected)?;
        let remote = valid_path(remote)?;
        if expected.path != remote || offset > expected.size {
            bail!("SMB download checkpoint revision/path/offset mismatch");
        }
        self.check_ancestors(&remote, false).await?;
        no_local_symlinks(local)?;
        let mut source = self.open_download(&remote).await?;
        let result = async {
            let current = self.protected_entry(&remote, &source).await?;
            if !current.same_revision(expected) {
                bail!("SMB remote identity or content changed; partial download preserved");
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true);
            if !local.exists() && offset == 0 {
                options.create_new(true);
            }
            no_follow(&mut options);
            let output = options.open(local)?;
            if !output.metadata()?.is_file() || output.metadata()?.len() != offset {
                bail!("SMB partial download must be a regular file of exactly the recorded size");
            }
            let mut output = tokio::fs::File::from_std(output);
            let transfer = async {
                let mut buffer = vec![0; CHUNK];
                let mut local_buffer = vec![0; CHUNK];
                let mut prefix = 0;
                let mut recoveries = 0;
                while prefix < offset {
                    let count = self
                        .read_download_chunk(
                            &remote,
                            &mut source,
                            expected,
                            &mut buffer[..CHUNK.min((offset - prefix) as usize)],
                            prefix,
                            &mut recoveries,
                        )
                        .await?;
                    if count == 0 {
                        bail!("SMB remote prefix ended early");
                    }
                    output.read_exact(&mut local_buffer[..count]).await?;
                    if buffer[..count] != local_buffer[..count] {
                        bail!("SMB partial download content differs from verified remote revision");
                    }
                    prefix += count as u64;
                }
                output.seek(SeekFrom::Start(offset)).await?;
                let mut position = offset;
                #[cfg(test)]
                let mut first_network_block = true;
                #[cfg(test)]
                self.guard_test_pause_at(GuardPhase::DownloadFirstWrite, offset)
                    .await;
                while position < expected.size {
                    let count = self
                        .read_download_chunk(
                            &remote,
                            &mut source,
                            expected,
                            &mut buffer[..CHUNK.min((expected.size - position) as usize)],
                            position,
                            &mut recoveries,
                        )
                        .await?;
                    if count == 0 {
                        bail!("SMB remote ended before expected size");
                    }
                    output.write_all(&buffer[..count]).await?;
                    position += count as u64;
                    #[cfg(test)]
                    if first_network_block {
                        first_network_block = false;
                        output.sync_all().await?;
                        self.guard_test_pause_at(GuardPhase::DownloadProgress, position)
                            .await;
                    }
                }
                Ok(())
            }
            .await;
            finish(
                transfer,
                output.sync_all().await.map_err(anyhow::Error::from),
            )
        }
        .await;
        finish(result, source.close().await.map_err(anyhow::Error::from))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let path = valid_path(path)?;
        if path.is_empty() {
            bail!("cannot create the configured SMB root");
        }
        self.check_ancestors(&path, false).await?;
        let mut args = FileCreateArgs::make_create_new(
            FileAttributes::new().with_directory(true),
            CreateOptions::new()
                .with_directory_file(true)
                .with_open_reparse_point(true),
        );
        args.desired_access = FileAccessMask::new().with_file_read_attributes(true);
        let resource = self.client.create_file(&self.unc(&path)?, &args).await?;
        handle(&resource).close().await?;
        Ok(())
    }

    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        let path = valid_path(path)?;
        if path.is_empty() {
            bail!("cannot delete the configured SMB root");
        }
        let current = self.stat(&path).await?.context("SMB path does not exist")?;
        if current.is_dir && !recursive {
            bail!(
                "directory removal requires --recursive; no deletion was requested from the server"
            );
        }
        if current.is_dir && recursive {
            let mut children = backend::walk(self, &path).await?;
            children.sort_by_key(|item| std::cmp::Reverse(item.path.matches('/').count()));
            for child in children {
                self.delete_one(&child.path, child.is_dir)
                    .await
                    .with_context(|| format!("remove SMB child {}", child.path))?;
            }
        }
        self.delete_one(&path, current.is_dir)
            .await
            .with_context(|| format!("remove SMB path {path}"))
    }

    async fn remove_checked(&self,path:&str,expected:&Entry)->Result<()>{
        let path=valid_path(path)?;
        if path.is_empty()||expected.path!=path||expected.is_dir{bail!("unsupported: guarded SMB deletion requires a non-root file revision");}
        require_strong_revision(expected)?;self.check_ancestors(&path,false).await?;
        let file=self.open_protected(&path,true).await?;
        let result=async{let actual=self.protected_entry(&path,&file).await?;if !actual.same_revision(expected){bail!("version_conflict: SMB delete source changed");}file.set_info(FileDispositionInformation::default()).await?;Ok(())}.await;
        finish(result,file.close().await.map_err(anyhow::Error::from))
    }

    fn namespace_matches(&self,expected:&Entry,actual:&Entry)->bool{
        if expected.is_dir&&actual.is_dir{
            let identity=|entry:&Entry|entry.etag.as_deref().and_then(|token|token.strip_prefix("W/smb-meta:")).map(|rest|rest.split(':').take(2).collect::<Vec<_>>().join(":"));
            identity(expected).is_some()&&identity(expected)==identity(actual)
        }else{expected.same_revision(actual)}
    }

    async fn rename_checked(&self,from:&str,to:&str,expected:&Entry)->Result<Entry>{
        let(from,to)=distinct_paths(from,to)?;
        if expected.path!=from{bail!("invalid SMB move revision");}
        self.check_ancestors(&from,false).await?;self.check_ancestors(&to,false).await?;
        if !expected.is_dir{
            require_strong_revision(expected)?;let file=self.open_protected(&from,true).await?;
            let result=async{let actual=self.protected_entry(&from,&file).await?;if !actual.same_revision(expected){bail!("version_conflict: SMB move source changed");}self.rename_handle(&file,&to).await?;let mut moved=actual;moved.path=to;Ok(moved)}.await;
            return finish(result,file.close().await.map_err(anyhow::Error::from));
        }
        let mut args=FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_read_attributes(true).with_delete(true));
        args.options=CreateOptions::new().with_directory_file(true).with_open_reparse_point(true);
        args.share_access=Some(ShareAccessFlags::new().with_read(true));
        let resource=self.client.create_file(&self.unc(&from)?,&args).await?;let file=handle(&resource);
        let result=async{
            let before=namespace_metadata(&from,file).await?;
            if !before.same_revision(expected){bail!("version_conflict: SMB directory move source changed");}
            file.set_info(FileRenameInformation{replace_if_exists:false.into(),root_directory:0,file_name:self.full_path(&to)?.replace('/',"\\").into()}).await?;
            namespace_metadata(&to,file).await
        }.await;
        finish(result,file.close().await.map_err(anyhow::Error::from))
    }

    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let (from, to) = distinct_paths(from, to)?;
        self.check_ancestors(&from, false).await?;
        self.check_ancestors(&to, false).await?;
        if overwrite {
            self.raw_stat(&self.full_path(&to)?).await?;
        }
        let mut args = FileCreateArgs::make_open_existing(
            FileAccessMask::new()
                .with_delete(true)
                .with_file_read_attributes(true),
        );
        args.options = CreateOptions::new().with_open_reparse_point(true);
        let resource = self.client.create_file(&self.unc(&from)?, &args).await?;
        let h = handle(&resource);
        let result = async {
            reject_reparse(
                h.query_info::<FileNetworkOpenInformation>()
                    .await?
                    .file_attributes,
            )?;
            // ReplaceIfExists is enforced by the server, rather than a preceding existence check.
            h.set_info(FileRenameInformation {
                replace_if_exists: overwrite.into(),
                root_directory: 0,
                file_name: self.full_path(&to)?.replace('/', "\\").into(),
            })
            .await?;
            Ok(())
        }
        .await;
        finish(result, h.close().await.map_err(anyhow::Error::from))
    }

    async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let (from, to) = distinct_paths(from, to)?;
        let source = self
            .stat(&from)
            .await?
            .context("SMB copy source does not exist")?;
        if !source.is_dir {
            return self.copy_file(&from, &to, overwrite).await;
        }
        // Directory merges are deliberately unsupported to avoid ambiguous partial overwrites.
        self.mkdir(&to).await?;
        let children = backend::walk(self, &from).await?;
        for child in children {
            let suffix = child
                .path
                .strip_prefix(&format!("{from}/"))
                .context("invalid SMB child path")?;
            let target = backend::join(&to, suffix)?;
            if child.is_dir {
                self.mkdir(&target).await?;
            } else {
                self.copy_file(&child.path, &target, false).await?;
            }
        }
        Ok(())
    }
}

fn handle(resource: &Resource) -> &ResourceHandle {
    match resource {
        Resource::File(file) => file.handle(),
        Resource::Directory(dir) => dir.handle(),
        Resource::Pipe(pipe) => pipe,
    }
}

async fn namespace_metadata(path:&str,file:&ResourceHandle)->Result<Entry>{
    let info=file.query_info::<FileNetworkOpenInformation>().await?;reject_reparse(info.file_attributes)?;
    let identity=stable_identity(file).await?;let mut result=entry(path,&info);
    result.etag=Some(format!("W/smb-meta:{:016x}:{:032x}:{}:{}:{}",identity.volume_serial_number,identity.file_id,info.end_of_file,info.last_write_time.since_epoch().as_nanos(),info.change_time.since_epoch().as_nanos()));Ok(result)
}

fn finish<T>(result: Result<T>, closed: Result<()>) -> Result<T> {
    match result {
        Ok(value) => closed.map(|()| value),
        Err(error) => Err(error),
    }
}

fn reject_reparse(attributes: FileAttributes) -> Result<()> {
    if attributes.reparse_point() {
        bail!("SMB symlinks and reparse points are not supported");
    }
    Ok(())
}

fn entry(path: &str, info: &FileNetworkOpenInformation) -> Entry {
    Entry {
        path: path.into(),
        is_dir: info.file_attributes.directory(),
        size: info.end_of_file,
        modified: unix_ms(info.last_write_time),
        etag: None,
    }
}

fn revision_token(hash: &str, identity: &FileIdInformation) -> String {
    format!(
        "smb-sha256:{hash}:{:016x}:{:032x}",
        identity.volume_serial_number, identity.file_id
    )
}

fn token_hash(token: Option<&str>) -> Option<String> {
    let mut parts = token?.split(':');
    if parts.next()? != "smb-sha256" {
        return None;
    }
    let hash = parts.next()?;
    let volume = parts.next()?;
    let id = parts.next()?;
    if parts.next().is_some()
        || hash.len() != 64
        || volume.len() != 16
        || id.len() != 32
        || ![hash, volume, id]
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(hash.to_ascii_lowercase())
}

fn token_identity(token: Option<&str>) -> Option<String> {
    token_hash(token)?;
    let mut parts = token?.split(':').skip(2);
    Some(format!("{}:{}", parts.next()?, parts.next()?))
}

async fn file_identity(file: &SmbFile) -> Result<String> {
    let identity = stable_identity(file).await?;
    Ok(format!(
        "{:016x}:{:032x}",
        identity.volume_serial_number, identity.file_id
    ))
}

async fn stable_identity(file: &ResourceHandle) -> Result<FileIdInformation> {
    match file.query_info::<FileIdInformation>().await {
        Ok(identity) => Ok(identity),
        Err(error)
            if matches!(&error,
            smb::Error::ReceivedErrorMessage(status, _) | smb::Error::UnexpectedMessageStatus(status)
                if matches!(smb::Status::try_from(*status).ok(), Some(smb::Status::InvalidInfoClass | smb::Status::NotSupported))) =>
        {
            let index = file
                .query_info::<FileInternalInformation>()
                .await
                .context("SMB server must supply stable internal file identity")?;
            let volume = file.query_fs_info::<FileFsVolumeInformation>().await?;
            Ok(FileIdInformation {
                volume_serial_number: volume.volume_serial_number as u64,
                file_id: index.index_number as u128,
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn open_local_input(path: &Path) -> Result<tokio::fs::File> {
    no_local_symlinks(path)?;
    if !std::fs::symlink_metadata(path)?.is_file() {
        bail!("local SMB source must be a regular file");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("local SMB source changed to a non-file");
    }
    Ok(tokio::fs::File::from_std(file))
}

fn read_upload_session(path: &Path) -> Result<Option<UploadSession>> {
    no_local_symlinks(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > 64 * 1024 {
        bail!("invalid SMB upload checkpoint file");
    }
    Ok(Some(serde_json::from_reader(file).context(
        "invalid SMB upload checkpoint; refusing to restart or overwrite",
    )?))
}

fn save_upload_session(path: &Path, session: &UploadSession) -> Result<()> {
    no_local_symlinks(path)?;
    crate::state::atomic_json(path, session)
}

fn validate_upload_session(
    session: &UploadSession,
    scope: &str,
    remote: &str,
    hash: &str,
    size: u64,
    intent: &UploadIntent,
) -> Result<()> {
    if session.version != 1
        || session.scope != scope
        || session.remote != remote
        || session.source_sha256 != hash
        || session.source_size != size
        || &session.intent != intent
        || session.confirmed_bytes > size
    {
        bail!(
            "SMB checkpoint belongs to a different source, target, credential scope or write condition"
        );
    }
    let parent = remote
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    let stage_parent = session
        .stage
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    if valid_path(&session.stage)? != session.stage
        || stage_parent != parent
        || !reserved_uuid_name(&session.stage, ".rdrive-upload-", "")
    {
        bail!("SMB checkpoint does not reference a valid private sibling stage");
    }
    if session.phase != UploadPhase::Prepared && session.stage_identity.is_none() {
        bail!(
            "SMB checkpoint lacks recorded stage identity; existing remote files are never adopted"
        );
    }
    if matches!(
        session.phase,
        UploadPhase::Staged | UploadPhase::Publishing | UploadPhase::Complete
    ) && (token_hash(session.stage_etag.as_deref()).as_deref() != Some(hash)
        || token_identity(session.stage_etag.as_deref()) != session.stage_identity)
    {
        bail!("SMB checkpoint strong staging revision is invalid");
    }
    if let Some(receipt) = &session.recovery {
        let backup_parent = receipt
            .backup
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let receipt_parent = receipt
            .receipt
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        if receipt.version != 1
            || receipt.target != remote
            || receipt.stage != session.stage
            || receipt.replacement_sha256 != hash
            || Some(&receipt.expected) != intent.expected.as_ref()
            || Some(&receipt.stage_etag) != session.stage_etag.as_ref()
            || backup_parent != parent
            || receipt_parent != parent
            || !reserved_uuid_name(&receipt.backup, ".rdrive-backup-", "")
            || !reserved_uuid_name(&receipt.receipt, ".rdrive-recovery-", ".json")
        {
            bail!("SMB recovery checkpoint contains inconsistent or unsafe paths/revisions");
        }
    }
    Ok(())
}

fn reserved_uuid_name(path: &str, prefix: &str, suffix: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id))
}

fn require_strong_revision(entry: &Entry) -> Result<()> {
    if entry.is_dir || token_hash(entry.etag.as_deref()).is_none() {
        bail!(
            "SMB guarded operation requires a strong SHA256 and file-identity revision; refresh the baseline"
        );
    }
    Ok(())
}

fn recovery_done_path(receipt: &str) -> Result<String> {
    if !reserved_uuid_name(receipt, ".rdrive-recovery-", ".json") {
        bail!("invalid SMB recovery receipt name");
    }
    let parent = receipt
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    let id = receipt
        .rsplit('/')
        .next()
        .unwrap()
        .strip_prefix(".rdrive-recovery-")
        .unwrap();
    backend::join(parent, &format!(".rdrive-recovery-done-{id}"))
}

fn recovery_done(receipt: &RecoveryReceipt) -> Result<RecoveryDone> {
    Ok(RecoveryDone {
        version: 1,
        receipt_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec_pretty(receipt)?)),
        target: receipt.target.clone(),
        stage_etag: receipt.stage_etag.clone(),
    })
}

fn validate_receipt(receipt: &RecoveryReceipt, path: &str) -> Result<()> {
    require_strong_revision(&receipt.expected)?;
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    if receipt.version != 1
        || receipt.receipt != path
        || receipt.expected.path != receipt.target
        || token_hash(Some(&receipt.stage_etag)).as_deref() != Some(&receipt.replacement_sha256)
    {
        bail!("invalid SMB recovery receipt revision or mapping");
    }
    for candidate in [&receipt.target, &receipt.stage, &receipt.backup] {
        if valid_path(candidate)? != *candidate
            || candidate
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("")
                != parent
        {
            bail!("SMB recovery receipt paths must be contained siblings");
        }
    }
    if !reserved_uuid_name(&receipt.stage, ".rdrive-upload-", "")
        || !reserved_uuid_name(&receipt.backup, ".rdrive-backup-", "")
    {
        bail!("invalid SMB recovery staging names");
    }
    Ok(())
}

fn internal_artifact(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    for prefix in [
        ".rdrive-recovery-done-",
        ".rdrive-upload-",
        ".rdrive-copy-",
        ".rdrive-backup-",
        ".rdrive-recovery-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let suffix = if prefix == ".rdrive-recovery-" || prefix == ".rdrive-recovery-done-" {
                suffix.strip_suffix(".json").unwrap_or("")
            } else {
                suffix
            };
            if uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.to_string() == suffix) {
                return true;
            }
        }
    }
    false
}

fn notification_filter() -> NotifyFilter {
    // Access-time notifications would make our own verification reads trigger
    // an endless rescan loop.
    NotifyFilter::new()
        .with_file_name(true)
        .with_dir_name(true)
        .with_attributes(true)
        .with_size(true)
        .with_last_write(true)
        .with_security(true)
}

async fn send_change(
    sender: &tokio::sync::mpsc::Sender<Result<ChangeEvent>>,
    cancel: &tokio_util::sync::CancellationToken,
    event: Result<ChangeEvent>,
) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        sent = sender.send(event) => sent.is_ok(),
    }
}

async fn hash_remote(file: &SmbFile, len: u64) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut offset = 0;
    while offset < len {
        let count = file
            .read_at(&mut buffer[..CHUNK.min((len - offset) as usize)], offset)
            .await?;
        if count == 0 {
            bail!("SMB file ended during strong revision verification");
        }
        hasher.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn unix_ms(time: FileTime) -> Option<i64> {
    if time.is_zero() {
        return None;
    }
    let duration = time
        .since_epoch()
        .checked_sub(Duration::from_secs(11_644_473_600))?;
    i64::try_from(duration.as_millis()).ok()
}

fn is_missing(error: &smb::Error) -> bool {
    match error {
        smb::Error::ReceivedErrorMessage(status, _)
        | smb::Error::UnexpectedMessageStatus(status) => matches!(
            smb::Status::try_from(*status).ok(),
            Some(smb::Status::ObjectNameNotFound | smb::Status::ObjectPathNotFound)
        ),
        _ => false,
    }
}

fn valid_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
    {
        bail!("unsafe or unsupported SMB path component: {name:?}");
    }
    Ok(())
}

fn valid_path(path: &str) -> Result<String> {
    let normalized = backend::normalize(path)?;
    for part in normalized.split('/').filter(|s| !s.is_empty()) {
        valid_component(part)?;
    }
    Ok(normalized)
}

fn distinct_paths(from: &str, to: &str) -> Result<(String, String)> {
    let from = valid_path(from)?;
    let to = valid_path(to)?;
    if from.is_empty() || to.is_empty() {
        bail!("cannot rename or copy the configured SMB root");
    }
    // SMB is normally case insensitive. Conservatively reject these even on case-sensitive shares.
    let source = from.to_lowercase();
    let target = to.to_lowercase();
    if source == target || target.starts_with(&format!("{source}/")) {
        bail!("destination must differ from source and cannot be its descendant");
    }
    Ok((from, to))
}

fn decode_segment(segment: &str) -> Result<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("invalid percent escape in SMB URL");
            }
            let high = (bytes[index + 1] as char)
                .to_digit(16)
                .context("invalid percent escape")?;
            let low = (bytes[index + 2] as char)
                .to_digit(16)
                .context("invalid percent escape")?;
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).context("SMB URL path must be UTF-8")?;
    valid_component(&decoded)?;
    Ok(decoded)
}

fn parse_target(address: &str) -> Result<(url::Url, String, String)> {
    let url = url::Url::parse(address)?;
    if url.scheme() != "smb" {
        bail!("expected smb:// URL");
    }
    // Read the original path because URL parsing itself normalizes dot segments.
    let authority_and_path = address.split_once("://").context("invalid SMB URL")?.1;
    let raw_path = authority_and_path
        .split_once('/')
        .context("SMB URL requires a share")?
        .1;
    let mut parts = Vec::new();
    for part in raw_path.trim_end_matches('/').split('/') {
        parts.push(decode_segment(part)?);
    }
    if parts.is_empty() {
        bail!("SMB URL requires a share");
    }
    let share = parts.remove(0);
    Ok((url, share, parts.join("/")))
}

fn no_local_symlinks(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut prefix = PathBuf::new();
    for part in absolute.components() {
        if matches!(part, Component::ParentDir) {
            bail!("parent traversal in local transfer path is unsupported");
        }
        prefix.push(part.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => bail!(
                "local transfer path contains a symlink: {}",
                prefix.display()
            ),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn no_follow(options: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = options;
}

async fn write_all_at(file: &SmbFile, bytes: &[u8], offset: u64) -> Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let count = file
            .write_at(&bytes[written..], offset + written as u64)
            .await?;
        if count == 0 {
            return Err(anyhow!("SMB server accepted a zero-byte write"));
        }
        written += count;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_smb_aliases_and_traversal() {
        for path in [
            "../x",
            "a/../x",
            "x:stream",
            "a\\b",
            "trailing.",
            "trailing ",
            "a/*",
            "a/\0b",
        ] {
            assert!(valid_path(path).is_err(), "accepted {path:?}");
        }
        for address in [
            "smb://host/share/%2e%2e/outside",
            "smb://host/share/../outside",
            "smb://host/share/a%2fb",
            "smb://host/share/a%5cb",
            "smb://host/share/%00",
        ] {
            assert!(parse_target(address).is_err(), "accepted {address}");
        }
        let (_, share, root) = parse_target("smb://host/share/folder%20one/nested/").unwrap();
        assert_eq!(share, "share");
        assert_eq!(root, "folder one/nested");
        assert!(distinct_paths("folder", "FOLDER/child").is_err());
    }
    #[test]
    fn weak_guarded_overwrite_is_rejected() {
        let revision = Entry {
            path: "a".into(),
            is_dir: false,
            size: 1,
            modified: Some(0),
            etag: None,
        };
        assert!(Smb::write_args(WriteCondition::Match(revision)).is_err());
        assert!(matches!(
            Smb::write_args(WriteCondition::CreateNew)
                .unwrap()
                .disposition,
            CreateDisposition::Create
        ));
    }
    #[test]
    fn filetime_conversion() {
        assert_eq!(unix_ms(FileTime::ZERO), None);
        assert_eq!(
            unix_ms(FileTime::from(116_444_736_010_000_000u64)),
            Some(1000)
        );
    }
    #[cfg(unix)]
    #[test]
    fn local_links_are_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().canonicalize().unwrap();
        std::fs::write(real.join("source"), "data").unwrap();
        symlink(real.join("source"), real.join("link")).unwrap();
        assert!(no_local_symlinks(&real.join("link")).is_err());
        assert!(no_local_symlinks(&real.join("source")).is_ok());
    }

    fn fixture_profile() -> Profile {
        Profile {
            protocol: crate::config::Protocol::Smb,
            url: std::env::var("RDRIVE_TEST_SMB_URL").expect("requires isolated fixture URL"),
            username: Some("test".into()),
            password_env: None,
            allow_http: false,
            timeout_secs: 15,
            smb_encryption: true,
            nextcloud_upload_url: None,
        }
    }

    fn sharing_violation(error: &smb::Error) -> bool {
        matches!(error, smb::Error::ReceivedErrorMessage(status, _) | smb::Error::UnexpectedMessageStatus(status)
            if smb::Status::try_from(*status).ok() == Some(smb::Status::SharingViolation))
    }

    #[tokio::test]
    #[ignore = "requires the isolated Samba fixture from tests/smb_workflows.py"]
    async fn guarded_smb_integration() -> Result<()> {
        let profile = fixture_profile();
        let mut first = Smb::new(&profile, "testpass").await?;
        let second = Smb::new(&profile, "testpass").await?;
        let prefix = format!("guard-proof-{}", uuid::Uuid::new_v4());
        first.mkdir(&prefix).await?;
        let remote = backend::join(&prefix, "target.bin")?;
        let directory = tempfile::tempdir()?;
        let local = directory.path().canonicalize()?.join("source");
        std::fs::write(&local, b"original")?;
        first
            .upload(&local, &remote, WriteCondition::CreateNew)
            .await
            .context("create lifecycle source")?;
        let before = first.stat(&remote).await?.unwrap();
        let info = first.raw_stat(&first.full_path(&remote)?).await?.unwrap();

        // A same-size edit with the old LastWriteTime restored must still conflict.
        std::fs::write(&local, b"competed")?;
        second.upload(&local, &remote, WriteCondition::Any).await?;
        let args = FileCreateArgs::make_open_existing(
            FileAccessMask::new().with_file_write_attributes(true),
        );
        let resource = second
            .client
            .create_file(&second.unc(&remote)?, &args)
            .await?;
        handle(&resource)
            .set_info(smb::FileBasicInformation {
                creation_time: FileTime::ZERO,
                last_access_time: FileTime::ZERO,
                last_write_time: info.last_write_time,
                change_time: FileTime::ZERO,
                file_attributes: FileAttributes::new(),
            })
            .await?;
        handle(&resource).close().await?;
        std::fs::write(&local, b"new-data")?;
        assert!(
            first
                .upload(&local, &remote, WriteCondition::Match(before))
                .await
                .is_err()
        );
        let current = first.stat(&remote).await?.unwrap();
        assert_eq!(
            first.content_sha256(&current),
            Some(format!("{:x}", Sha256::digest(b"competed")))
        );

        // A second ordinary SMB client cannot obtain WRITE or DELETE while comparison is protected.
        let hooks = Arc::new(GuardTestHooks {
            phase: GuardPhase::Compared,
            reached: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            offset: std::sync::atomic::AtomicU64::new(0),
        });
        first.guard_hooks = Some(hooks.clone());
        let first = Arc::new(first);
        let task = {
            let first = first.clone();
            let local = local.clone();
            let remote = remote.clone();
            tokio::spawn(async move {
                first
                    .upload(&local, &remote, WriteCondition::Match(current))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(30), hooks.reached.notified()).await?;
        for access in [
            FileAccessMask::new().with_file_write_data(true),
            FileAccessMask::new().with_delete(true),
        ] {
            let args = FileCreateArgs::make_open_existing(access);
            match second
                .client
                .create_file(&second.unc(&remote)?, &args)
                .await
            {
                Err(error) => assert!(sharing_violation(&error), "unexpected refusal: {error}"),
                Ok(resource) => {
                    let _ = handle(&resource).close().await;
                    panic!("server allowed conflicting writer/delete handle");
                }
            }
        }
        hooks.resume.notify_one();
        let written = task.await??;
        assert!(second.stat(&remote).await?.unwrap().same_revision(&written));
        let entries = second.list(&prefix).await?;
        let backup = entries
            .iter()
            .find(|e| e.path.contains(".rdrive-backup-"))
            .unwrap();
        let old = second.stat(&backup.path).await?.unwrap();
        assert_eq!(
            second.content_sha256(&old),
            Some(format!("{:x}", Sha256::digest(b"competed")))
        );
        let completed_receipt = entries
            .iter()
            .find(|e| reserved_uuid_name(&e.path, ".rdrive-recovery-", ".json"))
            .unwrap()
            .path
            .clone();
        let receipt = second.read_receipt(&completed_receipt).await?;
        let done_path = recovery_done_path(&completed_receipt)?;
        second.remove(&done_path, false).await?;
        second.check_sync_recovery(&completed_receipt).await?;
        assert!(
            second
                .raw_stat(&second.full_path(&done_path)?)
                .await?
                .is_none(),
            "sync planning wrote a completion marker"
        );
        second.write_recovery_done(&receipt).await?;

        // If another client creates the original name during the namespace gap, no-replace wins.
        let mut third = Smb::new(&profile, "testpass").await?;
        let hooks = Arc::new(GuardTestHooks {
            phase: GuardPhase::BackedUp,
            reached: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            offset: std::sync::atomic::AtomicU64::new(0),
        });
        third.guard_hooks = Some(hooks.clone());
        let third = Arc::new(third);
        let baseline = second.stat(&remote).await?.unwrap();
        std::fs::write(&local, b"proposed")?;
        let task = {
            let third = third.clone();
            let local = local.clone();
            let remote = remote.clone();
            tokio::spawn(async move {
                third
                    .upload(&local, &remote, WriteCondition::Match(baseline))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(30), hooks.reached.notified()).await?;
        std::fs::write(&local, b"outsider")?;
        second
            .upload(&local, &remote, WriteCondition::CreateNew)
            .await?;
        hooks.resume.notify_one();
        let error = task
            .await?
            .expect_err("publication should preserve the new target");
        assert!(format!("{error:#}").contains("recovery"));
        let retained = second.stat(&remote).await?.unwrap();
        assert_eq!(
            second.content_sha256(&retained),
            Some(format!("{:x}", Sha256::digest(b"outsider")))
        );
        let entries = second.list(&prefix).await?;
        assert!(
            entries
                .iter()
                .filter(|e| e.path.contains(".rdrive-backup-"))
                .count()
                >= 2
        );
        let mut pending = 0;
        for receipt in entries
            .iter()
            .filter(|e| reserved_uuid_name(&e.path, ".rdrive-recovery-", ".json"))
        {
            if second.check_sync_recovery(&receipt.path).await.is_err() {
                pending += 1;
            }
        }
        assert_eq!(
            pending, 1,
            "unfinished namespace gap must block sync without invalidating completed receipts"
        );
        assert!(
            entries
                .iter()
                .filter(|e| e.path.contains(".rdrive-recovery-"))
                .count()
                >= 2
        );
        Ok(())
    }

    fn test_hooks(phase: GuardPhase) -> Arc<GuardTestHooks> {
        Arc::new(GuardTestHooks {
            phase,
            reached: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            offset: std::sync::atomic::AtomicU64::new(0),
        })
    }

    #[tokio::test]
    #[ignore = "requires the isolated Samba fixture from tests/smb_workflows.py"]
    async fn resumable_smb_integration() -> Result<()> {
        let profile = fixture_profile();
        let mut first = Smb::new(&profile, "testpass").await?;
        let mut second = Smb::new(&profile, "testpass").await?;
        let prefix = format!("resume-proof-{}", uuid::Uuid::new_v4());
        first.mkdir(&prefix).await?;
        let remote = backend::join(&prefix, "target.bin")?;
        let directory = tempfile::tempdir()?;
        let directory = directory.path().canonicalize()?;
        let local = directory.join("source");
        let bytes: Vec<u8> = (0..(CHUNK * 35 + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&local, &bytes)?;
        let checkpoint = directory.join("upload.json");

        let hooks = test_hooks(GuardPhase::UploadProgress);
        first.guard_hooks = Some(hooks.clone());
        let first = Arc::new(first);
        let task = {
            let first = first.clone();
            let local = local.clone();
            let remote = remote.clone();
            let checkpoint = checkpoint.clone();
            tokio::spawn(async move {
                first
                    .upload_resume(&local, &remote, WriteCondition::CreateNew, &checkpoint)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(60), hooks.reached.notified()).await?;
        let durable_offset = hooks.offset.load(std::sync::atomic::Ordering::SeqCst);
        assert!(durable_offset >= 1024 * 1024 && durable_offset < bytes.len() as u64);
        task.abort();
        let _ = task.await;
        let mut session = read_upload_session(&checkpoint)?.unwrap();
        assert_eq!(session.phase, UploadPhase::Uploading);
        // Deliberately stale local offset: actual verified server length must win.
        session.confirmed_bytes = 0;
        save_upload_session(&checkpoint, &session)?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match second.open_staging(&session.stage, false).await {
                    Ok(file) => {
                        file.close().await?;
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await??;
        let hooks = test_hooks(GuardPhase::ResumeFirstWrite);
        second.guard_hooks = Some(hooks.clone());
        let second = Arc::new(second);
        let task = {
            let second = second.clone();
            let local = local.clone();
            let remote = remote.clone();
            let checkpoint = checkpoint.clone();
            tokio::spawn(async move {
                second
                    .upload_resume(&local, &remote, WriteCondition::CreateNew, &checkpoint)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(60), hooks.reached.notified()).await?;
        assert_eq!(
            hooks.offset.load(std::sync::atomic::Ordering::SeqCst),
            durable_offset
        );
        hooks.resume.notify_one();
        let written = task.await??;
        assert_eq!(
            token_hash(written.etag.as_deref()),
            Some(format!("{:x}", Sha256::digest(&bytes)))
        );
        assert_eq!(
            read_upload_session(&checkpoint)?.unwrap().phase,
            UploadPhase::Complete
        );

        // Saved prefix is checked byte-for-byte; resume starts at the existing nonzero boundary.
        let mut downloader = Smb::new(&profile, "testpass").await?;
        let hooks = test_hooks(GuardPhase::DownloadFirstWrite);
        downloader.guard_hooks = Some(hooks.clone());
        let downloader = Arc::new(downloader);
        let partial = directory.join("partial");
        std::fs::write(&partial, &bytes[..CHUNK + 13])?;
        let expected = second.stat(&remote).await?.unwrap();
        let task = {
            let downloader = downloader.clone();
            let remote = remote.clone();
            let partial = partial.clone();
            let expected = expected.clone();
            tokio::spawn(async move {
                downloader
                    .download_resume(&remote, &partial, &expected, (CHUNK + 13) as u64)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(60), hooks.reached.notified()).await?;
        assert_eq!(
            hooks.offset.load(std::sync::atomic::Ordering::SeqCst),
            (CHUNK + 13) as u64
        );
        hooks.resume.notify_one();
        task.await??;
        assert_eq!(std::fs::read(&partial)?, bytes);
        std::fs::write(&partial, b"corrupt-prefix")?;
        assert!(
            second
                .download_resume(&remote, &partial, &expected, 14)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&partial)?, b"corrupt-prefix");

        // Ambiguous publication is reconciled by recorded stage identity, without another rename.
        let mut publisher = Smb::new(&profile, "testpass").await?;
        let hooks = test_hooks(GuardPhase::Published);
        publisher.guard_hooks = Some(hooks.clone());
        let publisher = Arc::new(publisher);
        let final_remote = backend::join(&prefix, "published.bin")?;
        let final_checkpoint = directory.join("published.json");
        let task = {
            let publisher = publisher.clone();
            let local = local.clone();
            let remote = final_remote.clone();
            let checkpoint = final_checkpoint.clone();
            tokio::spawn(async move {
                publisher
                    .upload_resume(&local, &remote, WriteCondition::CreateNew, &checkpoint)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(60), hooks.reached.notified()).await?;
        task.abort();
        let _ = task.await;
        let reconciled = second
            .upload_resume(
                &local,
                &final_remote,
                WriteCondition::CreateNew,
                &final_checkpoint,
            )
            .await?;
        assert_eq!(
            token_hash(reconciled.etag.as_deref()),
            Some(format!("{:x}", Sha256::digest(&bytes)))
        );
        assert_eq!(
            read_upload_session(&final_checkpoint)?.unwrap().phase,
            UploadPhase::Complete
        );

        // Replacing an upload stage in place must be detected even when identity/length survive.
        let tampered_remote = backend::join(&prefix, "tampered.bin")?;
        let stage = backend::join(&prefix, &format!(".rdrive-upload-{}", uuid::Uuid::new_v4()))?;
        let staged = second.open_staging(&stage, true).await?;
        let identity = file_identity(&staged).await?;
        write_all_at(&staged, &bytes[..CHUNK], 0).await?;
        staged.flush().await?;
        staged.close().await?;
        let tampered_checkpoint = directory.join("tampered.json");
        let session = UploadSession {
            version: 1,
            scope: second.scope.clone(),
            remote: tampered_remote.clone(),
            stage: stage.clone(),
            source_sha256: format!("{:x}", Sha256::digest(&bytes)),
            source_size: bytes.len() as u64,
            intent: UploadIntent::from_condition(&WriteCondition::CreateNew),
            phase: UploadPhase::Uploading,
            stage_identity: Some(identity),
            stage_etag: None,
            confirmed_bytes: CHUNK as u64,
            recovery: None,
        };
        save_upload_session(&tampered_checkpoint, &session)?;
        let attacker = second.open_staging(&stage, false).await?;
        write_all_at(&attacker, &[255], 0).await?;
        attacker.flush().await?;
        attacker.close().await?;
        let error = second
            .upload_resume(
                &local,
                &tampered_remote,
                WriteCondition::CreateNew,
                &tampered_checkpoint,
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("prefix differs"));
        assert!(second.stat(&tampered_remote).await?.is_none());
        assert_eq!(
            read_upload_session(&tampered_checkpoint)?.unwrap().phase,
            UploadPhase::Uploading
        );

        // Matching content at an unrelated replacement identity is not a recovered acknowledgement.
        let prior_identity = reconciled.etag.clone();
        second
            .upload(&local, &final_remote, WriteCondition::Any)
            .await?;
        let foreign = second.stat(&final_remote).await?.unwrap();
        assert_ne!(foreign.etag, prior_identity);
        assert!(
            second
                .upload_resume(
                    &local,
                    &final_remote,
                    WriteCondition::CreateNew,
                    &final_checkpoint
                )
                .await
                .is_err()
        );
        assert!(
            second
                .stat(&final_remote)
                .await?
                .unwrap()
                .same_revision(&foreign)
        );

        // A crash after moving the protected original is recoverable without exposing deletions.
        let gap_remote = backend::join(&prefix, "gap.bin")?;
        std::fs::write(&local, b"old-gap")?;
        second
            .upload(&local, &gap_remote, WriteCondition::CreateNew)
            .await?;
        let gap_expected = second.stat(&gap_remote).await?.unwrap();
        std::fs::write(&local, b"new-gap")?;
        let mut interrupted = Smb::new(&profile, "testpass").await?;
        let hooks = test_hooks(GuardPhase::BackedUp);
        interrupted.guard_hooks = Some(hooks.clone());
        let interrupted = Arc::new(interrupted);
        let gap_checkpoint = directory.join("gap.json");
        let task = {
            let interrupted = interrupted.clone();
            let local = local.clone();
            let remote = gap_remote.clone();
            let expected = gap_expected.clone();
            let checkpoint = gap_checkpoint.clone();
            tokio::spawn(async move {
                interrupted
                    .upload_resume(
                        &local,
                        &remote,
                        WriteCondition::Match(expected),
                        &checkpoint,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(30), hooks.reached.notified()).await?;
        task.abort();
        let _ = task.await;
        let gap_session = read_upload_session(&gap_checkpoint)?.unwrap();
        let receipt = gap_session.recovery.as_ref().unwrap();
        assert!(second.stat(&gap_remote).await?.is_none());
        assert!(second.check_sync_recovery(&receipt.receipt).await.is_err());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match second.open_protected(&receipt.backup, true).await {
                    Ok(file) => {
                        file.close().await?;
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await??;
        let recovered = second
            .upload_resume(
                &local,
                &gap_remote,
                WriteCondition::Match(gap_expected),
                &gap_checkpoint,
            )
            .await?;
        assert_eq!(
            token_hash(recovered.etag.as_deref()),
            Some(format!("{:x}", Sha256::digest(b"new-gap")))
        );
        second.check_sync_recovery(&receipt.receipt).await?;
        let old = second.stat(&receipt.backup).await?.unwrap();
        assert_eq!(
            second.content_sha256(&old),
            Some(format!("{:x}", Sha256::digest(b"old-gap")))
        );
        Ok(())
    }

    async fn next_change(
        receiver: &mut tokio::sync::mpsc::Receiver<Result<ChangeEvent>>,
        path: &str,
        kinds: &[&str],
    ) -> Result<ChangeEvent> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = receiver.recv().await.context("SMB change feed ended")??;
                if event.path == path && kinds.contains(&event.kind.as_str()) {
                    return Ok(event);
                }
            }
        })
        .await?
    }

    #[tokio::test]
    #[ignore = "requires the isolated lifecycle-enabled Samba fixture"]
    async fn lifecycle_smb_integration() -> Result<()> {
        use smb::resource::{DurableContext, LifecycleRequest};
        let profile = fixture_profile();
        let first = Smb::new(&profile, "testpass").await?;
        let second = Smb::new(&profile, "testpass").await?;
        let prefix = format!("lifecycle-proof-{}", uuid::Uuid::new_v4());
        first.mkdir(&prefix).await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut events = first.change_feed(&prefix, true, cancel.clone()).await?;
        next_change(&mut events, &prefix, &["ready"]).await?;
        let original = backend::join(&prefix, "original.bin")?;
        let mut args = FileCreateArgs::make_create_new(
            FileAttributes::new(),
            CreateOptions::new().with_non_directory_file(true),
        );
        args.desired_access = FileAccessMask::new()
            .with_generic_read(true)
            .with_generic_write(true);
        let file = second.open_file(&original, &args).await?;
        write_all_at(&file, b"notification", 0).await?;
        file.flush().await?;
        file.close().await?;
        next_change(&mut events, &original, &["added"]).await?;
        let renamed = backend::join(&prefix, "renamed.bin")?;
        second.rename(&original, &renamed, false).await?;
        next_change(&mut events, &original, &["rename_old"]).await?;
        next_change(&mut events, &renamed, &["rename_new"]).await?;
        second.remove(&renamed, false).await?;
        next_change(&mut events, &renamed, &["removed"]).await?;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.recv().await.is_some() {}
        })
        .await?;
        println!("PROOF SMB CHANGE_NOTIFY registered, created, renamed, removed, canceled");

        let mut overflow_watcher = Smb::new(&profile, "testpass").await?;
        overflow_watcher.notify_buffer = Some(16);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut overflow = overflow_watcher
            .change_feed(&prefix, false, cancel.clone())
            .await?;
        next_change(&mut overflow, &prefix, &["ready"]).await?;
        let long_name = backend::join(&prefix, &format!("{}.bin", "n".repeat(120)))?;
        let file = second.open_file(&long_name, &args).await?;
        file.close().await?;
        next_change(&mut overflow, &prefix, &["rescan_required"]).await?;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while overflow.recv().await.is_some() {}
        })
        .await?;
        println!("PROOF real SMB notification overflow requires rescan");

        let directory = tempfile::tempdir()?;
        let local = directory.path().canonicalize()?.join("source");
        std::fs::write(&local, b"lease and durable bytes")?;
        let remote = backend::join(&prefix, "lease.bin")?;
        first
            .upload(&local, &remote, WriteCondition::CreateNew)
            .await?;
        let mut read_args = FileCreateArgs::make_open_existing(
            FileAccessMask::new()
                .with_file_read_data(true)
                .with_file_read_attributes(true),
        );
        read_args.options = CreateOptions::new()
            .with_non_directory_file(true)
            .with_open_reparse_point(true);
        read_args.lifecycle = Some(LifecycleRequest::read_handle(
            smb::Guid::generate(),
            smb::Guid::generate(),
            60_000,
        ));
        let reader = first
            .open_file(&remote, &read_args)
            .await
            .context("request RH lease and durable-v2 open")?;
        let lease = reader
            .lease_info()
            .context("server did not grant RH lease")?;
        assert!(lease.granted() && lease.state().read_caching() && lease.state().handle_caching());
        assert!(
            reader.durable_info().is_some(),
            "server did not grant durable-v2 handle"
        );
        let write_args = FileCreateArgs::make_open_existing(
            FileAccessMask::new()
                .with_file_write_data(true)
                .with_file_read_attributes(true),
        );
        let writer = tokio::time::timeout(
            Duration::from_secs(5),
            second.open_file(&remote, &write_args),
        )
        .await?
        .context("competing writer after lease break")?;
        write_all_at(&writer, b"l", 0).await?;
        writer.flush().await?;
        writer.close().await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while lease.breaks() == 0 || lease.acknowledgements() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "lease break not acknowledged: breaks={}, acks={}, state={:x}",
                lease.breaks(),
                lease.acknowledgements(),
                u32::from_le_bytes(lease.state().into_bytes())
            )
        })?;
        reader.close().await?;
        println!("PROOF RH lease grant and actual lease-break acknowledgement");

        read_args.lifecycle = Some(LifecycleRequest::read_handle(
            smb::Guid::generate(),
            smb::Guid::generate(),
            60_000,
        ));
        let reader = first.open_file(&remote, &read_args).await?;
        let durable = reader
            .durable_info()
            .context("server did not grant durable handle")?;
        let expected = first.protected_entry(&remote, &reader).await?;
        let server = url::Url::parse(&profile.url)?
            .host_str()
            .unwrap()
            .to_owned();
        let disconnect_cancel = tokio_util::sync::CancellationToken::new();
        let mut disconnected_feed = first
            .change_feed(&prefix, true, disconnect_cancel.clone())
            .await?;
        next_change(&mut disconnected_feed, &prefix, &["ready"]).await?;
        first.client.get_connection(&server).await?.close().await?;
        next_change(&mut disconnected_feed, &prefix, &["rescan_required"]).await?;
        disconnect_cancel.cancel();
        reader.abandon();
        let reauthenticated = Client::new(first.client.config().clone());
        reauthenticated
            .share_connect(&first.share, "test", "testpass".into())
            .await?;
        read_args.lifecycle = durable.reconnect_request();
        assert!(matches!(
            read_args.lifecycle.unwrap().durable,
            DurableContext::Reconnect { .. }
        ));
        let resumed = match reauthenticated
            .create_file(&first.unc(&remote)?, &read_args)
            .await?
        {
            Resource::File(file) => SmbFile::new(file, first.transfer.clone(), &remote),
            other => {
                handle(&other).close().await?;
                bail!("durable reconnect returned non-file");
            }
        };
        let reconnected = resumed
            .durable_info()
            .context("durable reconnect metadata missing")?;
        assert_eq!(reconnected.file_id.persistent, durable.file_id.persistent);
        assert!(
            first
                .protected_entry(&remote, &resumed)
                .await?
                .same_revision(&expected)
        );
        resumed.close().await?;
        println!(
            "PROOF TCP disconnect, fresh authenticated session, DH2C reconnect, stable file identity/content"
        );
        let resubscribe_cancel = tokio_util::sync::CancellationToken::new();
        let mut reconnected_feed = first
            .change_feed(&prefix, true, resubscribe_cancel.clone())
            .await?;
        next_change(&mut reconnected_feed, &prefix, &["ready"]).await?;
        resubscribe_cancel.cancel();
        println!(
            "PROOF notification disconnect requires rescan and fresh authenticated resubscription"
        );
        let bytes: Vec<u8> = (0..(CHUNK * 5 + 11)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&local, &bytes)?;
        let payload = backend::join(&prefix, "automatic.bin")?;
        second
            .upload(&local, &payload, WriteCondition::CreateNew)
            .await?;
        let expected = second.stat(&payload).await?.unwrap();
        let context = TransferContext::default();
        let metadata = Smb::new(&profile, "testpass")
            .await?
            .with_transfer_context(context.clone());
        assert!(
            metadata
                .stat_metadata(&payload)
                .await?
                .unwrap()
                .etag
                .unwrap()
                .starts_with("W/smb-meta:")
        );
        metadata.list_metadata(&prefix).await?;
        assert_eq!(
            context.snapshot().transferred,
            0,
            "metadata enumeration must read no file payload"
        );
        metadata.stat(&payload).await?;
        assert!(
            context.snapshot().transferred >= bytes.len() as u64,
            "strong revision verification must be metered"
        );
        println!(
            "PROOF metadata-only enumeration reads zero file payload; full revision hashes are metered"
        );

        for scenario in ["tcp", "restart", "wrong-password", "changed-content"] {
            let hooks = test_hooks(GuardPhase::DownloadProgress);
            let mut downloader = Smb::new(&profile, "testpass").await?;
            downloader.guard_hooks = Some(hooks.clone());
            let downloader = Arc::new(downloader);
            let partial = local.parent().unwrap().join(format!("download-{scenario}"));
            let mut task = {
                let downloader = downloader.clone();
                let payload = payload.clone();
                let partial = partial.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    downloader
                        .download_resume(&payload, &partial, &expected, 0)
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                tokio::select! {
                    _ = hooks.reached.notified() => Ok(()),
                    result = &mut task => bail!("download ended before fault hook ({scenario}): {result:?}"),
                }
            }).await.with_context(|| format!("download fault hook not reached ({scenario})"))??;
            let boundary = hooks.offset.load(std::sync::atomic::Ordering::SeqCst) as usize;
            assert!(boundary > 0 && boundary < bytes.len());
            assert_eq!(std::fs::read(&partial)?, bytes[..boundary]);
            downloader
                .client
                .get_connection(&server)
                .await?
                .close()
                .await?;
            match scenario {
                "restart" => {
                    fixture_docker(&["restart"]).await?;
                    fixture_wait_ready().await?;
                }
                "wrong-password" => {
                    *downloader.client.password.lock().unwrap() = "wrong-synthetic-password".into()
                }
                "changed-content" => {
                    fixture_docker(&["exec", "sh", "-c", &format!("printf X | dd of=/share/{payload} bs=1 seek={} conv=notrunc 2>/dev/null", boundary + 1)]).await?;
                }
                _ => (),
            }
            hooks.resume.notify_one();
            let outcome = tokio::time::timeout(Duration::from_secs(60), task).await??;
            let diagnostics = downloader.diagnostics();
            match scenario {
                "tcp" => {
                    outcome?;
                    assert_eq!(std::fs::read(&partial)?, bytes);
                    assert_eq!(diagnostics["durable_reconnects"], 1);
                }
                "restart" => {
                    outcome?;
                    assert_eq!(std::fs::read(&partial)?, bytes);
                    assert_eq!(diagnostics["verified_reopens"], 1);
                }
                _ => {
                    assert!(outcome.is_err(), "unsafe recovery must fail: {scenario}");
                    assert_eq!(
                        std::fs::read(&partial)?,
                        bytes[..boundary],
                        "no bytes may append before reconnect verification"
                    );
                }
            }
            println!("PROOF automatic download recovery {scenario}: {diagnostics}");
        }
        let upload_bytes: Vec<u8> = (0..(1024 * 1024 + CHUNK + 19))
            .map(|i| (i % 239) as u8)
            .collect();
        std::fs::write(&local, &upload_bytes)?;
        for scenario in ["tcp", "restart", "wrong-password", "tampered-prefix"] {
            let hooks = test_hooks(GuardPhase::UploadProgress);
            let mut uploader = Smb::new(&profile, "testpass").await?;
            uploader.guard_hooks = Some(hooks.clone());
            let uploader = Arc::new(uploader);
            let remote = backend::join(&prefix, &format!("upload-{scenario}.bin"))?;
            let checkpoint = local
                .parent()
                .unwrap()
                .join(format!("upload-{scenario}.json"));
            let task = {
                let uploader = uploader.clone();
                let local = local.clone();
                let remote = remote.clone();
                let checkpoint = checkpoint.clone();
                tokio::spawn(async move {
                    uploader
                        .upload_resume(&local, &remote, WriteCondition::CreateNew, &checkpoint)
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(30), hooks.reached.notified()).await?;
            let session = read_upload_session(&checkpoint)?.unwrap();
            assert!(session.confirmed_bytes > 0 && session.confirmed_bytes < session.source_size);
            uploader
                .client
                .get_connection(&server)
                .await?
                .close()
                .await?;
            match scenario {
                "restart" => {
                    fixture_docker(&["restart"]).await?;
                    fixture_wait_ready().await?;
                }
                "wrong-password" => {
                    *uploader.client.password.lock().unwrap() = "wrong-synthetic-password".into()
                }
                "tampered-prefix" => {
                    fixture_docker(&[
                        "exec",
                        "sh",
                        "-c",
                        &format!(
                            "printf X | dd of=/share/{} bs=1 seek=1 conv=notrunc 2>/dev/null",
                            session.stage
                        ),
                    ])
                    .await?
                }
                _ => (),
            }
            hooks.resume.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(60), task).await??;
            if matches!(scenario, "tcp" | "restart") {
                let written = result?;
                assert_eq!(
                    uploader.content_sha256(&written),
                    Some(format!("{:x}", Sha256::digest(&upload_bytes)))
                );
                assert_eq!(
                    read_upload_session(&checkpoint)?.unwrap().phase,
                    UploadPhase::Complete
                );
            } else {
                assert!(result.is_err());
                assert!(
                    Smb::new(&profile, "testpass")
                        .await?
                        .stat_metadata(&remote)
                        .await?
                        .is_none()
                );
                assert!(checkpoint.is_file());
            }
            println!(
                "PROOF automatic checkpointed upload recovery {scenario}: {}",
                uploader.diagnostics()
            );
        }
        Ok(())
    }

    async fn fixture_docker(arguments: &[&str]) -> Result<()> {
        let docker = std::env::var("RDRIVE_TEST_DOCKER")?;
        let container = std::env::var("RDRIVE_TEST_SMB_CONTAINER")?;
        let (verb, rest) = arguments.split_first().context("missing Docker action")?;
        let args: Vec<String> = [
            vec![verb.to_string(), container],
            rest.iter().map(|arg| arg.to_string()).collect(),
        ]
        .concat();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(docker).args(args).output()
        })
        .await??;
        if !output.status.success() {
            bail!(
                "synthetic fixture action failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    async fn fixture_wait_ready() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if fixture_docker(&["exec", "smbcontrol", "smbd", "ping"])
                    .await
                    .is_ok()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn full_notification_queue_can_be_cancelled() -> Result<()> {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(Ok(ChangeEvent {
                path: String::new(),
                kind: "ready".into(),
            }))
            .await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancellation = cancel.clone();
        let pending = tokio::spawn(async move {
            send_change(
                &sender,
                &cancellation,
                Ok(ChangeEvent {
                    path: "file".into(),
                    kind: "added".into(),
                }),
            )
            .await
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(!tokio::time::timeout(Duration::from_secs(1), pending).await??);
        Ok(())
    }
}

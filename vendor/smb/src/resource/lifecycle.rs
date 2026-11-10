//! Lease and durable-open state. This client does not keep dirty data caches.
use smb_dtyp::Guid;
use smb_msg::{FileId, LeaseState};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
};

#[derive(Clone, Copy)]
pub enum DurableContext {
    Request {
        create_guid: Guid,
        timeout_ms: u32,
    },
    Reconnect {
        file_id: FileId,
        create_guid: Guid,
        timeout_ms: u32,
        persistent: bool,
    },
}

#[derive(Clone, Copy)]
pub struct LifecycleRequest {
    pub lease_key: Guid,
    pub lease_state: LeaseState,
    pub lease_epoch: u16,
    pub durable: DurableContext,
}

impl LifecycleRequest {
    pub fn read_handle(create_guid: Guid, lease_key: Guid, timeout_ms: u32) -> Self {
        Self {
            lease_key,
            lease_state: LeaseState::new()
                .with_read_caching(true)
                .with_handle_caching(true),
            lease_epoch: 0,
            durable: DurableContext::Request {
                create_guid,
                timeout_ms,
            },
        }
    }
}

pub struct LeaseInfo {
    tree_id: u32,
    key: Guid,
    state: AtomicU32,
    epoch: AtomicU16,
    granted: AtomicBool,
    breaks: AtomicU64,
    acknowledgements: AtomicU64,
}

impl LeaseInfo {
    pub(crate) fn requested(request: LifecycleRequest, tree_id: u32) -> Self {
        Self {
            tree_id,
            key: request.lease_key,
            state: AtomicU32::new(u32::from_le_bytes(request.lease_state.into_bytes())),
            epoch: AtomicU16::new(request.lease_epoch),
            granted: AtomicBool::new(false),
            breaks: AtomicU64::new(0),
            acknowledgements: AtomicU64::new(0),
        }
    }
    pub fn key(&self) -> Guid {
        self.key
    }
    pub(crate) fn tree_id(&self) -> u32 {
        self.tree_id
    }
    pub fn state(&self) -> LeaseState {
        LeaseState::from_bytes(self.state.load(Ordering::SeqCst).to_le_bytes())
    }
    pub fn epoch(&self) -> u16 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn granted(&self) -> bool {
        self.granted.load(Ordering::SeqCst)
    }
    pub fn breaks(&self) -> u64 {
        self.breaks.load(Ordering::SeqCst)
    }
    pub fn acknowledgements(&self) -> u64 {
        self.acknowledgements.load(Ordering::SeqCst)
    }
    pub(crate) fn grant(&self, state: LeaseState, epoch: u16) {
        let previous = self.epoch();
        if self.breaks() == 0 || (epoch != previous && epoch.wrapping_sub(previous) < 32768) {
            self.state
                .store(u32::from_le_bytes(state.into_bytes()), Ordering::SeqCst);
            self.epoch.store(epoch, Ordering::SeqCst);
        }
        self.granted.store(true, Ordering::SeqCst);
    }
    pub(crate) fn break_to(&self, state: LeaseState, epoch: u16) {
        self.state
            .store(u32::from_le_bytes(state.into_bytes()), Ordering::SeqCst);
        self.epoch.store(epoch, Ordering::SeqCst);
        self.breaks.fetch_add(1, Ordering::SeqCst);
    }
    pub(crate) fn acknowledged(&self) {
        self.acknowledgements.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct DurableOpenInfo {
    pub file_id: FileId,
    pub create_guid: Guid,
    pub timeout_ms: u32,
    pub persistent: bool,
    pub server_guid: Guid,
    pub client_guid: Guid,
    pub lease: Option<Arc<LeaseInfo>>,
}

impl DurableOpenInfo {
    pub fn reconnect_request(&self) -> Option<LifecycleRequest> {
        let lease = self.lease.as_ref()?;
        Some(LifecycleRequest {
            lease_key: lease.key(),
            lease_state: lease.state(),
            lease_epoch: lease.epoch(),
            durable: DurableContext::Reconnect {
                file_id: self.file_id,
                create_guid: self.create_guid,
                timeout_ms: self.timeout_ms,
                persistent: self.persistent,
            },
        })
    }
}

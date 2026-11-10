# Local SMB sharing and lifecycle patch

Upstream: smb 0.12.1, https://github.com/afiffon/smb-rs, commit
7c7d4a60c930e3780e95208967aa13b6346eaa25. The source was copied from the
published crate. LICENSE.md comes from the same upstream commit.

FileCreateArgs gains an optional share_access field. None preserves upstream's
default read/write/delete sharing for disk shares. Explicit sharing modes allow
the server to deny concurrent data writers and delete/rename handles while
rdrive verifies content and performs a protected replacement.

Optional lifecycle requests add read/handle leases and durable-v2 CREATE and
reconnect contexts. Actual server grants are retained on open handles; write
caching leases are rejected. Lease breaks route by LeaseKey, including normal
connection-wide notifications with SessionId/TreeId zero. Acknowledgements use
the authenticated open's real tree and session. The async unsolicited receiver
now awaits its receive branch rather than an unreachable select `else` branch.

Directory.watch_controlled exposes cancellation IDs and a bounded notification
buffer. ResourceHandle.abandon permits discarding a dead transport handle
without closing a newly reconnected server open. Connection.is_closed and
terminal transport state are exposed; closed worker queues retain the typed
ConnectionStopped error. File I/O preserves typed source errors so the caller
can distinguish disconnection from permission or integrity failures.

No encryption, signing, credential verification, authentication algorithm, DFS,
or write replay implementation is replaced. Automatic reconnect policy,
revalidation, metering and persistent upload checkpoints live in rdrive's
src/smb.rs. See docs/SMB_LIFECYCLE.md for actual supported workflows and limits.

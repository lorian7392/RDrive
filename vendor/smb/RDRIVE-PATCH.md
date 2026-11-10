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

Asynchronous response handling returns credits from each validated interim
response immediately, as required by
[MS-SMB2 section 3.3.4.1.2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/46256e72-b361-4d73-ac7d-d47c04b32e4b).
The request's credit charge is accounted once across interim and final replies;
the received packet headers remain unchanged. Parallel workers retain a FIFO
queue for responses sharing a message ID, so a final reply cannot overwrite an
unconsumed interim reply. Connection-level tests use the real parallel worker
and a scripted packet transport to exercise both defects, malformed replies,
collision reconciliation, and the legacy single-credit path. The Linux CI job
runs all SDK library unit tests through the root workspace and lockfile.

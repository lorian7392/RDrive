# SMB notifications, leases and recovery

The backend uses the published pure Rust `smb` and `smb-msg` 0.12.1 sources,
vendored with the patches recorded in each crate's `RDRIVE-PATCH.md`. Both
retain their upstream MIT license and exact source commit. Packet definitions
alone do not imply that a workflow is supported; the following describes the
backend's implemented behavior.

## Change notifications

`Backend::change_feed` issues an actual SMB CHANGE_NOTIFY request on the opened
directory. `ready` means the server returned STATUS_PENDING and assigned an
asynchronous request ID. Notifications carry contained root-relative paths and
`added`, `removed`, `modified`, `rename_old`, or `rename_new`. Rename events
are hints, not a paired transaction or a complete change journal.

Real server-buffer overflow, invalid paths, handle cleanup and disconnection
require a rescan. A disconnected feed sends `rescan_required`, then its error,
and ends. The caller must rescan and resubscribe; a new subscription establishes
a fresh authenticated session if the previous connection stopped. Changes
between subscriptions or while the consumer rescans can be coalesced or lost.
The watcher must therefore treat the feed as a trigger for reconciliation.

Cancellation sends SMB CANCEL for a pending request and closes the directory.
An event queue that is full cannot prevent cancellation. Last-access events
are excluded to prevent verification reads from causing an endless rescan.

## Read leases and durable-v2 handles

SMB 3 read downloads request an RH lease (read and handle caching) and a
durable-v2 open, with a requested 15-second disconnect timeout. The server may
decline either request or choose a different timeout. The client retains the
actual granted state and timeout. SMB 2 downloads use protected ordinary opens
and can recover by verified reopen; durable-v2 contexts are not sent to SMB 2.

The implementation holds no dirty write cache or retained application-closed
handles. Lease breaks update granted rights and epoch; when required, the
client sends a real acknowledgement using that open's authenticated session
and tree. The lease can be downgraded without flushing an invented cache.
These leases are not byte-range locks and do not make ordinary path operations
transactional.

After a recoverable read transport/session error, the backend authenticates a
new session with the same client GUID, username, credentials and encryption
policy. It tries DH2C using the original file ID, create GUID, lease key/state
and epoch. It never weakens required encryption or falls back to guest.
If the server no longer has that durable open, a new protected read open is
allowed only after full disk-identity and SHA256 verification against the
recorded revision. Verification occurs before any additional local bytes are
written. A different file with identical contents is rejected by file identity.
The normal deny-write/delete sharing mode remains in force while reading.

At most three read recoveries occur per download. Each authentication sequence
has at most five transport retry attempts with bounded backoff; authentication,
permission, signature and content conflicts are not treated as successful
recovery. A failed transfer fsyncs and preserves its partial file.

This is ordinary durable-v2 recovery within a running process, not SMB
persistent handles, continuous availability, transparent failover, or a promise
that durable state survives a server restart. A restart can succeed by the
explicitly counted **verified reopen** fallback. A client process restart uses
the existing local partial-file/revision checkpoint and protected reopen; no
protocol durable token is persisted across process restarts. An abruptly lost
durable read open may temporarily keep the server's sharing restrictions until
the server releases it or its timeout expires.

## Upload recovery and ambiguous publication

Checkpointed uploads have their own application recovery protocol. After a
recoverable connection failure, the backend reauthenticates and re-enters the
persisted upload state machine, at most three times. Before append it verifies
the private stage's disk identity, actual server length and every existing
prefix byte against the immutable local snapshot. Saved acknowledgement offsets
alone are never trusted. Upload stages do not request write-cache leases or
protocol durable handles.

CREATE-new, rename, delete and replacement are never blindly replayed after an
ambiguous response. Persisted publication phases, original/replacement hashes,
stage identity and immutable recovery receipts distinguish a completed rename
from a competing file. Conflicts preserve the stage, original backup, receipt
and local checkpoint. A stage created without a recorded identity is not
silently adopted. Ordinary non-checkpointed uploads/copies can still fail with
an orphan stage; their source remains available to retry explicitly.

## Transactions and durability boundary

Protected ordinary-file replacement still moves the original to a recovery
backup and then publishes a completed stage with no-replace rename. There is a
namespace gap. Sharing modes and recovery receipts do not turn two renames
into one cross-crash transaction. An unresolved receipt blocks sync from
interpreting that gap as a user deletion; checking receipts during a sync plan
is read-only.

The separate managed repository uses immutable data, full manifests and
append-only no-replace commit slots to expose old or new logical snapshots to
cooperating readers. That is a different storage interface from arbitrary SMB
paths. It does not establish server power-loss durability. File FLUSH responses
do not prove that all parent directory updates, server caches, controller
caches and backing storage survive power loss; server policy such as Samba's
`strict sync` also matters.

## Metadata, progress and diagnostics

`stat_metadata` and `list_metadata` use CREATE/QUERY_INFO/QUERY_DIRECTORY only;
they never issue file READ requests. Their `W/smb-meta` token includes volume
and file ID, size, full-resolution last-write and change times. It is explicitly
weak and cannot authorize a guarded overwrite or checked download. Ordinary
`stat` and `list` still read full content to produce strong SHA256+identity
revisions.

All file payload I/O is charged to the caller's shared limiter, including
strong revision scans, recovery/prefix verification, copies and recovery JSON.
Read/write block size follows the limiter's burst budget, capped at 64 KiB.
Counters exclude SMB headers, encryption overhead, query-info/notification
PDUs and TCP retransmission overhead. Verification bytes are real additional
traffic and are reported as such, not as extra logical file completion.

Diagnostics distinguish authenticated reconnects, durable grants, DH2C
reconnections, verified reopens and failed reconnect authentication. Live lease
counters describe currently retained lease objects. They do not claim server
support merely because a request flag was sent.

## Validation

The isolated fixture is Samba 4.21.9 in
`alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f`,
with required SMB encryption and synthetic credentials. Lifecycle tests enable
Samba's documented durable-handle prerequisites (`durable handles = yes`,
`kernel oplocks = no`, `kernel share modes = no`, `posix locking = no`).
No host file-sharing service or real NAS is used.

Run the ignored `smb::tests::lifecycle_smb_integration` test through
`tests/smb_workflows.py` with `RDRIVE_SMB_LIFECYCLE=1`,
`RDRIVE_SMB_PROOF_ONLY=1` and `RDRIVE_SMB_TEST_RUNNER` pointing to the current
library test executable. The fixture requires that exactly one selected test
passes, and removes its container after success or failure.

The lifecycle proof passed on this fixture with actual encrypted server traffic:

- Registration, create, rename, remove and CANCEL, real 16-byte notification
  buffer overflow, disconnection/rescan and authenticated resubscription.
- Server RH lease and durable-v2 grants, another client's actual write causing
  a lease break and acknowledgement, TCP loss followed by DH2C with stable ID.
- Metadata-only stat/list transferred zero file payload; strong stat transferred
  and metered the full content.
- A download interrupted after a nonzero fsynced prefix automatically continued
  via DH2C. After an actual container/server restart it continued via verified
  reopen. Wrong reconnect credentials and modified server content preserved the
  existing prefix without appending any unverified bytes.
- A checkpointed upload interrupted after a nonzero flushed stage continued
  after TCP loss and after server restart. Wrong reconnect credentials and a
  tampered existing prefix left the final target absent and checkpoint intact.
- A separate unit regression confirms cancellation while the event queue is
  full. Server power loss, persistent CA handles and Windows/NAS interoperability
  beyond this Samba fixture were not tested.

The final 0.3.0 release executable also passed ten CLI workflow groups,
including a real SMB managed repository: multiple files and a directory remain
invisible until their commit, SHA256 and user xattrs round-trip, replaying the
same transaction creates no second commit slot, and two independent client
state directories compete with exactly one winning the next immutable slot.
The existing protected-replacement and crash/resume proofs passed against the
current library test executable. `SMB_VALIDATION.json` records the tested binary
checksum, exact image digest, groups and observed reconnect diagnostics.

## Primary protocol references

- [Microsoft: receiving lease breaks and selecting authenticated ACK session/tree](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/4f35576a-6f3b-40f0-a832-1c30b0afccb3)
- [Microsoft: re-establishing a durable open](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/3309c3d1-3daf-4448-9faa-81d2d6aa3315)
- [Microsoft: server handling of connection loss](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/eb5bfe99-47fe-4e87-8e87-08a084dcefb6)
- [Microsoft: FLUSH access checks and object-store behavior](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/026984f6-38af-4408-8200-50557eb0a286)
- [Samba: server configuration, durable handles and strict sync](https://www.samba.org/samba/docs/current/man-html/smb.conf.5.html)
- [Upstream SMB Rust source](https://github.com/afiffon/smb-rs/tree/7c7d4a60c930e3780e95208967aa13b6346eaa25)

# Local SMB message API patch

Upstream smb-msg 0.12.1, commit 7c7d4a60c930e3780e95208967aa13b6346eaa25,
https://github.com/afiffon/smb-rs. The source comes from the published crate;
the root MIT license is copied from that same commit.

The patch adds missing Batch/Lease oplock values and public constructors and
accessors for lease requests, lease-break acknowledgements, and durable-v2
reconnect requests. It enables lifecycle workflows in the vendored SMB client;
message types alone do not provide automatic reconnection or cache safety.

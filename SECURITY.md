# Security policy

RDrive 0.8.0 operates on remote files and retains local configuration, cached content, upload snapshots, and recovery records. Limit its access according to the storage account's permissions and the sensitivity of the data. The project does not declare long-term-support versions, fixed remediation deadlines, or a security-response SLA.

## Report a vulnerability

Do not put passwords, authorization headers, session or lock tokens, private URL parameters, complete state directories, user content, or sensitive exploit details into public issues.

If private vulnerability reporting is enabled for the repository, use [GitHub Security Advisories](https://github.com/lorian7392/RDrive/security/advisories/new). Availability depends on repository settings. This policy does not claim that a dedicated security mailbox or another private channel has been configured. If the advisory form is unavailable, ask how to contact a maintainer without publishing exploit material or sensitive data.

Include the affected version/commit, a minimal synthetic reproduction, the expected boundary, the actual impact, and redacted errors. Test only in environments you are authorized to use.

## Credentials and transport

- Passwords come from the process environment or macOS Keychain. The CLI has no plaintext password argument; profiles store an environment-variable name.
- WebDAV requires HTTPS and verifies certificates by default. `--allow-http` is an explicit insecure compatibility option.
- SMB requires transport encryption by default. Use `--allow-unencrypted-smb` only when you deliberately accept that risk.
- Do not embed accounts, passwords, signed parameters, or other credentials in URLs, shell history, or version control.
- Environment variables are not encrypted credential storage. Running processes need access to authentication material. Background services do not automatically persist terminal passwords securely.
- Native mounts use the macOS client's authentication and protocol behavior; Rust-client settings do not all apply to them.

## Local content and recovery copies

Caches, independent upload snapshots, and offline edits are stored in plaintext. Private directory permissions help isolate other accounts; they do not provide encryption at rest or protection against malicious processes running as the same user. Cryptomator vaults are not implemented.

Keep state, cache, IPC, and staging directories out of public or broadly shared locations. Do not weaken private permissions to bypass an error. Preserve unconfirmed content and recovery records. Reclaiming a clean cache is not secure erasure.

Remote versions and SMB/Smart recovery copies usually reside on the same server as the source data. Other clients with account access, or a server failure, can damage them. They are not off-site, immutable, or complete-system backups. Ordinary deletion, recoverable Smart deletion, job cancellation, and snapshot cleanup have different semantics; review the relevant guide first.

## Protocol and integration boundaries

Revision conditions, protected handles, and transaction publication depend on correct server implementations. Multiple ordinary-directory requests do not form a cross-file transaction. Managed snapshot visibility requires cooperating clients and correct server no-overwrite publication semantics; it does not promise persistence through server power loss.

RDrive rejects dangerous paths, recognized symbolic links, and unsupported metadata. Some servers transparently resolve remote links, so universal remote no-follow behavior cannot be guaranteed. Selected tag/xattr copying does not include ACLs, resource forks, quarantine metadata, or every system security attribute.

MCP is read-only by default. Writes require explicit enablement and local-directory scopes. Scope checks constrain tool arguments and do not replace OS process isolation. Remote filenames, text, and metadata are data, not instructions or authorization.

Ad-hoc native builds are development artifacts. They are not equivalent to a Developer ID signature, notarization, or production App Group entitlement. Validate the bridge and system extensions separately, and use an App that matches the intended source version.

See [configuration](docs/reference/CONFIGURATION.md), [feature boundaries](docs/FEATURES.md), [recovery](docs/TROUBLESHOOTING.md), and [testing](docs/TESTING.md).

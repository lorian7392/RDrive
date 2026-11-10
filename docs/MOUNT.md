# Native macOS mounts

`rdrive mount` uses the SMB and WebDAV clients included with macOS. Finder and
ordinary applications can access the mounted directory. No FUSE driver, system
sharing service, launch agent, or administrator configuration is installed.

```sh
mkdir -m 700 "$HOME/NetworkStorage"
rdrive mount nas "$HOME/NetworkStorage" --dry-run
rdrive mount nas "$HOME/NetworkStorage"
rdrive mounts
rdrive unmount "$HOME/NetworkStorage"
```

Use a separate empty directory for each volume. The mountpoint must already
exist, belong to the current user, have owner-only `0700` permissions, and have
real directory components. Paths through symbolic links, other users' writable
directories, existing data, another network volume, or a nested mount are
rejected. Use the real path if the system temporary directory is a symlink.
SMB mountpoints cannot contain `%`, because Apple's helper decodes percent
escapes in that argument. Directories are not created or deleted automatically.

`--read-only` requests an actual readonly native volume, and the resulting kernel
mount flags are checked. Mounts are hidden from the desktop by default; use
`--browse` to expose them there. `--dry-run` checks the directory and shows the
credential-free native command without authenticating or mounting.

## Credentials

- **SMB:** Apple's native terminal password prompt or existing macOS credentials
  are used. Profile `username` is encoded into the SMB URL, but passwords are
  never included. The profile's rdrive `password-env` and `rdrive:<profile>`
  Keychain entry do not supply SMB mount passwords. `--no-prompt` passes the
  native `-N` option and fails if native authentication cannot proceed. The
  implementation never writes `nsmb.conf` or adds Keychain items.
- **WebDAV:** When an rdrive password is available, the native helper receives a
  binary credential record through an anonymous seekable descriptor inherited
  as standard input. The temporary file is created with mode `0600` and unlinked
  **before any secret bytes are written**. The descriptor is overwritten and
  truncated after the mount attempt. Neither a pathname nor a password appears
  in the process arguments, and the configured credential environment variable
  is removed from the helper's environment. With no rdrive password, the native
  terminal prompt is used; `--no-prompt` instead lets macOS try its existing
  Internet-password credentials with all native dialogs suppressed.

WebDAV HTTPS mounts also request securely transmitted authentication. HTTP must
be explicitly allowed in the profile and is intended for trusted local tests.
Apple's native certificate, proxy, and authentication support can differ from
rdrive's own WebDAV transfer client. Interactive native prompts require a
controlling terminal. These commands do not create persistent credentials.

The WebDAV descriptor option `-a fd` is deprecated in Apple's manual, but is
present in the installed helper and Apple's published source. Its parser calls
`lseek`, so a pipe is unsuitable. The record contains five big-endian length
fields: username, password, proxy username, proxy password, certificate data.
Username and password are limited to 255 UTF-8 bytes by the native parser.

## Root, encryption, and verification

SMB mounts retain profile subdirectories, for example
`smb://server/share/team/documents`. Apple's client supports submounts by default,
but existing SMB preferences can turn them off. Before reporting success,
rdrive compares the actual mounted source's full decoded root with the profile.
It unmounts and fails if macOS substituted the broader share. SMB profiles require
transport encryption by default: the native command uses `sessionencrypt`,
`shareencrypt`, and `forcenewsession`. The explicit profile option allowing
unencrypted SMB also applies to mounting.

WebDAV mounts retain the configured collection URL. Apple's native client makes
DAV Class 1 servers readonly even when `--read-only` was not requested, because
that client requires DAV LOCK for write access. A successful readonly mount does
not prove that the server permits a writable native mount.

`rdrive mounts` reads the actual macOS kernel mount table, including SMB/WebDAV
volumes mounted by other applications. It reports source, mountpoint,
filesystem, owner, filesystem ID, readonly, and desktop visibility. Passwords in
pre-existing mount source strings are removed before display. The implementation
uses `getfsstat(MNT_NOWAIT)` with its own buffer so unrelated disconnected network
volumes are not synchronously probed.

After mounting, the kernel must report a volume at the exact requested path,
owned by the current user, with the expected filesystem and root. A zero exit
code alone is insufficient. Unexpected broader roots or ignored readonly flags
trigger an ordinary unmount; failed cleanup is reported with the exact path.
An unexpected filesystem or owner is left for inspection rather than detached.

Unmount accepts an exact user-owned SMB/WebDAV volume root, refuses nested
volumes, and verifies that the volume disappeared. Close applications using the
volume if an ordinary unmount fails. `--force` is an explicit option and can
interrupt active operations. rdrive does not remove the underlying directory.

Both native helpers accept paths, rather than a mountpoint directory descriptor.
rdrive holds and checks the underlying inode, serializes its own attempts with a
directory lock, and rejects untrusted path components. SMB also gets macOS
`nofollow`. The installed WebDAV helper rejects that option, so it cannot be
passed there. Another process running as the same user can still race a path
replacement or an external mount operation; these commands do not isolate
against hostile processes with that user's permissions.

## What mounting provides

This is an online network volume. macOS controls native caching, reconnects,
server locks, and metadata behavior. It does not implement Mountain Duck's cloud
placeholders, Keep Offline, background synchronization, or an offline upload
queue. `rdrive sync` and `rdrive cache` are separate commands and do not manage
the native volume's cache. Profile transfer timeout values do not change native
macOS mount timeouts or dialogs.

## Verification

The normal test suite covers command construction, source scope, credential
descriptor contents and cleanup, private directories, symlink and data
protection, mock helper outcomes versus mount-table evidence, root/readonly
failure cleanup, and verified unmounting.

An additional explicit test starts an authenticated loopback DAV fixture,
mounts it readonly with descriptor credentials, reads its synthetic file,
checks write rejection, then unmounts and verifies disappearance:

```sh
cargo test --test mount native_webdav_readonly_roundtrip -- --ignored --nocapture
```

This actual native roundtrip passed on the development Mac. The restricted
execution environment denied the fixture's loopback bind; the same test passed
with host execution allowed. The test never uses a real account or changes
system sharing.

The native SMB workflow uses a temporary Samba container with required SMB3
encryption. It supplies only synthetic credentials over a pseudoterminal,
verifies the actual AES cipher using `smbutil`, tests a readonly subdirectory
mount without exposing the broader share, and verifies unmounting. A second
writable mount checks a server-confirmed file write, rename, and deletion.
Both groups passed on the development Mac:

```sh
python3 tests/mount_workflows.py target/debug/rdrive
```

Docker is needed only for this isolated SMB integration fixture, not for
mounting or using rdrive. Its temporary container and mounted volume are removed
after the test.

## Primary references

- Installed `man mount`, `man mount_smbfs`, `man mount_webdav`, and `man umount`.
- [Apple native SMB helper options](https://github.com/apple-oss-distributions/SMBClient/blob/main/mount_smbfs/mount_smbfs.c).
- [Apple SMB submount behavior](https://github.com/apple-oss-distributions/SMBClient/blob/main/lib/smb/ctx.c).
- [Apple WebDAV credential parser and native mount](https://github.com/apple-oss-distributions/webdavfs/blob/main/mount.tproj/webdav_agent.c).
- [Apple WebDAV helper wrapper](https://github.com/apple-oss-distributions/webdavfs/blob/main/mount.tproj/mount_webdav.c).

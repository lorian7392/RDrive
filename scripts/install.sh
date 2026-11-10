#!/bin/bash
# Install the standalone RDrive CLI from an authenticated GitHub release.
set -euo pipefail
umask 077

repo=github.com/lorian7392/RDrive
asset=rdrive-macos-arm64
tag_pattern='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'

usage() {
    cat <<'EOF'
Usage: install.sh [--prefix ABSOLUTE_PATH] [--version vX.Y.Z]

Install the Apple Silicon CLI in a user-owned PREFIX/bin. The default prefix is
$HOME/.local and the default version is the latest stable release.
GitHub CLI (gh) must have access to the private lorian7392/RDrive repository.

  --replace-version vX.Y.Z  Replace only if the installed version still matches.
  -h, --help                 Show this help.
EOF
}

fail() {
    printf 'rdrive install: %s\n' "$*" >&2
    exit 1
}

prefix=''
version=''
replace_version=''
while (( $# )); do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --prefix|--version|--replace-version)
            (( $# >= 2 )) && [[ -n "$2" && "$2" != -* ]] || fail "missing value for $1"
            case "$1" in
                --prefix) prefix=$2 ;;
                --version) version=$2 ;;
                --replace-version) replace_version=$2 ;;
            esac
            shift 2
            ;;
        *) fail "unknown option: $1 (see --help)" ;;
    esac
done

if [[ -z "$prefix" ]]; then
    [[ "${HOME:-}" == /* ]] || fail 'HOME must be an absolute path or supply --prefix'
    prefix=${HOME%/}/.local
fi
while [[ "$prefix" != / && "$prefix" == */ ]]; do
    prefix=${prefix%/}
done
[[ "$prefix" == /* && "$prefix" != / && "$prefix" != *[[:cntrl:]]* ]] \
    || fail '--prefix must be an absolute path other than /'
case "$prefix/" in
    */./*|*/../*) fail '--prefix must not contain . or .. path components' ;;
esac
[[ -z "$version" || "$version" =~ $tag_pattern ]] || fail '--version must be a stable tag vX.Y.Z'
[[ -z "$replace_version" || "$replace_version" =~ $tag_pattern ]] \
    || fail '--replace-version must be a stable tag vX.Y.Z'

[[ "$(uname -s)" == Darwin ]] || fail 'this release supports macOS on Apple Silicon'
if [[ "$(uname -m)" != arm64 && "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" != 1 ]]; then
    fail 'this release supports macOS on Apple Silicon'
fi
command -v gh >/dev/null 2>&1 || fail 'install GitHub CLI (gh) and authenticate with gh auth login first'

# Resolve existing ancestors even if the requested prefix does not exist yet.
# The second location check catches aliases into app bundles or source trees.
resolve_prefix() {
    local base=$1 suffix=''
    while [[ ! -e "$base" && ! -L "$base" ]]; do
        suffix=/${base##*/}$suffix
        base=${base%/*}
        base=${base:-/}
    done
    (
        unset CDPATH
        cd -P -- "$base" && printf '%s%s\n' "${PWD%/}" "$suffix"
    )
}

check_location() {
    local directory=$1
    case "$directory/" in
        *.[aA][pP][pP]/*) fail 'choose a standalone prefix outside app bundles' ;;
        /[bB][iI][nN]/*|/[sS][bB][iI][nN]/*|/[sS][yY][sS][tT][eE][mM]/*|/usr/bin/*|/usr/sbin/*)
            fail 'choose a user-writable prefix outside macOS system directories' ;;
    esac
    while [[ "$directory" != / ]]; do
        if [[ -f "$directory/Cargo.toml" && -f "$directory/src/main.rs" && -f "$directory/scripts/install.sh" ]]; then
            fail 'choose a prefix outside the RDrive source checkout'
        fi
        directory=${directory%/*}
        directory=${directory:-/}
    done
}

check_target() {
    [[ ! -L "$destination" ]] || fail 'the destination rdrive is a symbolic link'
    if [[ -e "$destination" && ! -f "$destination" ]]; then
        fail 'the destination rdrive must be a standalone regular file'
    fi
}

check_location "$prefix/bin"
prefix=$(resolve_prefix "$prefix") || fail 'cannot resolve the install prefix'
[[ "$prefix" == /* && "$prefix" != / && "$prefix" != *[[:cntrl:]]* ]] || fail 'unsafe install prefix'
bin=$prefix/bin
destination=$bin/rdrive
check_location "$bin"
[[ ! -L "$bin" ]] || fail 'the install bin directory is a symbolic link'
check_target

release_args=(release view)
if [[ -n "$version" ]]; then
    release_args+=("$version")
fi
release_filter='select((.isDraft | not) and (.isPrerelease | not)) | .tagName'
tag=$(gh "${release_args[@]}" --repo "$repo" --json tagName,isDraft,isPrerelease --jq "$release_filter") \
    || fail 'cannot read the release with gh. Check gh auth status and repository access'
[[ "$tag" =~ $tag_pattern && ( -z "$version" || "$version" == "$tag" ) ]] \
    || fail 'no matching published stable release was found'

download_dir=''
staged=''
lock=''
cleanup() {
    [[ -z "$staged" ]] || rm -f -- "$staged" || true
    [[ -z "$lock" ]] || rmdir -- "$lock" || true
    [[ -z "$download_dir" ]] || rm -rf -- "$download_dir" || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

download_dir=$(mktemp -d "${TMPDIR:-/tmp}/rdrive-install.XXXXXXXX") || fail 'cannot create a private download directory'
gh release download "$tag" --repo "$repo" --pattern "$asset" --pattern SHA256SUMS --dir "$download_dir" \
    || fail "cannot download CLI release assets for $tag"
payload=$download_dir/$asset
manifest=$download_dir/SHA256SUMS
[[ -f "$payload" && ! -L "$payload" && -s "$payload" && -f "$manifest" && ! -L "$manifest" ]] \
    || fail 'release is missing a regular CLI asset or checksum file'

expected=''
checksum_row='^([[:xdigit:]]{64})[[:blank:]][[:blank:]*](.+)$'
while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -n "$line" ]] || continue
    [[ "$line" =~ $checksum_row ]] || fail 'SHA256SUMS contains an invalid checksum row'
    if [[ "${BASH_REMATCH[2]}" == "$asset" ]]; then
        [[ -z "$expected" ]] || fail "SHA256SUMS contains duplicate entries for $asset"
        expected=${BASH_REMATCH[1]}
    fi
done < "$manifest"
[[ -n "$expected" ]] || fail "SHA256SUMS has no exact entry for $asset"
expected=$(printf '%s' "$expected" | LC_ALL=C /usr/bin/tr 'A-F' 'a-f')
actual=$(cd -- "$download_dir" && /usr/bin/shasum -a 256 -- "$asset") || fail 'cannot compute the CLI checksum'
[[ "${actual%% *}" == "$expected" ]] || fail "SHA256 checksum mismatch for $asset"

if [[ -z "$replace_version" ]]; then
    mkdir -p -- "$bin" || fail 'cannot create the install bin directory'
fi
[[ -d "$bin" && ! -L "$bin" && -O "$bin" ]] || fail 'the install bin directory must be owned by the current user'
resolved_bin=$(resolve_prefix "$bin") || fail 'cannot resolve the install bin directory'
[[ "$resolved_bin" == "$bin" ]] || fail 'the install bin directory changed during installation'
check_location "$bin"

lock_path=$bin/.rdrive-install.lock
mkdir -- "$lock_path" 2>/dev/null || fail "cannot acquire the install lock at $lock_path"
lock=$lock_path
check_target
if [[ -n "$replace_version" ]]; then
    [[ -f "$destination" && -x "$destination" ]] || fail 'there is no installed executable rdrive to update'
    installed_version=$("$destination" --version) || fail 'cannot read the installed rdrive version'
    [[ "$installed_version" == "rdrive ${replace_version#v}" ]] \
        || fail "installed rdrive changed since the update check (expected $replace_version)"
fi

staged=$(mktemp "$bin/.rdrive-install.XXXXXXXX") || fail 'cannot stage the CLI in the install bin directory'
cat -- "$payload" > "$staged" || fail 'cannot write the staged CLI'
/bin/chmod 0755 "$staged" || fail 'cannot make the staged CLI executable'
check_target
# rename() replaces a file in this directory atomically and cannot move the
# staged file inside a directory that appeared at the destination.
/usr/bin/perl -e 'rename($ARGV[0], $ARGV[1]) or die "cannot replace rdrive: $!\n";' -- "$staged" "$destination" \
    || fail 'cannot replace the installed rdrive'
staged=''
printf 'Installed RDrive %s at %s\n' "$tag" "$destination"

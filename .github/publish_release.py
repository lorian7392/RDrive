#!/usr/bin/env python3
"""Verify the staged release files and publish a complete GitHub draft."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
BINARY = "rdrive-macos-arm64"
ASSETS = (BINARY, "SHA256SUMS", "install.sh")


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def verify_staged_files(directory, installer):
    if set(path.name for path in directory.iterdir()) != set(ASSETS):
        raise ValueError("Release artifact must contain exactly " + ", ".join(ASSETS))
    for name in ASSETS:
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"Release asset must be a nonempty regular file: {name}")

    hashes = {name: sha256(directory / name) for name in ASSETS}
    expected = f"{hashes[BINARY]}  {BINARY}\n".encode("ascii")
    if (directory / "SHA256SUMS").read_bytes() != expected:
        raise ValueError("SHA256SUMS differs from the canonical executable checksum")
    if hashes["install.sh"] != sha256(installer):
        raise ValueError("Release installer differs from scripts/install.sh in the tagged commit")
    return {name: f"sha256:{digest}" for name, digest in hashes.items()}


def gh(*arguments):
    return subprocess.check_output(["gh", *arguments], text=True, timeout=180)


def list_pages(endpoint):
    pages = json.loads(gh("api", "--paginate", "--slurp", endpoint + "?per_page=100"))
    return [entry for page in pages for entry in page]


def get_release(repo, tag):
    matches = [release for release in list_pages(f"repos/{repo}/releases") if release["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError(f"Found more than one GitHub release for {tag}")
    return next(iter(matches), None)


def verify_tag(repo, tag, commit):
    remote = json.loads(gh("api", f"repos/{repo}/git/ref/tags/{quote(tag, safe='')}"))
    target = remote["object"]
    seen = set()
    while target["type"] == "tag":
        sha = target["sha"]
        if sha in seen:
            raise ValueError(f"Tag {tag} has a cyclic target")
        seen.add(sha)
        target = json.loads(gh("api", f"repos/{repo}/git/tags/{sha}"))["object"]
    if target["type"] != "commit" or target["sha"] != commit:
        raise ValueError(f"Tag {tag} no longer points to the verified build commit")


def missing_assets(repo, release, hashes):
    seen = set()
    for asset in list_pages(f"repos/{repo}/releases/{release['id']}/assets"):
        name = asset["name"]
        if name in seen or name not in hashes:
            raise ValueError(f"Unexpected or duplicate asset on GitHub release: {name}")
        if asset.get("state") != "uploaded" or asset.get("digest") != hashes[name]:
            raise ValueError(f"Existing GitHub release asset differs from staged bytes: {name}")
        seen.add(name)
    return [name for name in ASSETS if name not in seen]


def publish(repo, tag, commit, directory, installer):
    hashes = verify_staged_files(directory, installer)
    verify_tag(repo, tag, commit)
    prerelease = "-" in tag.split("+", 1)[0]
    release = get_release(repo, tag)
    if release is None:
        flags = ["--prerelease"] if prerelease else []
        gh("release", "create", tag, "--repo", repo, "--verify-tag", "--draft",
           "--generate-notes", *flags)
        release = get_release(repo, tag)
        if release is None:
            raise ValueError(f"New draft for {tag} was not found")
    if release["prerelease"] != prerelease:
        raise ValueError(f"Existing release prerelease status differs from {tag}")

    missing = missing_assets(repo, release, hashes)
    if not release["draft"]:
        if missing:
            raise ValueError("Published release is missing required assets: " + ", ".join(missing))
        print(f"Release {tag} is already published with verified assets")
        return

    if missing:
        gh("release", "upload", tag, "--repo", repo, *(str(directory / name) for name in missing))
    missing = missing_assets(repo, release, hashes)
    if missing:
        raise ValueError("Draft release is missing required assets: " + ", ".join(missing))
    verify_tag(repo, tag, commit)
    result = json.loads(gh("api", "--method", "PATCH", f"repos/{repo}/releases/{release['id']}",
                           "-F", "draft=false"))
    if result["draft"] or result["tag_name"] != tag:
        raise ValueError(f"GitHub did not confirm publication of {tag}")
    print(f"Published release {tag} with verified assets")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--assets", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        if args.tag != "v" + version:
            raise ValueError(f"Release tag must match Cargo.toml version v{version}")
        publish(args.repo, args.tag, args.commit, args.assets.resolve(), ROOT / "scripts/install.sh")
    except (KeyError, OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Release publication failed: {error}\n")


if __name__ == "__main__":
    main()

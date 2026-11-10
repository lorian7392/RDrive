#!/usr/bin/env python3
"""Install the bundled RDrive skills without replacing existing skill folders."""

import argparse
import json
import os
from pathlib import Path
import shutil
import sys


SKILLS = ("rdrive-files", "rdrive-smart-sync", "rdrive-transactions")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target", type=Path,
        help="Skills directory (default: $CODEX_HOME/skills or ~/.codex/skills)",
    )
    parser.add_argument("--skill", action="append", choices=SKILLS,
                        help="Install a selected skill; may be repeated (default: all)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Print the installation plan without creating anything")
    args = parser.parse_args()
    target = args.target or Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex") / "skills"
    target = target.expanduser().absolute()
    source = Path(__file__).resolve().parent.parent / "skills"
    names = tuple(dict.fromkeys(args.skill or SKILLS))
    try:
        for name in names:
            folder = source / name
            if not (folder / "SKILL.md").is_file():
                raise ValueError(f"Bundled skill is missing: {name}")
            if any(path.is_symlink() for path in folder.rglob("*")):
                raise ValueError(f"Bundled skill contains a symbolic link: {name}")
        if target.is_symlink() or (target.exists() and not target.is_dir()):
            raise ValueError(f"Target must be a real directory: {target}")
        existing = [str(target / name) for name in names if os.path.lexists(target / name)]
        if existing:
            raise ValueError("Existing skills will not be overwritten: " + ", ".join(existing))
        plan = {"dry_run": args.dry_run, "target": str(target), "skills": list(names)}
        if args.dry_run:
            print(json.dumps(plan))
            return 0
        target.mkdir(parents=True, exist_ok=True)
        # mkdir is exclusive for each destination, including a concurrent install.
        # Never use copytree(..., dirs_exist_ok=True) on an existing user skill.
        created = []
        try:
            for name in names:
                destination = target / name
                destination.mkdir()
                created.append(destination)
                for entry in (source / name).iterdir():
                    if entry.is_dir():
                        shutil.copytree(entry, destination / entry.name)
                    else:
                        shutil.copy2(entry, destination / entry.name)
        except Exception:
            for destination in reversed(created):
                shutil.rmtree(destination)
            raise
        print(json.dumps({**plan, "installed": True}))
        return 0
    except (OSError, ValueError) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

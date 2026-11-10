#!/usr/bin/env python3
"""Print an MCP client configuration; do not modify client settings or store secrets."""
import argparse
import json
from pathlib import Path


def positive(value):
    result = int(value)
    if result < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path(__file__).resolve().parents[1] / "dist/rdrive")
    parser.add_argument("--home", required=True, type=Path,
                        help="Existing or new RDrive state directory (no credentials in this command)")
    parser.add_argument("--local-root", action="append", default=[], type=Path)
    parser.add_argument("--allow-write", action="store_true")
    parser.add_argument("--timeout", type=positive, default=300)
    parser.add_argument("--format", choices=("json", "toml"), default="json")
    options = parser.parse_args()
    binary = options.binary.expanduser().resolve()
    if not binary.is_file():
        parser.error("RDrive executable is missing; build it or pass --binary")
    home = options.home.expanduser().resolve()
    args = ["--home", str(home), "mcp", "serve", "--timeout", str(options.timeout)]
    if options.allow_write:
        args.append("--allow-write")
    for value in options.local_root:
        root = value.expanduser().resolve()
        if not root.is_dir():
            parser.error("each --local-root must be an existing directory")
        args.extend(("--local-root", str(root)))
    if options.format == "json":
        print(json.dumps({"mcpServers": {"rdrive": {"command": str(binary), "args": args}}},
                         ensure_ascii=False, indent=2))
    else:
        print("[mcp_servers.rdrive]")
        print("command = " + json.dumps(str(binary), ensure_ascii=False))
        print("args = " + json.dumps(args, ensure_ascii=False))


if __name__ == "__main__":
    main()

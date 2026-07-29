#!/usr/bin/env python3
"""Print one workspace package's version, from `cargo metadata` on stdin.

The release workflow compares this against the tag, so a tag that disagrees with
`Cargo.toml` fails before any artifact exists — a half-published release is worse
than none.

    cargo metadata --format-version 1 --no-deps \
      | python3 .github/scripts/package_version.py monitrs
"""

from __future__ import annotations

import json
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <package-name>", file=sys.stderr)
        return 2
    wanted = sys.argv[1]
    metadata = json.load(sys.stdin)
    for package in metadata.get("packages", []):
        if package["name"] == wanted:
            print(package["version"])
            return 0
    print(f"no package named {wanted!r} in the workspace", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())

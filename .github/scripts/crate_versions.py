#!/usr/bin/env python3
"""Print the resolved versions of one crate, from `cargo metadata` on stdin.

Used by CI to assert that exactly one `crossterm` major version resolves (§13):
ratatui 0.30 selects crossterm through a feature, and a mismatch produces
confusing type errors rather than a clear failure.

Lives in a file rather than inline in the workflow because embedded multi-line
Python inside a YAML `run: |` block is a trap — its lines have to stay indented
inside the block scalar, and getting that wrong makes the whole workflow
unparseable, which GitHub reports only as "a workflow file issue".

    cargo metadata --format-version 1 --all-features \
      | python3 .github/scripts/crate_versions.py crossterm
"""

from __future__ import annotations

import json
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <crate-name>", file=sys.stderr)
        return 2
    wanted = sys.argv[1]
    metadata = json.load(sys.stdin)
    versions = sorted(
        {p["version"] for p in metadata.get("packages", []) if p["name"] == wanted}
    )
    print(" ".join(versions))
    return 0


if __name__ == "__main__":
    sys.exit(main())

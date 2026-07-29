#!/usr/bin/env python3
"""Print the CHANGELOG.md section for one version.

§19.2 requires each release archive to carry a changelog *excerpt* rather than the
whole file. The section runs from its own heading to the next one.

Exits non-zero when the version has no section, so a release cannot be published
with empty notes.

    python3 .github/scripts/changelog_excerpt.py 0.1.0 [CHANGELOG.md]
"""

from __future__ import annotations

import pathlib
import re
import sys


def excerpt(text: str, version: str) -> str | None:
    pattern = re.compile(
        rf"^## \[?{re.escape(version)}\]?.*?(?=^## |\Z)",
        re.MULTILINE | re.DOTALL,
    )
    match = pattern.search(text)
    return match.group(0).strip() if match else None


def main() -> int:
    if not 2 <= len(sys.argv) <= 3:
        print(f"usage: {sys.argv[0]} <version> [changelog]", file=sys.stderr)
        return 2
    version = sys.argv[1]
    path = pathlib.Path(sys.argv[2] if len(sys.argv) == 3 else "CHANGELOG.md")

    section = excerpt(path.read_text(encoding="utf-8"), version)
    if section is None:
        print(f"{path} has no section for {version}", file=sys.stderr)
        return 1
    print(section)
    return 0


if __name__ == "__main__":
    sys.exit(main())

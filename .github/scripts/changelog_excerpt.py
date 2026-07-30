#!/usr/bin/env python3
"""Print the CHANGELOG.md section for one version.

§19.2 requires each release archive to carry a changelog *excerpt* rather than the
whole file. The section runs from its own heading to the next one.

Exits non-zero when the version has no section, so a release cannot be published
with empty notes.

    python3 .github/scripts/changelog_excerpt.py 0.1.0 [CHANGELOG.md] [--check-date]

`--check-date` compares the section's own date against today's and writes a GitHub
Actions warning if they differ. A *warning*, not a failure: writing the notes one day
and tagging the next is ordinary practice, and a hard check there would only teach
people to backdate the heading. What it prevents is the quiet case — a release dated
whenever the notes happened to be written, which nobody notices until someone reads the
changelog months later and the dates disagree with the tags.
"""

from __future__ import annotations

import datetime
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


def dated(section: str) -> str | None:
    """The `YYYY-MM-DD` in the section's heading, if it has one."""
    match = re.search(r"^## .*?(\d{4}-\d{2}-\d{2})", section)
    return match.group(1) if match else None


def warn_about_the_date(section: str, version: str) -> None:
    """Writes a GitHub Actions warning when the heading's date is not today's."""
    today = datetime.datetime.now(datetime.UTC).date().isoformat()
    heading_date = dated(section)
    if heading_date is None:
        print(
            f"::warning::the {version} changelog heading carries no date; "
            "Keep a Changelog dates a section by its release date",
            file=sys.stderr,
        )
        return
    if heading_date != today:
        print(
            f"::warning::the {version} changelog heading says {heading_date} but "
            f"today is {today}. Keep a Changelog dates a section by its release "
            "date, so this release will carry a date it was not made on.",
            file=sys.stderr,
        )


def main() -> int:
    arguments = sys.argv[1:]
    check_date = "--check-date" in arguments
    positional = [argument for argument in arguments if argument != "--check-date"]
    if not 1 <= len(positional) <= 2:
        print(
            f"usage: {sys.argv[0]} <version> [changelog] [--check-date]",
            file=sys.stderr,
        )
        return 2
    version = positional[0]
    path = pathlib.Path(positional[1] if len(positional) == 2 else "CHANGELOG.md")

    section = excerpt(path.read_text(encoding="utf-8"), version)
    if section is None:
        print(f"{path} has no section for {version}", file=sys.stderr)
        return 1
    if check_date:
        warn_about_the_date(section, version)
    print(section)
    return 0


if __name__ == "__main__":
    sys.exit(main())

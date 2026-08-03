#!/usr/bin/env python3
"""Decide whether a release needs the twelve-hour soak, from the files it changed.

§16.1's soak protects three properties — no unbounded memory growth, no descriptor
leak, no stall under sustained load — and those live in a knowable set of places: the
data model and history ring, the collectors, the sampler/channel/reducer path in the
binary, and the dependencies on that path. A change that touches none of them cannot
regress them, and a rule that demands fourteen hours and ten dollars for a README typo
gets quietly ignored, which is worse than a narrower rule that gets followed.

This does not decide anything on its own: it prints a verdict, because CI has no way
to know whether a human actually ran the soak. It exists so the question is answered
mechanically once per release instead of argued each time.

Usage:
    soak_required.py <base-ref> <head-ref>     # asks git for the changed files
    soak_required.py --files-from -            # reads a file list on stdin (for tests)

Exit status is 0 either way unless the arguments are wrong; the verdict is the output.
"""

from __future__ import annotations

import subprocess
import sys

# Deliberately coarse. A narrower list (only the history ring, only the rate engine)
# would be more precise and would need re-auditing every time a module moved; these are
# whole subtrees whose boundaries Cargo already enforces.
TRIGGERS: list[tuple[str, str]] = [
    ("crates/monitrs-core/src/", "the data model, history ring and rate engine"),
    ("crates/monitrs-collectors/", "the collectors — every per-tick allocation and every descriptor"),
    ("crates/monitrs-tui/src/app/", "the reducer, which owns every state transition"),
    ("crates/monitrs/src/runtime.rs", "the sampler, the bounded channel and coalescing"),
    ("crates/monitrs/src/interactive.rs", "the worker lifecycle and the frame loop"),
    ("crates/monitrs/tests/soak.rs", "the harness itself — on 2026-08-01 it was the leak"),
    ("Cargo.lock", "a dependency moved; check whether it is on the runtime path"),
]


def changed_files(base: str, head: str) -> list[str]:
    out = subprocess.run(
        ["git", "diff", "--name-only", f"{base}..{head}"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [line for line in out.stdout.splitlines() if line]


def verdict(files: list[str]) -> tuple[bool, list[tuple[str, str]]]:
    """Returns (required, [(path, reason), ...]) — the reasons that fired, with a file."""
    hits: list[tuple[str, str]] = []
    for prefix, reason in TRIGGERS:
        for path in files:
            if path == prefix or path.startswith(prefix):
                hits.append((path, reason))
                break
    return bool(hits), hits


def main() -> int:
    argv = sys.argv[1:]
    if argv[:1] == ["--files-from"]:
        if len(argv) != 2:
            print("usage: soak_required.py --files-from <path|->", file=sys.stderr)
            return 2
        source = sys.stdin if argv[1] == "-" else open(argv[1], encoding="utf-8")
        files = [line.strip() for line in source if line.strip()]
    elif len(argv) == 2:
        files = changed_files(argv[0], argv[1])
    else:
        print(__doc__, file=sys.stderr)
        return 2

    required, hits = verdict(files)
    print(f"{len(files)} file(s) changed")
    if required:
        print("\nSOAK REQUIRED — these changes can reach what the soak measures:\n")
        for path, reason in hits:
            print(f"  {path}\n      {reason}")
        print(
            "\nRun all three: twelve hours release-profile, one hour at 10,000 "
            "processes,\nand one hour on the real collector on Linux. See "
            "docs/soak-testing.md, and read it\nbefore you start — two of its traps were "
            "only found by running it for real."
        )
    else:
        print(
            "\nSoak not required by the diff. Nothing here touches the data model, the "
            "collectors,\nthe reducer, the sampler, the harness, or a dependency."
            "\n\nIt is still required for every minor release and at least quarterly, "
            "whatever the diff\nsays: dependency and platform drift arrive without one."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())

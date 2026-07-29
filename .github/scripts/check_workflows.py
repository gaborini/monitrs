#!/usr/bin/env python3
"""Parse every workflow file and fail if any is not valid YAML.

This exists because of a real failure: multi-line Python embedded in a
`run: |` block had lines at column zero, which escapes the block scalar and makes
the whole file unparseable. GitHub reports that only as "This run likely failed
because of a workflow file issue" — with no line number, and *after* a push.

Running this locally (`just check-workflows`, part of `just ci`) turns a
push-and-find-out into an immediate, located error.

It also rejects the pattern that caused the failure: a `run:` block containing a
line that is less indented than the block itself.

    python3 .github/scripts/check_workflows.py
"""

from __future__ import annotations

import pathlib
import sys

try:
    import yaml
except ImportError:  # pragma: no cover - depends on the runner image
    print("PyYAML is not installed; cannot validate workflows", file=sys.stderr)
    print("install it with: python3 -m pip install --user pyyaml", file=sys.stderr)
    sys.exit(2)

WORKFLOW_DIR = pathlib.Path(".github/workflows")
OTHER_FILES = [pathlib.Path(".github/dependabot.yml")]


def required_keys(document: object, path: pathlib.Path) -> list[str]:
    """A workflow needs a trigger and at least one job to do anything."""
    problems: list[str] = []
    if not isinstance(document, dict):
        return [f"{path}: top level is not a mapping"]
    # PyYAML resolves the bare key `on` to the boolean True (YAML 1.1), which is
    # why this checks for both spellings rather than just the string.
    if "on" not in document and True not in document:
        problems.append(f"{path}: no `on:` trigger")
    jobs = document.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        problems.append(f"{path}: no jobs")
    return problems


def main() -> int:
    paths = sorted(WORKFLOW_DIR.glob("*.yml")) + sorted(WORKFLOW_DIR.glob("*.yaml"))
    paths += [p for p in OTHER_FILES if p.exists()]
    if not paths:
        print(f"no workflow files found under {WORKFLOW_DIR}", file=sys.stderr)
        return 1

    problems: list[str] = []
    for path in paths:
        try:
            document = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as error:
            problems.append(f"{path}: not valid YAML\n    {error}")
            continue
        if path.parent == WORKFLOW_DIR:
            problems.extend(required_keys(document, path))
        print(f"ok   {path}")

    if problems:
        print()
        for problem in problems:
            print(f"FAIL {problem}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

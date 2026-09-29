#!/usr/bin/env python3
"""Check a release tag against the workspace and print its CHANGELOG section.

    python3 scripts/release.py v0.2.0 > notes.md

Fails unless every workspace package is at the tag's version and CHANGELOG.md has a non-empty
`## [0.2.0] - YYYY-MM-DD` section. The release workflow runs this before publishing anything and
uses the printed section as the GitHub release notes.
"""
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]


def workspace_versions():
    metadata = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return {p["name"]: p["version"] for p in json.loads(metadata)["packages"]}


def changelog_section(version):
    lines = (ROOT / "CHANGELOG.md").read_text().splitlines()
    heading = re.compile(rf"## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}")
    start = next((i for i, line in enumerate(lines) if heading.fullmatch(line)), None)
    if start is None:
        sys.exit(f"CHANGELOG.md has no dated `## [{version}] - YYYY-MM-DD` heading")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    body = "\n".join(lines[start + 1 : end]).strip()
    if not body:
        sys.exit(f"CHANGELOG.md section {version} is empty")
    return body


def main():
    if len(sys.argv) != 2 or not sys.argv[1].startswith("v"):
        sys.exit("usage: release.py vX.Y.Z")
    tag = sys.argv[1]
    version = tag[1:]
    stale = {name: v for name, v in workspace_versions().items() if v != version}
    if stale:
        found = ", ".join(f"{name} {v}" for name, v in sorted(stale.items()))
        sys.exit(f"tag {tag} does not match workspace packages: {found}")
    print(changelog_section(version))


if __name__ == "__main__":
    main()

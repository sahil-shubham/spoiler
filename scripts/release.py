#!/usr/bin/env python3
"""Check a release tag against the workspace version.

    python3 scripts/release.py v0.2.0

Fails unless every workspace package is at the tag's version. The release workflow runs this
before publishing anything.
"""
import json
import pathlib
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


def main():
    if len(sys.argv) != 2 or not sys.argv[1].startswith("v"):
        sys.exit("usage: release.py vX.Y.Z")
    tag = sys.argv[1]
    version = tag[1:]
    stale = {name: v for name, v in workspace_versions().items() if v != version}
    if stale:
        found = ", ".join(f"{name} {v}" for name, v in sorted(stale.items()))
        sys.exit(f"tag {tag} does not match workspace packages: {found}")


if __name__ == "__main__":
    main()

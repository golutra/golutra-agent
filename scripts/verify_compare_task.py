#!/usr/bin/env python3
"""Validate benchmark fixture files without invoking a shell."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--expected-json", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        expected = json.loads(args.expected_json)
    except json.JSONDecodeError as error:
        print(f"invalid expected JSON: {error}", file=sys.stderr)
        return 2
    if not isinstance(expected, dict):
        print("expected JSON must be an object", file=sys.stderr)
        return 2
    root = args.workspace.resolve()
    for relative, content in expected.items():
        if not isinstance(relative, str) or not isinstance(content, str):
            print("expected paths and contents must be strings", file=sys.stderr)
            return 2
        candidate = (root / relative).resolve()
        if root != candidate and root not in candidate.parents:
            print(f"path escapes workspace: {relative}", file=sys.stderr)
            return 1
        try:
            actual = candidate.read_text(encoding="utf-8")
        except OSError as error:
            print(f"cannot read {relative}: {error}", file=sys.stderr)
            return 1
        if actual != content:
            print(f"content mismatch: {relative}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

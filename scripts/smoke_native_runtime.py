#!/usr/bin/env python3
"""Run native configuration/data contracts before the slower release packaging jobs."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

from smoke_native_package import (
    check_desktop_settings,
    check_namespace_isolation,
    check_shared_home,
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-dir", type=Path, required=True)
    args = parser.parse_args()
    suffix = ".exe" if os.name == "nt" else ""
    checks: list[str] = []
    with tempfile.TemporaryDirectory(prefix="golutra runtime smoke ") as directory:
        root = Path(directory).resolve()
        desktop, npm = root / "desktop install", root / "npm install"
        home, workspace, empty_path = root / "shared home", root / "workspace 中文", root / "empty PATH"
        for path in (desktop, npm, home, workspace, empty_path):
            path.mkdir()
        cli = desktop / f"golutra-agent{suffix}"
        npm_cli = npm / cli.name
        server = desktop / f"golutra-agent-app-server{suffix}"
        shutil.copy2(args.binary_dir / cli.name, cli)
        shutil.copy2(cli, npm_cli)
        shutil.copy2(args.binary_dir / server.name, server)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("GOLUTRA_")}
        environment.update({"GOLUTRA_AGENT_HOME": str(home), "PATH": str(empty_path)})
        check_namespace_isolation(cli, root, environment, checks)
        check_shared_home(cli, npm_cli, home, workspace, environment, checks)
        check_desktop_settings(server, cli, home, workspace, environment, checks)
    print(json.dumps({"passed": True, "checks": checks,
                      "limitations": ["CI native runtime binaries; release archive verification remains required",
                                      "local mock/provider fixture; no cloud inference"]}, indent=2))


if __name__ == "__main__":
    main()

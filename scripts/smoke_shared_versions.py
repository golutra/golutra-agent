#!/usr/bin/env python3
"""用真实新旧原生 CLI 验收双向拒绝；不升级或续跑旧数据；版本号和二进制摘要同时记入报告。"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile

from desktop_release import digest
from smoke_native_package import run, database_connection
from smoke_npm_package import extract_package


def check_rejection_boundary(baseline: Path, candidate: Path, first: Path, home: Path,
                             workspace: Path, env: dict) -> list[str]:
    (home / "provider.json").write_text(json.dumps({"version": 2, "active_profile": "mock", "profiles": [
        {"name": "mock", "protocol": "mock", "model_id": "mock-model", "enabled": True}
    ]}), encoding="utf-8")
    exec_args = ["exec", "--json", "--tool-profile", "none", "--no-project-verifier-discovery"]
    run(first, [*exec_args, "format boundary fixture"], workspace, env)
    database = home / "state" / "runtime.sqlite"
    other = candidate if first == baseline else baseline
    with database_connection(database) as connection:
        before = list(connection.iterdump())
    for args in (["thread", "list"], [*exec_args, "must refuse incompatible format"]):
        error = run(other, args, workspace, env, success=False)
        normalized = " ".join(error.replace("│", " ").split())
        if not ("data_unsupported" in error or
                ("schema migration" in normalized and
                 ("unsupported" in normalized or "gap" in normalized))):
            raise ValueError("reader did not fail at the format boundary")
    with database_connection(database) as connection:
        if list(connection.iterdump()) != before:
            raise ValueError("refused reader changed the database")
        if connection.execute("PRAGMA integrity_check").fetchone() != ("ok",):
            raise ValueError("refused reader damaged database integrity")
    # 拒绝后原创建者必须仍可读，证明没有自动升级、重置或破坏账本。
    run(first, ["thread", "list"], workspace, env)
    return ["incompatible reader and execution rejected before logical database writes",
            "original creator still reads its unchanged data",
            "SQLite integrity_check=ok after refusal"]


def smoke(baseline: Path, candidate: Path) -> dict:
    binaries = [baseline.resolve(strict=True), candidate.resolve(strict=True)]
    records = [{"path": str(binary), "sha256": digest(binary),
                "version": subprocess.check_output([str(binary), "--version"], text=True, timeout=90).strip()}
               for binary in binaries]
    if records[0]["sha256"] == records[1]["sha256"]:
        raise ValueError("cross-build acceptance requires two distinct native binaries")
    checks = []
    # 两种启动顺序都要覆盖，防止仅验证新程序创建的数据库。
    for order in (binaries, list(reversed(binaries))):
        with tempfile.TemporaryDirectory(prefix="golutra version matrix ") as directory:
            root = Path(directory).resolve()
            home, workspace, empty_path = root / "shared home", root / "项目", root / "empty-path"
            for path in (home, workspace, empty_path):
                path.mkdir(parents=True)
            env = {key: value for key, value in os.environ.items() if not key.startswith("GOLUTRA_")}
            env["PATH"] = str(empty_path)
            env["GOLUTRA_AGENT_HOME"] = str(home)
            # 仅负向夹具强制旧程序打开同一目录；当前产品不接受这个旧变量。
            env["GOLUTRA_HOME"] = str(home)
            stage_checks = check_rejection_boundary(binaries[0], binaries[1], order[0], home, workspace, env)
            checks.append({"first_binary_sha256": digest(order[0]),
                           "home_selection": "explicit", "checks": stage_checks})
    return {"schema_version": 1, "passed": True, "binaries": records,
            "expected_compatibility": "reject",
            "shared_data_compatible": False,
            "system": {"os": platform.system(), "release": platform.release(), "machine": platform.machine()},
            "orders": checks, "limitations": ["only the exact recorded binary pair and stated compatibility outcome",
                "legacy binaries are not supported; no live mixed-version use",
                "local fixture, not cloud inference; current-build sharing is checked by native package smoke"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    baseline = parser.add_mutually_exclusive_group(required=True)
    baseline.add_argument("--baseline-cli", type=Path)
    baseline.add_argument("--baseline-npm-archive", type=Path)
    parser.add_argument("--baseline-version", default="0.2.0")
    parser.add_argument("--candidate-cli", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.baseline_cli:
        report = smoke(args.baseline_cli, args.candidate_cli)
    else:
        with tempfile.TemporaryDirectory(prefix="golutra released baseline ") as directory:
            package = extract_package(args.baseline_npm_archive, Path(directory))
            metadata = json.loads((package / "package.json").read_text(encoding="utf-8"))
            host_os = {"Darwin": "darwin", "Linux": "linux", "Windows": "win32"}[platform.system()]
            host_cpu = {"aarch64": "arm64", "arm64": "arm64", "amd64": "x64", "x86_64": "x64"}[platform.machine().lower()]
            if (metadata.get("name") != f"@golutra/agent-{host_os}-{host_cpu}"
                    or metadata.get("version") != args.baseline_version):
                raise ValueError("baseline package does not match pinned version/native platform")
            binary = package / "vendor" / "bin" / ("golutra.exe" if host_os == "win32" else "golutra")
            report = smoke(binary, args.candidate_cli)
            report["baseline_package"] = {"name": metadata["name"], "version": metadata["version"],
                                          "archive_sha256": digest(args.baseline_npm_archive)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

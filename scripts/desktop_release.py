#!/usr/bin/env python3
"""Create a pinned desktop inventory from verified native + npm release artifacts."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tarfile

from package_npm import PLATFORMS, NATIVE_BINARY_SPECS, validate_version, _verify_tarball
from package_release import PackageError, verify_package


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def verify_npm_parity(native: dict, npm_archive: Path) -> None:
    """校验归档内实际字节，不能仅相信两份可漂移的外置清单。"""
    platform = PLATFORMS[native["target"]]
    suffix = ".exe" if platform.os == "win32" else ""
    paths = [f"bin/{name}{suffix}" for _, name in NATIVE_BINARY_SPECS]
    _verify_tarball(
        npm_archive, expected_name=platform.package_name,
        expected_version=native["version"],
        required_paths={"package/package.json", *(f"package/vendor/{p}" for p in paths),
                        "package/LICENSE", "package/NOTICE"},
    )
    records = {record["path"]: record for record in native["files"]}
    with tarfile.open(npm_archive, "r:gz") as archive:
        for path in [*paths, "LICENSE", "NOTICE"]:
            npm_path = f"package/vendor/{path}" if path.startswith("bin/") else f"package/{path}"
            source = archive.extractfile(npm_path)
            if source is None or hashlib.sha256(source.read()).hexdigest() != records[path]["sha256"]:
                raise PackageError(f"npm/native payload mismatch: {path}")


def inventory(dist: Path, *, repository: str = "golutra/golutra-agent",
              allow_partial: bool = False, allow_development: bool = False) -> dict:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", repository):
        raise PackageError("repository must be a GitHub owner/name")
    artifacts = []
    versions, targets, commits = set(), set(), set()
    for sidecar in sorted(dist.glob("golutra-agent-v*.manifest.json")):
        archive = sidecar.with_name(sidecar.name.removesuffix(".manifest.json"))
        native = verify_package(archive)
        version, target = native["version"], native["target"]
        validate_version(version)
        if target not in PLATFORMS or target in targets:
            raise PackageError(f"unsupported or duplicate desktop target: {target}")
        if not allow_development and (native["profile"] != "release" or native["git_dirty"]):
            raise PackageError("desktop release requires clean release builds")
        platform = PLATFORMS[target]
        npm_archive = dist / "npm" / f"golutra-agent-npm-{platform.npm_suffix}-{version}.tgz"
        verify_npm_parity(native, npm_archive)
        smoke_path = Path(f"{archive}.smoke.json")
        smoke = json.loads(smoke_path.read_text(encoding="utf-8"))
        sha = digest(archive)
        if (smoke.get("passed") is not True or smoke.get("target") != target
                or smoke.get("archive_sha256") != sha
                or smoke.get("npm_sha256") != digest(npm_archive)):
            raise PackageError(f"missing or stale native runtime acceptance: {target}")
        contract = native.get("desktop_launch")
        suffix = ".exe" if platform.os == "win32" else ""
        required_contract = {"contract_version": 2, "cli": f"bin/golutra-agent{suffix}",
                             "tui": f"bin/golutra-agent-tui{suffix}", "home_env": "GOLUTRA_AGENT_HOME",
                             "node_required": False, "install_downloads": False, "update_owner": "distributor"}
        if not isinstance(contract, dict) or any(contract.get(key) != value for key, value in required_contract.items()):
            raise PackageError("native archive does not declare the Agent desktop launch contract 2")
        matrix_path = dist / f"shared-versions-{target}.json"
        matrix = json.loads(matrix_path.read_text(encoding="utf-8")) if matrix_path.exists() else None
        cli_hash = next(record["sha256"] for record in native["files"] if record["path"] == contract["cli"])
        if matrix is None and not allow_development:
            raise PackageError(f"missing cross-build shared data acceptance: {target}")
        if matrix is not None:
            binaries = matrix.get("binaries", [])
            outcome = matrix.get("expected_compatibility")
            limited = matrix.get("limited_acceptance", {})
            known_legacy_limitation = (
                platform.os == "win32" and outcome == "legacy_startup_unavailable"
                and matrix.get("legacy_baseline_available") is False
                and len(binaries) == 2 and binaries[0].get("version", "").split()[-1:] == ["0.2.0"]
                and limited.get("reason") == "windows-sqlite-url"
                and limited.get("legacy_reader_blocked_without_data_changes") is True
                and limited.get("candidate_rejected_old_schema_without_data_changes") is True
                and limited.get("old_schema_fixture") == {"version": 5, "synthetic": True}
                and bool(matrix.get("limitations")))
            if (matrix.get("passed") is not True or len(binaries) != 2
                    or binaries[1].get("sha256") != cli_hash
                    or binaries[0].get("sha256") == cli_hash
                    or (outcome != "reject" and not known_legacy_limitation)
                    or matrix.get("shared_data_compatible") is not False):
                raise PackageError(f"stale cross-build shared data acceptance: {target}")
        artifacts.append({
            "target": target, "platform": platform.os, "arch": platform.cpu,
            "archive": archive.name, "sha256": sha, "size": archive.stat().st_size,
            "url": f"https://github.com/{repository}/releases/download/v{version}/{archive.name}",
            "package_root": native["package_root"], "launch": contract,
            "files": native["files"], "npm_package": platform.package_name,
            "npm_sha256": digest(npm_archive), "runtime_acceptance": smoke["checks"],
            "runtime_limitations": smoke.get("limitations", []),
            "tested_system": smoke["system"],
            "shared_data_acceptance": matrix,
        })
        versions.add(version)
        targets.add(target)
        commits.add(native["git_commit"])
    if len(versions) != 1 or len(commits) != 1:
        raise PackageError("inventory requires one version and one source commit")
    if not allow_partial and targets != set(PLATFORMS):
        raise PackageError(f"missing native runtime acceptance: {sorted(set(PLATFORMS) - targets)}")
    return {"schema_version": 1, "version": versions.pop(), "git_commit": commits.pop(), "repository": repository,
            "development_only": allow_development, "complete": targets == set(PLATFORMS),
            "artifacts": artifacts}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"),
                        required=not os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--allow-partial", action="store_true")
    parser.add_argument("--allow-development", action="store_true")
    args = parser.parse_args()
    try:
        result = inventory(args.dist, repository=args.repository, allow_partial=args.allow_partial,
                           allow_development=args.allow_development)
        path = args.dist / f"desktop-release-v{result['version']}.json"
        path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(path)
        return 0
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print(f"desktop inventory failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

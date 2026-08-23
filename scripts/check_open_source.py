#!/usr/bin/env python3
"""Check repository metadata required for an Apache-2.0 public project."""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from pathlib import Path
from typing import Any


CANONICAL_REPOSITORY = "https://github.com/golutra/golutra-agent"
REQUIRED_FILES = (
    "LICENSE",
    "NOTICE",
    "README.md",
    "assets/readme/golutra-logo.png",
    "assets/readme/golutra-concept-hero.png",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".gitattributes",
    "CONTRIBUTING.md",
    "CODE_OF_CONDUCT.md",
    "SECURITY.md",
    "GOVERNANCE.md",
    "SUPPORT.md",
    "CHANGELOG.md",
    ".github/CODEOWNERS",
    ".github/dependabot.yml",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/ISSUE_TEMPLATE/config.yml",
    ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/ISSUE_TEMPLATE/feature_request.yml",
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    "npm/agent/package.json",
    "npm/agent/README.md",
    "npm/agent/bin/golutra.js",
    "npm/agent/bin/golutra-tui.js",
    "npm/agent/bin/run.js",
    "scripts/package_npm.py",
    "scripts/smoke_npm_package.py",
)
WORKSPACE_METADATA_FIELDS = (
    "description",
    "readme",
    "homepage",
    "documentation",
    "keywords",
    "categories",
)


def _load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as source:
        return tomllib.load(source)


def _workspace_inherited(value: Any) -> bool:
    return value == {"workspace": True}


def _metadata_is_present(field: str, value: Any) -> bool:
    if _workspace_inherited(value):
        return True
    if field in {"license", "repository", "homepage", "documentation", "rust-version"}:
        return isinstance(value, str) and bool(value)
    if field == "description":
        return isinstance(value, str) and bool(value.strip())
    return isinstance(value, list) and bool(value)


def _path_dependencies_without_versions(value: Any, prefix: str = "") -> list[str]:
    missing: list[str] = []
    if isinstance(value, dict):
        if "path" in value and "version" not in value:
            missing.append(prefix or "dependency")
        for key, child in value.items():
            child_prefix = f"{prefix}.{key}" if prefix else str(key)
            missing.extend(_path_dependencies_without_versions(child, child_prefix))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            missing.extend(_path_dependencies_without_versions(child, f"{prefix}[{index}]"))
    return missing


def _canonical_url(value: Any) -> str:
    if not isinstance(value, str):
        return ""
    return value.removesuffix("/").removesuffix(".git")


def check_repository(root: Path) -> list[str]:
    errors: list[str] = []

    for relative in REQUIRED_FILES:
        if not (root / relative).is_file():
            errors.append(f"required public file is missing: {relative}")

    license_path = root / "LICENSE"
    if license_path.is_file():
        license_text = license_path.read_text(encoding="utf-8")
        if "Apache License" not in license_text or "Version 2.0" not in license_text:
            errors.append("LICENSE is not the Apache License 2.0 text")
        if "Business Source License" in license_text or "MIT License" in license_text:
            errors.append("LICENSE contains a stale non-Apache license name")

    notice_path = root / "NOTICE"
    if notice_path.is_file():
        notice_text = notice_path.read_text(encoding="utf-8")
        if "Golutra Agent" not in notice_text:
            errors.append("NOTICE does not identify Golutra Agent")
        if "assets/readme/" not in notice_text:
            errors.append("NOTICE does not describe README asset provenance")

    cargo_path = root / "Cargo.toml"
    if cargo_path.is_file():
        cargo = _load_toml(cargo_path)
        workspace = cargo.get("workspace", {})
        package = workspace.get("package", {})
        if package.get("license") != "Apache-2.0":
            errors.append("Cargo workspace license must be exactly Apache-2.0")
        if package.get("repository") != CANONICAL_REPOSITORY:
            errors.append("Cargo workspace repository does not use the canonical URL")
        for field in WORKSPACE_METADATA_FIELDS:
            if field not in package:
                errors.append(f"Cargo workspace metadata is missing: {field}")
        for dependency in _path_dependencies_without_versions(cargo, "Cargo.toml"):
            errors.append(f"path dependency has no version requirement: {dependency}")

        for member in workspace.get("members", []):
            manifest_path = root / member / "Cargo.toml"
            if not manifest_path.is_file():
                errors.append(f"Cargo workspace member manifest is missing: {member}")
                continue
            member_manifest = _load_toml(manifest_path)
            member_package = member_manifest.get("package", {})
            for dependency in _path_dependencies_without_versions(member_manifest, member):
                errors.append(f"path dependency has no version requirement: {dependency}")
            for field in ("license", "repository", "rust-version", *WORKSPACE_METADATA_FIELDS):
                value = member_package.get(field)
                if not _metadata_is_present(field, value):
                    errors.append(f"{member} is missing usable workspace metadata: {field}")
                elif field == "license" and not _workspace_inherited(value) and value != "Apache-2.0":
                    errors.append(f"{member} must use Apache-2.0")
                elif field in {"repository", "homepage"} and not _workspace_inherited(value):
                    if value != CANONICAL_REPOSITORY:
                        errors.append(f"{member} has a non-canonical {field} URL")

    python_path = root / "sdk/python/pyproject.toml"
    if python_path.is_file():
        project = _load_toml(python_path).get("project", {})
        license_value = project.get("license")
        if not (
            license_value == "Apache-2.0"
            or isinstance(license_value, dict)
            and license_value.get("text") == "Apache-2.0"
        ):
            errors.append("Python SDK license must identify Apache-2.0")
        urls = project.get("urls", {})
        if urls.get("Repository") != CANONICAL_REPOSITORY:
            errors.append("Python SDK repository URL is missing or incorrect")

    typescript_path = root / "sdk/typescript/package.json"
    if typescript_path.is_file():
        package_json = json.loads(typescript_path.read_text(encoding="utf-8"))
        if package_json.get("license") != "Apache-2.0":
            errors.append("TypeScript SDK license must be exactly Apache-2.0")
        repository = package_json.get("repository", {})
        repository_url = repository.get("url") if isinstance(repository, dict) else None
        if _canonical_url(repository_url) != CANONICAL_REPOSITORY:
            errors.append("TypeScript SDK repository URL is missing or incorrect")
        if package_json.get("private") is not True:
            errors.append("TypeScript SDK must remain private until its publish contract is defined")

    npm_path = root / "npm/agent/package.json"
    if npm_path.is_file():
        npm_package = json.loads(npm_path.read_text(encoding="utf-8"))
        if npm_package.get("name") != "@golutra/agent":
            errors.append("npm launcher package name must be @golutra/agent")
        if npm_package.get("private") is True:
            errors.append("npm launcher package must not be private")
        if npm_package.get("license") != "Apache-2.0":
            errors.append("npm launcher package license must be exactly Apache-2.0")
        repository = npm_package.get("repository", {})
        repository_url = repository.get("url") if isinstance(repository, dict) else None
        if _canonical_url(repository_url) != CANONICAL_REPOSITORY:
            errors.append("npm launcher repository URL is missing or incorrect")
        if _canonical_url(npm_package.get("homepage")) != CANONICAL_REPOSITORY:
            errors.append("npm launcher homepage URL is missing or incorrect")
        publish_config = npm_package.get("publishConfig", {})
        if not isinstance(publish_config, dict) or publish_config.get("access") != "public":
            errors.append("npm launcher package must publish with public access")
        if npm_package.get("bin") != {
            "golutra": "bin/golutra.js",
            "golutra-tui": "bin/golutra-tui.js",
        }:
            errors.append("npm launcher package bin map is incomplete")
        lifecycle_scripts = {
            "preinstall",
            "install",
            "postinstall",
            "prepare",
        }
        scripts = npm_package.get("scripts", {})
        if not isinstance(scripts, dict):
            errors.append("npm launcher package scripts must be a JSON object")
            scripts = {}
        if lifecycle_scripts.intersection(scripts):
            errors.append("npm launcher must not use install-time lifecycle scripts")
        if "optionalDependencies" in npm_package:
            errors.append("npm launcher optionalDependencies must be generated at release time")

        npm_readme = root / "npm/agent/README.md"
        if npm_readme.is_file():
            npm_readme_text = npm_readme.read_text(encoding="utf-8")
            for phrase in (
                "npm install -g @golutra/agent",
                "golutra\n",
                "golutra exec",
                "golutra-tui",
                "does not run a network download script",
            ):
                if phrase not in npm_readme_text:
                    errors.append(f"npm/agent/README.md does not contain: {phrase}")

        package_script = root / "scripts/package_npm.py"
        if package_script.is_file():
            package_script_text = package_script.read_text(encoding="utf-8")
            for platform_suffix in (
                "linux-x64",
                "linux-arm64",
                "darwin-x64",
                "darwin-arm64",
                "win32-x64",
                "win32-arm64",
            ):
                if f'"{platform_suffix}"' not in package_script_text:
                    errors.append(
                        f"npm packaging script does not define {platform_suffix}"
                    )
            if "npm pack" not in package_script_text:
                errors.append("npm packaging script must use npm pack")

        release_workflow = root / ".github/workflows/release.yml"
        if release_workflow.is_file():
            release_workflow_text = release_workflow.read_text(encoding="utf-8")
            if "npm install --global npm@11.17.0" not in release_workflow_text:
                errors.append(
                    "release workflow must install an OIDC-capable npm version"
                )
            if release_workflow_text.count(
                "NPM_BOOTSTRAP_TOKEN: ${{ secrets.NPM_BOOTSTRAP_TOKEN }}"
            ) != 2:
                errors.append(
                    "release workflow must scope the npm bootstrap token to both publish steps"
                )
            if release_workflow_text.count(
                'export NODE_AUTH_TOKEN="$NPM_BOOTSTRAP_TOKEN"'
            ) != 2:
                errors.append(
                    "release workflow must conditionally expose the bootstrap token to npm"
                )
            if 'test "${GITHUB_REF_NAME#v}" = "$version"' not in release_workflow_text:
                errors.append(
                    "release workflow must require the tag to match the package version"
                )
            if 'test "${#platform_manifests[@]}" -eq 6' not in release_workflow_text:
                errors.append(
                    "release workflow must require all six native npm manifests"
                )
            if "is already published; skipping" not in release_workflow_text:
                errors.append(
                    "release workflow must support retrying partially published npm releases"
                )
            for runner, target, platform_suffix in (
                ("ubuntu-latest", "x86_64-unknown-linux-gnu", "linux-x64"),
                ("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "linux-arm64"),
                ("macos-15-intel", "x86_64-apple-darwin", "darwin-x64"),
                ("macos-15", "aarch64-apple-darwin", "darwin-arm64"),
                ("windows-latest", "x86_64-pc-windows-msvc", "win32-x64"),
                ("windows-11-arm", "aarch64-pc-windows-msvc", "win32-arm64"),
            ):
                matrix_entry = (
                    f"os: {runner}\n"
                    f"            target: {target}\n"
                    f"            npm_suffix: {platform_suffix}"
                )
                if matrix_entry not in release_workflow_text:
                    errors.append(
                        "release workflow is missing the npm matrix entry: "
                        f"{runner}/{target}/{platform_suffix}"
                    )

    for relative in ("Cargo.toml", "sdk/python/pyproject.toml", "sdk/typescript/package.json"):
        path = root / relative
        if path.is_file() and "MIT OR Apache-2.0" in path.read_text(encoding="utf-8"):
            errors.append(f"{relative} still contains the old dual-license declaration")

    readme = root / "README.md"
    if readme.is_file():
        readme_text = readme.read_text(encoding="utf-8")
        for link in (
            "LICENSE",
            "CONTRIBUTING.md",
            "SECURITY.md",
            "docs/README.md",
            "assets/readme/golutra-logo.png",
            "assets/readme/golutra-concept-hero.png",
            "npm install -g @golutra/agent",
        ):
            if link not in readme_text:
                errors.append(f"README.md does not link to {link}")

    return errors


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    return parser.parse_args(sys.argv[1:] if argv is None else argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    root = args.root.resolve()
    try:
        errors = check_repository(root)
    except (OSError, UnicodeError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"open-source metadata check failed: {error}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"open-source metadata ok: {root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

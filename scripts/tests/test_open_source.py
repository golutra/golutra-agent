from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check_open_source.py"
SPEC = importlib.util.spec_from_file_location("check_open_source", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
check_open_source = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = check_open_source
SPEC.loader.exec_module(check_open_source)


class OpenSourceMetadataTest(unittest.TestCase):
    def test_repository_has_required_public_metadata(self) -> None:
        root = Path(__file__).resolve().parents[2]
        self.assertEqual(check_open_source.check_repository(root), [])

    def test_npm_launcher_is_a_public_wrapper_without_install_hooks(self) -> None:
        root = Path(__file__).resolve().parents[2]
        package = json.loads(
            (root / "npm/agent/package.json").read_text(encoding="utf-8")
        )
        self.assertEqual(package["name"], "@golutra/agent")
        self.assertEqual(package["license"], "Apache-2.0")
        self.assertEqual(package["publishConfig"]["access"], "public")
        self.assertEqual(
            package["bin"],
            {"golutra": "bin/golutra.js", "golutra-tui": "bin/golutra-tui.js"},
        )
        self.assertNotIn("optionalDependencies", package)
        self.assertFalse(
            set(package.get("scripts", {})).intersection(
                {"preinstall", "install", "postinstall", "prepare"}
            )
        )
        readme = (root / "npm/agent/README.md").read_text(encoding="utf-8")
        self.assertIn("npm install -g @golutra/agent", readme)
        self.assertIn("does not run a network download script", readme)

    def test_release_scopes_bootstrap_auth_to_publish_steps(self) -> None:
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(
            workflow.count(
                "NPM_BOOTSTRAP_TOKEN: ${{ secrets.NPM_BOOTSTRAP_TOKEN }}"
            ),
            2,
        )
        self.assertEqual(
            workflow.count('export NODE_AUTH_TOKEN="$NPM_BOOTSTRAP_TOKEN"'),
            2,
        )
        self.assertIn('test "${GITHUB_REF_NAME#v}" = "$version"', workflow)
        self.assertIn('test "${#platform_manifests[@]}" -eq 6', workflow)
        self.assertIn("is already published; skipping", workflow)


if __name__ == "__main__":
    unittest.main()

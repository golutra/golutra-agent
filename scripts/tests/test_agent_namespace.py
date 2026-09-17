"""Agent 自有命名空间与桌面连接合同的静态发布边界。"""
import json
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
DESKTOP_ENV = {"GOLUTRA_COMMAND_IPC_ADDR", "GOLUTRA_COMMAND_IPC_PATH", "GOLUTRA_COMMAND_SCOPE_TOKEN",
               "GOLUTRA_RUNTIME_PROFILE", "GOLUTRA_RUNTIME_HOST_KIND"}


class AgentNamespaceTest(unittest.TestCase):
    def test_all_crates_and_dependencies_are_agent_owned(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
        for member in workspace["workspace"]["members"]:
            self.assertTrue(member.startswith("crates/golutra-agent-"), member)
            manifest = tomllib.loads((ROOT / member / "Cargo.toml").read_text())
            self.assertTrue(manifest["package"]["name"].startswith("golutra-agent-"))
            for group in ("dependencies", "dev-dependencies", "build-dependencies"):
                for name in manifest.get(group, {}):
                    if name.startswith("golutra-"):
                        self.assertTrue(name.startswith("golutra-agent-"), name)

    def test_public_commands_have_no_desktop_alias(self):
        package = json.loads((ROOT / "npm/agent/package.json").read_text())
        self.assertEqual(set(package["bin"]), {"golutra-agent", "golutra-agent-tui"})
        self.assertFalse((ROOT / "npm/agent/bin/golutra.js").exists())
        self.assertIn('runNative("golutra-agent")', (ROOT / "npm/agent/bin/golutra-agent.js").read_text())

    def test_rust_environment_names_are_owned_or_explicit_desktop_contracts(self):
        for file in (ROOT / "crates").rglob("*.rs"):
            for name in re.findall(r'\bGOLUTRA_[A-Z0-9_]+', file.read_text()):
                self.assertTrue(name.startswith("GOLUTRA_AGENT_") or name in DESKTOP_ENV,
                                f"{file.relative_to(ROOT)}: {name}")


if __name__ == "__main__":
    unittest.main()

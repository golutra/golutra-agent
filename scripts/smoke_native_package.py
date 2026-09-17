#!/usr/bin/env python3
"""Exercise a desktop native archive without Node/npm; never touches the real user home."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from threading import Thread

from desktop_release import digest, verify_npm_parity
from package_npm import PLATFORMS
from package_release import PackageError, _read_archive, verify_package
from smoke_npm_package import extract_package, run_unix_pty_command


@contextmanager
def local_provider_probe():
    # mock CLI profile 固定名为 mock；用本地 HTTP fixture 才能真实验证不同 profile 的登录事务。
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_json({"object": "list", "data": [{"id": "fixture", "object": "model", "owned_by": "test"}]})

        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            self.send_json({"id": "offline-probe", "object": "chat.completion", "created": 0,
                "model": "fixture", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                                                  "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})

        def send_json(self, value):
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            worker.join(timeout=5)


def run(binary: Path, args: list[str], workspace: Path, environment: dict,
        *, success: bool = True) -> str:
    result = subprocess.run([str(binary), "--cwd", str(workspace), *args],
                            cwd=workspace, env=environment, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, encoding="utf-8", timeout=90)
    if (result.returncode == 0) != success:
        raise PackageError(f"native command {args} returned {result.returncode}: {result.stderr}")
    return result.stdout if success else result.stdout + result.stderr


@contextmanager
def database_connection(path: Path):
    # sqlite3 的事务上下文不关闭句柄；Windows 清理临时目录前必须显式关闭。
    connection = sqlite3.connect(path)
    try:
        with connection:
            yield connection
    finally:
        connection.close()


def check_namespace_isolation(cli: Path, root: Path, environment: dict, checks: list[str]) -> None:
    user = root / "isolated user 中文"
    workspace = root / "isolated project"
    legacy_home = user / ".golutra"
    legacy_project = workspace / ".golutra"
    for directory in (legacy_home, legacy_project):
        directory.mkdir(parents=True)
        (directory / "runtime.json").write_bytes(b"invalid desktop configuration; do not read")
    (legacy_home / "provider.json").write_bytes(b"invalid old provider; do not read")
    before = {file: file.read_bytes() for directory in (legacy_home, legacy_project)
              for file in directory.iterdir()}
    env = {key: value for key, value in environment.items() if not key.startswith("GOLUTRA_")}
    env.update({"HOME": str(user), "USERPROFILE": str(user), "GOLUTRA_HOME": str(legacy_home),
                "GOLUTRA_PROVIDER_PROTOCOL": "invalid-old-provider"})
    run(cli, ["thread", "list"], workspace, env)
    current = user / ".golutra-agent"
    if not (current / "state" / "runtime.sqlite").is_file():
        raise PackageError("default Agent home was not isolated from desktop/old home")
    (current / "provider.json").write_text(json.dumps({"version": 2, "active_profile": "mock", "profiles": [
        {"name": "mock", "protocol": "mock", "model_id": "mock-model", "enabled": True}
    ]}), encoding="utf-8")
    exec_args = ["exec", "--json", "--tool-profile", "none", "--no-project-verifier-discovery", "namespace fixture"]
    if "mock provider completed" not in run(cli, exec_args, workspace, env):
        raise PackageError("current Agent execution did not ignore invalid old configuration")
    if set(legacy_home.iterdir()) | set(legacy_project.iterdir()) != set(before):
        raise PackageError("Agent created files in desktop/old directories")
    if any(file.read_bytes() != content for file, content in before.items()):
        raise PackageError("Agent modified desktop/old configuration")
    # 新项目目录必须仍生效，不能靠忽略全部项目配置来通过隔离测试。
    project_settings = workspace / ".golutra-agent"
    project_settings.mkdir()
    (project_settings / "runtime.json").write_bytes(b"invalid current config")
    # thread list 是不依赖模型设置的诊断命令；实际执行才加载项目运行设置。
    error = run(cli, exec_args, workspace, env, success=False)
    if ".golutra-agent/runtime.json" not in error.replace("\\", "/"):
        raise PackageError("current project configuration was not read at execution time")
    checks.append("new default home and project namespace; old variables/directories ignored and unchanged")


def check_shared_home(cli: Path, npm_cli: Path, home: Path, workspace: Path,
                      environment: dict, checks: list[str]) -> None:
    # 离线 mock 只验证执行/持久化合同，不宣称验证真实模型或联网工具。
    config = home / "provider.json"
    config.write_text(json.dumps({"version": 2, "active_profile": "mock", "profiles": [
        {"name": "mock", "protocol": "mock", "model_id": "mock-model", "enabled": True}
    ]}), encoding="utf-8")
    original_config = config.read_bytes()
    args = ["exec", "--json", "--tool-profile", "none", "--no-project-verifier-discovery", "offline smoke"]
    # 真正的独立进程同时打开同一新数据库，覆盖迁移锁及不同会话的并发写入。
    with ThreadPoolExecutor(max_workers=4) as executor:
        outputs = list(executor.map(lambda binary: run(binary, args, workspace, environment),
                                    [cli, npm_cli, cli, npm_cli]))
    if not all("mock provider completed" in output for output in outputs):
        raise PackageError("offline execution did not complete")
    for output in outputs:
        if not all(json.loads(line).get("type") for line in output.splitlines()):
            raise PackageError("exec stdout violated the JSONL contract")
    checks.append("four independent desktop/npm native processes share config and history")
    desktop_history = json.loads(run(cli, ["thread", "list"], workspace, environment))
    npm_history = json.loads(run(npm_cli, ["thread", "list"], workspace, environment))
    if desktop_history != npm_history or not desktop_history:
        raise PackageError("desktop/npm history differs or is empty")
    if config.read_bytes() != original_config:
        raise PackageError("read-only use rewrote provider configuration")
    # 实际并发配置事务应保留两个 profile，而非 last-writer-wins 覆盖。
    with local_provider_probe() as url, ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(run, binary, ["provider", "login", "--protocol", "openai-compatible",
                   "--profile", name, "--base-url", url, "--model", "fixture",
                   "--api-key", "offline-fixture"], workspace, environment)
                   for binary, name in ((cli, "desktop"), (npm_cli, "npm"))]
        for future in futures:
            future.result()
    profiles = {item["name"] for item in json.loads(config.read_text())["profiles"]}
    if not {"desktop", "npm", "mock"} <= profiles:
        raise PackageError("concurrent configuration update lost a profile")
    credentials = json.loads((home / "credentials.json").read_text())
    if len(credentials["credentials"]) != 2:
        raise PackageError("concurrent login lost a credential")
    checks.append("concurrent local-fixture logins preserve both profiles and credentials")
    database = home / "state" / "runtime.sqlite"
    with database_connection(database) as connection:
        if connection.execute("PRAGMA integrity_check").fetchone() != ("ok",):
            raise PackageError("shared SQLite integrity check failed")
        count = connection.execute("SELECT COUNT(*) FROM threads").fetchone()[0]
        if count < 4:
            raise PackageError("concurrent native executions lost history")
        current = connection.execute("SELECT MAX(version) FROM schema_migrations").fetchone()[0]
        connection.execute("INSERT INTO schema_migrations VALUES (?, ?, ?, ?)",
                           (current + 1, "future desktop fixture", "unknown", "future"))
    with database_connection(database) as connection:
        before = list(connection.iterdump())
    for binary in (cli, npm_cli):
        error = run(binary, ["thread", "list"], workspace, environment, success=False)
        if "data_unsupported" not in error:
            raise PackageError(f"future database did not report migration incompatibility: {error}")
    with database_connection(database) as connection:
        if list(connection.iterdump()) != before:
            raise PackageError("unsupported database schema was modified")
        connection.execute("DELETE FROM schema_migrations WHERE version = ?", (current + 1,))
    checks.append("future database schema rejected without logical data changes")
    credentials_before = (home / "credentials.json").read_bytes()
    for version in (1, 999):
        unsupported_config = json.dumps({"version": version, "profiles": [],
                                        "env": {"OLD_SECRET": "must-not-migrate"}}).encode()
        config.write_bytes(unsupported_config)
        for binary in (cli, npm_cli):
            error = run(binary, ["provider", "login", "--protocol", "mock", "--profile", "reject"],
                        workspace, environment, success=False)
            if ("unsupported provider settings version" not in error
                    or config.read_bytes() != unsupported_config
                    or (home / "credentials.json").read_bytes() != credentials_before):
                raise PackageError("unsupported provider config or credentials were modified")
    config.write_bytes(original_config)
    checks.append("old and future provider settings rejected; config and credentials preserved")


def check_desktop_settings(server: Path, cli: Path, home: Path, workspace: Path,
                           environment: dict, checks: list[str]) -> None:
    def rpc(requests):
        result = subprocess.run([str(server), "--stdio"], cwd=workspace, env=environment,
            input="".join(json.dumps({"jsonrpc": "2.0", "id": index, "method": method, "params": params}) + "\n"
                          for index, (method, params) in enumerate(requests)),
            capture_output=True, text=True, encoding="utf-8", timeout=30)
        if result.returncode != 0:
            raise PackageError(f"native stdio server returned {result.returncode}: {result.stderr}")
        return [json.loads(line) for line in result.stdout.splitlines()]

    first = rpc([("config/runtime/read", {})])[0]["result"]
    changed = rpc([("config/runtime/patch", {"expected_revision": first["revision"],
                   "patch": {"subagent_max_concurrent": 10}})])[0]["result"]
    if changed["settings"]["subagent_max_concurrent"] != 10:
        raise PackageError("desktop settings patch was not persisted")
    before = (home / "runtime.json").read_bytes()
    rejected = rpc([
        ("config/runtime/patch", {"expected_revision": first["revision"], "patch": {"model": "stale"}}),
        ("config/runtime/patch", {"expected_revision": changed["revision"], "patch": {"api_key": "forbidden"}}),
        ("config/runtime/patch", {"expected_revision": changed["revision"], "patch": {"subagent_max_concurrent": 0}}),
    ])
    if [reply.get("error", {}).get("code") for reply in rejected] != [-32009, -32602, -32602]:
        raise PackageError("desktop config conflict/validation errors were not explicit")
    if (home / "runtime.json").read_bytes() != before:
        raise PackageError("rejected settings update changed the file")
    status = json.loads(run(cli, ["storage", "status"], workspace, environment))
    identity = status["identity"]
    # Windows Rust canonicalize 使用扩展路径前缀；比较真实目录身份，不比较字符串形式。
    if (not Path(identity["config_home"]).samefile(home)
            or not Path(identity["runtime_home"]).samefile(home)
            or not Path(identity["workspace"]).samefile(workspace)
            or status["compatibility"]["status"] != "supported"):
        raise PackageError("CLI storage identity/compatibility disagrees with desktop home")
    checks.append("stdio settings revision conflicts and field validation preserve data; CLI reports same home")


def smoke(archive: Path, npm_archive: Path, *, require_pty: bool = False) -> dict:
    native = verify_package(archive)
    expected = PLATFORMS[native["target"]]
    host_os = {"Darwin": "darwin", "Linux": "linux", "Windows": "win32"}.get(platform.system())
    host_cpu = {"aarch64": "arm64", "arm64": "arm64", "amd64": "x64", "x86_64": "x64"}.get(platform.machine().lower())
    if (expected.os, expected.cpu) != (host_os, host_cpu):
        raise PackageError("native smoke must run on the matching OS/CPU, not a cross compiler")
    verify_npm_parity(native, npm_archive)
    checks = []
    with tempfile.TemporaryDirectory(prefix="golutra desktop smoke ") as directory:
        root = Path(directory).resolve()
        contents, modes = _read_archive(archive)
        for relative, content in contents.items():
            destination = root / "desktop install" / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)
            destination.chmod(modes[relative] & 0o777)
        installed = root / "desktop install" / native["package_root"]
        npm = extract_package(npm_archive, root / "npm install")
        workspace = root / "workspace 中文"
        workspace.mkdir()
        home = root / "shared home"
        home.mkdir()
        empty_path = root / "empty PATH"
        empty_path.mkdir()
        environment = {key: value for key, value in os.environ.items() if not key.startswith("GOLUTRA_")}
        environment.update({"GOLUTRA_AGENT_HOME": str(home), "PATH": str(empty_path)})
        if shutil.which("node", path=environment["PATH"]) or shutil.which("npm", path=environment["PATH"]):
            raise PackageError("smoke PATH unexpectedly contains Node/npm")
        suffix = ".exe" if expected.os == "win32" else ""
        cli = installed / "bin" / f"golutra-agent{suffix}"
        tui = installed / "bin" / f"golutra-agent-tui{suffix}"
        npm_cli = npm / "vendor" / "bin" / f"golutra-agent{suffix}"
        for binary in (cli, tui, npm_cli):
            version = run(binary, ["--version"], workspace, environment).strip().split()
            if len(version) != 2 or version[-1] != native["version"]:
                raise PackageError(f"native version does not match manifest: {version}")
            run(binary, ["--help"], workspace, environment)
        checks.append("absolute-path version/help without Node/npm; spaces and CJK paths")
        # 缺包时必须失败，不能启动 PATH 或同目录中的旧名桌面程序。
        incomplete = root / "incomplete install"
        incomplete.mkdir()
        isolated_cli = incomplete / cli.name
        shutil.copy2(cli, isolated_cli)
        old_tui = incomplete / f"golutra-tui{suffix}"
        old_tui.write_bytes(b"desktop-owned placeholder")
        error = run(isolated_cli, ["--yolo"], workspace, environment, success=False)
        if "cannot start" not in error or f"golutra-agent-tui{suffix}" not in error:
            raise PackageError("incomplete native installation did not report the missing Agent sibling")
        if old_tui.read_bytes() != b"desktop-owned placeholder":
            raise PackageError("incomplete native installation modified a desktop program")
        checks.append("missing Agent TUI fails explicitly without old-name fallback")
        check_namespace_isolation(cli, root, environment, checks)
        if expected.os == "win32":
            windows_environment = environment.copy()
            windows_environment.pop("HOME", None)
            windows_environment.pop("GOLUTRA_AGENT_HOME", None)
            windows_environment["USERPROFILE"] = str(root / "windows user")
            run(cli, ["thread", "list"], workspace, windows_environment)
            if not (root / "windows user" / ".golutra-agent" / "state" / "runtime.sqlite").exists():
                raise PackageError("native Windows USERPROFILE fallback did not initialize home")
            checks.append("native Windows USERPROFILE fallback without HOME")
        check_shared_home(cli, npm_cli, home, workspace, environment, checks)
        check_desktop_settings(installed / "bin" / f"golutra-agent-app-server{suffix}", cli,
                               home, workspace, environment, checks)
        config = home / "provider.json"
        database = home / "state" / "runtime.sqlite"
        if require_pty:
            # 测试 helper 接收同一无 Node 环境；隔离 onboarding home 不需要网络。
            transcript = run_unix_pty_command([str(cli), "--yolo"], workspace, env=environment)
            if "golutra" not in transcript.lower():
                raise PackageError("native TUI did not render in a real PTY")
            checks.append("unified native entry starts sibling TUI with --yolo in real PTY without Node/npm")
        before_uninstall = config.read_bytes()
        shutil.rmtree(npm)
        run(cli, ["thread", "list"], workspace, environment)
        if config.read_bytes() != before_uninstall or not database.exists():
            raise PackageError("removing npm payload damaged desktop shared data")
        checks.append("npm payload removal leaves desktop native executable and shared data intact")
        npm = extract_package(npm_archive, root / "npm install")
        npm_cli = npm / "vendor" / "bin" / f"golutra-agent{suffix}"
        shutil.rmtree(installed)
        run(npm_cli, ["thread", "list"], workspace, environment)
        if config.read_bytes() != before_uninstall or not database.exists():
            raise PackageError("removing desktop payload damaged shared data")
        checks.append("desktop payload removal leaves npm native executable and shared data intact")
    return {"schema_version": 1, "passed": True, "target": native["target"],
            "archive_sha256": digest(archive), "npm_sha256": digest(npm_archive),
            "system": {"os": platform.system(), "release": platform.release(), "machine": platform.machine()},
            "checks": checks, "limitations": ["offline mock; no live provider", "future schema fixture, not historical binary compatibility", "desktop installer itself not in this repository"]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--npm-archive", type=Path, required=True)
    parser.add_argument("--require-pty", action="store_true")
    args = parser.parse_args()
    try:
        result = smoke(args.archive, args.npm_archive, require_pty=args.require_pty)
        path = Path(f"{args.archive}.smoke.json")
        path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result, indent=2))
        return 0
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"native smoke failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

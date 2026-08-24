#!/usr/bin/env python3
"""Run a native smoke test against a root and platform Golutra npm package."""

from __future__ import annotations

import argparse
import json
import os
import selectors
import signal
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path, PurePosixPath


class SmokeError(RuntimeError):
    pass


def extract_package(archive_path: Path, destination: Path) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    destination_root = destination.resolve()
    seen_names: set[str] = set()
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        for member in members:
            relative = PurePosixPath(member.name)
            if (
                relative.is_absolute()
                or ".." in relative.parts
                or not relative.parts
                or relative.parts[0] != "package"
                or "\\" in member.name
            ):
                raise SmokeError(f"unsafe npm archive path: {member.name}")
            if member.issym() or member.islnk() or not (member.isdir() or member.isfile()):
                raise SmokeError(f"unsupported npm archive member: {member.name}")
            if member.name in seen_names:
                raise SmokeError(f"duplicate npm archive member: {member.name}")
            seen_names.add(member.name)

            target = destination.joinpath(*relative.parts)
            resolved_target = target.resolve()
            if not resolved_target.is_relative_to(destination_root):
                raise SmokeError(f"npm archive path escapes destination: {member.name}")
            if target.is_symlink():
                raise SmokeError(f"destination contains a symlink: {member.name}")
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                target.chmod(member.mode & 0o777)
                continue

            target.parent.mkdir(parents=True, exist_ok=True)
            if target.exists() and target.is_symlink():
                raise SmokeError(f"destination contains a symlink: {member.name}")
            source = archive.extractfile(member)
            if source is None:
                raise SmokeError(f"npm archive member cannot be read: {member.name}")
            target.write_bytes(source.read())
            target.chmod(member.mode & 0o777)
    package_dir = destination / "package"
    if not package_dir.is_dir():
        raise SmokeError(f"npm archive has no package directory: {archive_path}")
    return package_dir


def run_command(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        raise SmokeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def run_unix_pty_command(command: list[str], cwd: Path) -> str:
    """通过真实 PTY 启动入口并返回捕获的终端字节。

    npm 包必须走用户实际调用的交互入口；普通管道无法覆盖 TUI 根据 PTY
    选择的 raw mode 和光标恢复生命周期。
    """
    if os.name == "nt":
        raise SmokeError("real PTY smoke is only available on Unix runners")
    import fcntl
    import pty
    import struct
    import termios

    master_fd, slave_fd = pty.openpty()
    # openpty() starts with a zero-sized window on macOS and Linux.  Ratatui's
    # inline viewport treats that as an unbounded redraw and never reaches a
    # stable first frame, so model the dimensions a real terminal advertises.
    fcntl.ioctl(
        slave_fd,
        termios.TIOCSWINSZ,
        struct.pack("HHHH", 24, 80, 0, 0),
    )
    environment = os.environ.copy()
    environment["GOLUTRA_HOME"] = str(cwd / ".smoke-home")
    environment.setdefault("TERM", "xterm-256color")
    try:
        child = subprocess.Popen(
            command,
            cwd=cwd,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            env=environment,
            start_new_session=True,
        )
    except OSError as error:
        os.close(master_fd)
        os.close(slave_fd)
        raise SmokeError(f"could not start PTY launcher: {error}") from error
    finally:
        os.close(slave_fd)

    selector = selectors.DefaultSelector()
    selector.register(master_fd, selectors.EVENT_READ)
    output = bytearray()
    sent_quit = False
    sent_interrupts = 0
    quit_sent_at: float | None = None
    started_at = time.monotonic()
    deadline = started_at + 20
    try:
        while time.monotonic() < deadline:
            now = time.monotonic()
            # Wait for the native header before writing input.  Sending bytes while
            # the launcher is still replacing the terminal mode can leave them in
            # the shell's canonical input queue and make the smoke hang.
            ready = b"GOLUTRA" in output or now - started_at >= 8
            if not sent_quit and ready:
                try:
                    # The isolated home intentionally opens provider setup on
                    # first launch; option 5 is its deterministic Quit action.
                    os.write(master_fd, b"5\r")
                except OSError as error:
                    raise SmokeError("could not send /quit to PTY launcher") from error
                sent_quit = True
                quit_sent_at = now
            elif (
                sent_quit
                and quit_sent_at is not None
                and sent_interrupts < 2
                and now - quit_sent_at >= 2.0 + sent_interrupts * 0.25
            ):
                # Ctrl-C is intentionally a two-press quit shortcut in the TUI;
                # retain that contract as a state-independent fallback for an
                # initial setup modal or a launcher that has not accepted /quit.
                try:
                    os.write(master_fd, b"\x03")
                except OSError:
                    pass
                sent_interrupts += 1
            events = selector.select(timeout=0.25)
            for _, _ in events:
                try:
                    output.extend(os.read(master_fd, 16 * 1024))
                except OSError:
                    break
            if child.poll() is not None:
                break
        if child.poll() is None:
            try:
                os.write(master_fd, b"\x03")
            except OSError:
                pass
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired as error:
                # 子进程使用独立 session，超时时连同其可能启动的 native 子进程一起清理。
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait(timeout=3)
                raise SmokeError("PTY launcher did not exit after provider Quit/Ctrl-C") from error
        elif child.returncode != 0:
            raise SmokeError(f"PTY launcher exited with status {child.returncode}")
    finally:
        selector.close()
        os.close(master_fd)
    return output.decode("utf-8", errors="replace")


def smoke(
    root_archive: Path,
    platform_archive: Path,
    node_bin: str,
    require_pty: bool,
) -> None:
    with tempfile.TemporaryDirectory(prefix="golutra-npm-smoke-") as temp_dir:
        root_dir = extract_package(root_archive.resolve(), Path(temp_dir) / "root")
        platform_dir = extract_package(
            platform_archive.resolve(), Path(temp_dir) / "platform"
        )
        root_package = json.loads((root_dir / "package.json").read_text(encoding="utf-8"))
        platform_package = json.loads(
            (platform_dir / "package.json").read_text(encoding="utf-8")
        )
        package_name = platform_package["name"]
        if package_name not in root_package.get("optionalDependencies", {}):
            raise SmokeError(f"root package does not declare {package_name}")

        dependency_dir = root_dir / "node_modules" / "@golutra" / package_name.rsplit("/", 1)[1]
        dependency_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(platform_dir, dependency_dir)

        version = root_package["version"]
        for entrypoint in ("golutra.js", "golutra-tui.js"):
            output = run_command(
                [node_bin, str(root_dir / "bin" / entrypoint), "--version"],
                root_dir,
            )
            expected_name = entrypoint.removesuffix(".js")
            if output.strip() != f"{expected_name} {version}":
                raise SmokeError(
                    f"unexpected version output for {entrypoint}: {output!r}"
                )

        run_command([node_bin, str(root_dir / "bin" / "golutra.js"), "--help"], root_dir)
        # 静态检查避免在无真实终端时启动交互 TUI 并阻塞 smoke。
        root_launcher = (root_dir / "bin" / "golutra.js").read_text(encoding="utf-8")
        if 'process.argv.length === 2 ? "golutra-tui" : "golutra"' not in root_launcher:
            raise SmokeError("golutra launcher does not route no-argument use to the TUI")
        run_command(
            [node_bin, str(root_dir / "bin" / "golutra-tui.js"), "--help"], root_dir
        )
        if require_pty:
            transcript = run_unix_pty_command(
                [node_bin, str(root_dir / "bin" / "golutra.js")], root_dir
            )
            if "GOLUTRA" not in transcript and "golutra" not in transcript.lower():
                raise SmokeError("PTY launcher output did not contain the TUI header")
            if transcript.count("\x1b[2J") > 1 or transcript.count("\x1b[H") > 1:
                raise SmokeError(
                    "launcher emitted repeated full-screen clears during the short PTY smoke"
                )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root-tarball", type=Path, required=True)
    parser.add_argument("--platform-tarball", type=Path, required=True)
    parser.add_argument("--node-bin", default="node")
    parser.add_argument(
        "--require-pty",
        action="store_true",
        help="exercise the no-argument launcher through a real Unix PTY",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        smoke(args.root_tarball, args.platform_tarball, args.node_bin, args.require_pty)
    except (OSError, subprocess.SubprocessError, SmokeError, tarfile.TarError, ValueError) as error:
        print(f"npm package smoke test failed: {error}", file=sys.stderr)
        return 1
    print("npm package smoke test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

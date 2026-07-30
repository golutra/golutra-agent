# Golutra Agent

Golutra 是 Rust-first 的本地 Coding Agent Runtime。CLI、TUI、SDK 和 app-server 共享同一套 command/query/event 协议；工作目录只是执行、权限和历史隔离边界，不决定 daemon 生命周期。

## 运行

要求 Rust 1.93、Node.js 22（TypeScript SDK）和 Python 3.11（Python SDK）。

```bash
cargo run -p golutra-tui
cargo run -p golutra-cli -- chat "inspect this workspace"
cargo run -p golutra-app-server -- --addr 127.0.0.1:47831
cargo run -p golutra-cli -- --cwd "$PWD" exec "inspect this workspace"
cargo run -p golutra-cli -- --cwd "$PWD" exec --json "run the checks"
cargo run -p golutra-cli -- --cwd "$PWD" mcp-server
cargo run -p golutra-tui -- --cwd "$PWD" remote --url http://127.0.0.1:47831
```

Cargo 参数后的 `--` 会把后续选项传给 TUI；启用 unrestricted 模式：

```bash
cargo run -p golutra-tui -- --yolo
```

Agent 或自动化测试可以直接驱动真实离屏 TUI：

```bash
cargo run -p golutra-tui -- --cwd "$PWD" inspect --embedded --session new --prompt "hello" --view response+developer
cargo run -p golutra-tui -- --cwd "$PWD" driver --embedded --stdio --session new
```

协议、冻结快照、Unix socket 和安全边界见 [docs/tui-driver.md](docs/tui-driver.md)。

TUI/CLI 默认使用当前进程内的 durable RuntimeHost。显式传入 `--daemon` 时，Unix 本地客户端使用 owner-only Unix socket；Windows 或远程客户端使用经过认证的 HTTP/SSE。

首次没有 live provider 时，TUI 会进入 provider setup。也可以显式配置：

```bash
cargo run -p golutra-cli -- provider protocols
cargo run -p golutra-cli -- provider login --protocol openai-compatible
```

配置和凭据位于 `$GOLUTRA_HOME`（默认 `~/.golutra`）。provider profile 不保存 secret；API key/OAuth token 写入 owner-only `credentials.json`，不会访问 OS keychain。runtime facts、artifact、checkpoint 和 rollout 位于 `$GOLUTRA_HOME/state`，项目内 `.golutra` 不参与持久化。

## 安装

Unix：

```bash
./scripts/install.sh --prefix "$HOME/.local"
```

Windows PowerShell：

```powershell
./scripts/install.ps1 -Prefix "$HOME/.local"
```

安装产物包括 `golutra`、`golutra-tui`、`golutra-app-server`、`golutra-vis`、
`golutra-supervisor`、`golutra-launcher` 和 `golutra-eval-worker`。

构建可分发归档：

```bash
python3 scripts/package_release.py --output-dir dist
python3 scripts/package_release.py --verify dist/golutra-agent-v*-*.tar.gz
```

Windows 目标生成 `.zip`，Unix 目标生成可复现的 `.tar.gz`。每个归档旁都有
`.sha256` 和 `.manifest.json`；manifest 逐个记录 binary 的来源、大小、mode 和 SHA-256。
tag `v*` 会由独立 Release workflow 在 Linux、macOS、Windows 构建并发布这三类文件，
tag 必须与 `workspace.package.version` 完全一致。本仓库不会打包或安装其他桌面 App。

下载 Unix release 后，在归档所在目录校验并安装：

```bash
ARCHIVE="golutra-agent-v0.1.0-aarch64-apple-darwin.tar.gz"
shasum -a 256 -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE"
install -d -m 755 "$HOME/.local/bin"
install -m 755 "${ARCHIVE%.tar.gz}/bin/"* "$HOME/.local/bin/"
```

Linux 可将 `shasum -a 256` 换成 `sha256sum`。Windows 先用 `Get-FileHash -Algorithm SHA256`
与 `.sha256` 第一列核对，再 `Expand-Archive` 并把解压目录的 `bin/*.exe` 放入 PATH。
仓库内还可运行 `package_release.py --verify` 做 manifest 级完整校验。

## 验证

```bash
just fmt-check
just clippy
just test
just schema
just ts-check
just py-check
just release-package-smoke
```

架构入口见 [docs/README.md](docs/README.md)，当前实施状态见 [docs/initial-implementation-plan.md](docs/initial-implementation-plan.md)。
各运行入口的进程模型、SDK 示例和 JSON-RPC/SSE 边界见
[docs/runtime-entrypoints.md](docs/runtime-entrypoints.md)。

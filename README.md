# Golutra Agent

Golutra 是 Rust-first 的本地 Coding Agent Runtime。CLI、TUI、SDK 和 app-server 共享同一套 command/query/event 协议；工作目录只是执行、权限和历史隔离边界，不决定 daemon 生命周期。

## 运行

要求 Rust 1.93、Node.js 22（TypeScript SDK）和 Python 3.11（Python SDK）。

```bash
cargo run -p golutra-tui
cargo run -p golutra-cli -- chat "inspect this workspace"
cargo run -p golutra-app-server -- --addr 127.0.0.1:47831
```

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

安装产物包括 `golutra`、`golutra-tui`、`golutra-app-server` 和 `golutra-vis`。

## 验证

```bash
just fmt-check
just clippy
just test
just schema
just ts-check
just py-check
```

架构入口见 [docs/README.md](docs/README.md)，当前实施状态见 [docs/initial-implementation-plan.md](docs/initial-implementation-plan.md)。

# Extension、SDK 与交付规格

## 当前状态

截至 2026-07-15，Golutra 的扩展和交付层已经进入可运行主链：

- `golutra-plugin` 管理 owner-only 本地插件包，生命周期为 `stage -> review -> enable -> disable/rollback`。
- `golutra-mcp` 使用官方 `rmcp 2.2.0` 适配 stdio MCP server；外部工具进入统一 `ToolRegistry`、PolicyEvaluation、approval、timeout/cancel、artifact/evidence 链路。
- Unix 本地 daemon 默认通过 owner-only Unix socket 连接；socket 请求复用同一个 Axum Router，HTTP/SSE 继续用于 Windows 和远端。
- TypeScript 与 Python SDK 都从 Rust 协议 schema 生成类型，并实现 cwd attachment、command/query、event replay/live stream、thread 与治理 API。
- 根安装脚本覆盖 Unix 和 Windows；CI 对 Linux/macOS/Windows 执行 workspace all-target compile，并在 Linux 执行完整 Rust/SDK 门禁。

## Plugin Store

插件属于用户级扩展，不写入 workspace：

```text
$GOLUTRA_HOME/plugins/
  registry.json
  registry.lock
  packages/<plugin-id>/<revision-id>/
    golutra-plugin.json
    ... package files
```

目录权限在 Unix 上为 `0700`，registry 和普通文件为 `0600`，可执行入口为 `0700`。stage 会拒绝 symlink、special file、超限文件数/体积和无效 manifest，并对不可变 package 计算 SHA-256。review、enable 和每次 RuntimeHost 加载都会重新校验 checksum 与 manifest。

CLI 生命周期：

```bash
golutra plugin stage ./my-plugin
golutra plugin review <plugin-id> <revision-id>
golutra plugin enable <plugin-id> <revision-id>
golutra plugin list
golutra plugin disable <plugin-id>
golutra plugin rollback <plugin-id>
```

manifest 只保存命令、参数、所需环境变量名称、workspace/network 权限和经过人工审查的工具 schema，不保存 secret：

```json
{
  "schema_version": 1,
  "id": "example",
  "version": "1.0.0",
  "display_name": "Example",
  "description": "Example MCP tools",
  "server": {
    "command": "node",
    "args": ["server.js"],
    "env": ["EXAMPLE_API_TOKEN"]
  },
  "permissions": {
    "workspace_access": "read_only",
    "allow_network": false
  },
  "tools": [{
    "name": "lookup",
    "description": "Lookup an item",
    "input_schema": {"type": "object"},
    "output_schema": {"type": "object"},
    "side_effect_type": "external_system"
  }]
}
```

## MCP 执行边界

模型看到的 MCP 工具名为 `mcp__<plugin-id>__<tool-name>`。调用必须经过以下链路：

```text
provider tool call
-> reviewed ToolContract JSON Schema
-> PolicyDecision::Ask
-> human approval
-> verify enabled revision checksum
-> SystemSandbox launch plan
-> one-shot stdio MCP initialize
-> tools/list 与 reviewed schema 对照
-> tools/call
-> bounded/redacted ToolResultEnvelope + Artifact/Evidence
-> close and reap child process
```

未批准时不会启动 MCP 进程。调用默认 30 秒 timeout，取消会终止 MCP service/child，输出上限为 8 MiB。远端 annotation 不参与权限判断。macOS 使用 Seatbelt，Linux 检测 `bubblewrap`；没有 OS-enforced sandbox 时外部插件执行被拒绝，因此 Windows 当前可以管理插件但不会执行 MCP server。

当前不提供网络 marketplace，也不自动下载依赖。Wasm plugin runtime、签名分发和组织级 registry 属于未来产品能力，不是本地插件主链的兼容要求。

## Transport

| 场景 | Transport | 边界 |
| --- | --- | --- |
| CLI/TUI 默认 | `EmbeddedTransport` | 当前进程持有 RuntimeHost，共享全局 durable facts |
| Unix 本地 daemon | `UnixIpcTransport` | owner-only socket，复用 Axum command/query/SSE Router |
| Windows 本地 daemon | `HttpSseTransport` | loopback + bearer token + protocol version |
| 远端/端口转发 | `HttpSseTransport` | HTTPS 或 loopback HTTP，cursor replay + SSE live |

IPC 不是第二套业务协议。它把受限 HTTP-like request 交给同一个 Router，并以有界 response frame 回传 body/SSE；attachment、认证、status code、event cursor 和错误语义与 HTTP 对拍。

## SDK

`just schema` 从 Rust 类型生成 `schemas/sdk-protocol.schema.json`，再生成：

- `sdk/typescript/src/generated.ts`
- `sdk/python/src/golutra_sdk/generated.py`

两个 SDK 都要求绝对 cwd 和 transport token，先执行 `/runtime/attach`，再访问 command/query/thread/event API；attachment 失效时只在服务端明确返回 `410 Gone` 后重新 attach。JSON response、SSE frame、timeout 和 cursor 去重都有固定上限。

验证命令：

```bash
just schema
just ts-check
just py-check
```

## 安装与升级

Unix 使用 `scripts/install.sh`，Windows 使用 `scripts/install.ps1`。脚本从固定 Rust toolchain 构建 release binary，并安装：

- `golutra`
- `golutra-tui`
- `golutra-app-server`
- `golutra-vis`

升级不会写项目目录。SQLite 使用幂等 migration 和 legacy column backfill；provider v1 明文 env map 只在 provider settings lock 内迁移到 disk SecretRef。rollout 可从 SQLite 重建，未开始 pending turn 可在 owner 重启后恢复，已开始 turn 不做不安全重放。

升级前仍建议停止 daemon 并备份整个 `$GOLUTRA_HOME`。恢复时以 `runtime.sqlite`、credentials/provider 配置和 artifact/checkpoint 文件共同作为一个备份单元，不应只恢复派生 rollout。

## 交付门禁

发布前必须通过：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
just schema
just fixture
just ts-check
just py-check
```

跨进程验收覆盖多 cwd、daemon 重启、HTTP/SSE、Unix IPC、command 幂等、thread fork/rebind 和 durable evaluation；稳定性 smoke 连续执行多轮 turn，验证 event sequence 单调、同一 thread 不分叉并能在 RuntimeHost 重启后恢复。

# Extension、SDK 与交付规格

## 当前状态

截至 2026-07-23，Golutra 的扩展和交付层已经进入可运行主链：

- `golutra-plugin` 管理 owner-only 本地插件包，生命周期为 `stage -> review -> enable -> disable/rollback`。
- `golutra-mcp` 使用官方 `rmcp 2.2.0` 适配 stdio MCP server；外部工具进入统一 `ToolRegistry`、PolicyEvaluation、approval、timeout/cancel、artifact/evidence 链路。
- Unix 本地 daemon 默认通过 owner-only Unix socket 连接；socket 请求复用同一个 Axum Router，HTTP/SSE 继续用于 Windows 和远端。
- TypeScript 与 Python SDK 都从 Rust 协议 schema 生成类型，并实现 cwd attachment、command/query、event replay/live stream、thread 与治理 API。
- Agent 高层 SDK 还提供统一的 `Thread`/`TurnHandle` 生命周期；`exec`、MCP 和 Remote TUI 复用同一个 App Server/Agent event projector，不形成第二套执行状态机。SDK 可发送 actor 元数据，但控制权由 App Server 为 attachment 分配的 server-side actor 决定，不能通过伪造 header 提权。
- 根安装脚本覆盖 Unix 和 Windows；CI 对 Linux/macOS/Windows 执行 workspace all-target compile，并在 Linux 执行完整 Rust/SDK 门禁。

治理 SDK 读取和 evaluator 写入也共用同一条事实链：SDK 只能通过
`taskTrace`/`completeTaskTrace` 读取分页完整 trace，通过
`ingestExternalEvaluation` 提交带 `base_trace_digest`、`runtime_identity`、
`result_digest` 和 trust/attestation 的结果；SDK 不得自行修改
`VerificationRecord`、`RegressionResult` 或 `PromotionDecision`。

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
| CLI/TUI 默认 | `EmbeddedTransport` -> `RuntimeApplication` | 当前进程持有 RuntimeHost，共享全局 durable facts；command/query/session/trace 通过 facade，不直接拼装 store |
| Unix 本地 daemon | `UnixIpcTransport` | owner-only socket，复用 Axum command/query/SSE Router；server-side IPC marker 可访问 forensic trace |
| Windows 本地 daemon | `HttpSseTransport` | loopback + bearer token + protocol version |
| 远端/端口转发 | `HttpSseTransport` | HTTPS 或 loopback HTTP，cursor replay + SSE live；最多 full-redacted trace |
| App Server JSON-RPC | HTTP、WebSocket、stdio；Unix IPC 通过共享 `/rpc` Router | thread/turn 控制和 `agent/event` 增量通知；按 server-issued attachment actor 隔离控制权；复用同一 `RuntimeApplication` |

IPC 不是第二套业务协议。它把受限 HTTP-like request 交给同一个 Router，并以有界 response frame 回传 body/SSE；attachment、认证、status code、event cursor 和错误语义与 HTTP 对拍。

## SDK

`just schema` 从 Rust 类型生成 `schemas/sdk-protocol.schema.json`，再生成：

- `sdk/typescript/src/generated.ts`
- `sdk/python/src/golutra_sdk/generated.py`

两个 HTTP SDK 都要求绝对 cwd 和 transport token，先读取认证后的 `/runtime/info` 验证 runtime protocol range，再执行 `/runtime/attach`，之后访问 command/query/thread/event API；attachment 失效时只在服务端明确返回 `410 Gone` 后重新 attach。JSON response、SSE frame、timeout 和 cursor 去重都有固定上限。

TypeScript 的 `@golutra/agent-sdk/tui-driver` 和 Python 的 `TuiDriverClient` 直接消费 Native TUI Driver。两者都支持启动 stdio 子进程、连接 owner-only Unix socket、并发 `request_id` 路由、notification/diagnostic 分流、请求超时、冻结 frame 聚合和显式 socket reconnect。连接中断会拒绝全部 pending request；prompt/key/paste 等非幂等输入永远不会在重连后自动重放。SDK socket 客户端在连接前还会校验真实 socket、owner UID 和 `0600` 权限。

治理读取使用同一份生成类型：

- `contextProjection(sessionId, taskId)` 返回模型实际输入的脱敏 `ContextSnapshot` 投影。
- `evaluationProjection(sessionId, taskId)` 返回 review/candidate/regression/promotion/job 生命周期。
- `taskTrace(request)` / `task_trace(request)` 按 cursor 返回 `TaskTracePage` 和完整性原因。
- `completeTaskTrace(request)` / `complete_task_trace(request)` bounded 聚合所有 page，校验 session/task/view、cursor 前进和 event-chain digest。
- `readArtifactChunk(request)` 按范围读取带 checksum 的 artifact 内容。

高层 client 还提供：

```text
debugProjection(session_id, task_id)
replay(session_id, task_id, capsule_id?)
ingestExternalEvaluation(session_id, record)
runRegressionCampaign(session_id, candidate_id, candidate_files, matrix)
```

`runRegressionCampaign` 的矩阵按 `case × partition × provider × seed` 解释，
最低可信外部覆盖使用 `minimumTrustedExternalPairs`（Python 为
`minimum_trusted_external_pairs`）。旧的 `...Evaluations` 参数仅作为兼容
别名解析，传输到 runtime 后统一转换为 pair 语义。

`summary` trace 省略 context/artifact/evidence 明细并净化事件 payload；`full` 返回脱敏 manifest；HTTP 客户端请求 `forensic` 会得到 `403 Forbidden`。浏览器 attach 页面同样先读取 runtime info，不再复制 Rust protocol 常量。

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
- `golutra-supervisor`
- `golutra-launcher`
- `golutra-eval-worker`

如果只需要交互式 TUI 或脚本 CLI，可以使用 npm 分发：

```bash
npm install -g @golutra/agent
golutra-tui
```

`@golutra/agent` 是无状态 JavaScript wrapper，平台原生包通过 npm
`optionalDependencies` 安装，wrapper 根据 `process.platform` 和 `process.arch`
选择 binary，并转发 stdio、信号和退出码。它没有 `postinstall` 联网下载步骤；完整的
app-server、观测、supervisor 和 evaluation 入口仍使用下面的 release archive。

`python3 scripts/package_release.py` 使用相同 binary 集合生成版本化 `.tar.gz` 或 `.zip`，并同时生成外置 manifest 与 SHA-256 sidecar。manifest 也内嵌在归档中；归档根目录还包含 `LICENSE` 和 `NOTICE`。`--verify ARCHIVE` 会复验 sidecar、归档路径、文件类型、执行位、文档权限、逐文件 size/hash 和内外 manifest 一致性。归档拒绝 symlink、special file、路径穿越、空 binary、缺失法律文件和未声明文件。`SOURCE_DATE_EPOCH` 可固定时间戳；默认使用当前 Git commit 时间，因此相同输入得到相同归档。

`.github/workflows/release.yml` 是本仓库独立交付面：tag 或手动触发后分别在 Linux、macOS、Windows 构建，tag 构建只有在 `v<workspace-version>` 完全匹配时才发布。它交付 Golutra Agent 的完整 7 个 Rust 入口归档，并额外发布 `@golutra/agent` 的 CLI/TUI npm wrapper 与当前矩阵生成的原生平台包；npm 发布使用 OIDC provenance，不通过 `postinstall` 联网下载。它不包含 `/Applications/Golutra.app` 或任何外部桌面应用。

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
just release-package-smoke
```

跨进程验收覆盖多 cwd、daemon 重启、HTTP/SSE、Unix IPC、command 幂等、thread fork/rebind 和 durable post-task evaluation；稳定性 smoke 连续执行多轮 turn，验证 event sequence 单调、同一 thread 不分叉并能在 RuntimeHost 重启后恢复。runtime terminal fact 先写入 SQLite，active worker 随后建立治理调度屏障并创建 `PostTaskJob`；worker 通过 lease、retry 和 recovery 接管，Embedded one-shot 退出后可由下一 Host/daemon 继续执行。
入口级验收还覆盖 `exec`/`exec resume` 的独立进程、MCP stdio 子进程、WebSocket/stdio JSON-RPC、SDK 高层 handle 和 Remote TUI attach；详见 `runtime-entrypoints.md`。

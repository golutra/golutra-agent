# Agent Runtime 技术选型建议

## 文档定位

本文档基于 `ARCHITECTURE.md` 的架构判断，给出 Golutra agent runtime 的语言、模块和库选型建议。

核心原则：

- 核心 runtime 优先选择稳定、可测试、可分发、可长期维护的技术。
- CLI/TUI/App/SDK 都只是入口，不应重复实现状态机。
- 工具、权限、状态、恢复、预算、trace 和 verification 应集中在 runtime。
- 选型优先满足可恢复、可治理、可验证、可演化，而不是短期拼装速度。
- 主线采用 `cg` 式 Rust native runtime：核心执行、TUI、状态、沙箱、观测和 provider adapter 统一在 Rust 内治理。
- 其他项目只吸收能力，不改变 Rust-first 主线：Kimi Code 的 wire/state/vis、OpenCode 的事件化 session/API、多端共享核心、Pi 的 harness/provider 分层、Claude Code Best 的终端体验、Hermes Agent 的 SQLite/FTS5 和插件 provider。
- 第一阶段按 coding agent 收敛，不按通用 agent 平台做全入口同优先级铺开。

## 总体推荐

### 语言选择

| 层 | 推荐语言 | 结论 |
| --- | --- | --- |
| Runtime Core | Rust | 主语言，承载 query loop、state、tool pipeline、permission、store、verification |
| CLI / TUI | Rust | 与 runtime 同进程或低成本调用，减少入口层复杂度 |
| App Server | Rust | 用统一 runtime 对外暴露 HTTP/WebSocket/SSE |
| SDK / Web UI | TypeScript | 适合前端、插件开发体验和对外类型分发 |
| Python | 可选 | 只做 SDK、实验脚本、第三方生态适配，不做核心 runtime |

首推路线：

```text
Rust core + Rust CLI/TUI/App Server + TypeScript SDK/Web
```

完整推荐组合：

```text
Rust Runtime Kernel
  + crossterm / ratatui TUI
  + SQLite + event log
  + structured tool result
  + tracing / OpenTelemetry / replay / evaluation
  + provider contract + capability matrix
  + sandbox / permission / policy / workspace isolation
```

推荐补充一个稳定的访问层收敛模型：

```text
Frontend
  -> Frontend SDK
  -> RuntimeClient
  -> Transport Adapter
  -> RuntimeHost
  -> RuntimeCore
```

这里的关键不是多做几套 API，而是把不同入口都压到同一套 runtime 语义上。

第一阶段推荐优先级：

- `CLI + TUI + InProcessTransport`
- `app-server + HttpSseTransport`
- `SDK / Web attach`
- `IDE attach`

不建议：

- Python 做核心 agent loop。
- Node/TypeScript 做本地工具权限、状态恢复和 sandbox 核心。
- 一开始引入大而全 agent 框架作为主架构。

## 为什么核心用 Rust

Rust 更适合当前文档里的 agent runtime 目标：

- 类型系统适合定义稳定协议：Message、SessionState、ToolResultEnvelope、TaskRecord。
- 可控并发适合 tool execution、background task、event stream。
- 单 binary 分发更适合本地 CLI/TUI。
- 文件系统、进程、权限和 sandbox 边界更容易集中治理。
- 与 `cg` 项目的 Rust workspace 思路一致，适合按能力拆 crate。

Python 和 TypeScript 的优势主要在生态和开发速度，但它们更适合放在边缘：

- TypeScript：Web UI、SDK、插件类型、配置编辑器。
- Python：用户脚本、数据处理、研究实验、兼容 SDK。

核心状态机不要跨语言分裂。

## Rust Workspace 拆分

推荐先按 runtime 能力拆分，而不是按入口拆分。

```text
golutra-core
golutra-runtime
golutra-event
golutra-context
golutra-tools
golutra-policy
golutra-store
golutra-memory
golutra-llm
golutra-verify
golutra-eval
golutra-otel
golutra-cli
golutra-tui
golutra-vis
golutra-app-server
sdk/typescript
sdk/python
```

### 模块职责

| 模块 | 职责 |
| --- | --- |
| `golutra-core` | Message、SessionState、GoalState、LoopGuard、LoopDecision、ToolResultEnvelope、TaskRecord、Policy 等核心类型 |
| `golutra-runtime` | query loop、turn 状态机、LoopDecision 生成、tool/model 回流、resume/compact 调度 |
| `golutra-event` | ProviderRawEvent、RuntimeEvent、UiSdkEvent、durable/live-only 事件协议 |
| `golutra-context` | ContextBuilder、TokenBudgetTracker、WorkingSummary、CompactManager、history 分层、context projection |
| `golutra-tools` | ToolSchema、ToolAccesses、tool registry、schema validation、tool execution、ToolResultEnvelope |
| `golutra-policy` | PermissionPolicy、`allow/ask/deny`、workspace isolation、路径/网络/命令策略 |
| `golutra-store` | SQLite state、durable event log、artifact store、FTS5、migration |
| `golutra-memory` | MemoryRetriever、MemoryGovernance、项目索引、代码片段召回、memory promotion/rollback |
| `golutra-llm` | ProviderConfig、ModelCatalog、CapabilityMatrix、ModelRouteDecision、adapter、usage 解析 |
| `golutra-verify` | verification runner、PASS/FAIL/PARTIAL、证据记录 |
| `golutra-eval` | eval_runner、trajectory_recorder、post_task_reviewer、vcr/golden fixture |
| `golutra-otel` | tracing、metrics、event export/import |
| `golutra-client` | `RuntimeClient`、`RuntimeQuery`、event subscription、transport abstraction |
| `golutra-cli` | 薄 CLI 入口 |
| `golutra-tui` | TUI 入口，只展示 runtime projection，支持 normal/debug panel |
| `golutra-vis` | 离线 replay、wire/event/context/blob 检查和审计视图 |
| `golutra-app-server` | HTTP/WebSocket/SSE 入口 |
| `sdk/typescript` | Web/插件/外部集成 SDK |
| `sdk/python` | 可选兼容 SDK，不承载核心逻辑 |

### 收敛边界

这些模块不是外部 agent 项目的能力拼盘。所有扩展能力必须归入四个主线系统：

```text
Runtime Loop
Durable State
Context & Memory
Governance
```

模块落地时遵守以下边界：

- `golutra-runtime` 只产生 loop 状态和 `LoopDecision`，不直接拥有长期 memory。
- `golutra-context` 只负责模型可见输入投影、token 预算和 compaction，不把完整 transcript 当 prompt 回灌。
- `golutra-memory` 只负责可解释召回和长期 memory 晋升治理，不直接改写当前任务状态。
- `golutra-store` 保存 raw event、artifact 和 projection，避免 UI、provider adapter 或 tool 层维护自己的任务真相。
- `golutra-eval` 和 `golutra-verify` 基于 durable event/replay 做验证，不另建一套不可回放的评估输入。
- `golutra-client` 只暴露统一 runtime 语义，不携带前端私有状态机。

判断一个新能力是否进入主架构时，必须回答：它产生什么 runtime fact、改变什么 state projection、是否影响 context projection、是否参与 LoopDecision 或 PromotionGate。如果回答不清楚，就先作为插件或实验能力，不进入核心。

## RuntimeClient 与 Transport

Golutra 不应为 TUI、Web、IDE、SDK、API 各做一套独立接口。更合理的是保留一套统一客户端语义：

```text
RuntimeCommand
RuntimeQuery
RuntimeEvent Subscription
```

Rust 内部可以收敛成类似下面的接口：

```rust
trait RuntimeClient {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck>;
    async fn query(&self, query: RuntimeQuery) -> Result<QueryResult>;
    async fn subscribe(&self, filter: EventFilter) -> Result<EventStream>;
}
```

推荐 transport 分层：

- `InProcessTransport`：TUI / CLI 第一阶段首选。和 `RuntimeCore` 同进程，最简单、最省资源。
- `HttpSseTransport`：Web / SDK / daemon 模式第一阶段首选。HTTP 发 command 和 query，SSE 接 event stream。
- `WebSocketTransport`：后续用于高频双向同步或远程协作，不作为第一阶段必做。
- `IpcTransport`：后续用于 IDE companion、本地桌面端或侧车进程。

关键判断：

- transport 可以不同，但 `SessionCommand`、`RuntimeQuery`、`RuntimeEvent` 语义必须完全一致。
- 一个 task 在 SDK 中运行时，TUI / Web attach 后应通过同一 client 语义查询到相同状态，并订阅到同一条流式输出。
- daemon 只是 `RuntimeHost + HttpSseTransport` 的组合，不是新的任务接口体系。

对于 coding agent，推荐再加两条默认约束：

- 一个 `session` 同时只允许一个 `active task`，避免工具副作用和工作区状态冲突。
- 多前端 attach 时默认采用 `one active controller + many observers`，而不是多 controller 并发写入。

## 核心库推荐

### CLI / TUI

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| CLI 参数 | `clap` | `chat/resume/summary/usage/compact/trace/manifest` 命令 |
| TUI | `ratatui` + `crossterm` | 交互式终端、状态卡片、工具进度、历史渲染 |
| 错误展示 | `miette` 或 `color-eyre` | 面向 CLI/TUI 的可读错误 |

CLI 层要保持薄，不要在命令 handler 里拼 prompt 或裁剪历史。

TUI 选型明确采用 `cg` 式路线：底层终端控制交给 `crossterm`，布局和绘制交给 `ratatui`，上层只自建 Golutra 业务组件层。不要从零实现 terminal renderer，也不要把 React/Ink/OpenTUI 放进核心 CLI TUI。

推荐 TUI 组件：

```text
AppShell
Transcript
Composer
ToolCard
PermissionPrompt
DiffPreview
EvidencePanel
VerificationPanel
DebugAuditPanel
ReplayPanel
```

这些组件只消费 runtime event 和 state projection，不直接拥有任务状态，不直接执行工具。

### 异步与服务层

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 异步 runtime | `tokio` | LLM streaming、tool task、background task、HTTP server |
| Web/App Server | `axum` | 本地 API、SSE/WebSocket、桌面或 Web 前端入口 |
| Middleware 抽象 | `tower` | permission、hooks、telemetry、retry、rate limit |

`tower` 的 service/middleware 思路很适合 tool pipeline：

```text
schema validation -> pre hook -> permission -> sandbox -> execute -> post hook -> envelope
```

### 协议、Schema 与错误

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 序列化 | `serde`、`serde_json` | 所有 message/state/tool result 均结构化 |
| Schema 生成 | `schemars` | 从 Rust 类型生成 JSON Schema |
| Schema 校验 | `jsonschema` | tool input/output、配置、插件声明校验 |
| 类型错误 | `thiserror` | library 层错误类型 |
| 应用错误 | `anyhow` / `miette` | binary 层聚合错误 |

建议所有对外协议都 schema-first，避免 SDK 和 runtime 类型漂移。

### LLM Provider

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| HTTP client | `reqwest` | OpenAI/Anthropic/OpenAI-compatible provider 调用 |
| Streaming | `tokio` + `reqwest` stream | token 流、tool call 流、usage 事件 |
| Provider abstraction | 自研 trait | 不把 runtime 绑死到某个 vendor SDK |
| 多 provider 加速接入 | `genai` / `rust-genai` | 作为 `GenaiProviderAdapter` 使用，不进入核心协议 |

建议核心用 provider abstraction：

```rust
trait LlmProvider {
    async fn stream(&self, request: LlmRequest) -> Result<LlmStream>;
}
```

原因：

- OpenAI-compatible provider 很多，字段和错误形态不完全一致。
- 不同模型对 tool call、usage、reasoning token 的返回差异很大。
- provider SDK 可以做 adapter，但不要进入 runtime core。

Provider 层不直接照搬某个项目：

- 采用 `cg` 的 Responses-first / OpenAI-compatible 统一性，保证主链路简单。
- 吸收 Pi 的 provider registry，保证 adapter 可插拔。
- 吸收 OpenCode 的 provider 覆盖意识，但不把 AI SDK 作为 Rust core 依赖。
- 吸收 Hermes Agent 的 plugin discovery，但插件必须经过 capability matrix 和 policy gate。
- 吸收 Claude Code Best 对 Anthropic 能力的深集成经验，但不能让 runtime 绑定单一 provider。

`rust-genai` 的定位是接入加速器，不是 Golutra 的核心 Provider 协议。推荐采用一层反腐适配：

```text
Golutra ProviderContract
  -> GenaiProviderAdapter
       -> genai::Client
```

这样可以先获得 `rust-genai` 对 OpenAI、OpenAI Responses、Anthropic、Gemini、Ollama、OpenRouter、Groq、DeepSeek、xAI、Bedrock、Vertex、Moonshot、Aliyun 等 provider 的覆盖，同时保留 Golutra 自己的 request、stream event、tool call、usage、error、capability 和 telemetry 标准。

不建议让 runtime core 直接使用 `genai::ChatRequest`、`genai::ChatResponse` 或其内部事件作为核心数据模型。`GenaiProviderAdapter` 必须完成：

- `LlmRequest -> genai request` 的转换。
- `genai stream/event -> Golutra LlmStreamEvent` 的归一化。
- tool call id、tool arguments、finish reason、usage、reasoning、error 的标准化。
- provider 原始字段写入 `provider_metadata` 和 artifact ref，供 debug/replay 使用。
- 根据 `rust-genai` 支持范围补全 Capability Matrix，并对不确定能力标记为 unknown。
- 对关键 provider 建 recorded/golden tests，避免升级 `genai` 后破坏回放和评估。

完整 ProviderContract 至少要记录：

```text
provider_id
auth_mode
native_protocol
model_capabilities
tool_call_format
stream_event_format
reasoning_support
vision_audio_support
context_window
max_output
usage_normalization
cost_model
rate_limit
retry_policy
fallback_policy
privacy_policy
provider_quirks
```

### 持久化与本地数据

| 能力 | 推荐库 | 建议 |
| --- | --- | --- |
| 主存储 | SQLite + `sqlx` 或 `rusqlite` | 用于 session/thread metadata、message index、tool call index、permission、verification、artifact index |
| 事件轨迹 | append-only event log / JSONL | 用于 raw turn event、model envelope、tool summary、replay timeline、benchmark fixture |
| 全文检索 | SQLite FTS5 | 用于跨会话搜索、历史浏览、证据和 artifact 检索 |
| 轻量 KV | `redb` | 可选，用于 cache 或小型本地索引 |
| migration | `refinery` 或自研 SQL migration | 保持 store 可升级 |

推荐采用 SQLite + event log 的双层存储，而不是只用 JSONL 或只用数据库。

理由：

- SQLite 适合查询、索引、跨会话检索、权限审计和 UI 列表。
- append-only event log 适合 replay、benchmark、trace diff 和失败复现。
- FTS5 可以吸收 Hermes Agent 的跨会话搜索优势，但不需要把核心 runtime 改成 Python 平台。
- Kimi Code 的 `wire/state` 分离说明，运行轨迹和当前状态必须分开保存。

对于 coding agent，建议 task 级查询和回放优先：

- `query(task_id)` 优先于全文 transcript 重放。
- `replay(task_id)` 优先于 session 级模糊恢复。
- artifact 应重点保存 `diff`、测试结果、命令输出、诊断日志和关键工具原始输出。

### Memory 与代码理解

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 全文检索 | `tantivy` | 本地 docs/code/memory 搜索 |
| 代码解析 | `tree-sitter` | 代码符号、函数范围、片段召回 |
| 文件遍历 | `ignore`、`walkdir` | 遵守 `.gitignore`，避免扫无关目录 |
| glob 匹配 | `globset` | workspace policy、工具路径匹配 |
| 路径处理 | `camino` | UTF-8 path，减少跨平台坑 |

Memory 优先使用可解释、可回放的检索链路：

1. 项目文件索引
2. 文本检索
3. 代码片段定位
4. working summary / memory fact 检索

向量检索可以作为增强能力，但不应成为 session 恢复、审计、replay、benchmark 的基础依赖。

### 权限、Sandbox 与插件

| 能力 | 推荐库 / 方案 | 建议 |
| --- | --- | --- |
| 权限决策 | 自研 policy engine | `allow/ask/deny` 必须可解释 |
| 路径隔离 | `camino` + canonicalize + policy matcher | 防路径穿越、symlink 逃逸 |
| 进程执行 | `tokio::process` | 统一封装 stdout/stderr/exit code |
| 插件 sandbox | `wasmtime` | 用于高风险插件隔离和可分发插件运行时 |
| MCP | 官方 Rust SDK `rmcp` | 放在 adapter 层，不进入 core |

Sandbox 和权限至少要覆盖：

- read-only / shared / temp / worktree workspace 类型
- destructive 操作识别
- path allow/deny
- shell command approval

### Trace、Telemetry 与 Verification

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 结构化日志 | `tracing` | runtime event、tool event、decision event |
| 日志订阅 | `tracing-subscriber` | CLI/TUI/App Server 不同输出 |
| 指标导出 | OpenTelemetry | 外部 trace/log/metrics 映射 |
| Snapshot/Golden test | `insta` | message、tool envelope、trace 输出测试 |
| 临时目录 | `tempfile` | tool/sandbox/store 测试 |
| Mock HTTP | `wiremock` 或 `httpmock` | LLM provider 测试 |

Verification 需要单独建模：

```text
check title
command
exit code
key output
evidence path
PASS / FAIL / PARTIAL
verdict
```

建议在 `RuntimeEvent` 上预留与外部观测平台兼容的字段，但仅作为 runtime 自己的投影输入，不作为产品核心：

```text
trace_id
span_id
parent_span_id
latency
token_usage
cost
provider
model
tool
```

这样可以把 LangSmith / Braintrust / Promptfoo 的一些工程能力吸收到 Golutra 里：

- trace view 变成 `DebugProjection`
- dataset / scorer 变成 `EvaluationProjection`
- red team case 变成 `PolicyEvaluation` 和 `VerificationRecord`
- regression 对比变成 `golutra-eval`

但不要把外部平台的 UI 或实验管理逻辑直接搬进 runtime core。

## 完整目标技术栈

推荐基础栈：

```text
Rust workspace
tokio
clap
serde / serde_json
schemars / jsonschema
reqwest
rusqlite
tracing / tracing-subscriber
thiserror / anyhow
ignore / walkdir / globset / camino
ratatui / crossterm
axum
SQLx or rusqlite
OpenTelemetry
tantivy or SQLite FTS5
tree-sitter
rmcp
wasmtime
TypeScript SDK
```

这些能力都属于目标架构，不是临时插件。实现顺序可以分阶段，但架构文档不应把它们写成可有可无的 MVP 外围能力。

不建议作为核心主线：

- React/Ink 核心 TUI
- OpenTUI/Solid 核心 TUI
- Python 核心 agent loop
- Node/TypeScript 核心执行内核
- LangChain 类大框架作为 runtime core
- 外部向量数据库作为基础依赖

## 完整能力分层

工程上可以按依赖关系落地，但目标能力应一次性设计完整：

```text
Runtime Kernel
  Rust / Tokio / state / event log / sandbox / permission / policy

Entry
  clap CLI / ratatui TUI / axum App Server / SDK

Storage
  SQLite metadata + FTS5 + event log + artifact store + migration

Provider
  ProviderContract + Capability Matrix + Routing Policy + adapters

Tools
  schema validation + permission + sandbox + execution + ToolResultEnvelope

Observability
  tracing + OpenTelemetry + replay + verification + evaluation harness

Extension
  MCP adapter + plugin contract + optional Wasm sandbox + TypeScript/Python SDK
```

## 不推荐的选型

### 不推荐 Python 核心

原因：

- 工具执行、权限、路径隔离、并发和二进制分发更难长期治理。
- 类型边界不如 Rust 稳定，message/state/schema 容易漂移。
- 适合实验和 SDK，不适合核心 runtime。

### 不推荐 Node 核心

原因：

- 本地文件、进程、sandbox、权限策略更容易分散。
- 长期运行的本地 agent runtime 更需要强边界。
- TypeScript 更适合 SDK/Web，而不是底层执行内核。

### 不推荐 LangChain 类框架做主架构

可以参考，不建议成为 core dependency。

原因：

- 当前文档强调的是 runtime state、permission、trace、compact、verification。
- 大框架容易把核心控制权藏进抽象里。
- Golutra 应该拥有自己的 message model 和 tool pipeline。

### 不推荐把向量数据库作为基础依赖

原因：

- 本地全文检索、tree-sitter 结构切片、working summary 已经能覆盖大量场景。
- 向量检索可以作为增强能力，但不应该成为 session 恢复、审计、replay、benchmark 的基础。
- 如果引入向量检索，原始 evidence、artifact、decision 和 verification 仍必须落在结构化 store 中。

## 推荐工程落地顺序

工程落地以 `implementation-blueprint.md` 的第一阶段为准。下面是技术模块顺序：

1. 建 `golutra-core`：定义 Message、SessionState、GoalState、LoopGuard、LoopDecision、TaskRecord、ArtifactRef、DecisionRecord、EvidenceRecord。
2. 建 `golutra-store`：SQLite metadata、event log、artifact store、migration、FTS5。
3. 建 `golutra-event`：ProviderRawEvent、RuntimeEvent、UiSdkEvent，明确 durable 与 live-only。
4. 建 `golutra-llm`：ProviderConfig、ModelCatalog、CapabilityMatrix、GenaiProviderAdapter、ModelRouteDecision、FallbackPolicy。
5. 建 `golutra-tools`：ToolSchema、ToolAccesses、ToolResultEnvelope、tool registry。
6. 建 `golutra-policy`：PermissionPolicy、permission `allow/ask/deny`、workspace isolation、sandbox policy。
7. 建 `golutra-runtime`：turn flow、LoopGuard、LoopDecision 生成、recorded events、resume、compact、replay、tool/model 回流。
8. 建 `golutra-context`：working summary、history 分层、compact boundary、token budget。
9. 建 `golutra-verify`：验证结果结构化。
10. 建 `golutra-client`：`RuntimeClient`、`InProcessTransport`、query / subscribe 语义。
11. 建 `golutra-cli`：薄 CLI 命令面。
12. 建 `golutra-tui`：`crossterm + ratatui + Golutra 业务组件`，通过 `InProcessTransport` 访问 runtime。
13. 建 `golutra-app-server`：HTTP/WebSocket/SSE 入口，先实现 `HttpSseTransport`，复用同一 runtime facts。
14. 建 `golutra-otel`：tracing、OTel adapter、debug/replay/audit 查询。
15. 建 `golutra-vis`：离线 replay、event/wire/context/artifact 检查。
16. 建 `golutra-eval`：eval_runner、trajectory_recorder、deep post_task_reviewer、vcr/golden fixture。
17. 建 memory index、MCP ToolEntry bridge、plugin capability package、TypeScript/Python SDK 和 Web/IDE 集成。

## 参考链接

- Tokio: https://tokio.rs/
- clap: https://docs.rs/clap/
- Serde: https://serde.rs/
- reqwest: https://docs.rs/reqwest/
- tracing: https://docs.rs/tracing/
- tower: https://tower-rs.github.io/tower/
- Ratatui: https://ratatui.rs/
- crossterm: https://docs.rs/crossterm/
- rusqlite: https://docs.rs/rusqlite/
- SQLx: https://sqlx.dev/
- tree-sitter: https://tree-sitter.github.io/tree-sitter/
- Tantivy: https://tantivy-search.github.io/
- Wasmtime: https://docs.wasmtime.dev/
- Model Context Protocol: https://modelcontextprotocol.io/

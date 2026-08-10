# Agent Runtime 技术选型建议

## 文档定位

本文档基于 `ARCHITECTURE.md` 的架构判断，给出 Golutra agent runtime 的语言、模块和库选型建议。

核心原则：

- 核心 runtime 优先选择稳定、可测试、可分发、可长期维护的技术。
- CLI/TUI/App/SDK 都只是入口，不应重复实现状态机。
- 工具、权限、状态、恢复、预算、trace 和 verification 应集中在 runtime。
- 选型优先满足可恢复、可治理、可验证、可演化，而不是短期拼装速度。
- 主线采用 `cg` 式 Rust native runtime：核心执行、TUI、状态、沙箱、观测和 provider adapter 统一在 Rust 内治理。
- 其他项目只吸收能力，不改变 Rust-first 主线：Kimi Code 的 wire/state/vis、OpenCode 的事件化 session/API、多端共享核心、Pi 的 harness/provider 分层、Claude Code Best 的终端体验、Hermes Agent 的 SQLite 和插件 provider。
- 第一阶段按 coding agent 收敛，不按通用 agent 平台做全入口同优先级铺开。

## 总体推荐

### 语言选择

| 层 | 推荐语言 | 结论 |
| --- | --- | --- |
| Runtime Core | Rust | 主语言，承载 query loop、state、tool pipeline、permission、store、verification |
| CLI / TUI | Rust | 与 runtime 同进程或低成本调用，减少入口层复杂度 |
| App Server | Rust | 用统一 runtime 对外暴露 Unix IPC 与 HTTP/SSE |
| TypeScript SDK / Web UI | TypeScript | 适合前端、插件开发体验和对外类型分发 |
| Python SDK | Python 3.11+ | schema 生成的兼容 SDK、实验脚本和第三方生态适配，不做核心 runtime |

首推路线：

```text
Rust core + Rust CLI/TUI/App Server + TypeScript/Python SDK + Web attach
```

完整推荐组合：

```text
Rust Runtime Kernel
  + crossterm / ratatui TUI
  + SQLite + event log
  + structured tool result
  + tracing / replay / evaluation
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

- `CLI + TUI + EmbeddedTransport`
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
golutra-protocol
golutra-protocol-fixtures
golutra-context
golutra-tools
golutra-policy
golutra-sandbox
golutra-store
golutra-memory
golutra-file-search
golutra-code-intelligence
golutra-llm
golutra-verify
golutra-eval
golutra-evolution
golutra-plugin
golutra-mcp
golutra-test-client
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
| `golutra-core` | Message、SessionState、GoalState、RuntimeLane、BusyPolicyDecision、LoopGuard、LoopDecision、ToolResultEnvelope、TaskRecord、Policy 等核心类型 |
| `golutra-runtime` | query loop、RuntimeLane、turn 状态机、LoopGuard、LoopDecision 生成、tool/model 回流、resume/compact 调度 |
| `golutra-protocol` | `SessionCommand`、`RuntimeQuery`、`RuntimeEvent`、app-server transport contract、SDK 共享类型 |
| `golutra-protocol-fixtures` | schema 产物、协议 fixture、跨语言契约测试输入 |
| `golutra-context` | ContextBuilder、TokenBudgetTracker、WorkingSummary、CompactManager、history 分层、context projection |
| `golutra-tools` | ToolSchema、ToolAccesses、tool registry、schema validation、tool execution、ToolResultEnvelope |
| `golutra-policy` | PermissionPolicy、`allow/ask/deny`、workspace isolation、路径/网络/命令策略 |
| `golutra-sandbox` | macOS Seatbelt、Linux bubblewrap、process-only fallback 与受控 launch environment |
| `golutra-store` | SQLite state、durable event log、artifact store、workspace checkpoint ref、migration |
| `golutra-memory` | MemoryRetriever、MemoryGovernance、项目索引、代码片段召回、memory promotion/rollback |
| `golutra-file-search` | ignore-aware 文件枚举、rg 搜索和 SQLite metadata |
| `golutra-code-intelligence` | tree-sitter symbol/reference/import graph 与 owner-only code index |
| `golutra-llm` | ProviderConfig、ModelCatalog、CapabilityMatrix、ModelRouteDecision、adapter、usage 解析 |
| `golutra-verify` | verification runner、PASS/FAIL/PARTIAL、证据记录 |
| `golutra-eval` | eval_runner、trajectory_recorder、post_task_reviewer、vcr/golden fixture |
| `golutra-evolution` | GeneratedTask curriculum/frontier、隔离执行和 Skill 生命周期 |
| `golutra-plugin` | reviewed plugin package、checksum、enable/disable/rollback |
| `golutra-mcp` | 官方 rmcp stdio client、schema 对照、sandbox 与 ToolRegistry bridge |
| `golutra-test-client` | app-server 协议 smoke、transport 对拍、fixture replay、SDK 集成验证 |
| `golutra-client` | `RuntimeClient`、`RuntimeQuery`、event subscription、transport abstraction |
| `golutra-cli` | 薄 CLI 入口 |
| `golutra-tui` | TUI 入口，只展示 runtime projection，支持 normal/debug panel |
| `golutra-vis` | replay、audit、event 和 OpenTelemetry JSON 投影 |
| `golutra-app-server` | Unix IPC 与 HTTP/SSE 入口，共用同一个 Axum Router |
| `sdk/typescript` | Web/插件/外部集成 SDK |
| `sdk/python` | schema 生成的 Python SDK，不承载核心逻辑 |

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
- `golutra-runtime` 负责 `RuntimeLane` 和 busy policy；CLI/TUI/SDK 不能各自实现排队、注入或中断。
- `golutra-context` 只负责模型可见输入投影、token 预算和 compaction，不把完整 transcript 当 prompt 回灌。
- `golutra-memory` 只负责可解释召回和长期 memory 晋升治理，不直接改写当前任务状态。
- `golutra-store` 保存 raw event、artifact 和 projection，避免 UI、provider adapter 或 tool 层维护自己的任务真相。
- `golutra-eval` 和 `golutra-verify` 基于 durable event/replay 做验证，不另建一套不可回放的评估输入。
- `golutra-client` 只暴露统一 runtime 语义，不携带前端私有状态机。
- `golutra-protocol` 负责跨 crate、跨 transport、跨语言的契约定义；协议升级必须先过 fixture 和 SDK 契约测试。
- `golutra-event` 仅作为旧 workspace 依赖的兼容 re-export 保留；新实现不得把事件类型放回独立协议副本。
- `golutra-test-client` 不承载业务逻辑，只用于 app-server、SDK、transport 和 schema 对拍。

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

- `EmbeddedTransport`：TUI / CLI 默认入口。和 `RuntimeHost / RuntimeCore` 同进程，但连接 `$GOLUTRA_HOME/state/runtime.sqlite`，不是临时 store。
- `UnixIpcTransport`：Unix 本地 `--daemon` 默认入口，通过 owner-only socket 发送受限 HTTP-like frame，直接复用 app-server Router；不会形成第二套业务 API。
- `HttpSseTransport`：Windows 本地 daemon、Web、TypeScript/Python SDK 和显式 remote 模式入口。先用 `/runtime/attach` 绑定 canonical cwd，HTTP 发 command/query，SSE 接 event stream。

关键判断：

- transport 可以不同，但 `SessionCommand`、`RuntimeQuery`、`RuntimeEvent` 语义必须完全一致。
- 一个 task 在 SDK 中运行时，TUI / Web attach 后应通过同一 client 语义查询到相同状态，并订阅到同一条流式输出。
- daemon 只是用户级 `cwd -> RuntimeHost` registry + IPC/HTTP transport，Unix IPC 与 HTTP/SSE 共用认证、协议版本、attachment 和 cursor 语义；它不是新的任务接口体系，也不是每个 workspace 一个进程。
- `EmbeddedTransport` 不能退化成 `sqlite::memory:`；它必须持有完整 `RuntimeHost` 并连接全局 durable store。不同 Embedded 进程共享历史，但 live task handle 只属于持有 session lease 的进程。
- `subscribe` 的目标形态是 `cursor replay + live stream`。一次性返回历史事件只适合 smoke，不足以支撑 TUI、Web 或 SDK 观察运行中任务。
- TUI 的复杂度应限制在输入、布局、渲染和 debug panel；任务排队、运行中输入、abort、approval、provider/tool loop 都归 `RuntimeHost`。

对于 coding agent，推荐再加两条默认约束：

- 一个 `session` 同时只允许一个 `active task`，避免工具副作用和工作区状态冲突。
- 多前端 attach 时默认采用 `one active controller + many observers`，而不是多 controller 并发写入。
- active task 运行中收到新输入时，必须通过 `RuntimeLane` 统一裁决为 `append`、`inject`、`interrupt` 或 `reject`。
- `inject` 只能在 provider call 前或工具安全间隙发生，不能打断正在执行的副作用。

## 核心库推荐

### CLI / TUI

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| CLI 参数 | `clap` | `chat/resume/summary/usage/compact/trace/manifest` 命令 |
| TUI | `ratatui` + `crossterm` | 交互式终端、状态卡片、工具进度、历史渲染 |
| 错误展示 | `miette` | 面向 CLI/TUI 的可读错误 |

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
| Web/App Server | `axum` | Unix IPC 复用 Router、本地/远端 HTTP API 与 SSE event stream |
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
| TypeScript 类型产物 | `ts-rs` | 从 Rust 协议类型生成 TS 类型，减少 SDK 与 runtime 漂移 |
| Schema 校验 | `jsonschema` | tool input/output、配置、插件声明校验 |
| 类型错误 | `thiserror` | library 层错误类型 |
| 应用错误 | `miette` | binary 层聚合错误与用户可读诊断 |

建议所有对外协议都 schema-first，避免 SDK 和 runtime 类型漂移。

协议层再补两条：

- app-server contract 要有 fixture 产物，供 Rust、TypeScript、Python SDK 共用。
- 协议升级要先跑 `golutra-test-client` 和 SDK 契约测试，避免 runtime 与 SDK 版本语义偏移。

### LLM Provider

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| HTTP client | `reqwest` | runtime 自有 HTTP 能力、测试和非 provider 网络调用 |
| Streaming | `tokio` | LLM stream、tool task、background task |
| Provider abstraction | 自研 trait | 不把 runtime 绑死到某个 vendor SDK |
| LLM provider 调用 | `reqwest` + `eventsource-stream` + `genai` | OpenAI-compatible/Responses 使用受控 HTTP adapter，native 协议差异交给 `genai` |

建议核心用 provider abstraction：

```rust
trait LlmProvider {
    async fn stream(&self, request: LlmRequest) -> Result<LlmStream>;
}
```

原因：

- provider 调用需要方便接入多家模型，第一阶段不手写多套 provider 协议实现。
- 不同模型对 tool call、usage、reasoning token 的返回差异很大，必须在 Golutra contract 层归一化。
- `genai` 可以作为 provider 调用层，但不要让它的类型进入 runtime core。

Provider 层不直接照搬某个项目：

- 吸收 Maka 的分层方式：底层用统一 provider runtime，上层用自己的 contract 反腐归一化。
- 吸收 OpenCode 的 provider 覆盖意识，但不把 AI SDK 或第三方 SDK 类型作为 Rust core 依赖。
- 吸收 Pi 的 provider contract 思路，但不采用 Pi 式多协议自研矩阵。
- 吸收 Hermes Agent 的 plugin discovery，但插件必须经过 capability matrix 和 policy gate。
- 吸收 Claude Code Best 对 provider 深能力的经验，但不为单一 provider 单独维护手写 adapter 类型。

`genai` 的定位是 Golutra 长期默认 LLM provider adapter，不是 Golutra 的核心 Provider 协议。推荐采用一层反腐适配：

```text
Golutra ProviderContract
 -> GenaiProviderAdapter
       -> genai::Client
```

这样可以获得 `genai` 对 OpenAI、OpenAI Responses、Anthropic、Gemini、Ollama、OpenRouter、Groq、DeepSeek、xAI、Bedrock、Vertex、Moonshot、Aliyun 等 provider 的覆盖，同时保留 Golutra 自己的 request、stream event、tool call、usage、error、capability 和 telemetry 标准。

Golutra 保留独立 `OpenAiCompatibleProvider` 和 ChatGPT OAuth 专用 `OpenAiResponsesProvider`，并用一个 `GenaiProviderAdapter` 承担 Anthropic、Gemini、Vertex 和 model-routed genai。Responses adapter 的存在理由是 subscription token、account header、SSE 和 encrypted reasoning replay 不能由 Chat Completions 兼容层正确表达；不会再并行维护 `AnthropicNativeAdapter`、`GeminiNativeAdapter` 等手写类型。各家 native wire 差异统一交给固定版本的 `genai` 处理，并由协议 golden tests 锁定升级影响。

`genai` 只对应 Maka / Vercel AI SDK 的 provider client 层，不承担完整 `streamText` / tool loop / UI stream / agent framework 职责。Golutra 的 agent loop、tool execution、permission、verification、fallback、replay 和 UI projection 仍由 runtime 自己掌控。

不建议让 runtime core 直接使用 `genai` 的请求/响应或其内部事件作为核心数据模型。`GenaiProviderAdapter` 必须完成：

- `LlmRequest -> genai request` 的转换。
- `genai stream/event -> Golutra LlmStreamEvent` 的归一化。
- tool call id、tool arguments、finish reason、usage、reasoning、error 的标准化。
- provider 原始字段写入 `provider_metadata` 和 artifact ref，供 debug/replay 使用。
- 根据 `genai` 支持范围补全 Capability Matrix，并对不确定能力标记为 unknown。
- 对关键 provider 建 recorded/golden tests，避免升级 `genai` 后破坏回放和评估。

### Custom Provider 设置与验证

Custom Provider 的交互流程要对齐 qwen-code 的协议优先设计，不能只做一个 OpenAI-compatible 表单。推荐流程：

```text
Step 1/6 Protocol
  OpenAI-compatible
  Anthropic
  Gemini
  Vertex AI
  genai model router
Step 2/6 Base URL
Step 3/6 API Key
Step 4/6 Model IDs
Step 5/6 Advanced Config
Step 6/6 Review
```

验证边界：

- Step 1 必须先确定协议，因为 base URL 默认值、env key、模型格式、tool call 映射和后续连通性 probe 都依赖协议。
- OpenAI-compatible 默认走 Chat Completions 兼容路径，base URL 可规范化到 `/v1`。
- Anthropic、Gemini、Vertex AI 和 genai 已进入 UI 协议选择，并通过同一个 `GenaiProviderAdapter` 路由到各自 native wire。
- 所有 live profile 都必须在保存前通过实际 adapter probe；认证、endpoint 或模型错误不能显示为连接成功。
- Review 阶段必须展示 protocol、base URL、model、API key 掩码、scope 和 config path。
- provider 配置校验必须按 protocol 检查必填字段，不能只校验 OpenAI-compatible。
- adapter 仍要通过 Golutra 的 `ProviderContract` 归一化 request、stream event、usage、tool call、finish reason 和 error；OpenAI-compatible streaming 已按该边界进入 runtime，其他协议即使由 genai capture 也不能泄漏 native type。

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
| 主存储 | SQLite + `sqlx` | 用于 session/thread metadata、message index、tool call index、permission、verification、artifact index |
| 事件轨迹 | append-only event log / JSONL | 用于 raw turn event、model envelope、tool summary、replay timeline、benchmark fixture |
| 全文检索 | `rg` | 用于跨会话文件内容搜索、历史浏览、证据和 artifact 定位 |
| 轻量 KV | `redb` | 可选，用于 cache 或小型本地索引 |
| migration | `refinery` 或自研 SQL migration | 保持 store 可升级 |

推荐采用 SQLite + event log 的双层存储，而不是只用 JSONL 或只用数据库。

理由：

- SQLite 适合查询、索引、跨会话检索、权限审计和 UI 列表。
- append-only event log 适合 replay、benchmark、trace diff 和失败复现。
- 可吸收 Hermes Agent 的跨会话搜索目标，但第一阶段不引入额外全文索引层。
- Kimi Code 的 `wire/state` 分离说明，运行轨迹和当前状态必须分开保存。

对于 coding agent，建议 task 级查询和回放优先：

- `query(task_id)` 优先于全文 transcript 重放。
- `replay(task_id)` 优先于 session 级模糊恢复。
- artifact 应重点保存 `diff`、测试结果、命令输出、诊断日志和关键工具原始输出。

额外建议增加两个持久化边界：

- `thread/session metadata` 与 `runtime state/event index` 可以共库，但逻辑上要分表和访问层，避免 thread 浏览与 runtime 真相耦合。
- `artifact blob` 默认走文件系统或对象目录；SQLite 只保留索引、checksum、大小和类型，不直接吞大对象正文。

### Memory 与代码理解

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 全文检索 | `rg` | 本地 docs/code/memory 搜索 |
| 代码解析 | `tree-sitter` | 代码符号、函数范围、片段召回 |
| 文件遍历 | `ignore` | 遵守 `.gitignore`，避免扫无关目录 |
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
| 路径隔离 | `PathBuf` + canonicalize + policy matcher | 防路径穿越、symlink 逃逸 |
| 进程执行 | `tokio::process` | 统一封装 stdout/stderr/exit code |
| OS sandbox | `golutra-sandbox` | macOS Seatbelt、Linux bubblewrap；未检测到 OS sandbox 时外部插件拒绝执行 |
| MCP | 官方 Rust SDK `rmcp 2.2.0` | 一次性 stdio client 放在 adapter 层，不进入 core |

平台边界建议明确：

- macOS 已使用 Seatbelt，Linux 在存在 bubblewrap 时使用 mount/network namespace；两者都以 workspace access、scratch 和 allow_network 生成 launch plan。
- Windows 当前保留 process policy、timeout/cancel 和插件 package 管理，但没有可声明为 OS-enforced 的 MCP sandbox，所以外部插件执行会明确拒绝。

Sandbox 和权限至少要覆盖：

- workspace read-only/read-write、独立 scratch 与默认断网
- destructive 操作识别、结构化 argv 与 shell metacharacter guard
- canonical path allow/deny、symlink 和内部目录边界
- shell/MCP command approval、timeout、cancel 和 process-group 回收

Wasm plugin runtime、签名分发和 marketplace 是独立产品方向，不作为当前 MCP plugin 主链的未完成兼容层。

### Trace、Telemetry 与 Verification

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 结构化日志 | `tracing` | runtime event、tool event、decision event |
| 日志订阅 | `tracing-subscriber` | CLI/TUI/App Server 不同输出 |
| OpenTelemetry 投影 | `golutra-vis` | 从 durable RuntimeEvent 生成脱敏 trace/span JSON；不引入第二份观测真相 |
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

### 构建、分发与安装

除了 runtime 本身，Golutra 还需要明确工程与分发层选型：

| 能力 | 推荐方案 | 说明 |
| --- | --- | --- |
| Rust workspace 构建 | `cargo` | 第一阶段主构建入口 |
| 开发任务编排 | `just` | 统一 `fmt`、`test`、`run`、`replay`、`schema`、`sdk` 等命令 |
| Node/TS 包管理 | `pnpm` | 管 SDK/Web 依赖，不进入 runtime core |
| Python SDK 构建 | `uv` 或标准 `pyproject` | 仅用于 SDK/脚本，不进入核心运行时 |

第一阶段不建议引入 Bazel/Nix 这类更重工程体系。等 monorepo、跨平台构建矩阵和远程构建需求明确后，再评估。

分发层建议：

- 主分发物是 Rust 原生二进制。
- TypeScript/Python 包只承载 SDK 和生成类型，不承载 runtime 实现本体。
- 当前 SDK 连接已运行 app-server；CLI/TUI 负责 Embedded 或 local daemon 生命周期，避免 SDK 私自复制进程管理状态机。
- Unix 与 PowerShell 安装脚本构建并安装 `golutra`、`golutra-tui`、`golutra-app-server`、`golutra-vis`；CI 在 Linux/macOS/Windows 编译全 workspace/all targets，并在 Linux 执行完整 Rust 与双 SDK 门禁。

## 完整目标技术栈

推荐基础栈：

```text
Rust workspace
tokio
clap
serde / serde_json
schemars / jsonschema
reqwest
sqlx
tracing / tracing-subscriber
thiserror / miette
ignore / walkdir
ratatui / crossterm
axum
SQLite
rg
tree-sitter
rmcp
genai
Seatbelt / bubblewrap
TypeScript / Python SDK
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
  SQLite metadata + rg + event log + artifact store + migration

Provider
  ProviderContract + Capability Matrix + Routing Policy + adapters

Tools
  schema validation + permission + sandbox + execution + ToolResultEnvelope

Observability
  tracing + replay + verification + evaluation harness

Extension
  MCP adapter + plugin contract + TypeScript/Python SDK
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
2. 建 `golutra-store`：SQLite metadata、event log、artifact store、migration。
3. 在 `golutra-protocol` 中定义 ProviderRawEvent、RuntimeEvent、UiSdkEvent，明确 durable 与 live-only。
4. 建 `golutra-llm`：ProviderConfig、ModelCatalog、CapabilityMatrix、GenaiProviderAdapter、ModelRouteDecision、FallbackPolicy。
5. 建 `golutra-tools`：ToolSchema、ToolAccesses、ToolResultEnvelope、tool registry。
6. 建 `golutra-policy`：PermissionPolicy、permission `allow/ask/deny`、workspace isolation、sandbox policy。
7. 建 `golutra-runtime`：turn flow、LoopGuard、LoopDecision 生成、recorded events、resume、compact、replay、tool/model 回流。
8. 建 `golutra-context`：working summary、history 分层、compact boundary、token budget。
9. 建 `golutra-verify`：验证结果结构化。
10. 建 `golutra-protocol`：协议类型、schema、TS 类型生成。
11. 建 `golutra-client`：`RuntimeClient`、`EmbeddedTransport`、`HttpSseTransport`、query / subscribe 语义。
12. 建 `golutra-cli`：薄 CLI 命令面。
13. 建 `golutra-tui`：`crossterm + ratatui + Golutra 业务组件`，默认通过 `EmbeddedTransport` 访问 runtime。
14. 建 `golutra-app-server`：Unix IPC 与 HTTP/SSE 入口，复用同一 Axum Router 和 runtime facts。
15. 建 `golutra-test-client`：协议 fixture、transport 对拍、app-server smoke。
16. 建 `golutra-vis`：承载 audit、event replay 与 OpenTelemetry JSON 投影，不另建事实库。
17. 建 `golutra-sandbox`、`golutra-code-intelligence`：固化 OS 执行边界和结构化代码检索。
18. 建 `golutra-eval`：eval_runner、trajectory_recorder、deep post_task_reviewer、vcr/golden fixture。
19. 建 `golutra-evolution`、`golutra-plugin`、`golutra-mcp` 和 TypeScript/Python SDK；Web/IDE 产品入口不在当前范围。

## 结合 Codex 的实施加权

在上面的顺序基础上，再加一个现实优先级判断。Codex 的工程经验说明，下面这些模块不是“可有可无的补充”，而是 runtime-first 多前端系统真正落地的骨架：

### 第一优先级

1. `golutra-protocol`
2. `golutra-client`
3. `golutra-app-server`
4. `golutra-test-client`

原因：

- 没有协议、client、app-server、test-client，多前端一致性就只是文档承诺。
- 这四层是 `RuntimeCore` 向外提供统一能力的基础设施。

### 第二优先级

1. `golutra-store`
2. `golutra-file-search`
3. `golutra-policy`

原因：

- store/event/search/policy 决定 coding agent 能否长期运行、恢复、定位和受控执行。
- 搜索应独立成模块，不要散在 TUI、CLI 或 memory 里。

### 第三优先级

1. `golutra-vis`
2. `golutra-eval`
3. `golutra-evolution`

原因：

- 这三层决定系统是否真正可调试、可回放、可评估。
- 没有它们，观测体系很容易停留在字段定义层。

## 已显式落地的模块

结合 Codex 的实际工程结构，下列能力已经从“隐含能力”升级为显式模块：

- `golutra-file-search` 与 `golutra-code-intelligence`：分别承载 rg/ignore metadata 和 tree-sitter symbol/reference graph。
- `golutra-app-server`：作为 `RuntimeHost` 的用户级 daemon 承载方式，不新增语义，只提供 IPC/HTTP attach、query、command 和 subscribe。
- `golutra-sandbox`：统一生成 macOS Seatbelt、Linux bubblewrap 或 process-only launch plan，并显式暴露 `os_enforced`。
- `golutra-plugin` 与 `golutra-mcp`：把 package review/lifecycle 与外部工具 transport 分开，最终汇入同一 ToolContract/policy/artifact/evidence 链。

约束：

- 这些模块增加的是工程承载能力，不增加新的 runtime 语义模型。
- 不允许因为模块变多而重新长出第二套任务状态机或第二套事件协议。

## 参考链接

- Tokio: https://tokio.rs/
- clap: https://docs.rs/clap/
- Serde: https://serde.rs/
- reqwest: https://docs.rs/reqwest/
- tracing: https://docs.rs/tracing/
- tower: https://tower-rs.github.io/tower/
- Ratatui: https://ratatui.rs/
- crossterm: https://docs.rs/crossterm/
- SQLx: https://sqlx.dev/
- tree-sitter: https://tree-sitter.github.io/tree-sitter/
- Model Context Protocol: https://modelcontextprotocol.io/

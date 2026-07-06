# Golutra Agent 初始实施计划

## 文档定位

本文档把现有架构文档收敛成可执行工程计划，用于从当前“文档仓库”启动到可运行的 Golutra Agent P0 runtime。

主架构真相：

- `ARCHITECTURE.md`
- `implementation-blueprint.md`
- `runtime-contracts.md`
- `agent-runtime-technology-selection.md`

第一阶段目标不是完整平台化，而是完成单 agent、多入口、可恢复、可验证、可 debug 的 coding agent runtime 闭环。

## 当前状态

截至 2026-07-05：

- 仓库已初始化 Rust workspace，并具备 core、protocol、store、runtime、tools、policy、llm、context、verify、client、CLI、TUI、app-server、test-client、eval、config、governor、memory 和 file-search crate。
- 当前代码已经具备 workspace 级共享 runtime 主干：CLI、TUI 和 app-server 主路径默认连接 `.golutra/runtime.sqlite`，并通过 `.golutra/default-session` 复用同一 workspace 默认 session。
- `RuntimeHost` 已接管 `InProcessTransport` 后端，统一持有 `RuntimeStore`、`RuntimeLaneManager`、EventBus、sequence 分配和 session 生命周期；`send_command` 不再直接写入口层假事件。
- app-server `/events` 已改为 cursor 历史 replay + 长连接轮询 live stream，TUI 可 attach 到 CLI 已创建的同一 running task。
- `RuntimeHost` 已接入后台 `AgentLoop` 执行器，P0 可通过 mock provider 触发本地工具、写入 artifact/evidence、checkpoint、verification/loop decision，并把终态投影给 CLI/TUI/app-server。
- `golutra-llm` 已提供可用的 OpenAI-compatible live provider adapter；默认仍走 deterministic mock provider，只有显式设置 `GOLUTRA_PROVIDER_PROTOCOL=openai-compatible` 或兼容的 `GOLUTRA_PROVIDER_MODE=live` 且配置 key/model 时才联网。live 配置缺失会显式失败，支持 `GOLUTRA_PROVIDER_*` 与 `OPENAI_*` env，并提供 CLI `provider protocols/current/probe` 脱敏检查入口。
- LLM 协议 catalog 已注册 `mock`、`openai-compatible`、`anthropic`、`gemini`、`vertex-ai`、`genai`；除 `mock` 与 `openai-compatible` 外，其它协议当前处于 catalog/diagnostic ready 状态，选择后会返回 `adapter_not_implemented` 而不是静默 fallback。
- provider onboarding、凭据持久化和 thread resume/fork 已完成最小闭环：`golutra-config` 支持 `$GOLUTRA_HOME/provider.json` 与 `<workspace>/.golutra/provider.json`，CLI 支持 `provider login/set-key/use/current/probe`，TUI 首屏展示 provider 状态，store/client/CLI 支持 `ThreadId`、`threads` 表、`.golutra/default-thread`、`thread list/resume/fork`。
- P1/P2 先落稳定 schema、guardrail、SDK 和最小页面，复杂自动化和外部凭据相关能力仍通过 TODO 决策位收敛。
- 第一阶段范围已在 `implementation-blueprint.md` 中明确，不能继续扩张到复杂 multi-agent 或不可审计的自动自我改进。

## 实施原则

- 先固定 runtime 事实链路，再增强 agent 能力。
- 先实现 `SessionCommand -> RuntimeEvent -> StateProjection -> LoopDecision -> VerificationRecord`，再做多入口体验。
- CLI、TUI、SDK、app-server 都只能通过统一 protocol 访问 runtime，不能各自维护任务状态机。
- provider、tool、terminal、cancel、retry、fallback 和 side effect 必须先有契约，再有实现。
- raw tool output、大 diff、日志和 provider 原始字段默认进入 artifact，模型只读取摘要、结构化事实和受控 excerpt。
- coding task 没有客观 evidence 不能 `stop_success`。
- 后台评估、改进候选和开放式探索只保留扩展位，不进入 P0 同步链路。

## P0 成功定义

P0 完成时必须能演示以下闭环：

```text
CLI 创建 coding task
-> runtime 写入 SessionCommand / RuntimeEvent
-> ContextBuilder 构造模型输入
-> mock 或真实 provider 发起 tool call
-> tool 执行并生成 ToolResultEnvelope / ArtifactRecord / EvidenceRecord
-> VerificationRecord 判断任务结果
-> LoopDecision 终止或继续
-> UserProjection 展示结果
-> DebugProjection / replay 能解释关键决策
-> 进程重启后能恢复 task 状态
```

最低验收场景：

1. 一个只读任务：读取文件、搜索内容、总结结果。
2. 一个代码修改任务：编辑文件、产生 diff、运行验证命令、生成 evidence。
3. 一个失败任务：工具失败或验证失败后不能 `stop_success`。
4. 一个取消任务：`abort` 后不再产生新的文件、进程或网络副作用。
5. 一个多入口一致性任务：同一 task 能被 test client / app-server / TUI 看到一致状态和 event stream。

## 总体里程碑

| 里程碑 | 目标 | 可并行性 | 完成标准 |
| --- | --- | --- | --- |
| P0.0 | 工程骨架与质量门禁 | 低 | workspace 可编译，fmt/clippy/test 可运行 |
| P0.1 | Protocol 与核心 schema | 中 | 核心类型可序列化，fixture roundtrip 通过 |
| P0.2 | Store、event log、projection | 中 | 重启后可恢复 session/task 状态 |
| P0.3 | RuntimeLane 与状态机 | 低 | active task、busy policy、abort 语义成立 |
| P0.4 | Tool pipeline 与 artifact/evidence | 中 | 工具成功/失败/timeout/cancelled 都有 envelope |
| P0.5 | Provider adapter 与 ContextBuilder | 中 | provider usage、token snapshot、tool call 归一化 |
| P0.6 | 最小 agent loop 与 verification | 低 | coding task 无 evidence 不能成功终止 |
| P0.7 | CLI/TUI 入口 | 中 | 入口只发 command/query，不维护私有状态；必须连接同一 RuntimeHost |
| P0.8 | app-server、RuntimeClient、多入口一致性 | 中 | 多端看到同一 task 状态和 event stream；支持 cursor replay + live stream |
| P0.9 | checkpoint、debug、replay、改进候选 | 中 | 文件副作用有恢复点，失败任务可复盘 |

P1 继续做：provider onboarding 交互式 TUI/AuthDialog、真实 provider 覆盖增强、thread rollout JSONL、resume picker、TypeScript SDK、基础 Web attach、评估回归套件。
P2 才做：长期 memory 晋升、PromotionDecision、Open-Endedness、复杂 benchmark hardening。

## 推荐 Workspace 结构

第一版按能力拆分，不按入口拆分：

```text
crates/
  golutra-core
  golutra-config
  golutra-protocol
  golutra-protocol-fixtures
  golutra-event
  golutra-file-search
  golutra-store
  golutra-runtime
  golutra-context
  golutra-tools
  golutra-policy
  golutra-memory
  golutra-llm
  golutra-verify
  golutra-governor
  golutra-client
  golutra-cli
  golutra-tui
  golutra-app-server
  golutra-eval
  golutra-test-client
sdk/
  typescript
docs/
```

P0 可以先创建全部 crate 空壳，但只实现 P0 必需模块：

- 必需：`core`、`protocol`、`event`、`store`、`runtime`、`context`、`tools`、`policy`、`llm`、`verify`、`client`、`cli`、`app-server`、`test-client`。
- 可延后：`tui` 可先做最小 attach；`eval` 只放 schema；`sdk/typescript` 到 P1 再做类型消费。

## 技术基线

| 能力 | 默认选型 | 说明 |
| --- | --- | --- |
| 主语言 | Rust | runtime、store、tool、policy、CLI/TUI/app-server |
| 异步 runtime | `tokio` | provider stream、tool task、HTTP server |
| CLI | `clap` + `miette` | 薄入口和用户可读错误 |
| TUI | `ratatui` + `crossterm` | 只消费 projection/event |
| App server | `axum` + SSE | P0 首选 HTTP command/query + SSE event stream |
| 序列化 | `serde` / `serde_json` | 所有协议和事件结构化 |
| Schema | `schemars` | 生成 JSON Schema |
| TS 类型 | `ts-rs` | P1 用于 TypeScript SDK |
| Store | SQLite + `sqlx` | 状态、索引、session/task 元数据 |
| Event log | append-only JSONL 或 SQLite table | P0 可先 SQLite table，保留 JSONL export |
| Artifact | 文件系统 blob + SQLite metadata | 大输出、raw provider、diff、日志 |
| Provider | 自研 contract + `genai` adapter | `genai` 不进入 core 类型 |
| 搜索 | `rg` 调用封装 | P0 先作为工具，P1 再独立 file-search |

TODO 决策占位：

- TODO(provider-config)：P0 env 入口已确定为 `GOLUTRA_PROVIDER_PROTOCOL`、`GOLUTRA_PROVIDER_*`、`OPENAI_*` 兼容 fallback 和默认 mock；user/workspace provider config 文件路径已落地，secretRef/OAuth 存储策略仍待确定。
- TODO(sqlite-path)：确定默认数据目录，建议遵守 XDG / macOS app support，并支持 `GOLUTRA_HOME` 覆盖。
- TODO(event-log-layout)：决定 P0 event log 只用 SQLite，还是 SQLite + JSONL 双写。
- TODO(runtime-host)：补齐 `RuntimeHost`，让 `InProcessTransport` 和 `HttpSseTransport` 都连接同一 host，而不是各自包一份 store。
- TODO(event-bus)：实现 `cursor replay + live stream`，替换一次性 `Vec<Event>` 订阅语义。
- TODO(session-resolver)：workspace 默认 session 与 default-thread 已落地；后续补 rollout JSONL 重建和跨 workspace index。
- TODO(checkpoint-strategy)：在 `shadow_git` 与独立 snapshot 之间做 P0 选择。
- TODO(sandbox-profile)：明确默认 shell 允许命令、网络策略、敏感路径和 secret 排除规则。
- TODO(protocol-version)：确定 schema version 初始值和兼容策略。

## P0.0 工程骨架

目标：让仓库从文档项目变成可编译、可测试、可持续演进的 Rust workspace。

任务：

- 初始化 `Cargo.toml` workspace。
- 创建 crate 目录和最小 `lib.rs` / `main.rs`。
- 增加 `rust-toolchain.toml`，固定 stable 版本。
- 增加 `justfile` 或等价脚本，统一本地命令。
- 增加基础 CI 配置。
- 增加 `README` 开发入口说明。
- 增加 `.editorconfig`、`rustfmt.toml`、基础 lint 策略。

建议本地命令：

```text
just fmt
just clippy
just test
just check
just schema
just smoke
```

验收：

- `cargo check --workspace` 通过。
- `cargo fmt --check` 通过。
- `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test --workspace` 通过。

不做：

- 不接真实模型。
- 不做 TUI 复杂界面。
- 不做复杂 migration。

## P0.1 Protocol 与核心 Schema

目标：先固定 runtime 语言，避免后续入口和 SDK 字段漂移。

核心类型：

- `SessionCommand`
- `RuntimeQuery`
- `RuntimeEvent`
- `StateProjection`
- `RuntimeLane`
- `BusyPolicyDecision`
- `ProviderContract`
- `ToolContract`
- `ToolResultEnvelope`
- `ArtifactRecord`
- `EvidenceRecord`
- `PolicyEvaluation`
- `VerificationRecord`
- `LoopGuardRule`
- `LoopDecision`
- `UserProjection`
- `DebugProjection`
- `TokenBudgetSnapshot`
- `TokenUsageRecord`
- `TokenAttribution`
- `WorkspaceCheckpoint`

任务：

- 在 `golutra-core` 定义 ID、时间、状态、错误和基础枚举。
- 在 `golutra-protocol` 定义 command/query/event/projection 的对外类型。
- 在 `golutra-protocol-fixtures` 建 fixture：
  - 正常只读任务。
  - 工具失败任务。
  - abort 任务。
  - verification failed 任务。
  - 多入口 attach 任务。
- 生成 JSON Schema。
- 增加 roundtrip 测试。

验收：

- 所有协议类型可 `serde` roundtrip。
- fixture 与 schema 生成稳定。
- schema 字段能追溯到 `implementation-blueprint.md`。

不做：

- 不做 TypeScript SDK 正式发布。
- 不做复杂 schema migration，只保留 `schema_version`。

## P0.2 Store、Event Log 与 StateProjection

目标：建立 runtime 事实来源。

核心接口：

```text
append_event(event)
load_events(task_id, cursor)
reduce_state(events)
query_state(session_id, task_id)
store_artifact(metadata, bytes_or_path)
load_artifact(ref)
```

任务：

- 在 `golutra-store` 初始化 SQLite migration。
- 实现 `runtime_events` append-only 表。
- 实现 `sessions`、`tasks`、`turns`、`state_projections` 基础表。
- 实现 `artifact_records`、`evidence_records` metadata 表。
- 实现 event sequence / cursor。
- 实现 reducer：从 event 还原 `StateProjection`。
- 实现 artifact blob 目录和 checksum。

验收：

- 写入 `SessionCommand` 后能产生 durable event。
- reducer 对相同 event 顺序产生相同 `StateProjection`。
- 进程重启后可查询 session/task 状态。
- 大 artifact 不进入 event payload，只保存 ref。

不做：

- 不做分布式锁。
- 不做向量检索。
- 不做 event sampling。

## P0.3 RuntimeLane 与 Task 状态机

目标：把并发、运行中输入和取消做成 runtime 语义。

P0 支持：

- `append`
- `reject`
- `abort`
- `pause`
- `resume`

P0 保留接口但默认不启用：

- `inject`
- `interrupt`
- `takeover`

状态：

```text
idle
running
waiting_approval
pausing
paused
aborting
completed
partial
failed
blocked
```

任务：

- 在 `golutra-runtime` 实现 `RuntimeLane`。
- 实现 active controller 检查。
- 实现一个 session 只有一个 active task。
- 实现 running task 收到 prompt 时生成 `BusyPolicyDecision`。
- 实现 abort 后阻止后续副作用。
- 所有状态转换写入 `RuntimeEvent`。

验收：

- 同一 session 不能同时启动两个 active task。
- 非 active controller 的 prompt 被 reject 并有原因。
- abort 后不会继续执行 shell/write/network。
- busy policy 决策可在 `DebugProjection` 中看到。

## P0.4 Policy、ToolContract 与基础工具

目标：让工具调用成为可验证、可审计、可恢复的 runtime step。

第一批工具：

- `read_file`
- `write_file`
- `edit_file`
- `list_dir`
- `rg_search`
- `shell`

执行链路：

```text
schema validation
-> policy evaluation
-> workspace/path guard
-> execute
-> ToolResultEnvelope
-> ArtifactRecord
-> EvidenceRecord
```

任务：

- 在 `golutra-policy` 实现 workspace 路径限制、敏感路径拦截、approval decision。
- 在 `golutra-tools` 实现 tool registry。
- 每个工具先定义 `ToolContract`，再实现 executor。
- shell 工具必须支持 timeout、cancel、stdout/stderr artifact。
- write/edit 工具必须记录 changed files。
- 大输出必须截断为 `model_visible_excerpt`，raw 内容进入 artifact。

验收：

- success/error/timeout/cancelled 都有稳定 `ToolResultEnvelope`。
- 副作用工具声明 `side_effect_type`、retry、idempotency 和 artifact policy。
- raw stdout/stderr 不直接进入 prompt。
- workspace 外路径默认被 block 或 ask。

不做：

- 不做完整 VM sandbox。
- 不做外部系统工具。
- 不做网络工具，除非后续明确允许。

## P0.5 ProviderContract 与 ContextBuilder

目标：接入模型但不让 runtime 依赖 provider 原生类型，模型输入必须来自 projection。

Provider 内部模型：

- `ProviderRequest`
- `ProviderEvent`
- `ProviderResponse`
- `ProviderToolCall`
- `ProviderUsage`
- `ProviderError`
- `ProviderFinishReason`

任务：

- 在 `golutra-llm` 定义 provider trait 和 contract。
- 实现 `MockProvider`，用于 P0 smoke 和 fixture。
- 实现 `GenaiProviderAdapter` 基础骨架。
- 归一化 stream event、tool call、usage、finish reason、error、rate limit。
- provider raw metadata 进入 debug artifact 或 event payload ref。
- 在 `golutra-context` 实现 `ContextContributor`、`ContextBuilder`、`TokenBudgetTracker`。
- 每次 provider call 前写 `TokenBudgetSnapshot`。
- 每次 provider response 后写 `TokenUsageRecord`。
- usage 缺失时标记 `unknown`，不能填 0 伪装。

验收：

- mock provider 可完成一次无工具响应。
- mock provider 可发起一次 tool call。
- provider usage 能生成 `TokenUsageRecord`。
- context overflow 会进入 `LoopDecision`。
- `genai` 类型不出现在 `core/protocol/runtime` 公共类型中。

TODO 配置占位：

- TODO(genai-version)：确定 `genai` crate 版本和最小 provider 覆盖。
- TODO(model-catalog)：P0 已有内置 protocol catalog 与最小模型能力字段；P1 仍需补 provider 品牌目录、可安装模型列表、streaming/vision/json/schema/reasoning 等能力矩阵。
- TODO(tokenizer)：确定 P0 token 估算方式，允许先用粗略估算但必须标记来源。

## P0.6 最小 ReAct Loop 与 Verification

目标：跑通完整 coding agent loop，并让终止状态由 evidence 支撑。

链路：

```text
ContextBuilder
-> ProviderCall
-> ToolCall
-> ToolResultEnvelope
-> ProviderCall
-> FinalResponse
-> VerificationRecord
-> LoopDecision
```

LoopGuard 第一版：

- `max_iteration`
- `repeated_tool_failure`
- `empty_response`
- `context_overflow`
- `retry_cost_exceeded`
- `oversized_tool_output`

Verification 默认规则：

- 文档任务：目标条目覆盖、结构一致、引用有效、无明显冲突。
- 代码修改任务：必须有 diff/changed files，加上 test/lint/typecheck/build/command exit code 至少一类 evidence。
- 工具执行任务：exit code、stdout/stderr 摘要、artifact 和 policy 状态。
- 配置任务：schema 校验、dry-run 或配置解析证据。

任务：

- 实现 `AgentLoop`。
- 实现 loop iteration 上限和 guard 触发。
- 实现 `VerificationRunner` 基础规则。
- 实现 terminal states：`stop_success`、`stop_partial`、`stop_failed`、`blocked`。
- minimal `PostTaskReview` 写入任务结果和残余风险。

验收：

- 同一工具确定性失败达到阈值后不会无限重试。
- provider 空回复有限恢复，失败后不污染长期历史。
- 没有 evidence 不能 `stop_success`。
- 测试失败时只能 partial/failed/blocked。
- 每次循环都有 `LoopDecision`。

不做：

- 不做复杂 planner。
- 不做 LLM judge 作为唯一验证来源。
- 不做 multi-agent。

## P0.7 CLI 与 TUI 第一入口

目标：用户可以本地跑起来，入口保持薄。

CLI 命令：

```text
golutra chat
golutra status
golutra resume
golutra abort
golutra trace
golutra export
```

TUI P0 功能：

- attach session/task。
- 展示 user projection。
- 展示工具进度。
- 展示 approval / abort 状态。
- debug panel 最小展示 event timeline。

任务：

- 在 `golutra-client` 定义 `RuntimeClient` trait。
- 实现 `RuntimeHost`，统一持有 `RuntimeStore`、`RuntimeLaneManager`、`EventBus` 和 session/task 生命周期。
- TODO(runtime-host-agent-loop)：把 provider/tool `AgentLoop` 作为 RuntimeHost 后台执行器接入，并把 Verification/LoopDecision/ToolResult 写回 event store。
- 实现 `SessionResolver`，支持 workspace 默认 session、显式 `--session`、最近 active task 恢复。
- 实现 `InProcessTransport`，但它必须连接 `RuntimeHost`，不能只包一份临时 `RuntimeStore`。
- 默认使用 workspace 持久化 SQLite store，不能把 `sqlite::memory:` 作为 CLI/TUI 主路径。
- `golutra-cli` 只把用户输入转为 `SessionCommand`。
- `golutra-tui` 只消费 `UserProjection`、`DebugProjection` 和 event stream。

验收：

- CLI/TUI 不直接拼 prompt。
- CLI/TUI 不直接调用工具。
- status/resume/abort 都通过 command/query/event 完成。
- CLI 创建或驱动 task 后，TUI attach 同一 session/task 能看到同一 running 状态。
- TUI 发送 prompt 后，CLI status 能看到同一 task 状态。
- TUI 断开后重新 attach 能恢复当前 projection。
- 当前实现如果只是 `InProcessTransport::in_memory()` 的独立 demo，不算通过 P0.7。

## P0.8 App Server、Test Client 与多入口一致性

目标：把 runtime 暴露成统一服务，验证多前端共享同一事实。

接口：

```text
POST /commands
POST /queries
GET /events?session_id=...&task_id=...&cursor=...
```

任务：

- 在 `golutra-app-server` 实现 HTTP command/query。
- app-server 必须连接同一 `RuntimeHost`，不能在 HTTP 进程里创建另一套临时 store。
- 实现 SSE event stream：先按 cursor replay 历史事件，再保持连接接收 live event。
- command 返回 accepted，不表示任务完成。
- 在 `golutra-test-client` 做 transport 对拍。
- 增加多入口 smoke：
  - test client 创建 task。
  - app-server 查询 task。
  - TUI 或 in-process observer attach。
  - 任意端 abort。
  - 所有端看到一致终止事件。

验收：

- event cursor 可用于断线恢复。
- SSE 连接在没有新事件时保持打开，而不是一次性返回已有事件后结束。
- 高优先级状态事件不能因为前端消费慢而静默丢失；低优先级 UI delta 可以降级或合并，但必须产生 lag / skipped 标记。
- task 状态由 `RuntimeEvent + StateProjection` 推导，不来自入口缓存。
- approval/abort/resume 在多端可见。
- transport 不改变 runtime 语义。

不做：

- WebSocketTransport 后置。
- IDE companion 后置。
- 正式 Web UI 后置。

## P0.9 WorkspaceCheckpoint、Debug、Replay 与 ImprovementCandidate

目标：文件副作用可恢复，失败任务可解释，后续改进有候选但不自动应用。

任务：

- 实现 `WorkspaceCheckpoint` P0 策略。
- edit/write 后记录 checkpoint ref、changed files、restore hint。
- checkpoint 遵守 `.gitignore`、policy 排除和 secret 排除。
- 实现 `DebugProjection`：
  - event timeline
  - context projection summary
  - provider raw refs
  - tool envelope
  - verification
  - LoopDecision
- 实现 `replay(task_id)` 最小能力：按 event/artifact 定位关键决策。
- 失败任务生成 `ImprovementCandidate`，状态为 `proposed`。

验收：

- checkpoint 不修改用户 `.git`。
- checkpoint 失败写 event，并进入 residual risk。
- 失败任务能解释停止、失败或 blocked 的原因。
- ImprovementCandidate 不会自动修改 prompt、policy、runtime code 或 memory。

TODO 决策占位：

- TODO(secret-detection)：确定 P0 secret 排除规则是否只用文件名/路径模式，还是接入扫描器。
- TODO(snapshot-retention)：确定 checkpoint 默认保留数量和清理策略。

## P1 计划

P1 在 P0 稳定后推进，不反向污染 P0：

- [x] TypeScript SDK 从 schema/type 产物消费协议。
- [x] Web attach 页面，只展示 projection 和 event stream。
- [ ] 真实 provider golden tests。TODO(provider-golden)：需要确定真实 provider、模型、密钥环境变量和可提交的脱敏 golden fixture。
- [x] Provider onboarding 基础层：实现 `ProviderInstallPlan`、`provider login/set-key/use`、TUI provider 状态提示和 `/auth` slash command。
- [x] Thread/session 基础层：引入用户可见 `ThreadId`、`threads` 表、`default-thread`、`thread list/resume/fork` 和 TUI `/resume`、`/threads`、`/fork` slash command。
- [x] TUI 首次 connect provider flow：补最小 qwen-code 风格 provider setup，支持 OpenAI-compatible base URL/model/API key 和 Continue with mock。
- [ ] Web 首次 connect provider flow 与 provider probe rollback。TODO(provider-web-onboarding)：Web 需要复用 RuntimeHost/config service 的 provider command，配置失败时回滚 active selection。
- [ ] TUI resume picker：当前 workspace / all workspaces、resume / fork、预览 transcript。
- [ ] 多工作区索引：增加 `$GOLUTRA_HOME/index.sqlite`，跨 workspace 列出最近 thread，同时保持 workspace SQLite 为事实来源。
- [x] 更完整的 config loader 和 model catalog。
- [x] file-search 独立模块，加入 SQLite metadata + rg。
- [x] evaluation runner 最小可用：
  - `EvaluationCase`
  - `EvaluationRun`
  - `EvaluationResult`
  - regression fixture
- [x] benchmark metadata 固化：
  - dataset version
  - harness version
  - scaffold id
  - token/cost/runtime
  - leakage/judge checks
- [x] deep `PostTaskReview` 和人工查看的 `ImprovementCandidate` 列表。

## P2 计划

P2 做治理增强和长期能力，当前已先落 schema / guardrail 骨架，运行时深度自动化后续继续：

- [x] `VerificationTier`
- [x] `EventSamplingPolicy`
- [x] `ContextProjectionCache`
- [x] `GoalLedger`
- [x] `GoalAlignmentCheck`
- [x] `RuntimeGovernor`
- [x] 长期 memory 晋升与 rollback。
- [x] RegressionResult 与 PromotionDecision。
- [x] 低风险候选自动晋升。
- [x] Open-Endedness：
  - GeneratedTask
  - CurriculumItem
  - CapabilityFrontier
  - SkillCandidate
  - BenchmarkPromotion

## 实施顺序建议

第一周建议只做 P0.0 到 P0.2：

1. 初始化 Rust workspace 和质量门禁。
2. 建 core/protocol/event/store/runtime 空 crate。
3. 写第一批 schema 类型和 fixture。
4. 实现 event append 和 projection reducer。
5. 用 integration test 验证重启恢复。

第二周推进 P0.3 到 P0.5：

1. RuntimeLane 和状态机。
2. Policy + tool registry + read/list/rg/shell。
3. Artifact/Evidence 最小链路。
4. MockProvider + ContextBuilder + TokenBudgetSnapshot。
5. 用 mock tool call 跑通一次 loop 前半段。

第三周推进 P0.6 到 P0.8：

1. AgentLoop + LoopGuard。
2. VerificationRunner。
3. CLI + InProcessTransport。
4. app-server + SSE。
5. test client 多入口一致性 smoke。

第四周推进 P0.9 和硬化：

1. WorkspaceCheckpoint。
2. DebugProjection。
3. replay。
4. ImprovementCandidate。
5. 补齐失败、abort、timeout、verification failed 测试。

## 测试策略

测试按风险分层：

- unit：核心类型、reducer、policy、tool envelope、loop guard。
- integration：store 恢复、runtime lane、tool pipeline、mock provider loop。
- contract：schema fixture、app-server transport、event stream cursor。
- smoke：CLI 创建任务、abort、trace、replay。
- regression：从失败 task 沉淀的 case，P1 后纳入 CI。

P0 必须覆盖：

- schema roundtrip。
- event ordering。
- reducer determinism。
- tool success/error/timeout/cancelled。
- abort 后无副作用。
- provider empty response。
- repeated tool failure。
- verification failed。
- no evidence no success。
- app-server query/subscribe/abort/resume。

## 质量门禁

每次提交前至少运行：

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

文档或 schema 变更还要运行：

```text
just schema
just fixture
```

P0 release candidate 还要通过：

```text
just smoke
just replay-smoke
just transport-smoke
```

## 风险与控制

| 风险 | 控制方式 |
| --- | --- |
| 架构继续膨胀 | P0 只做 runtime 闭环，P1/P2 列表不能提前混入 |
| 协议漂移 | schema fixture 先行，SDK 不手写近似字段 |
| TUI 变状态机 | TUI 只能消费 projection/event |
| provider 类型污染 runtime | `genai` 只存在 adapter 内 |
| 工具输出污染 prompt | raw output 进 artifact，模型只看 summary/excerpt |
| 验证流于形式 | coding task 必须有客观 evidence |
| checkpoint 污染用户仓库 | 不修改用户 `.git`，遵守 ignore 和 secret policy |
| retry 重复副作用 | 副作用工具必须声明幂等和 retry policy |
| 多入口状态不一致 | 所有入口共享 `RuntimeClient` 语义和 event stream |
| benchmark 被污染 | P1 后记录 harness/scaffold/token/cost/leakage/judge metadata |

## 明确不做

P0 不做：

- 复杂 multi-agent orchestration。
- 自动修改 runtime 代码。
- 自动晋升长期 memory。
- 动态 benchmark。
- Open-Endedness 主动探索。
- 插件 marketplace。
- 复杂 RuntimeGovernor / GoalLedger。
- WebSocketTransport。
- IDE companion。
- 完整 VM sandbox。

## 下一步任务清单

立即开始实施时按下面顺序开 PR 或提交：

- [x] P0.0-1 初始化 Rust workspace。
- [x] P0.0-2 创建 core/protocol/event/store/runtime/context/tools/policy/llm/verify/client/cli/app-server/test-client crate。
- [x] P0.0-3 添加 `justfile`、fmt、clippy、test、schema、smoke 命令。
- [x] P0.1-1 定义 ID、时间、状态、错误基础类型。
- [x] P0.1-2 定义 `SessionCommand`、`RuntimeQuery`、`RuntimeEvent`、`StateProjection`。
- [x] P0.1-3 定义 tool/provider/verification/loop/token/artifact/evidence schema。
- [x] P0.1-4 建协议 fixture 和 roundtrip 测试。
- [x] P0.2-1 实现 SQLite migration 和 store 初始化。
- [x] P0.2-2 实现 event append/load/cursor。
- [x] P0.2-3 实现 StateProjection reducer。
- [x] P0.2-4 实现 artifact/evidence metadata 和 blob 存储。
- [x] P0.3-1 实现 RuntimeLane active task 和 active controller。
- [x] P0.3-2 实现 append/reject/abort/pause/resume。
- [x] P0.4-1 实现 policy guard 和 tool registry。
- [x] P0.4-2 实现 read/list/rg/shell。
- [x] P0.4-3 实现 write/edit 和 changed files 记录。
- [x] P0.5-1 实现 MockProvider。
- [x] P0.5-2 实现 ContextBuilder 和 TokenBudgetSnapshot。
- [x] P0.5-3 接入 GenaiProviderAdapter 骨架。
- [x] P0.6-1 实现 AgentLoop 和 LoopGuard。
- [x] P0.6-2 实现 VerificationRunner 和 terminal states。
- [x] P0.7-1 实现 CLI 演示入口。
- [x] P0.7-2 实现 `RuntimeHost + SessionResolver + workspace 持久化 store`。
- [x] P0.7-3 让 `InProcessTransport` 连接共享 `RuntimeHost`，并让 TUI attach / composer / debug timeline 消费同一 runtime。
- [x] P0.8-1 实现 app-server command/query/SSE 演示入口。
- [x] P0.8-2 实现 `cursor replay + live stream` SSE。
- [x] P0.8-3 实现 test client 多入口一致性 smoke。
- [x] P0.8-4 接入 RuntimeHost 后台 AgentLoop 执行器。
- [x] P0.9-1 实现 WorkspaceCheckpoint。
- [x] P0.9-2 实现 DebugProjection、replay 和 ImprovementCandidate。

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

截至 2026-07-15：

- 仓库已初始化 Rust workspace，并具备 core、protocol、store、runtime、tools、policy、sandbox、auth、llm、context、verify、client、CLI、TUI、app-server、test-client、eval、evolution、config、governor、memory、file-search、code-intelligence、plugin、MCP 和 vis crate。
- 当前代码采用 Codex 式混合进程模型：CLI/TUI 默认通过 durable `EmbeddedTransport` 在当前进程运行 `RuntimeHost`；显式 `--daemon` 连接用户级单实例 app-server，`--connect` 连接指定远端；TypeScript SDK 先按 canonical cwd 创建 attachment。cwd 只决定执行、权限和历史过滤，不决定进程生命周期。
- `RuntimeHost` 统一持有 `RuntimeStore`、`RuntimeLaneManager`、`AgentLoop`、EventBus、sequence 分配、task handle、`CancellationToken`、pending turn queue 和 session 生命周期；command ack 按 workspace-scoped idempotency key 持久化，已完成 ack 的重试不会重复启动 task。command provisional ack 与后续业务事件仍不是同一个事务，极端 owner crash 窗口保持 at-least-once 语义。
- app-server 是 `$GOLUTRA_HOME/app-server` 下的用户级单实例，维护有 128 cwd 上限的 `cwd -> EmbeddedTransport` registry 和 attachment 路由，失败初始化会释放槽位；每次 attach 刷新该 cwd 最近 thread/session。Unix 本地 daemon 同时发布 owner-only `app-server.sock`，CLI/TUI 默认使用 `UnixIpcTransport`；Windows 和远端使用 bearer + protocol version 保护的 HTTP/SSE。IPC 与 HTTP 进入同一个 Axum Router，command/query/attachment/cursor 语义一致。`/events` 已实现 cursor 历史 replay + live stream，断线后从最后成功消费的 cursor 续订。daemon 在没有 transport auth 前强制绑定 loopback，并校验 Host/Origin 和本地 endpoint root HTTP loopback URL。
- `RuntimeHost` 已接入后台 `AgentLoop` 执行器，P0 可通过 mock provider 触发本地工具、写入 artifact/evidence、checkpoint、verification/loop decision，并把终态投影给 CLI/TUI/app-server。
- `golutra-llm` 已提供独立 OpenAI-compatible Chat Completions adapter、ChatGPT OAuth 专用 OpenAI Responses SSE adapter，以及基于固定 `rust-genai` 版本的 `GenaiProviderAdapter`。OpenAI-compatible 已使用真实 SSE 增量流并产生有序 `ProviderStreamed` 事件；probe 会从 `/models` 元数据更新 streaming/tools/JSON Schema/reasoning/vision/context window/max output capability。native adapter 按协议强制路由 Anthropic Messages、Gemini generateContent、Vertex AI generateContent，或按 model namespace 执行 `genai` 路由；provider 原生类型不会进入 core/protocol/runtime。
- LLM 协议 catalog 中的 `mock`、`openai-compatible`、`openai-responses`、`anthropic`、`gemini`、`vertex-ai`、`genai` 均可执行。统一映射 system/user/assistant/tool message、tool call/result、usage、reasoning effort、finish reason 和脱敏错误；Responses 的 encrypted reasoning item 可跨工具回合安全 replay；缺失或损坏的 live 配置显式失败，不静默 fallback。
- provider onboarding、凭据持久化和 thread resume/fork 已完成最小闭环：`golutra-config` 使用 `$GOLUTRA_HOME/provider.json` v2 作为全局 provider/auth 配置，profile 只保存 SecretRef 和非敏感 metadata；`golutra-auth` 负责 owner-only `$GOLUTRA_HOME/credentials.json`/env 凭据解析与 OAuth token 生命周期，不访问 OS keychain。受审计 catalog 参考 opencode 内置 OpenAI ChatGPT browser/headless、xAI browser/device 和 GitHub Copilot device，并将 auth method 固定绑定实际模型 adapter；CLI 支持 `provider auth-methods/login/set-key/oauth-login/logout/use/current/probe`，TUI `/auth` 按 provider 展示 OAuth/API key 选项。未配置时 current/probe/runtime 一致解析为 mock。thread 元数据统一位于全局 SQLite，session 唯一索引保证一对一绑定，并按 canonical cwd 过滤 `list/resume/fork`。
- 用户可见 conversation 层已完成轻量闭环：`TaskCreated` 记录用户输入，`AssistantMessage` 记录最终回复，`UserProjection.final_message` 可从历史事件恢复；resume 后继续任务时，RuntimeHost 会把同一 session 的压缩历史摘要加入 provider context。
- P1 provider onboarding 已补齐 TUI `/auth` qwen-code 风格主流程：provider 分组、第三方 preset、provider-specific auth method、协议选择、baseUrl、disk/env 凭据、model/advanced config、保存前 review 和同名 profile 覆盖提示；Custom Provider 已按 `(protocol, baseUrl)` 派生 envKey，保存后 probe 失败会 rollback。内置 OpenAI/xAI/Copilot OAuth 与显式 `/auth oauth-login`、`/auth logout` 已接入 browser PKCE/device flow、固定/动态 loopback callback、OpenID account metadata、token refresh/revoke 和取消；Web 首次 provider onboarding 不在当前产品范围。
- `AgentLoop` 已支持 provider/tool 多轮消息、LoopGuard、provider retry/fallback、governor 检查和终态 verification；初始或累积 context overflow 会形成结构化 Blocked/AskUser `LoopDecision`。pause/resume/abort 会驱动真实 task cancellation，运行中 prompt 进入 durable pending turn queue；owner 重启后只自动恢复尚未产生 `TurnStarted` 的 turn。
- 文件副作用已使用修改前持久化的 before-image checkpoint，checkpoint 成功写入 owner-only manifest/artifact blob 后才执行修改；runtime DB、artifact、checkpoint、memory/evaluation 和 endpoint metadata 在 Unix 上使用 owner-only 权限，项目 `.golutra` 不参与 runtime 持久化，工具 policy 仍阻断 `.git`/`.golutra` 内部路径；artifact blob 带 SHA-256 checksum 和敏感字段清洗。ToolContract 使用 JSON Schema 校验，必填路径/搜索参数拒绝空串，schema 错误隐藏实例值，structured facts 递归脱敏；shell 与 file-search 均有 timeout/cancel/output 边界。
- TypeScript 与 Python SDK 都由同一份 Rust schema 生成，已提供 cwd attachment、command/query/SSE、thread fork/rollout export/rebind 以及 memory/evaluation/evolution/candidate 高层 API；JSON 请求、SSE frame 和 timeout 都有固定上限，attachment 只在服务端明确返回 `410 Gone` 后重建。跨进程测试覆盖一个 daemon 同时路由两个 cwd、daemon 重启、Unix IPC/HTTP 事实对拍、SSE replay/live、command 幂等、fork/rebind 和 durable memory/evaluation 恢复。
- SQLite `runtime_events` 是 thread 历史的 canonical facts；每个 cwd 分区会物化 owner-only、逐行 checksum、递归脱敏的 rollout JSONL。启动和显式 export 会从 SQLite 原子重建，增量 append 与重建共享跨进程锁。fork 可复制完整历史或截断到指定 turn，在单一 SQLite 事务中重建 EventId/TaskId/TurnId，并保留 immutable artifact lineage；rebind 要求显式旧 canonical cwd，只允许 inactive 且未被其它 runtime 持有的 thread。
- project memory、deep evaluation、RegressionResult、PromotionDecision 和 RuntimeGovernor 已完成受控最小实现：memory 只有 evidence-backed candidate 可晋升并支持 expiry-aware contradiction gate/feedback/rollback，单条内容和 memory/evaluation 状态文件有尺寸边界；自动 apply 仅限通过 clean regression 的低风险 benchmark。user/global scope 只保留需要人工 review 的模型边界，不由任务自动晋升。
- `golutra-evolution` 已把 GeneratedTask 从 schema 占位推进到受预算治理的执行主链：planner 从 durable evaluation 生成 novelty/curriculum/frontier/environment recipe，选中任务只在隔离目录、deterministic mock provider、无网络 sandbox profile 中通过同一 RuntimeHost 执行。Skill 必须经过 stage、带 regression refs 的人工 review、checksum 校验和 install，安装后只按目标相关性作为 `ContextContributor` 注入，并支持 rollback。
- `golutra-plugin` 与 `golutra-mcp` 已建立用户级扩展主链：插件包经过 stage/review/enable/disable/rollback、不可变 checksum、owner-only 权限和 manifest schema 审核；外部 MCP 工具默认 `PolicyDecision::Ask`，批准后才在 macOS Seatbelt 或 Linux bubblewrap 中启动一次性 stdio server，并继续经过 timeout/cancel、redaction、artifact/evidence 和 ToolContract 校验。没有 OS-enforced sandbox 时拒绝执行。
- `golutra-vis` 已提供 Audit、Events 与 OpenTelemetry JSON 投影；普通 TUI 仍只展示 UserProjection，显式 developer/debug mode 才消费治理事实。
- 第一阶段范围已在 `implementation-blueprint.md` 中明确，不能继续扩张到复杂 multi-agent 或不可审计的自动自我改进。

## 已知实现边界

- command ack/claim 与命令产生的全部业务事件尚未组成单一数据库事务；owner 在两者之间崩溃时可能重处理 provisional command，因此副作用仍必须依赖 checkpoint、policy 和幂等约束。
- 已产生 `TurnStarted` 的中断 turn 不会自动重放；runtime 只能安全恢复尚未开始的 pending turn，并把原 active task 标为 cancelled。
- checkpoint 对单文件使用临时文件 + fsync + 原子替换；多文件 rollback 会先完整校验 manifest，但不是跨文件系统事务。
- `/events/replay`、DebugProjection 和显式 TUI developer mode 当前会物化请求范围内的完整历史；普通 TUI 不查询该投影。SSE 主链按页 replay，但超长 session 的显式全量 debug/export 仍需要后续分页协议和 UI 虚拟化。
- macOS Seatbelt 与 Linux bubblewrap 已进入 shell/MCP 执行主链；Windows 当前只有 process policy、timeout/cancel 和插件管理能力，没有可声明为 OS-enforced 的外部插件 sandbox，因此 Windows 会拒绝执行 MCP server。
- provider streaming、动态 capability discovery 和运行中跨客户端 `ProviderAuthRequired -> Submitted/Cancelled -> resume` 已完成；Web 首次 provider onboarding 不是当前产品范围。
- 自动修改 runtime code 和自动部署新版本不属于当前 P0/P1/P2 交付范围，其 P3 目标另见 `self-evolving-runtime-design.md`；复杂 multi-agent orchestration、正式 Web/IDE 产品面和网络插件 marketplace 仍不是当前交付目标。

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

P1 主链已完成：provider streaming、动态 capability discovery、运行时动态凭据请求、双 SDK、Unix IPC、Plugin/MCP 与跨进程验收均已落地。
P2 已落地受控最小闭环：memory/evaluation/governor/evolution/skill 能运行、持久化、回归和回滚；长期线上监控、自动 runtime redeploy 和高风险自动晋升明确后置。

## 推荐 Workspace 结构

第一版按能力拆分，不按入口拆分：

```text
crates/
  golutra-core
  golutra-config
  golutra-auth
  golutra-protocol
  golutra-protocol-fixtures
  golutra-event
  golutra-file-search
  golutra-code-intelligence
  golutra-store
  golutra-runtime
  golutra-sandbox
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
  golutra-evolution
  golutra-plugin
  golutra-mcp
  golutra-vis
  golutra-test-client
sdk/
  typescript
  python
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
| App server | `axum` + Unix IPC + SSE | Unix 本地 daemon 使用 owner-only socket；Windows/远端使用 HTTP command/query + SSE |
| 序列化 | `serde` / `serde_json` | 所有协议和事件结构化 |
| Schema | `schemars` | 生成 JSON Schema |
| SDK 类型 | JSON Schema 生成 | TypeScript 与 Python SDK 共用 Rust protocol schema |
| Store | SQLite + `sqlx` | 状态、索引、session/task 元数据 |
| Event log | SQLite canonical table + derived rollout JSONL | SQLite 负责事务事实，JSONL 负责可携带 replay/export |
| Artifact | 文件系统 blob + SQLite metadata | 大输出、raw provider、diff、日志 |
| Provider | 自研 contract + `genai` adapter | `genai` 不进入 core 类型 |
| 搜索 | `rg` 调用封装 | P0 先作为工具，P1 再独立 file-search |

已确定的技术决策：

- provider 配置使用全局 `$GOLUTRA_HOME/provider.json` v2；workspace 不覆盖 auth。配置只保存 `credential_ref` 和非敏感 metadata，API key 与 OAuth access/refresh token set 进入 owner-only `$GOLUTRA_HOME/credentials.json`，CI 继续使用只读 env ref；v1 明文 `env` map 仅在持锁的一次性迁移中读取并写入 disk SecretRef，不涉及 OS keychain。
- runtime state、event log、projection 和 thread index 使用 `$GOLUTRA_HOME/state/runtime.sqlite`；artifact 和 cwd 分区状态位于同一 state 根目录，项目目录不写 runtime 文件。
- CLI/TUI 默认使用 durable `EmbeddedTransport`；用户级 loopback app-server 和远端 endpoint 只在显式 `--daemon` / `--connect` 时使用。
- event stream 使用 `cursor replay + broadcast live stream`；cursor 只在消费成功后推进，lag 必须显式产生 skipped/lag 语义。
- checkpoint 使用独立 artifact before-image，不修改用户 Git。
- shell 默认无网络工具、只执行结构化 argv，并受 policy、approval、timeout、cancel、workspace guard 和 `SystemSandbox` 约束；macOS 使用 Seatbelt，Linux 检测 bubblewrap，未检测到 OS sandbox 时只允许经过保守 policy 的内置进程工具，外部 MCP 一律拒绝。
- runtime protocol 使用显式 `name + minimum/current version range` 握手；HTTP 与 IPC 都校验当前版本，未知版本直接拒绝。
- thread resolver 从全局 SQLite 按 canonical cwd 选择最近 session/thread；无历史时只生成内存 ID，首个 prompt 才持久化；显式新 session 会创建独立 thread，不能覆盖旧 thread。session owner 异常退出后，后续成功取得 lease 的 host 会按需取消孤儿 active task。rollout 位于 cwd hash 分区并可从 SQLite 重建。

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
- 实现 OpenAI-compatible adapter 与 `GenaiProviderAdapter`，覆盖 Anthropic、Gemini、Vertex AI 和 model-routed genai。
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

已确定配置项：

- `genai` crate 已固定为 `0.7.0-beta.12`；升级必须先更新所有协议 golden fixture 并通过 wire diff 审查。
- 内置 provider/model catalog 已包含 declared capability，OpenAI-compatible probe 可从模型目录更新 streaming、tools、JSON Schema、reasoning、vision、context window 和 max output；discovered 与 declared source 明确区分。
- 当前 context 预算使用确定性的字符近似估算，并在 attribution 中标记为 `tokenizer`/`estimated`；provider 返回 usage 时以 provider facts 为准，缺失值保持 unknown，不伪装为 0。引入模型专用 tokenizer 属于精度优化，不是 runtime 正确性缺口。

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
- 普通启动只展示 UserProjection；显式 developer/debug panel 展示 runtime facts、verification、LoopDecision、evaluation/improvement 阶段计数和最近 event，不污染用户 transcript。

任务：

- 在 `golutra-client` 定义 `RuntimeClient` trait。
- 实现 `RuntimeHost`，统一持有 `RuntimeStore`、`RuntimeLaneManager`、`EventBus` 和 session/task 生命周期。
- 把 provider/tool `AgentLoop` 作为 RuntimeHost 后台执行器接入，并把 Verification/LoopDecision/ToolResult 写回 event store。
- 实现 cwd thread resolver，支持最近 session、显式 `--session` 和孤儿 active task 恢复。
- 实现 `EmbeddedTransport`，它持有完整 `RuntimeHost` 并连接全局 durable store，不能只包一份临时 `RuntimeStore`。
- 默认在 CLI/TUI 进程内运行 durable Embedded host；显式 daemon/remote 模式通过 `HttpSseTransport` 和 attachment 协议连接同一套 runtime 语义。
- `golutra-cli` 只把用户输入转为 `SessionCommand`。
- `golutra-tui` 只消费 `UserProjection`、`DebugProjection` 和 event stream。

验收：

- CLI/TUI 不直接拼 prompt。
- CLI/TUI 不直接调用工具。
- status/resume/abort 都通过 command/query/event 完成。
- daemon/remote 模式下，CLI 创建或驱动 task 后，TUI attach 同一 session/task 能看到同一 running 状态和 live event。
- 默认 Embedded 模式下，其他进程可以查询同一 durable task 状态，但不能伪造 pause/abort 或订阅另一个进程的内存 EventBus。
- TUI 断开后重新 attach 能从全局 store 按 cursor 恢复当前 projection；实时续流要求连接原 attachment。
- `RuntimeTransport::in_memory()` 只允许测试显式使用，不能作为 CLI/TUI 主路径。

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

- 不实现 WebSocketTransport；IPC + HTTP/SSE 已覆盖当前本地与远端 transport 需求。
- IDE companion 和正式 Web UI 不是当前产品范围。

## P0.9 WorkspaceCheckpoint、Debug、Replay 与 ImprovementCandidate

目标：文件副作用可恢复，失败任务可解释，后续改进有候选但不自动应用。

任务：

- 实现 `WorkspaceCheckpoint` P0 策略。
- edit/write 前持久化目标文件 before-image、checkpoint artifact ref、changed files 和 restore hint，成功后才执行文件写入。
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

已确定策略：

- checkpoint 在副作用前同时应用 canonical workspace guard、`.gitignore`、`.git`/`.golutra` 与敏感路径规则；artifact/structured facts 进入统一递归 redaction。高熵扫描器可作为额外防御，但不是保存已知敏感路径的替代品。
- 每个 workspace 默认只保留最近 20 个 checkpoint；RuntimeHost 启动和显式 storage maintenance 都会执行 checkpoint/artifact/temp 清理，并保护仍被 lineage、verification 或 rollback 引用的 artifact。

## P1 计划

P1 在 P0 稳定后推进，不反向污染 P0：

- [x] TypeScript SDK 从 schema/type 产物消费协议。
- [x] Web attach 页面，只展示 projection 和 event stream。
- [x] 真实 provider golden tests：对 OpenAI-compatible、OpenAI Responses、Anthropic、Gemini、Vertex AI、genai 的实际 adapter wire 做本地 HTTP 捕获，覆盖完整请求、tool round-trip、encrypted reasoning replay、usage/finish reason、auth header 和 401；live smoke 只读取专用 `GOLUTRA_LIVE_*` env，缺失时安全跳过。
- [x] Provider onboarding 基础层：实现 `ProviderInstallPlan`、`provider login/set-key/use`、TUI provider 状态提示和 `/auth` slash command。
- [x] Thread/session 基础层：引入用户可见 `ThreadId`、全局 `threads` 表、按 cwd 的 `thread list/resume/fork` 和 TUI `/resume`、`/threads`、`/fork` slash command。
- [x] TUI 首次 connect provider flow：补 qwen-code 风格 provider setup，支持 Golutra API / Third-party Providers / Custom Provider / mock 分组，第三方 provider preset，OpenAI-compatible/Anthropic/Gemini/Vertex AI/genai 协议、base URL/API key/model/advanced config，以及保存前 review、脱敏 install plan、同名 profile 覆盖提示、Custom Provider 派生 envKey 和 probe 失败 rollback。
- [x] SecretRef/OAuth：新增独立 `golutra-auth`，provider config v2 只保存 credential ref；交互 API key 和 OAuth token set 默认写 owner-only `$GOLUTRA_HOME/credentials.json`，CI 使用 env ref；支持受审计 provider auth catalog、browser PKCE/device flow、固定/动态 loopback callback、持久 token set、OpenID account metadata、提前刷新、401 单次重试、进程内/跨进程 single-flight、revoke/logout 和 v1 到 disk 的原子迁移。
- [x] Thread rollout/fork/rebind：物化 checksum + redaction JSONL，支持按 turn 完整 fork、artifact lineage、CLI/TUI/HTTP/SDK export 与显式路径 rebind，并覆盖 daemon restart。
- [x] TUI 当前 workspace resume picker：支持 session 列表、resume / fork、历史 transcript 恢复和滚动。
- [x] 多工作区事实索引：全局 `$GOLUTRA_HOME/state/runtime.sqlite` 存储所有 thread，并按 canonical cwd 建索引和过滤。
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
- [x] Provider streaming 与动态能力：OpenAI-compatible SSE delta/tool/usage 归一化、`ProviderStreamed` runtime event、模型目录 capability discovery 和 malformed/truncated stream 边界。
- [x] 运行中认证协议：`ProviderAuthRequired`、等待态、跨客户端 submitted/cancelled、verified reload/probe 和安全恢复。
- [x] Unix IPC：owner-only socket 复用 app-server Router，并与 HTTP/SSE 对拍 command/query/thread/event/cursor 语义。
- [x] Python SDK：与 TypeScript SDK 共用 schema，覆盖 attachment、command/query/event/thread 和治理 API。
- [x] Plugin/MCP：用户级 package lifecycle、reviewed manifest、OS sandbox、approval、stdio MCP、schema 对照与 artifact/evidence。
- [x] 安装与跨平台 CI：Unix/PowerShell 安装脚本，Linux/macOS/Windows all-target compile，生成产物漂移检查。

## P2 计划

P2 做治理增强和长期能力；当前确定性 governor、durable evaluation、memory、Evolution/Skill 已形成受控运行闭环，仍禁止高风险自动 redeploy：

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
- [x] Evolution 执行：预算与 novelty/difficulty/safety gate、隔离 GeneratedTask RuntimeHost、durable run/execution 状态。
- [x] Skill 生命周期：stage、regression-backed review、checksum install、相关性 context 注入与 rollback。
- [x] OpenTelemetry projection：从 RuntimeEvent 生成脱敏、稳定 trace/span JSON，并通过 `golutra-vis` 导出。

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
3. CLI + EmbeddedTransport。
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
- [x] P0.5-3 完成 OpenAI-compatible、Anthropic、Gemini、Vertex AI 与 genai adapter 及 provider golden tests。
- [x] P0.6-1 实现 AgentLoop 和 LoopGuard。
- [x] P0.6-2 实现 VerificationRunner 和 terminal states。
- [x] P0.7-1 实现 CLI 演示入口。
- [x] P0.7-2 实现 `RuntimeHost + cwd thread resolver + 全局持久化 store`。
- [x] P0.7-3 让 `EmbeddedTransport` 持有完整 `RuntimeHost`，并让 TUI composer / developer debug projection 消费同一 runtime。
- [x] P0.8-1 实现 app-server command/query/SSE 演示入口。
- [x] P0.8-2 实现 `cursor replay + live stream` SSE。
- [x] P0.8-3 实现 test client 多入口一致性 smoke。
- [x] P0.8-4 接入 RuntimeHost 后台 AgentLoop 执行器。
- [x] P0.9-1 实现 WorkspaceCheckpoint。
- [x] P0.9-2 实现 DebugProjection、replay 和 ImprovementCandidate。
- [x] Hardening-1 建立默认 Embedded、显式用户级 daemon/remote 的混合进程模型，并让 SDK 通过 cwd attachment 连接 HTTP/SSE RuntimeHost。
- [x] Hardening-2 实现 task handle、`CancellationToken`、durable pending turn 恢复和真实 pause/resume/abort；只重放未开始 turn。
- [x] Hardening-3 实现多轮 provider/tool message、LoopGuard、retry/fallback 和 governor hook。
- [x] Hardening-4 将 checkpoint 收敛为修改前 before-image，并落地 artifact blob/checksum/redaction。
- [x] Hardening-5 补齐 approval、ToolContract JSON Schema、异步 shell timeout/cancel/process-group termination。
- [x] Hardening-6 完成 SSE replay/live、`HttpSseTransport`、schema 生成 SDK 和跨进程重启测试。
- [x] Hardening-7 接入 project memory、durable evaluation、regression/promotion gate、RuntimeGovernor 和受控 P2 candidate 状态机。
- [x] Hardening-8 完成 rollout JSONL、完整历史/指定 turn fork、artifact lineage、显式 cwd rebind 和跨进程重启验收。
- [x] Hardening-9 完成真实 provider streaming、模型 capability discovery 和运行中 ProviderAuthRequired 恢复协议。
- [x] Hardening-10 完成 artifact/checkpoint retention、storage stats/maintenance 和受引用 artifact 保护。
- [x] Hardening-11 完成 tree-sitter code intelligence、symbol/reference 工具和 owner-only 持久索引。
- [x] Hardening-12 完成 Evolution/Skill 受控执行、回归 review、安装注入和 rollback。
- [x] Hardening-13 完成 Plugin/MCP 生命周期、OS sandbox、approval 和外部工具统一治理。
- [x] Hardening-14 完成 Unix IPC、Python SDK、安装脚本、三平台 compile 与连续会话/跨进程验收。
- [x] Hardening-15 完成 Audit/Events/OpenTelemetry 投影和普通/开发者 TUI 观测隔离。

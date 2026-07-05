# Golutra Agent Runtime 架构规格

## 文档定位

本文档是 Golutra 的主架构规格，回答：

```text
Golutra 的核心系统是什么，
从用户输入到任务完成经过哪些链路，
哪些能力属于核心，哪些能力属于扩展。
```

外部项目影响和调研结论保留在 `framework-comparison.md`。实现时优先以本文档作为架构真相。
具体落地顺序、最小 schema 和同步/后台/离线边界见 `implementation-blueprint.md`。

## 核心结论

Golutra 不是普通 CLI agent，也不是 prompt + tools 的包装层。它应设计为 Rust-first Agent Runtime OS。

第一阶段核心链路是：

```text
User Input
-> Session Command Protocol
-> Runtime Event
-> State Projection
-> Context Projection
-> Provider / Tool Loop
-> Verification
-> LoopDecision
-> Projection
   -> User Result
   -> Debug / Audit
   -> Replay / Evaluation
   -> Improvement
```

架构收敛原则是：

```text
所有能力都必须围绕 Runtime Event、State Projection、Context Projection、LoopDecision 和多投影观测展开。
```

如果一个能力无法说明它产生什么 runtime fact、改变什么 state projection、是否影响 context projection、是否参与 Verification / LoopDecision / PromotionGate，它就不进入核心，只作为插件或实验能力。

后续治理增强对 Golutra 有两个直接提醒：

- Planning Drift 不能只靠任务结束时检查。必须在运行中持续检查当前计划、工具动作和原始目标是否仍然一致。
- Cost Explosion 不能只靠全局 token 上限。必须对验证、审计、索引、上下文构造和离线评估分级，否则完整观测会拖垮普通任务。

当基础 runtime 稳定后，Golutra 的治理控制层可以从“事件 + 循环判断”升级为：

```text
GoalLedger
-> RuntimeGovernor
-> LoopDecision
```

这些能力属于后续治理增强，不是第一阶段同步链路的必做项。第一阶段只保留扩展位，避免过早引入额外判断、索引和模型评估成本。

## 阶段分层

为避免把目标态误读成第一阶段必做，Golutra 按三层理解：

| 层级 | 说明 | 当前状态 |
| --- | --- | --- |
| 目标态 | 完整 Runtime OS，包含多投影、多入口、改进闭环、回放与治理增强 | 架构方向 |
| 第一阶段 | coding agent 主场景下的单 agent、单 active task、强 verification、可 replay 核心 runtime | 当前实现目标 |
| 后续增强 | GoalLedger、RuntimeGovernor、VerificationTier、EventSamplingPolicy、ContextProjectionCache、自动晋升等 | 后续演进 |

阅读原则：

- `ARCHITECTURE.md` 描述目标态与稳定边界。
- `implementation-blueprint.md` 决定第一阶段真正要做什么。
- 其他专题文档默认写目标态，但如果与第一阶段范围冲突，以 `implementation-blueprint.md` 为准。

## 主架构边界

主架构只保留最稳定的骨架与边界，支持层和未来治理细节分别下沉到专题文档：

- Agent 核心是 runtime，不是 prompt 包装器。CLI、TUI、API、SDK 都要进入同一套 runtime loop。
- 任务完成必须由 `VerificationRecord` 判定，不能只看模型自然语言。
- `ProviderContract`、`ToolContract`、`PolicyEvaluation`、`ArtifactRecord`、`EvidenceRecord` 属于支持层，细节见 `implementation-blueprint.md` 和观测/记忆专题文档。
- `GoalLedger`、`RuntimeGovernor`、`VerificationTier`、`EventSamplingPolicy`、`ContextProjectionCache` 只作为后续治理增强入口，不进入第一阶段主链路。
- 多入口只共享同一套 session protocol，入口层不能各自实现状态机。
- 长期 memory 是受治理的 durable state，不是直接回灌完整历史。

## Runtime-First 多前端边界

Golutra 要支持的不是“多个前端各跑一套 agent”，而是“一个 runtime 被多个前端同时观察和驱动”。

统一边界如下：

- 同一 `workspace_id + session_id + task_id` 只能有一份 runtime 真相，来源是 `RuntimeEventLog + StateProjection`。
- SDK、TUI、Web、IDE、API 只能通过统一协议访问 runtime，不能各自维护私有任务状态。
- 一个前端提交 `SessionCommand` 后，其他已附着到同一 session/task 的前端应该看到同样的运行状态变化。
- 流式输出也属于共享 runtime 事件；差异只允许出现在 projection 和渲染层，不允许出现在任务事实层。
- daemon 不是额外的一套业务接口，只是 `RuntimeCore` 的一种 host / transport 承载方式。

这意味着下面这种场景必须成立：

```text
SDK 正在执行某个 workspace 里的 task
-> TUI attach 到同一个 session / task
-> Web 也 attach 到同一个 session / task
-> 三端看到同一个 running 状态、同一组工具进度、同一条流式输出
-> 如果其中一端发起 approve / abort，其他端也能看到同一条状态变化
```

## Coding Agent 默认约束

Golutra 当前主场景是 coding agent，不按通用 agent 平台做第一阶段收敛。默认约束如下：

- 资源层级采用 `workspace -> session -> task -> turn`。
- `workspace` 表示一个代码仓库或工作目录。
- `session` 表示绑定某个 workspace 的长期工作会话，可承载多个历史 task。
- `task` 表示一次明确的编码目标，例如修 bug、加功能、做重构。
- `turn` 表示 task 执行过程中的单步交互或单次模型推进。
- 一个 `session` 同时只允许一个 `active task`，避免文件、命令、测试和 git 状态互相污染。
- 多前端可以 attach 到同一个 `session/task`，但同一时刻只允许一个 `active controller` 发起新的 prompt。
- 其他已 attach 前端默认为 observer，只共享状态、流式输出和工具进度。
- `approve`、`deny`、`abort`、`takeover` 这类控制动作必须写入 runtime event，不能走前端私有逻辑。

## RuntimeLane 与运行中输入

同一 `session/task` 必须有一个串行执行域，称为 `RuntimeLane`。它解决的是：任务执行中用户又输入、取消、接管或补充约束时，runtime 应该如何处理，而不是让入口层各自猜。

第一阶段默认策略：

- `append`：当前 task 正在运行时，把新输入排到后续 turn。
- `inject`：当前 loop 尚未进入不可中断工具副作用时，把用户补充合并到下一次 provider call 前。
- `interrupt`：取消当前 turn，写入 cancellation event，再由新输入接管执行。
- `reject`：非 active controller 或不满足安全边界的新输入直接拒绝，但要返回可解释原因。

约束：

- `RuntimeLane` 是 runtime 状态，不是 TUI / SDK 私有队列。
- `inject` 只能在安全边界处发生，不能打断正在执行的文件写入、shell、网络或外部系统副作用。
- 所有 busy policy 决策都必须写入 `RuntimeEvent`，并进入 `StateProjection`。
- `interrupt` 与 `abort` 都必须走 `CancellationContract`，不能只停止 UI stream。

## 四个核心系统

### Runtime Loop

负责一轮任务如何运行、是否继续、是否压缩、是否重试、是否 fallback、是否验证、是否结束。

关键要求：

- 模型不能单独决定任务完成。
- `VerificationRecord` 先于最终 `LoopDecision`，任务完成必须先被证据证明。
- provider fallback 必须发生在 loop 层，不能藏在 provider adapter。
- `LoopDecision` 只记录继续、压缩、重试、fallback、询问用户和结束原因。
- 任务终止必须有 `VerificationRecord` 或明确的失败 / 阻塞原因。

### Durable State

负责系统事实、运行轨迹、恢复、审计和回放。

核心存储：

```text
SQLite state
Durable event log
Artifact store
rg-backed content search
State snapshot
Replay timeline
```

关键要求：

- transcript 不是系统状态。
- UI 展示事件和 durable runtime event 必须分离。
- 大工具输出、diff、日志、网页内容默认进入 artifact，不直接进入 prompt。
- 任意 turn 都应该能通过 event + state + artifact 恢复和 replay。

### Context & Memory

负责模型能看到什么、历史如何压缩、长期记忆如何注入。

核心对象：

```text
ContextBuilder
TokenBudgetTracker
WorkingSummary
CompactManager
CompactionRecord
MemoryRetriever
MemoryGovernance
```

关键要求：

- 模型输入是投影结果，不是完整历史回灌。
- 历史分为 hot / warm / cold。
- compact 是 durable state transition，不是普通聊天总结。
- 长期 memory 必须有 evidence、scope、confidence、expiry、contradiction check 和 rollback。

详细规则见 `context-memory.md`。

### Governance

负责权限、安全、证据、验证、复盘和能力晋升，但不作为第一阶段主链路展开。

专题文档分工：

- `evaluation-observability.md`：观测、验证、复盘、EvaluationCase / EvaluationRun / Scorer / benchmark。
- `agent-improvement-loop.md`：失败轨迹如何变成可验证、可回滚的 agent 改进。
- `agent-open-endedness-design.md`：开放式能力、技能晋升和 Promotion Gate。
- `runtime-contracts.md`：tool/provider/terminal/cancel/retry/fallback 的硬契约。
- `artifact-evidence-ledger.md`：artifact 与 evidence 的事实层。
- `benchmark-hardening.md`：benchmark 元数据、judge 风险和防跑分规则。

## 完整链路

```text
1. 用户从 CLI / TUI / API / SDK 输入请求
2. Entry Layer 转成 SessionCommand
3. Host Runtime 创建 Session / Turn / GoalState
4. RuntimeLane 根据 busy policy 判断 append / inject / interrupt / reject
5. Runtime 写入 input event 和 turn snapshot
6. ContextBuilder 根据 state、summary、memory、evidence 构造模型输入，并生成 TokenBudgetSnapshot
7. Provider Router 根据 CapabilityMatrix 和预算选择模型
8. Provider 返回 assistant message / tool calls / usage / raw events，ProviderContract 归一化为 TokenUsageRecord
9. Tool System 校验 schema、权限、sandbox 和资源访问
10. ToolResultEnvelope 写入 summary、structured facts、artifact ref、evidence refs
11. Verification 判断任务是否达成、证据是否可靠、是否违反 policy
12. LoopGuard 与 LoopDecision 判断 continue / compact / retry / fallback / ask_user / stop
13. WorkspaceCheckpoint 在文件副作用后生成可恢复快照
14. 任务结束后按需生成 PostTaskReview 和可选 ImprovementCandidate
15. Projection Layer 按用途生成 User / Runtime Control / Debug / Evaluation 四类投影
16. 后续治理增强可在第 5、6、9、11、12 步之间接入 GoalLedger、RuntimeGovernor、VerificationTier、EventSamplingPolicy 和 ContextProjectionCache
```

## 模块划分

| 模块 | 职责 |
| --- | --- |
| `golutra-core` | 核心协议与状态类型 |
| `golutra-runtime` | RuntimeLane、turn 状态机、loop 执行、LoopGuard、resume、compact、fallback |
| `golutra-event` | Durable / live-only 事件协议 |
| `golutra-context` | ContextBuilder、TokenBudgetTracker、TokenBudgetSnapshot、WorkingSummary、context projection |
| `golutra-memory` | MemoryRetriever、MemoryGovernance、memory promotion/rollback |
| `golutra-store` | SQLite、event log、artifact store、state snapshot、workspace checkpoint refs |
| `golutra-llm` | Provider adapter、CapabilityMatrix、routing、usage normalization、TokenUsageRecord |
| `golutra-tools` | ToolContract、tool registry、tool execution、ToolResultEnvelope |
| `golutra-governor` | 后续治理增强：GoalLedger、RuntimeGovernor、GovernanceDecision |
| `golutra-policy` | PermissionPolicy、PolicyEvaluation、workspace isolation |
| `golutra-verify` | verification runner、PASS/FAIL/PARTIAL、证据记录 |
| `golutra-eval` | EvaluationCase、EvaluationRun、Scorer、TrajectoryReplay、CounterfactualReplay、CausalComparison、benchmark、regression |
| `golutra-tui` | 只展示 runtime projection 的 TUI |
| `golutra-cli` | 薄 CLI 入口 |
| `golutra-app-server` | HTTP/WebSocket/SSE 入口 |
| `golutra-vis` | replay、trace、context、artifact 审计视图 |

## Host / Transport / Projection

Golutra 的多前端支持要按三层收敛，而不是为每个入口各造一套接口：

```text
Frontend
  -> Frontend SDK
  -> RuntimeClient
  -> Transport Adapter
  -> RuntimeHost
  -> RuntimeCore
  -> RuntimeEvent / RuntimeQuery
  -> Projection
```

各层职责：

- `RuntimeCore`：唯一执行核心，负责 loop、provider、tool、policy、verification。
- `RuntimeHost`：承载 `RuntimeCore`，管理 session 生命周期，对外暴露本地或远程访问方式。
- `Transport Adapter`：把统一协议映射到进程内调用、HTTP + SSE、WebSocket 或 IPC。
- `RuntimeClient`：前端统一客户端接口，屏蔽不同 transport。
- `Projection`：把同一批 runtime facts 渲染成 User / Debug / Evaluation 等不同视图。

核心约束：

- 语义接口只有一套：`SessionCommand`、`RuntimeEvent`、`RuntimeQuery`。
- transport 可以有多种，但不能改变任务语义。
- 任意前端都只能消费统一 projection 或原始 runtime event，不能直接读取内部可变状态。

## 用户模式

Golutra 的观测不是一条全部展示给用户的链路，而是同一批 runtime facts 的多种投影。

```text
Runtime Control Projection
  给 runtime 自己使用，用于 LoopDecision、context、verification、retry、fallback、compact。

User Projection
  给普通 CLI / TUI / API 用户使用，只展示进度、权限请求、最终结果和必要风险。

Debug / Audit Projection
  给开发者、人类审计者或其他 agent 使用，展开 event、policy、evidence、token、provider raw event、context projection。

Evaluation / Improvement Projection
  给离线 replay、benchmark、regression 和 agent 改进使用，用于分析失败并生成 memory / policy / skill / benchmark 候选。
```

普通用户模式只使用 `User Projection`：

- 展示输入、工具进度、权限确认、结果、必要风险。
- 不展示完整 trace、token 细节和内部决策记录。

Debug / Audit / Replay 模式使用 `Debug / Audit Projection`：

- 展示 runtime event、LoopDecision、PolicyEvaluation、EvidenceRecord、VerificationRecord、context projection、token budget、provider raw event。
- 用于调试、复盘、benchmark 和回归验证。

Evaluation / Improvement 模式使用 `Evaluation / Improvement Projection`：

- 离线或后台读取 durable event、artifact、context projection、verification 和 post review。
- 用于 replay 失败、生成 benchmark、比较 provider、验证 prompt/tool/schema/policy/runtime 修改。
- 不应阻塞普通用户返回，除非当前任务明确要求同步验证。

## 能力分层

必须核心：

- Session Protocol
- Runtime Loop
- Durable State
- Context Projection
- Artifact / Evidence
- Tool Contract
- Tool Permission / Sandbox
- Provider Contract / CapabilityMatrix
- Verification

强推荐核心：

- PostTaskReview
- Replay
- Debug Mode
- MemoryGovernance
- Evaluation Harness / EvaluationCase / Regression / CounterfactualReplay

高级演进：

- GoalLedger / RuntimeGovernor
- VerificationTier / EventSamplingPolicy / ContextProjectionCache
- Open-Endedness
- Dynamic Benchmark
- Skill Promotion
- Multi-Agent Orchestration
- Plugin Marketplace

## 反模式

- CLI/TUI 自己拼 prompt。
- 每轮回灌完整 transcript。
- provider adapter 私自 fallback。
- 工具输出原文直接进模型。
- memory 自动写入长期存储。
- 没有 verification 就声明任务完成。
- debug 信息污染普通用户返回。
- 多 agent 共享混乱上下文。

## 关联文档

- `agent-runtime-technology-selection.md`：语言、crate、workspace 和库选型。
- `runtime-contracts.md`：runtime 硬契约。
- `artifact-evidence-ledger.md`：artifact / evidence 事实层规格。
- `benchmark-hardening.md`：benchmark 防污染与元数据规范。
- `context-memory.md`：token、context、compact、memory 规格。
- `evaluation-observability.md`：观测、验证、复盘、benchmark 规格。
- `agent-improvement-loop.md`：失败轨迹如何变成可验证、可回滚的 agent 改进。
- `implementation-blueprint.md`：第一阶段实现蓝图、最小 schema 和同步/后台边界。
- `agent-open-endedness-design.md`：开放式能力和 Promotion Gate。
- `framework-comparison.md`：六个外部 agent 项目的架构影响。

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

核心主线是：

```text
User Input
-> Session Command Protocol
-> Goal Ledger
-> Runtime Event
-> State Projection
-> Context Projection
-> Runtime Governor
-> Provider / Tool Loop
-> LoopDecision
-> Verification
-> PostTaskReview
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

如果一个能力无法说明它产生什么 runtime fact、改变什么 state projection、是否影响 context projection、是否参与 RuntimeGovernor / LoopDecision / PromotionGate，它就不进入核心，只作为插件或实验能力。

后续治理增强对 Golutra 有两个直接提醒：

- Planning Drift 不能只靠任务结束时检查。必须在运行中持续检查当前计划、工具动作和原始目标是否仍然一致。
- Cost Explosion 不能只靠全局 token 上限。必须对验证、审计、索引、上下文构造和离线评估分级，否则完整观测会拖垮普通任务。

当基础 runtime 稳定后，Golutra 的治理控制层可以从“事件 + 循环判断”升级为：

```text
GoalLedger
-> RuntimeGovernor
-> LoopDecision
```

`GoalLedger` 负责保存原始目标、约束和成功标准；`RuntimeGovernor` 负责在每轮动作前后统一判断 goal drift、policy、risk、cost、verification tier 和 approval escalation；`LoopDecision` 负责把判断结果落实为继续、重试、压缩、询问用户、阻塞或结束。

这些能力属于后续治理增强，不是第一阶段同步链路的必做项。第一阶段只保留扩展位，避免过早引入额外判断、索引和模型评估成本。

## 四个核心系统

### Runtime Loop

负责一轮任务如何运行、是否继续、是否压缩、是否重试、是否 fallback、是否验证、是否结束。

核心对象：

```text
Session
Turn
GoalRecord
GoalState
LoopGuard
LoopDecision
GoalAlignmentCheck
GovernanceDecision
ProviderResult
ToolResultEnvelope
VerificationRecord
PostTaskReview
```

关键要求：

- 模型不能单独决定任务完成。
- provider fallback 必须发生在 loop 层，不能藏在 provider adapter。
- 第一阶段由 `LoopDecision` 记录继续、压缩、重试、fallback、询问用户和结束原因；后续再接入 `RuntimeGovernor`。
- 每轮结束必须生成 `LoopDecision`。
- 任务终止必须有 `VerificationRecord` 或明确的失败/阻塞原因。

### Durable State

负责系统事实、运行轨迹、恢复、审计和回放。

核心存储：

```text
SQLite state
Durable event log
Artifact store
FTS5 / tantivy index
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

负责权限、安全、证据、验证、复盘和能力晋升。

核心对象：

```text
GoalLedger
RuntimeGovernor
GoalAlignmentCheck
GovernanceDecision
PermissionPolicy
PolicyEvaluation
CostBudgetState
VerificationTier
EventSamplingPolicy
EvidenceRecord
VerificationRecord
PostTaskReview
FailureTaxonomy
PromotionGate
ChangeManifest
ImprovementCandidate
RegressionResult
PromotionDecision
```

关键要求：

- 工具副作用必须有权限、sandbox、artifact 和 evidence。
- 任务完成必须可验证，不能只看模型自然语言。
- 第一阶段只做基础验证和 debug/audit 扩展位；后续再加入 `VerificationTier`。
- 第一阶段完整保存轻量 runtime event；后续再加入 `EventSamplingPolicy` 控制索引和离线评估成本。
- 后续治理增强可加入 `GoalLedger`、`GoalAlignmentCheck` 和 `RuntimeGovernor`，用于长任务目标漂移控制。
- 复盘结果可以产生 memory、benchmark、policy、skill 候选，但不能直接晋升。
- agent 改进必须经过 ImprovementCandidate、RegressionResult 和 PromotionDecision。
- Open-Endedness 必须经过 sandbox、verification、regression 和 human gate。

详细规则见 `evaluation-observability.md`、`agent-improvement-loop.md` 与 `agent-open-endedness-design.md`。

`PostTaskReview` 分为两层：

- minimal review：同步生成，只记录任务 outcome、关键失败类型、证据质量和必要风险。
- deep review：后台或离线生成，用于 benchmark、memory、policy、skill 候选和 agent 改进。

改进闭环固定为：

```text
deep PostTaskReview
-> FailureTaxonomy
-> ImprovementCandidate
-> RegressionResult
-> PromotionDecision
```

## 完整链路

```text
1. 用户从 CLI / TUI / API / SDK 输入请求
2. Entry Layer 转成 SessionCommand
3. Host Runtime 创建 Session / Turn / GoalState
4. Runtime 写入 input event 和 turn snapshot
5. ContextBuilder 根据 state、summary、memory、evidence 构造模型输入
6. Provider Router 根据 CapabilityMatrix 和预算选择模型
7. Provider 返回 assistant message / tool calls / usage / raw events
8. Tool System 校验 schema、权限、sandbox 和资源访问
9. ToolResultEnvelope 写入 summary、structured facts、artifact ref、evidence refs
10. Verification 判断任务是否达成、证据是否可靠、是否违反 policy
11. LoopGuard 与 LoopDecision 判断 continue / compact / retry / fallback / ask_user / stop
12. 任务结束后生成 PostTaskReview 和可选 ImprovementCandidate
13. Projection Layer 按用途生成 User / Runtime Control / Debug / Evaluation 四类投影
14. 后续治理增强可在第 4、5、8、10、11 步之间接入 GoalLedger、RuntimeGovernor、VerificationTier、EventSamplingPolicy 和 ContextProjectionCache
```

## 模块划分

| 模块 | 职责 |
| --- | --- |
| `golutra-core` | 核心类型：Message、SessionState、GoalState、LoopDecision、ToolResultEnvelope、Policy |
| `golutra-runtime` | turn 状态机、loop 执行、LoopDecision、resume、compact、fallback、post review |
| `golutra-event` | ProviderRawEvent、RuntimeEvent、UiSdkEvent、durable/live-only 事件协议 |
| `golutra-context` | ContextBuilder、TokenBudgetTracker、WorkingSummary、CompactManager、context projection |
| `golutra-memory` | MemoryRetriever、MemoryGovernance、memory promotion/rollback、项目索引 |
| `golutra-store` | SQLite、event log、artifact store、FTS5、migration、snapshot |
| `golutra-llm` | ProviderConfig、ModelCatalog、CapabilityMatrix、adapter、routing、usage |
| `golutra-tools` | ToolSchema、ToolAccesses、tool registry、tool execution、ToolResultEnvelope |
| `golutra-governor` | 后续治理增强：GoalLedger、RuntimeGovernor、GoalAlignmentCheck、GovernanceDecision、cost/risk/approval gate |
| `golutra-policy` | PermissionPolicy、workspace isolation、路径/网络/命令策略 |
| `golutra-verify` | verification runner、PASS/FAIL/PARTIAL、证据记录；后续扩展 VerificationTier |
| `golutra-eval` | eval runner、trajectory recorder、vcr/golden fixture、post-task review |
| `golutra-tui` | ratatui/crossterm TUI，只展示 runtime projection |
| `golutra-cli` | 薄 CLI 入口 |
| `golutra-app-server` | HTTP/WebSocket/SSE 入口 |
| `golutra-vis` | replay、trace、context、artifact 审计视图 |

## 用户模式

Golutra 的观测不是一条全部展示给用户的链路，而是同一批 runtime facts 的多种投影。

```text
Runtime Control Projection
  给 runtime 自己使用，用于 LoopDecision、context、verification、retry、fallback、compact。
  每次任务都必须生成最小必要数据。

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

- Runtime Loop
- Durable State
- Context Projection
- Tool Permission / Sandbox
- Provider Contract / CapabilityMatrix
- Verification

强推荐核心：

- PostTaskReview
- Replay
- Debug Mode
- MemoryGovernance
- Evaluation Harness

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
- `context-memory.md`：token、context、compact、memory 规格。
- `evaluation-observability.md`：观测、验证、复盘、benchmark 规格。
- `agent-improvement-loop.md`：失败轨迹如何变成可验证、可回滚的 agent 改进。
- `implementation-blueprint.md`：第一阶段实现蓝图、最小 schema 和同步/后台边界。
- `agent-open-endedness-design.md`：开放式能力和 Promotion Gate。
- `framework-comparison.md`：六个外部 agent 项目的架构影响。

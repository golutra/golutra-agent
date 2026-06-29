# file 文件夹情报汇总与架构建议

## 文档定位

本文档汇总 `/Users/skyseek/Desktop/project/open/golutra-agent/file` 下的 Agent 情报日报、周报、月报和索引，目的不是重复罗列每条情报，而是把这些调查结果转成 Golutra Agent Runtime 后续架构修改建议。

当前结论：Golutra 现有 docs 的主方向正确，不建议推翻；需要补强的是 runtime 硬契约、artifact/evidence、benchmark hardening、policy/sandbox 和 API/session protocol 这几类落地边界。

## 数据概况

截至 2026-06-29：

| 项目 | 当前状态 |
| --- | --- |
| 索引文件 | `file/index.md`、`file/index.jsonl` |
| 高信号条目 | 242 条 |
| 唯一 `dedupe_key` | 242 个，无重复 |
| 学术与 Labs | 82 条 |
| 开源与 Benchmark | 79 条 |
| 失败案例与实验线索 | 76 条 |
| 未标 section 的早期条目 | 5 条 |
| 证据强度：高 | 143 条 |
| 证据强度：中高 | 79 条 |
| 证据强度：中 | 20 条 |

主要输入文档：

- `file/index.md`
- `file/index.jsonl`
- `file/reademe.md`
- `file/WeeklyReports/2026/06/2026-06-agent-intel-monthly.md`
- `file/WeeklyReports/2026/06/*/*daily.md`
- `file/WeeklyReports/2026/06/*/*weekly.md`

## 总体判断

`file/` 里的调查结果反复指向同一个趋势：Agent 开发的竞争点正在从模型能力和 prompt 技巧，转向 runtime、状态、权限、恢复、评测、证据、artifact 和真实工作流。

换句话说，强 Agent 系统不是一个更复杂的 prompt，而是一套可恢复、可验证、可审计、可治理的运行时系统。

这与 Golutra docs 当前的主线一致：

```text
RuntimeEvent
-> StateProjection
-> ContextProjection
-> ToolResultEnvelope
-> VerificationRecord
-> LoopDecision
-> PostTaskReview
-> Replay / Evaluation / Improvement
```

因此，现有架构无需推倒重写。下一步应该把“抽象正确”推进到“契约可实现、可测试、可验收”。

## 从 file 情报中提炼出的 10 个关键信号

### 1. Runtime 边界成为主战场

OpenHands、Cline、Skyvern、CrewAI、LangGraph 高频变更集中在 conversation limits、sandbox、checkpoint、rollback、token budget、tool output cap、cancel propagation、session gate、run-scoped artifact、error-chain retention。

对 Golutra 的启示：

- runtime core 是产品核心，不是 CLI 的辅助层。
- `LoopDecision`、`RuntimeEvent`、`ToolResultEnvelope` 必须成为硬协议。
- 取消、超时、重试、fallback、pause、resume、abort 都必须有状态机语义。

### 2. 终止语义不能交给模型自然语言

Skyvern 的 structured output verification、typed terminal criteria、atomic terminal barrier、honest turn-halt 等条目说明，Agent 说“完成了”不等于任务完成。

对 Golutra 的启示：

- 任务结束必须绑定 `VerificationRecord`。
- `stop_success`、`stop_partial`、`stop_failed` 要有结构化原因和证据。
- 终止前要检查 artifact、tool result、用户目标和残余风险。

### 3. 工具契约比工具数量更重要

ToolMaze、ContractBench、Unreliable Feedback、Cline JSON-like tool input normalization、tool output truncation、AutoGen MCP cleanup 都说明：工具失败常常来自契约不清，而不是模型不会调用工具。

对 Golutra 的启示：

- 每个工具必须声明输入 schema、输出 schema、错误语义、取消语义、幂等语义、副作用策略、artifact 策略。
- 工具输出默认结构化，raw output 默认进 artifact。
- misleading observation 需要作为一类 failure taxonomy。

### 4. Artifact 和 evidence 是 Agent 系统的事实层

EnterpriseClawBench、Skyvern run-scoped downloads、evidence-backed completion、BenchJack answer leakage 都提示：没有 artifact provenance，评估和回放都会失真。

对 Golutra 的启示：

- `ArtifactStore` 应该有独立规格，不只是 storage 实现细节。
- artifact 必须记录来源、run scope、checksum、secret scrub、retention、引用关系。
- `EvidenceRecord` 应明确支撑哪些 verification、memory、review、benchmark。

### 5. Benchmark 需要 hardening，不只是跑分

BenchJack、HarnessFix、GAIA scaffold、EnterpriseClawBench、Co-Failure Ceiling 说明 benchmark 容易被 scaffold、harness、答案泄漏、judge 偏差和 all-wrong tail 污染。

对 Golutra 的启示：

- benchmark 报告必须记录 `scaffold_id`、`harness_version`、`tool_budget`、`attempt_count`、`cost`、`runtime`、`artifact_delivery`。
- 要防 test-hook injection、answer leakage、judge input pollution。
- leaderboard 分数不能单独作为能力结论。

### 6. 记忆不是存更多文本，而是状态系统

StreamMemBench、GitOfThoughts、Agent-native Memory、MEMPROBE、Memory Depth、JERP 指向：长期记忆需要版本化、diff、merge、压缩、过期、审计、恢复隐藏用户状态。

对 Golutra 的启示：

- `MemoryCandidate` 不能直接晋升为长期 memory。
- memory 必须有 evidence、scope、confidence、expiry、contradiction、rollback。
- 需要区分 retrieval recall、memory depth、post-unload recovery、stale memory invalidation。

### 7. 安全检查正在从 prompt 层前移到设计和执行层

Design-time Verification、Probabilistic Verification、OSGuard、DeepMind AI Control Roadmap、CrewAI SSRF 修复、Skyvern session-bound CodeBlock runner 都说明：prompt 拒绝不是安全边界。

对 Golutra 的启示：

- workflow / subagent graph 需要 design-time lint。
- network、redirect、secret、credential forwarding 要有 policy gate。
- side-effect tool 默认应有 halt-on-failure 策略。

### 8. Provider 适配是反腐层，不是简单 HTTP 封装

Skyvern OpenRouter truncated output、Cline sticky session cache billing、provider request token budgeting、fetch error cause chain 都说明 provider 差异会直接污染成本、上下文和失败诊断。

对 Golutra 的启示：

- ProviderContract 必须标准化 stream event、tool call、usage、reasoning token、error、rate limit、cost。
- fallback 归 loop 层，不允许 provider adapter 私自切换语义。
- provider adapter 需要 recorded/golden tests。

### 9. 多 agent 不能先堆角色，必须先定通信和状态契约

Multiagent Protocols、Contagion Networks、Co-Failure Ceiling、OrchRM、Recursive Agent Harnesses 说明多 agent 的核心风险是身份、消息因果、共享状态、judge 偏差和 orchestration reward。

对 Golutra 的启示：

- 第一阶段不做复杂 multi-agent 是正确的。
- 但现在应先定义未来禁止项：不允许无因果 ID 的跨 agent 消息，不允许共享裸 context，不允许无 claim/lock 的任务认领。

### 10. 自改进必须先有回归门禁

TRACE、SkillAxe、SkillCAT、AgentFixer、Probe-and-Refine、agent-improvement-loop 相关条目说明，自动改 prompt、skill、policy、memory 很容易修症状不修根因。

对 Golutra 的启示：

- `ImprovementCandidate` 只能是候选。
- 晋升前必须跑 replay、benchmark、regression、cost/latency delta 和 rollback plan。
- prompt / skill / memory / policy 的改动都应该有 `PromotionDecision`。

## 对现有 docs 的评价

### 已经做对的部分

- `ARCHITECTURE.md` 把 Golutra 定位为 Runtime OS，方向正确。
- `implementation-blueprint.md` 把第一阶段收敛为单 agent、多入口、可恢复、可验证、可 debug，范围控制正确。
- `context-memory.md` 已经区分 context、compaction、memory governance，没有把 transcript 当状态。
- `evaluation-observability.md` 已经把 verification、replay、failure taxonomy 放到核心位置。
- `agent-improvement-loop.md` 已经明确复盘不能直接改 agent，必须经过候选、回归和晋升。
- `agent-runtime-technology-selection.md` 的 Rust-first、thin CLI/TUI/API/SDK、provider adapter 反腐层方向正确。

### 当前主要缺口

| 缺口 | 影响 | 建议 |
| --- | --- | --- |
| P0/P1/P2/P3 分层不够醒目 | 读者容易把后续治理增强误认为第一阶段必做 | 在 `ARCHITECTURE.md` 增加阶段分层表 |
| Tool contract 没有独立文档 | 工具副作用、幂等、取消、截断、artifact 规则容易散落 | 新增 `runtime-contracts.md` |
| Artifact / Evidence 没有独立规格 | replay、verification、memory、benchmark 的事实来源不够硬 | 新增 `artifact-evidence-ledger.md` |
| Benchmark hardening 缺失 | 容易把污染分数当真实能力 | 新增 `benchmark-hardening.md` |
| Policy / sandbox / security 偏散 | SSRF、secret、network、side effect 没有统一边界 | 新增 `policy-sandbox-security.md` |
| Provider contract 埋在技术选型里 | provider 差异会污染 usage、cost、tool call 和 error | 在 `runtime-contracts.md` 或单独 provider 文档中硬化 |
| API / Session Protocol 不够具体 | CLI/TUI/API/SDK 可能重复实现状态机 | 新增 `api-session-protocol.md` |
| Memory 需要吸收 06 月新情报 | 长期记忆仍偏 candidate/promotion，没有完整 memory audit | 修改 `context-memory.md` |
| Evaluation 需要吸收 benchmark hardening 和 judge 风险 | 当前 eval 文档偏观测，hardening 不够 | 修改 `evaluation-observability.md` |

## 建议新增文档

### 1. `runtime-contracts.md`

优先级：P0。

目的：把 runtime 最容易出事故的边界写成硬契约。

建议包含：

- `ToolContract`
- `ProviderContract`
- `TerminalStateContract`
- `CancellationContract`
- `IdempotencyContract`
- `SideEffectPolicy`
- `TimeoutPolicy`
- `RetryPolicy`
- `FallbackPolicy`
- `StructuredOutputContract`

最小 schema 建议：

```text
ToolContract
  tool_name
  input_schema
  output_schema
  error_schema
  idempotency_key_policy
  side_effect_type: none | file | process | network | external_system
  timeout_policy
  cancellation_policy
  retry_policy
  artifact_policy
  permission_policy_ref
  sandbox_policy_ref
```

```text
ProviderContract
  provider_id
  model_id
  native_protocol
  stream_event_mapping
  tool_call_mapping
  usage_mapping
  reasoning_mapping
  finish_reason_mapping
  error_mapping
  rate_limit_mapping
  cost_model
  capability_matrix_ref
  golden_fixture_refs
```

### 2. `artifact-evidence-ledger.md`

优先级：P0。

目的：定义 artifact 和 evidence 的事实层，服务 replay、verification、memory、benchmark 和 audit。

建议包含：

- `ArtifactRecord`
- `EvidenceRecord`
- artifact scope：session / turn / tool_call / run / benchmark
- checksum 和 content-addressing
- secret scrub 和 redaction
- retention policy
- run-scoped artifact isolation
- artifact-to-verification linkage
- artifact-to-memory linkage
- artifact-to-benchmark linkage

最小 schema 建议：

```text
ArtifactRecord
  artifact_id
  run_id
  session_id
  turn_id
  tool_call_id
  artifact_type
  uri
  checksum
  size_bytes
  created_at
  producer
  redaction_status
  retention_policy
  provenance_refs
```

```text
EvidenceRecord
  evidence_id
  claim
  artifact_refs
  source_event_refs
  evidence_strength
  verifier
  confidence
  limitations
```

### 3. `benchmark-hardening.md`

优先级：P1。

目的：把 06 月情报里的 BenchJack、HarnessFix、GAIA scaffold、EnterpriseClawBench、Co-Failure Ceiling 等结论固化为评测规范。

建议包含：

- benchmark metadata 必填项
- scaffold / harness / protocol version
- answer leakage 检查
- test-hook injection 检查
- judge input sanitization
- no-feedback fallback
- all-wrong tail
- evaluator bias propagation
- artifact delivery / visual quality / runtime / cost
- benchmark health report

最小 schema 建议：

```text
BenchmarkRun
  benchmark_id
  dataset_version
  harness_version
  scaffold_id
  scaffold_version
  model_id
  provider_id
  tool_budget
  attempt_count
  runtime_ms
  cost_usd
  artifact_delivery_status
  score
  failure_taxonomy
  leakage_checks
  judge_checks
```

### 4. `policy-sandbox-security.md`

优先级：P1。

目的：统一权限、安全和 sandbox 的边界，避免散落在多个文档里。

建议包含：

- path policy
- process policy
- network policy
- redirect / SSRF policy
- secret / credential policy
- BYOK / managed key policy
- approval escalation
- side-effect halt policy
- session-bound execution
- sandbox profile
- design-time workflow lint

最小 schema 建议：

```text
PolicyEvaluation
  policy_ref
  subject
  action
  resource
  context
  decision: allow | ask | deny | block
  reason
  evidence_refs
  escalation
```

### 5. `api-session-protocol.md`

优先级：P1。

目的：保证 CLI、TUI、API、SDK 不重复实现状态机。

建议包含：

- `SessionCommand`
- `SessionEvent`
- `RuntimeEvent` API 视图
- pause / abort / resume / compact / verify / replay
- SSE / WebSocket event shape
- SDK type generation
- idempotent prompt submission
- pagination / cursor
- error shape

最小命令建议：

```text
session.create
session.prompt
session.abort
session.pause
session.resume
session.compact
session.verify
session.replay
session.debug
session.export
```

## 建议修改现有文档

### 修改 `ARCHITECTURE.md`

新增一节：`架构阶段分层`。

建议分层：

| 阶段 | 内容 | 是否第一阶段必做 |
| --- | --- | --- |
| P0 Runtime Kernel | SessionCommand、RuntimeEvent、StateProjection、ContextProjection、ToolResultEnvelope、LoopDecision、VerificationRecord | 是 |
| P1 Hard Contracts | ToolContract、ProviderContract、Artifact/Evidence、PolicyEvaluation、Benchmark metadata | 是，至少写规格和最小实现 |
| P2 Governance | GoalLedger、RuntimeGovernor、GoalAlignmentCheck、VerificationTier、EventSamplingPolicy | 否，保留扩展位 |
| P3 Evolution | Open-Endedness、Skill Promotion、Dynamic Benchmark、自动课程 | 否，只做候选和门禁 |

同时把 `GoalLedger -> RuntimeGovernor -> LoopDecision` 明确标成 P2，避免读者误解为 P0 必做。

### 修改 `implementation-blueprint.md`

建议新增：`P0 验收测试矩阵`。

至少覆盖：

- tool call success / error / cancelled / timeout
- provider stream success / truncated / rate limit / malformed event
- abort 后无后续 side effect
- retry 不重复写副作用
- compact 后可 replay
- artifact 可引用、可校验 checksum
- VerificationRecord 能阻止 false success
- DebugProjection 能定位失败 turn

### 修改 `context-memory.md`

吸收 06 月情报：StreamMemBench、GitOfThoughts、Agent-native Memory、MEMPROBE、Memory Depth、JERP。

建议新增：

- memory versioning
- memory diff / merge
- stale memory invalidation
- hidden user state recovery audit
- post-unload recovery
- memory depth vs retrieval recall
- rule-policy drift

### 修改 `evaluation-observability.md`

吸收 06 月情报：BenchJack、HarnessFix、Grading the Grader、SHERLOC、OpenRCA 2.0、Ask Don't Judge、Co-Failure Ceiling、Contagion Networks。

建议新增：

- benchmark hardening summary
- process-level evidence
- causal path supervision
- binary question decomposition
- judge artifact reliability
- no-feedback fallback
- evaluator bias propagation
- all-wrong tail

### 修改 `agent-runtime-technology-selection.md`

建议把 provider 部分拆成：

- `ProviderContract`：核心协议。
- `ProviderAdapter`：实现适配。
- `ProviderGoldenTests`：升级防回归。
- `CapabilityMatrix`：能力声明。

现在写法偏技术选型，建议进一步强调 provider 是反腐层。

### 修改 `README.md`

把新增文档加入推荐阅读顺序：

1. `ARCHITECTURE.md`
2. `implementation-blueprint.md`
3. `runtime-contracts.md`
4. `artifact-evidence-ledger.md`
5. `policy-sandbox-security.md`
6. `api-session-protocol.md`
7. `benchmark-hardening.md`
8. `agent-runtime-technology-selection.md`
9. `context-memory.md`
10. `evaluation-observability.md`
11. `agent-improvement-loop.md`
12. `agent-open-endedness-design.md`
13. `framework-comparison.md`
14. `file文件夹汇总.md`

## 建议优先级

### 立即做

1. 在 `ARCHITECTURE.md` 补 P0/P1/P2/P3 分层。
2. 新增 `runtime-contracts.md`。
3. 新增 `artifact-evidence-ledger.md`。
4. 在 `implementation-blueprint.md` 补 P0 验收测试矩阵。

原因：这些直接决定第一阶段能否落地，不是后续锦上添花。

### 第二批做

1. 新增 `policy-sandbox-security.md`。
2. 新增 `api-session-protocol.md`。
3. 新增 `benchmark-hardening.md`。
4. 修改 `evaluation-observability.md`，吸收 benchmark hardening 和 judge 风险。

原因：这些决定系统进入真实任务、真实安全边界和真实评测时是否可信。

### 第三批做

1. 修改 `context-memory.md`，吸收 memory depth、GitOfThoughts、MEMPROBE。
2. 修改 `agent-open-endedness-design.md`，补 JERP / rule-policy drift / self-compaction 风险。
3. 修改 `agent-improvement-loop.md`，补 prompt / skill / memory / policy 改动的 regression matrix。

原因：这些能力重要，但不应该抢在 runtime contract 前面。

## 不建议现在做的事

- 不建议现在实现复杂多 agent orchestration。
- 不建议现在做自动 self-improvement。
- 不建议现在引入外部向量数据库作为基础依赖。
- 不建议把 LangChain / CrewAI / AutoGen 这类框架作为 Golutra 主架构。
- 不建议把完整治理层 `RuntimeGovernor` 放进第一阶段同步链路。
- 不建议把所有 trace 都同步做深度分析，否则成本和延迟会过早失控。

## 推荐的第一阶段验收口径

第一阶段完成，不应以“能和模型聊天、能调用工具”为标准，而应以以下能力为标准：

- 任意 turn 都有 durable `RuntimeEvent`。
- 任意工具调用都有 `ToolResultEnvelope`。
- 任意任务停止都有 `LoopDecision`。
- 成功、失败、部分完成都有 `VerificationRecord` 或明确阻塞原因。
- 大输出、diff、日志、下载、截图都进入 artifact，不直接污染 prompt。
- abort / timeout / cancel 不会继续产生副作用。
- replay 能重建关键 state 和 artifact 引用。
- debug projection 能定位失败 turn、tool、provider、policy、verification。
- provider fallback 由 loop 层控制，adapter 不私自改变任务语义。
- minimal PostTaskReview 能产出 failure taxonomy 和必要 next action。

## 最后判断

Golutra docs 当前最有价值的判断是：把 Agent 当 Runtime OS，而不是当 prompt 编排器。这个判断应该保留。

接下来要补的不是更多概念，而是更硬的工程边界：

```text
ToolContract
ProviderContract
ArtifactRecord
EvidenceRecord
PolicyEvaluation
BenchmarkRun
SessionCommand
VerificationRecord
LoopDecision
```

只要这些边界做硬，Golutra 后续无论接 CLI、TUI、API、SDK、多 agent、memory、benchmark 还是 open-endedness，都不会把核心状态机写散。

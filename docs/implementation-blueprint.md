# Golutra 实现蓝图

## 文档定位

本文档把 `ARCHITECTURE.md` 的目标架构收敛成可落地的工程蓝图，回答：

```text
第一阶段先实现什么，
哪些能力同步运行，
哪些能力后台或离线运行，
核心数据结构至少长什么样。
```

## 第一阶段目标

第一阶段不追求完整开放式演进，也不追求复杂多 agent。目标是跑通单 agent、多入口、可恢复、可验证、可 debug 的核心 runtime。

主场景默认按 coding agent 收敛：

- `workspace -> session -> task -> turn`
- 一个 `session` 同时只有一个 `active task`
- 多前端可 attach 同一 `session/task`
- 同时只有一个 `active controller`
- 其他端默认 observer

必须完成：

```text
SessionCommand
RuntimeEvent
StateProjection
ContextProjection
RuntimeQuery
RuntimeLane
BusyPolicyDecision
ProviderContract
ToolContract
ToolResultEnvelope
ArtifactRecord / EvidenceRecord
PolicyEvaluation
VerificationRecord
LoopDecision
LoopGuardRule
WorkspaceCheckpoint
UserProjection
DebugProjection
EvaluationCase
EvaluationRun
EvaluationResult
TokenBudgetSnapshot
TokenUsageRecord
TokenAttribution
CounterfactualReplay
CausalComparison
SecurityUtilityResult
```

暂不作为第一阶段核心：

```text
Open-Endedness
Dynamic Benchmark
Skill Promotion
GoalLedger / RuntimeGovernor
GoalAlignmentCheck
VerificationTier
EventSamplingPolicy
ContextProjectionCache
Plugin Marketplace
复杂 Multi-Agent Orchestration
自动修改 runtime 代码
```

第一阶段只生成 `ImprovementCandidate`，不自动应用改动。
`EvaluationCase`、`EvaluationRun` 和 `EvaluationResult` 第一阶段只作为离线评估的最小数据模型，不进入普通用户任务同步链路。
`CounterfactualReplay`、`CausalComparison` 和 `SecurityUtilityResult` 属于后续离线评估增强，第一阶段只保留 schema 方向，不阻塞 P0 runtime。

## 第一阶段吸收的架构启示

第一阶段不新增复杂治理层，但必须吸收以下 runtime 硬边界：

- `SessionCommand` 是 CLI / TUI / API / SDK 的唯一入口协议，入口层不能绕过 runtime 自建状态机。
- `RuntimeQuery` 是查询当前 session / task 状态的统一接口；不同前端不能各自维护私有状态快照作为真相。
- 协议类型必须有统一 schema 产物；Rust、TypeScript、Python 侧不能各自手写一套含义接近但字段漂移的契约。
- `ProviderContract` 是 provider 反腐层，统一 stream event、tool call、usage、finish_reason、error、rate limit 和 cost。
- Custom Provider 设置必须协议优先：先选 OpenAI-compatible / Anthropic-compatible / Gemini-compatible，再填写 base URL、API key、model、advanced config 和 review；当前 live adapter 未支持的协议必须明确拦截，不能显示为连接成功。
- `ToolContract` 先于工具实现定义，明确 schema、错误、取消、重试、幂等、副作用和 artifact 策略。
- `ArtifactRecord` / `EvidenceRecord` 是事实层，raw output 默认进 artifact，模型只读取受控摘要和 evidence refs。
- `VerificationRecord` 决定任务是否完成，模型自然语言不能直接触发成功终止。
- `PolicyEvaluation` 必须在执行层阻断高风险文件、进程、网络、secret 和外部副作用。
- `MemoryCandidate` 只作为候选，长期 memory 不能从 transcript 自动晋升。
- `RuntimeLane` 负责同一 task 的串行执行和运行中输入处理，入口层不能私自排队、注入或中断。
- `LoopGuardRule` 把重复工具失败、空回复、context overflow、max iteration 等循环异常变成显式规则。
- `WorkspaceCheckpoint` 负责 coding agent 文件副作用后的恢复点，不能污染用户自己的 `.git`。

这些是第一阶段的架构约束，不等于要实现完整 benchmark hardening、复杂 multi-agent、自改进或动态评测系统。

## 后续治理增强

以下能力有架构价值，但不适合放进第一阶段同步链路。原因是它们会增加 schema、判断逻辑、索引策略和额外模型调用，前期容易把 runtime 做复杂，也会浪费 token。

```text
GoalLedger
RuntimeGovernor
GoalAlignmentCheck
GovernanceDecision
VerificationTier
EventSamplingPolicy
ContextProjectionCache
```

建议放到后续大版本，触发条件是：基础 runtime 已经稳定，真实失败轨迹足够多，且确实观察到目标漂移、验证成本过高或上下文构造成本过高。

推荐落地顺序：

1. `VerificationTier`：先把验证分级做起来，收益最大，成本最低。
2. `EventSamplingPolicy`：再控制 debug/audit/eval 的索引和分析成本。
3. `ContextProjectionCache`：当 context 构造成为明确瓶颈时再做。
4. `GoalLedger + GoalAlignmentCheck`：长任务和多步骤任务稳定后再加。
5. `RuntimeGovernor + GovernanceDecision`：最后统一治理层，避免前期过早抽象。

前期只保留这些能力的扩展位：

- `LoopDecision.reason` 能记录目标偏移、预算超限、权限阻塞等原因。
- `VerificationRecord` 能记录检查来源和残余风险。
- `DebugProjection` 能展示 event、context、policy、verification。
- `PostTaskReview` 能把疑似 drift / cost / context 问题归入失败分类。

## Coding Agent 生命周期默认值

如果用户没有额外指定，第一阶段按以下语义实现：

- `workspace`：一个代码仓库或工作目录。
- `session`：绑定某个 workspace 的长期上下文容器，允许累积多个历史 task。
- `task`：一次明确用户请求，对应一次可 replay、可 verification、可 improvement 的执行轨迹。
- `turn`：task 内的一步推进，例如一次模型调用、一次用户补充或一次恢复动作。
- `resume` 默认恢复 `session`，并定位最近的 `active task` 或 latest task。
- `replay`、`debug`、`evaluation` 以 `task_id` 为主，不以原始 transcript 为主。

并发默认值：

- 一个 `session` 同时只允许一个 `active task`。
- 同一 workspace 可以存在多个 session，但第一阶段不鼓励共享同一个可写 working tree 并发执行。
- 多前端 attach 到同一 task 时，共享同一 `StateProjection` 和 `RuntimeEvent` 流。
- 新 prompt 只接受来自 `active controller`；其他前端默认只能观察，除非显式执行 `takeover`。
- active task 正在运行时，新输入必须通过 `RuntimeLane` 选择 `append`、`inject`、`interrupt` 或 `reject`，不能由 CLI/TUI/SDK 各自处理。
- `inject` 只允许在 provider call 前或工具安全间隙合并，不允许打断正在执行的副作用。

## 最小核心 Schema

### SessionCommand

```text
SessionCommand
  command_id
  session_id
  kind: create | prompt | approve | deny | pause | resume | abort | compact | verify | replay | export
  idempotency_key
  actor: user | api | tui | cli | sdk
  payload
  timestamp
```

### RuntimeEvent

```text
RuntimeEvent
  id
  session_id
  turn_id
  task_id
  parent_event_id
  event_type
  timestamp
  source: runtime | provider | tool | policy | verifier | user
  payload_ref
  durable: true | false
```

### RuntimeQuery

```text
RuntimeQuery
  query_id
  session_id
  task_id
  kind: session_state | task_state | user_projection | debug_projection | replay_cursor
  requester: user | api | tui | cli | sdk | web | ide
  cursor
  timestamp
```

### RuntimeLane

```text
RuntimeLane
  lane_id
  workspace_id
  session_id
  task_id
  active_turn_id
  active_controller
  status: idle | running | draining | cancelled
  pending_turns
  injected_inputs
  busy_policy_default: append | inject | interrupt | reject
```

### BusyPolicyDecision

```text
BusyPolicyDecision
  decision_id
  lane_id
  command_id
  requested_policy: append | inject | interrupt | reject
  applied_policy: append | inject | interrupt | reject
  reason
  safe_to_inject: true | false
  affected_turn_id
  event_ref
```

### ProviderContract

```text
ProviderContract
  provider_id
  model_id
  native_protocol
  stream_event_mapping
  tool_call_mapping
  usage_mapping
  finish_reason_mapping
  error_mapping
  rate_limit_mapping
  cost_model
  capability_matrix_ref
```

### TokenBudgetSnapshot

```text
TokenBudgetSnapshot
  task_id
  turn_id
  context_window
  max_output
  reserved_output_tokens
  planned_input_tokens
  planned_tool_tokens
  planned_summary_tokens
  budget_limit
  budget_policy
  action_if_exceeded: trim | compact | ask_user | block
```

### TokenUsageRecord

```text
TokenUsageRecord
  task_id
  turn_id
  provider_id
  model_id
  request_event_id
  response_event_id
  input_tokens
  output_tokens
  reasoning_tokens
  cached_input_tokens
  tool_result_tokens
  total_tokens
  estimated_cost
  budget_snapshot_ref
  attribution_ref
  usage_source: provider | estimated | unknown
```

`input_tokens` 包含 system prompt、developer/runtime instructions、policy constraints、user / assistant recent messages、context projection、working summary、memory、evidence summary、tool instructions 和 tool result excerpts 等所有进入 provider request 的模型可见内容。

### TokenAttribution

```text
TokenAttribution
  system_prompt_tokens
  developer_instruction_tokens
  runtime_context_tokens
  policy_tokens
  user_message_tokens
  assistant_recent_tokens
  working_summary_tokens
  memory_tokens
  evidence_tokens
  tool_instruction_tokens
  tool_result_excerpt_tokens
  output_tokens
  reasoning_tokens
  cached_input_tokens
  source: tokenizer | provider | mixed | unknown
```

### LoopDecision

```text
LoopDecision
  task_id
  turn_id
  action: continue | ask_user | compact | retry | fallback | verify | stop_success | stop_partial | stop_failed | blocked
  reason
  evidence_refs
  verification_ref
  policy_ref
  budget_state
  tool_state
  model_state
  next_step
```

### LoopGuardRule

```text
LoopGuardRule
  rule_id
  trigger: repeated_tool_failure | empty_response | context_overflow | max_iteration | retry_cost_exceeded | oversized_tool_output
  threshold
  action: nudge | compact | retry | fallback | ask_user | synthesize_final | blocked
  reason
```

第一阶段内置规则：

- 同一工具连续确定性失败时，不能无限重复同一调用；应要求换方法、询问用户或 blocked。
- provider 空回复时最多做有限恢复，恢复失败后不能写入污染历史。
- context overflow 优先裁剪旧工具输出和低价值上下文，裁剪失败进入 `LoopDecision`。
- max iteration 不应静默失败，必须生成可解释的 partial / failed / blocked 结果。
- retry 或 fallback 成本超过预算时，必须转为 ask_user 或 blocked。

### VerificationRecord

```text
VerificationRecord
  task_id
  objective
  completion_criteria
  checks
  evidence_refs
  result: pass | fail | partial | unknown
  policy_status
  residual_risks
```

### ToolContract

```text
ToolContract
  tool_name
  input_schema
  output_schema
  error_schema
  side_effect_type: none | file | process | network | external_system
  idempotency_key_policy
  timeout_policy
  cancellation_policy
  retry_policy
  artifact_policy
  permission_policy_ref
```

### ToolResultEnvelope

```text
ToolResultEnvelope
  tool_call_id
  tool_name
  status: ok | error | blocked | cancelled
  summary
  structured_facts
  model_visible_excerpt
  raw_artifact_ref
  evidence_refs
  risk
  verification_hint
```

### WorkspaceCheckpoint

```text
WorkspaceCheckpoint
  checkpoint_id
  workspace_id
  task_id
  turn_id
  checkpoint_type: shadow_git | snapshot | external
  changed_files
  artifact_refs
  created_after_event_id
  restore_hint
  retention_policy
```

第一阶段推荐使用 shadow-git 或等价独立快照机制。它只能作为 Golutra 的恢复网，不能修改用户自己的 `.git` 历史，也不能把被 `.gitignore` 或 policy 排除的敏感文件写入快照。

### ArtifactRecord

```text
ArtifactRecord
  artifact_id
  session_id
  turn_id
  tool_call_id
  artifact_type
  uri
  checksum
  size_bytes
  producer
  redaction_status
  retention_policy
```

### EvidenceRecord

```text
EvidenceRecord
  evidence_id
  claim
  artifact_refs
  source_event_refs
  evidence_strength
  verifier
  limitations
```

### PolicyEvaluation

```text
PolicyEvaluation
  policy_ref
  subject
  action
  resource
  decision: allow | ask | deny | block
  reason
  evidence_refs
```

### PostTaskReview

```text
PostTaskReview
  task_id
  mode: minimal | deep
  outcome
  failure_taxonomy
  evidence_quality
  suggested_improvements
  promotion_candidates
```

### EvaluationCase

```text
EvaluationCase
  case_id
  source: live_task | benchmark | regression | adversarial | manual
  task_type
  input_refs
  expected_outcome
  success_criteria
  required_evidence
  policy_constraints
  fixture_refs
  leakage_risk
  tags
```

### EvaluationRun

```text
EvaluationRun
  run_id
  dataset_id
  case_ids
  system_version
  candidate_ref
  provider_config_ref
  runtime_config_ref
  cost
  latency
  artifact_refs
  result_refs
```

### EvaluationResult

```text
EvaluationResult
  run_id
  case_id
  scorer_results
  verdict: pass | fail | partial | unknown
  quality_score
  cost
  latency
  failure_taxonomy
  evidence_refs
  residual_risks
```

### CounterfactualReplay

```text
CounterfactualReplay
  replay_id
  source_task_id
  baseline_config_ref
  variant_config_ref
  changed_layer: context | memory | tool_policy | provider_route | prompt | verification | token_policy | security_policy
  controlled_variables
  replay_mode: fixture | sandbox_live
  result_refs
  limitations
```

### CausalComparison

```text
CausalComparison
  comparison_id
  baseline_run_id
  variant_run_id
  changed_layer
  controlled_variables
  delta_quality
  delta_cost
  delta_latency
  delta_token_usage
  delta_tool_calls
  delta_security
  regressions
  confidence
  verdict: improved | regressed | mixed | inconclusive
```

### SecurityUtilityResult

```text
SecurityUtilityResult
  run_id
  case_id
  utility_score
  security_score
  policy_violations
  data_exfiltration_risk
  prompt_injection_signal
  unsafe_tool_use
  evidence_refs
  verdict: pass | fail | needs_review
```

### CompactionRecord

```text
CompactionRecord
  id
  session_id
  turn_id
  first_kept_entry_id
  summary
  dropped_raw_refs
  evidence_refs
  unresolved_items
  token_before
  token_after
  verification_status
```

### MemoryCandidate

```text
MemoryCandidate
  source_task_id
  evidence_ids
  proposed_scope: user | project | global
  confidence
  contradiction_ids
  expiry
  promotion_status
```

### ImprovementCandidate

```text
ImprovementCandidate
  id
  source_task_id
  source_failure_ids
  target_type: prompt | tool_schema | policy | memory | provider_route | context_rule | runtime_code
  target_id
  proposed_change
  expected_effect
  risk_level
  evidence_refs
  rollback_plan
  status: proposed | testing | rejected | promoted
```

### RegressionResult

```text
RegressionResult
  candidate_id
  baseline_version
  candidate_version
  cases_run
  regressions
  cost_delta
  latency_delta
  quality_delta
  verdict: pass | fail | needs_review
```

### PromotionDecision

```text
PromotionDecision
  candidate_id
  decision: approve | reject | needs_human_review
  reason
  reviewer: system | human | agent
  applied_version
  rollback_ref
```

## 同步、后台、离线边界

### 同步必跑

这些能力参与当前任务正确性，必须在用户任务链路中同步运行：

- SessionCommand 归一化。
- RuntimeEvent 写入。
- StateProjection 更新。
- RuntimeLane / BusyPolicyDecision。
- ContextProjection 构造。
- TokenBudgetSnapshot 生成。
- ProviderContract 映射。
- TokenUsageRecord 写入。
- ToolContract 校验。
- ToolResultEnvelope 生成。
- ArtifactRecord / EvidenceRecord 最小记录。
- PolicyEvaluation。
- VerificationRecord 基础验证。
- LoopDecision。
- WorkspaceCheckpoint。
- UserProjection。
- minimal PostTaskReview。

### 多前端一致性边界

第一阶段就要保证同一 workspace/session/task 在多个入口下看到的是同一份状态真相，而不是“看起来差不多”的近似结果。

必须成立的规则：

- `StateProjection` 是当前任务状态的唯一投影结果。
- `RuntimeEvent` 是流式输出、工具进度、权限请求、完成状态的唯一事实来源。
- TUI、Web、IDE、SDK 对同一 task 的实时展示，必须来自同一条 event stream 或同一份 projection 查询结果。
- 一个前端发起 `approve`、`deny`、`abort`、`resume` 后，其他前端应通过后续 event 看到同一状态变化。
- 前端本地缓存只能用于渲染加速，断线重连后必须能通过 `RuntimeQuery + RuntimeEvent` 恢复一致状态。

第一阶段重点支持的场景：

```text
1. SDK 创建或驱动一个 task
2. TUI attach 到同一个 session / task
3. Web attach 到同一个 session / task
4. 三端查询到同一个 task status、visible steps、approval state
5. 三端订阅到同一条流式输出和工具进度
```

### RuntimeHost 与事件流边界

多前端一致性的前提是存在一个中心 `RuntimeHost`。如果 CLI、TUI、app-server 各自创建本地内存 store，即使协议类型一致，也只是三个互不相干的演示 runtime。

第一阶段需要明确以下实现边界：

- `RuntimeHost` 是执行与状态所有权边界，负责接收 `SessionCommand`、驱动 `RuntimeLane / AgentLoop`、写入 event、更新 projection、广播 live event。
- `RuntimeStore` 是 durable facts，不直接等于 runtime host；store 可以恢复状态，但不能替代任务调度、运行中取消、订阅和 provider/tool loop。
- `EventBus` 负责把 durable event 与 live event 统一起来：先 append 到 store，再发布给订阅者；断线重连时按 cursor replay，再接 live stream。
- `InProcessTransport` 第一阶段可以用于 CLI/TUI，但它必须持有或连接 `Arc<RuntimeHost>`，不能只包 `RuntimeStore`。
- `HttpSseTransport` 用于 Web / SDK / daemon 模式，语义必须与 `InProcessTransport` 对拍一致。
- `RuntimeClient::subscribe` 的目标语义是 event stream；如果短期保留 snapshot API，也必须新增 live watch 能力，不能让 TUI 长期轮询历史事件。
- `SessionResolver` 必须稳定解析 workspace 当前 session / task，不能每次入口启动都生成新的默认 session。

这条边界决定了 TUI 的实施顺序：先让多个入口看到同一 task，再完善终端布局和组件。否则 TUI 会被迫维护自己的状态机，最终和 runtime 脱节。

### 协议与 SDK 约束

第一阶段需要把“runtime 协议”当成独立资产，而不只是 Rust 内部类型：

- `SessionCommand`、`RuntimeQuery`、`RuntimeEvent` 要有稳定 schema 产物。
- TypeScript SDK 应尽量从协议产物生成类型，避免手写接口漂移。
- Python SDK 第一阶段可以后置，但其 transport 语义必须与 TypeScript SDK 和 app-server 一致。
- 本地 SDK 允许两种运行方式：
  - 连接已运行的 `app-server`
  - 按配置拉起本地 runtime host
- 无论哪种运行方式，`task_id`、event 顺序、approval、resume、replay 语义必须一致。

### 协议测试与 smoke 约束

第一阶段除业务测试外，至少还要有三类契约测试：

- schema / fixture 测试：保证协议产物稳定可消费。
- app-server test client：对 `query`、`subscribe`、`approve`、`abort`、`resume` 做 transport 对拍。
- SDK 集成 smoke：保证 SDK 与 runtime 不会在字段和事件顺序上漂移。

### Coding Agent 验证默认值

coding agent 第一阶段默认采用强客观验证：

- 代码修改任务至少需要 `diff` 和一类客观验证证据。
- 客观验证证据优先来自 `test`、`lint`、`typecheck`、`build`、`command exit code`。
- 如果没有足够 evidence，任务不能 `stop_success`。
- 无法完成强验证时，只能输出 `stop_partial`、`blocked` 或 `stop_failed`。
- 文档/调研型 task 可以允许较弱验证，但 coding task 不应退化为模型自述完成。

### Coding Agent 记忆默认值

第一阶段不做重型长期记忆，默认只做：

- `WorkingSummary`
- `CompactionRecord`
- `MemoryCandidate`
- `project-scoped retrieval`

第一阶段不默认实现：

- 自动晋升长期 memory
- `user/global` 长期记忆写入
- 向量记忆作为基础依赖

### 后台可跑

这些能力可以在任务完成后后台运行，不应阻塞普通用户返回：

- deep PostTaskReview。
- FailureTaxonomy 深度归因。
- Evaluation / Improvement Projection。
- memory / policy / skill / benchmark 候选生成。
- provider routing 质量分析。
- ImprovementCandidate 生成。
- 从失败或高价值 trajectory 生成 EvaluationCase 候选。

### 离线运行

这些能力用于长期改进，不属于普通任务执行链路：

- replay_runner。
- vcr / golden fixture。
- EvaluationCase / EvaluationRun / EvaluationResult。
- CounterfactualReplay / CausalComparison / SecurityUtilityResult。
- regression suite。
- dynamic benchmark promotion。
- open-ended task generation。
- runtime / prompt / tool schema 改进实验。
- RegressionResult。
- PromotionDecision。

## 任务类型验证策略

| 任务类型 | 验证来源 | 完成判断 |
| --- | --- | --- |
| 代码修改 | diff、测试、lint、类型检查、命令退出码 | 修改存在且验证通过；失败时说明残余风险 |
| 文档修改 | 目标条目覆盖、重复减少、结构一致、引用有效 | 文档包含用户要求且没有明显重复/冲突 |
| 调研总结 | 来源、日期、引用、交叉验证、结论置信度 | 关键结论有来源，时效性信息已验证 |
| 工具执行 | exit code、stdout/stderr 摘要、artifact、policy | 工具完成且结果可解释；失败有错误归因 |
| 配置修改 | schema 校验、配置读取、dry-run、回滚点 | 配置可解析且影响范围明确 |
| 多步骤任务 | 每步 evidence、最终 verification、post review | 子目标完成且最终目标没有未解释缺口 |

## User Projection 格式

普通用户不看完整 trace，只看压缩后的任务视图：

```text
UserProjection
  session_id
  task_id
  status: running | waiting_approval | completed | partial | failed | blocked
  visible_steps
  approval_request
  result_summary
  changed_files
  verification_summary
  residual_risks
  next_actions
```

TUI、CLI、API 都从 `UserProjection` 展示，不直接读取 raw runtime event。

## Debug Projection 格式

```text
DebugProjection
  session_id
  task_id
  event_stream
  loop_decisions
  policy_evaluations
  evidence_records
  verification_records
  context_projection
  token_budget
  provider_raw_events
  tool_result_envelopes
```

Debug Projection 只在 debug/audit/replay 模式启用。

## P0 验收矩阵

第一阶段完成时至少覆盖这些硬边界，不用等后续治理增强：

| 场景 | 必须验证 |
| --- | --- |
| 多入口请求 | CLI / TUI / API / SDK 都转成 `SessionCommand`，没有入口私有状态机 |
| 多前端一致性 | 同一 `workspace/session/task` 在 SDK / TUI / Web 查询到相同状态，并能看到同一条运行中事件流 |
| 运行中输入 | active task 运行时，新输入必须被记录为 append / inject / interrupt / reject 之一，且其他前端看到同一状态 |
| provider 正常流 | stream event、usage、finish_reason、tool call 映射进 `ProviderContract` |
| custom provider 验证 | 协议选择、base URL、API key、model、review 都通过校验；未实现 live adapter 的协议不能保存成 ready |
| provider 异常流 | truncated stream、malformed event、rate limit、network error 都有结构化错误 |
| token 观测 | 每次 provider request 前有 `TokenBudgetSnapshot`，response 后有 `TokenUsageRecord`；usage 缺失时记录 unknown 或估算来源 |
| tool 成功 | `ToolContract` 校验通过，生成 `ToolResultEnvelope`、artifact refs 和 evidence refs |
| tool 失败 | error、timeout、cancelled、blocked 都有明确状态，不把 raw stderr 直接塞进模型 |
| abort / pause | abort 后不能继续产生外部副作用，pause/resume 不破坏 event 顺序 |
| retry | 有副作用的 tool retry 必须依赖 idempotency 或显式阻断 |
| loop guard | 重复工具失败、空回复、context overflow、max iteration 都有明确 `LoopDecision`，不能无界循环 |
| artifact | raw output 可通过 checksum 校验，模型只读取摘要或受控 excerpt |
| workspace checkpoint | 文件副作用后有可恢复引用，且不修改用户 `.git` |
| verification | 没有足够 evidence 时不能 `stop_success`，只能 `stop_partial`、`stop_failed` 或 `blocked` |
| memory | `MemoryCandidate` 不自动晋升长期 memory，必须保留 evidence、scope 和 rollback 信息 |

## 第一阶段落地顺序

1. `golutra-core`：核心 schema。
2. `golutra-store`：SQLite、event log、artifact store。
3. `golutra-event`：durable/live-only event。
4. `golutra-runtime`：RuntimeLane、turn loop、LoopGuard、LoopDecision、verification 调度。
5. `golutra-context`：ContextBuilder、TokenBudgetTracker、WorkingSummary。
6. `golutra-llm`：provider contract、capability matrix、routing、usage normalization。
7. `golutra-tools`：tool schema、permission、ToolResultEnvelope。
8. `golutra-store` checkpoint 子模块：workspace checkpoint。
9. `golutra-verify`：任务类型基础验证策略。
10. `golutra-runtime` host 子模块：`RuntimeHost`、`SessionResolver`、`EventBus`、workspace 默认 SQLite 路径。
11. `golutra-client`：统一 `RuntimeClient`、`RuntimeQuery`、event replay 和 live subscription 接口。
12. `golutra-cli` / `golutra-tui`：通过 `InProcessTransport` 连接同一 `RuntimeHost`，只消费 command/query/event。
13. `golutra-app-server`：暴露 `HttpSseTransport`，支持 Web / SDK attach、query、stream。
14. `golutra-vis`：DebugProjection 和 replay 查询。
15. `golutra-eval`：ImprovementCandidate、RegressionResult、PromotionDecision 的离线链路。

入口优先级默认值：

1. `CLI + TUI + InProcessTransport`
2. `app-server + HttpSseTransport`
3. `SDK + Web attach`
4. `IDE attach`

## 通过标准

第一阶段完成时必须满足：

- 单个任务能从 CLI/TUI/API 进入同一 runtime。
- 每个 turn 有 durable event。
- 每个 provider 响应都通过 ProviderContract 归一化。
- 每个工具执行前有 ToolContract 和 PolicyEvaluation。
- 每个工具结果有 ToolResultEnvelope。
- 运行中用户输入经过 RuntimeLane 和 BusyPolicyDecision。
- raw output、日志和大内容有 ArtifactRecord，关键结论有 EvidenceRecord。
- 文件副作用后能生成 WorkspaceCheckpoint 或明确说明不可快照原因。
- 每个任务结束有 VerificationRecord。
- 每次循环结束有 LoopDecision。
- 普通用户只看到 UserProjection。
- Debug 模式可以展开 RuntimeEvent、ContextProjection、Evidence 和 Verification。
- 失败任务能 replay 到关键决策点。
- 失败任务能生成至少一个可人工查看的 ImprovementCandidate。

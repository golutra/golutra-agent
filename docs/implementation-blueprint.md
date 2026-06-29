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

必须完成：

```text
SessionCommand
RuntimeEvent
StateProjection
ContextProjection
ProviderContract
ToolContract
ToolResultEnvelope
ArtifactRecord / EvidenceRecord
PolicyEvaluation
LoopDecision
VerificationRecord
UserProjection
DebugProjection
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

## 第一阶段吸收的架构启示

第一阶段不新增复杂治理层，但必须吸收以下 runtime 硬边界：

- `SessionCommand` 是 CLI / TUI / API / SDK 的唯一入口协议，入口层不能绕过 runtime 自建状态机。
- `ProviderContract` 是 provider 反腐层，统一 stream event、tool call、usage、finish_reason、error、rate limit 和 cost。
- `ToolContract` 先于工具实现定义，明确 schema、错误、取消、重试、幂等、副作用和 artifact 策略。
- `ArtifactRecord` / `EvidenceRecord` 是事实层，raw output 默认进 artifact，模型只读取受控摘要和 evidence refs。
- `VerificationRecord` 决定任务是否完成，模型自然语言不能直接触发成功终止。
- `PolicyEvaluation` 必须在执行层阻断高风险文件、进程、网络、secret 和外部副作用。
- `MemoryCandidate` 只作为候选，长期 memory 不能从 transcript 自动晋升。

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
- ContextProjection 构造。
- ProviderContract 映射。
- ToolContract 校验。
- ToolResultEnvelope 生成。
- ArtifactRecord / EvidenceRecord 最小记录。
- PolicyEvaluation。
- VerificationRecord 基础验证。
- LoopDecision。
- UserProjection。
- minimal PostTaskReview。

### 后台可跑

这些能力可以在任务完成后后台运行，不应阻塞普通用户返回：

- deep PostTaskReview。
- FailureTaxonomy 深度归因。
- Evaluation / Improvement Projection。
- memory / policy / skill / benchmark 候选生成。
- provider routing 质量分析。
- ImprovementCandidate 生成。

### 离线运行

这些能力用于长期改进，不属于普通任务执行链路：

- replay_runner。
- vcr / golden fixture。
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
| provider 正常流 | stream event、usage、finish_reason、tool call 映射进 `ProviderContract` |
| provider 异常流 | truncated stream、malformed event、rate limit、network error 都有结构化错误 |
| tool 成功 | `ToolContract` 校验通过，生成 `ToolResultEnvelope`、artifact refs 和 evidence refs |
| tool 失败 | error、timeout、cancelled、blocked 都有明确状态，不把 raw stderr 直接塞进模型 |
| abort / pause | abort 后不能继续产生外部副作用，pause/resume 不破坏 event 顺序 |
| retry | 有副作用的 tool retry 必须依赖 idempotency 或显式阻断 |
| artifact | raw output 可通过 checksum 校验，模型只读取摘要或受控 excerpt |
| verification | 没有足够 evidence 时不能 `stop_success`，只能 `stop_partial`、`stop_failed` 或 `blocked` |
| memory | `MemoryCandidate` 不自动晋升长期 memory，必须保留 evidence、scope 和 rollback 信息 |

## 第一阶段落地顺序

1. `golutra-core`：核心 schema。
2. `golutra-store`：SQLite、event log、artifact store。
3. `golutra-event`：durable/live-only event。
4. `golutra-llm`：provider contract、capability matrix、routing。
5. `golutra-tools`：tool schema、permission、ToolResultEnvelope。
6. `golutra-context`：ContextBuilder、TokenBudgetTracker、WorkingSummary。
7. `golutra-runtime`：turn loop、LoopDecision、verification 调度。
8. `golutra-verify`：任务类型基础验证策略。
9. `golutra-cli` / `golutra-tui`：UserProjection 展示。
10. `golutra-vis`：DebugProjection 和 replay 查询。
11. `golutra-eval`：ImprovementCandidate、RegressionResult、PromotionDecision 的离线链路。

## 通过标准

第一阶段完成时必须满足：

- 单个任务能从 CLI/TUI/API 进入同一 runtime。
- 每个 turn 有 durable event。
- 每个 provider 响应都通过 ProviderContract 归一化。
- 每个工具执行前有 ToolContract 和 PolicyEvaluation。
- 每个工具结果有 ToolResultEnvelope。
- raw output、日志和大内容有 ArtifactRecord，关键结论有 EvidenceRecord。
- 每个任务结束有 VerificationRecord。
- 每次循环结束有 LoopDecision。
- 普通用户只看到 UserProjection。
- Debug 模式可以展开 RuntimeEvent、ContextProjection、Evidence 和 Verification。
- 失败任务能 replay 到关键决策点。
- 失败任务能生成至少一个可人工查看的 ImprovementCandidate。

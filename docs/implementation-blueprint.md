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
ToolResultEnvelope
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
Plugin Marketplace
复杂 Multi-Agent Orchestration
自动修改 runtime 代码
```

第一阶段只生成 `ImprovementCandidate`，不自动应用改动。

## 最小核心 Schema

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

- RuntimeEvent 写入。
- StateProjection 更新。
- ContextProjection 构造。
- ToolResultEnvelope 生成。
- PolicyEvaluation。
- VerificationRecord。
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

## 第一阶段落地顺序

1. `golutra-core`：核心 schema。
2. `golutra-store`：SQLite、event log、artifact store。
3. `golutra-event`：durable/live-only event。
4. `golutra-llm`：provider contract、capability matrix、routing。
5. `golutra-tools`：tool schema、permission、ToolResultEnvelope。
6. `golutra-context`：ContextBuilder、TokenBudgetTracker、WorkingSummary。
7. `golutra-runtime`：turn loop、LoopDecision、verification 调度。
8. `golutra-verify`：任务类型验证策略。
9. `golutra-cli` / `golutra-tui`：UserProjection 展示。
10. `golutra-vis`：DebugProjection 和 replay 查询。
11. `golutra-eval`：ImprovementCandidate、RegressionResult、PromotionDecision 的离线链路。

## 通过标准

第一阶段完成时必须满足：

- 单个任务能从 CLI/TUI/API 进入同一 runtime。
- 每个 turn 有 durable event。
- 每个工具结果有 ToolResultEnvelope。
- 每个任务结束有 VerificationRecord。
- 每次循环结束有 LoopDecision。
- 普通用户只看到 UserProjection。
- Debug 模式可以展开 RuntimeEvent、ContextProjection、Evidence 和 Verification。
- 失败任务能 replay 到关键决策点。
- 失败任务能生成至少一个可人工查看的 ImprovementCandidate。

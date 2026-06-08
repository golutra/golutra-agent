# Evaluation 与 Observability 架构规格

## 文档定位

本文档定义 Golutra 如何观测任务执行、判断任务是否达成、复盘失败原因，并把经验转化为可验证改进。

主架构见 `ARCHITECTURE.md`。
改进候选、回归验证和晋升决策见 `agent-improvement-loop.md`。

## 核心原则

```text
只记录日志不够。
Agent 必须记录发生了什么、为什么这么做、证据是什么、结果是否可靠、后续怎么改进。
```

同时，观测不能被理解成“普通用户每次都看到完整审计链路”。Golutra 应把同一批 runtime facts 投影成四种用途：

```text
Runtime Control Projection
  agent/runtime 自己用，每次任务都必须生成最小必要数据。

User Projection
  普通用户看，只展示进度、权限、结果和必要风险。

Debug / Audit Projection
  开发者、人类审计者或其他 agent 看，用于展开链路。

Evaluation / Improvement Projection
  离线或后台系统看，用于 replay、benchmark、regression 和 agent 改进。
```

因此，运行时最小观测是执行链路的一部分；完整审计、复盘和改进分析是按需或后台链路。

## 观测对象

每个任务至少观测九类数据：

```text
DataEvent
ObservationRecord
EvidenceRecord
PolicyEvaluation
CandidateAction
DecisionRecord
ExecutionResult
VerificationRecord
DecisionEvaluation
```

这些数据来自 runtime、provider、tool、policy、verification、user approval 和 post-task review，而不是事后从聊天文本里猜。

## RuntimeEvent

所有关键动作都应写入 durable event：

```text
RuntimeEvent
  session_id
  turn_id
  task_id
  event_type
  timestamp
  parent_event_id
  payload
  durable: true | false
```

durable event 用于恢复、审计、replay、benchmark。live-only event 只用于 UI 动画和临时状态。

## DecisionRecord

关键决策必须记录：

```text
DecisionRecord
  candidates
  selected_action
  rejected_reasons
  evidence_refs
  policy_refs
  risk
  confidence
  expected_outcome
```

需要记录的决策包括：

- provider 选择。
- tool 选择。
- 权限升级。
- retry / fallback。
- compact。
- 是否继续任务。
- 是否完成任务。
- memory / skill / benchmark 候选晋升。

## Verification

任务是否达成不能只由模型自然语言判断。

`VerificationRecord` 至少包含：

```text
objective
completion_criteria
checks
evidence_refs
result: pass | fail | partial | unknown
policy_status
residual_risks
```

常见验证来源：

- 文件 diff。
- 测试结果。
- 命令退出码。
- 静态检查。
- 用户确认。
- artifact 证据。
- policy evaluation。
- benchmark / golden fixture。

## PostTaskReview

任务结束后生成复盘，但复盘分为同步最小复盘和后台深度复盘：

```text
minimal PostTaskReview
  同步生成
  只记录 outcome、关键失败类型、证据质量、必要风险

deep PostTaskReview
  后台或离线生成
  用于 failure taxonomy、benchmark、memory/policy/skill 候选和 agent 改进
```

```text
PostTaskReview
  task_id
  mode: minimal | deep
  outcome
  success_reasons
  failure_reasons
  evidence_quality
  policy_issues
  context_issues
  tool_issues
  provider_issues
  suggested_improvements
  promotion_candidates
```

复盘的作用：

- 判断任务是否真正完成。
- 区分失败来自模型、工具、上下文、权限、验证还是环境。
- 形成 memory、benchmark、policy、skill 候选。
- 形成 ImprovementCandidate，但不直接修改 agent。
- 给后续 replay、eval 和 regression 提供输入。

普通用户请求默认只等待 minimal review。deep review 不应阻塞用户返回，除非任务明确要求同步审计。

## Failure Taxonomy

建议统一失败分类：

| 类型 | 含义 |
| --- | --- |
| GoalFailure | 目标理解错或完成条件不清 |
| ContextFailure | 关键上下文缺失、压缩错误、memory 注入错误 |
| ToolFailure | 工具失败、参数错误、输出不可用 |
| PolicyFailure | 权限、网络、文件、命令或安全策略违规 |
| ProviderFailure | 模型 API、stream、tool calling、reasoning 或 token 限制失败 |
| VerificationFailure | 不能证明任务完成或证据质量不足 |
| StateFailure | session、branch、resume、artifact 或 event 状态漂移 |
| CostFailure | token、时间、工具次数或预算超限 |
| HumanInteractionFailure | 需要用户确认但没有拿到明确输入 |

## Debug / Audit / Replay

普通用户不需要看到完整观测链路。Golutra 应把观测数据拆成四类投影：

```text
runtime_control
  给 LoopDecision、ContextBuilder、Verification、PolicyEvaluation 使用
  必须轻量、同步、可恢复

user
  展示结果、工具进度、权限请求、必要风险

debug
  展示 event stream、token、provider raw event、tool envelope、LoopDecision

audit / replay
  展示完整 evidence、policy、verification、post review、context projection

evaluation / improvement
  离线读取 durable event、artifact、trajectory、verification、post review
  生成 benchmark、regression、memory/policy/skill 候选和代码改进建议
```

UI 只展示 runtime projection，不维护自己的任务真相。

四类投影的关系：

| 投影 | 是否每次运行 | 使用者 | 作用 |
| --- | --- | --- | --- |
| Runtime Control | 是 | agent runtime | 判断下一步、是否 compact/retry/fallback/stop |
| User | 是 | 普通用户 | 展示进度、权限、结果、必要风险 |
| Debug / Audit | 按需 | 开发者、人类审计者、其他 agent | 展开链路、定位错误、解释行为 |
| Evaluation / Improvement | 后台或离线 | eval 系统、改进 agent | replay、benchmark、regression、改 prompt/tool/schema/policy/runtime |

## Evaluation Harness

Evaluation Harness 基于 durable event 和 artifact，而不是重新拼 prompt。

核心能力：

```text
trajectory_recorder
replay_runner
vcr / golden fixture
provider_comparison
failure_taxonomy_report
regression_suite
post_task_reviewer
```

它用于：

- 复现失败。
- 比较 provider 和 routing 策略。
- 验证 prompt/context/tool/schema 修改是否变好。
- 把历史失败晋升为 benchmark。
- 为 ImprovementCandidate 生成 RegressionResult。

## 任务类型验证策略

| 任务类型 | 验证来源 | 完成判断 |
| --- | --- | --- |
| 代码修改 | diff、测试、lint、类型检查、命令退出码 | 修改存在且验证通过；失败时说明残余风险 |
| 文档修改 | 目标条目覆盖、重复减少、结构一致、引用有效 | 文档包含用户要求且没有明显重复/冲突 |
| 调研总结 | 来源、日期、引用、交叉验证、结论置信度 | 关键结论有来源，时效性信息已验证 |
| 工具执行 | exit code、stdout/stderr 摘要、artifact、policy | 工具完成且结果可解释；失败有错误归因 |
| 配置修改 | schema 校验、配置读取、dry-run、回滚点 | 配置可解析且影响范围明确 |
| 多步骤任务 | 每步 evidence、最终 verification、post review | 子目标完成且最终目标没有未解释缺口 |

## User Projection

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

CLI、TUI、API 都从 `UserProjection` 展示，不直接读取 raw runtime event。

## Dynamic Benchmark

Benchmark 不应只是一组固定题。Golutra 可以从真实任务中沉淀：

```text
failed trajectory
near miss
manual correction
policy violation
context compaction failure
tool misuse
provider fallback failure
```

晋升条件：

- 有可回放输入。
- 有稳定期望结果。
- 有明确验证方式。
- 有失败分类。
- 能防止直接记忆答案。

## OpenTelemetry 映射

内部事件可以映射到 OTel span：

```text
planning
context_build
memory_retrieval
provider_call
tool_execution
safety_check
delegation
verification
post_task_review
```

每个 span 关联：

```text
session_id
turn_id
task_id
trace_id
parent_id
provider
model
tool
token_usage
latency
cost
```

## 判断标准

合格的观测评估体系必须满足：

- 能解释每个关键决策为什么发生。
- 能证明任务是否达成。
- 能区分失败归因。
- 能 replay 任意关键 turn。
- 能把失败转成 benchmark。
- 能把高质量经验转成受控 memory/skill/policy 候选。
- 能把失败转成 ImprovementCandidate，并通过 regression 与 PromotionDecision 决定是否采用。
- 普通用户不会被 debug 信息干扰。

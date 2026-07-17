# Evaluation 与 Observability 架构规格

## 文档定位

本文档定义 Golutra 如何观测任务执行、判断任务是否达成、复盘失败原因，并把经验转化为可验证改进。

主架构见 `ARCHITECTURE.md`。
改进候选、回归验证和晋升决策见 `agent-improvement-loop.md`。
完整 task trace、持久后台作业和真实 regression 的实施记录见 `runtime-governance-completion-design.md`。

## 当前实现状态

截至 2026-07-16，runtime 已具备持久化 evaluation、完整 trace 和多投影观测；P2.5 当前范围已经形成可信闭环，并把完整事实交给独立 P3 本地 Supervisor：

- terminal task 可生成 `PostTaskReview`、`EvaluationCase`、`TrajectoryReplay`、`EvaluationRun` 和 `EvaluationResult`，并按 canonical cwd hash 持久化到 `$GOLUTRA_HOME/state/workspaces/<cwd-hash>/evaluation.json`；状态更新有文件锁、大小边界和 owner-only 权限。
- pass/partial/fail、latency、evidence refs、residual risks 和 failure taxonomy 来自 runtime facts 与 verification plan/assertions，不从聊天文本反推；当前支持的路径、内容、命令和 policy assertion 会进入三维 hard gate，无法客观证明的标准保持 Unknown/Partial。
- 失败或 partial trajectory 可生成 benchmark、generated-task 和 improvement 候选；CLI、transport 与双 SDK 可以查询候选、regression、apply 和 rollback 状态。
- `TrajectoryReplay` 仍是 event/artifact 的 projection replay；候选 regression 由 `RuntimeHost::run_regression_campaign` 启动配对 baseline/candidate RuntimeHost，`run_regression` 对已记录 execution facts 做纯比较。
- `EvaluationStore::compare_counterfactual` 能比较调用方提供的 baseline/variant durable run facts，但不会自行生成受控 paired execution；没有 execution refs 的结果不能作为未来代码晋升证据。
- deep evaluation 在 TaskCompleted 前写入 SQLite `PostTaskJob`，worker 提供 lease/retry/recovery；Embedded one-shot 退出后新 Host/daemon 可继续完成 job。
- event writer 可在导出边界生成不可变 rollout snapshot；TaskTrace 通过 cursor 分页和 integrity/disclosure 读取 canonical facts，迟到 evaluation event 不会改写已导出的边界。
- `golutra-vis` 可从 RuntimeEvent/DebugProjection 导出 Audit、Events 和 OpenTelemetry JSON span；TUI 只有显式 `--debug` 或 `/debug` 才查询有界开发者摘要，普通启动不渲染治理噪声。
- `golutra-supervisor` 只接收 complete TaskTrace，使用 paired execution、sealed/fresh/security/migration、holdout disclosure budget 和 OS-enforced TrustedBuilder 决定 runtime code release；普通 Runtime 无 stable pointer 写权限。

隔离 GeneratedTask 已能由 `golutra-evolution` 通过独立 fixture RuntimeHost 执行；任意冻结候选的 baseline/candidate regression 也已由 `golutra-client` 接入。完整 `TaskTrace`、SQLite durable job、语义 verification 和 execution-backed regression 属于已完成的 P2.5 当前范围。

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

## 前沿观点的客观采纳

近一年 Agent 研究和工程实践对 Golutra 最有价值的提醒，不是“让模型更会规划”，而是“让运行时更可控”。这些方向已经按成本分层进入当前实现，但不能全部塞进同步模型调用链路。

| 方向 | 对 Golutra 的价值 | 采纳方式 | 局限 |
| --- | --- | --- | --- |
| Goal Drift / Planning Drift | 防止长任务逐步偏离用户原始目标 | 加入 `GoalLedger` 和 `GoalAlignmentCheck` | 对齐分数本身仍可能误判，必须保留 evidence 和人可接管 |
| Runtime Governance | 把目标、策略、风险、预算、审批统一成运行时决策 | 加入 `RuntimeGovernor` / `GovernanceDecision` | 不应把所有判断都同步大模型化，否则成本过高 |
| Runtime Verification | 任务完成和高风险动作必须有证据 | 加入 `VerificationTier` | 低风险动作不应完整验证，高风险动作不能省略验证 |
| Event Sampling | 避免完整观测导致存储、索引、评估成本爆炸 | 加入 `EventSamplingPolicy` | raw event 仍需轻量保存，否则恢复和 replay 会断链 |
| Context Projection Cache | 降低 token，并减少长上下文造成的漂移 | 加入 `ContextProjectionCache` | cache 失效规则必须严格，否则会引用过期状态 |

这些补充有借鉴意义，但不能直接照搬成“每一步都评估、每个事件都索引、每个任务都深度审计”。当前同步主链保留基础 event、LoopDecision、VerificationRecord、Goal/Governor decision 和 minimal PostTaskReview；deep evaluation、candidate/evolution 在终态后由 durable worker 或显式命令运行。Golutra 的设计原则是：

```text
运行时自用判断必须轻量同步；
普通用户展示必须克制；
debug/audit 必须按需展开；
离线评估必须分层采样。
```

## 可借鉴的外部平台能力

LangSmith、Braintrust、Promptfoo 这类工具不是 Golutra 的架构模板，但它们在工程上证明了几件事值得吸收：

- `trace / span` 结构：把一次任务拆成可定位的阶段，而不是只有一条聊天记录。
- `dataset / experiment / scorer`：把失败样本、回归和对比做成可重复实验，而不是只写复盘文字。
- `red team / safety case`：把 prompt injection、越权调用、敏感信息泄漏和 policy bypass 变成可自动化检查项。
- `CI` 集成：让评估进入持续集成，而不是只靠人工抽查。

Golutra 只吸收这些方法，不吸收它们的产品定位。对应映射为：

```text
RuntimeEvent / trace_id / span_id
DebugProjection / replay trace view
EvaluationProjection / dataset / scorer
VerificationRecord / assertion / evidence
RegressionResult / experiment comparison
PolicyEvaluation / red team case
```

推荐保留的最小字段：

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

这些字段的价值主要体现在 DebugProjection、Evaluation Harness 和 regression 对比里，不应把 Golutra 改造成一个纯评测平台。

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

## Evaluation 分层模型

Evaluation 不能只理解成“任务结束后打分”。在 Golutra 里，它分成五层，分别服务不同时间点和不同成本等级：

| 层级 | 运行时机 | 解决的问题 | 默认成本 |
| --- | --- | --- | --- |
| Runtime Verification | 同步运行 | 当前任务能否声明完成 | 低 |
| Trajectory Evaluation | 后台或按需 | 执行过程是否有效、是否漂移、是否浪费 | 中 |
| Evaluation Case / Dataset | 离线沉淀 | 哪些真实任务可以复现、回归和比较 | 中 |
| Regression / Benchmark | 离线运行 | 某个改动是否真的变好、是否引入回归 | 中到高 |
| Meta Evaluation | 离线审计 | 评估器、judge、benchmark 是否可靠 | 中到高 |

分层原则：

- 普通任务只同步执行 `Runtime Verification` 和 minimal `PostTaskReview`。
- `Trajectory Evaluation` 默认不阻塞用户返回，只在失败、高风险、用户纠正或 debug/audit 模式下展开。
- `Evaluation Case` 从真实 trajectory、benchmark 样本和人工构造用例中沉淀。
- `Regression / Benchmark` 只用于比较版本、provider、prompt、tool schema、policy 和 context 规则。
- `Meta Evaluation` 用于防止过拟合、答案泄漏、judge 偏置和 harness 变厚造成的虚假提升。

这套分层的核心目标是：

```text
让 evaluation 成为改进 agent 的事实系统，
而不是把每个用户任务都变成昂贵评测。
```

## Evaluation Core Model

Golutra 的评估系统应围绕以下对象收敛：

```text
RuntimeEvent / ArtifactRecord / EvidenceRecord / VerificationRecord
-> TrajectoryReplay
-> CounterfactualReplay
-> EvaluationCase
-> EvaluationRun
-> Scorer
-> EvaluationResult
-> CausalComparison
-> RegressionResult
-> PromotionDecision
```

### TrajectoryReplay

用于复现一次历史任务，不重新从聊天文本猜测输入。

```text
TrajectoryReplay
  replay_id
  source_task_id
  event_refs
  artifact_refs
  context_projection_refs
  provider_fixture_refs
  tool_fixture_refs
  replay_mode: exact | simulated_provider | live_provider
  determinism_level
  limitations
```

要求：

- 能定位当时的 event、artifact、context projection 和 verification。
- provider 和 tool 可以用 fixture 回放，也可以在隔离环境中 live replay。
- replay 结果必须声明确定性边界，不能把不可复现结果当稳定证据。

当前实现仍将历史 `TrajectoryReplay` 明确标记为 projection replay；TaskTrace 已保存 `ContextSnapshot`，candidate regression 会重新调用隔离 RuntimeHost/provider/tool。只有引用隔离 execution run 的结果才能作为 promotion evidence。

### CounterfactualReplay

用于在同一个任务或同一批 case 上替换某一层策略，比较“到底是哪一层导致变好或变坏”。它属于离线 evaluation，不进入普通任务同步链路。

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

典型对照：

- 有 memory vs 无 memory。
- 长上下文 vs compact 后上下文。
- 工具输出全文 vs summary + artifact ref。
- provider A vs provider B。
- 宽松 policy vs 严格 policy。
- 不同 token budget / truncation 策略。

要求：

- 每次只改变一个主要层，其他变量尽量固定。
- 无法固定的变量必须写入 `controlled_variables` 或 `limitations`。
- 结果必须同时比较质量、成本、延迟、工具次数和安全风险。

### EvaluationCase

用于把真实任务、失败轨迹或 benchmark 样本变成可重复评估的 case。

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

要求：

- 每个 case 必须有成功标准和所需证据，不能只有自然语言描述。
- 从真实任务晋升为 case 时，要保留来源 task、失败分类和 artifact/evidence 引用。
- 高风险 case 要标记数据泄漏、隐私、外部副作用和 judge 风险。

### EvaluationRun

用于记录某个系统版本、provider 路由、prompt/context/tool/policy 组合在一批 case 上的结果。

```text
EvaluationRun
  run_id
  dataset_id
  case_ids
  system_version
  candidate_ref
  provider_config_ref
  runtime_config_ref
  started_at
  completed_at
  cost
  latency
  artifact_refs
  result_refs
```

要求：

- 必须能对比 baseline 和 candidate。
- 必须记录 system version、provider config、runtime config、tool budget、attempt count、cost 和 latency。
- run 的输入来自 `EvaluationCase`，输出进入 `EvaluationResult`，不能直接用一段总结替代。

### Scorer

Scorer 是可替换的评分器，不等于 LLM judge。

```text
Scorer
  scorer_id
  kind: command | rule | unit_test | snapshot | human | llm_judge | composite
  input_contract
  output_contract
  evidence_requirements
  confidence_policy
  known_biases
```

推荐优先级：

1. 客观命令：test、lint、typecheck、build、exit code。
2. 规则检查：diff、文件存在、schema、artifact checksum、policy。
3. snapshot / golden：协议、工具输出、projection、trace。
4. 人审：高风险、开放性强或 judge 不稳定的 case。
5. LLM judge：只作为辅助，不能成为高风险结论的唯一来源。

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
  judge_reliability
  residual_risks
```

要求：

- 结论必须绑定 evidence。
- `unknown` 是合法结果，不能强行归为 pass/fail。
- 需要同时记录质量、成本、延迟和失败分类，避免只优化单一分数。

### CausalComparison

用于比较 baseline 和 variant 的差异，给 ImprovementCandidate 提供更强证据。

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

判断原则：

- 质量提升但成本、延迟或安全风险不可接受时，不能直接判定为 improved。
- 成本下降但通过率、证据质量或安全性下降时，不能直接晋升。
- `inconclusive` 是合法结果，尤其适用于不可复现或变量无法控制的 replay。

### SecurityUtilityResult

用于同时评估任务效用和安全性，避免 agent 只追求完成任务而牺牲 policy。

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

它特别适合 coding agent，因为代码仓库、README、issue、网页内容、测试输出和 package script 都可能包含恶意指令。高风险 case 不应只看任务是否完成，还必须看是否存在越权读取、数据外泄、恶意命令执行或 prompt injection 成功。

### MetaEvaluation

用于评估 evaluation 本身是否可靠。

```text
MetaEvaluation
  target_run_id
  leakage_checks
  judge_checks
  harness_checks
  scorer_disagreement
  overfitting_signals
  verdict: reliable | risky | invalid
```

检查重点：

- benchmark 答案是否泄漏到 prompt、artifact、memory 或检索层。
- LLM judge 是否被格式诱导，是否缺少 evidence。
- harness / scaffold 是否变厚，导致分数提升不可比较。
- scorer 之间是否严重分歧。
- candidate 是否只对公开 benchmark 变好，对 shadow / regression set 退化。

## Planning Drift 观测链路

Planning Drift 的核心问题是：任务在执行过程中看起来每一步都合理，但整体逐渐偏离原始目标。当前 `RuntimeGovernor` 已在 provider、tool、tool result 和 completion 边界执行确定性的 goal alignment、budget、policy 与 security check；它提供可审计的最小防线，不声称替代语义级 planner 或额外 judge。

Golutra 用三层数据控制它：

```text
GoalRecord
  记录原始目标、约束、成功标准和禁止偏移项

GoalAlignmentCheck
  检查 plan / tool_call / tool_result / completion_claim 是否仍服务原始目标

GovernanceDecision
  决定 allow / warn / ask_user / replan / block / terminate
```

典型触发点：

- 生成新计划时。
- 准备执行高风险工具前。
- 多轮任务超过固定 step 或 token 阈值时。
- compact 前后。
- 声称任务完成前。
- 用户目标、工具结果和当前计划出现语义冲突时。

示例：

```text
用户目标：找最便宜的东京机票。
当前计划：研究航空公司商业模式。

GoalAlignmentCheck:
  alignment_score: 0.12
  drift_type: wrong_objective
  action: replan
```

这类检查的作用不是保证模型永远正确，而是让偏移有记录、有阈值、有恢复动作。

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

### Verification Tier

为控制成本，验证按以下目标层级理解：

| Tier | 适用动作 | 同步成本 | 处理方式 |
| --- | --- | --- | --- |
| Tier0 | 低风险读取、普通列表、无副作用查询 | 极低 | 记录 event 和摘要，不额外验证 |
| Tier1 | 非破坏性工具调用、普通文件读取 | 低 | exit code、schema、文件存在性、摘要一致性 |
| Tier2 | 代码修改、配置修改、网络请求、策略边界动作 | 中 | diff、测试、lint、artifact、policy/evidence |
| Tier3 | 删除、覆盖、权限升级、生产环境、改进晋升 | 高 | 强验证 + approval gate，必要时人工确认 |

当前 runtime 已用结构化 `VerificationCheckKind` 区分 assistant response、tool execution、workspace change 和 objective validation，并用任务/文件类型决定是否要求代码验证；`VerificationTier` 数据模型保留可配置分级。这样解决 Cost Explosion：普通对话不强制工具 evidence，workspace/code change 必须有 diff 和目标验证，高风险工具继续经过 policy/approval。面向组织的自定义 tier policy 属于配置扩展，不是终态正确性缺口。

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

## Token / Cost 观测链路

Token 消耗是 CostFailure、ContextFailure 和 ProviderFailure 的共同输入，不能只作为 provider usage 的附属字段。Golutra 应把 token 观测拆成同步记录和离线归因两层：

详细预算、上下文分层和超预算处理归属 `context-memory.md`；本文只定义 token / cost 如何进入 debug、evaluation、regression 和失败归因。

```text
同步记录
  ContextBuilder 生成 TokenBudgetSnapshot
  ProviderContract 归一化 ProviderUsage
  Runtime 写入 TokenUsageRecord
  LoopDecision 读取 budget_state

离线归因
  Debug / Evaluation Projection 聚合 token timeline
  PostTaskReview 识别 token waste 和 cost risk
  EvaluationRun / RegressionResult 对比成本变化
```

### 必须观测的 token 数据

```text
TokenBudgetSnapshot
  预算、阈值、预估输入、预留输出、超限动作

TokenUsageRecord
  provider 返回或估算的 input / output / reasoning / cached / total tokens

TokenAttribution
  system_prompt / developer_instruction / runtime_context / policy / user_message / assistant_recent / working_summary / memory / evidence / tool_instruction / tool_result_excerpt / output / reasoning / cached_input 的占比

CostRecord
  provider、model、unit price、estimated cost、cost source、confidence
```

第一阶段要求：

- 每次 provider request 前都有 `TokenBudgetSnapshot`。
- 每次 provider response 后都有 `TokenUsageRecord`，usage 缺失时记录 unknown 和估算来源。
- `input_tokens` 必须覆盖所有进入 provider request 的模型可见内容，包括提示词、运行时上下文、历史摘要、memory、evidence、工具说明和工具结果片段。
- `TokenAttribution` 要能区分提示词、上下文、memory、工具结果片段、输出、reasoning 和 cached input，字段不完整时标记 unknown。
- DebugProjection 能看到每个 turn 的 token timeline。
- EvaluationRun / RegressionResult 必须记录 cost 和 latency，不允许只比较质量分。
- PostTaskReview 可以把 token 相关问题归入 `CostFailure` 或 `ContextFailure`。

### Token Waste 归因

Token waste 不在同步链路里做重分析，默认后台或 debug 模式归因。

常见类型：

| 类型 | 含义 | 可能改进 |
| --- | --- | --- |
| stale_context | 旧上下文仍反复进入 prompt | compact 或 ContextProjectionCache |
| low_relevance_memory | 低相关 memory 占用 prompt | 调整 MemoryRetriever 阈值 |
| oversized_tool_output | 工具输出片段过长 | 工具改 summary + artifact ref |
| repeated_retry | retry/fallback 消耗过多 | 调整 RetryPolicy 或 ask_user |
| judge_overuse | 过多 LLM judge / deep review | 降级为 rule / command scorer |
| missing_cache | 稳定前缀或 projection 重复构造 | 后续引入 cache |

Token 观测的目的不是让普通用户看到所有成本细节，而是让 runtime 和后续评估能回答：

```text
这次任务为什么贵？
贵在 context、工具输出、模型输出、retry，还是 evaluation？
改动后质量是否提升，成本是否也可接受？
```

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

## Event Sampling

完整保存和完整分析是两回事。当前 canonical RuntimeEvent 轻量完整保存，deep evaluation 由 durable `PostTaskJob` worker 执行，普通 TUI 不查询 DebugProjection。`EventSamplingPolicy` 保留为将来出现独立高成本索引时的配置模型；当前没有派生 event index，因此不会为了“实现采样”而丢弃 canonical facts。

若未来增加派生索引，必须采用三层事件策略：

```text
Raw Event
  轻量完整保存，保证恢复和基本 replay。

Indexed Event
  只索引关键决策、高风险动作、异常、失败、人工修正和小比例随机样本。

Evaluation Event
  只进入离线评估、benchmark、regression 和改进候选生成。
```

推荐保留规则：

- risk >= high：必须进入 indexed event 和 evaluation event。
- policy violation：必须进入 evaluation event。
- verification fail / partial / unknown：必须进入 evaluation event。
- goal alignment 低于阈值：必须进入 evaluation event。
- 用户人工纠正：必须进入 evaluation event。
- 普通低风险事件：raw 保存，按比例抽样进入 indexed event。

这样可以保留可回放性，同时避免每个工具流、每段输出、每个 token 都进入高成本分析。

## Context Projection Cache

长上下文会增加成本，也会提高目标漂移风险。当前已使用 `WorkingSummary`、durable compaction、bounded contributor 和基础 `ContextProjection`；`ContextProjectionCacheEntry` 只保留 schema。ContextBuilder 还不是性能瓶颈，启用 cache 会增加 stale context 风险，因此当前设计明确不缓存 prompt projection。

只有在 profiling 证明 context 构造成为瓶颈后，才可按以下链路引入 cache：

```text
RuntimeEvent
-> StateProjection
-> FactGraph / WorkingSummary
-> ContextProjectionCache
-> Prompt
```

`ContextProjectionCache` 的作用：

- 避免相同 state 重复构造 prompt。
- 让模型读取“当前状态”，而不是滚动历史全文。
- compact 前后保持可校验的状态转移。
- 让 debug/audit 能看到“模型当时到底看到了什么”。

失效条件：

- 新 evidence 改变任务事实。
- GoalRecord 或 success criteria 改变。
- ToolResultEnvelope 产生新的 structured facts。
- MemoryCandidate 被晋升或回滚。
- PolicyEvaluation 改变允许范围。

## Evaluation Harness

Evaluation Harness 的输入是完整 `TaskTrace`，其中 event/artifact 是 canonical facts，带 redacted artifact ref 的 `ContextSnapshot` 证明当时实际送入 provider 的内容。当前 minimal runner 仍只把必要事实投影进 evaluation state，不会从摘要伪造精确 prompt，也不会把 projection replay 重命名为 execution replay。

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

内部事件已经由 `golutra-vis` 映射到 OTel-compatible span JSON：

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
- 能持续检查计划和动作是否偏离原始目标。
- 能按风险分级验证，避免每个动作都付出完整审计成本。
- 能区分 raw event、indexed event 和 evaluation event，避免观测成本失控。
- 能证明模型当时看到的 context projection 是什么。
- 能区分失败归因。
- 能 replay 任意关键 turn。
- 能把失败转成 benchmark。
- 能把高质量经验转成受控 memory/skill/policy 候选。
- 能把失败转成 ImprovementCandidate，并通过 regression 与 PromotionDecision 决定是否采用。
- 普通用户不会被 debug 信息干扰。

这些标准中，P2.5 G0-G6 已转成可执行门禁并有 unit/integration/cross-process 回归；P3 本地 Supervisor、不可变 source/bin、preview/canary、launcher 和 rollback 已接入。远端 fleet 与 E5 meta-evolution 仍独立后置。

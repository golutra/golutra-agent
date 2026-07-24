# Golutra 实现蓝图

## 文档定位

本文档把 `ARCHITECTURE.md` 的目标架构收敛成可落地的工程蓝图，回答：

```text
第一阶段先实现什么，
哪些能力同步运行，
哪些能力后台或离线运行，
核心数据结构至少长什么样。
```

P0-P2 骨架到 P3 自进化之间的治理可信性补全不在本文重复展开，统一见 `runtime-governance-completion-design.md`。
五类运行入口的进程模型和跨进程协议不在本文重复展开，统一见 `runtime-entrypoints.md`。

## 第一阶段目标

第一阶段不追求复杂多 agent。目标是跑通单 agent、多入口、可恢复、可验证、可 debug 的核心 runtime；截至 2026-07-16，该阶段及其生产硬化已经完成。

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

第一阶段之后的落地状态：

```text
Open-Endedness / Dynamic Benchmark / Skill Promotion：已完成受控本地最小闭环
GoalLedger / RuntimeGovernor / GoalAlignmentCheck：已进入同步 runtime 边界
VerificationTier：以结构化 check kind 和任务分类进入主链，schema 保留可配置 tier
EventSamplingPolicy / ContextProjectionCache：保留 schema，不启用无收益的派生索引/cache
Plugin/MCP：已完成本地 reviewed package 与 sandboxed stdio 主链
复杂 Multi-Agent Orchestration：非当前产品范围
自动修改或部署 runtime 代码：普通 P0/P1/P2 Runtime 明确禁止；独立 P3 Supervisor 的本地受治理流程见 `self-evolving-runtime-design.md` 和 `supervisor-operations.md`
```

P2 当前状态包含类型、持久状态和受控本地流程；P2.5 已在此基础上完成可信治理闭环：统一 `TaskTraceService`、实际 provider request 的 `ContextSnapshot`、SQLite durable post-task job、客观 assertion、真实 baseline/candidate execution 和 memory quarantine 均已接入。P3 本地 Supervisor 也已接入完整 trace、候选隔离、密封/新鲜门禁、可信构建、内容寻址 release、canary、launcher 和 rollback；远端 fleet 与 E5 meta-evolution 后置。

`ImprovementCandidate`、Evaluation、counterfactual comparison、regression/promotion 和 Evolution 都在后台或显式命令中运行，不污染普通用户同步链路。普通 Runtime 的自动 apply 只允许 clean regression 后的低风险 benchmark state；Skill 必须人工 review，runtime code/policy 放宽不会自动应用。runtime code 候选和连续发布只由独立 Supervisor 承担密封评测、不可变构建、canary 和 rollback，不改变普通任务同步边界。

### 运行入口完成状态

当前五类入口已共享同一 `RuntimeHost`/`RuntimeApplication` 边界：

- `golutra exec` 提供 stdin、JSONL、durable thread resume、output file 和 ephemeral 模式；
- App Server 提供 HTTP/SSE、Unix IPC、WebSocket 和 stdio JSON-RPC；
- Python/TypeScript SDK 提供 thread/turn handle、流式事件、steer、interrupt 和 approval；
- `golutra mcp-server` 默认连接用户级 daemon，并支持显式 remote/embedded；
- `golutra-tui remote` 将终端渲染与远程 Runtime 分离。

这些入口只负责传输、参数和 projection，不复制 RuntimeLane、AgentLoop 或终态验证逻辑。跨进程验收命令和输出语义见 `runtime-entrypoints.md`。

## 第一阶段吸收的架构启示

第一阶段不新增复杂治理层，但必须吸收以下 runtime 硬边界：

- `SessionCommand` 是 CLI / TUI / API / SDK 的唯一入口协议，入口层不能绕过 runtime 自建状态机。
- `RuntimeQuery` 是查询当前 session / task 状态的统一接口；不同前端不能各自维护私有状态快照作为真相。
- 协议类型必须有统一 schema 产物；Rust、TypeScript、Python 侧不能各自手写一套含义接近但字段漂移的契约。
- `ProviderContract` 是 provider 反腐层，统一 stream event、tool call、usage、finish_reason、error、rate limit 和 cost。
- Custom Provider 设置必须协议优先：先选 OpenAI-compatible / Anthropic / Gemini / Vertex AI / genai，再填写 base URL、API key、model、advanced config 和 review；保存前必须使用同一 runtime adapter probe，失败回滚 active selection。
- OAuth 必须由受审计 provider auth method 同时绑定 flow、callback、scope、模型协议和 API endpoint；OpenAI ChatGPT subscription token 固定走 Responses adapter，xAI/Copilot 使用各自注册的 OpenAI-compatible 请求扩展，Custom Provider 不自动推断 OAuth。
- `ToolContract` 先于工具实现定义，明确 schema、错误、取消、重试、幂等、副作用和 artifact 策略。
- `ArtifactRecord` / `EvidenceRecord` 是事实层，raw output 默认进 artifact，模型只读取受控摘要和 evidence refs。
- `VerificationRecord` 决定任务是否完成，模型自然语言不能直接触发成功终止。
- `PolicyEvaluation` 必须在执行层阻断高风险文件、进程、网络、secret 和外部副作用。
- `MemoryCandidate` 不能从 transcript 直接晋升。当前 project candidate 先进入 quarantine，再由独立 evidence 或人工 review 激活；legacy active record 在读取时降级为 quarantine。
- `RuntimeLane` 负责同一 task 的串行执行和运行中输入处理，入口层不能私自排队、注入或中断。
- `LoopGuardRule` 把重复工具失败、空回复、context overflow、max iteration 等循环异常变成显式规则。
- `WorkspaceCheckpoint` 在 coding agent 文件副作用前捕获并持久化 before-image、checksum 和恢复引用；不能污染用户自己的 `.git`。

这些是第一阶段的架构约束，不等于要实现完整 benchmark hardening、复杂 multi-agent、自改进或动态评测系统。

## 治理增强状态

以下能力有架构价值，但不应全部变成昂贵的同步模型调用。当前实现采用确定性同步治理与后台深度评估分层：

```text
GoalLedger
RuntimeGovernor
GoalAlignmentCheck
GovernanceDecision
VerificationTier
EventSamplingPolicy
ContextProjectionCache
```

当前状态：

1. `GoalLedger + GoalAlignmentCheck + RuntimeGovernor` 已在 provider/tool/result/completion 边界执行，不调用额外 judge。
2. 验证已按 plain conversation、workspace objective、workspace change 和 code change 分级，并用 `VerificationCheckKind` 记录客观来源。mutation 不等于 validation；最后一次工作区修改后若缺少新的客观检查，AgentLoop 会以 `RetryScheduled` 最多回送两次验证要求。caller-owned external verifier 直接承担后置检查，不重复要求模型执行 shell；同一检查重跑以最新结果为准。
3. deep PostTaskReview/evaluation 在终态前先持久 enqueue，之后由带 lease/retry/recovery 的 worker 执行；普通 TUI 不查询 debug/evaluation projection。
4. `EventSamplingPolicy` 只保留配置模型；canonical RuntimeEvent 不能采样丢失，当前也没有独立高成本派生索引需要抽样。
5. `ContextProjectionCache` 只保留带 invalidation refs 的模型；当前 ContextBuilder 成本未形成瓶颈，启用 cache 反而会引入 stale context 风险。
6. `CausalLedger` 在 canonical append 前补齐事件 provenance，并把 provider failure、tool completion、verification 和 external evaluation 连接到可审计的因果链；event append 失败会回滚 ledger 索引。
7. deterministic replay 只消费带 source sequence boundary 的 owner-only artifact capsule；`ReplayExecution` 的 divergence/incomplete 不能伪装成 execution-backed regression。
8. 外部评估按精确 `case × partition × provider × seed` 矩阵配对，`minimum_trusted_external_pairs` 的单位是 baseline/candidate pair；digest、runtime identity、trust 和 holdout gate 在 host 侧验证。
9. `exec --run-dir` 的 raw state、full observations、debug export 和 evaluator overlay 可在进程退出后重开；刷新验证旧 event prefix 不变，并以 pending 文件表达未完成收集。

稳定扩展位：

- `LoopDecision.reason` 能记录目标偏移、预算超限、权限阻塞等原因。
- `VerificationRecord` 能记录检查来源和残余风险。
- `DebugProjection` 能展示有界 event 摘要、context、policy 和 verification；它不是完整 task trace，完整性和分页由已实现的 `TaskTraceService` 负责。
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

- 同一工具连续确定性失败时，不能无限重复同一调用；应要求换方法、询问用户或 blocked。同一个 provider response 内的重复调用按一个失败轮次计算，不能在模型收到工具结果前提前触发跨轮重复 guard。
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
  created_before_tool_call_id
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
  block_disposition: recoverable | terminal | null
  reason
  evidence_refs
```

`block_disposition` 只对 `block` 有意义。`recoverable` 仅拒绝当前工具调用并允许模型改正；`terminal` 停止整个任务。为保持安全兼容，反序列化旧 Block 记录时若字段缺失，effective disposition 必须是 `terminal`。

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
- TUI、Web attach、TypeScript/Python SDK 对同一 task 的实时展示，必须来自同一条 event stream 或同一份 projection 查询结果。
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

多前端实时一致性的前提是它们连接同一个 `RuntimeHost` attachment。默认 Embedded 进程可以各自持有 host，但必须共享全局 durable store，并通过 session lease 禁止两个进程同时拥有同一 active session；如果各自创建本地内存 store，则仍只是互不相干的演示 runtime。

第一阶段需要明确以下实现边界：

- `RuntimeHost` 是执行与状态所有权边界，负责接收 `SessionCommand`、驱动 `RuntimeLane / AgentLoop`、写入 event、更新 projection、广播 live event。
- `RuntimeStore` 是 durable facts，不直接等于 runtime host；store 可以恢复状态，但不能替代任务调度、运行中取消、订阅和 provider/tool loop。
- `EventBus` 负责把 durable event 与 live event 统一起来：先 append 到 store，再发布给订阅者；断线重连时按 cursor replay，再接 live stream。
- `EmbeddedTransport` 是 CLI/TUI 默认入口，必须持有 `Arc<RuntimeHost>` 并连接 `$GOLUTRA_HOME/state/runtime.sqlite`，不能只包临时 `RuntimeStore`。
- `UnixIpcTransport` 用于 Unix 本地 daemon，`HttpSseTransport` 用于 Windows/Web/SDK/显式 remote；两者必须先按 cwd attachment，并与 `EmbeddedTransport` 对拍一致。
- `RuntimeClient::subscribe` 的目标语义是 event stream；如果短期保留 snapshot API，也必须新增 live watch 能力，不能让 TUI 长期轮询历史事件。
- cwd thread resolver 从全局 thread index 选择当前 cwd 最近 session/thread；TUI 新建会话可以显式生成新 ID，但首个 prompt 前不持久化 placeholder。

这条边界决定了 TUI 的实施顺序：先让多个入口看到同一 task，再完善终端布局和组件。否则 TUI 会被迫维护自己的状态机，最终和 runtime 脱节。

### 协议与 SDK 约束

第一阶段需要把“runtime 协议”当成独立资产，而不只是 Rust 内部类型：

- `SessionCommand`、`RuntimeQuery`、`RuntimeEvent` 要有稳定 schema 产物。
- TypeScript 与 Python SDK 都从同一 Rust schema 生成类型，生成后必须执行漂移检查，不能手写近似协议。
- 本地入口允许两种运行方式：
  - CLI/TUI 进程内创建 durable Embedded host
- SDK/Web/CLI/TUI 连接显式启动的用户级 `app-server`；Unix CLI/TUI 使用 IPC，其他客户端使用 HTTP/SSE
- 无论哪种运行方式，`task_id`、event 顺序、approval、resume、replay 语义必须一致。

### 协议测试与 smoke 约束

第一阶段除业务测试外，至少还要有三类契约测试：

- schema / fixture 测试：保证协议产物稳定可消费。
- app-server test client：对 `query`、`subscribe`、`approve`、`abort`、`resume` 做 transport 对拍。
- SDK 集成 smoke：保证 SDK 与 runtime 不会在字段和事件顺序上漂移。

### Coding Agent 验证默认值

coding agent 第一阶段默认采用基础客观 evidence gate：

- 代码修改任务至少需要 `diff` 和一类客观验证证据。
- 客观验证证据优先来自 `test`、`lint`、`typecheck`、`build`、`command exit code`。
- 如果没有足够 evidence，任务不能 `stop_success`。
- 无法完成要求的验证时，只能输出 `stop_partial`、`blocked` 或 `stop_failed`。
- 文档/调研型 task 可以允许较弱验证，但 coding task 不应退化为模型自述完成。

该门禁现在由 `VerificationPlan + VerificationAssertion + VerifierRegistry` 补齐当前支持的语义目标验证；无法由现有 verifier 客观证明的标准保持 Unknown/Partial，不会被模型自述强行改成 Pass。

### Coding Agent 记忆默认值

当前 runtime 默认只自动使用：

- `WorkingSummary`
- `CompactionRecord`
- evidence-backed `MemoryCandidate`
- project-scoped quarantine、retrieval、feedback 和 rollback；activation 需要独立 evidence 或 human review，并带 expiry/invalidation

当前明确不自动执行：

- 无 evidence 或未通过 gate 的长期 memory 晋升
- `user/global` 长期记忆自动写入；这些 scope 需要 human review
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

当前“后台可跑”同时具备退出恢复语义：deep evaluation 通过 durable `PostTaskJob` 把普通 task terminal 与 evaluation terminal 分开，新的 Host/daemon 可接管过期 lease。

### 后台或显式治理命令

这些能力用于长期改进，不属于普通任务执行链路：

- replay_runner。
- vcr / golden fixture。
- EvaluationCase / EvaluationRun / EvaluationResult。
- CounterfactualReplay / CausalComparison / SecurityUtilityResult。
- regression suite。
- dynamic benchmark promotion。
- open-ended plan/run；GeneratedTask 只进入隔离 RuntimeHost。
- runtime / prompt / tool schema 改进实验。
- RegressionResult。
- PromotionDecision。

## P2.5 治理可信性阶段（已完成）

P2.5 不再增加并列的治理名词，而是把现有事实和状态机补成可验收闭环：

1. G0 固定 TaskTrace、ContextSnapshot、PostTaskJob、VerificationPlan、RegressionCampaign 和 MemoryClaim 协议，并修正文档能力声明。
2. G1 由统一 `TaskTraceService` 分页关联 event、context、artifact/evidence、verification、evaluation 和 memory lifecycle。
3. G2 将 deep evaluation 改为 SQLite durable job，支持 lease、retry、crash recovery 和显式等待。
4. G3 把 completion criteria 映射为客观 assertion，并用 Evidence/Object/Policy 三维 hard gate 决定终态。
5. G4 为冻结候选启动 baseline/candidate 隔离 RuntimeHost，生成 execution-backed RegressionResult。
6. G5 将 project memory 改为 structured claim 和 quarantine 生命周期。
7. G6 只把 complete TaskTraceBundle 与 execution-backed RegressionResult 接给 P3 Supervisor。

字段、crate 映射和端到端验收以 `runtime-governance-completion-design.md` 为准。当前 gate 已禁止 projection-only、unknown verification、缺 paired execution 或控制面修改进入自动 promotion；可信输入由 P3 独立 Supervisor 消费，具体发布操作见 `supervisor-operations.md`。

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
  recent_events
  event_window_limit
  loop_decisions
  policy_evaluations
  evidence_records
  verification_records
  context_projection
  token_budget
  provider_raw_events
  tool_result_envelopes
```

Debug Projection 只在 debug/audit/replay 模式启用，并且只承担治理摘要与当前事件窗口；typed projection 同时携带 post-task jobs、trace completeness、missing sections 和 retention losses，TUI developer mode 通过 `EventPage` cursor 按需加载更早事件。完整、分页且带缺失原因的事实包由 P2.5 `TaskTraceService` 返回，调用方本地 `DebugExportCoordinator` 再将选定 SessionWindow 按 high-watermark 物化为 `full-redacted` bundle；期间发生新事件时必须标记 incomplete。

## P0 验收矩阵

第一阶段完成时至少覆盖这些硬边界，不用等后续治理增强：

| 场景 | 必须验证 |
| --- | --- |
| 多入口请求 | CLI / TUI / API / SDK 都转成 `SessionCommand`，没有入口私有状态机 |
| 多前端一致性 | 同一 `workspace/session/task` 在 SDK / TUI / Web 查询到相同状态，并能看到同一条运行中事件流 |
| 运行中输入 | active task 运行时，新输入必须被记录为 append / inject / interrupt / reject 之一，且其他前端看到同一状态 |
| provider 正常流 | stream event、usage、finish_reason、tool call 映射进 `ProviderContract` |
| custom provider 验证 | 协议选择、base URL、API key、model、review 都通过校验；OpenAI-compatible/Anthropic/Gemini/Vertex AI/genai 使用实际 adapter probe 后才能保存成 ready |
| provider 异常流 | truncated stream、malformed event、rate limit、network error 都有结构化错误 |
| token 观测 | 每次 provider request 前有 `TokenBudgetSnapshot`，response 后有 `TokenUsageRecord`；usage 缺失时记录 unknown 或估算来源 |
| tool 成功 | `ToolContract` 校验通过，生成 `ToolResultEnvelope`、artifact refs 和 evidence refs |
| tool 失败 | error、timeout、cancelled、blocked 都有明确状态，不把 raw stderr 直接塞进模型 |
| abort / pause | abort 后不能继续产生外部副作用，pause/resume 不破坏 event 顺序 |
| retry | 有副作用的 tool retry 必须依赖 idempotency 或显式阻断 |
| loop guard | 重复工具失败、空回复、context overflow、max iteration 都有明确 `LoopDecision`，不能无界循环 |
| artifact | raw output 可通过 checksum 校验，模型只读取摘要或受控 excerpt |
| workspace checkpoint | 文件副作用前已持久化 before-image、artifact 和可恢复引用，且不修改用户 `.git` |
| verification | 没有足够 evidence 时不能 `stop_success`；工作区最后一次 mutation 后必须有 fresh objective validation，或有 caller-owned external verifier；同一检查只采信最新重跑结果 |
| provenance / replay | 每个 provider lifecycle 必须以 completed/failed 闭合；replay 必须校验 source prefix、artifact checksum 和 request/tool fixture 消费量 |
| external evaluation | evaluator 结果必须绑定 trace digest/runtime identity；回归矩阵缺 cell、untrusted pair 或 holdout 泄漏时只能 `NeedsReview` |
| memory | 只有 evidence-backed project candidate 可按 gate 晋升；user/global 或冲突候选必须 human review，并保留 scope/expiry/rollback |

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
10. `golutra-client` host 子模块：`RuntimeHost`、cwd thread resolver、`EventBus`、全局 `RuntimePaths`。
11. `golutra-client`：统一 `RuntimeClient`、`RuntimeQuery`、event replay 和 live subscription 接口。
12. `golutra-cli` / `golutra-tui`：默认通过 `EmbeddedTransport`，可显式选择 daemon/remote，只消费 command/query/event。
13. `golutra-app-server`：用户级单实例管理多 cwd attachment，暴露 Unix IPC 与 HTTP command/query/SSE stream。
14. `golutra-vis`：DebugProjection、event replay、audit 和 OTel JSON。
15. `golutra-eval` / `golutra-evolution`：ImprovementCandidate、RegressionResult、PromotionDecision、GeneratedTask 与 Skill 生命周期。
16. `golutra-plugin` / `golutra-mcp`：reviewed package、OS sandbox、approval 和统一 ToolContract bridge。
17. TypeScript/Python SDK 与安装/三平台 CI 交付。

入口优先级默认值：

1. `CLI + TUI + EmbeddedTransport`
2. `app-server + UnixIpcTransport/HttpSseTransport`
3. `TypeScript/Python SDK + Web attach`

IDE 产品入口不在当前范围；未来只能复用现有 transport/protocol，不能新增状态机。

## 通过标准

第一阶段完成时必须满足：

- 单个任务能从 CLI/TUI/API 进入同一 runtime。
- 每个 turn 有 durable event。
- 每个 provider 响应都通过 ProviderContract 归一化。
- 每个工具执行前有 ToolContract 和 PolicyEvaluation。
- 每个工具结果有 ToolResultEnvelope。
- 运行中用户输入经过 RuntimeLane 和 BusyPolicyDecision。
- raw output、日志和大内容有 ArtifactRecord，关键结论有 EvidenceRecord。
- 文件副作用前能捕获 WorkspaceCheckpoint before-image，或在执行前明确阻断并说明不可快照原因。
- 每个任务结束有 VerificationRecord。
- 每次循环结束有 LoopDecision。
- 普通用户只看到 UserProjection。
- Debug 模式可以展开 RuntimeEvent、ContextProjection、Evidence 和 Verification。
- 失败任务能 replay 到关键决策点。
- 失败任务能生成至少一个可人工查看的 ImprovementCandidate。

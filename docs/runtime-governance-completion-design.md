# Golutra 治理可感知与可信闭环实施记录

## 文档定位

本文记录 Golutra 的 P2.5 实施结果：把 RuntimeEvent、artifact/evidence、Verification、evaluation 和 memory 骨架，补成开发者能够完整检查、进程退出后仍能继续、并且可以真实验证改动效果的治理闭环。

它位于两个阶段之间：

```text
P0/P1/P2
  可运行、可恢复、具有受控治理骨架的 Coding Agent Runtime

P2.5（本文）
  已完成：完整任务事实包、durable 后台作业、语义验证、真实回归、memory quarantine

P3
  external/internal evolver、密封评测、不可变 release、canary、rollback
```

P3 不能绕过本文。没有完整事实、可靠验证和真实 regression，自进化只会放大误判。

本文与 `initial-implementation-plan.md`、`ARCHITECTURE.md` 共同记录 P2.5 实现真相。后续 P3 本地 Supervisor、external/internal command producer、密封评测、可信构建和版本切换已经独立落地，见 `self-evolving-runtime-design.md` 与 `supervisor-operations.md`。

## 当前事实与缺口

截至 2026-07-24，真实隔离任务已经证明以下主链存在：

- 成功 coding task 会持久化 context、provider、tool、policy、checkpoint、verification、LoopDecision、memory 和 minimal evaluation 事件。
- failed/partial task 会生成 deep PostTaskReview、ImprovementCandidate 和 AutomationCandidate。
- artifact blob、evidence、rollout 和 evaluation state 都会落盘。
- 普通 TUI 与 developer/debug 投影已经分离。
- 大 workspace change payload 已转为 artifact 引用；binary/超预算文件仍保留 before/after checksum。失败复盘使用带恢复/取代状态的 `FailureEpisode`，外部 evaluator evidence 导入后会重新投影 diagnosis/candidate；context snapshot 和 token record 可逐 contributor 对账。

P2.5 验收后，以下 capability truth matrix 记录真实边界：

| 能力 | 当前真实边界 | P2.5 完成门槛 |
| --- | --- | --- |
| 查看一个任务的完整事实 | `DebugProjection` 仍有窗口上限；`TaskTraceService` 按 cursor 返回 page，CLI `trace --full` 聚合所有页 | 已完成：引用可解析或在 `TraceIntegrity` 中明确列为 unresolved/missing；分页不会静默丢失事件 |
| 证明模型当时看到了什么 | provider request 前保存 `ContextSnapshot`、digest、contributor/message/tool manifest；凭据始终脱敏 | 已完成：redacted snapshot 可追溯，restricted raw capture 仍不默认开启 |
| deep evaluation 不因进程退出丢失 | job 与排队事件在同一 SQLite 事务中提交；Embedded host 退出后由新 host/daemon claim | 已完成：lease、retry、recovery、幂等和 `--wait-evaluation` |
| Verification 证明目标正确完成 | `VerificationPlan`/assertion 在 runtime loop 中生成，Evidence/Object/Policy 三维结果共同决定终态 | 已完成当前支持的客观 verifier；无法构造的自然语言标准保持 Unknown/Partial，不伪造 Pass |
| replay/regression 真实重跑候选 | projection replay 只用于观察；regression campaign 建立独立 baseline/candidate workspace 与 RuntimeHost | 已完成：paired execution 必须有独立 trace/verification refs；无 refs 不能 promotion |
| project memory 不污染 | 成功任务只调用 `quarantine`；检索排除 quarantine/expired/rolled_back，激活需要独立任务证据或 human review | 已完成：structured claim、默认 expiry、invalidation、incorrect feedback 回滚和 legacy active migration |
| 自动改进形成可信输入 | G6 gate 消费完整 trace 和 paired execution；候选不能修改 evaluator/sandbox/signer/promotion control plane | 已完成并接入独立 P3 Supervisor；不可变 release、canary、launcher 和 rollback 在独立控制面执行 |

必须固定以下术语边界：

```text
event replay != execution replay
evidence present != objective satisfied
artifact metadata available != artifact content inspectable
candidate proposed != improvement verified
memory gated != memory pollution eliminated
```

### 因果完整性与失败闭合

RuntimeHost 在 append 前维护 per-session/per-task `CausalLedger`，统一补齐
`CausalContext`、父事件和 provider/tool/verification 生命周期链接。ledger
只在 event transaction 成功后推进；重复键、校验失败或存储错误不会把失败
尝试留在后续事件的因果头上。provider 失败也有独立的
`ProviderFailed` 终态，不能只留下一个未闭合的 `ProviderStarted`。

`TraceIntegrity` 的 `missing_causal_links`、`broken_lifecycle_pairs`、
`provenance_mismatches`、`artifact_checksum_failures` 和
`external_overlay_failures` 都是 promotion/complete 的硬输入。debug 窗口
只是一种投影，不代表完整性；完整 trace 必须按 cursor 聚合并校验 event
chain digest。

### Replay 与外部 evaluator 的边界

`ReplayCapsule` 保存 source event prefix 的最后序号和 digest。deterministic
replay 只向 `AgentLoop` 注入 owner-only provider/tool artifact fixture，并
验证请求、工具调用、artifact 所属 session、类型、redaction、大小和
checksum。缺 source boundary 或发生 divergence 时，结果是显式
`Incomplete`/`Diverged` replay record，不得被当成 execution-backed
regression。

外部 evaluator 通过 `ExternalEvaluationRecord` 进入 canonical overlay：host
验证 base trace digest、runtime identity、canonical result digest、trust
level 和 holdout disclosure，再写 `ExternalEvaluationIngested`。回归覆盖按
`case_ref × partition × provider_variant × seed` 展开，每个 cell 必须有
baseline/candidate pair；`minimum_trusted_external_pairs` 的单位是 pair。
不完整矩阵、未签名 holdout、untrusted local result 或缺 paired trace 都只
能得到 `NeedsHumanReview`。

## 成功定义

P2.5 完成后，开发者应能对任意 task 执行一个命令，获得完整且自证一致的事实包：

```text
golutra trace --task <task-id> --full --wait-evaluation
```

该结果必须回答：

1. 用户要求和运行环境是什么。
2. Runtime 实际构造了哪些模型可见消息和工具 schema。
3. provider、tool、policy、approval、checkpoint 分别发生了什么。
4. raw 大输出保存在哪里，模型实际只看到了哪一部分。
5. 每条 completion criterion 由哪个 verifier、artifact 和 evidence 证明。
6. task 为什么结束为 completed、partial、failed、blocked 或 cancelled。
7. post-task 作业是否完成，失败分类和候选是什么。
8. candidate 是否经过真实 baseline/candidate execution。
9. 哪些 memory 仍在 quarantine，哪些已激活、过期或回滚。
10. 哪些事实因 redaction、retention 或权限不可见，不能静默缺失。

普通 TUI 继续只显示 UserProjection。完整事实只在显式 developer/debug/trace/export 入口提供；`/export` 会在调用方本地生成可移交给其他 agent 的脱敏事实包。

### 调试导出与会话窗口（当前实现）

`golutra export <ABSOLUTE_DIR> [--thread-id ID] [--range 1|+N|-N]` 与 TUI `/export` 共用 `DebugExportCoordinator`。它先通过 `SessionWindowRequest` 解析当前 canonical cwd 的 anchor 范围，再读取：

- `manifest.json`：格式版本、选择范围、完整性、redaction、missing、retention loss 和每个 task 的 trace 状态。
- `conversation.md` 与每个 session 的 `conversation.jsonl`：用户/assistant 对话，不包含运行时治理噪声。
- 每个 session 的 `events.jsonl`、`thread.json` 和每个 task 的 `tasks/<task-id>/trace.json`。
- `artifacts/sha256/<digest>`：按 checksum 去重、分块读取、重新计算 SHA-256；`RedactionStatus::Raw` 的 checkpoint 等 blob 不写入，只在 manifest 留下 `omitted_raw` 记录。

目的地必须是调用方机器上的绝对、尚不存在的目录。写入在同文件系统 owner-only 临时目录完成，文件和目录同步后原子 rename；导出失败或保留策略造成的缺失不会被静默伪装成 complete。

每个 session 在读取前固定 event high-watermark，只导出该边界内的事件；task trace/artifact 完成后再次读取 watermark。期间有新 turn/event 时 bundle 仍可落盘，但 `events_complete=false`、`manifest.complete=false`，并在 `missing_data` 记录 moving session，避免把非原子视图误报为完整快照。

Session protocol v3 增加稳定的 `(recency_at, thread_id)` cursor page 与 anchor window：`1` 为单 session，`+N` 为 anchor 加 N-1 个更新 session，`-N` 为 anchor 加 N-1 个更旧 session。Embedded、HTTP/SSE、Unix IPC 和 TypeScript/Python SDK 共用同一请求/响应类型。

## 目标架构

```text
Task Execution Plane
  RuntimeHost / AgentLoop
  ContextBuilder / Provider / Tools / Verification
            |
            v
Canonical Fact Plane
  RuntimeEventLog
  Artifact / Evidence Store
  ContextSnapshot Store
  Durable Job Store
            |
            +---------------------------+
            |                           |
            v                           v
TaskTraceService                PostTaskCoordinator
  complete bundle                 durable evaluation jobs
  pagination                      failure taxonomy
  integrity/redaction             candidate generation
            |                           |
            v                           v
Developer / SDK / Vis           RegressionService
  summary/full/export             isolated baseline/candidate runs
                                      |
                                      v
                               Promotion / P3 Supervisor

MemoryGovernanceService
  proposed -> quarantined -> active -> deprecated/rolled_back
```

新增能力不能复制第二份任务事实库。SQLite RuntimeEventLog 和 artifact store 仍是 canonical facts；trace、evaluation 和 OpenTelemetry 都是可重建投影。

## 深模块与接口

### TaskTraceService

TaskTraceService 是完整可观测性的唯一外部 seam。调用方不需要分别拼 event、context、artifact、evidence、evaluation 和 memory 文件。

```text
TaskTraceService
  read(TaskTraceRequest) -> TaskTracePage
  read_complete(TaskTraceRequest) -> TaskTracePage
  read_artifact(ArtifactReadRequest) -> ArtifactChunk
```

它内部负责：

- cursor pagination、bounded all-pages merge 和完整性判断；cursor 不前进、页身份不一致或页数超限显式失败。
- event、context snapshot、artifact/evidence、verification 和 post-task job 的关联。
- owner/local/remote 权限检查。
- redaction、retention 和 artifact range read。
- checksum、引用闭包和缺失原因校验。
- summary/full/forensic 三种视图。

禁止让 CLI、TUI、SDK 和 `golutra-vis` 各自实现同一套拼装逻辑。

### PostTaskCoordinator

PostTaskCoordinator 管理持久化后台作业，不依赖某个 RuntimeHost 内存任务存活。

```text
PostTaskCoordinator
  enqueue(PostTaskJobSpec) -> PostTaskJob
  claim(WorkerLease) -> Option<PostTaskJob>
  complete(PostTaskJobResult)
  recover(RecoveryPolicy) -> RecoverySummary
```

实现可以位于 `golutra-client` 的独立模块，job repository 位于 `golutra-store`。P3 的 evaluation/build/deploy job 后续复用同一 lease 语义，但不能复用 Runtime task lane。

### VerificationService

VerificationService 隐藏任务分类、assertion planning、verifier adapter 和最终判定。

```text
VerificationService
  plan(TaskVerificationInput) -> VerificationPlan
  verify(VerificationPlan, VerificationFacts) -> VerificationRecord
```

模型可以提出 assertion，但不能自行声明 assertion 已通过。每个 assertion 必须由注册 verifier 产生结果。

### RegressionService

RegressionService 的接口只接收冻结候选和 campaign，不接收“给这个候选一个通过结果”的可变回调。

```text
RegressionService
  run(RegressionCampaign) -> RegressionResult
```

内部负责 fixture clone、baseline/candidate run、资源预算、结果配对、差异计算和清理。event-only projection replay 只用于调试，不能生成 release evidence。

### MemoryGovernanceService

```text
MemoryGovernanceService
  propose(MemoryObservation) -> MemoryCandidate
  review(MemoryReviewInput) -> MemoryDecision
  retrieve(MemoryQuery) -> RetrievedMemorySet
  feedback(MemoryFeedback) -> MemoryLifecycleRecord
```

它必须把候选抽取、晋升、检索和反馈放在同一生命周期规则中，避免 RuntimeHost 直接把任务摘要写成 active memory。

## 完整任务事实包

### TaskTraceBundle

```text
TaskTraceBundle
  schema_version
  generated_at
  workspace_id
  workspace_root_digest
  session_id
  task_id
  task_snapshot
  event_manifest
  context_snapshots
  provider_exchanges
  tool_executions
  policy_and_approvals
  checkpoints
  artifact_manifest
  evidence_records
  verification_plan
  verification_record
  loop_decisions
  post_task_jobs
  evaluation_refs
  improvement_refs
  memory_lifecycle_refs
  integrity
  disclosure
```

### 完整性清单

```text
TraceIntegrity
  event_count
  first_sequence
  last_sequence
  event_chain_digest
  unresolved_refs
  missing_sections
  retention_losses
  redacted_fields
  complete: bool
```

`complete=false` 必须带原因。不能因为 DebugProjection 到达 512 条上限而返回一个看似完整的 JSON。

### 视图级别

| 级别 | 内容 | 默认入口 |
| --- | --- | --- |
| summary | 阶段、计数、状态、验证和风险 | TUI developer panel、Audit |
| full | 全部分页事件、redacted context、artifact/evidence manifest、evaluation | 本地 CLI/SDK developer scope |
| forensic | 受限 raw artifact/context、完整 provenance、retention 详情 | owner-only 本地显式命令 |

remote HTTP 默认最多返回 full-redacted；forensic 需要本地 owner transport 或显式独立授权，不能复用 provider bearer。

当前实现把 artifact chunk 的 `redaction_status` 纳入协议：HTTP handler 在返回内容前拒绝 `Raw`，只有 owner-only Unix IPC 可读取；range read 使用 seek + 有界 buffer，不再为了一个 chunk 先加载整个 blob。完整导出仍对所有 chunk 重新计算 SHA-256。

## ContextSnapshot

每次 provider request 在发送前生成不可变 snapshot：

```text
ContextSnapshot
  snapshot_id
  task_id
  turn_id
  provider_request_id
  provider_id
  model_id
  contributor_manifest
  message_manifest
  tool_schema_digests
  generation_config_digest
  budget_snapshot
  canonical_request_digest
  redacted_request_artifact_ref
  restricted_request_artifact_ref
  created_at
```

`ContributorSnapshot` 至少记录：

```text
name
role
source_refs
included
trimmed
original_estimated_tokens
retained_estimated_tokens
strategy
estimated_tokens
content_digest
redacted_content_ref
invalidation_refs
```

规则：

- canonical request digest 对实际送给 adapter 的请求做稳定序列化后计算。
- redacted request artifact 默认保存，owner-only，允许开发者检查模型可见内容。
- restricted raw capture 默认关闭；显式启用时使用短 retention、owner-only 权限和独立审计事件。
- provider credential、Authorization 和 OAuth token 永远不进入 snapshot。
- tool result 必须记录 raw artifact、structured facts 和真正进入 request 的 excerpt，三者不能混为一个 token 数。

### Context budget enforcement

`ContextSnapshot` 不能只用于事后观察，还要驱动可验证的输入预算：

1. 从已 probe/声明的 provider capability 读取 context window/max output；未知值必须显式配置，不能沿用固定 8192 假装模型能力。
2. 优先使用 model-aware tokenizer；没有 tokenizer 时使用带误差边界的保守估算，并在 snapshot 标记 estimate source。
3. 先预留 output、reasoning 和下一轮 tool-call budget，再给 system/objective/recent turn/evidence/memory/tool excerpt 分配独立预算。
4. 初始 contributor 超限时按稳定顺序 trim；task 内活跃 working set 达到 16,384 个估算 input token 时，用 durable summary 替换旧 assistant/tool message group 并保留最近完整 tool pair。provider hard limit 仍是最终安全边界，无法保留 protected prefix 时才 AskUser/Block。
5. 每次 trim/compact 都记录 contributor、原始/保留 token、策略和 source refs；不能只记录一个总 token 数。
6. provider 返回 actual usage 后生成 attribution delta，用于校准估算和发现 system/context/tool/retry 哪一层持续膨胀。

验收要求：同一长 session 或单个多步骤 task 连续运行时，provider request 不随 transcript/tool-call 数量线性增长；任何退出活跃 working set 的内容仍可通过 artifact/trace 定位，但不会自动重新注入模型。`CompactionRecord` 必须给出 hard budget、实际 compaction limit、target input、逐 message 决策和 replacement artifact，便于基准前后对账。

当前 provider runtime 在 generation config 缺省时使用 protocol capability；无法声明窗口的协议会要求 `context_window_size`。每个 contributor manifest 记录 original/retained token、`include_full`/`retain_head`/`retain_tail` 策略和稳定 source ref。

## Tool 输出与 Artifact 按需读取

在现有 ToolResultEnvelope 上补充硬边界：

```text
ToolOutputBudget
  max_summary_bytes
  max_structured_facts_bytes
  max_excerpt_bytes
  max_raw_artifact_bytes
  truncation_strategy
```

规则：

- summary 和 structured facts 都必须有独立大小上限。
- 超限 structured facts 改存 artifact，并在 envelope 中保留 schema 摘要和 ref。
- model-visible excerpt 必须标记 head/tail/range、原始大小和是否截断。
- 新增受 policy 约束的 `artifact.read` 工具，模型只能按 range 读取需要的片段。
- 外部 MCP、网页和命令输出继续标记 untrusted，不因进入 artifact 就提升 evidence strength。
- artifact blob 读取验证 checksum；缺失或 retention 删除必须进入 TraceIntegrity。

## Durable Post-Task Job

### 状态模型

```text
PostTaskJob
  job_id
  job_kind: deep_evaluation | candidate_generation | regression_execution
  workspace_id
  session_id
  task_id
  input_refs
  status: queued | leased | running | succeeded | failed | cancelled
  attempt
  max_attempts
  lease_owner
  lease_expires_at
  result_refs
  last_error
  created_at
  started_at
  completed_at
```

### 生命周期顺序

```text
VerificationCompleted
LoopDecided
PostTaskJobQueued       <- 必须先 durable
TaskCompleted

worker claim
PostTaskJobStarted
PostTaskReviewed
EvaluationCompleted
ImprovementCandidateCreated（按需）
PostTaskJobCompleted
```

任务 terminal 与 deep evaluation terminal 是两个状态，不能用一个 `TaskCompleted` 假装后台分析已经完成。

### Embedded 与 daemon

- Embedded one-shot 在退出前必须确保 job 已 durable enqueue。
- 普通 `chat` 可以不等待 deep result，但返回 `post_task_status=queued|running|completed`。
- `--wait-evaluation` 和 developer mode 等待有 deadline 的 job terminal。
- 用户级 daemon 持续 claim job；没有 daemon 时，下一个 RuntimeHost 启动会恢复过期 lease。
- worker crash 后只重试无外部副作用或声明幂等的 job。
- deep evaluation 事件使用 store 分配的 sequence，不依赖旧 host 的内存原子计数。

## 语义 Verification

### VerificationPlan

```text
VerificationPlan
  plan_id
  task_id
  task_class
  criteria
  assertions
  policy_assertions
  required_artifact_types
  generated_by
  verifier_versions
  created_at
```

每条 completion criterion 映射为一个或多个 assertion：

```text
VerificationAssertion
  assertion_id
  criterion_id
  kind
  subject
  expected
  verifier_id
  required_evidence_strength
  blocking
```

criteria 来源和演进必须可审计：

- 用户显式给出的完成条件优先，Runtime 补充 policy、安全和交付类强制条件。
- Runtime 可以从任务文本推导可验证条件，但有歧义或会改变交付范围时必须询问用户，不能静默扩大目标。
- 模型可以提出 assertion，只有 VerifierRegistry 能产生通过结果。
- plan 创建、修订和废弃都写 event；任务终态引用确切 plan revision，避免结束时临时降低标准。
- `plain_conversation` 不要求工具 evidence，只验证 provider 产生了可交付回复且没有 policy failure。
- `read_only_analysis` 只对其事实性结论要求来源/evidence；`workspace_change` 和 `code_change` 才强制 file/diff/command/test 等客观 verifier。

第一批 verifier：

| Verifier | 证明内容 |
| --- | --- |
| FileStateVerifier | 文件存在、内容 digest、路径和修改范围 |
| DiffVerifier | 实际 diff、禁止路径、意外文件变化 |
| CommandExitVerifier | 结构化 argv、exit code、timeout、stdout/stderr artifact |
| TestVerifier | 测试发现、执行、通过/失败/跳过数量和目标关联 |
| DiagnosticVerifier | compiler/linter/typecheck 诊断变化 |
| SchemaVerifier | 结构化输出和协议 fixture |
| PolicyVerifier | approval、sandbox、secret 和网络约束 |
| DeliveryVerifier | 用户要求的文件、artifact 或输出是否真正交付 |

最终结果至少分成三个维度：

```text
EvidenceStatus
ObjectiveStatus
PolicyStatus
```

只有三者都满足 blocking assertions 才能 `StopSuccess`。有工具 evidence 但目标 assertion 未满足只能 Partial/Fail。

禁止规则：

- 不能因为执行了 `write_file` 就默认目标验证成功。
- 不能因为 shell 命令名称包含 test/check 就默认通过；必须检查 exit code 和解析结果。
- 不能把模型最终文字作为 coding task 的唯一 assertion。
- 无法构造可靠 assertion 时必须 AskUser 或保留 residual risk，不能伪造 Pass。

当前 hosted task 只采用 payload 中显式 `completion_criteria`，不再注入“有任意 evidence 即完成”的固定条件。Delivery assertion 要求关联到通过的 matching check；Test criterion 只接受 `objective:test:*`，shell test 还必须同时满足 exit code、timeout/cancel 和“至少执行了一个测试”的输出解析。普通对话仍可只凭非空 assistant response 完成。

调用方现在可以通过 `ExternalVerificationSpec` 声明独立于模型的客观检查。Runtime 在候选回复产生后、终态判定前，以结构化 argv 执行检查，并把 exit code、timeout/cancel、有界输出 artifact 和 evidence 写回同一事实链。该检查不经过模型工具审批，因为命令来自受信任调用方；但 cwd 必须位于 workspace 内，仍使用无网络 sandbox，且不能绕过模型工具的 Block/Deny 策略。外部检查名称固定为 `objective:test:external_verifier`，因此可满足 code task 的 `tests_or_diagnostics`，失败则形成 blocking assertion。

## 真实 Regression Execution

### Replay 模式分级

| 模式 | 是否重执行 | 可作为 promotion evidence |
| --- | --- | --- |
| projection | 否，只重建事件/视图 | 否 |
| simulated | 使用固定 provider/tool fixture | 低风险候选的辅助证据 |
| fixture_execution | 在隔离 checkout 真实执行 baseline/candidate | 是 |
| live_provider | 使用受预算的真实 provider 重跑 | 仅作为组合证据 |

当前 `TrajectoryReplay` 的 event/artifact 统计能力应明确命名为 projection replay。真正的 RegressionResult 必须引用 execution run。

### RegressionCampaign

```text
RegressionCampaign
  campaign_id
  candidate_id
  candidate_digest
  baseline_version
  environment_recipe
  case_refs
  replay_modes
  provider_matrix
  seeds
  resource_budget
  hard_gates
  started_at
  completed_at
```

```text
RegressionExecution
  execution_id
  campaign_id
  case_ref
  role: baseline | candidate
  runtime_version
  workspace_snapshot_digest
  task_trace_ref
  verification_ref
  cost_latency_ref
  status
```

`task_trace_ref` 不能指向函数返回后即删除的临时 runtime。当前实现会在隔离 home 回收前，将 complete trace 和其引用的 redacted artifact blob 打包为父 workspace 的 content-addressed `artifact://regression-trace/...`；缺 blob、trace 不完整或 bundle 超过 64 MiB 时 execution 保持 Inconclusive。

执行规则：

1. candidate 必须包含实际 patch/config artifact 和 digest，不能只有失败摘要。
2. baseline 和 candidate 使用同一 fixture、预算、provider config 和 verifier version。
3. 每个 run 启动独立 RuntimeHost 和 sandbox workspace。
4. 结果比较覆盖 objective、security、cost、latency、tokens、tool count 和 artifact correctness。
5. event-only replay、单次 LLM judge 和来源 case 通过都不能单独晋升。
6. runtime code candidate 在 P3 前保持 human review，不自动 apply。

当前 campaign 要求非空 `candidate_files`，按排序后的路径/内容计算 canonical SHA-256，并拒绝调用方声明 digest 不一致或应用后 workspace digest 未变化的 no-op。CLI 使用 `golutra eval regress <candidate-id> --candidate-files <JSON_FILE> [--candidate-digest sha256:...]` 提交路径到内容的 JSON map。每个 case 的 expected/observed verdict 来自 baseline/candidate `RegressionExecution.status`，case hard gate 同时验证不同的持久 trace ref 和 workspace delta；旧 durable evaluation result 只保留 fixture/security 辅助检查，不再充当 observed verdict。

## Memory Quarantine

### 生命周期

```text
proposed
-> quarantined
-> active
-> deprecated | rolled_back | expired
```

默认规则：

- 单个成功 task 只能产生 quarantined project memory，不能直接 active。
- generic objective/final-message/tool-summary 不构成稳定 memory；必须抽取结构化 claim。
- 自动激活至少需要两个独立 task 的一致 evidence，或一次显式 human approval。
- observation 类 memory 默认 30 天 expiry；配置/代码事实绑定 file、lockfile 或 config digest 作为 invalidation refs。
- user/global scope 永远显式人审。
- contradiction 不能只按文本高相似度判断；先比较结构化 subject/predicate/object 和有效期，再用语义检索辅助。

当前 runtime 的显式 human activation 要求交互 actor、`human_approval=true` 和非空 reason；自动路径会验证至少两个真实、不同 task 的 Pass verification、非空 evidence，并要求 supporting objective 与 memory claim 内容相关。MemoryStore 不再把任意非空 reviewer 字符串自动视为人审。
- retrieval 默认排除 quarantined、expired、rolled_back 和 invalidated memory。

### MemoryClaim

```text
MemoryClaim
  subject
  predicate
  object
  scope
  source_task_refs
  evidence_refs
  confidence
  valid_from
  expires_at
  invalidation_refs
```

检索结果必须解释：命中了什么 claim、为什么相关、是否接近过期、有哪些负反馈。错误反馈立即隔离，不等下一轮批处理。

## Projection 与产品体验

### UserProjection

保持当前简洁体验：

- 用户输入和 assistant 结果。
- 必要工具进度、approval 和 residual risk。
- 不显示 context snapshot、token attribution、candidate 和 job 内部状态。

### DeveloperProjection

继续显示固定高度摘要：

- fact/event/artifact/evidence 数量。
- provider、token、tool、verification 和 LoopDecision 摘要。
- post-task job 状态和 trace completeness。
- 最近事件。

当前 `DebugProjection` 还包含 typed `post_task_jobs`、`trace_complete`、`missing_sections` 和 `retention_losses`。TUI 固定摘要显示 terminal jobs 与 completeness；event window 尚未分页完成时显式记为 `event_window` missing，而不是显示假 complete。

它不是完整数据容器。选择详情时通过 TaskTraceService 分页读取，不把全量 JSON 塞进 TUI state。

### CLI / SDK

建议入口：

```text
golutra trace --task <id> --view summary
golutra trace --task <id> --full --wait-evaluation
golutra trace --task <id> --forensic --output <path>
golutra artifact get <artifact-id> --offset <n> --length <n>
golutra eval job <job-id>
golutra eval run-regression <candidate-id>
golutra memory candidates
golutra memory review <candidate-id>
```

TypeScript/Python SDK 从同一 schema 生成 `taskTrace`、`artifactChunk`、`postTaskJob`、`verificationPlan` 和 `regressionCampaign` 类型，并分别提供 `completeTaskTrace` / `complete_task_trace` 聚合所有 cursor page。

## 协议扩展

新增 query：

```text
TaskTraceSummary
TaskTracePage
TaskTraceIntegrity
ContextSnapshots
ArtifactManifest
ArtifactChunk
PostTaskJobs
VerificationPlan
RegressionCampaign
MemoryCandidates
```

新增 command：

```text
RunVerification
WaitPostTaskJob
RetryPostTaskJob
RunRegressionCampaign
ReviewMemoryCandidate
ExpireMemory
```

新增 event：

```text
ContextSnapshotCreated
PostTaskJobQueued
PostTaskJobStarted
PostTaskJobCompleted
PostTaskJobFailed
PostTaskStageFailed
VerificationPlanned
VerificationAssertionCompleted
RegressionCampaignStarted
RegressionExecutionCompleted
MemoryCandidateQuarantined
MemoryActivated
MemoryInvalidated
```

协议版本升级必须带 fixture、Rust/TypeScript/Python 生成物和旧 state migration 测试。

## 与现有 crate 的映射

| Crate | P2.5 已实施内容 |
| --- | --- |
| `golutra-core` | VerificationPlan/Assertion、ContextSnapshot、PostTaskJob、MemoryClaim 基础类型 |
| `golutra-protocol` | typed Context/Evaluation projection、TaskTrace、artifact chunk、job、regression、memory query/command/event schema（runtime protocol v7） |
| `golutra-store` | context snapshot、job lease、trace ref closure、artifact range read、migration；`RuntimeRepositories` 五类事实访问 seam |
| `golutra-context` | canonical request snapshot、contributor manifest、tool output budget |
| `golutra-runtime` | task 前 verification plan、criterion/assertion 终态判定；completion/context guard/retry/trace/verification 模块边界 |
| `golutra-verify` | VerifierRegistry 和首批客观 verifier |
| `golutra-client` | `RuntimeApplication/GovernedRuntime` facade；command/query/session/execution/trace/post-task/governance/regression 独立模块和服务；Embedded 主路径通过 facade |
| `golutra-eval` | execution-backed campaign/result，projection replay 降级为调试输入 |
| `golutra-memory` | quarantine、structured claim、expiry/invalidation、review lifecycle |
| `golutra-tools` | structured facts 上限、artifact range read、claim-specific evidence |
| `golutra-tui` | 普通模式不变；developer mode 增加 completeness/job 状态和详情分页入口 |
| `golutra-vis` | TaskTraceBundle 到 audit/OTel/lineage 的纯投影 |
| `golutra-app-server` | trace/artifact/job endpoint 与 owner/remote 权限限制 |

本阶段没有新增 `golutra-jobs`：durable job 目前由 `golutra-store` 持久化、`golutra-client` worker 编排，已被 evaluation/evolution 主链复用。只有 release job 形成独立复用需求时才重新评估拆 crate。

应用层重构已把 command/query/session/trace/governance 入口固定在 `RuntimeApplication`，post-task worker 固定在 `PostTaskCoordinator`；`RuntimeHost` 仍是 lane、事件事务锁和 task supervision 的唯一 owner。该边界避免为了缩短文件而引入第二套事实或执行状态。

## 数据迁移与切换

P2.5 已迁移已有事实，并且不保留两套运行语义：

- SQLite 已增加 context snapshot、post-task job、verification plan/assertion 和 trace reference 表；旧 RuntimeEvent 保持可读，不重写 canonical history。
- 历史 task 没有 ContextSnapshot 时，TaskTraceBundle 返回 `complete=false` 和 `missing_sections=[context_snapshot]`，不能从 rollout 文本伪造快照。
- 旧 RegressionResult 统一标记 `evidence_kind=projection_only`，可用于调试和候选发现，但不能满足新的 promotion hard gate。
- 现有自动 active 的 generic project memory 迁移为 `quarantined_legacy` 或过期；只有重新获得独立 evidence 或人工 review 才能 active。
- durable worker 已取代 `TaskCompleted` 后直接 `tokio::spawn` deep evaluation 的旧路径，不保留双执行 fallback。
- MemoryGovernanceService 已取代 RuntimeHost 直接 propose 后立即 promote 的旧路径；旧 `promote` API 已删除。
- DebugProjection wire 保持有界摘要语义；新增 TaskTrace query，不把旧 query 静默改成可能超大响应。
- protocol/schema version 明确拒绝不认识新 hard-gate 字段的旧 remote client；本地 migration 失败保持原数据库不变并返回可行动错误。

已覆盖旧 projection/legacy memory fixture、重复启动幂等和进程恢复；迁移不访问 provider credential，也不把 restricted artifact 复制进普通 event。未来 protocol downgrade 仍按版本范围拒绝。

## 实施阶段与验收状态

依赖关系已按以下顺序完成：G0 固定协议后，G1/G2/G5 并行落地；G3 使用 G1 的事实引用；G4 使用 G1/G2/G3；G6 在 G0-G5 门禁完成后接入 promotion。

### G0：事实命名与协议（已完成）

- 已修正文档中的完成声明。
- 已固定 TaskTrace、ContextSnapshot、PostTaskJob、VerificationPlan、RegressionCampaign 和 MemoryClaim schema。
- 已增加 capability truth matrix、schema 生成和兼容 fixture。

验收结果：schema roundtrip、旧 projection 兼容反序列化和术语边界测试通过。

### G1：完整任务事实包（已完成）

- provider request 前持久化 redacted ContextSnapshot 和 digest。
- TaskTraceService 关联事实并集中实现分页聚合；CLI `trace --full`、内部 regression 和 SDK all-pages helper 复用相同完整性规则。
- artifact range read、checksum 和 retention disclosure。
- summary 净化 event payload 并省略 context/artifact/evidence，full 返回脱敏 manifest；forensic 仅允许 owner-only local IPC/embedded，raw capture 缺失时 `complete=false`。
- CLI/TypeScript/Python SDK 已接入 typed ContextProjection、EvaluationProjection、TaskTrace 和 artifact chunk；普通 TUI 仍只显示 UserProjection。

验收结果：512+ 事件 cursor 回归、artifact checksum/range、HTTP/Unix IPC trace 对拍通过；历史缺失 section 会明确返回 `complete=false`。

### G2：Durable Post-Task Job（已完成）

- SQLite job table、lease、retry、recovery 已实现；claim 在事务查询中按 workspace_id 过滤，worker 执行前再次复核 partition。
- runtime terminal fact 先写入，随后 active worker 以 settlement barrier 完成 best-effort enqueue；deep evaluation 仍由 durable job 执行，治理失败只产生诊断/integrity fact，不改写 TaskCompleted。
- Embedded/daemon/remote 查询和等待语义一致。

验收结果：Host restart 恢复 queued job、lease exhausted terminal、幂等 candidate 和跨进程查询测试通过。

### G3：语义 Verification（已完成当前范围）

- task class 与 criterion/assertion plan 已固定。
- 首批客观 verifier、目标路径/内容和命令分类已接入。
- StopSuccess 使用 Evidence/Object/Policy 三维 hard gate；unsupported assertion 保持 Unknown。
- provider/runtime error 和 cancellation 也生成固定失败 plan、assertion 和 VerificationRecord，提前失败不能绕过验证事实。

验收结果：错误文件、错误内容、`echo tests passed`、未运行目标测试回归通过。

### G4：真实 Regression（已完成当前范围）

- task-level candidate artifact/digest、逐 `case_ref` baseline/candidate 隔离执行、durable regression trace bundle、paired result、资源预算和 hard gate 已实现；任一 case 缺 pair 时持久化 NeedsHumanReview，而不是中断在无 PromotionDecision 状态。
- candidate file set 首次执行时冻结为不可变 `candidate_patch_set` artifact，并通过独立 `CandidatePatchFrozen` 事件进入完整性对账；campaign 和每次 candidate execution 都读取同一 artifact bytes。
- deep failure 的 runtime-change candidate 自动进入 regression/promotion dispatcher；缺冻结补丁或隔离执行失败时形成 blocked `RegressionResult(NeedsReview)` 与 `PromotionDecision(NeedsHumanReview)`，不自动 apply runtime code。
- runtime 源码版本候选走独立 P3 路径：stable release 与 candidate evaluation build 中的 `golutra-eval-worker` 是两个实际不同的 binary；它们在独立 home/workspace 和外层断网 OS sandbox 中运行。Supervisor 不向 worker 发送 assertion、partition、真实 case id 或 holdout 答案，并在进程外验证 workspace outcome、完整 trace、VerificationRecord 和引用 artifact blob。
- candidate worktree 由 Supervisor 从 epoch 的 immutable parent release source 创建；冻结时按完整文件摘要集合计算 canonical changed paths。proposal 的 `target_paths` 只是待核对声明，不能掩盖 evaluator、sandbox、release、policy 或其他 sealed 路径的新增、修改和删除。
- secondary evaluation/memory store 负责 durable lifecycle，但不能伪装成事件源；EvaluationProjection 必须把 review/result/candidate/regression/decision 与 source-task RuntimeEvent 对账。缺事件、未终态 job/evaluation 或 unresolved regression artifact 都让 TaskTrace integrity 失败。
- regression Pass/Fail/NeedsReview 都必须生成显式 PromotionDecision；失败不能只修改 candidate status。

验收结果：无 execution refs、trace 不完整、verification unknown 和控制面修改均不能 Promotion；baseline/candidate workspace digest 独立；同一个已编译 RuntimeHost 的两次 replay 不再能冒充 runtime-code candidate evidence。

### G5：Memory Quarantine（已完成）

- structured claim、quarantine、expiry/invalidation 和 review 已实现。
- legacy generic active memory 会在读取时迁移到 quarantine。
- retrieval/feedback 会记录来源，incorrect feedback 立即回滚。

验收结果：单次成功只 quarantine，独立 evidence/human review 后才 active；expiry、legacy migration 和 incorrect feedback 测试通过。

### G6：P3 输入门禁（已完成并接入独立执行面）

- promotion gate 只接受 complete trace、Pass verification 和 Supervisor 持久化的 paired execution refs；公开 CLI 不接受手工 `EvaluationInput` 或外部 `BuildReport`。
- candidate control-plane mutation 会被拒绝，unknown/inconclusive 不会自动晋升。
- P3 的本地 Supervisor、OS-enforced build、内容寻址 release、preview/canary/stable pointer、launcher 和 rollback 已消费该门禁；远端 fleet 和 E5 meta-evolution 不属于本地完成范围。

## 端到端验收场景

1. 写文件任务产生完整 TaskTraceBundle；context、tool excerpt、artifact、evidence、verification 和 job refs 全部可解析。
2. 产生 700 条事件的任务通过 cursor 分页导出，event_count 与 SQLite 相同，没有静默丢失前 188 条。
3. one-shot CLI 在 terminal 后退出，deep evaluation 由下一 host 恢复且不重复生成 candidate。
4. Agent 修改了错误文件，即使 write tool 成功也不能通过 FileStateVerifier 和 DiffVerifier。
5. shell 执行 `echo tests passed` 不能被 TestVerifier 当成测试成功。
6. 2 MiB shell 输出只给模型有界 excerpt，raw blob 可按 range 读取且 checksum 一致。
7. failed task 产生带 patch digest 的 candidate，baseline/candidate 在同一 fixture 重跑并输出两个独立 trace ref。
8. 单次成功任务只形成 quarantined memory；第二个独立 evidence 或人工 review 后才 active。
9. 普通 TUI 的 transcript 不增加治理噪声；developer mode 能看到 trace completeness 和 job 状态。
10. summary/full/forensic 三种导出都不包含 provider credential；HTTP forensic 请求返回 403，restricted capture 未启用时 owner forensic 返回带原因的 `complete=false`。

## 不接受的实现

- 把 DebugProjection 的 limit 调大后宣称完整 trace 已解决。
- 把完整 provider request 无限制写入普通 event payload。
- 让 CLI、TUI、SDK 分别拼装自己的 TaskTraceBundle。
- 继续把后台 tokio task 当作 durable job。
- 继续用“工具执行成功”代替“用户目标满足”。
- 把 event/artifact summary 命名为 exact execution replay。
- regression 不启动 baseline/candidate RuntimeHost。
- 单次成功任务直接写 active、无 expiry 的长期 memory。
- 为了减少污染而删除 canonical RuntimeEvent。
- 绕过 P3 Supervisor、TrustedBuilder、sealed/fresh gate 或 canary 直接发布 runtime code。

## 一句话结论

```text
Golutra 已把已有事实连成可分页、可校验的 TaskTrace，
把后台分析变成 durable job，把当前支持的 Verification 变成客观断言，
把 regression 变成真实隔离执行，把 memory 先隔离再晋升；
可信事实已经接入受预算的 P3 本地执行面；远端 fleet 与 E5 meta-evolution 继续独立演进。
```

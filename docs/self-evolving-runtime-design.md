# Golutra 自进化 Runtime 与连续发布设计

## 文档定位

本文定义并记录 Golutra 的 P3 受治理执行面：利用任务执行和开发者观察链路生成针对 agent 自身的代码候选，在隔离环境完成评估、构建、发布、监控和回滚，再由新版本承接下一轮任务。具体命令和持久化边界见 `supervisor-operations.md`，外部研究证据见 `research-self-evolving-agent-systems.md`。

截至 2026-07-17，P2.5 前置门禁和 P3 E0-E4 的本地控制面已经落地：`golutra-supervisor`、`golutra-release`、sealed `golutra-eval-worker`、internal/external command producer、完整 TaskTrace ingestion、候选冻结、stable/candidate 不同二进制的 paired execution、sealed/fresh disclosure gate、OS-enforced producer/TrustedBuilder、内容寻址 release、stable/preview/canary pointer、rollback 及 `golutra-launcher` 都有实现和回归测试。当前进程不会热替换自己；已有任务继续由旧版本完成，下一次 launcher 启动读取 stable pointer。复杂远端集群调度、签名/TUF 服务和 E5 meta-evolution 仍不是当前本地交付范围。

目标闭环：

```text
跑任务
-> 采集 RuntimeEvent / Verification / Evaluation
-> 识别重复失败和能力边界
-> 内部或外部 Evolver 生成 Golutra 代码候选
-> 隔离构建与分层评估
-> PromotionDecision
-> 不可变 release
-> shadow / canary / blue-green
-> promote 或 rollback
-> 下一轮任务
```

## 当前代码闭环

```text
golutra trace --full
-> observation_from_trace
-> EvolutionSupervisor::observe_trace
-> start_epoch（预算 + deadline）
-> InternalCommandProducer / ExternalCommandProducer
-> register_candidate（worktree + allowlist + digest freeze）
-> evaluate_suite（stable/candidate eval-worker + Supervisor 外部 assertion）
-> TrustedBuilder（Seatbelt/bubblewrap，无网络）
-> ReleaseStore::build_checked（source/binary checksum）
-> preview -> canary -> promote / rollback
-> golutra-launcher 读取 stable pointer 并 exec 下一版本
```

普通 RuntimeHost 不持有 stable pointer 写权限。Supervisor 也不读取 provider credential；command producer 子进程必须位于 OS-enforced sandbox，只有 candidate worktree 和独立 scratch 的写权限，没有网络，并使用清空敏感值后的环境，只能提交候选提案。版本评测同样不读取真实 provider credential：stable/candidate worker 使用 deterministic mock、独立 home/workspace 和外层断网 sandbox。assertion、sealed 答案、Supervisor state 与 release pointer 不挂载给 worker。process-only backend、越界 worktree、timeout 和超量输出都会失败。

## 对“自进化”的理解

自进化不是一个永不返回的 AgentLoop，也不是让当前进程直接覆盖正在执行的二进制。它由四个不同时间尺度的循环组成：

| 循环 | 作用 | 时间尺度 |
| --- | --- | --- |
| Task Loop | 完成一个用户或 benchmark 任务 | 秒到小时 |
| Learning Loop | 从多条 trajectory 形成候选改进 | 多任务或按事件触发 |
| Release Loop | 评估、构建、部署和回滚一个版本 | 分钟到天 |
| Meta Loop | 改进候选生成和选择机制 | 多个 release epoch |

“持续循环”应由持久化 scheduler 在新事实到达后创建有限 `EvolutionEpoch`。单个 epoch 必须有预算和终态，不能递归自调用直到资源耗尽。`source_version` 是硬边界：failure cluster 不跨 release 混合计数；开始 epoch 时 opportunity 的 source release 必须仍是 stable pointer，否则旧 opportunity 失效并等待新版本上的独立观察。

## 最重要的架构约束

### 自修改与自部署必须分权

内部 CandidateProducer 有两种边界。当前已实现的是离线 `InternalCommandProducer`：它与 external command producer 一样没有网络或 credential。受信 RuntimeHost 持有 provider credential、只向模型暴露 child checkout 工具的在线 internal evolver 尚未接入 scheduler；接入后也只能返回 `CandidateProposal`，不能获得评测与发布权限。

但是，当前候选不能同时拥有以下权限：

- 读取 sealed holdout。
- 修改 evaluator、selection policy 或查询预算。
- 修改系统 sandbox root 或 release policy。
- 读取构建、签名和更新密钥。
- 修改 stable release pointer。
- 删除上一稳定版本或回滚状态。

这些权限属于独立的 `EvolutionSupervisor`。否则候选可以通过修改日志、测试发现或评分器制造虚假提升。

### 当前进程永远不原地替换自己

内部自进化产生的是后继候选，不是对当前二进制的热修改：

```text
Runtime Vn
  -> child worktree
  -> Candidate Vn+1
  -> trusted evaluation/build/deploy
  -> new process Vn+1
```

已经开始的任务在 Vn 完成或被安全取消；新任务在 stable pointer 切换后进入 Vn+1。任何版本都必须保留其 parent、artifact digest 和 rollback ref。

## 目标组件

```text
golutra-supervisor                 不参与普通任务执行
  ObservationIngestor              读取脱敏 runtime facts
  OpportunityMiner                 聚类失败和能力边界
  CandidateBroker                  调度内部/外部 producer
  CandidateArchive                 保存 lineage 和多目标表现
  EvaluatorBroker                  隔离公开/密封/shadow 评测
  PromotionGate                    生成晋升决策
  TrustedBuilder                   生成 provenance 和不可变 artifact
  DeploymentController             preview/canary/promote/rollback
  EpochScheduler                   持久化有限循环

golutra-runtime Vn                可演化对象
  AgentLoop
  Context / Provider / Tools
  RuntimeEvent / Verification
  InternalCandidateProducer

ExternalCandidateProducer          可替换的外部 coding agent
  -> 与 internal producer 输出同一 EvolutionCandidate
```

第一版可以把 Supervisor 的模块放在一个小型 Rust binary 中，但它的源码、安装目录和权限必须与 candidate worktree 分离。候选不能修改正在运行的 Supervisor。

## 内部与外部自进化

### External Evolver

外部 Evolver 是独立 coding agent。它读取经过脱敏和压缩的观察窗口，在隔离 worktree 中修改 Golutra。

优势：

- Golutra 当前版本损坏时仍可修复。
- 可使用不同模型和 scaffold，降低同源盲点。
- 更容易保持 evaluator 与被测对象分离。

### Internal Evolver

内部 Evolver 使用当前稳定 Runtime 自己完成“修改 Golutra”任务：

```text
source snapshot + observation bundle + improvement hypothesis
-> RuntimeHost Vn
-> tools in isolated child worktree
-> patch artifact
-> EvolutionCandidate(producer_kind = internal)
```

限制：

- child worktree 不挂载 production state、credentials 或 holdout。
- 只能修改 candidate allowlist。
- 输出 patch 后终止，不自行构建生产 release 或切流。
- 其结果与 External Evolver 使用完全相同的评估和发布门禁。

### 两类 Producer 的协作

同一个 opportunity 可以同时产生 internal/external candidates。可进行：

- 互相 code review，但 reviewer 结论不是 release evidence。
- patch 合并，合并结果必须作为新 candidate 重新评估。
- champion/challenger 比较。
- 跨模型、跨 producer 的迁移测试。

不能因为两个 agent 意见一致就跳过 sealed gate。

## 从开发者观察链路生成改进

### 输入事实

ObservationIngestor 只消费已存在的结构化事实，并且输入必须来自完整 `TaskTraceBundle`：

- `RuntimeEvent`
- `ArtifactRecord` / `EvidenceRecord`
- `VerificationRecord`
- `PostTaskReview`
- `EvaluationResult`
- `RegressionResult`
- `PromotionDecision`
- provider/tool/token/cost/latency/security 投影
- 人工纠正、回滚和失败恢复记录

`RegressionResult` 还必须引用真实 baseline/candidate execution；projection replay summary 只能用于调试和机会发现，不能作为发布证据。用户聊天文本和私有源码不能未经脱敏直接进入长期 evolution dataset。

### Opportunity 生成

单次失败默认只形成 case，不立即改 runtime。OpportunityMiner 在以下条件触发代码候选：

- 相同 failure taxonomy 在多个独立任务重复出现。
- 一个严重安全或恢复缺陷有确定性复现。
- 某 capability slice 长期停留在 near-miss。
- 成本、延迟或工具次数稳定偏离基线。
- 回滚、人工接管或用户纠正集中指向同一层。

每个 opportunity 必须先形成因果假设：

```text
EvolutionOpportunity
  opportunity_id
  source_version
  source_task_refs
  observation_refs
  failure_cluster
  suspected_layer
  causal_hypothesis
  expected_effect
  confidence
  privacy_class
  proposed_eval_slices
```

没有可证伪假设的“感觉可以优化”不进入 runtime code lane。

## 候选与 Archive

```text
EvolutionCandidate
  candidate_id
  parent_release_id
  producer_kind: internal | external
  producer_version
  source_commit
  patch_artifact_ref
  target_paths
  change_class
  opportunity_refs
  generation_model
  generation_config_digest
  risk_level
  state_migration_ref
  rollback_plan
  created_at

CandidateArchiveEntry
  candidate
  lineage_parent_ids
  build_digest
  public_eval_summary
  capability_slice_scores
  novelty_descriptor
  descendant_success_rate
  novel_descendant_count
  improvement_cost
  rollback_rate
  status
```

Archive 不只保存当前最高分版本。父代选择同时考虑：

- quality / security / cost / latency 的 Pareto frontier。
- capability slice 的覆盖和结构新颖性。
- lineage 的后代成功率，即 metaproductivity。
- 尚未充分探索的分支。
- 候选生成和评估成本。

sealed holdout 的精确分数不能进入 parent fitness，只能提供晋升 verdict。

## 候选工作区与修改边界

候选必须从已签名 parent source digest 创建独立 worktree：

```text
$GOLUTRA_HOME/evolution/supervisor/worktrees/<candidate_id>/
$GOLUTRA_HOME/evolution/supervisor/artifacts/<candidate_id>/
```

worktree 只能由 Supervisor 从 epoch 绑定的 immutable stable release source 创建。冻结时不能信任 producer 声明的 `target_paths`：控制面会比较 parent/candidate 的完整文件集合和内容摘要，得到 canonical changed paths，再逐项应用 allowlist/denylist，并要求声明覆盖每一个真实变化。暗改或删除 sealed 文件即使未声明也会失败。

初始 allowlist：

- runtime loop 与 context strategy。
- provider/tool orchestration。
- 普通工具实现和结构化输出策略。
- TUI developer projection。
- 低风险性能和可靠性修复。

默认 denylist：

- Supervisor、EvaluatorBroker、HoldoutVault。
- sandbox root、permission/policy root。
- auth、credential、signer、updater。
- hidden dataset 和 evaluator fixture。
- release pointer 与上一稳定 artifact。
- CI/release gate 的删除或放宽。

denylist 变更进入独立高风险 lane，必须人工审核，不能无人值守晋升。

## 防测试集过拟合

### 五层数据隔离

| 层 | Candidate 可见性 | 反馈 | 用途 |
| --- | --- | --- | --- |
| Search/Evolution | 可见 | 逐 case 详细反馈 | 复现来源问题并生成 patch |
| Development Regression | 可见 | 详细反馈 | 防已知能力退化 |
| Sealed Release Holdout | 不可见 | `pass/fail/inconclusive` 和粗粒度类别 | 晋升门禁 |
| Shadow/Fresh | 不可见 | 不回流当前 family | 防 adaptive overfit |
| Canary | 仅匿名聚合 | 部署健康与 rollback signal | 验证真实分布 |

任何 case 一旦把详细失败信息返回 CandidateProducer，就标记为 `exposed`，只能降级为 development regression，不能继续作为 release evidence。

### Holdout 查询预算

```text
EvaluationDisclosureBudget
  dataset_id
  candidate_family_id
  query_count
  exact_feedback_count
  aggregate_feedback_count
  threshold_feedback_count
  maximum_queries
  exhausted_at
```

同一 candidate family 不能通过大量小改动反复探测 sealed set。预算耗尽后只能：

- 等待新鲜 holdout。
- 使用公开数据继续改进。
- 停止该 evolution epoch。

### 候选冻结后的新测试

候选 patch hash 冻结后，EvaluatorBroker 再生成或选择：

- metamorphic cases。
- property tests。
- mutation tests。
- 随机化输入和故障注入。
- 时间后移的新 issue。
- 跨 repo 结构、语言和依赖版本的 case。

这样候选无法按已知 test path 和 expected output 写特判。

### 评估器独立性

- 客观编译、schema、property、differential 和安全 invariant 优先。
- LLM judge 只能作为组合信号，不能单独晋升高风险代码。
- baseline/candidate 使用相同模型、seed、预算、工具和环境做 paired comparison。
- 随机系统运行多个 seed，报告区间而不是单次最好成绩。
- evaluator、harness 和 runtime patch 禁止在同一个 candidate 中共同变化。
- 修改 evaluator 必须单独版本化，由上一稳定 evaluator 评估。

### 多目标门禁

不能只优化 pass rate。PromotionGate 至少比较：

```text
quality
security
utility
cost
latency
token usage
tool count
artifact correctness
state compatibility
maintainability
```

安全、隐私、sandbox、凭据和 release integrity 是不可补偿 hard gate。质量提升不能抵消这些回归。

## Evaluation Campaign

```text
EvaluationCampaign
  campaign_id
  candidate_id
  baseline_release_id
  evaluator_version
  dataset_partition_refs
  disclosure_budget_ref
  environment_digest
  provider_matrix
  seeds
  resource_budget
  started_at
  completed_at

GeneralizationGateResult
  campaign_id
  development_result
  sealed_verdict
  fresh_result
  cross_repo_result
  security_result
  migration_result
  cost_latency_result
  confidence
  verdict
```

最小顺序：

```text
static policy
-> build/fmt/clippy/unit/schema
-> source failure replay
-> development regression
-> adversarial/security
-> sealed holdout
-> fresh/metamorphic
-> state migration dry-run
-> preview smoke
```

任一 hard gate 失败立即拒绝，不能靠后续平均分补回。

## Release 与部署

### 版本化安装

当前安装脚本直接复制 binary。自部署需要改为内容寻址目录和稳定 launcher：

```text
$GOLUTRA_HOME/releases/<release_digest>/bin/
$GOLUTRA_HOME/releases/<release_digest>/release.json
$GOLUTRA_HOME/releases/stable
$GOLUTRA_HOME/releases/preview
```

稳定的 `golutra-launcher` 读取受 Supervisor 管理的 stable pointer。候选只写新 release 目录，不能覆盖 launcher 或当前 stable 目录。

### ReleaseManifest

```text
ReleaseManifest
  release_id
  parent_release_id
  candidate_id
  source_commit
  source_digest
  dependency_lock_digest
  toolchain_digest
  artifact_digests
  protocol_version_range
  state_schema_version_range
  migration_plan_ref
  provenance_ref
  update_metadata_ref
  rollback_release_id
  created_at
```

TrustedBuilder 从只读 source commit 和 lockfile 构建，Cargo target 只进入临时 scratch，校验后的 binary 再由 Supervisor 写入 owner-only artifact staging；构建前后 source digest 必须与 frozen candidate 一致。它生成 SLSA-style provenance。TUF-style metadata 授权 artifact digest 并防止未授权回退、冻结和混搭。签名密钥不进入 Golutra provider credential store。

### 本地 blue-green

不需要 Kubernetes 也可以实现同样语义：

1. stable daemon A 继续服务。
2. preview daemon B 使用独立 socket/port 和 state copy 启动。
3. B 完成协议握手、迁移 dry-run、smoke 和 shadow replay。
4. canary cohort 的新 attachment 进入 B，A 继续接管 stable cohort。
5. 达到成功阈值后停止给 A 新任务，等待 active task drain。
6. 原子更新 endpoint/stable pointer 到 B。
7. 保留 A、旧 artifact 和兼容 state snapshot 到 rollback window 结束。

普通 Embedded CLI/TUI 在启动时读取 stable pointer；已经运行的 TUI 在当前版本完成，不做进程内热替换。

### 自动回滚

触发条件至少包含：

- 启动或 protocol handshake 失败。
- state migration 或 projection 恢复失败。
- task failure / abort / takeover 率越界。
- 安全、sandbox、凭据或 policy violation。
- cost/latency/resource 明显超预算。
- canary 与 stable 的质量差异超过容忍度。

`inconclusive` 必须暂停，不能默认成功。

## State Schema 与回滚

自部署前必须替换当前 ad-hoc schema ensure 方式，建立正式 migration ledger：

```text
StateSchemaManifest
  current_version
  minimum_reader_version
  minimum_writer_version
  forward_migrations
  rollback_strategy
  irreversible
```

无人值守部署只允许：

- expand-contract 中的 expand 阶段。
- N 与 N-1 都能读取的状态。
- 在 state clone 上验证过的迁移。
- 有一致性校验和恢复快照的变更。

不可逆 migration、credential schema、sandbox/policy root 和 updater metadata 变更必须人工审核。二进制可回滚不代表状态可回滚，这一项必须作为独立 hard gate。

## EvolutionEpoch 状态机

```text
observing
-> planning
-> generating
-> evaluating
-> awaiting_promotion
-> building_release
-> previewing
-> canarying
-> promoted
```

允许的终态：

```text
no_opportunity
no_improvement
rejected
budget_exhausted
inconclusive
paused_for_review
rolled_back
promoted
```

预算：

```text
EvolutionEpochBudget
  max_candidates
  max_generations
  max_provider_tokens
  max_cost
  max_build_minutes
  max_holdout_queries
  max_canary_releases
  deadline
```

外层 scheduler 只有在新任务、新失败、新鲜 benchmark 或人工恢复后才创建下一 epoch。连续运行不等于无条件忙循环。

## Protocol 与事件

建议新增命令：

```text
evolution.observe
evolution.plan
evolution.generate
evolution.evaluate
evolution.promote
release.build
release.preview
release.canary
release.promote
release.rollback
```

建议新增事件：

```text
EvolutionOpportunityIdentified
EvolutionEpochStarted
CodeCandidateProposed
CandidateBuildCompleted
EvaluationCampaignCompleted
GeneralizationGateCompleted
ReleaseBuilt
PreviewStarted
CanaryStarted
ReleasePromoted
ReleaseRolledBack
EvolutionEpochCompleted
```

Supervisor 保留独立 append-only control log，并把脱敏摘要镜像为 RuntimeEvent，供 `golutra-vis` 和 developer mode 展示。候选不能改写 control log。

## 与当前 crate 的映射

| 当前模块 | P3 扩展 |
| --- | --- |
| `golutra-eval` | execution-backed regression 和 P2.5 promotion 输入 |
| `golutra-evolution` | GeneratedTask、skill 和当前 Runtime 内受控探索 |
| `golutra-client` | 完整 TaskTrace export 和实际 baseline/candidate RuntimeHost |
| `golutra-store` | schema migration ledger、release/evolution refs；不保存 signer secret |
| `golutra-sandbox` | candidate build/run profile 与 denylist |
| `golutra-governor` | evolution budget、risk lane、kill switch |
| `golutra-app-server` | 现有版本继续服务 active attachment；部署调用方负责 drain 后再切 pointer |
| `golutra-vis` | lineage、campaign、deployment、rollback 投影 |
| `golutra-supervisor` | opportunity、有限 epoch、internal/external producer、archive、sealed/fresh gate、CLI 和 append-only control log |
| `golutra-release` | TrustedBuilder、manifest、不可变 source/bin、stable/preview/canary pointer、launcher 和 rollback |

## 分阶段实施

### E-1：P2.5 前置门禁

- `TaskTraceBundle.complete=true`，所有缺失、redaction 和 retention loss 可解释。
- deep evaluation 使用 durable `PostTaskJob`，进程退出后可恢复。
- completion criteria 由客观 assertion 验证，不能只依赖“有工具 evidence”。
- RegressionResult 引用配对 baseline/candidate execution。
- project memory 默认 quarantine，不把单次成功直接注入后续候选上下文。

任一条件未满足时，Supervisor 只能观察和生成待人工研究的 opportunity，不能自动评测、构建或发布 runtime code candidate。

### E0：契约和控制面骨架（已完成）

- 固定 EvolutionCandidate、Campaign、ReleaseManifest、DeploymentObservation schema。
- 新增 Supervisor 与 append-only control log。
- 建立正式 state schema version/migration ledger。
- 所有部署仍人工执行。

### E1：外部候选闭环（已完成 OS 隔离的本地命令 producer 范围）

- External Evolver 从 developer observation bundle 生成隔离 patch。
- 自动运行 L0/L1，人工批准 sealed evaluation 和 release。
- Candidate archive 保存 lineage 和多目标指标。

### E2：密封评测与不可变 release（已完成本地范围）

- RuntimeEvaluationSuite、五类 partition、threshold-only result 和 disclosure budget。
- stable release 与 candidate evaluation build 分别运行 sealed eval-worker；Supervisor 外部验证 workspace/trace/verification/artifact blob，手工 EvaluationInput 不构成入口。
- TrustedBuilder、provenance、release digest、stable/preview 目录。
- migration clone 与 rollback artifact。

### E3：preview/canary/rollback（已完成稳定指针与 launcher 范围）

- 本地 blue-green release pointer 与 checksum-validating launcher；不包含常驻部署编排 daemon。
- shadow/canary observation contract、手工 promote/rollback；active-process drain、指标采集和自动回滚 scheduler 由部署调用方负责，当前 Supervisor 不伪造这些能力。
- 只允许无不可逆 migration 的低/中风险 allowlist 代码自动发布。
- promotion 至少需要一条当前 release 健康样本且所有 hard health gate 健康；rollback 校验 active pointer 所有权。

### E4：离线内部 CandidateProducer（已完成统一 trait、命令协议与隔离边界）

- Runtime Vn 在 child worktree 修改自身。
- 与 external candidates 同门禁竞争或合并。
- producer 本身的效果进入 archive，但无部署权限。
- 持有 provider credential 的 RuntimeHost internal evolver 和自动 scheduler 仍是后续产品层，不属于当前离线 command adapter。

### E5：开放式 archive 与 meta-evolution（后续研究范围）

- Pareto + diversity + metaproductivity parent selection。
- 候选生成器/meta-agent 可以作为独立候选演化。
- 控制面升级继续走上一稳定控制面和人工 review lane。

## 验收场景

1. 一个重复 tool output 污染问题生成 opportunity，External Evolver 产生 patch，经 regression 后 preview，但 sealed 失败而拒绝。
2. Internal Evolver 在 child worktree 修复确定性 runtime bug，candidate 无法读取 hidden fixture 或 release key。
3. 一个只对来源测试特判的 patch 通过 development，却被候选冻结后生成的 metamorphic case 拦截。
4. preview 对旧 runtime.sqlite migration 失败，stable daemon 不受影响且无数据损失。
5. canary 质量正常但成本越界，自动切回 stable。
6. 新 release 晋升后下一轮任务由新版本处理，正在运行的旧任务不中途换 binary。
7. candidate 试图修改 evaluator、sandbox root 或 stable pointer，在 build 前被 policy hard gate 阻断。
8. 连续多个 epoch 没有统计可靠提升时进入 `no_improvement`，不会无限消耗 token 和构建资源。

## 不接受的实现

- 当前 agent 同时修改自身、测试、评分器和部署器。
- 在主 Git 工作区直接覆盖用户改动或强制更新 main。
- 只因来源失败 case 通过就部署。
- 把 sealed holdout 逐 case 日志返回生成器。
- 用同一个模型的一次 judge 结果决定高风险晋升。
- 只保留最新版本，部署后立即删除 parent 和 state snapshot。
- 在 active task 中途替换 binary。
- 将 provider key、signing key 或 TUF root key 挂载给 candidate。
- 对不可逆数据库迁移执行无人值守 canary。
- 用一个没有预算和终态的 while loop 表示“持续自进化”。

## 一句话结论

```text
Golutra 的可信自进化，不是让 agent 获得修改一切的权限，
而是让内部和外部 agent 都能持续提出后继代码候选，
再由独立、不可被本轮候选修改的评估与发布控制面，
把真正可泛化、可回滚的改进变成下一稳定版本。
```

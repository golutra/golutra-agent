# Agent 改进闭环架构规格

## 文档定位

本文档定义 Golutra 如何从一次任务失败或低质量轨迹中产生可验证的 agent 改进。

runtime code 自动修改、密封评测、不可变 release、canary 和下一版本接管任务的 P3 目标见 `self-evolving-runtime-design.md`；外部研究依据见 `research-self-evolving-agent-systems.md`。
execution-backed regression 和可信晋升输入的 P2.5 实施记录见 `runtime-governance-completion-design.md`。

阶段边界说明：

- 本文档描述当前改进闭环和仍保留的 P3 目标态。
- 当前已做到 `PostTaskReview -> candidate -> isolated baseline/candidate execution -> paired RegressionResult -> PromotionDecision -> 受限 benchmark apply/rollback` 的受控状态机。
- projection replay 仍只复用 event/artifact facts；需要晋升的候选由 `golutra-client` 启动独立 baseline/candidate RuntimeHost，二者不共享 workspace、home 或 trace。
- “可自动晋升”是长期能力预留，不代表当前默认实现会自动 redeploy 或自动替换线上执行版本。

## 当前实现边界

截至 2026-07-24：

- failed/partial trajectory 可生成 `ImprovementCandidate`、`BenchmarkPromotion`、`GeneratedTask` 和对应 `AutomationCandidate`；成功且有 evidence 的 trajectory 可生成 `SkillCandidate`。
- GeneratedTask 只有通过 budget、novelty、difficulty、fixture-only 和 no-external-side-effects gate 才会在隔离目录、deterministic mock provider、同一 RuntimeHost/Verification 主链中执行；结果和 verification ref 持久化到 evolution state。
- SkillCandidate 可以 stage 为 owner-only `SKILL.md`，但必须有 project scope、evidence、rollback metadata、regression refs 和显式 reviewer；review 后 checksum 未变化才可 install，安装后只按 objective 相关性注入，支持 rollback。
- 中高风险 prompt/policy/tool schema/runtime code 候选仍不会自动 apply；没有自动改 runtime 代码或自动部署新二进制的路径。
- 只有低风险 benchmark candidate 在 clean regression 后可由 system reviewer approve；apply 只更新 workspace evaluation dataset 状态，不执行任意代码。
- candidate 状态转换受约束，不能跳过 regression/promotion gate；apply 后可 rollback，原因和 applied version 会持久化。
- `RuntimeGovernor` 在 provider/tool/result/completion 阶段执行确定性 token/cost/tool/time budget、policy/security risk 和目标对齐检查，但不自动生成或部署改动。
- deep review 在 TaskCompleted 落盘后通过 durable PostTaskJob 执行；worker 使用 lease/retry/recovery。若进程在终态提交与 job 入队之间退出，下一 Host/daemon 会按 workspace 扫描 pending terminal fact 并幂等补建 job。
- deep failure 的 `ImprovementCandidate` 与 `RuntimeChange AutomationCandidate` 使用同一 candidate id 和同步状态；diagnosis 或 external evaluation 更新后，摘要、证据、回归计划与 rollback ref 同步刷新。
- runtime change 候选由 dispatcher 自动推进。存在 `candidate_patch_set` 时使用冻结 bytes 启动隔离回归；不存在可执行补丁时写入真实的 blocked `RegressionResult(NeedsReview)` 和 `PromotionDecision(NeedsHumanReview)`。该路径不调用 `CandidateApplied`。

核心原则：

```text
PostTaskReview 不是终点。
只有失败能变成 benchmark，改动能变成 candidate，
candidate 能经过 regression 和 PromotionGate，
才算真正具备 agent 改进能力。
```

## 完整链路

```text
Task Execution
-> RuntimeEvent
-> VerificationRecord
-> minimal PostTaskReview
-> deep PostTaskReview
-> FailureTaxonomy
-> ImprovementCandidate
-> Replay / Benchmark
-> RegressionResult
-> PromotionDecision
-> Apply Change
-> Monitor New Version
```

## ImprovementCandidate

复盘不能只写“建议优化”，必须明确改什么、为什么改、怎么回滚。

```text
ImprovementCandidate
  id
  source_task_id
  source_failure_ids
  target_type: prompt | tool_schema | policy | memory | provider_route | context_rule | runtime_code
  target_id
  proposed_change
  expected_effect
  risk_level: low | medium | high | critical
  evidence_refs
  causal_evidence_refs
  counterfactual_result_refs
  benchmark_refs
  rollback_plan
  status: proposed | testing | rejected | promoted
```

示例：

```text
source_failure: search tool 返回过长导致 context 污染
target_type: tool_schema
proposed_change: search 默认只返回 top 5、summary、artifact_ref
expected_effect: 降低 token、减少无关上下文
counterfactual_result: 同一批 case 下 token 下降，质量和安全结果无明显回归
rollback_plan: 恢复旧 search result policy
```

## Causal Evidence

高价值候选不应只来自复盘文字，还应尽量绑定反事实证据：

```text
CounterfactualReplay
-> CausalComparison
-> ImprovementCandidate.causal_evidence_refs
```

示例：

- 修改 memory 检索阈值前后，对同一批 case 比较成功率、token、误召回和安全风险。
- 修改工具输出策略前后，对同一批 case 比较 token、证据质量和任务通过率。
- 修改 provider route 前后，对同一批 case 比较质量、延迟、成本和 tool calling 稳定性。

当前 `compare_counterfactual` 仍只对调用方提供的 baseline/variant durable facts 做 projection comparison，不声称自行重跑；真正的 candidate regression 由 `run_regression_campaign` 启动 paired RuntimeHost。无法控制变量或没有 paired execution 时必须保持 inconclusive，只有引用真实 execution run 的结果才能进入 PromotionDecision。

## RegressionResult

候选改动必须通过回归验证，不能只看单个失败样本。

```text
RegressionResult
  candidate_id
  baseline_version
  candidate_version
  cases_run
  passed_cases
  failed_cases
  regressions
  cost_delta
  latency_delta
  quality_delta
  security_delta
  causal_comparison_refs
  verdict: pass | fail | needs_review
```

最低要求：

- 至少跑来源失败轨迹。
- 高风险候选必须跑相关历史 benchmark。
- 改 prompt、tool schema、policy、provider routing 时必须记录成本和失败回归。
- 涉及 context、memory、tool policy、provider route、prompt 或 security policy 的候选，优先使用 CounterfactualReplay 做对照。
- 不能用同一个模型 judge 的一句话作为唯一判断。

`run_regression` 只负责对已记录 execution facts 做纯比较；冻结候选的“跑”语义由 `RuntimeHost::run_regression_campaign` 提供。首次接收 `candidate_files` 时先生成不可变、checksummed `candidate_patch_set` artifact 与 `CandidatePatchFrozen` 事件，campaign 只引用该 artifact 和 canonical digest，后续请求不能替换 bytes。campaign 的每个 durable `case_ref` 都使用同 fixture、同预算、同 verifier version 建立独立 baseline/candidate workspace 与 RuntimeHost；complete trace 和引用 blob 在临时 home 删除前打包进父 workspace governance artifact，再把带 `case_ref` 的 paired refs 写入 evaluation store。任一 case 缺完整、可持久读取的 pair 时 verdict 为 `NeedsReview`，并继续产生显式 `PromotionDecision`。

## PromotionDecision

通过 regression 不等于自动进入主系统。晋升必须可审计、可回滚。

```text
PromotionDecision
  candidate_id
  decision: approve | reject | needs_human_review
  reason
  reviewer: system | human | agent
  applied_version
  rollback_ref
  expires_at
```

## 自动化边界

### 可自动晋升

低风险、可回滚、影响范围小的候选可以自动晋升：

- 工具输出截断策略。
- debug 展示规则。
- 低风险 project-scope memory。
- provider routing 小权重调整。
- benchmark case 记录。

### 必须人审

以下候选必须经过 human review：

- 放宽 policy。
- 删除或覆盖长期 memory。
- 修改核心 system prompt。
- 修改 tool schema 的破坏性字段。
- 修改 runtime code。
- 修改 sandbox / permission 规则。
- 跨 project/global 作用域的 memory 或 policy。

## 分阶段落地

已完成的基础阶段：

```text
失败任务
-> deep PostTaskReview
-> ImprovementCandidate
-> 人工查看
```

已完成的受控状态机骨架：

```text
ImprovementCandidate
-> projection replay / durable fact gate
-> RegressionResult
-> PromotionDecision
```

P2.5 可信闭环已完成：

```text
ImprovementCandidate + candidate digest
-> baseline/candidate isolated execution
-> paired trace / verification refs
-> execution-backed RegressionResult
-> PromotionDecision
```

自动 dispatcher 的保守分支同样是完整闭环：

```text
ImprovementCandidate
-> RuntimeChange candidate
-> candidate_patch_set present ? isolated regression : blocked regression
-> PromotionDecision
-> never auto-apply runtime code
```

已完成的受治理 Evolution/Skill 阶段：

```text
GeneratedTask -> isolated RuntimeHost -> Verification
SkillCandidate -> stage -> regression-backed human review -> install/rollback
```

本地 P3 Supervisor 已能对低/中风险 runtime candidate 做密封评测、可信构建、canary、stable pointer 切换和 rollback；组织级远端 fleet 监控、签名服务与跨主机自动 redeploy 仍不在当前范围，不能被解释为已经具备。

## 与其他系统的关系

| 系统 | 关系 |
| --- | --- |
| RuntimeEvent | 提供失败轨迹和事实来源 |
| VerificationRecord | 判断任务是否达成 |
| PostTaskReview | 生成失败归因和候选改进 |
| Evaluation Harness | replay、benchmark、regression |
| MemoryGovernance | 管理 memory 候选和晋升 |
| Policy System | 管理 policy 候选和人审 |
| PromotionGate | 决定是否进入默认系统 |
| ArtifactStore | 保存 raw evidence、diff、log、fixture |

## 判断标准

合格的 agent 改进闭环必须满足：

- 能从失败轨迹生成明确候选改动。
- 能说明改动目标和预期效果。
- 能 replay 来源失败。
- 能跑相关 regression。
- 能记录通过、失败和回归。
- 能决定晋升、拒绝或人审。
- 能回滚已晋升改动。
- 能区分低风险自动晋升和高风险人审。

这些验收标准的 P2.5 当前范围已经通过：真实 regression、durable deep job、完整 task facts、verification gate 和 memory quarantine 均有实现与回归测试。P3 本地 Supervisor 已在此基础上实现双 producer contract、密封/新鲜评测、可信构建、不可变 release、canary、launcher 和 rollback；普通 Runtime 仍不能自行发布代码。

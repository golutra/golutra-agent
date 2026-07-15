# Agent 改进闭环架构规格

## 文档定位

本文档定义 Golutra 如何从一次任务失败或低质量轨迹中产生可验证的 agent 改进。

阶段边界说明：

- 本文档描述的是完整改进闭环的目标态。
- 当前已做到 `deep PostTaskReview -> candidate -> durable replay/counterfactual comparison -> regression -> PromotionDecision -> apply/rollback` 的受控闭环。
- 当前 replay 复用 event/artifact facts；GeneratedTask 可以在独立 fixture RuntimeHost 中执行，但不会用真实用户 provider 或主 workspace 伪装成 deterministic replay。
- “可自动晋升”是长期能力预留，不代表当前默认实现会自动 redeploy 或自动替换线上执行版本。

## 当前实现边界

截至 2026-07-15：

- failed/partial trajectory 可生成 `ImprovementCandidate`、`BenchmarkPromotion`、`GeneratedTask` 和对应 `AutomationCandidate`；成功且有 evidence 的 trajectory 可生成 `SkillCandidate`。
- GeneratedTask 只有通过 budget、novelty、difficulty、fixture-only 和 no-external-side-effects gate 才会在隔离目录、deterministic mock provider、同一 RuntimeHost/Verification 主链中执行；结果和 verification ref 持久化到 evolution state。
- SkillCandidate 可以 stage 为 owner-only `SKILL.md`，但必须有 project scope、evidence、rollback metadata、regression refs 和显式 reviewer；review 后 checksum 未变化才可 install，安装后只按 objective 相关性注入，支持 rollback。
- 中高风险 prompt/policy/tool schema/runtime code 候选仍不会自动 apply；没有自动改 runtime 代码或自动部署新二进制的路径。
- 只有低风险 benchmark candidate 在 clean regression 后可由 system reviewer approve；apply 只更新 workspace evaluation dataset 状态，不执行任意代码。
- candidate 状态转换受约束，不能跳过 regression/promotion gate；apply 后可 rollback，原因和 applied version 会持久化。
- `RuntimeGovernor` 在 provider/tool/result/completion 阶段执行确定性 token/cost/tool/time budget、policy/security risk 和目标对齐检查，但不自动生成或部署改动。

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

当前 `compare_counterfactual` 会从 baseline/variant durable run 生成 CausalComparison；无法控制变量或没有 paired run 时保持 inconclusive。只有通过反事实对照或 clean regression 的 candidate，才适合进入 PromotionDecision。

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

已完成的受控最小阶段：

```text
ImprovementCandidate
-> replay / regression
-> RegressionResult
-> PromotionDecision
```

已完成的受治理 Evolution/Skill 阶段：

```text
GeneratedTask -> isolated RuntimeHost -> Verification
SkillCandidate -> stage -> regression-backed human review -> install/rollback
```

组织级持续监控和自动 runtime redeploy 不在当前本地产品范围；高风险候选保持 human review，不因为阶段名称被解释为可自动晋升。

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

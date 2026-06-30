# Benchmark Hardening 规格

## 文档定位

本文档定义 Golutra 如何避免 benchmark 被 scaffold、harness、答案泄漏、judge 偏差和运行环境噪声污染。

主架构见 `evaluation-observability.md` 和 `agent-improvement-loop.md`。

## 核心原则

```text
benchmark 分数不是能力本身。
没有 hardening 的 benchmark，只是在奖励某种实现技巧。
```

## 第一阶段必做

第一阶段不需要完整 benchmark 平台，但至少要固定 benchmark 元数据和基础防污染检查。

最低范围：

- benchmark metadata
- harness / scaffold 标识
- tool budget / attempt count / runtime / cost
- leakage checks
- judge checks

## BenchmarkRun

```text
BenchmarkRun
  benchmark_id
  dataset_version
  harness_version
  scaffold_id
  scaffold_version
  model_id
  provider_id
  tool_budget
  attempt_count
  runtime_ms
  cost_usd
  artifact_delivery_status
  score
  failure_taxonomy
  leakage_checks
  judge_checks
```

## 必须记录的元数据

这些字段没有，就不应该把分数当成正式结论：

- `dataset_version`
- `harness_version`
- `scaffold_id`
- `model_id`
- `provider_id`
- `tool_budget`
- `attempt_count`
- `runtime_ms`
- `cost_usd`

## 需要重点防的污染

### Answer Leakage

要防：

- benchmark 答案被 prompt、fixture、tool output 或缓存带回模型
- 评测样本直接暴露在 artifact 或检索层

### Judge Pollution

要防：

- judge 输入被 agent 输出格式诱导
- 同一个模型 judge 自己的答案且无额外 evidence

### Harness / Scaffold Inflation

要防：

- 分数提升其实来自 scaffold 变厚，不是 agent 变强
- 不记录 harness 版本，导致历史分数无法对比

## 最低检查项

第一阶段至少要有：

- `leakage_checks`
  - answer leakage
  - test-hook injection
  - hidden fixture exposure
- `judge_checks`
  - judge input sanitization
  - evidence-backed grading
  - no single-model sentence as sole verdict

## 推荐分层

后续 benchmark 建议分成：

- `release benchmark`
  决定是否晋级
- `shadow benchmark`
  不参与优化，只防过拟合
- `regression set`
  防止已知能力退化
- `adversarial set`
  测试边界与脆弱点

## 第一阶段落地建议

- 先把失败任务沉淀成 replay / regression case。
- benchmark 与真实任务轨迹要能通过 artifact/evidence 建关联。
- 评测结果必须记录 harness 和 scaffold，而不是只记 score。
- 分数变化必须同时看：
  - `score`
  - `cost`
  - `runtime`
  - `artifact_delivery_status`
  - `failure_taxonomy`

## P0 验收口径

- benchmark run 能唯一定位 dataset / harness / scaffold 版本。
- score 旁边必须能看到 cost 和 runtime。
- judge 结论不能脱离 evidence 独立存在。
- regression 通过不等于 release 通过。
- 同一项提升如果来自 scaffold 变厚，必须能被识别出来。

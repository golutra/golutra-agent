# Agent Open-Endedness 架构设计

## 文档定位

这份文档回答一个问题：

```text
在现有 Agent Runtime Operating System 基础上，
如何引入 Agent Open-Endedness，
让系统不是只会完成用户给定任务，
而是能持续发现新任务、新失败、新技能和新评测，
并把这些经验转化成长期能力增长。
```

Open-Endedness 不等于让 agent 无限乱跑，也不等于让模型自己改自己。它更接近一套受控的开放式学习系统：

```text
生成任务
-> 选择刚好有学习价值的任务
-> 执行与验证
-> 发现能力边界
-> 提取技能
-> 固化 benchmark
-> 形成 ChangeManifest
-> 回归验证
-> 扩展能力边界
```

在 Golutra 里，它不应该替代 Runtime Kernel、Decision Audit 或 Evaluation Harness，而应该作为 `Evolution System` 的高级形态。

## 当前实现状态

截至 2026-07-15，Golutra 已完成受控的本地 Evolution/Skill 闭环：

- `golutra-evolution` 从 durable evaluation state 读取 GeneratedTask，计算 lexical novelty、difficulty、CapabilityFrontier 和 CurriculumItem；只有 fixture-only、no-external-side-effects 且位于配置难度区间的任务可被选择。
- OpenEndedBudget 限制生成数、选中数、每任务工具数和 wall-clock；越界计划被拒绝，不会无界探索。
- 每个选中任务在 `$GOLUTRA_HOME/state/workspaces/<cwd-hash>/evolution-runs` 的隔离目录中启动独立 RuntimeHost，强制 deterministic mock provider、内置工具和无网络环境，不触碰用户主 workspace。
- run、plan、novelty、curriculum、environment recipe、frontier、execution 和 verification ref 持久化到 owner-only `evolution.json`，CLI/transport/TypeScript/Python SDK 可查询与驱动。
- SkillCandidate 必须经过 stage、regression-backed human review、checksum install，安装后只对匹配目标注入最多 3 条 context contributor，并支持 rollback。
- Evolution 不会自动修改 prompt、policy、provider route、runtime code 或主 workspace；网络探索、environment mutation 和自动二进制部署不在当前范围。

## 前沿共识

### 1. 开放式系统需要自动课程

POET 和 Enhanced POET 的核心思想是同时演化环境和 agent。系统持续生成新的挑战，只保留那些既有新颖性、又处在当前能力边界附近的任务。

对 Golutra 的启发：

- 不要只等用户给任务。
- 要从历史失败、低覆盖 benchmark、真实项目变更、工具能力缺口中自动生成任务。
- 任务生成必须受预算、权限、环境和验证约束。

### 2. 技能库必须从成功轨迹中沉淀

Voyager 的关键不是单次完成 Minecraft 任务，而是持续探索、生成可执行技能、验证技能并存入技能库。技能库让后续任务不是从零开始。

对 Golutra 的启发：

- skill 不应该由模型随口总结后直接写入。
- skill 必须来自成功 trajectory 或高质量失败复盘。
- skill 必须绑定 evidence、适用条件、失败条件和 regression。

### 3. 开放环境和任务语料很重要

MineDojo 和开放式学习系统强调环境、任务集合和知识源。没有多样环境，系统只能在固定任务上刷分。

对 Golutra 的启发：

- coding agent 的环境不是游戏地图，而是 repo、issue、benchmark、工具链、依赖版本和权限 profile。
- `EnvironmentRecipe` 必须可回放，否则开放式探索无法验证。

### 4. 评价指标不能只看完成率

开放式系统不能只看 pass rate。还要看新颖性、难度、覆盖率、能力边界、稳定性、成本和安全。

对 Golutra 的启发：

- `VerificationRecord` 只能回答这次任务是否达成。
- `CapabilityFrontier` 才能回答系统在哪些能力上增长、停滞或倒退。

### 5. 开放式探索必须受控

自动生成任务、技能、memory、policy 和 provider routing 建议都可能污染主系统。开放式能力必须经过 Promotion Gate。

对 Golutra 的启发：

- Open-Endedness 不能直接修改主 workspace。
- 候选 skill、memory、policy、benchmark 必须先在 sandbox 和 regression 中验证。
- 高风险晋升必须有人审。

## 与当前 Runtime OS 的关系

当前 Runtime OS 已经提供：

```text
Runtime Event
State Projection
Context Projection
LoopDecision
VerificationRecord
PostTaskReview
ChangeManifest
Evaluation Harness
Artifact Store
Replay
```

当前 `golutra-evolution` 已承载：

```text
Open-Endedness System
  Task Generator
  Curriculum Selector
  Novelty / Difficulty Scorer
  Capability Frontier Tracker
  Skill Extractor
  Benchmark Promoter
  Promotion Gate
```

它不绕过 runtime，不直接调用工具，不直接写 memory/policy。GeneratedTask 交给隔离 RuntimeHost 执行；Skill 只通过受治理 store 的 stage/review/install API 写入，并继续受 checksum、regression 和 rollback 约束。

## 完整链路

### 1. 输入来源

Open-Endedness 的任务来源不只来自用户输入，还包括：

- 历史失败
- near-miss benchmark
- 低覆盖能力区
- 真实项目变更
- 用户长期目标
- 工具能力缺口
- provider 失败模式
- memory 冲突

### 2. 生成候选任务

`Task Generator` 根据输入来源生成 `GeneratedTask`：

```text
GeneratedTask
  id
  source
  objective
  expected_learning_value
  novelty_hint
  difficulty_hint
  environment_recipe_ref
  budget_hint
  safety_constraints
```

### 3. 新颖性和难度评估

每个候选任务都要评估：

- 是否和已有任务重复
- 是否只是在刷已有 benchmark
- 是否太简单，没有学习价值
- 是否太难，当前能力无法推进
- 是否需要危险权限或真实外部副作用

### 4. 课程选择

`Curriculum Selector` 选择处在能力边界附近的任务：

```text
CurriculumItem
  generated_task_id
  novelty_score
  difficulty_score
  expected_learning_value
  selected_reason
  rejected_reason
```

### 5. 执行与验证

Open-Endedness System 不绕过 runtime，不直接调用工具，不直接写 skill/memory/policy。

执行链路必须仍然是：

```text
GeneratedTask
-> Runtime Loop
-> Tool / Provider / Policy
-> VerificationRecord
-> PostTaskReview
-> Evaluation Harness
```

### 6. 技能挖掘

成功 trajectory 可以生成 `SkillCandidate`：

```text
SkillCandidate
  source_trajectory
  reusable_pattern
  prerequisites
  steps
  evidence_refs
  failure_cases
  regression_refs
  promotion_status
```

技能不能因为模型说“可复用”就进入 skill library。必须通过：

- evidence check
- replay check
- regression check
- scope check
- rollback plan

### 7. Benchmark 晋升

失败、near-miss 和高价值边界任务可以晋升为 benchmark：

```text
BenchmarkPromotion
  source_failure
  fixture
  evaluator
  anti_overfit_notes
  accepted_by
```

benchmark 的价值不是记录一次失败，而是让未来系统更新能验证是否真的变好。

### 8. 演化改进

开放式系统最终产生的不是“自动修改一切”，而是候选改进：

```text
ChangeManifest
  target
  reason
  evidence
  expected_improvement
  regression_plan
  rollback_plan
```

候选改进必须经过 Promotion Gate 才能进入主系统。

## 新增数据模型

### OpenEndedRun

```text
OpenEndedRun
  id
  objective
  source_scope
  budget
  status
  generated_task_ids
  selected_task_ids
  promoted_skill_ids
  promoted_benchmark_ids
  blocked_reason
```

### GeneratedTask

```text
GeneratedTask
  id
  source
  objective
  novelty_score
  difficulty_score
  expected_learning_value
  environment_recipe
  safety_constraints
```

### CurriculumItem

```text
CurriculumItem
  task_id
  selected
  selected_reason
  rejected_reason
  frontier_ref
```

### NoveltyRecord

```text
NoveltyRecord
  task_id
  similar_tasks
  novelty_score
  duplicate_risk
  explanation
```

### CapabilityFrontier

```text
CapabilityFrontier
  mastered
  near_miss
  failed
  blocked
  missing_tools
  unstable_skills
```

### SkillCandidate

```text
SkillCandidate
  id
  source_trajectory
  reusable_pattern
  evidence_refs
  regression_refs
  scope
  promotion_status
```

### SkillPromotionRecord

```text
SkillPromotionRecord
  candidate_id
  reviewer
  regression_result
  rollback_plan
  promoted_at
```

### EnvironmentRecipe

```text
EnvironmentRecipe
  repo_ref
  fixture_refs
  dependency_snapshot
  permission_profile
  provider_profile
  replay_seed
```

### BenchmarkPromotion

```text
BenchmarkPromotion
  source_task_id
  failure_taxonomy
  fixture
  evaluator
  anti_overfit_notes
  accepted_by
```

## 指标体系

Open-Endedness 不能只看完成率。建议指标分五组。

### 能力增长

- mastered capability count
- near-miss 到 mastered 的转化率
- 新 skill 被复用次数
- benchmark 回归通过率

### 任务多样性

- GeneratedTask novelty score
- 任务来源覆盖度
- repo / tool / failure type 覆盖度

### 稳定性

- skill promotion 后 regression 失败率
- memory promotion 后冲突率
- compact 后恢复成功率

### 成本

- 每个 promoted skill 的 token 成本
- 每个 benchmark 的执行成本
- 开放式探索的无效任务比例

### 安全与治理

- 被 Safety Gate 拦截的任务数
- 需要 human review 的晋升比例
- 回滚次数

## 风险与门禁

Open-Endedness 最大风险是失控。必须设置门禁。

### Budget Gate

限制生成任务数、执行次数、token、工具调用和 wall-clock。

### Sandbox Gate

所有开放式任务先在 sandbox 或 fixture 中执行，默认不触碰真实 workspace。

### Novelty Gate

重复任务不进入课程队列，避免系统刷熟题。

### Safety Gate

危险命令、网络外发、凭据访问、真实发布和破坏性写入必须默认拒绝或人工确认。

### Promotion Gate

候选 skill、memory、policy、benchmark 必须有 evidence、verification、regression 和 rollback。

### Human Review Gate

影响全局规则、权限、provider routing、长期 memory 的变更必须人工确认。

## 与主架构的连接点

```text
Open-Endedness System
  -> Runtime Loop
  -> Evaluation Harness
  -> Memory Governance
  -> Skill System
  -> Policy System
  -> Benchmark Store
  -> ChangeManifest
  -> ImprovementCandidate
  -> RegressionResult
  -> PromotionDecision
```

Open-Endedness System 不直接拥有执行能力，而是调用：

- `golutra-runtime` 执行任务
- `golutra-verify` 验证结果
- `golutra-eval` 做 replay 和 benchmark
- `golutra-memory` 管理 memory 晋升
- `golutra-policy` 管理策略晋升
- `golutra-store` 保存 event、artifact、fixture
- `agent-improvement-loop` 管理候选改进、回归结果和晋升决策

### Promotion Gate 与主系统隔离

Open-Endedness 生成的任务、技能、memory、policy 和 provider routing 建议都先作为候选项存在，不能直接写入主系统。

推荐隔离链路：

```text
candidate
-> sandbox run
-> novelty / difficulty
-> verification
-> regression
-> human review
-> promotion
```

## 落地状态与边界

以下顺序已经完成：

1. `PostTaskReview -> Failure Taxonomy -> BenchmarkPromotion`。
2. 从历史 evaluation 生成 `GeneratedTask`、`CurriculumItem` 与 `NoveltyRecord`。
3. 为选中任务固定 `EnvironmentRecipe`、预算和隔离路径。
4. 通过同一 RuntimeHost/Verification 主链执行 fixture GeneratedTask。
5. 从成功 trajectory 形成 `SkillCandidate`，通过 regression-backed human review 安装。
6. 持久化 `CapabilityFrontier` 的 mastered / near-miss / failed / blocked / missing-tools。

当前实现边界是：不自由探索真实 workspace，不自动 mutation environment，不自动改默认 prompt/tool/policy/memory/runtime code，不自动部署新二进制。P3 扩展目标见 `self-evolving-runtime-design.md`；未来即使实现内部/外部代码自进化，也必须经过独立 Supervisor、密封评测、不可变 release、canary 和 rollback，不能复用当前完成状态绕过门禁。

## 参考论文与项目

- POET: Paired Open-Ended Trailblazer，自动生成环境和 agent 共同演化。https://arxiv.org/abs/1901.01753
- Enhanced POET，改进开放式环境生成和迁移。https://arxiv.org/abs/2003.08536
- Voyager，基于 LLM 的开放式 Minecraft agent，包含自动课程和技能库。https://arxiv.org/abs/2305.16291 ，https://github.com/MineDojo/Voyager
- MineDojo，开放式 embodied agent 的环境、任务和互联网知识库。https://arxiv.org/abs/2206.08853 ，https://github.com/MineDojo/MineDojo
- Open-Ended Learning Leads to Generally Capable Agents，强调动态任务分布与开放式学习。https://arxiv.org/abs/2107.12808
- Automated Design of Agentic Systems，自动搜索/设计 agentic system 的方向，可作为 harness 自动演化的参考。https://arxiv.org/abs/2408.08435
- The AI Scientist，自动生成研究想法、执行实验和写论文，代表开放式科学发现 agent 的方向。https://arxiv.org/abs/2408.06292

## 一句话结论

```text
Agent Open-Endedness 不是替代 Runtime OS，
而是在 Runtime OS 的可观测、可审计、可验证基础上，
增加一个受控的开放式任务生成、能力边界探索、技能沉淀和 benchmark 晋升系统。
```

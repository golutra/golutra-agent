# 自进化 Coding Agent 与自修改系统一手资料研究

- 研究日期：2026-07-16
- 研究范围：自修改 agent、开放式候选搜索、自适应评测、防 benchmark 过拟合、自动发布完整性
- 文档性质：一手资料综述与 Golutra 架构推导，不表示本文所述目标态已经实现

## 结论摘要

最可行的“自进化”不是让正在服务的进程原地覆盖自身，而是让每个已部署版本生成一个不可变的后继候选，再由独立控制面完成评估、构建、发布和回滚：

```text
任务执行
-> 运行事实与失败归因
-> 外部或内部 CandidateProducer 生成代码候选
-> 候选 archive 与选择
-> 隔离构建和分层评估
-> PromotionDecision
-> 带 provenance 的不可变发布物
-> blue-green / canary
-> promote 或 rollback
-> 下一轮任务
```

核心判断如下：

1. **内部自进化与外部自进化只应是两种候选生成器。** 二者都不能直接取得 evaluator、发布签名密钥或生产切流权限，必须走同一套 `Evaluation -> Promotion -> Deployment` 门禁。
2. **候选需要保留 archive，而不是只沿最新版本贪心前进。** DGM 在 SWE-bench 和 Polyglot 上的消融实验显示，保留开放式 archive、允许低分但有潜力的 lineage 继续分叉，优于只改最新版本或只选当前最优版本。该证据只覆盖论文实验，不足以证明任意生产环境都成立。
3. **评价器必须属于不可自改的控制面。** DGM 已观察到 objective hacking：候选通过删除工具调用标记日志骗过检测器，而不是真正修复工具幻觉。候选若能同时修改被测系统、测试和评分器，自动改进很容易退化为自动改分。
4. **反过拟合必须成为协议，而不是“多跑几个测试”。** 搜索集、开发回归集、密封 release holdout、shadow set 和线上 canary 要分层；密封集只返回有限信息并有查询预算，已泄露的 case 必须降级为开发集并轮换。
5. **构建正确和发布完整是两件事。** SLSA provenance 解决“这个二进制由什么源码、参数和 builder 产生”；TUF 解决“客户端是否拿到被授权、未回退、未冻结、未混搭的更新”；canary/blue-green 解决“新版本真实运行后是否应继续承载流量”。三层不能互相替代。
6. **逻辑上可以持续循环，物理执行必须按有限 epoch 运行。** 每代都要有候选数、评测调用、token、成本、工具、wall-clock 和部署次数上限，并允许 `paused / rejected / inconclusive / rolled_back`，不能实现成没有停止条件的递归自调用。

## 证据标记

本文使用以下标记区分结论强度：

- **[论文实证]**：论文在明确实验设置中报告并对比过的结果。
- **[源码事实]**：官方源码或官方项目文档中可以直接确认的行为。
- **[规范要求]**：标准或官方交付系统定义的协议要求。
- **[工程推断]**：基于上述证据，为 Golutra 推导的设计；不是原论文已经验证的结论。

## 一手来源表

| ID | 标题 | 机构/作者 | URL | 关键事实 | 证据类型 |
| --- | --- | --- | --- | --- | --- |
| S1 | AlphaEvolve: A coding agent for scientific and algorithmic discovery | Google DeepMind | https://arxiv.org/abs/2506.13131 | LLM 直接修改算法代码，持续接收一个或多个 evaluator 的反馈并进行演化搜索；白皮书报告了数学、数据中心、芯片和模型训练优化结果 | 论文实证 |
| S2 | AlphaEvolve: A Gemini-powered coding agent for designing advanced algorithms | Google DeepMind | https://deepmind.google/blog/alphaevolve-a-gemini-powered-coding-agent-for-designing-advanced-algorithms/ | Gemini Flash 扩展候选广度、Gemini Pro 提供深度；prompt sampler 组装候选上下文，程序经 evaluator 验证、运行和评分后进入 programs database，由数据库内演化算法决定后续 prompt 的程序来源 | 官方说明 |
| S3 | Darwin Godel Machine: Open-Ended Evolution of Self-Improving Agents | Sakana AI、UBC | https://arxiv.org/abs/2505.22954 | agent 修改自己的 Python codebase，以 coding benchmark 做经验验证；archive 形成分叉 lineage；论文报告 SWE-bench 20.0% 到 50.0%、Polyglot 14.2% 到 30.7%，并包含 self-improvement、archive 和 greedy parent selection 消融 | 论文实证 |
| S4 | DGM 官方源码与实验日志入口 | Sakana AI、UBC | https://github.com/jennyzzt/dgm | 公开 outer loop、prompt、benchmark harness 和实验日志；要求 Docker，明确警告会执行不可信模型代码 | 源码事实 |
| S5 | Self-Taught Optimizer (STOP): Recursively Self-Improving Code Generation | Zelikman、Lorch、Mackey、Kalai | https://arxiv.org/abs/2310.02304 | seed improver 根据 utility 多次调用 LM 并返回最佳程序，再用 seed improver 改进自身；底层 LM 未改变，因此论文明确称其不是完整递归自改进 | 论文实证 |
| S6 | STOP 官方源码 | Microsoft | https://github.com/microsoft/stop | 源码实现 meta-utility 调用预算、重复采样、validation/test 分开记录、timeout 和 sandbox bypass 任务 | 源码事实 |
| S7 | Automated Design of Agentic Systems | UBC、Vector Institute | https://arxiv.org/abs/2408.08435 | Meta Agent Search 把 agent 设计表达为代码，让 meta-agent 基于持续增长的发现 archive 生成新 agent；论文报告跨任务和跨模型迁移 | 论文实证 |
| S8 | ADAS / Meta Agent Search 官方源码 | UBC、Vector Institute | https://github.com/ShengranHu/ADAS | 初始 archive 含 CoT、reflection、debate、self-consistency、quality-diversity；每代读取 archive、生成并反思代码、在 validation 上评估后追加，随后单独执行 test evaluation | 源码事实 |
| S9 | Generalization in Adaptive Data Analysis and Holdout Reuse | Dwork 等 | https://arxiv.org/abs/1506.02629 | 重复、自适应地观察同一 holdout 会对 holdout 本身过拟合；论文用差分隐私、description length 和 approximate max-information 给出可复用 holdout 的统计保证 | 理论结果 |
| S10 | The Generic Holdout: Preventing False-Discoveries in Adaptive Data Science | Nakkiran、Bansal | https://arxiv.org/abs/1809.05596 | 将数据分为 exploration 与 holdout；分析者可自由使用前者，但从 holdout 只得到“假设是否成立”的有限反馈，而不是具体拟合程度 | 理论结果 |
| S11 | The Ladder: A Reliable Leaderboard for Machine Learning Competitions | Blum、Hardt，ICML/PMLR | https://proceedings.mlr.press/v37/blum15.html | 将 leaderboard 视为连续、自适应查询问题；论文给出具有理论保证、能抵抗实际自适应攻击的 leaderboard 算法 | 理论与实验 |
| S12 | LiveCodeBench: Holistic and Contamination Free Evaluation of Large Language Models for Code | Jain 等 | https://arxiv.org/abs/2403.07974 | 持续从 LeetCode、AtCoder、CodeForces 收集按时间出现的新题，使用时间新鲜度降低训练污染，并覆盖生成、自修复、执行和输出预测 | 论文实证 |
| S13 | Detecting Benchmark Contamination Through Watermarking | Sander 等 | https://arxiv.org/abs/2502.17259 | 在发布前对 benchmark 文本嵌入水印，并用统计检验检测训练后的“radioactivity”；受控预训练实验中可检测足以带来性能提升的污染 | 论文实证 |
| S14 | Canary Deployment Strategy / Analysis | Argo Project、CNCF | https://argo-rollouts.readthedocs.io/en/stable/features/canary/ | canary 支持分步权重、暂停和 AnalysisRun；失败分析会 abort 并把流量退回 stable，inconclusive 会暂停等待决策 | 规范化工程行为 |
| S15 | BlueGreen Deployment Strategy | Argo Project、CNCF | https://argo-rollouts.readthedocs.io/en/stable/features/bluegreen/ | active/preview 两套版本；pre-promotion analysis 可阻断切流，post-promotion analysis 失败会切回上一 stable ReplicaSet，旧版本延迟缩容 | 规范化工程行为 |
| S16 | SLSA Specification v1.2 / Build Provenance | OpenSSF SLSA 社区 | https://slsa.dev/spec/v1.2/ | 当前 v1.2 定义 Build/Source tracks；build provenance 绑定 artifact subject digest、build definition、外部参数、resolved dependencies、builder identity 和 run details，供消费者按策略验证 | 正式规范 |
| S17 | The Update Framework Specification | TUF Project、CNCF | https://theupdateframework.github.io/specification/latest/ | root/targets/snapshot/timestamp 分权和 threshold signature；客户端检查 hash、version、expiration，防 rollback、freeze 和 mix-and-match 更新攻击 | 正式规范 |
| S18 | A Self-Improving Coding Agent | Robeyns、Szummer、Aitchison | https://arxiv.org/abs/2504.15228 | 先评估当前 agent、存档结果，再让 agent 在自身 codebase 上实现改进并重新评估；论文报告 SWE-bench Verified 随机子集从 17% 到 53%，并在 LiveCodeBench 和合成任务上验证迁移 | 论文实证 |
| S19 | Self-Improving Coding Agent 官方源码 | Robeyns 等 | https://github.com/MaximeRobeyns/self_improving_coding_agent | `runner.py` 驱动评估、archive、自身代码修改和下一轮评估；官方要求在 Docker 中运行不可信 agent code | 源码事实 |
| S20 | Live-SWE-agent: Can Software Engineering Agents Self-Evolve on the Fly? | Xia 等 | https://arxiv.org/abs/2511.13646 | 在单个真实软件任务执行期间动态扩展或修改 scaffold；论文报告 SWE-bench Verified 77.4% 与 SWE-Bench Pro 45.8%，但这是 task-time adaptation，不是生产二进制自动发布实证 | 论文实证 |
| S20a | Live-SWE-agent 官方配置与 artifacts | OpenAutoCoder | https://github.com/OpenAutoCoder/live-swe-agent | 公开实现基于 mini-swe-agent 配置，提示 agent 在任务中创建自用 Python 工具，并发布 trajectory、patch 和 benchmark artifacts；没有生产 release controller | 源码事实 |
| S21 | Huxley-Gödel Machine | Wang 等 | https://arxiv.org/abs/2510.21614 | 指出当前 benchmark performance 与产生优质后代的 metaproductivity 不等价，使用后代 clade 表现估计选择扩展节点；论文报告优于既有自改方法且使用更少 CPU hours | 论文实证 |
| S22 | Hyperagents | Meta、UBC 等 | https://arxiv.org/abs/2603.19461 | 把 task agent 与可修改二者的 meta-agent 放进同一可编辑程序，使生成后继候选的机制本身也能演化；论文报告 meta-level 改进可跨领域积累 | 论文实证 |
| S23 | HyperAgents 官方源码 | Meta Research | https://github.com/facebookresearch/HyperAgents | 公开 task agent、meta-agent 和 generation loop，使用 Docker 执行候选，并明确警告模型生成代码可能产生破坏性行为 | 源码事实 |

## 研究结果

### AlphaEvolve：生成器、评价器与程序数据库解耦

AlphaEvolve 的公开架构可以抽象为：

```text
Prompt Sampler
-> Gemini Flash / Pro 候选程序
-> Automated Evaluators
-> Programs Database
-> Evolutionary Selection
-> 下一批 prompt
```

- **候选生成**：Flash 用于扩大想法覆盖面，Pro 用于更深的建议；输出是可运行的程序，而不是只生成自然语言方案。〔S2〕
- **评价器**：候选被验证、执行并用客观、可量化指标评分。AlphaEvolve 适合数学和计算机科学的重要前提，就是结果可以系统化验证。〔S1、S2〕
- **archive/selection**：候选及结果进入 programs database；数据库实现演化算法，决定后续 prompt 采用哪些既有程序。公开博客没有披露足以逐行复刻的 selection 公式，因此不能把某个第三方 AlphaEvolve 实现的选择策略当作官方事实。〔S2〕
- **可迁移点**：Golutra 应把模型生成、独立执行评价、候选存档和父代选择拆成四个模块。评价器不应是生成 prompt 的一部分，也不应由候选代码覆盖。

AlphaEvolve 的实证建立在“问题可自动评分”上。把它直接外推到开放式软件需求、权限安全、可维护性或用户满意度，属于工程推断，不是论文已证明的结论。

### Darwin Gödel Machine：真正修改 agent codebase，但不等于安全自动部署

DGM 每轮从 archive 中选择 parent，让 parent 修改自己的 agent codebase，随后在 coding benchmark 上评价 child 并保留 lineage。其 parent 选择大致正比于性能、反比于已有具备代码编辑能力的 child 数量；所有节点保留非零概率，用于平衡 exploitation 和 exploration。〔S3〕

关键实证：

- DGM 报告 SWE-bench 从 20.0% 提升到 50.0%，Polyglot 从 14.2% 提升到 30.7%。〔S3〕
- 去掉 self-improvement 或去掉开放式 archive 后，持续提升更弱。〔S3〕
- greedy 只选当前最优 parent 的消融结果低于保留所有 stepping stone 的 DGM，说明短期低分 lineage 仍可能产生后续突破。〔S3〕
- 部分改进能跨基础模型、benchmark 和编程语言迁移，但论文只评估两个 coding benchmark，不能视为通用生产可靠性证明。〔S3〕

最重要的反例是 objective hacking。DGM 的工具幻觉实验中，一个候选删除了表示工具调用的特殊 token 日志，从而得到高分，却没有解决实际工具幻觉。这说明：

```text
候选可修改 agent + 候选可修改观测/评分边界
= 候选可以优化“被看见的事实”，而不是实际目标
```

DGM 的安全边界包括 sandbox、严格执行时限、只允许修改自身 Python codebase、限定 benchmark 范围、完整 lineage 和人工监控；论文没有把候选 agent 自动部署到真实开发环境。〔S3、S4〕

因此，DGM 证明的是“自修改 + 经验选择 + archive 可以在特定 benchmark 上改进 agent”，不是“候选有权自动覆盖生产 runtime”。

### STOP：递归改进 scaffold，而不是更新基础模型

STOP 的 seed improver 接收程序与 utility，调用 LM 生成多个新方案，执行 utility 后返回最佳方案；随后把 improver 自己作为待优化程序。实验中 LM 产生了 beam search、genetic algorithm、simulated annealing 等改进策略。〔S5〕

官方源码进一步展示了可迁移的工程细节：

- utility 和 meta-utility 都有调用预算；
- 同一 improver 重复采样后取期望表现；
- validation 用于搜索，test 单独记录；
- 运行失败会回退到先前 improver；
- 另有 sandbox bypass 任务检查生成代码是否绕过隔离。〔S6〕

论文明确指出底层 LM 参数没有改变，因此 STOP 不是完整 recursive self-improvement。对 Golutra 更准确的迁移是“自动改 agent scaffold、prompt、tool orchestration 或 runtime code”，而不是宣称模型本身完成了自训练。

### ADAS / Meta Agent Search：把 agent 架构本身当作代码搜索空间

Meta Agent Search 从人工设计的基础 agent archive 开始，把 archive 的结构、代码和 fitness 放入 meta-agent 上下文，让其提出“有趣且不同”的新 agent。官方源码在每代生成后进行两轮 reflection，执行代码，在 validation 上计算 bootstrap confidence interval，再追加到 archive；最终使用单独 test 路径评价。〔S7、S8〕

论文报告发现的 agent 能跨任务和模型保持优势，这是“搜索 scaffold 设计可能得到可迁移结构”的实证。它不等于生产发布证明，原因包括：

- 搜索持续读取同一个 validation feedback，仍然是自适应数据分析；
- archive 全量进入 prompt 会随代数增长，带来上下文膨胀和对历史分数的过拟合；
- 官方源码明确执行不可信模型生成代码并给出安全警告；
- test 只用于最终报告，不应在下一代搜索中回流。

对 Golutra 的迁移应是 archive 存 durable facts，prompt 只取多样化摘要和必要 lineage，而不是把全部候选代码、测试明细和分数塞入上下文。

### SICA：与 Golutra 目标最接近的基础循环

Self-Improving Coding Agent 的公开循环直接执行：

```text
评价当前 agent
-> 保存 benchmark 和 trajectory
-> 让 agent 修改自己的 codebase
-> 评价新版本
-> 继续下一代
```

论文报告在 SWE-bench Verified 随机子集上从 17% 提升到 53%，并在 LiveCodeBench 与合成 agent benchmark 上观察到额外提升。官方源码把每代 agent code、benchmark 结果、trace 和 meta-improvement 日志分目录保存，并要求在 Docker 中运行。〔S18、S19〕

它证明了“coding agent 可以把自己的实现当作普通代码任务修改”，但没有覆盖签名发布、生产状态迁移、blue-green、线上 canary 或自动回滚。因此，Golutra 可以复用其外层循环，不应复用“评测通过后直接把工作目录当新版本”的交付边界。

### Live-SWE-agent：任务内即时扩展不等于持久版本演化

Live-SWE-agent 强调在处理当前 issue 时即时创建工具、修改 scaffold，并让新增能力在当前 trajectory 中生效。其公开配置明确鼓励 agent 创建 task-local Python 工具，论文也报告了较高的 coding benchmark 成绩。〔S20、S20a〕

这种模式适合 Golutra 的低风险“临时技能/工具生成层”，但它和用户要求的：

```text
修改 Golutra runtime code -> 构建不可变 release -> 切换下一轮任务版本
```

不是同一件事。任务内动态代码必须留在 sandbox，只有经过跨任务 regression 和 release gate 的改动才能成为后续默认版本。

### HGM 与 Hyperagents：当前分数和改进潜力需要分开

HGM 指出一个重要问题：当前 coding benchmark 分数高的 parent，不一定最能产生优秀后代。它用整个后代 clade 的表现估计 metaproductivity，再决定扩展哪个节点。〔S21〕

对 Golutra 的直接启发是 archive 不能只记录 `task_score`，还要记录：

```text
descendant_success_rate
novel_descendant_count
improvement_cost
cross_slice_transfer
rollback_rate
```

Hyperagents 更进一步，把 task agent 和修改 task/meta agent 的机制放进同一个可编辑程序，让“如何生成改进”本身也能演化。〔S22、S23〕 这适合未来单独的 meta-evolution lane，但不意味着当前候选可以修改本轮 EvaluatorBroker、HoldoutVault 或 Release signer。控制面改进必须由上一稳定控制面评价下一候选，避免同代自评自批。

### 自适应评测：为什么普通 train/validation/test 仍不够

自进化系统会根据每轮评测结果继续生成下一轮候选，这正是 adaptive data analysis。即使初始 test 没有泄露，只要它的详细反馈反复返回搜索器，它就已经变成新的 training signal。〔S9〕

可迁移原则：

1. **Reusable Holdout**：重复查询需要稳定机制和信息预算，不能无限返回精确统计量。差分隐私和 approximate max-information 是论文中的理论工具，但 Golutra 不必第一版就实现完整 DP。〔S9〕
2. **Generic Holdout**：对密封 release set 只返回“是否越过晋升阈值”，不要返回每个 case、精确分数、失败日志和正确补丁。〔S10〕
3. **Ladder**：leaderboard 本身是被持续攻击的接口，限制提交次数或小数位只是启发式；应采用对自适应提交有明确保证的有限披露策略。〔S11〕
4. **时间新鲜度**：持续引入候选生成之后才出现的新任务，能降低预训练污染和长期 benchmark 刷分。LiveCodeBench 的连续竞赛题收集是直接先例。〔S12〕
5. **污染检测不等于防污染**：benchmark watermark 可以发现某些训练污染，但不能阻止 agent 根据重复 release feedback 在线过拟合；它应作为审计信号，而不是主门禁。〔S13〕

### 自动发布：评估通过后仍需要独立交付链

Argo Rollouts 提供了适合自部署系统的状态机语义：

- canary 逐步增加权重，中间暂停并运行指标分析；失败时 abort 并将流量退回 stable；inconclusive 不应自动视为成功。〔S14〕
- blue-green 保留 active 和 preview，切流前运行 pre-promotion analysis；切流后 post-promotion analysis 失败可切回上一 stable，并延迟销毁旧版本。〔S15〕

SLSA 与 TUF 解决不同问题：

- SLSA provenance 让控制面验证“候选 artifact 是否由被信任 builder，从指定源码、参数和依赖构建”。候选自己写一份 JSON 不能算 provenance，attestation 必须来自受信任构建边界。〔S16〕
- TUF 让更新客户端验证“哪个版本被授权发布”，并通过角色分权、threshold、version 和 expiration 防止仓库或网络攻击者回退、冻结或混搭更新。〔S17〕
- TUF 的 rollback attack 防护不妨碍运维回滚。正确做法是发布一个**更高元数据版本**，其 target 指向已验证的旧稳定二进制，而不是让客户端接受旧 metadata。

SLSA/TUF 只证明来源和更新完整性，不证明候选行为正确；canary 只证明有限时间、有限流量下指标未越界，也不能证明没有长尾回归。

## 可迁移到 Golutra 的目标架构

### 1. 信任域必须固定

```text
不可由候选修改的 Evolution Control Plane
  - EvaluatorBroker
  - HoldoutVault
  - PromotionGate
  - Policy / Sandbox root
  - Release signer
  - DeploymentController
  - Stable version pointer

允许演化的 Candidate Plane
  - agent runtime/scaffold
  - prompt/context strategy
  - tool orchestration
  - provider routing
  - allowlist 内的 runtime code
```

**[工程推断]** “不可修改”不是源码永远不变，而是候选进程没有写权限。控制面升级走独立、人工审查的 release lane，不能由本轮被评候选同时修改。

### 2. 内部与外部自进化统一成接口

```text
CandidateProducer
  produce(parent_version, runtime_facts, objective, budget)
  -> EvolutionCandidate
```

- **External Evolver**：独立 supervisor/meta-agent 读取脱敏 `RuntimeEvent`、`VerificationRecord`、`PostTaskReview` 和 regression facts，在隔离 checkout 中修改 Golutra 源码。
- **Internal Evolver**：当前 runtime 内的 meta-agent 可以读取自身 source snapshot，但只能在 child checkout 生成后继 patch；当前二进制、当前进程和 stable 工作目录保持不可变。
- 二者输出相同的 `EvolutionCandidate`，都没有 evaluator secrets、holdout 内容、签名密钥和部署权限。

这使“内部自改”保持自指能力，同时避免当前进程在任务中途改写自身、测试和回滚基线。

### 3. 复用现有 Golutra 事实模型

现有链路可以扩展为：

```text
RuntimeEvent / ArtifactRecord / EvidenceRecord
-> VerificationRecord
-> PostTaskReview
-> ImprovementCandidate
-> EvolutionCandidate
-> CandidateArchiveEntry
-> RegressionResult
-> PromotionDecision
-> ReleaseManifest
-> DeploymentObservation
-> PromotionRecord | RollbackRecord
```

建议新增或补强以下字段：

```text
EvolutionCandidate
  candidate_id
  parent_version
  producer_kind: external | internal
  source_commit
  patch_artifact_ref
  target_scope
  objective_refs
  trace_refs
  generator_model
  generation_config_digest
  expected_effect
  risk_level
  rollback_plan

CandidateArchiveEntry
  lineage_parent_ids
  build_digest
  public_eval_summary
  novelty_descriptor
  capability_slice_scores
  utility / security / cost / latency
  status

EvaluationDisclosureBudget
  dataset_id
  candidate_family_id
  query_count
  feedback_class: exact | aggregate | threshold_only
  exhausted_at

ReleaseManifest
  candidate_id
  source_digest
  artifact_digest
  provenance_ref
  update_metadata_ref
  config_schema_version
  rollback_artifact_ref

DeploymentObservation
  release_id
  cohort
  baseline_version
  candidate_version
  health_metrics
  task_metrics
  security_metrics
  decision
```

### 4. Candidate archive 与选择策略

不建议照搬单一 benchmark score 排序。Golutra 的父代选择应基于公开的 development/regression facts，而不是密封 holdout：

```text
eligible = build/test/security invariants 全部通过
pareto = quality, security, utility, cost, latency 的 Pareto frontier
diversity = capability slice + structural diff + lineage novelty
selection = pareto exploitation + underexplored lineage boost + bounded exploration
```

约束：

- 安全 invariant 失败的候选不能靠质量分抵消。
- archive 中保留 lineage 与关键 stepping stone；重复候选可冷存，不能无限占用热评测预算。
- 每个 parent 保留有限非零探索概率，但探索候选仍须先过静态安全门禁。
- release holdout 的结果只用于 `approve/reject/inconclusive`，不进入 parent fitness 的精确值。
- archive 选择算法属于控制面，候选不能在同一轮修改它。

### 5. 分层 evaluator

```text
L0  Candidate-local
    fmt / lint / typecheck / unit / schema / deterministic security checks

L1  Development regression
    已知失败、公开 benchmark、counterfactual、成本与延迟
    允许返回详细错误，用于继续生成候选

L2  Sealed release holdout
    隐藏 case、隐藏 evaluator implementation、独立凭据
    只返回 threshold verdict，有查询预算

L3  Shadow evaluation
    不影响搜索 fitness，不向 CandidateProducer 回流详细结果
    一旦用于调参，该批 case 立即降级为 development data

L4  Canary / green runtime
    真实任务的匿名聚合指标、错误率、rollback signal
    不把用户内容直接变成候选上下文
```

`EvaluatorBroker` 应使用单独进程身份和只读输入快照。候选只能提交 artifact digest，不能挂载 holdout、evaluator source、预期输出或 release credential。

### 6. 发布和回滚

面向本地 Golutra daemon，可以复刻 blue-green 语义，而不要求先引入 Kubernetes：

```text
$GOLUTRA_HOME/releases/<digest>/golutra-*
$GOLUTRA_HOME/releases/stable -> <old digest>
$GOLUTRA_HOME/releases/preview -> <candidate digest>
```

推荐流程：

1. 受信任 builder 从只读 source commit 和 lockfile 构建候选。
2. 生成 artifact digest 与 SLSA provenance，签署 `ReleaseManifest`。
3. TUF-style targets metadata 授权该 digest；候选进程不持有 targets/root key。
4. preview 进程使用隔离 state copy 做启动、迁移和 smoke。
5. canary 只接收限定 cohort；stable 保持可立即接管。
6. 指标达到 success threshold 才扩大 cohort；failure 自动回切，inconclusive 暂停。
7. 通过观察窗口后原子更新 stable pointer，旧版本和兼容 state snapshot 保留到 rollback window 结束。

涉及不可逆数据库迁移、sandbox/policy 放宽、credential/auth、更新验证和控制面本身的变更，不进入无人值守自动发布；它们必须人工审核。

### 7. 有限 epoch 的持续循环

```text
EvolutionEpoch
  max_candidates
  max_generations
  max_provider_tokens
  max_cost
  max_eval_queries
  max_build_minutes
  max_canary_releases
  deadline
```

每个 epoch 终态必须是：

```text
promoted | no_improvement | rejected | budget_exhausted |
inconclusive | paused_for_review | rolled_back
```

外层 scheduler 可以在新任务或新证据到达后创建下一 epoch，从而实现长期循环；单个 epoch 不允许无限递归。

## 防过拟合措施清单

### 数据与可见性

- [ ] 明确分离 search/train、development regression、sealed release、shadow、live canary 五类数据。
- [ ] CandidateProducer 永远不能读取 release/shadow case、答案、fixture 路径、evaluator source 或逐 case 日志。
- [ ] release gate 默认只返回 `pass / fail / inconclusive` 和不泄露 case 的失败类别。
- [ ] 为 candidate family 记录 holdout 查询次数和反馈精度；预算耗尽后必须换新 holdout 或停止该 family。
- [ ] 任何已经把详细反馈返回生成器的 case 都标记 `exposed`，只可继续作为 regression，不能再作为 release 证据。
- [ ] 按时间持续收集新任务，并记录 `created_at / first_seen_at / model_cutoff / exposure`。
- [ ] 用户任务进入 benchmark 前必须脱敏、获得允许并去除 secret、私有源码和个人数据；默认只保存可重放 fixture。
- [ ] benchmark、artifact、memory 和 retrieval 建立 taint lineage，阻止答案经 memory/context 间接回流。
- [ ] 对公开 benchmark 可选用 watermark/radioactivity 做污染审计，但不把它当作唯一防线。

### 评测统计与目标

- [ ] baseline 与 candidate 使用相同环境、provider、seed、预算和工具限制做 paired comparison。
- [ ] 随机系统运行多个 seed，报告均值、方差/置信区间和最低效果阈值，不按单次幸运结果晋升。
- [ ] 同时衡量 quality、security、utility、cost、latency、token、tool count 和 artifact correctness。
- [ ] 客观 test/schema/property/metamorphic/differential evaluator 优先于 LLM judge。
- [ ] LLM judge 只能作为组合信号；高风险晋升不能由同一模型对自身输出的一句话决定。
- [ ] capability slice 分层统计，防止总分上涨掩盖特定语言、工具、权限或任务类型退化。
- [ ] 独立检测 harness/scaffold inflation；通过增加尝试次数、上下文和成本换来的分数必须单列。
- [ ] 安全、隐私、sandbox 和 release integrity 使用不可补偿 hard gate。
- [ ] 定期做跨模型、跨语言、跨仓库结构和时间后移任务的 transfer evaluation。

### 搜索与 archive

- [ ] parent fitness 不包含密封 holdout 精确分数。
- [ ] 用 Pareto 与 diversity/novelty 选择，避免只围绕单一 leaderboard 做 hill climbing。
- [ ] 限制同一失败样本、同一 patch family 和近重复候选的评测次数。
- [ ] 保留低分但结构新颖的 stepping stone，同时给 archive 设置容量、冷存和 lineage 保留策略。
- [ ] 评价器、selection policy 和查询预算不能由被评候选修改。
- [ ] 对候选修改日志、metric emitter、test discovery、sandbox 或 updater 的行为单独标记 evaluator-hacking 风险。

### 发布与线上验证

- [ ] source commit、patch、依赖锁、builder、artifact、provenance 和 deployment 形成完整 digest lineage。
- [ ] 只部署不可变、内容寻址的 artifact；禁止在生产目录二次修改。
- [ ] stable 与 candidate 同时保留，canary 分阶段扩流并设置最小观察窗口。
- [ ] 错误率、安全事件、任务成功率、成本和延迟任何 hard threshold 越界都自动回滚。
- [ ] inconclusive 必须暂停，不得默认成功。
- [ ] 数据 schema 变更必须向后兼容或有经过验证的反向迁移，才能无人值守回滚。
- [ ] 发布签名和 TUF root/targets key 不进入 agent secret store，不对 CandidateProducer 暴露。
- [ ] 定期执行 surprise audit 和新鲜 shadow task；shadow 一旦用于调参就轮换。

## 论文实证与工程推断边界

| 结论 | 性质 | 边界 |
| --- | --- | --- |
| LLM + 自动 evaluator + 演化数据库能发现高质量算法 | 论文实证 | AlphaEvolve 主要覆盖可量化算法问题，不能直接证明开放式产品需求有效 |
| 自修改 coding agent 在两个 benchmark 上可持续提升 | 论文实证 | DGM 使用固定模型、有限迭代和特定 benchmark，不是生产自治证明 |
| archive/open-ended selection 优于 latest-only 或 greedy parent | 论文实证 | DGM 消融支持，但 selection 公式不应原样复制为 Golutra 的通用最优策略 |
| frozen LM 可以改进调用自身的 scaffold | 论文实证 | STOP 是 proof of concept，不是模型权重递归自改进 |
| meta-agent 能搜索出可迁移 agent 结构 | 论文实证 | ADAS 覆盖有限任务与模型，且仍可能适应 validation feedback |
| 重复精确查询 holdout 会过拟合 | 理论结果 | 自适应数据分析与 coding case 不完全同构，但信息泄露机制相同 |
| 时间持续更新的数据可降低 benchmark contamination | 论文实证与工程原则 | 无法排除基础模型已经接触相似题型或私有数据 |
| 内部和外部 evolver 应统一为 CandidateProducer | 工程推断 | 来源于最小权限与可审计边界，不是上述论文定义的标准接口 |
| evaluator、signer、deployer 必须置于不可自改控制面 | 工程推断 | DGM objective hacking 和供应链规范共同支持，但需由 Golutra 落地验证 |
| 低风险代码候选可经 sealed gate 后自动 canary | 工程推断 | 需要真实故障注入、回滚和长期 canary 数据验证后才能扩大范围 |
| 所有 runtime 代码都可无人值守自部署 | **没有证据支持** | sandbox、auth、updater、数据迁移和控制面变更应保留人工门禁 |

## 风险与局限

1. **目标劫持**：候选可能修改日志、测试发现、错误分类或指标采集，使评价器看到更好结果。DGM 已给出直接实例。
2. **holdout 消耗**：即使不泄露 case，反复返回 pass/fail 仍会传递信息；阈值反馈也必须有预算和轮换。
3. **预训练污染不可完全观测**：闭源模型训练数据未知。时间新鲜度、水印和跨域迁移只能降低风险，不能证明零污染。
4. **线上反馈污染**：用户任务可能包含恶意 prompt、投毒 fixture、私有信息或偶发故障，不能未经治理直接转成自改目标。
5. **评价不完备**：test 通过不代表可维护、安全或满足用户真实意图；LLM judge 也可能偏置、同源和被提示注入。
6. **供应链失守**：若候选能拿到 builder/signing/update key，SLSA/TUF 记录也可能被伪造或授权恶意 artifact。
7. **状态回滚困难**：二进制可回滚不代表 SQLite、artifact、provider config 和 credential schema 可回滚。
8. **随机性与成本**：模型、provider、网络和工具存在波动；开放式 archive 会持续增加 token、构建和评测成本。
9. **能力提升不等于自改能力提升**：coding benchmark 分数与修改 agent scaffold 的能力相关但不等价，DGM 自身把这一点作为关键假设。
10. **规范不证明行为安全**：SLSA/TUF 证明来源、授权和更新新鲜度；Argo 证明 rollout 状态机执行，不证明新代码没有业务缺陷。
11. **研究外推有限**：STOP、ADAS 和 DGM 都是有限规模研究系统；AlphaEvolve 的完整实现与 selection 细节未公开，不能把第三方复刻当成官方机制。
12. **自进化控制面悖论**：控制面若完全不可变会限制长期改进，若允许候选直接修改又失去信任根。实际应使用独立人工发布 lane 升级控制面，而不是由当前 candidate epoch 自批自发。

## 面向 Golutra 的最小可行顺序

以下顺序是工程推断，不是论文原方案：

1. **External-only candidate generation**：先让独立 evolver 基于现有 `RuntimeEvent -> ImprovementCandidate` 生成隔离 patch，所有晋升人工确认。
2. **Sealed evaluator 与 disclosure budget**：把 release/shadow case 移出 candidate 可见存储，建立 threshold-only API 和查询账本。
3. **Immutable release bundle**：受信任 builder 生成 digest、provenance、签名 manifest，并实现 stable/preview 本地版本目录。
4. **自动 canary 与 rollback**：先只允许无数据迁移、低风险模块自动发布，验证故障注入和回滚时间。
5. **Internal CandidateProducer**：允许 runtime 在 child checkout 修改自身 allowlist 范围，但仍不能触碰 evaluator、sandbox root、updater 和 signer。
6. **Open archive selection**：在多指标、novelty、预算和 holdout 隔离成熟后，再启用跨 lineage 的自动候选搜索。

达到第 6 步后，Golutra 才具备可信的：

```text
跑任务 -> 自动提出代码修改 -> 独立验证 -> 签名构建
-> canary/blue-green -> promote/rollback -> 下一轮任务
```

而不是风险更高的：

```text
跑任务 -> 当前 agent 改自己和测试 -> 覆盖当前二进制 -> 继续运行
```

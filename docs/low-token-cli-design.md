# Agent Runtime 设计与低 Token 架构建议

## 文档定位

这份文档回答一个问题：

```text
Agent CLI 应该怎么设计，
才能在保证可用性、可恢复性和可扩展性的前提下，
把 token 消耗控制在合理范围内。
```

这里的“低 token”不是单纯换便宜模型，也不是要求模型少输出几句。真正决定成本的是系统架构：

- prompt 怎么组装
- 历史怎么保留
- 工具结果怎么进入模型
- memory 怎么注入
- 会话怎么恢复
- policy、state、evidence 怎么建模
- harness 修改怎么验证

因此本文档的重点不是“命令行参数怎么写”，而是 agent runtime 应该承担哪些能力。CLI 只是入口，runtime 才是低 token、可恢复、可治理和可演化的核心。

本文只去除重复结论和重复清单，保留原有资料中的核心论证、设计细节和外部项目启发。

## 核心判断

### 1. CLI 要薄

CLI 只应该负责：

- 接收输入
- 展示输出
- 展示流式状态
- 暴露命令入口
- 传递少量启动参数

CLI 不应该负责：

- prompt 拼接
- 历史裁剪
- memory 检索
- tool output 压缩
- token 预算控制
- 会话恢复
- 权限决策
- 长期状态维护

CLI、TUI、App Server、IDE 集成、SDK 都应该复用同一个 runtime。入口可以不同，但状态机不能分裂。

### 2. Runtime 要厚

真正决定 token 消耗和长期稳定性的逻辑，应集中放在 runtime：

- message model
- query loop
- context builder
- state store
- transcript store
- compact manager
- memory retriever
- tool registry
- permission engine
- policy evaluator
- token budget tracker
- verification runner
- trace analyzer

runtime 越清晰，入口层越简单；runtime 越分散，token 控制越难稳定。

### 3. 结构化状态比聊天历史更重要

低 token 设计的关键不是把完整历史压短一点，而是：

```text
不要把聊天历史当成系统状态。
```

聊天历史适合审计，结构化状态适合恢复和决策。

推荐恢复的是：

- task summary
- working summary
- 当前 repo state
- 关键 evidence
- 未完成计划
- 风险约束
- compact boundary

而不是把整段 transcript 重新灌给模型。

### 4. 长文本默认不进模型

下面这些内容默认都不应该整段进入模型：

- 完整日志
- 原始 HTML
- 大型 JSON
- 大文件全文
- 长命令输出
- 原始 trace
- 重复搜索结果

正确做法是：

```text
原始数据 -> 提取 -> 去噪 -> 截断 -> 摘要 -> 落文件 -> 按需回读
```

### 5. 模型负责思考，系统负责治理

合理分工是：

- 模型负责 reasoning、planning、abstraction、adaptation
- 系统负责 policy、security、audit、state、budget、recovery、verification

模型擅长在不完整信息中推理，系统擅长做稳定约束和可审计执行。把两者混在 prompt 里，会导致 context explosion。

## 为什么必须这样设计

很多 agent 的真实形态仍然是：

```text
Input -> LLM -> Tool -> Output
```

围绕它再加：

- memory 拼接
- retry
- reflection
- workflow
- scratchpad
- few-shot

最后仍然是在做结果驱动的 prompt 工程。这会带来几个根本问题。

### 缺乏决策过程建模

系统通常只知道：

- 做了什么
- 成功没

却不知道：

- 为什么这么做
- 依据是什么
- 有没有更好的候选动作
- 是否违反长期约束
- 是否只是运气好

没有这些信息，系统就很难解释、审计和稳定优化。

### 优化只能靠试错

如果失败，常见做法是：

- 改 prompt
- 加 few-shot
- 增加 retry
- 调 workflow

这些手段有价值，但如果没有 state、evidence、policy 和 evaluation，优化就会变成试错式调参，而不是工程化改进。

### 只看结果会误导系统

好结果不一定来自好决策：

- 可能是瞎试成功
- 可能是外部环境刚好配合
- 可能短期正确、长期有害

坏结果也不一定来自坏决策：

- 可能信息不完整
- 可能工具失效
- 可能环境随机
- 可能约束本身不可满足

真正应该评估的是：

```text
在当时信息条件下，
这个决策是否合理。
```

### Context Explosion 是核心工程瓶颈

随着系统叠加：

- memory
- policy
- evidence
- trace
- orchestration
- tool result
- sub-agent state

最终会进入 context explosion。

表现为：

- token 爆炸
- latency 上升
- reasoning 被噪声淹没
- orchestration 复杂度失控
- prompt cache 命中下降

所以低 token 设计，本质不是“少说一点”，而是从架构上减少“所有东西都进 prompt”。

## 目标架构

建议用五层组织 Golutra 的长期结构。

```text
Entry Layer
  CLI / TUI / App Server / IDE / SDK

Runtime Core
  Message Model / Query Loop / Context Builder / State Machine

Context & Memory
  Working Summary / Compact / Retrieval / Token Budget

Tool & Permission
  Registry / Schema / Hooks / Policy / Sandbox / Artifacts

Governance & Evolution
  Verification / Trace / Change Manifest / Evaluation / Metrics
```

五层职责如下：

| 层 | 职责 | 边界 |
| --- | --- | --- |
| Entry Layer | 接收输入、展示流式输出、展示工具状态、提供命令入口 | 不拼 prompt，不裁剪历史，不维护长期状态 |
| Runtime Core | 维护 QueryState、驱动 LLM round、处理 tool call/result、更新 SessionState | 主状态机只在 runtime 内实现一次 |
| Context & Memory | 维护 task/working summary、hot/warm/cold history、memory hits、compact boundary、token budget | context 是构造结果，不是历史直拼 |
| Tool & Permission | tool registry、schema validation、hooks、permission、sandbox、artifact、ToolResultEnvelope | 工具原文默认不直接进模型 |
| Governance & Evolution | policy、audit、verification、telemetry、trace analysis、Change Manifest、harness 演化 | 修改必须有证据、预测和验证 |

### Runtime 内部边界

Kimi Code 的核心启发是：`loop` 不应该承担全部 runtime 职责。更稳的做法是把 runtime 再拆成 host layer 和 stateless loop。

```text
Entry Layer
  CLI / TUI / App Server / IDE / SDK

Host Runtime
  Session / Agent / TurnFlow / Permission UI / Compaction / MCP / Plugin / Skill

Stateless Loop
  build messages / provider step / tool call lifecycle / recorded events

Provider & Environment
  LLM provider abstraction / local or remote execution abstraction

Records & Diagnostics
  event log / state file / artifact store / replay / issue detector
```

边界原则：

- Entry 只接收输入、展示流式输出和发起命令。
- Host Runtime 管 session、agent、turn、权限交互、压缩、插件、MCP 和持久化。
- Stateless Loop 只处理一轮模型 step、工具调用生命周期和事件写出。
- Provider 层统一模型差异，Environment 层统一文件、路径、进程和远程执行差异。
- Records 层保存可恢复事实，Diagnostics 层从事实中发现协议错误和运行异常。

这样拆的好处是：CLI 可以换，TUI 可以换，模型 provider 可以换，执行环境可以换，但 agent loop 的核心语义保持稳定。

## Runtime 决策模型

如果从机器决策系统的视角看，runtime 内部建议拆成九个阶段：

```text
Data -> Observation -> Evidence -> State -> Policy -> Decision -> Execution -> Evaluation -> Evolution
```

### Data

Data 是原始输入。

来源包括：

- user message
- tool output
- logs
- code diff
- test output
- environment signal
- external API response

Data 未经解释，不应直接等同于事实。

### Observation

Observation 是从 Data 中提取出的结构化事实。

示例：

```json
{
  "kind": "test_failure",
  "module": "auth",
  "exit_code": 101,
  "source": "cargo test -p auth"
}
```

Observation 不是日志，而是世界状态变化。

### Evidence

Evidence 是很多 agent 最缺的一层。它回答：

- 某个判断依据来自哪里
- 哪些日志、测试、diff、trace 支撑它
- 证据是否可靠
- 是否存在反证
- 原始内容在哪里可以回看

推荐字段：

- source
- excerpt 或 summary
- confidence
- contradiction
- artifact path

没有 evidence，系统很难审计“为什么这么做”。

### State

State 是系统当前对世界的结构化理解。

示例：

```json
{
  "repo_dirty": true,
  "target_file": "docs/runtime.md",
  "tests_failed": [],
  "last_action": "edited_design_doc"
}
```

State 应该是持续存在的、可持久化的、可恢复的。

### Policy

Policy 表达长期偏好和治理约束。

示例：

```yaml
never:
  - delete_user_changes_without_request
  - run_destructive_git_commands_without_confirmation

prefer:
  - reversible_changes
  - small_scoped_edits
  - evidence_backed_final_answers
```

Policy 不应该散落在 prompt 文案里，而应成为 runtime 可检查对象。

### Decision

不要把决策理解为：

```text
decision = LLM(prompt)
```

更合理的形式是：

```text
decision =
  select(
    candidate_actions,
    state,
    evidence,
    policy,
    constraints,
    token_budget
  )
```

至少分两步：

1. 生成候选动作
2. 比较候选动作并选择

比较时要考虑：

- correctness
- risk
- reversibility
- policy alignment
- cost
- expected evidence gain

### Execution

Execution 只负责把 decision 变成 action：

- 调 shell
- 调 API
- 修改文件
- 调 MCP
- 运行测试
- 调外部服务

Execution 本身不代表智能，它必须被 permission、sandbox 和 audit 包住。

### Evaluation

成熟系统不只评估结果，还应评估：

- 是否达成目标
- 决策质量是否合理
- evidence 是否可靠
- 是否违反 policy
- 是否具有长期收益
- 是否可恢复

验证结果建议统一为：

- `PASS`
- `FAIL`
- `PARTIAL`

并记录实际命令和关键输出。

### Evolution

Evolution 负责根据证据改进 harness。

可演化对象包括：

- System Rules
- Tool Descriptions
- Tool Implementations
- Middleware
- Skills
- Sub-Agents
- Long-Term Memory

这就是 HARNESS 七组件。

### 决策链路数据模型

把上面的决策链路落到架构里，核心是把“看到了什么、怎么判断、依据什么、为什么这么做、结果如何、下次怎么改”全部结构化。

| 决策层 | 架构数据对象 | 作用 |
| --- | --- | --- |
| Data | `DataEvent` / `RawArtifact` | 原始输入、工具输出、日志、diff、用户反馈 |
| Observation | `ObservationRecord` | 从原始数据提取出的结构化事实 |
| Evidence | `EvidenceRecord` | 某个判断的依据、来源、可靠度、反证 |
| State | `SessionState` / `WorldState` / `StateTransition` | 当前世界模型和状态变化 |
| Policy | `PolicyRule` / `PolicyCheck` | 长期约束、权限、风险偏好、治理规则 |
| Decision | `CandidateAction` / `DecisionRecord` | 候选动作、比较结果、最终选择原因 |
| Execution | `ToolCall` / `PermissionDecision` / `ExecutionResult` / `ToolResultEnvelope` | 工具调用、权限、执行输出、摘要回流 |
| Evaluation | `VerificationRecord` / `DecisionEvaluation` | 结果验证、决策质量评估、PASS/FAIL/PARTIAL |
| Evolution | `ChangeManifest` / `LearningSignal` / `PolicyUpdateProposal` | 系统如何根据证据长期改进 |

### 数据来源与权威性

这些数据不是人工手写出来的，而是由采集、解析、推导、验证和复盘几条链路共同生成。

| 数据类 | 主要来源 | 生成方式 | 权威性怎么保证 |
| --- | --- | --- | --- |
| Data | 用户输入、tool output、logs、diff、API 响应、环境信号 | runtime/adapter 直接采集 | 保留 raw_ref、timestamp、source，不做二次改写 |
| Observation | Data 中可结构化的部分 | parser / normalizer 提取 | 保留原始引用和提取规则，支持回放 |
| Evidence | Data + Observation + 反证 | runtime evidence builder 关联 | 保留 source、confidence、contradiction、artifact path |
| State | 上一轮 state + 新 observation/evidence | state reducer 汇总 | 状态可持久化、可恢复、可比较 |
| Policy | 用户配置、项目配置、默认规则、安全规则 | policy loader / manager 装载 | 策略版本化、来源明确、可覆盖顺序明确 |
| Decision | state + evidence + policy + 候选动作 | decision engine 选择 | 保留 candidate_actions 和拒绝原因 |
| Execution | decision 驱动工具、shell、API、sandbox | executor / tool runner 执行 | 保留命令、退出码、stdout/stderr、artifact |
| Evaluation | 独立 verifier、测试、人工检查、指标系统 | verifier / evaluator 输出 | 真实命令 + 真实输出 + PASS/FAIL/PARTIAL |
| Evolution | 复盘、ChangeManifest、失败分析、指标回流 | review / learning loop 生成 | 每次变更都有证据、根因和验证计划 |

### 完整观测要求

完整观测不是只看“最后对不对”，而是把一个决策闭环都看见。

最少要覆盖：

1. 决策边界：这次决策从哪一刻开始、在哪一刻结束。
2. 原始输入：用户消息、工具输出、日志、diff、环境信号。
3. 结构化事实：从原始数据里提炼出什么。
4. 证据链：这个判断依据是什么，有没有反证。
5. 状态和规则：当时的 SessionState / WorldState 和 PolicyRule 是什么。
6. 候选动作：当时有哪些可选方案，为什么没选别的。
7. 执行结果：最后做了什么，工具怎么跑的，输出是什么。
8. 验证和复盘：结果是否被独立验证，决策质量如何，下次怎么改。

### 当前架构能否达到

现有架构已经有骨架，可以支撑这套观测，但还不是默认自动做到。

已有的骨架包括：

- `SessionState`
- `ToolResultEnvelope`
- `permission`
- `verification`
- `trace`
- `ChangeManifest`

还需要强制补成一等公民的部分：

- `DataEvent`
- `ObservationRecord`
- `EvidenceRecord`
- `DecisionRecord`
- `VerificationRecord`
- `StateTransition`

如果这些对象都被 runtime 强制生成、持久化和查询，那么架构就能达到完整观测；如果只保留摘要，不保留来源和证据链，就只能算部分观测。

### 强制观测约束

要让这套观测真正成立，下面三条必须变成 runtime 约束，而不是可选日志：

1. **强制生成**
   - 每个 turn 都要生成对应的 `DataEvent`、`ObservationRecord`、`EvidenceRecord`、`DecisionRecord`、`ExecutionResult`、`VerificationRecord`。
   - 不允许只有自然语言总结，没有结构化对象。

2. **强制持久化**
   - 原始数据、结构化事实、证据、状态变化、验证结果和演化记录都要落盘。
   - 至少保留 `raw_ref`、`artifact_path`、`timestamp`、`source`、`turn_id`。

3. **强制可查询**
   - 这些对象必须能按 `session_id / turn_id / decision_id / tool_call_id / task_id` 查询。
   - 这样才能支持 trace、复盘、审计、回放和演化分析。

## 查漏补缺

在现有架构基础上，再补这些关键点，能让观测、验证和演化链路更完整。

| 补强点 | 建议 |
| --- | --- |
| 数据模型版本化 | 给 `DataEvent / ObservationRecord / EvidenceRecord / DecisionRecord / VerificationRecord / StateTransition` 增加 `schema_version`、`created_at`、`producer`，避免字段演进后历史 trace 失效 |
| 全链路关联 ID | 统一 `session_id / turn_id / message_id / tool_call_id / decision_id / task_id / trace_id`，保证决策链可串联 |
| 原始与结构化分层 | `raw artifact` 单独存，结构化 record 单独存，避免摘要覆盖原始证据 |
| 可回放验证 | 验证记录必须包含命令、输入、输出、环境快照、版本号，支持重放 |
| 策略可追踪 | `PolicyRule` 增加来源、优先级、作用域和版本，避免 default/project/user 规则混淆 |
| 演化闭环 | 用 `ChangeManifest` 把失败证据、修复动作、验证结果、回归结论串起来 |
| host/loop 边界 | 将 session、权限 UI、compaction、MCP、plugin、skill 放在 host runtime，保持核心 loop 无状态 |
| recorded/live event 分离 | 持久化事件只记录可恢复事实，UI 流式事件只服务展示，避免 UI 状态污染 replay |
| 工具资源访问声明 | 每个工具执行前声明读写路径、搜索目录或全局副作用，用于权限、审计和并发调度 |
| wire/state/artifact 三层存储 | `wire` 保存事件事实，`state` 保存当前快照，`artifact` 保存大输出和原始证据 |
| trace issue detector | 从 event log 自动检测 orphan tool call、缺失 result、未闭合 step、未完成 compaction 等协议问题 |

### 推荐的最小字段

如果现在就要落表，最少建议每类对象保留这些公共字段：

```text
id
schema_version
session_id
turn_id
created_at
source
raw_ref
artifact_path
trace_id
parent_id
```

### 可选实施方案

可以按 4 个层次推进：

| 方案 | 结构 | 适合程度 |
| --- | --- | --- |
| 方案 1 | 单体分层：`CLI/TUI -> Runtime -> Store/Policy/Tools/Verify` | 最快落地，适合先做 MVP |
| 方案 2 | 核心 runtime + 适配器：`Runtime Core` 独立，入口都做薄 | 当前最推荐，兼顾维护和扩展 |
| 方案 3 | 事件溯源 + 可回放：所有决策链路对象事件化、可查询、可重放 | 适合你要的完整观测体系 |
| 方案 4 | 平台化 + 插件/多 agent：runtime + plugin + worker + verifier + harness | 后期演进，别一开始就上 |

### 推荐组合

主线建议是：

- 结构上按 **方案 2**
- 观测上按 **方案 3**
- 实施节奏按 **方案 1**
- 平台化能力放到后期，按 **方案 4** 扩展

## 决策审计与演化链路

前面的 Data / Observation / Evidence / State / Policy / Decision / Execution / Evaluation / Evolution 更像基础数据模型。要达到真正可用的观测体系，还需要把它升级为 **决策审计与演化链路**。

这个名字比“观察检测链路”更准确，因为目标不是只看 agent 做了什么，而是让每次任务都能做到：

- 可回放：能还原当时输入、状态、环境、工具输出和策略边界。
- 可评测：能把任务放进固定评测集或回归集里重复比较。
- 可归因：失败时能区分是数据、证据、状态、策略、规划、执行还是验证问题。
- 可演化：每次改 memory、skill、policy、tool、prompt 或 runtime，都能说明依据、风险和验证结果。

### Trajectory / Replay

`Trajectory` 应作为一等对象，而不是 transcript 的别名。

它记录一次任务或一个 turn 的完整行为轨迹：

- 用户输入和任务边界
- 关键 `DataEvent`
- `ObservationRecord`
- `EvidenceRecord`
- `SessionState` / `WorldState` 快照
- `PolicyCheck`
- 候选动作与 `DecisionRecord`
- 工具调用与 `ExecutionResult`
- `VerificationRecord`
- 最终结论和 `DecisionEvaluation`

`Replay` 是基于 trajectory 的复盘能力。它至少要支持三种用法：

| Replay 类型 | 作用 |
| --- | --- |
| 原样回放 | 还原当时输入、环境、工具输出和状态，判断当时为什么这么做 |
| 替换模型回放 | 同一 trajectory 换模型或参数，比较决策变化 |
| 修复后回放 | 修复 prompt、tool、policy、memory 后，验证同类任务是否改善 |

关键约束：

- 不依赖完整自然语言对话作为唯一依据。
- 不要求保存模型隐藏推理，只保存可审计的 decision summary、candidate actions、evidence refs 和 rejection reasons。
- 工具长输出仍然通过 `ToolResultEnvelope` 入模，原始内容落 artifact。
- Replay 必须能找到当时的环境快照，否则只能算 trace 浏览，不能算可回放。

### Evaluation Harness

`Verification` 和 `Evaluation Harness` 要分开。

`Verification` 回答的是：

```text
这一次任务是否完成？
```

`Evaluation Harness` 回答的是：

```text
这套 agent 架构、prompt、tool、skill、memory、policy 是否比上一版更可靠？
```

因此 evaluation harness 至少需要：

- 固定任务集：代表真实使用场景、边界场景和历史失败场景。
- 输入夹具：用户输入、项目文件、环境变量白名单、mock API 或录制工具输出。
- 期望标准：不是只有最终文本，还包括必须调用/禁止调用的工具、必须保留的证据、必须执行的验证。
- 指标：成功率、验证通过率、工具失败率、权限拒绝率、token 成本、重试次数、恢复成功率。
- 回归对比：新旧版本同一任务集的差异。
- 人工抽检入口：对模型判断质量、证据充分性和策略遵守情况做样本审计。

建议把历史失败 trajectory 自动沉淀成 evaluation case。这样每次修复都不是“感觉更好”，而是能回答：

```text
这个失败类型有没有被固定住？
是否引入新的回归？
成本有没有明显上升？
```

### Failure Taxonomy

完整观测必须有失败分类，否则 trace 很多但归因仍然模糊。

建议先采用下面这组最小分类：

| 类型 | 含义 | 典型信号 |
| --- | --- | --- |
| DataFailure | 原始数据缺失、过期、解析失败 | raw_ref 为空、工具输出不完整、网页/API 失败 |
| ObservationFailure | 从原始数据提取事实错误 | observation 与 raw artifact 不一致 |
| EvidenceFailure | 证据不足、证据冲突或引用不可靠 | confidence 低、contradiction 未处理 |
| StateFailure | 会话状态、任务状态或世界模型漂移 | resume 后上下文错乱、状态转移无法解释 |
| PolicyFailure | 权限、约束、优先级或安全规则处理错误 | 该拒绝未拒绝、该询问未询问 |
| PlanningFailure | 候选动作不足、顺序错误、过早收敛 | 没比较方案、跳过必要步骤 |
| ExecutionFailure | 工具、命令、网络、文件系统执行失败 | exit_code 非 0、timeout、sandbox 拒绝 |
| VerificationFailure | 验证缺失或验证标准错误 | 无真实命令、只用自然语言声称完成 |
| ContextFailure | 关键上下文被裁掉或无关上下文污染 | compact 后丢关键事实、memory 注入错误 |
| EvolutionFailure | 修复没有证据、不可回滚或无法评估 | ChangeManifest 缺根因/验证计划 |

每个 `DecisionEvaluation` 至少要能挂一个 `failure_type`。复杂失败可以挂多个，但必须有 primary failure，方便统计和优先级排序。

### EnvironmentSnapshot

很多 agent 决策失败不是模型本身的问题，而是环境变化导致的。因此 `EnvironmentSnapshot` 必须成为 trajectory 的组成部分。

建议最小字段：

```text
snapshot_id
session_id
turn_id
workspace_root
cwd
os
shell
git_ref_or_file_hashes
dependency_versions
tool_versions
model_provider
model_name
model_params
sandbox_policy
permission_scope
network_policy
env_whitelist
created_at
```

实现上不用首版就记录全量环境，但至少要记录会影响复盘的内容：

- 当前工作目录和 workspace root。
- 关键文件 hash 或版本引用。
- 工具版本，例如 git、node、pnpm、cargo、python、rustc。
- 模型 provider、model、temperature、max tokens、系统规则版本。
- sandbox、权限、网络代理和允许/拒绝策略。
- 环境变量白名单，不记录密钥原文。

没有 `EnvironmentSnapshot` 的 trace 只能解释“当时看起来发生了什么”，不能稳定解释“为什么在那个环境下发生”。

### OpenTelemetry / OpenInference 映射

内部数据模型不要被外部观测协议牵着走，但要能映射出去。

建议做一层 `ObservabilityAdapter`：

| 内部对象 | 外部映射 |
| --- | --- |
| `SessionState` / `TaskRecord` | trace root / span attributes |
| `DecisionRecord` | decision span / event |
| `ToolCall` / `ExecutionResult` | tool span |
| `ToolResultEnvelope` | span event + artifact ref |
| `PolicyCheck` | policy event |
| `VerificationRecord` | evaluation / verification span |
| `DecisionEvaluation` | eval result attributes |
| `FailureTaxonomy` | error type / status / attributes |

这样可以同时满足两点：

- 内部仍按 agent runtime 语义建模，不被 vendor schema 绑死。
- 后期可以接 Phoenix、Langfuse、OpenTelemetry GenAI、OpenInference 等生态，降低自研观测面板成本。

### Memory / Skill / Policy 分层

Memory、Skill、Policy 必须拆开管理，不能都塞进“长期记忆”。

| 层 | 保存什么 | 更新方式 | 风险 |
| --- | --- | --- | --- |
| Memory | 事实、偏好、历史经验、项目上下文 | 由 evidence 和用户反馈驱动 | 污染上下文、过期事实 |
| Skill | 可复用操作流程、工具组合、领域方法 | 由成功 trajectory 和人工整理沉淀 | 过拟合旧环境、步骤陈旧 |
| Policy | 权限、安全、治理、预算、不可违反约束 | 用户/项目/系统显式配置 | 错误放权或过度拒绝 |

三者的区别：

- Memory 影响“知道什么”。
- Skill 影响“怎么做”。
- Policy 影响“允许不允许做”。

这三类对象要使用不同的版本、审核和回滚机制。尤其是 Policy，不应该由模型在普通对话中自动改写；Skill 可以半自动提案，但需要验证；Memory 可以自动提取，但必须保留来源和过期机制。

### 外部研究和项目启发

这些建议对应到已有研究和开源项目，可以作为后续设计校准点：

| 来源 | 可吸收的设计点 |
| --- | --- |
| ReAct | action / observation 交替结构，适合作为 trajectory 的基本行为骨架 |
| Reflexion | 把失败复盘转成后续行为改进信号，但要用结构化 evidence 限制幻觉式反思 |
| Tree of Thoughts | 显式候选动作和分支评估，适合增强 `CandidateAction` 和 `DecisionRecord` |
| Voyager | Skill 和 long-term memory 分离沉淀，适合长期任务能力积累 |
| WebArena | 环境化任务评测，说明 agent 评测不能只看静态问答 |
| SWE-agent trajectory | 任务轨迹落盘和复盘，适合参考 CLI/代码任务 trace 结构 |
| LangGraph persistence | checkpoint / thread state 思路，适合恢复和回放 |
| OpenTelemetry / Phoenix / Langfuse | 外部 trace 与 eval 生态，适合作为观测导出层而不是内部核心模型 |

这里的核心结论是：

```text
不要只继续堆数据对象。
下一步要把数据对象组织成可回放、可评测、可归因、可演化的闭环。
```

## Prompt、历史、工具与预算设计

这一部分直接影响 token 消耗。

### Prompt 结构

低 token agent 的 prompt 结构应该固定，而且边界清晰。

推荐顺序：

1. 稳定系统规则
2. 当前任务摘要
3. 当前 SessionState
4. 当前 plan 或 open decisions
5. 最近关键 evidence
6. 命中的 memory facts
7. 最近必要消息
8. 工具结果摘要
9. token budget 提醒

### 稳定前缀与动态后缀

System Prompt 不应该是一段每轮都变化的大文案。应该区分稳定前缀和动态后缀。

稳定前缀：

- 核心身份
- 长期硬规则
- 输出协议
- 安全边界

动态后缀：

- 当前任务
- 当前仓库状态
- memory hits
- compact summary
- tool result summary
- token budget hint

这样能提高 prompt cache 命中，也能避免每轮重造长 prompt。

### 短系统规则

系统规则只保留稳定、长期、必须执行的内容。

不要每轮注入：

- 大段风格要求
- 重复性工程规范全文
- 完整 SOP
- 不必要示例

正确做法：

- 固定成短模板
- 大规则外置
- 按需摘要注入

### 当前任务摘要

不要反复塞原始多轮用户对话，应该压成一小段。

建议包含：

- 当前要完成什么
- 当前限制条件
- 当前输出物是什么

示例：

```text
任务：为 agent runtime 设计低 token 消耗方案。
限制：保持工具可用、支持恢复、避免长 prompt。
输出：设计建议文档。
```

### 当前工作上下文

只放与当前动作直接相关的信息，例如：

- 当前仓库路径
- 当前目标文件
- 当前正在修改的模块
- 当前验证命令

不要把整个目录树、完整 README、完整 docs 都塞进去。

### 命中的相关记忆

不要全量注入长期记忆。

正确做法：

- 先检索
- 只注入少量命中事实
- 每条尽量一两行
- 保留来源引用

### 最近必要交互摘要

不要回放整段 transcript。

只保留：

- 上一轮关键结论
- 当前未完成动作
- 最近一次关键工具结果
- 最近一次用户明确约束

### 历史分层

历史是 token 膨胀的第一来源。

建议分三层：

| 层级 | 内容 | 入模策略 |
| --- | --- | --- |
| Hot | 最近 1 到 3 轮关键原文 | 默认可入模 |
| Warm | working summary、关键 evidence、当前 plan | 默认入模 |
| Cold | 更早 transcript、raw artifacts、旧 tool output | 默认不入模，按需检索 |

历史策略的核心是：

```text
能摘要的摘要，能索引的索引，能外置的外置。
```

### Working Summary

每轮结束应更新 working summary。

建议字段：

- 已完成动作
- 关键发现
- 当前风险
- 未完成计划
- 下轮入口

working summary 是 resume 的主要来源。

### Compact Boundary

执行 compact 时必须写入边界。

示例：

```json
{
  "type": "compact_boundary",
  "turn_id": "turn_42",
  "summary_ref": "summaries/turn_42.md",
  "covered_message_range": ["turn_1", "turn_41"]
}
```

compact boundary 的价值：

- 防止重复摘要
- 让恢复知道哪些历史已被替代
- 支撑 transcript 审计和上下文压缩同时存在

### Tool 输出策略

工具输出是第二大 token 消耗源。

默认规则应该是：

1. 结构化优先
2. 摘要优先
3. 长文本落文件
4. 模型只看摘要和路径

推荐返回格式：

```json
{
  "tool_name": "exec_command",
  "status": "success",
  "summary": "发现 3 个相关文件，其中 1 个为 CLI 入口。",
  "artifact_paths": [
    "/abs/path/src/cli.rs",
    "/abs/path/src/runtime.rs"
  ],
  "truncated": true,
  "raw_ref": "artifacts/tool-output/turn-12.txt"
}
```

有了 `ToolResultEnvelope`，模型只拿到真正需要的信息。

### 长输出处理

下面这些内容不要直接整段进模型：

- 全量日志
- 完整 HTML
- 大文件全文
- 超长命令输出
- 大型 JSON
- trace 明细

应该先做：

- 提取关键信息
- 去重
- 去噪
- 截断
- 落文件
- 建立 raw_ref

### 面向不同输入的压缩策略

代码仓库：

1. 先列文件
2. 再搜关键词
3. 再只读相关片段
4. 最后把片段摘要送进模型

网页内容：

1. 去掉 script / style
2. 去掉导航、页脚、重复按钮文案
3. 提取正文、标题、时间、来源
4. 长正文落 artifact

日志与终端输出：

1. 保留命令
2. 保留退出码
3. 保留错误类型
4. 保留关键报错行
5. 丢弃大量成功噪声

大型 JSON：

1. schema 优先
2. 关键字段抽取
3. 按路径引用原文
4. 必要时生成 JSON pointer 摘要

Trace：

1. overview 看整体
2. detail 看单任务
3. raw trace 只按需回读

### Token 预算控制

runtime 应维护显式预算，而不是等模型报超长再处理。

建议记录：

- 当前轮输入 token
- 当前轮输出 token
- 会话累计 token
- 工具输出消耗
- memory 注入消耗
- compact 节省估算

动态降级顺序：

1. 先压缩工具输出
2. 再压缩历史
3. 再减少 memory hits
4. 再降低最近原文轮数
5. 再收紧输出长度
6. 再触发 compact
7. 最后才中断或请求用户选择

## 消息、状态与任务模型

### Message Model

这类系统的核心不是函数调用，而是结构化消息驱动状态变化。

建议消息类型：

- `system`
- `user`
- `assistant`
- `tool_use`
- `tool_result`
- `progress`
- `summary`
- `compact_boundary`
- `notification`
- `attachment`
- `tombstone`

建议字段：

```json
{
  "id": "msg_123",
  "type": "tool_result",
  "parent_id": "msg_122",
  "turn_id": "turn_9",
  "created_at": "2026-06-03T10:00:00Z",
  "content": {},
  "artifact_refs": [],
  "token_usage": {}
}
```

`parent_id` 对恢复、分支、子任务和 UI 展示都很重要。

### QueryState

运行时主循环不应该是一段隐式副作用脚本。它应该显式维护 QueryState。

建议字段：

- `messages`
- `turn_count`
- `active_tools`
- `pending_tool_summary`
- `auto_compact_tracking`
- `has_attempted_reactive_compact`
- `max_output_recovery_count`
- `stop_hook_active`
- `transition_reason`

这些字段让 runtime 知道当前为什么继续、为什么停止、为什么 compact。

### SessionState

SessionState 是 resume 的核心。

推荐字段：

```json
{
  "session_id": "s_123",
  "task_summary": "重构 agent runtime 设计文档",
  "working_summary": "已去除重复内容并保留详细说明",
  "recent_turns": ["turn_20", "turn_21"],
  "open_plan": ["检查结构", "更新历史记录"],
  "evidence_refs": ["artifacts/tool-output/turn-21.txt"],
  "memory_refs": [],
  "compact_boundary": "turn_18",
  "token_usage": {},
  "policy_context": {}
}
```

SessionState 不是 transcript 的替代品。transcript 继续用于审计和回放，但不应默认整体入模。

### Memory 分层

Memory 不应该是“全文注入仓库”。

建议分层：

- 用户级记忆
- 项目级记忆
- 会话级记忆
- 子任务级记忆

子 agent、fork、background task 都必须有上下文继承边界，不能把子任务噪声原样回灌主线程。

### TaskRecord

后台任务和子任务必须结构化。

推荐字段：

```json
{
  "task_id": "task_1",
  "type": "verification",
  "session_id": "s_123",
  "parent_task_id": null,
  "status": "running",
  "output_path": "artifacts/tasks/task_1.md",
  "error": null,
  "result_summary": null,
  "progress": []
}
```

状态建议：

- `pending`
- `running`
- `waiting_permission`
- `backgrounded`
- `completed`
- `failed`
- `cancelled`
- `killed`

没有 TaskRecord，就不应急着做后台 agent。

### 配置优先级

配置来源包括：

- CLI 参数
- session 配置
- 项目配置
- 用户全局配置
- plugin / skill frontmatter
- 环境变量
- 默认配置

优先级建议：

```text
runtime override / CLI 参数
> session 配置
> 项目配置
> 用户全局配置
> 默认配置
```

统一配置优先级能避免行为散落在代码里。

## 工具执行、安全与恢复

### Tool Pipeline

工具执行建议固定为以下管线：

1. 找到工具
2. 校验输入 schema
3. 执行自定义 validation
4. 执行 `PreToolUse` hooks
5. 做权限决策
6. 声明资源访问范围
7. 选择 sandbox
8. 进入工具调度器
9. 执行工具
10. 记录 telemetry
11. 执行 `PostToolUse` hooks
12. 生成 ToolResultEnvelope
13. 回流给模型

这条链路让 tool calling 从“模型说跑就跑”变成可校验、可阻断、可审计、可恢复的 runtime 行为。

### Tool Access Scheduler

Kimi Code 的工具调度值得吸收：工具不应该只有“能不能执行”，还要声明“会访问什么资源”。

建议每个工具执行前生成 `ToolAccesses`：

| 类型 | 示例 | 作用 |
| --- | --- | --- |
| file read | 读取单个文件 | 可与其他读并发 |
| file write | 修改单个文件 | 与同路径读写冲突 |
| file readwrite | 读后写同一文件 | 与相关路径读写冲突 |
| tree search | 搜索目录 | 可与只读并发，和目录写入冲突 |
| all | bash、外部服务、无法精确建模的副作用 | 默认全局互斥或进入更严格策略 |

调度规则：

- 只读和 search 可以并发。
- 任一任务包含写操作，并且路径重叠，就必须串行。
- 递归目录访问要按前缀判断 overlap。
- 无法建模副作用的工具默认 `all`，不能和其他有副作用工具并发。
- 工具结果仍按模型返回顺序写回，避免上下文顺序漂移。

这样可以同时获得两点：

- 安全性：避免两个工具并发写同一个文件或目录。
- 效率：允许多个只读搜索、读取、检查并行执行。

对 Golutra 来说，`ToolAccesses` 不只是调度输入，也应该进入 `PermissionDecision`、`ExecutionResult` 和 `DecisionRecord`，成为完整观测的一部分。

### Permission

权限结果统一为：

- `allow`
- `ask`
- `deny`

权限判断输入：

- 工具名
- 参数
- 文件路径
- workspace 类型
- 是否 destructive
- 用户策略
- 项目策略
- 当前任务风险等级

开放 bash、文件写入、网络、删除、git 操作之前，必须先有权限系统。

### Workspace Isolation

建议支持这些 workspace 类型：

| 类型 | 用途 |
| --- | --- |
| shared | 普通协作，直接修改当前工作区 |
| read-only | 探索、规划、审查 |
| temp | 临时验证、生成中间文件 |
| worktree | 高风险实现或并行实验 |
| remote | 远程任务或隔离运行 |

角色默认边界：

- Explore Agent：read-only
- Plan Agent：read-only
- Verification Agent：read-only + temp writable
- General Agent：shared 或 worktree
- 高风险实现任务：worktree

### Failure Recovery

失败不应只靠 retry。建议先分类：

- tool error
- permission denied
- context overflow
- model output invalid
- validation failed
- test failed
- workspace conflict
- external service unavailable

恢复策略：

| 失败类型 | 恢复策略 |
| --- | --- |
| tool error | 提取错误摘要，尝试替代工具或更小输入 |
| permission denied | 请求用户确认或改用只读方案 |
| context overflow | 触发 compact 或减少上下文 |
| model output invalid | 结构化纠错，限制重试次数 |
| test failed | 保存 evidence，进入修复循环 |
| workspace conflict | 停止覆盖，提示冲突文件 |
| external service unavailable | 降级、重试或延后 |

### Verification

验证是独立产品能力，不是最终回答里的自我声明。

验证结果必须包含：

- 检查项标题
- 实际执行命令
- 真实输出
- evidence path
- `PASS / FAIL / PARTIAL`
- 统一 verdict

最终回答里的“已完成”必须由 evidence 支撑。

## CLI 产品面

CLI 命令应该围绕 runtime 能力设计。

### `chat`

默认交互模式。

要求：

- 自动使用 working summary
- 自动压缩工具输出
- 自动记录 token usage
- 自动维护 transcript

### `resume`

恢复会话。

恢复内容：

- SessionState
- working summary
- compact boundary
- 最近关键 evidence
- 未完成 plan

不要直接回放完整 transcript 给模型。

### `summary`

显式生成当前会话摘要。

输出应能直接进入 SessionState，而不是自然语言随笔。

### `usage`

展示成本来源：

- 当前轮输入 token
- 当前轮输出 token
- 会话累计 token
- 工具输出 token
- memory 注入 token
- compact 节省估算

### `compact`

手动触发压缩。

结果：

- 生成 working summary
- 插入 compact boundary
- 冷历史转 artifact
- 下轮上下文只使用摘要和最近必要消息

### `trace`

查看任务轨迹。

至少支持：

- overview
- tool calls
- evidence
- verification
- token timeline

### `manifest`

记录 harness 修改。

对应 Change Manifest：

- failure evidence
- root cause
- changed component
- expected impact
- risk
- verification plan

## MVP 顺序

推荐按这个顺序落地：

1. 定义 message model 和 transcript 存储。
2. 实现最小 query loop。
3. 接入 basic tools。
4. 增加 ToolResultEnvelope。
5. 增加 transcript / resume。
6. 增加 permission engine。
7. 增加 working summary。
8. 增加 compact boundary。
9. 增加 memory 检索式注入。
10. 增加 token budget tracker。
11. 增加 verification runner。
12. 增加 task model。
13. 增加 workspace isolation。
14. 增加 hooks。
15. 增加最小 skills / plugin / MCP。
16. 增加 Change Manifest 和 trace analysis。
17. 增加 Trajectory / Replay，把历史失败任务沉淀成可回放样本。
18. 增加 Failure Taxonomy 和 DecisionEvaluation，支持稳定归因。
19. 增加 Evaluation Harness，把 verification evidence 转成回归评测集。
20. 增加 EnvironmentSnapshot，支撑真实复盘和跨版本对比。
21. 增加 OpenTelemetry / OpenInference 映射层，后期对接外部观测生态。

不建议首版优先做：

- 复杂 UI
- 大量 agent 类型
- 大而全插件系统
- 没有 transcript/resume 的 background task
- 没有权限系统的 bash 开放
- 没有 compact 的长期会话

这个顺序的重点是先把单 agent 闭环做稳，再扩展多入口、多 agent 和插件生态。

## 外部项目启发

这一部分只保留各外部资料的独有贡献，避免重复前文已经说明过的原则。

### `cg` 项目的架构优点

`cg` 的核心启发是平台化。

它不是简单命令行工具，而是一套围绕本地 agent runtime 构建的工程化平台。

最值得吸收的点：

- 外层 npm / 命令入口轻，核心逻辑放在统一 runtime。
- CLI、TUI、exec、app-server、mcp-server、SDK 复用同一套核心能力。
- Rust workspace 按能力拆分，例如 core、protocol、state、thread-store、tools、sandboxing、execpolicy、models-manager。
- 协议、状态、恢复是一等公民，而不是脚本附属逻辑。
- 工具执行、安全边界、权限策略单独成层。

对 Golutra 的启发：

```text
不要围绕某个命令入口组织系统，
要围绕 runtime 能力组织系统。
```

### 根目录 `docs/` 的核心共识

根目录 `docs/` 的多框架对比资料给出几条稳定共识：

- 运行时内核要明确：轻量场景可用回合制，长期演进更需要事件流和队列协议。
- 会话恢复不能靠临时拼接，必须有 transcript、状态对象和恢复链。
- 工具协议和权限模型要早定，否则后续安全、摘要和成本控制都会变得困难。
- 多 agent 之前先把单 agent 做稳。
- 成本控制不是附属能力，token 计数、prompt 压缩、历史裁剪、预算提醒、模型路由和输出长度控制都应内建。

### `ai-agent-deep-dive` 源码分析报告

这组报告把“低 token CLI”落到了可执行的产品 runtime 结构上。

独有贡献：

- 产品定位是面向软件工程任务的 AI 执行系统，不是聊天机器人或工具调用脚本。
- 能力地图覆盖入口、prompt 编排、工具执行、agent 调度、扩展生态、memory/session、任务后台、质量保证、界面体验。
- System Prompt 必须动态拼装，并区分稳定前缀和动态后缀。
- Message Model 要先定义，支撑 resume、compact、sub-agent 和 UI。
- QueryState 必须显式维护，不能依赖隐式副作用。
- Context Management 必须区分必须保留和可压缩信息。
- Verification 是独立产品能力，需要真实命令、真实输出和 PASS/FAIL/PARTIAL。
- TaskRecord 是后台任务和子 agent 的前提。
- Workspace Isolation 应按角色和风险分配。

这些内容已经分别落到本文的 prompt/context、message/state、tool/permission、verification 和 MVP 顺序中。

### Agentic Harness Engineering

AHE 的核心价值不是提供更强 CLI，而是把 agent 外层 harness 变成可观测、可演化、可回滚的软件工程对象。

HARNESS 七组件：

1. System Rules
2. Tool Descriptions
3. Tool Implementations
4. Middleware
5. Skills
6. Sub-Agents
7. Long-Term Memory

Change Manifest 应记录：

- 失败证据
- 根因
- 针对性修复
- 预测影响
- 风险任务
- 验证计划

Trace 分析建议分层：

- `overview.md` 看全局行为和主要失败模式
- `detail/{task}.md` 看单个任务的工具调用、运行日志、verifier output 和结果历史

对 Golutra 最值得吸收的部分：

- 用 HARNESS 七组件描述“改 agent 到底改了哪里”
- 用 Change Manifest 记录每次修改的证据、根因、预测和验证
- 长工具输出截断并落文件
- 旧上下文自动 compaction
- 用评测和 trace 驱动下一轮修改
- 从最小 agent 闭环开始演化

### Kimi Code

Kimi Code 的核心价值不是提出新算法，而是把 coding agent runtime 的工程边界做得很清楚。

最值得提取的精华：

- `stateless loop + thick host layer`：loop 只管模型 step、工具生命周期和事件输出；session、权限 UI、compaction、MCP、plugin、skill 都放到 host runtime。
- recorded event 和 live-only event 分离：恢复和审计只依赖 recorded event，UI streaming 失败不影响 turn。
- `wire.jsonl + state + blobs`：事件事实、当前状态和大原始数据分层存储，支持恢复、导出和诊断。
- `ToolAccesses`：工具声明读写资源，runtime 根据冲突关系决定并发或串行。
- `Kaos` 类环境抽象：工具不直接绑定本地文件系统和进程，未来可以切换 local、SSH、container 或 sandbox。
- `Kosong` 类 provider 抽象：模型调用、stream、tool call、finish reason 和 usage 统一归一化。
- Vis issue detector：从 wire 中自动发现缺失 tool result、未闭合 step、未完成 compaction 等协议问题。

对 Golutra 的取舍：

- 要学它的 runtime 分层、工具治理、事件持久化和 trace 诊断。
- 不要只学它的 wire，因为 wire 更像执行轨迹，不是完整决策审计。
- Golutra 应在这些工程底座上继续强制生成 `ObservationRecord / EvidenceRecord / DecisionRecord / VerificationRecord / DecisionEvaluation`。

## 反模式

下面这些设计会明显增加 token、复杂度和长期风险：

1. 每轮回灌完整对话历史。
2. 每轮注入完整系统规范和完整技能文档。
3. 工具返回原始全文，没有摘要层。
4. 网页抓取直接喂原始 HTML。
5. 会话恢复时直接重放 transcript。
6. CLI 自己拼上下文，runtime 只做转发。
7. 没有 transcript/resume 就做后台任务。
8. 没有权限系统就开放 bash 和文件写入。
9. 没有 compact boundary 就做长期会话。
10. 没有 verification evidence 就声称任务完成。
11. 把所有治理规则都写进 prompt。
12. 一开始就堆多 agent、复杂 memory 和插件系统。

## 最终路线

结合内部设计和外部启发，Golutra 的 agent 设计应收敛为：

1. 用薄 CLI 做入口和展示。
2. 用 host runtime 管理 session、agent、turn、权限 UI、compaction、MCP、plugin 和 skill。
3. 用 stateless loop 管理 message building、provider step、tool lifecycle 和 recorded events。
4. 用 ToolResultEnvelope 控制工具输出入模形态。
5. 用 ToolAccesses 和调度器控制工具并发与写冲突。
6. 用 working summary、SessionState 和 compact boundary 替代完整历史回灌。
7. 用检索式 memory 注入少量高相关事实。
8. 用 permission、hooks、policy 和 workspace isolation 管住工具执行。
9. 用 verification evidence 支撑最终完成判断。
10. 用 wire/state/artifact 和 issue detector 支撑恢复、诊断和可视化。
11. 用 HARNESS 七组件描述 agent 可演化部件。
12. 用 Change Manifest 管理每次 harness 修改。
13. 用 trace overview/detail 和 metrics 评估演化效果。
14. 用 Trajectory / Replay、Failure Taxonomy 和 Evaluation Harness 把 trace 升级成决策审计能力。
15. 用 EnvironmentSnapshot 和 ObservabilityAdapter 支撑真实复盘与外部观测生态。
16. 先把单 agent 闭环做稳，再扩展多入口、多 agent、插件和 MCP。

如果只保留一个工程判断：

```text
低 token 不是 prompt 技巧，而是 runtime 架构结果。
```

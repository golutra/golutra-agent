# Context 与 Memory 架构规格

## 文档定位

本文档定义 Golutra 的上下文、token 压缩和长期记忆治理。主架构见 `ARCHITECTURE.md`。

阶段边界说明：

- 本文档描述的是目标态的 Context & Memory 设计。
- 第一阶段以 [implementation-blueprint.md](/Users/skyseek/Desktop/project/open/golutra-agent/golutra-agent/docs/implementation-blueprint.md) 为准，只默认实现 `WorkingSummary`、`CompactionRecord`、`MemoryCandidate` 和 `project-scoped retrieval`。
- `user/global` 长期 memory 晋升、复杂 memory promotion 和重型检索策略属于后续增强，不是第一阶段必做。

## 当前实现状态

截至 2026-07-10，已落地以下受控最小闭环：

- `ContextBuilder` 按 contributor 构建 stable system prompt、canonical workspace environment context、会话摘要、project memory、evidence 和工具说明；provider request 前后分别记录 `TokenBudgetSnapshot` 与 `TokenUsageRecord`。
- compact 是 durable command/event；后续 turn 会复用 compact summary，同一 session 的历史不会作为完整 transcript 无界回灌。
- `MemoryStore` 持久化到 `<workspace>/.golutra/memory.json`，写入通过临时文件原子替换；文件 I/O 使用 `spawn_blocking`，不会阻塞 async runtime worker。Unix runtime 目录为 `0700`、memory 文件为 `0600`。
- 成功任务只能从 durable evidence 生成 project-scoped `MemoryCandidate`；promotion gate 检查 evidence、confidence、scope、敏感内容和 contradiction，失败或不安全候选不会写入 active memory。
- 每轮 task 会记录 `MemoryRetrieved`，只有 active、未过期且与 query 相关的 project memory 才进入 context；完整 memory 记录不直接当作 prompt 历史。
- `memory list` 和 `memory rollback` 已通过 CLI、HTTP transport 和 TypeScript SDK 暴露；rollback 保留事实记录和原因，不物理擦除审计历史。

当前没有实现 user/global memory、向量数据库、OS 级 secret store 或自动覆盖已有 memory。上述能力继续按本文件的治理边界后置。

核心原则：

```text
模型输入是 runtime state 的投影，
不是 transcript、memory、tool output 的简单拼接。
```

## 解决的问题

Context & Memory 子系统解决四类问题：

- token 持续膨胀。
- 关键事实被旧聊天淹没。
- 工具原文和日志污染模型注意力。
- 错误经验进入长期 memory 后持续误导 agent。

## Prompt 分层

推荐模型输入结构：

```text
Stable System Rules
Dynamic Runtime Context
Task Summary
Working Summary
Relevant Memory
Recent Necessary Interaction
Evidence Summary
Policy Constraints
Tool Instructions
```

稳定前缀保持短、稳定、可缓存；动态上下文按任务需要注入。

## 历史分层

```text
hot
  最近必要交互、未完成 tool call、当前回合关键上下文

warm
  working summary、compact summary、关键 evidence、当前文件状态

cold
  完整 transcript、rollout、artifact、raw tool output、历史 trace
```

恢复任务时优先读取 warm 和 hot，cold 只按需检索。

## 核心组件

### TokenBudgetTracker

每轮计算：

```text
system prompt
runtime context
working summary
relevant memory
recent messages
tool result excerpt
expected output
reserve for tool calls / summary
```

超预算时按顺序处理：

1. 移除低相关 memory。
2. 压缩旧交互。
3. 工具输出只保留摘要和结构化事实。
4. 旧 evidence 改成引用。
5. 触发 compact。
6. 必要时要求用户缩小目标。

Token 预算不是只在 provider 返回 usage 后才统计。Golutra 要把 token 分成三类观测：

```text
planned_tokens
  ContextBuilder 构造 prompt 前的预算与预估。

actual_tokens
  provider 返回的 input / output / reasoning / cached / total usage。

wasted_tokens
  后台或 debug 归因出的无效上下文、重复工具输出、失败 retry、无贡献模型输出。
```

`input_tokens` 必须被理解为完整 provider request 的模型可见输入，不只是用户消息。它至少包含：

```text
system prompt
developer / runtime instructions
policy constraints
user messages
assistant recent messages
context projection
working summary
memory snippets
evidence summary
tool instructions
tool result excerpts
```

`output_tokens` 是 provider 生成的可见 assistant 输出和 tool call 参数。`reasoning_tokens` 是 provider 暴露的隐藏推理消耗。`cached_input_tokens` 是 provider 侧命中的输入缓存部分。`tool_result_tokens` 只统计进入模型输入的工具结果片段，不统计保存在 artifact 中但没有进入 provider request 的 raw output。

### Token 消耗观测链路

第一阶段必须建立轻量同步链路：

```text
ContextBuilder
-> TokenBudgetSnapshot
-> ProviderRequest
-> ProviderUsage
-> TokenUsageRecord
-> LoopDecision.budget_state
-> DebugProjection / EvaluationProjection
```

每次模型调用至少记录：

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

provider 返回后记录：

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
```

为了定位 token 消耗来源，`TokenUsageRecord` 应关联一份可选的 `TokenAttribution`：

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
```

其中 attribution 可以来自 tokenizer 预估、provider usage、或二者结合。字段不完整时必须标记为 unknown，不能用 0 伪装成没有消耗。

如果 provider 不能返回完整 usage，`TokenUsageRecord` 也必须存在，但把缺失字段标为 unknown，并记录估算来源。不能因为 usage 不完整就断掉成本链路。

Token 消耗归因按来源分组：

| 来源 | 说明 | 处理方式 |
| --- | --- | --- |
| system / policy | 稳定系统规则、权限约束 | 保持短且可缓存 |
| runtime context | 当前任务状态、LoopDecision、约束 | 必须进入预算 |
| working summary | 当前任务摘要 | compact 后替代旧历史 |
| memory | 检索出的长期/项目记忆 | 低相关先剔除 |
| evidence | 关键证据摘要和引用 | 大内容只放 artifact ref |
| tool excerpt | 工具输出给模型看的片段 | 默认截断和摘要 |
| user / assistant recent | 近期必要交互 | 超预算时压缩旧消息 |
| output reserve | 给模型输出和 tool call 预留 | 不足时先缩输入 |

预算状态进入 `LoopDecision.budget_state`：

```text
budget_state
  planned_input_tokens
  actual_input_tokens
  output_tokens
  total_tokens
  estimated_cost
  budget_remaining
  compact_recommended
  cost_risk: low | medium | high | exceeded
```

触发动作：

- planned input 超过阈值：先 trim 低相关 memory 和 tool excerpt。
- 多轮 token 增长过快：触发 compact。
- tool output token 占比过高：要求工具改为 summary + artifact ref。
- retry / fallback 成本过高：LoopDecision 可转为 ask_user 或 blocked。
- evaluation / debug 需要深度分析时，只读 `TokenUsageRecord` 和 artifact，不重新把完整上下文塞回模型。

### ContextBuilder

`ContextBuilder` 从结构化状态投影模型输入：

```text
SessionState
GoalState
WorkingSummary
RelevantMemory
EvidenceSummary
PolicyState
RecentNecessaryInteraction
ToolInstructions
```

它不直接拥有 session，不写 UI，不做权限弹窗，不调用 provider。

第一阶段可以把输入来源实现为 `ContextContributor` 列表，而不是在一个巨型函数里拼 prompt：

```text
ContextContributor
  name
  order
  source: state | summary | memory | evidence | policy | tool | recent_interaction
  token_budget_hint
  build()
```

要求：

- stable 信息、volatile 信息和 history 信息分开预算。
- contributor 只能贡献模型可见片段和 metadata，不能执行工具或改变 runtime state。
- 每个 contributor 的 token 占比要能进入 `TokenAttribution`，便于定位浪费上下文。

### WorkingSummary

保存当前任务活状态：

```text
objective
completion_criteria
done
in_progress
blocked
key_files
key_evidence
unresolved_items
next_steps
risks
```

它是 resume 和 compact 后恢复任务的主要载体。

### CompactManager

compact 不是普通总结，而是 durable boundary：

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
  created_by
  verification_status
```

规则：

- 不能切断 tool call 和 tool result 配对。
- 不能丢失未解决问题、权限状态、修改文件和关键 evidence。
- compact 前后的 context projection 必须可 replay。
- compact 失败要进入 LoopDecision，不能无限重试。

### ToolResultEnvelope

工具输出必须分层：

```text
ToolResultEnvelope
  summary
  structured_facts
  model_visible_excerpt
  raw_artifact_ref
  evidence_refs
  risk
  verification_hint
```

模型默认只看摘要、结构化事实和必要片段。完整输出进入 artifact store。

### MemoryRetriever

memory 检索必须可解释：

```text
query
scope: user | project | global
retrieved_memory_ids
source_refs
confidence
reason
```

第一阶段优先使用 SQLite、rg、tree-sitter 和结构化索引。向量检索可以作为增强，但不能成为恢复、审计、replay 和 benchmark 的基础依赖。

长期记忆后端如果要扩展，核心只依赖窄契约：

```text
MemoryBackend
  recall(query, scope, top_k)
  store(session_id, messages)
  feedback(signals)
  start()
  stop()
```

第一阶段不必做完整插件生态，但文档和接口要避免把 memory backend 设计成拥有 context、runtime、tool 和 policy 的大对象。

### MemoryGovernance

长期 memory 不能自动乱写。候选结构：

```text
MemoryCandidate
  source_task_id
  evidence_ids
  proposed_scope
  confidence
  contradiction_ids
  expiry
  promotion_status

MemoryPromotionRecord
  candidate_id
  reviewer
  benchmark_result
  rollback_plan
```

写入条件：

- 来自成功或高质量失败 trajectory。
- 有 evidence。
- 有明确 user/project/global 作用域。
- 通过 contradiction check。
- 高风险 memory 经过 human review 或 benchmark gate。

## 六个项目带来的边界

- Pi：compact boundary、recent tokens 保留、不能切断 tool result。
- Kimi Code：durable wire event 和 context projection。
- OpenCode：结构化 compaction summary、工具输出截断、compaction event。
- cg：Rust runtime 内把 compaction 作为事件接入 normal/debug/replay。
- Claude Code Best：token 阈值、warning/blocking、auto-compact 失败熔断。
- Hermes Agent：memory provider、context engine、memory 注入清洗和作用域隔离。

Golutra 只吸收这些边界，不照搬六套系统。

## 与 Runtime Loop 的关系

```text
TokenBudgetTracker
-> ContextBuilder
-> Provider Step
-> ToolResultEnvelope
-> Verification
-> LoopDecision
   -> continue
   -> compact
   -> ask_user
   -> stop
```

compact、memory retrieval 和 memory promotion 都必须产生 runtime event。

## 判断标准

合格的 Context & Memory 系统必须满足：

- 长会话不会线性增加 prompt。
- resume 不依赖完整 transcript 回灌。
- 工具大输出不会污染模型输入。
- 关键 evidence 不会被 compact 丢失。
- 长期 memory 有来源、有作用域、有过期、有回滚。
- debug/replay 能解释某条上下文为什么进入模型。

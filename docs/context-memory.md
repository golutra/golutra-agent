# Context 与 Memory 架构规格

## 文档定位

本文档定义 Golutra 的上下文、token 压缩和长期记忆治理。主架构见 `ARCHITECTURE.md`。

阶段边界说明：

- 本文档描述的是目标态的 Context & Memory 设计。
- 第一阶段以 [implementation-blueprint.md](/Users/skyseek/Desktop/project/open/golutra-agent/golutra-agent/docs/implementation-blueprint.md) 为准，只默认实现 `WorkingSummary`、`CompactionRecord`、`MemoryCandidate` 和 `project-scoped retrieval`。
- `user/global` 长期 memory 晋升、复杂 memory promotion 和重型检索策略属于后续增强，不是第一阶段必做。

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

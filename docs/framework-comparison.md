# Agent 框架对比与 Golutra 架构影响

## 文档定位

本文档承载外部项目调研结论，避免主架构文档被调研过程污染。Golutra 主架构见 `ARCHITECTURE.md`，技术选型见 `agent-runtime-technology-selection.md`。

## 总体结论

六个项目不是谁完全胜出，而是分别证明了不同边界：

| 项目 | 核心价值 | Golutra 吸收点 |
| --- | --- | --- |
| cg | Rust-first runtime、SQLite、event、sandbox、ratatui | 作为 runtime kernel 和 TUI 主线参考 |
| OpenCode | 事件化 session、API、多端共享、provider 覆盖意识 | 吸收 session/API 设计，不采用 TS 核心 |
| Pi | harness/provider 分层、compaction、session tree | 吸收 loop hook、compact boundary、provider registry |
| Kimi Code | wire/state 分离、context projection、vis/replay | 吸收 durable event、context projector、issue detector |
| Claude Code Best | 终端交互、权限体验、fallback、auto compact | 吸收交互密度、阈值熔断、fallback 一致性 |
| Hermes Agent | SQLite/FTS5、memory provider、多入口、插件 provider | 吸收 memory governance 和长期运行经验 |

Golutra 的最佳路线不是照搬任一项目，而是：

```text
Rust Runtime Kernel
+ ratatui/crossterm TUI
+ SQLite / event log / artifact store
+ ProviderContract / CapabilityMatrix
+ ToolResultEnvelope / PermissionPolicy / SandboxBackend
+ ContextBuilder / CompactManager / MemoryGovernance
+ Verification / Replay / PostTaskReview
```

## 评测平台借鉴

LangSmith、Braintrust、Promptfoo 不是这里六个框架的一部分，但它们代表了另一类更成熟的工程能力：观测、评估、红队和 CI 检查。

它们值得借鉴的点是：

| 平台 | 可借鉴能力 | 在 Golutra 里的对应 |
| --- | --- | --- |
| LangSmith | trace、span、debug view、run 回看 | `RuntimeEvent`、`DebugProjection`、`replay` |
| Braintrust | dataset、experiment、scorer、对比实验 | `EvaluationProjection`、`RegressionResult` |
| Promptfoo | 配置式 eval、red team、CI 检查、安全测试 | `PolicyEvaluation`、`VerificationRecord`、`golutra-eval` |

这些能力的价值不在于“架构更先进”，而在于它们已经证明：

1. 任务轨迹需要结构化，不只是聊天记录。
2. 评估需要可重复，不只是一次性总结。
3. 安全和越权要能自动化测试，不只是人工审核。

Golutra 应该吸收这些方法，但仍然保持自己的 runtime 定位，不把自己改造成评测平台。

## TUI 选型

推荐：

```text
crossterm TerminalRuntime
ratatui Render Layer
Golutra business components
RuntimeEvent -> UiState projection
```

不推荐：

- React/Ink 作为核心 TUI。
- OpenTUI/Solid 作为核心 TUI。
- TUI 自己维护任务状态。

原因：

- Golutra 是 Rust-first runtime，同进程 TUI 更容易共享 event/state。
- 权限确认、工具进度、debug panel、replay panel 都应来自 runtime projection。
- 普通 UI 和 debug UI 可以共享数据源，但展示层不同。

## Provider 选型

推荐：

```text
Golutra ProviderContract
  -> NativeAdapter
  -> GenaiProviderAdapter
  -> OpenAICompatibleAdapter
```

`rust-genai` 可作为 adapter 加速覆盖多 provider，但不进入核心数据模型。

必须保留：

- `ProviderConfig`
- `ModelCatalog`
- `CapabilityMatrix`
- `ModelRouteDecision`
- `FallbackPolicy`

原因：

- native adapter 承载 OpenAI Responses、Anthropic thinking/cache、Gemini multimodal 等深能力。
- genai adapter 负责快速接入常见 provider。
- fallback 需要 loop 层掌握上下文一致性，不能由 adapter 私自处理。

## Storage 选型

推荐：

```text
SQLite state
Durable event log
Artifact store
FTS5 / tantivy
State snapshot
Replay timeline
```

原因：

- SQLite 适合本地状态、索引、权限审计、session 列表。
- event log 适合 replay、benchmark、trace diff 和失败复现。
- artifact store 适合保存大工具输出、diff、raw logs。
- FTS5/tantivy 适合可解释检索，优先级高于外部向量数据库。

## Context / Memory 选型

吸收点：

- Pi 的 compact boundary。
- Kimi 的 context projection。
- OpenCode 的结构化 summary。
- Claude 的 token 阈值和 auto-compact failure circuit breaker。
- Hermes 的 memory provider 和注入清洗。
- cg 的 Rust event integration。

Golutra 统一为：

```text
TokenBudgetTracker
ContextBuilder
WorkingSummary
CompactManager
MemoryRetriever
MemoryGovernance
ArtifactStore
```

## 架构取舍

采用：

- Rust-first runtime。
- 薄 CLI/TUI/API/SDK。
- 统一 Session Command Protocol。
- RuntimeEvent / StateProjection / ContextProjection。
- LoopDecision 作为任务循环唯一判断出口。
- normal/debug/replay 展示分离。

不采用：

- Python 或 TypeScript 作为核心 runtime。
- LangChain 类框架作为主架构。
- 单纯 transcript resume。
- 自动写长期 memory。
- provider adapter 内部隐式 fallback。
- 多 agent 直接共享上下文。

## 关联文档

- `ARCHITECTURE.md`：最终主架构规格。
- `context-memory.md`：上下文和记忆规格。
- `evaluation-observability.md`：观测、验证和评估规格。

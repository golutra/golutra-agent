# Agent Runtime 技术选型建议

## 文档定位

本文档基于 `low-token-cli-design.md` 的架构判断，给出 Golutra agent runtime 的语言、模块和库选型建议。

核心原则：

- 核心 runtime 优先选择稳定、可测试、可分发、可长期维护的技术。
- CLI/TUI/App/SDK 都只是入口，不应重复实现状态机。
- 工具、权限、状态、恢复、预算、trace 和 verification 应集中在 runtime。
- 选型优先满足可恢复、可治理、可验证、可演化，而不是短期拼装速度。

## 总体推荐

### 语言选择

| 层 | 推荐语言 | 结论 |
| --- | --- | --- |
| Runtime Core | Rust | 主语言，承载 query loop、state、tool pipeline、permission、store、verification |
| CLI / TUI | Rust | 与 runtime 同进程或低成本调用，减少入口层复杂度 |
| App Server | Rust | 用统一 runtime 对外暴露 HTTP/WebSocket/SSE |
| SDK / Web UI | TypeScript | 适合前端、插件开发体验和对外类型分发 |
| Python | 可选 | 只做 SDK、实验脚本、第三方生态适配，不做核心 runtime |

首推路线：

```text
Rust core + Rust CLI/TUI/App Server + TypeScript SDK/Web
```

不建议：

- Python 做核心 agent loop。
- Node/TypeScript 做本地工具权限、状态恢复和 sandbox 核心。
- 一开始引入大而全 agent 框架作为主架构。

## 为什么核心用 Rust

Rust 更适合当前文档里的 agent runtime 目标：

- 类型系统适合定义稳定协议：Message、SessionState、ToolResultEnvelope、TaskRecord。
- 可控并发适合 tool execution、background task、event stream。
- 单 binary 分发更适合本地 CLI/TUI。
- 文件系统、进程、权限和 sandbox 边界更容易集中治理。
- 与 `cg` 项目的 Rust workspace 思路一致，适合按能力拆 crate。

Python 和 TypeScript 的优势主要在生态和开发速度，但它们更适合放在边缘：

- TypeScript：Web UI、SDK、插件类型、配置编辑器。
- Python：用户脚本、数据处理、研究实验、兼容 SDK。

核心状态机不要跨语言分裂。

## Rust Workspace 拆分

推荐先按 runtime 能力拆分，而不是按入口拆分。

```text
golutra-core
golutra-runtime
golutra-context
golutra-tools
golutra-policy
golutra-store
golutra-memory
golutra-llm
golutra-verify
golutra-otel
golutra-cli
golutra-tui
golutra-app-server
sdk/typescript
sdk/python
```

### 模块职责

| 模块 | 职责 |
| --- | --- |
| `golutra-core` | Message、SessionState、ToolResultEnvelope、TaskRecord、Policy 等核心类型 |
| `golutra-runtime` | query loop、turn 状态机、tool call/result 回流、resume/compact 调度 |
| `golutra-context` | prompt builder、working summary、history 分层、token budget |
| `golutra-tools` | tool registry、schema validation、tool execution、ToolResultEnvelope |
| `golutra-policy` | permission `allow/ask/deny`、workspace isolation、路径策略 |
| `golutra-store` | transcript、session state、artifact、task record、migration |
| `golutra-memory` | memory 检索、项目索引、代码片段召回 |
| `golutra-llm` | provider abstraction、模型请求、流式响应、usage 解析 |
| `golutra-verify` | verification runner、PASS/FAIL/PARTIAL、证据记录 |
| `golutra-otel` | tracing、metrics、event export/import |
| `golutra-cli` | 薄 CLI 入口 |
| `golutra-tui` | TUI 入口，只展示 runtime 状态 |
| `golutra-app-server` | HTTP/WebSocket/SSE 入口 |
| `sdk/typescript` | Web/插件/外部集成 SDK |
| `sdk/python` | 可选兼容 SDK，不承载核心逻辑 |

## 核心库推荐

### CLI / TUI

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| CLI 参数 | `clap` | `chat/resume/summary/usage/compact/trace/manifest` 命令 |
| TUI | `ratatui` + `crossterm` | 交互式终端、状态卡片、工具进度、历史渲染 |
| 错误展示 | `miette` 或 `color-eyre` | 面向 CLI/TUI 的可读错误 |

CLI 层要保持薄，不要在命令 handler 里拼 prompt 或裁剪历史。

### 异步与服务层

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 异步 runtime | `tokio` | LLM streaming、tool task、background task、HTTP server |
| Web/App Server | `axum` | 本地 API、SSE/WebSocket、桌面或 Web 前端入口 |
| Middleware 抽象 | `tower` | permission、hooks、telemetry、retry、rate limit |

`tower` 的 service/middleware 思路很适合 tool pipeline：

```text
schema validation -> pre hook -> permission -> sandbox -> execute -> post hook -> envelope
```

### 协议、Schema 与错误

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 序列化 | `serde`、`serde_json` | 所有 message/state/tool result 均结构化 |
| Schema 生成 | `schemars` | 从 Rust 类型生成 JSON Schema |
| Schema 校验 | `jsonschema` | tool input/output、配置、插件声明校验 |
| 类型错误 | `thiserror` | library 层错误类型 |
| 应用错误 | `anyhow` / `miette` | binary 层聚合错误 |

建议所有对外协议都 schema-first，避免 SDK 和 runtime 类型漂移。

### LLM Provider

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| HTTP client | `reqwest` | OpenAI/Anthropic/OpenAI-compatible provider 调用 |
| Streaming | `tokio` + `reqwest` stream | token 流、tool call 流、usage 事件 |
| Provider abstraction | 自研 trait | 不把 runtime 绑死到某个 vendor SDK |

建议核心用 provider abstraction：

```rust
trait LlmProvider {
    async fn stream(&self, request: LlmRequest) -> Result<LlmStream>;
}
```

原因：

- OpenAI-compatible provider 很多，字段和错误形态不完全一致。
- 不同模型对 tool call、usage、reasoning token 的返回差异很大。
- provider SDK 可以做 adapter，但不要进入 runtime core。

### 持久化与本地数据

| 能力 | 推荐库 | 建议 |
| --- | --- | --- |
| 主存储 | SQLite + `rusqlite` | 首选，用于 transcript、SessionState、TaskRecord、artifact index |
| async DB | `sqlx` | 如果 app-server 中强依赖 async DB，再考虑 |
| 轻量 KV | `redb` | 可选，用于 cache 或小型本地索引 |
| migration | `refinery` 或自研 SQL migration | 保持 store 可升级 |

首版推荐 SQLite，不要一开始上外部数据库。

理由：

- 本地 agent 需要可复制、可备份、可排查。
- transcript 和 task record 适合 SQL 查询。
- SQLite 比 ad-hoc JSON 文件更容易做 migration 和索引。

### Memory 与代码理解

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 全文检索 | `tantivy` | 本地 docs/code/memory 搜索 |
| 代码解析 | `tree-sitter` | 代码符号、函数范围、片段召回 |
| 文件遍历 | `ignore`、`walkdir` | 遵守 `.gitignore`，避免扫无关目录 |
| glob 匹配 | `globset` | workspace policy、工具路径匹配 |
| 路径处理 | `camino` | UTF-8 path，减少跨平台坑 |

首版 memory 不建议直接上向量数据库。先做：

1. 项目文件索引
2. 文本检索
3. 代码片段定位
4. working summary / memory fact 检索

向量检索可以作为后续增强，不应成为 MVP 前提。

### 权限、Sandbox 与插件

| 能力 | 推荐库 / 方案 | 建议 |
| --- | --- | --- |
| 权限决策 | 自研 policy engine | `allow/ask/deny` 必须可解释 |
| 路径隔离 | `camino` + canonicalize + policy matcher | 防路径穿越、symlink 逃逸 |
| 进程执行 | `tokio::process` | 统一封装 stdout/stderr/exit code |
| 插件 sandbox | `wasmtime` | 后期做 Wasm 插件，不作为首版阻塞项 |
| MCP | 官方 Rust SDK `rmcp` | 放在 adapter 层，不进入 core |

Sandbox 不建议一开始做成大平台。MVP 先有：

- read-only / shared / temp / worktree workspace 类型
- destructive 操作识别
- path allow/deny
- shell command approval

### Trace、Telemetry 与 Verification

| 能力 | 推荐库 | 用法 |
| --- | --- | --- |
| 结构化日志 | `tracing` | runtime event、tool event、decision event |
| 日志订阅 | `tracing-subscriber` | CLI/TUI/App Server 不同输出 |
| 指标导出 | OpenTelemetry | 后期接入，不阻塞 MVP |
| Snapshot/Golden test | `insta` | message、tool envelope、trace 输出测试 |
| 临时目录 | `tempfile` | tool/sandbox/store 测试 |
| Mock HTTP | `wiremock` 或 `httpmock` | LLM provider 测试 |

Verification 需要单独建模：

```text
check title
command
exit code
key output
evidence path
PASS / FAIL / PARTIAL
verdict
```

## MVP 技术栈

第一阶段建议只选这些：

```text
Rust workspace
tokio
clap
serde / serde_json
schemars / jsonschema
reqwest
rusqlite
tracing / tracing-subscriber
thiserror / anyhow
ignore / walkdir / globset / camino
ratatui / crossterm
```

第一阶段先不做：

- Wasm 插件系统
- 完整 MCP 生态
- 向量数据库
- 多 agent 编排
- 复杂 Web UI
- 云端控制面

## 第二阶段技术栈

当 message model、query loop、tool pipeline、resume、permission、compact、verification 稳定后，再引入：

```text
axum
tantivy
tree-sitter
OpenTelemetry
wasmtime
rmcp
TypeScript SDK
```

第二阶段目标：

- 本地 app server
- 更强 memory retrieval
- 代码结构理解
- trace export/import
- 插件和 MCP adapter
- Web/IDE SDK

## 不推荐的选型

### 不推荐 Python 核心

原因：

- 工具执行、权限、路径隔离、并发和二进制分发更难长期治理。
- 类型边界不如 Rust 稳定，message/state/schema 容易漂移。
- 适合实验和 SDK，不适合核心 runtime。

### 不推荐 Node 核心

原因：

- 本地文件、进程、sandbox、权限策略更容易分散。
- 长期运行的本地 agent runtime 更需要强边界。
- TypeScript 更适合 SDK/Web，而不是底层执行内核。

### 不推荐 LangChain 类框架做主架构

可以参考，不建议成为 core dependency。

原因：

- 当前文档强调的是 runtime state、permission、trace、compact、verification。
- 大框架容易把核心控制权藏进抽象里。
- Golutra 应该拥有自己的 message model 和 tool pipeline。

### 不推荐首版上向量数据库

原因：

- MVP 的 token 问题主要来自历史和工具输出，不是语义检索不够。
- 本地全文检索、tree-sitter 结构切片、working summary 已经能覆盖大量场景。
- 向量检索可后置，避免首版引入模型、索引、召回质量和存储复杂度。

## 推荐落地顺序

1. 建 `golutra-core`：定义 Message、SessionState、ToolResultEnvelope、TaskRecord。
2. 建 `golutra-store`：SQLite transcript/session/task/artifact 存储。
3. 建 `golutra-runtime`：最小 query loop。
4. 建 `golutra-tools`：tool registry + ToolResultEnvelope。
5. 建 `golutra-policy`：permission `allow/ask/deny` 和 workspace isolation。
6. 建 `golutra-context`：working summary、history 分层、compact boundary、token budget。
7. 建 `golutra-llm`：provider abstraction + reqwest adapter。
8. 建 `golutra-cli`：薄 CLI 命令面。
9. 建 `golutra-verify`：验证结果结构化。
10. 建 `golutra-tui`：复用 runtime 状态做展示。
11. 第二阶段再做 `golutra-app-server`、memory index、MCP、插件和 SDK。

## 参考链接

- Tokio: https://tokio.rs/
- clap: https://docs.rs/clap/
- Serde: https://serde.rs/
- reqwest: https://docs.rs/reqwest/
- tracing: https://docs.rs/tracing/
- tower: https://tower-rs.github.io/tower/
- Ratatui: https://ratatui.rs/
- crossterm: https://docs.rs/crossterm/
- rusqlite: https://docs.rs/rusqlite/
- SQLx: https://sqlx.dev/
- tree-sitter: https://tree-sitter.github.io/tree-sitter/
- Tantivy: https://tantivy-search.github.io/
- Wasmtime: https://docs.wasmtime.dev/
- Model Context Protocol: https://modelcontextprotocol.io/

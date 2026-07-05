# Agent Terminal Issue Log

## Description
- This file is the issue fact log for recent execution problems.
- Load this file before each task to review recent issue patterns and avoid repeating them.
- Add a new entry whenever execution hits a real problem such as rework, misjudgment, omission, rollback, compatibility pit, validation gap, command misuse, or unintended scope expansion.
- Each entry must stay concrete and include `Problem`, `Action`, and `Verification`.
- If no issues occur in the current task, do not add a new entry.
- Do not write long-term rules, abstractions, or one-time task summaries here; promote stable lessons to `agent-guidelines.md`.
- Use datetime format `YYYY-MM-DD HH:MM` (24h).

## Mandatory Action
- MUST: If this registration table reaches 50 entries, summarize the stable and reusable lessons into `agent-guidelines.md`, then clear only the summarized issue rows.

## Registration Table
| Time | Problem | Action | Verification |
| --- | --- | --- | --- |
| 2026-07-05 22:35 | 后台 AgentLoop 接入后，client 测试继续依赖 `sleep` 任务的调度时间来验证 persisted active task，导致任务完成时序不稳定并触发断言失败。 | 将该测试改为直接写入持久 `TaskCreated` 事件来构造 active task 状态，避免依赖后台任务执行时长。 | `cargo test -p golutra-client` 通过。 |
| 2026-07-05 22:15 | 检查 TUI 文件非 ASCII 时误把 NUL 范围写进 shell 命令，导致 `exec_command` 拒绝执行。 | 改用 `rg -nP '[^\\x00-\\x7F]' crates/golutra-tui/src/main.rs` 检查非 ASCII 内容。 | 命令返回退出码 1 且无输出，确认目标文件无非 ASCII 字符。 |
| 2026-07-05 21:21 | TUI 单元测试把 `RuntimeEventSource` / `RuntimeEventType` 误从 `golutra-core` 导入，实际这两个协议枚举定义在 `golutra-protocol`，导致 `cargo test -p golutra-tui` 编译失败。 | 调整测试导入为 `golutra_protocol::{RuntimeEventSource, RuntimeEventType}`，保持 core 只承载基础 ID 和领域类型。 | `cargo test -p golutra-tui`、`cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 通过。 |
| 2026-07-05 18:54 | P0.4 工具执行器保留了未接入的 `excerpt_limit` 字段，导致 clippy dead-code 失败。 | 移除该未使用字段，P0 使用统一常量控制模型可见 excerpt 长度。 | `cargo clippy --workspace --all-targets -- -D warnings` 通过。 |
| 2026-07-05 18:54 | P0.5 context crate 需要消费 provider usage，但 `golutra-llm` 只私有导入了 `ProviderUsage` / `UsageSource`，导致跨 crate 编译失败。 | 在 `golutra-llm` 显式 re-export usage 类型，保持 context 通过 llm 反腐层消费 provider usage。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
| 2026-07-05 18:54 | P0.6 `AgentLoop` 初版循环末尾无条件 `break`，触发 clippy `never_loop`，说明 max iteration 语义没有真正生效。 | 调整循环为有工具证据后下一轮停止进入 verification，保留 max iteration 边界。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
| 2026-07-05 18:54 | P0.8 app-server 测试使用 `chrono::Utc` 但未声明 dev-dependency，且保留了未使用的 `StreamExt` 导入。 | 补充 `chrono` dev-dependency 并删除未使用导入。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
| 2026-07-05 18:39 | `golutra-runtime` 引入了未使用的 `StateProjection` 导入，导致 clippy 在 `-D warnings` 下失败。 | 删除未使用导入，保持 runtime crate 只依赖实际使用的 protocol 类型。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
| 2026-07-05 18:37 | `golutra-store` 的测试代码使用 `chrono::Utc`，但 `chrono` 未声明为该 crate 的 dev-dependency，导致 clippy 的 test target 编译失败。 | 将 `chrono.workspace = true` 加入 `golutra-store` 的 dev-dependencies。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
| 2026-07-05 18:34 | `schemars` 1.x 的 `schema_for!` 返回类型不再支持旧式 `.schema.metadata()` 字段访问，导致 clippy/test 编译失败。 | 将 schema smoke 改为把 schema 序列化成 JSON 后检查对象结构，避免依赖旧 API 细节。 | `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace` 均通过。 |
|  |  |  |  |

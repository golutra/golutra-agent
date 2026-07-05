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
| 2026-07-06 01:55 | 默认 mock provider 实际可运行，但 onboarding 返回 `configured=false/missing active_profile`，TUI 底部误导用户以为必须先配置真实 provider；同时 mock agent 对 `write file smoke.txt with content ok` 这类自然语言只写默认文件名。 | 将无 provider 配置的 onboarding 状态明确为默认 mock ready；mock write plan 优先结构化 payload，缺省时解析 `write/create ... with content ...` 的路径和内容；resume picker 增加可视窗口滚动。 | `cargo test -p golutra-client`、`cargo test -p golutra-config`、`cargo test -p golutra-tui`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 通过；临时 workspace CLI smoke 创建 `smoke.txt=ok`；TUI smoke 首屏显示 `ready (mock)` 并可 `/quit`。 |
| 2026-07-06 01:49 | TUI `/resume` 仍是直接恢复默认 thread，没有像 Codex 一样进入当前 workspace 的 session 列表；同时 raw mode 下 Ctrl+C 没有稳定结束当前 TUI 进程。 | 增加当前 workspace resume picker，`/resume` 打开 session 列表，方向键/数字选择、Enter 恢复、Esc 取消；压缩顶部 header；Ctrl+C 先 abort 活跃任务再退出 TUI。 | `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 通过；TUI 伪终端 `/resume` 列表和 Ctrl+C 退出 smoke 通过。 |
| 2026-07-06 01:38 | 上次修复 stale default thread 时保留了 `list_threads(None, 1)` 全局 fallback，可能让当前 workspace 的 `/resume` 绑定到其他 workspace 的 session。 | 对齐 Codex 的 cwd/workspace filter 设计，移除 workspace 启动 repair 的全局 fallback；`resume_thread` 和 `fork_thread` 显式拒绝 workspace_root 不匹配的 thread；新增跨 workspace 回归测试。 | `cargo test -p golutra-client`、`cargo test -p golutra-tui`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 通过；TUI 伪终端 `/resume`、`/quit` smoke 通过。 |
| 2026-07-06 01:32 | `.golutra/default-thread` 指向 SQLite `threads` 表中不存在的 thread，导致 TUI 启动和 `/resume` 报 `thread ... not found`。 | 在 workspace RuntimeHost 启动时 repair default thread：优先使用有效指针，其次 fallback 当前 workspace 最近 thread，再 fallback 全局最近 thread，最后 bootstrap 新 thread；同时让 prompt 按 session 更新 resumed/forked thread 元数据，并给 TUI 增加 slash command 提示。 | `cargo test -p golutra-client`、`cargo test -p golutra-tui`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 通过；TUI 伪终端 `/resume`、`/help`、`/quit` smoke 通过；密钥扫描无匹配。 |
| 2026-07-05 23:52 | live provider smoke 读取不存在的 `README.md` 时，工具返回失败但 verifier 只检查 evidence_refs 非空，导致弱错误 evidence 被误判为 Pass。 | 将 AgentLoop 中每个 tool report 转成 `VerificationCheck`，只有 `ToolResultStatus::Ok` 才通过；失败工具现在生成 Partial/Failed 而不是 StopSuccess。 | `cargo test -p golutra-runtime` 通过；新增 `agent_loop_does_not_stop_success_when_tool_fails` 测试。 |
| 2026-07-06 00:06 | `ProviderProtocol::OpenAiCompatible` 使用 serde `kebab-case` 自动序列化成 `open-ai-compatible`，与配置和文档约定的 `openai-compatible` 不一致。 | 为该枚举变体增加显式 serde rename，并新增 wire id 序列化单元测试。 | `cargo test -p golutra-llm` 通过；`cargo run -p golutra-cli -- provider protocols` 输出 `openai-compatible`。 |
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

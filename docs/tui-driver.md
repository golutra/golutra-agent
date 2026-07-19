# Native TUI Driver

## 目标

`golutra-tui inspect` 和 `golutra-tui driver` 让测试程序、SDK 和 coding agent 直接驱动 Golutra 的真实 TUI，而不是抓取 PTY ANSI 输出或维护第二套 headless UI。

Driver 复用交互 TUI 的 `TuiApp`、`TuiRuntimeController` 和 `draw_ui`。每个快照都由 `ratatui::TestBackend` 离屏渲染，因此文字、折行、CJK 宽字符、Developer runtime、滚动状态和鼠标命中区域与正常 TUI 使用同一份实现。任务、事件和治理事实仍只来自 `RuntimeHost`；Driver 只拥有输入框、选择、滚动、布局和冻结帧缓存等 UI 状态。

```text
agent / test / SDK
        |
        | inspect CLI or versioned NDJSON
        v
TuiDriver -> TuiApp -> draw_ui(TestBackend)
        |
        v
TuiRuntimeController -> RuntimeTransport -> RuntimeHost
```

## 启动方式

一次性运行 prompt，等待治理作业结束，返回本轮回复和 Developer runtime：

```bash
golutra-tui --cwd /absolute/workspace inspect \
  --session new \
  --prompt "inspect this workspace" \
  --view response+developer \
  --width 160 \
  --height 40 \
  --format json
```

长期 stdio Driver：

```bash
golutra-tui --cwd /absolute/workspace driver \
  --stdio \
  --session new \
  --width 160 \
  --height 40
```

可断线重连的 Unix socket Driver：

```bash
install -d -m 700 "$HOME/.golutra/tui-driver"
golutra-tui --cwd /absolute/workspace driver \
  --socket "$HOME/.golutra/tui-driver/session.sock" \
  --session current
```

Driver 默认连接用户级 `golutra-app-server`。`--embedded` 创建仅供隔离测试使用的进程内 RuntimeHost；`--connect URL` 使用经过认证的 HTTP/SSE runtime。daemon transport 下，Driver 退出或 socket 断开不会取消 runtime task。embedded transport 的 runtime 生命周期属于 Driver 进程，进程退出后不能继续执行任务。

## Workspace、Session 和 Task 绑定

一个 Driver 实例在启动时固定绑定一个 canonical workspace 和一个 session：

| `--session` | 语义 |
| --- | --- |
| 省略或 `new` | 创建随机的新 session/thread；首个 prompt 才持久化 thread |
| `current` | 使用 runtime attachment 当前广告的 default session；有历史时即当前 workspace 最新 thread |
| `new:<uuid>` | 使用调用方指定的新 session ID；已存在时返回 `session_exists` |
| `<uuid>` | 只允许绑定当前 workspace 已存在的 session，否则返回 `session_not_found` 或 workspace 错误 |

全局 `--task-id <uuid>` 是可选的严格过滤。该 task 必须已有事件并属于所选 session，否则启动失败并返回 `task_not_found`。Driver 不会把其他 workspace 或其他 session 的 task 静默映射到当前视图。

绑定在 Driver 生命周期内不可变。`input_slash`、composer 键盘路径以及 `/n`、`/r` 等候选补全路径中的 `/new`、`/resume`、`/fork` 均返回 `session_binding_immutable`；调用方必须为目标 session 启动另一个 Driver。显式 `--task-id` 用于只读观察，prompt、takeover、abort、abort-before-close、审批快捷键和会话控制 slash command 均返回 `task_binding_read_only`，避免历史 task Driver 操作同 session 的其他 active task。`/status`、`/debug`、`/threads`、`/export`、`/clear`、`/quit` 等只读或本地 UI 命令仍可使用。

同一 active session 可以被多个 daemon Driver 观察，但只有 active controller 能提交输入。observer 的 prompt 会被 runtime busy policy 拒绝；显式 `takeover` 成功后才可控制该 lane。

## Inspect 语义

`inspect` 可选参数：

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--prompt TEXT` | 无 | 提交一个 prompt；省略时只查看已有 session |
| `--wait CONDITION` | 有 prompt 时 `auto`，否则 `none` | `auto` 对普通回复等待 `task-terminal`，对 Developer 视图等待 `evaluation-terminal`；无 prompt 时显式条件仍会等待已有 session/task |
| `--timeout-ms N` | `120000` | wait 超时 |
| `--view` | `response` | `response`、`response+developer`、`developer`、`task`、`task+developer`、`session`、`screen` |
| `--detail` | `text` | `text` 或 `cells` |
| `--rows START:END` | 全部 | 1-based 闭区间，单次最多 200 行 |
| `--format` | `json` | `json`、`ndjson` 或 `text` |

`response+developer` 的自动等待包含 durable post-task evaluation，因此完整帧应覆盖 `VerificationRecord`、terminal `PostTaskJob` 和 evaluation 事件。若 retention、event window、verification、context 或治理作业仍不完整，命令仍返回帧，但 `complete=false` 且 `missing_sections` 明确列出缺口。

## NDJSON 协议

权威 Rust contract 位于 `golutra-protocol::tui_driver`，当前协议版本为 `1`。每个请求是一行 JSON，必须带 1 到 128 bytes 的 `request_id`。Driver 启动或 socket 客户端每次连接后，先主动发送 `request_id="ready"` 的 `ready` 响应。

```json
{"request_id":"hello-1","type":"hello","protocol_version":1}
{"request_id":"prompt-1","type":"input_prompt","text":"你好"}
{"request_id":"wait-1","type":"wait","until":{"kind":"task_terminal"},"timeout_ms":120000}
{"request_id":"frame-1","type":"snapshot","scope":"current_turn","panes":"response_and_developer","width":160,"height":40,"rows":{"start":1,"end":40},"detail":"text"}
{"request_id":"close-1","type":"close","abort_active_task":false}
```

支持的输入和控制请求：

- `input_prompt`：直接提交 prompt。
- `input_slash`：执行以 `/` 开头的 TUI slash command；切换 session 的命令受固定绑定约束，认证/OAuth、resume 或 export modal 活跃时仅允许 `/quit` 穿透。
- `input_key`、`input_paste`：复用真实 composer、候选列表和快捷键路径。
- `input_mouse`：左键、滚轮上移和滚轮下移。
- `resize`：改变 active viewport，并清空旧冻结帧。
- `wait`：等待 ready、idle、task/turn terminal、approval/auth、evaluation 或指定 RuntimeEvent。
- `state`、`capabilities`、`ping`、`takeover`、`abort`、`close`。

Driver 会推送轻量 `event` 和 heartbeat 通知，但不会主动推送完整 frame。调用方始终通过 `snapshot` 拉取确定的 UI 状态。stdio 模式 stdout 只写 NDJSON；诊断只写 stderr。

`wait` 会注册为挂起条件，由协议循环在同步 tick 上判定，而不会独占请求处理器。注册时冻结当前 command ID 和 event cursor，task/turn 可由随后到达的同 command 事件补全，因此后续 prompt 不会把旧 wait 改绑。每个 tick 只遍历事件一次并构建不可变 `WaitFacts` 索引，所有 pending wait 共享 command anchor、task/turn terminal、approval/auth、event watermark 和 evaluation job 集合。等待期间同一连接仍可处理 `ping`、输入、`abort`、`takeover` 和 `close`，heartbeat 也会继续发送；最多 64 个 wait 可按各自唯一的 `request_id` 和 deadline 并存。复用挂起 ID 返回 `duplicate_request_id`，超限返回 `too_many_pending_waits`。

`accepted` 只表示对应 UI/Runtime 操作实际被接受。prompt、slash controls、takeover、abort、审批快捷键和 `close(abort_active_task=true)` 都检查 Runtime `CommandAck.accepted`；observer、无 active lane 或其他拒绝返回 `command_rejected`。abort-before-close 被拒绝时 Driver 保持打开，不会把仍在运行的任务伪装成已关闭。`/quit` 与两次 Ctrl+C 会返回 `closed`，不再只修改内部 `TuiApp.should_quit`。

### 连续 prompt

每次 prompt 都记录提交前的 event high-watermark 和 command ID。`task_started`、`task_terminal`、`turn_terminal` 和 `evaluation_terminal` 只匹配本次提交解析出的 task/turn，不会因上一轮仍显示 `Completed` 而提前返回。

### 尺寸和输入上限

- viewport width：`40..=320`。
- viewport height：`8..=200`。
- 单帧最多 64K cells。
- 单次返回最多 200 行。
- prompt/paste/key text 单次最多 256 KiB，composer、认证和导出编辑字段的累积输入也不超过 256 KiB，并拒绝 NUL。
- 单行 NDJSON 请求最多 1 MiB；超限行会被完整丢弃，下一行仍可处理。
- wait 最长 10 分钟；Driver idle timeout 最长 24 小时。
- 每个连接最多 64 个挂起 wait，挂起期间 `request_id` 不得复用。

长进程必须先发送 `resize`，再以相同 width/height 请求 `snapshot`。尺寸不一致返回 `viewport_mismatch`，从而保证快照 hit region 和后续鼠标输入属于同一个 active viewport。

## Snapshot 和冻结分页

`scope` 决定事实范围：

- `current_turn`：只保留最新 task 的最新 turn。
- `task`：只保留最新或显式绑定的 task。
- `session`：当前加载的 session window。
- `screen`：当前 TUI 屏幕状态，保留交互滚动位置。

`panes` 可选 `transcript`、`developer`、`response_and_developer` 或 `full_screen`。语义视图从尾部确定性渲染；`screen/full_screen` 用于观察鼠标和键盘改变后的真实滚动状态。所有 `full_screen` 都会脱敏瞬时 UI 状态并返回 `redaction_status="redacted"`；debug mode 下还包含 Developer pane 并执行 Developer completeness 检查，普通模式的全屏不要求 Developer 数据。

每个新快照得到内容寻址的 SHA-256 `frame_id`。Driver 最多缓存 8 个完整帧，TTL 为 60 秒。后续分页请求携带相同 `frame_id`，并且 width、height、scope、panes 和 detail 必须完全一致；分页期间即使 runtime 又产生事件，也返回同一冻结帧、相同 `event_high_watermark` 和相同 `frame_id`。过期返回 `frame_expired`，参数漂移返回 `frame_mismatch`。

`TuiFrame.lines[].row` 和 cell row/column 是选定输出区域内的 1-based 坐标。`hit_regions` 是 0-based 屏幕全局坐标，可直接用于 `input_mouse`。CJK/emoji 宽字符的 continuation cell 不会重复出现在 cell 输出中。

## 安全和披露

Developer frame 使用 rollout 的 canonical redaction policy 处理事件 payload 和完整 DebugProjection。敏感键、authorization/token/password 字段和已知 secret 前缀会替换为 `<redacted-secret>`。`full_screen` 渲染前也会脱敏 composer draft、命令消息、状态错误以及 provider setup 的 API key、custom headers、credential review，再恢复真实 UI 状态，因此未发送或暂存的 credential 不会进入 lines 或 cells。frame 只包含脱敏文字、样式 cell、hit region、artifact manifest 和治理统计，不包含 artifact blob、provider authorization 数据、API key、OAuth token 或 credential store 内容。

所有 snapshot 都对事件和可能覆盖所选 pane 的瞬时 UI 应用 canonical redaction，并返回 `redaction_status="redacted"`。完整 raw artifact 仍受 Runtime transport 和 artifact disclosure policy 控制，不能通过 TUI Driver 读取。

Unix socket 模式执行以下边界：

- socket 父目录必须真实存在或由 Driver 创建，并且不能向 group/other 开放。
- socket 与 lock file 权限为 `0600`。
- exclusive lock lease 防止并发 Driver 抢占同一路径，也防止未持锁进程删除 stale socket。
- socket 和 lock path 拒绝 symlink 或错误文件类型。
- 客户端断开后 TuiDriver、滚动状态、冻结帧和 `instance_id` 保留；下一个客户端收到同一个实例。

daemon 暂时不可用时，Driver 保持 UI 实例并报告有界 sync error；后台 sync 最长占用协议循环 1 秒。socket 客户端仍会得到缓存的 `ready` 和 `state`，并可按 `frame_id` 读取重启前的 frozen frame。app-server 使用同一 home 重启后，transport 会重新 attach workspace；每次 accepted prompt 都从当前 event cursor 重建订阅并 replay，避免仍连接旧 RuntimeHost 的 stale SSE 漏掉新任务事件。socket 客户端重连始终得到原 Driver `instance_id`。

## SDK 和验证

`TuiDriverProtocolBundle` 已包含在 `SdkProtocolBundle`。请求、响应、wait、状态、controller、notification、pane 和 redaction 字段在生成 SDK 中保留为判别联合或枚举；Python 不会退化成 `dict[str, Any]`。以下命令生成 JSON Schema、TypeScript 和 Python 类型：

```bash
just schema
just ts-check
just py-check
```

进程级验收位于 `crates/golutra-tui/tests/tui_driver_process.rs`，覆盖：

- one-shot complete Developer frame 和 secret redaction。
- 多轮 stdio prompt、按键提交、CJK cells、冻结分页、resize 和 close。
- owner-only socket、lock contention、断开重连和不安全目录拒绝。
- socket/lock symlink 拒绝、pending wait 期间 heartbeat 和 idle timeout 生命周期。
- daemon observer/takeover/approval、Driver 退出后任务继续。
- strict session/task/workspace binding，包括 session 切换拒绝和显式 task 只读约束。
- 挂起 wait 的 submission anchor、数量/ID 上限，以及期间 ping、abort 和 heartbeat/control 继续响应。
- 冻结帧分页稳定性、overlay 脱敏和 CJK continuation cell。
- app-server 停机时 cached ready/frozen frame、重启后 transport reattach、prompt/event replay 和 Driver instance 保持。

```bash
cargo test -p golutra-tui --test tui_driver_process -- --test-threads=1
```

8 帧 cache capacity 和 60 秒 TTL 使用 Tokio instant 的确定性单元测试覆盖，避免进程验收固定等待一分钟。

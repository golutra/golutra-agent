# 会话恢复与历史编辑审查

审查日期：2026-09-26。范围为 `/resume`、双 Esc、历史导航/编辑、鼠标复制、分支边界及共享 runtime 控制权。对照本地 Codex 源码 `project/codex`，提交 `fc269b66`，工作区无修改；不把本地源码版本等同于所有已发布 Codex 客户端。

## 业务语义对照

| 场景 | Codex 源码行为 | Golutra 当前行为 |
| --- | --- | --- |
| `/resume` 已有会话 | 恢复原 thread；恢复当前 thread 是 no-op | 保留原 thread/session；同一会话保留草稿、历史和滚动位置 |
| 确认历史请求 | 在选中 prompt 之前创建保留源会话的分支，恢复原文供编辑 | 同样创建新 thread/session；确认不调用模型，再次 Enter 才提交 |
| 第一条请求 | 新分支没有之前的对话 | 已覆盖第一条和后续请求回归 |
| 历史左右/上下 | 左右移动选择；上下滚动 transcript | 相同键位，但 Golutra 预览为 prompt/assistant/tool 摘要，并非完整 transcript 的同一渲染投影 |
| 历史页 Esc | 此 Codex 版本仍将 Esc 解释为选择更早请求，q 退出 | 按用户明确要求，Esc 和 q 都退出；这是保留的产品差异 |
| 双 Esc 时间 | primed 状态，无固定 500ms 窗口 | 两次空输入 Esc 间隔不超过 500ms，其他按键取消组合 |
| steer | 可以出现在 transcript，确认时拒绝独立分支 | 从可编辑选择列表隐藏，并由 runtime 再校验 |
| 工作区文件 | 编辑历史不会回滚文件 | 相同；对话分支与文件恢复不是同一操作 |
| 鼠标复制 | 原生终端选择依赖终端模式 | Resume/历史页不捕获鼠标；滚轮借助 alternate-scroll；不支持该模式的终端仍需方向键/PageUp/PageDown |
| 历史分支在 `/resume` | 会话选择器与 backtrack 状态分离；活动 thread 由 ID 标识 | 默认隐藏已被后续历史编辑替代的父分支，保留最新叶子；`Alt+B` 展开全部分支；当前项即使是父分支也保留并标为“当前” |
| 分支标题 | lineage 由 thread/session ID 保存，不依赖标题文本 | 新建分支的持久化标题压平为单层 `Fork of ...`；TUI 列表展示根标题并标记“分支”，旧数据同样处理 |

Codex 依据：`codex-rs/tui/src/app_backtrack.rs`、`app_backtrack/legacy_input.rs`、`app/session_lifecycle.rs`。其中 prompt-edit 校验同时检查持久化输入和选择器里的原文/附件是否一致。

## 本轮已完成的修复

- `/resume` 使用会话分页，突破原来只列 50 个条目的限制；兼容旧服务器的有限列表接口。
- 恢复当前会话不清空页面、不重复记录 `/resume`；切换之前先验证目标快照。
- before-turn 分支排除所选 prompt 及对应的 `CommandReceived`；识别 causal turn ID；拒绝 steer 和明确未结束的 turn。
- 分支加载失败清理已创建的 child，保留源会话与选择；确认前重新读取并核对原文/附件，不使用失败读取时的过期缓存。
- observer 提交被拒绝时保留草稿，`/takeover` 仍可到达 runtime；driver 返回 `command_rejected`。
- 修复已结束任务的旧 controller 导致恢复/daemon 重启后不能发消息的问题；只有活动任务保留排他控制权。
- `/new` 清除旧 observer 状态及 Esc 组合状态，避免新会话继承旧控制权。
- 历史页先裁剪视口前的完整逻辑行，修复多行历史超过 65,535 行后无法显示末尾请求的问题。
- PTY fixture 接收 socket 后显式关闭非阻塞模式，避免 BSD/macOS 上客户端尚未写请求就被误判为空连接。
- 历史编辑分支使用单层可读标题；`/resume` 按 parent thread 折叠已被后续历史分支替代的父项，保留当前 thread 并支持 `Alt+B` 查看完整分支树。
- 双 Esc 打开历史时锁定 `(thread_id, session_id)` 基线，异步加载或 UI 状态变化不会把 prompt edit 错 fork 到其他会话；关闭、恢复或新建会话时清理该基线。

## 剩余四项的实施结果

### 排队请求采用完整消费边界

before-turn 和普通 inclusive fork 共用消费顺序：首轮从接收命令之前开始，排队轮从 TurnStarted 开始。复制边界内的历史时排除尚未消费、所选及后续轮次的队列事件和关联命令，保留前序完整回复与工具结果。未消费/已取消的队列没有执行位置，若恢复其原文则在已完成历史尾部建立分支，不截断当时正在执行的工具。

中间轮次不再要求同 turn 的 TaskCompleted；同 task 后续终态或下一轮开始可以证明交接。TUI 删除重复校验，runtime 统一校验活动任务与 steer。inclusive fork 的下一边界跳过 steer，保留本轮的补充指令。

验证包含真实 Embedded runtime 的 A 等待工具审批、B/C 排队、更新 B、取消另一条队列、完成至 C 后逐轮分支，以及确定性工具调用/结果配对与源历史不变回归。

### 分支身份统一重映射

store 在同一事务中选择并复制事件，同时映射 envelope、payload、causal_context 中的 session/task/turn 和内部 causal_links；只在 causal_context 中出现的 turn 也参与映射。边界外 parent_event_id/causal_links 被移除，父来源通过 ThreadForked 和 thread lineage 明确记录。队列快照删除未复制 turn 的引用。

run、provider、tool 等实际执行身份保持原值；分支复制历史并不代表重新执行。回归覆盖非空因果字段、causal-only turn、内部链接和边界外链接。

### 后台加载与可取消交接

session_loading.rs 负责 /resume 目录分页、历史加载、恢复与 prompt-edit。页面立即显示，目录分页到达即展示；列表不再查询事件页或拼接运行时元数据，加载期间可搜索、滚动、Esc 退出。

每个加载有独立通道与取消令牌，并绑定源 session；切换页面或会话会丢弃旧结果。历史读取期间的新事件合并去重，失败保留源会话、草稿和选择。目标数据准备完毕后一次性交接，controller 不再重复读取准备好的全量历史。

fork 请求发出后不直接 abort：worker 等待 child 身份和 UI 接收确认，取消、选择变化、加载失败或未被接收时回收 child。正常退出等待清理；清理失败明确报告。目录分页限流回到键盘循环，慢服务器不会占住按键处理。

### 历史物理行缓存与虚拟视口

历史预览按内容快照、终端宽度和配色缓存物理行及 turn 范围，选择高亮只应用于可见行；上下/左右、滚动上限和鼠标区域复用同一份布局。使用 usize 行索引直接截取视口，不再把单行折行偏移传给 u16 scroll。

中文、组合 emoji 和窄窗口使用现有按字素/显示宽度折行器。宽度变化按逻辑行/显示列恢复手动视口，不把页面重新拉回所选请求。测试覆盖超过 65,535 行的单个无换行输入、末尾中文/emoji、1–3 列窄窗、缓存复用、内容失效及 resize。

## 验证与保留差异

全量命令：`cargo test --locked -p golutra-agent-store -p golutra-agent-client -p golutra-agent-app-server -p golutra-agent-tui --quiet`。

| 范围 | 结果 |
| --- | --- |
| app-server 单元 / 跨进程 / RPC | 57 + 7 + 6 通过；长时间 soak 1 项忽略 |
| client | 446 通过 |
| store 单元 / 集成 | 63 + 4 通过 |
| TUI lib / main | 19 + 489 通过 |
| 真实 PTY | 35 通过 |
| TUI driver / daemon 重启 | 7 通过；真实上游 smoke 1 项忽略 |

合计 **1,133 项通过，2 项既有可选测试忽略**。后续选择器收口移除了事件页元数据懒加载，新增回归改为断言普通界面不显示运行验证字段。四个 crate 全目标 clippy `-D warnings`、fmt 和 diff check 通过。

实施中曾出现新增方法插错 impl、测试仍按同步加载断言、使用了未封装的 ratatui Terminal，以及中文末尾跨行的断言不匹配；这些均已修正并通过上述全量回归，未把首次失败隐藏为成功。

保留的产品差异：500ms 双 Esc 组合、Esc 退出历史页、历史摘要预览。工作区文件不随会话分支回滚。原生鼠标复制由终端实现，自动测试覆盖终端模式、输入与 PTY 回放，不冒充所有终端的系统剪贴板人工验收。

未 commit、push 或发布版本。

## 2026-09-28 会话选择器收口

- 普通 `/resume` 和 `/export` 不再读取事件页来拼接 `status`、`verify`、`model` 等运行时事实；列表只显示会话目录提供的更新时间。运行验证、模型和详细事件继续由 `/debug` 提供。
- 恢复请求绑定选中的 `thread_id`。确认后如果用户改变选择，旧的异步结果会被取消并丢弃，迟到的结果不会切换到旧会话；Esc 取消后同样不会交接旧结果。
- 双 Esc 历史编辑在启动和交接两个阶段都校验 `(thread_id, session_id)` 来源身份。会话已经变化时不创建分支，保留当前草稿和历史。
- `/resume` 与 `/export` 复用同一套列表行投影：中文“当前”、中文“分支”、预览、更新时间和 Alt+I 详情保持一致；导出仍显示完整分支树，但不重新加入运行时元数据。
- 会话选择器内提前拒绝当前已附着会话的 Archive/Delete，避免发送必然被 runtime 拒绝的命令。
- 导出目录构造不再逐个请求事件页，选择器打开速度与会话数量线性相关于目录分页。

本轮新增回归覆盖迟到恢复结果、过期历史来源、当前会话归档/删除保护，以及列表中不出现运行验证字段。工作区仍未 commit、push 或发布版本。

## 2026-09-28 本轮回归

- TUI：19 个 lib、490 个主测试、35 个真实 PTY、7 个 Driver 通过，1 个既有 Driver 测试忽略。
- client：446 项通过；store：57 个单元、63 个集成、4 个迁移测试通过；app-server：7 个单元、6 个跨进程测试通过，1 个既有 soak 忽略。
- 四个 crate 的 all-targets Clippy `-D warnings`、`cargo fmt -- --check`、`git diff --check` 通过。
- 本轮未执行 commit、push 或发布；工作区原有未提交修改全部保留。

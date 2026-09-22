# 手动交接 `/handoff`

`/handoff` 把当前未完成工作的必要上下文整理为新会话的起始草稿；`/handoff <目标>` 则围绕指定目标整理。来源为当前会话有效历史路径上的最新压缩摘要和其后的对话，不包含其他分支，也不复制整个执行历史。

## 使用

1. 当前任务结束或中断后，输入 `/handoff`，例如 `/handoff 继续修复 parser，保留现有 API`。
2. 全屏页面生成草稿。按 `Esc` 取消，留在原会话。
3. 直接编辑草稿；`Enter` 换行，方向键移动，`Home/End` 到开头/结尾，`Ctrl+Z/Ctrl+Y` 撤销/重做。保留终端原生文字选择，不接管鼠标。
4. 按 `Ctrl+S` 确认，创建关联会话，并将编辑后的文本放入输入框。**不会自动发送，也不会开始执行任务。**
5. 检查输入框后，按 `Enter` 才发送。原会话仍可通过 `/resume` 找回。

生成或创建失败时保留原会话。生成失败可 `Esc` 返回后重试；创建失败保留编辑内容，重试使用原目标 ID，避免重复建会话。创建已经开始后，`Esc` 不撤销提交中的数据库事务。确认过的初始草稿和新会话原子保存；退出后 `/resume` 可恢复未发送的初始草稿，发送后的再次编辑不属于该持久化快照。

## 实现边界

- 使用当前 provider/profile/model/generation 设置，沿用共享 `LlmProvider` 抽象，未为不同协议另建摘要执行逻辑。无工具调用，不授予新权限。
- 复用 `CompactionSummaryPlan` 的输入/输出预算、完整结束检查和一次长度修复：只接受正常结束、无工具请求、非空且可完整保存的结果。截断或超预算时从原历史重写一次；持续失败直接报错，不用不完整摘要伪装成功。
- 不安装 `CompactionCompleted`，也不生成主任务的 `Prompt`、`TaskCreated` 或 `TurnStarted`。来源会话仅新增会话级辅助模型调用审计记录和用量，标记 `auxiliary_operation = handoff`；来源 task/turn ID 留作溯源，不更新当前任务指针。
- 新会话使用 `parent_thread_id` 与零历史 fork 边界 `forked_from_sequence_no = 0`。这表示关联的普通会话，不是 delegated child；不复制原任务、审批、工具或历史事件。
- 草稿保存在新会话的 `SessionCreated.handoff_draft`，关联源保存在 `handoff_source_thread_id`。创建事务内检查 thread/session ID 冲突，不能通过 upsert 覆盖其他会话。
- 生成时持有来源会话租约，防止另一进程同时推进其历史。取消有独立操作 ID，支持取消先于生成到达；本地取消会通知远端。断网导致取消通知无法送达时，远端辅助生成最迟受 600 秒期限约束，HTTP/IPC 留 610 秒返回错误。这不是主任务时间限制。
- 空历史、活动任务、工作区不匹配、mock provider、输入超出模型窗口均明确拒绝。目标最多 16 KiB，用户编辑后的草稿最多 128 KiB。摘要是有损整理，应检查约束、事实和待办，不能视为完整历史的无损替代。
- 会话绑定不可变的 TUI Driver 拒绝 `/handoff`，与 `/new`、`/resume`、`/fork` 一致。

## 客户端接口

Embedded / HTTP / Unix IPC 共用 `RuntimeTransport::handoff_thread(source_thread_id, request)`，HTTP/IPC 路由为：

```text
POST /threads/{source_thread_id}/handoff
```

沿用 app-server 身份验证、协议版本和 attachment headers。请求为以下带 `action` 的 JSON 之一：

```json
{"action":"prepare","operation_id":"<uuid>","goal":"继续修复 parser","provider":{"provider_profile":"custom","provider_model":"model-id"}}
```

```json
{"action":"cancel","operation_id":"<same uuid>"}
```

```json
{"action":"create","thread_id":"<new uuid>","session_id":"<new uuid>","draft":"用户确认后的文本"}
```

响应分别为 `{"kind":"draft","draft":"..."}`、`{"kind":"cancelled"}` 或 `{"kind":"created","thread":{...}}`。创建重试必须保持目标两个 ID 及草稿一致。provider 覆盖仅接受 `provider_profile`、`provider_model`、`provider_generation_config`；其余执行或权限字段忽略。

## 验证范围

自动化回归覆盖完整摘要及截断重试、旧压缩边界、默认/自定义目标、无工具/无自动发送、创建幂等与原子性、取消先到、中文多行编辑、原生选择模式、错误及过期结果隔离、未发送草稿恢复、小窗口光标、HTTP 认证及路由。

2026-09-22 本地验证：store/client/TUI/app-server 的 lib/bin 回归累计 1,020 项通过，其中 13 项为 handoff 专项；all-targets Clippy、格式和 diff 检查通过。

生成测试使用受控 `LlmProvider`；未使用用户凭据调用真实上游模型，未宣称真实模型摘要质量或不同平台终端实机验收已完成。

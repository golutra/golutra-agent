# Provider 状态与流式失败展示修复

日期：2026-09-16 18:55。仅修改 Agent 仓库，由主 Agent 独立实施。

## 问题与证据

本机故障会话只有一次 provider 请求，实际路由为已配置的真实模型与 Chat Completions 接口。收到完整问候语的文本片段后，同一请求失败；底栏仍显示启动时的 mock 模型。这不是“问候完成后额外发起验证模型请求”。持久化错误只有摘要，无法据其中文“400错误”确认 HTTP 状态、具体无效参数或网络原因。

## 改动

- 认证完成、运行时刷新和状态查询统一更新底栏的默认模型。全局配置刷新同步 profile 列表和默认生成配置；当前会话显式选择的 profile、模型、reasoning 和权限保持优先。
- Chat Completions、Responses 和 Genai 的硬失败也保留可获得的脱敏元数据。`response_http_status` 表示实际传输状态；`http_status` 表示错误分类使用的状态（优先 HTTP 错误状态，否则使用结构化响应状态）。HTTP 200 内的 SSE 400 分别记录为 200 和 400；不从消息文字推断状态。
- ProviderFailed trace 经观测队列和持久化事件携带 `error_metadata`，包含上述状态、provider code、request ID 和 retry-after。仅写诊断白名单，不增加原始请求、凭据或用户正文日志；未知值保持 null。
- 明确的 4xx（429 除外）优先于 stream/connection 等消息关键词，不因关键词进入瞬态重试或 fallback。已有输出后的重放边界保持不变。
- 失败任务显示一张 `Task failed` 卡及已有诊断；同任务的重复完成卡与重复 residual risk 不再铺开。独立风险、未知原因的失败和其他任务失败仍显示。没有实际展示失败步骤的分页窗口仍保留风险证据。
- 所有原始事件、错误摘要、验证结果和任务 Failed 状态保持不变；可见文本不代表 provider 协议成功完成，不将部分回复转换为成功。

## 验证与排障

首轮 LLM 87 项、runtime 193 项、client 423 项通过。TUI 首轮 360 通过、1 个新夹具失败：遗漏 turn_id 导致流式文字未纳入显示；修正夹具后 361 项通过。

真实 macOS PTY + 本地 HTTP/SSE 夹具首次发现额外完成卡仍出现：去重条件比较了显示用的 `Failed`，真实序列化值是 `failed`。改为解析 TaskStatus 枚举，测试也使用真实枚举序列化，定向 PTY 复跑通过。断言同时覆盖中英文部分文本、唯一错误、HTTP 200 / SSE 400、request ID、无多余完成/验证风险卡，以及仅一次 provider 请求。

Clippy 首轮指出新增 HTTP 夹具未处理读取字节数，改为有界读取完整请求头后，四个相关 crate 的 all-targets Clippy（`-D warnings`）通过。最终 TUI 361 项、完整 PTY 16 项、LLM 87 项、runtime 194 项全部通过；client 423 项在首轮通过后未发生生产改动。按最终覆盖统计合计 1,081 项，不累计重复执行。fmt 与 diff 检查通过。

本轮不访问真实云端 provider、不切换用户协议、不修改全局配置或用户数据库；本地夹具通过不表示上游服务的原始错误已经消失。其他操作系统本轮未运行。

## 开发记录

| 日期时间 | 类型 | 摘要 | 原因 | 影响范围 | 关联链接 | 负责人 | 备注 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-09-16 18:55 | fix | 同步 provider 显示并收口失败诊断 | 配置与底栏状态分离，错误元数据丢失，失败重复呈现 | llm/runtime/client/tui | 本文及相应回归测试 | 主 Agent | 事件增加诊断字段，不更改数据库 schema 或 provider 协议 |

本轮按开发规范保留初次失败和修正证据。可复用经验：事件 fixture 应使用实际枚举序列化和真实 turn/task 身份；UI 去重必须按任务归属并考虑分页窗口；读取错误正文中的数字不能代替获取传输状态。

## 完整错误展示补充

Responses 故障暴露了另一个展示问题：`LoopDecided.summary` 是紧凑摘要，可能在真正原因开头就截断；同一事件的 `error` 已保留完整诊断。失败卡现在优先读取非空 `error`，缺失时才退回摘要，既覆盖实时显示，也覆盖历史恢复及 ProviderFailed 不在当前分页内的情况。

展示层去掉重复的 runtime/provider 包装；对于 Genai 的 `Failed to parse stream data … Cause: …`，直接展示实际 Cause。该库也用同一错误类型表示上游 `response.failed`，因此不能仅凭外层文案断言 JSON 解析错误。未知错误文案保留，原始持久化事件、脱敏规则、错误长度安全上限和任务失败语义均不变。

诊断元数据仅在同任务、同 turn 且错误内容匹配时附加。回归覆盖长错误、中文末尾、缺少 ProviderFailed 的历史分页，以及本地 Responses SSE 失败在真实 macOS PTY 中换行后全文可见；已有部分输出不触发自动重试。

补充验证：TUI 单元测试 363/363 通过；真实 macOS PTY 定向验证 2/2 通过（Chat Completions 错误元数据、Responses 长错误全文）；TUI all-targets Clippy（`-D warnings`）、fmt 和 diff 检查通过。首次编译因本机依赖加载长时间等待被中止，重启编译后通过；没有调整测试断言或超时来规避问题。本次未访问真实云端服务。

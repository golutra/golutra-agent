# 后台任务重构验收记录 · 2026-11-16

本轮按照[总体设计](background-tasks.md)实施，主代理与三个子代理并行负责宿主集成、
存储及兼容、运行时投递、工具及浏览器执行。修改保留在工作区，未提交、推送或发布 npm。

## 实现结果

- `shell`、`browser_run` 支持显式后台执行，控制工具支持查询、读取和取消。
- 结果静默持久化；用户新输入优先，在输入准入时刷新持久收件箱。
- 没有用户新输入时，当前有限批次全部结束后可继续原 Agent 循环，不创建新会话、
  不重置预算，不靠反复调用模型等待结果。
- 停止、暂停、接管和批次取消撤销自动继续；真实成功结果不会被事后取消改成失败。
- 后台操作所有权独立于前台会话执行锁；审计、证据、工具产物元数据与结果同事务提交。
- 崩溃恢复不会重放未知副作用；重复回调不会重复审计、证据、验证计数或外部操作。
- 异步截图可以传入模型；恢复时若图片字节不在内存，会明确说明并保留产物引用。

## 自动回归

Rust 工作区拆分执行覆盖全部 crate：除 CLI 外 2,271 项通过，CLI 40 项通过。
最后增加登记中任务的并发可见性回归，再跑完整工具库得到 258 项通过、1 项忽略，
替换此前该目标的 257 项通过。按各目标最终结果合计 2,312 项通过、0 项失败、
11 项忽略。忽略项包含须显式启用的浏览器实测和测试辅助入口；本轮另行运行了下方
两种浏览器实测。

TypeScript SDK 9 项、Python SDK 14 项和 schema smoke 均通过。
最终 `cargo clippy --workspace --all-targets --locked -- -D warnings`、
`cargo fmt --all -- --check` 与 `git diff --check` 均通过。

首次工作区运行有一次 CLI app-server readiness 超时。原测试丢弃启动 stderr，无法追溯
唯一原因；原代码定向与完整 CLI 复跑均通过。排查中发现测试先预占端口再释放的竞态，
已改为服务器直接绑定 `127.0.0.1:0`，并保留退出状态及 stderr。修改后 CLI 全量通过。

真实后台 shell 集成验证了副作用仅一次、保持原任务及轮次、共三个模型请求且没有轮询，
结果在 provider context 持久化后才确认消费。模型部分使用可控测试 provider，
没有据此宣称完成真实远程模型服务的端到端验收。

## 浏览器实测

| 环境 | 结果 | 已观察证据 |
| --- | --- | --- |
| 已安装 Ego 0.6.3.14，官方 launcher/SDK | 通过，1.18 秒 | 导航 1 次、点击副作用 1 次、PNG 显示 `Count: 1`，观察后独立 finish 成功 |
| Chrome for Testing，独立 OpenCLI 2.0.3 daemon + 官方扩展 1.0.27 | 通过，4.33 秒 | 后台打开及点击两个任务，导航 1 次、副作用 1 次、PNG 显示 `Count: 1`，观察后独立 finish 成功 |
| 用户普通 Chrome，现有 OpenCLI daemon | 未通过，约 76 秒超时 | 页面 HTTP 与 favicon 均完整响应，但操作返回结果丢失；后续观察/finish 可执行 |

普通 Chrome 的 daemon 日志显示反复连接，且普通 Chrome 和 Ego 的进程都连接同一个
OpenCLI daemon。共享 contextId 导致互相替换连接是待确认的解释，尚未读取每个连接的
身份来证明。不能把隔离环境通过等同于用户当前 Chrome 环境已经稳定。
未为了绕过该问题关闭用户浏览器、修改扩展身份或重放结果未知的操作。

后续核查更新：已通过连接采样确认 Chrome 和 Ego 交替接入同一个 daemon，并触发相同
contextId 的连接替换。停用 Ego 内的 OpenCLI 扩展后，普通 Chrome 的 61 次采样均在线，
随后后台导航、点击及截图通过。Ego 保持原生 SDK 接入。早期失败保留在上表；后续结果见
[整体验收补记](full-validation-2026-11-16.md#后续普通-chrome-复测与选择入口)及
[浏览器选择增量验收](browser-selection-validation-2026-11-16.md)，未据短时成功宣称长期稳定。

首轮环境检查也确认：全局 `opencli` 是 1.7.2，被固定版本校验拒绝；之后使用
Golutra 管理目录中的 2.0.3。Ego 使用 `~/.local/bin/ego-browser` 官方 launcher，
直接将 app 主程序作为 SDK launcher 的首次检查未进入浏览器操作。

复现隔离测试见[浏览器接入文档](browser-connections.md#后台任务隔离验收)。

## 本机证据

- 回归日志：`/tmp/golutra-background-validation.VpgWom/`。
- Ego manifest、结果及截图：系统临时目录 `golutra-background-browser-live-EplsOZ`。
- 隔离 Chrome manifest、结果及截图：系统临时目录 `golutra-background-browser-live-UtqTLv`。
- 隔离 OpenCLI fixture：系统临时目录 `golutra-opencli-live-ChcpLL`。
- 普通 Chrome 带 HTTP 日志的失败记录：系统临时目录 `golutra-background-browser-live-Ml3EjT`。

这些临时目录可用于本机复核，不是仓库内永久保存的验收附件。

## 运行边界

后台任务可以跨 Agent 前台轮次存活，但仍由宿主进程执行；宿主退出后不会自动重放。
浏览器调用在同一进程内串行，独立 shell 可以并行。跨宿主浏览器 Profile 的任意并发访问
没有获得安全保证。本轮未增加浏览器扩展身份修复或外部常驻任务服务。

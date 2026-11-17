# 浏览器选择与安装增量验收 · 2026-11-16

本轮在已有未提交修改上实现：自动优先 Ego、其次 Chrome；都没有时先确认再安装 Ego；
Agent 通过 `browser_config` 的 `browser_backend` 字段修改会话选择，并读取实际后端。
选择不会改写用户全局配置，活动/未知结果/接管的任务固定原绑定，排队中的后台操作也阻止改选。
使用方法见 [浏览器连接](browser-connections.md)。

## 验证结果

| 范围 | 结果 |
| --- | --- |
| tools 全模块 | 265 通过、1 忽略；包含会话路由、排队限制、取消与未知结果回归 |
| 后续 Ego 状态隔离回归 | 1 通过；未绑定时不返回用户无关的空名称 Space |
| CLI 浏览器进程 | 6 通过：自动优先级、未引导、无浏览器未确认、明确配置、禁用、旧连接兼容 |
| 安装器定向测试 | 2 通过：官方 URL/平台边界、取消并回收子进程 |
| TUI 连接定向测试 | 3 通过：确认与参数入口、输出限量且持续排空、取消并回收进程 |
| tools/CLI/client/TUI | 严格 Clippy 全目标检查、fmt/diff 检查通过；CLI/TUI debug 构建通过 |
| 无配置实机发现 | 独立 Agent home 返回 `browser_backend:auto`、`resolved_backend:ego`、`status:ready` |
| 本机安装检查 | 检出已有 Ego，跳过下载；未替换现有应用 |

测试命令：

```sh
cargo test -p golutra-agent-tools --lib
cargo test -p golutra-agent-tools unbound_status_does_not_include_unrelated_unnamed_user_spaces --lib
cargo test -p golutra-agent-cli --test browser_process
cargo test -p golutra-agent-cli browser::install --bin golutra-agent
cargo test -p golutra-agent-tui browser_connection --bin golutra-agent-tui
cargo clippy -p golutra-agent-tools -p golutra-agent-cli -p golutra-agent-client -p golutra-agent-tui --all-targets -- -D warnings
cargo build -p golutra-agent-cli -p golutra-agent-tui
```

最初路由回归 4 通过/1 失败，揭示转换错误时丢失 `outcome_unknown`；修正后保留后端持久状态。
首次 TUI 编译缺少新操作类型的 Debug，已补齐。实机状态检查另发现旧过滤器用空名称匹配到
无关用户 Space，已收紧为非空名称/非空 ID，并加入回归；重建后的 CLI 状态返回空绑定列表。
保留这些初次失败，不将只跑过的定向测试说成整仓重测。

## 真实浏览器

`background_browser_live` 已改为经过生产 `BrowserRouter`，先用 `browser_config` 设置选择，
检查 `browser_backend/resolved_backend`，再执行受控本地页面导航、点击和截图。
两个后端均验证后台回执先返回、同调用 ID 不重复执行、导航计数和点击效果计数均为 1。

| 后端 | 本轮结果 | 证据目录末段 |
| --- | --- | --- |
| Ego 0.6.3.14 / SDK 指南 2.2.1 | 1.45 秒；截图 `Count: 1`；Space 26 随后确认关闭 | `golutra-background-browser-live-wQkLhQ` |
| 用户普通 Chrome / OpenCLI 2.0.3 | 6.41 秒；截图 `Count: 1`；任务随后确认 unbound | `golutra-background-browser-live-kzQW1W` |

完整证据目录位于 `/var/folders/fv/2smpjc6d2fl4d73rdtdy12q00000gn/T/`，内含 manifest、
summary、工具回执和 PNG。图片已人工查看，每个任务均在查看后单独执行
`background_browser_live_finish`（`GOLUTRA_AGENT_BACKGROUND_LIVE_RELEASE=1`）。
Ego 测试依据已安装的官方 Skill/SDK 指南，执行链路使用 Golutra 适配器；未碰用户其他页面。
两种调用结构不同，单次耗时仅作记录，不能用作性能排名。

## 范围与限制

- 本机 macOS arm64；Windows 安装器仅有代码/平台边界检查，未在 Windows 真机安装。
- 本机已有 Ego，未为测试卸载应用或执行一次全新的网络下载安装；覆盖了未同意不下载、
  已安装跳过、下载来源约束、签名验证路径和取消边界，不将其计为完整安装实测。
- 首次引导、系统许可及 Chrome 官方扩展安装仍需用户处理。
- 新版本使用稳定联合合同，派发前按实际后端再次校验；不会把 Ego 代码交给 Chrome，或忽略跨后端参数。
- Chrome 反复重连的提示已加入帮助、结果与 TUI。短时复测通过，不承诺第三方扩展永久稳定。
- 没有执行 commit、push 或 npm 发布；全局已安装的旧 npm 二进制不会自动获得这些源码修改。

# Hacker News 真实模型浏览器验收

时间：2026-11-11 22:18:46–22:19:28（Asia/Shanghai），约 41.8 秒，包含运行时收尾。

使用本地构建的 `target/debug/golutra-agent exec`，真实 provider 为 GPT-6-Astra / high /
OpenAI Responses。工作区及会话证据隔离，未修改业务源码、全局 provider 配置或用户 Chrome Profile。

## 结果

- Agent 自行通过 `browser_run navigate` 访问 `https://news.ycombinator.com/news`。
- 读取当时排名第一的新闻：Google SWE points 48 AI agents at NASA Lidar, finds unmapped Maya pyramid in 72h。
- 首页记录 ID：`50452455`，标题链接：`https://pietromiotti.com/latest/maya-site/?maya`。
- 使用 `browser_run click` 实际点击 `tr.athing[id="50452455"] .titleline > a`，不是直接导航到文章地址。
- 最终页面地址与标题链接一致，标题为 `My AI Agents Found an Unmapped Maya Site`。
- 截图已人工式视觉核验，包含文章标题、正文与图像。最后文档状态为 `interactive`，没有宣称全部资源加载完成。
- 9 次浏览器工具调用全部成功，CLI 退出码 0；没有使用 shell、curl、搜索等替代网页操作。

调用顺序：help → navigate → snapshot → evaluate（确认第一条）→ click → snapshot → screenshot → evaluate（核对落点）→ finish。

## 启动边界

此次使用带界面的 Chrome for Testing 独立 Profile。验收脚本负责启动浏览器、加载扩展，
并经扩展界面完成测试标签页授权；模型随后独立导航和点击。
这证明已连接后的真实模型操作链可用，不能算作普通 Chrome 从未配置到自动启动/授权的全流程通过。
本轮开始时正式 Chrome 的扩展连接为 disconnected；当前浏览器工具没有独立 launch 动作。

模型 finish 仅释放控制、保留页面。验收脚本结束后关闭自己创建的测试浏览器进程，用户浏览器未被关闭。

## 证据

- 验收脚本：`/tmp/golutra-hn-smoke.v9JiSA/run.mjs`。
- 根目录：`/var/folders/fv/2smpjc6d2fl4d73rdtdy12q00000gn/T/golutra-hn-live-pTjcHO`。
- 根目录内 `agent-events.jsonl`、`agent-stderr.log`、`final.txt`、`final-targets.json`。
- 完整 session/观测：根目录内 `agent-run`；session 为 `01a1caf9-ba33-70e1-8019-431c7f08091d`。
- PNG：`agent-run/state/artifacts/01a1cafa-1b21-72d1-b75b-460c8d84ab64.blob`。

本次只进行验收及记录，未 commit、push 或发布。

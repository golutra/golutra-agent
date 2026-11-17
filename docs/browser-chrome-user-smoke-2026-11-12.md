# 普通 Chrome 原有 Profile 验收

2026-11-12 15:31:26–15:32:45（Asia/Shanghai），约 79.2 秒，不含首次安装、授权及最终导出。
本地 debug CLI，真实 GPT-6-Astra、OpenAI Responses；使用用户日常 Chrome Profile，
没有创建测试 Profile。用户加载扩展后，测试页通过扩展按钮授权，本机状态返回 ready。
首次准备阶段桌面控制曾窗口丢失及超时，恢复后继续；这些不计为模型工具调用。

## 结果

- Golutra Agent 自行导航 Hacker News，观察当时排名第一的新闻。
- 新闻：93% of paint splatters are valid Perl programs (2019)，记录 ID `50459745`。
- 实际点击原始标题链接，进入 `https://www.mcmillen.dev/sigbovik/`。
- 落点标题：93% of Paint Splatters are Valid Perl Programs | Colin McMillen。
- `readyState=complete`，正文长度 2358 字符，标题和正文检查通过；PNG 已视觉核验。
- `browser_finish` 成功释放控制，保留原文页；CLI exit 0，任务 Completed。
- 浏览器工具共 14 次，12 次成功、2 次错误；不能写为全程零错误。

首页有沉浸式翻译插入的重复链接。模型首次使用数字开头的 `#50459745` 选择器，
Chrome 拒绝为非法 CSS；随后 evaluate 被结果未知保护拒绝。模型通过 snapshot 重新观察，
再用 `tr.athing[id="50459745"] span.titleline > a` 点击成功，未重复执行实际副作用。
读取第一行全部链接的 evaluate 还包含投票 URL 的认证查询参数，因此不能宣称返回数据
完全没有敏感字段；报告不复制该参数，后续应只提取任务需要的标题链接。

## 证据与边界

- 根目录：`/tmp/golutra-chrome-user-live.Bclk9A`。
- session：`01a1ceab-2ad2-76a0-b1f8-472d7763bc4a`。
- 观测：`run/observations/sessions/<session>/events.jsonl`；最终回复：`final.txt`。
- PNG：`run/state/artifacts/01a1ceac-38de-76e3-a588-818dbe34b978.blob`。

页面显示原有 Hacker News 登录态，未重新登录、导入 Cookie 或修改账号。
本轮验证的是普通 Profile 中新闻导航、点击、观察、截图与释放，不代表所有网站或 Ego 通过。
安装后自动连接、自动授权新建任务页尚未实现；现有版本仍依赖显式页面授权。
本轮没有 commit、push 或 npm 发布。

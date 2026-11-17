# Ego 浏览器工具

Golutra 通过安装的 Ego 原生启动器操作浏览器。没有 browser.json 时自动优先 Ego，其次 Chrome；
已有明确配置时沿用。Agent 可通过 browser_config 的 browser_backend 字段修改会话选择，
已有任务保持绑定，明确断开后不注册浏览器工具。此功能为源码中的新增能力，
尚未随 npm 发布。

## 安装与配置

推荐使用 `/connect ego` 或 `golutra-agent browser connect ego --launch --wait 120` 启动、自动定位、
验证并保存配置；首次引导和 Chrome 接入详见 [浏览器连接](browser-connections.md)。
下方 JSON 用于自定义启动器/SDK 的高级配置。

先安装你使用的 Ego 浏览器发行版。citrolabs/ego-lite 仓库提供 SDK 和文档，
不包含完整 Chromium 浏览器。单独安装 Node SDK 不能提供原生浏览器接口。
当前上游发行版支持 macOS 和 Windows；自定义发行版需验证对应平台的启动器。
安装后须先完成浏览器的首次启动设置。原生启动器在设置未完成时可能输出
“Please complete the onboarding process first”并以 0 退出；这不代表脚本已执行。
Golutra 同时检查执行回执，不会把这种情况判为成功。

在 Agent home（默认 ~/.golutra-agent，可由 GOLUTRA_AGENT_HOME 指定）创建 browser.json：

~~~json
{
  "executable": "/absolute/path/to/ego-browser"
}
~~~

必须填写支持 nodejs 和 skill 子命令的原生启动器绝对路径，不是应用目录、
Node 可执行文件或 SDK 脚本。路径含空格和中文可以直接填写，Golutra 不经 shell 拼接。
使用自定义 SDK 时可增加 sdk_path 字段，值为 dist/out/index.js 的绝对路径；
两个子命令使用同一路径，避免文档与执行接口不一致。不配置时使用浏览器内置 SDK。
配置中不保存浏览器登录凭据。

只打开应用可以使用 `/connect launch-ego` 或 `golutra-agent browser launch ego`。
沿用 Ego 自己的原有 Profile；任务 Space 只分隔页面，不创建独立的登录存储。
不会复制 Chrome 的登录数据；多账号和首次登录由用户选择、完成。
启动成功不等于 SDK 已就绪，后续仍需检查连接。

浏览器脚本具有 Node 代码执行能力，可访问 Profile 中的已有登录状态。
因此适配层只在显式 unrestricted 模式执行，不宣称对外部浏览器实施了 OS 沙箱：

~~~bash
golutra-agent --yolo
golutra-agent exec --yolo "用浏览器打开目标页面，查看内容并总结"
~~~

源码修改后需重新构建 CLI/TUI；已安装的旧 npm 可执行文件不会自动获得新工具。

## 工具和操作

| 工具 | 用途 |
| --- | --- |
| browser_config | 读取/修改会话 browser_backend（auto/ego/chrome），返回实际后端和待切换状态 |
| browser_launch | 启动/唤起用户原有 Ego；不切换配置、不创建 Space、不自动授权 |
| browser_status | 查看浏览器版本和当前会话绑定；不创建 Space、不抢回控制权 |
| browser_help | 分页读取所配置 SDK 的说明，携带 guide_id 按 next_offset 继续读取；校验完整结束标记 |
| browser_run | 在当前目标的 Space 中执行 JavaScript |
| browser_handoff | 交给用户操作，保留标签页 |
| browser_finish | 成功后原生结束任务，默认保留全部管理页面；可选择结果页或明确清理 |

browser_run 已提供 task、page（默认 p1）和 browser。不要照抄上游独立 CLI 示例
再次创建 Space，也不要直接调用 task.finish() 或 task.handOff()；使用专用工具同步生命周期。
每次调用的 JS 变量重置，标签页标签保留。先读取 SDK 说明，再观察陌生页面：

~~~javascript
await page.goto("https://example.com");
console.log(await page.snapshot());
~~~

已明确的连续操作可以合并；动作后验证实际业务结果：

~~~javascript
await page.fill("loc=css:#name", "Alice");
await page.click("loc=css:#save");
await page.waitForSelector("text=Saved");
console.log(await page.snapshot());
~~~

需要模型查看截图时使用：

~~~javascript
await browser.screenshot(page);
~~~

此帮助方法保存 PNG，并把真实图片字节传入模型请求。直接调用 page.screenshot()
只返回文件路径，不会自动附图。每次最多两张截图、合计 4 MiB。截图按原始图片保存；
文字脱敏不会修改图片。

下载文件须在当前脚本结束前保存，避免 Ego 清理临时文件：

~~~javascript
const pending = page.waitForEvent("download");
await page.click("loc=css:#download");
const download = await pending;
const destination = browser.artifactPath("report.csv");
await download.saveAs(destination);
console.log({ destination });
~~~

artifactPath 只接受文件名，返回本次调用的持久化绝对路径。上传也使用绝对路径；
不要假设浏览器脚本继承了启动 Golutra 的 shell 环境变量或工作目录。

## 会话和恢复

- 工作区与 session 共同决定绑定。恢复原 session 沿用 Space；fork/new session
  创建自己的 Space，不继承对父会话浏览器的控制。
- 同一浏览器目标复用同一 Space，包括成功结束并保留管理页面后的后续消息。同一 session 使用跨进程互斥锁；
  重复的变更调用 ID 被拒绝，不重复执行网页副作用。
- 用户接管后停止操作。用户明确确认交回控制权后，才设置 browser_run 的
  resume_after_user 为 true。运行时不会自动接管或 claim 用户自己的 Space。
- browser_finish 默认使用原生 `finish({keep:"all"})` 结束任务并保留页面；`keep:["p2"]`
  只保留选定结果，`release:true` 或 `keep:[]` 明确清理管理页面。keep 和 release 不可同时传入。
  回执记录实际保留/关闭的标签与是否关闭 Space，不能把缺失回执当成功。
  错误、暂停、超时不会自动 finish。完成后的 Page 标签全部失效；继续同一目标时，工具层只 claim
  本会话经有效结束回执确认保留的 Space，重新 adopt Agent 页面并返回当前 labels/URL。
  首次续接不要传旧 page 标签，使用注入的 page；后续可按返回标签选页。用户或 unknown 来源页面不自动 adopt。
  新目标显式传 `browser_run {"new_goal":true,"code":"..."}`，仅允许在 idle/finished 时切换；
  原 Space 不存在时明确报错，不静默创建替代品。旧状态缺少保留回执事实时不自动 claim。
  本契约使用 Ego 原生结束能力，不沿用 OpenCLI 的闲置约 10 分钟规则。
- 超时、取消、连接中断可能发生在网页副作用之后，结果标记 outcome_unknown。
  必须先调用 `browser_run {"observation":"snapshot"}` 或 `{"observation":"screenshot"}`，
  不附带 code。若 `requires_reload=false`，观察成功后可以继续；若可能存在仍在执行的页面 JS，
  即使观察成功也保持 outcome_unknown。显式使用 `recovery:"reload"` 配合固定观察，重载全部受管页面后再观察，
  才能解除保护；这可能丢失未保存的编辑，不会撤回已经发生的服务器副作用，也不可据此重放原动作。
  `executionStopped:false`、`mayHaveLateEffects:true`、缺少明确停止证明的 evaluate 超时及启动器中断均保留该保护。
  观察不会新建目标或绕过用户控制；中断的已授权 claim 可在原 Space 中恢复。不根据最后一个定位错误推断整段脚本没有副作用。
- 暂停状态在启动 SDK 前拦截，只有用户确认后的 `resume_after_user:true` 可以恢复。
  原生 INACTIVE 单独标记 ended，不尝试 takeOver；需要新的浏览器目标时使用新 session。
- 已绑定的 Space 被删除时报告缺失，不用新的空白 Space 伪装恢复成功。
- Space 隔离任务标签页，不隔离 Profile 的账号、Cookie 或存储。本实现使用默认
  Profile，不扫描或自动挑选其他账号。

固定快照默认只有当前视口。页面下方内容使用 `{"observation":"snapshot","scope":"full_page"}`；
iframe 子树使用 `{"observation":"snapshot","scope":"subtree","root":"@12"}`，root 必须来自当前快照。
重载后 refs 失效，不能把 subtree 与 recovery:reload 合用。普通脚本也可使用官方 snapshot 参数。

browser_help 每次从安装版本执行 `ego-browser skill`；缺失官方结束标记、底层截断或执行异常均报错。
后续分页必须携带第一页的 guide_id；内容变化（例如浏览器升级）时拒绝混读，重新从 offset=0 开始。
官方独立 CLI 默认关闭页面，Golutra 仍按产品约定默认保留；脚本不直接管理 Space 生命周期。

绑定、调用回执和下载路径位于 Agent home 的 state/browser/workspace-hash/session-id
子目录。恢复数据不匹配时明确报错。模型可见快照有长度预算，完整输出和截图进入
现有 artifact/evidence 链路。缺失或错误的浏览器启动器在工具调用时报告。

## 验证

自动化测试使用显式 SDK 协议替身，验证恢复、fork、互斥、重复调用、接管、取消、
超时、图片产物和配置边界。Provider 测试捕获本地 HTTP 请求，验证各协议的原生图片块。
这些测试不代表真实浏览器已通过验收。

### 2026-11-16 官方 skill 对齐与新版实机验收

依据本机 Ego 0.6.3.14 的 `ego-browser skill` v2.2.1 补齐续接与恢复：
完成保留后按原 Space claim/adopt，新目标显式切换；保留跨进程的 claim 进度与迟到副作用保护；
帮助要求结束标记、分页指纹一致；固定观察支持视口/全页/iframe 子树。
用户主动接管仍需明确交回控制权；unknown 来源标签不自动 adopt 或关闭。

工具模块全量 241 通过、0 失败、1 个既有协议压力测试忽略；CLI 连接进程 5 项通过。
tools/CLI/TUI 全目标严格 Clippy、格式/脚本语法检查和本地 CLI/TUI 构建通过。
新增 8 项回归覆盖完成续接/过期标签、用户标签保护、缺失 Space/接管、迟到副作用、
明确停止后的观察恢复、失败重载/交回控制权、claim 中断和帮助截断/版本变化及参数组合。
这些故障注入是协议回归，不宣称已在实机强制触发真实 evaluate 超时。

新版原生实机最终验收通过：15 次工具调用全部 ok，8.83 秒（不含编译）。
覆盖完整帮助分页、表单/PNG、运行时重建、iframe 子树、popup、上传下载内容、
Shadow DOM、富文本原生粘贴、prompt 对话框、全页快照、handoff/takeover、
原生 finish 后再次重建运行时并 claim/adopt，原 Space 18 与中文表单内容保持一致。
最终再次 finish，结果页仍保留；截图已视觉核验。标签必须重新获取，字符串可能被 SDK 重用。
证据：`/var/folders/fv/2smpjc6d2fl4d73rdtdy12q00000gn/T/golutra-ego-acceptance-QxBInD`。
此前同轮实机 `L424II` 为 9.46 秒通过；最终轮增加 iframe 子树验证。
本地服务随测试结束停止，保留页面可查看，刷新需重跑测试。默认 Chrome 选择未改；未发布 npm。

### 2026-11-12 生命周期优化

对照上游 SDK 提交 `bacaece` 的 `TaskSpace.finish`：支持 keep=all 与完整结束回执。
本次新增默认保留、指定页面保留、显式清理、缺失回执不报成功、已结束任务不接管、
取消前不启动及固定观察恢复回归。协议替身覆盖实际桥接脚本和状态落盘，不计为实机验证。

18:52 检查时本机 `/Users/skyseek/.local/bin/ego-browser` 为失效链接，其 active_version_dir 指向已删除的
`/private/tmp/golutra-ego-live.2KPBzJ/ego lite.app`；Applications 与 Spotlight 未找到可用 Ego。
该阶段未作实机通过结论，也未切换当前 Chrome 配置；19:04 安装后实测结果见下。

工具模块全量首轮 230 通过、2 失败、1 忽略：连续 Ego 调用偶发 browser_busy。
用共享文件描述的确定性回归复现，确认仅关闭父进程 File 不能保证 flock 立即释放；
StateLease Drop 改为显式 unlock，保持调用内互斥并避免被无关子进程的临时继承延长。
修复后工具全量 233 通过、0 失败、1 既有压力测试忽略；CLI 连接 5 项通过。
tools/CLI/TUI 全目标严格 Clippy、格式与脚本语法检查、本地 CLI/TUI 编译通过。
这个公共租约修复同时适用于 Chrome；没有通过增加重试或削弱互斥掩盖失败。

### 2026-11-12 19:04 安装及实机验收

用户授权命令行安装后，从官方仓库 README 链接下载 Apple Silicon 0.6.2.10：
`https://cdn.ego.app/setup/macos/arm64/egolite-Y7MbxKIuhzFB.dmg`。
下载 149520840 字节，SHA-256 与官方响应头一致：
`989b1fad1cefb9654d736ac44a15b41a9acc4dee7633bba6961ace08525f1d27`。
下载中途停滞，断点续传完成；原 DMG 内 FinderInfo 导致 strict 签名检查失败，
用 ditto --norsrc --noextattr 暂存复制后，codesign --deep --strict 与 spctl 均通过。
签名为 CITRO LABS PTE. LIMITED（JGQLC6YQYJ），Notarized Developer ID。

安装位置 `/Applications/ego lite.app`，约 445 MiB。应用启动后自动修复 `~/.local/bin/ego-browser`
及 active_version_dir 到正式安装位置。原生脚本返回版本 0.6.2.10 与 ready 标记；未修改默认 Chrome 选择。

真实 ToolRuntime → Ego 原生浏览器验收 1 通过、0 失败，耗时 7.84 秒（不含编译）。
7 次工具调用全部 ok：状态、中文表单与 PNG、重建运行时恢复、iframe、popup、上传、下载内容校验、
handoff/takeover、原生 finish、结束后状态。完成回执 closedSpace=false，保留 p1/p2，
没有关闭管理页面；最后 phase=finished，Space 1 仍存在。截图已视觉核验。
证据目录：`/var/folders/fv/2smpjc6d2fl4d73rdtdy12q00000gn/T/golutra-ego-acceptance-at0iT3`。
测试页面已保留供用户查看，本地 HTTP 服务在验收结束后关闭。未 commit/push/npm。

真实浏览器验收须显式指定原生启动器：

~~~bash
GOLUTRA_AGENT_EGO_LIVE_BINARY="/absolute/path/to/ego-browser" \
  cargo test -p golutra-agent-tools --test ego_browser_live \
  -- --ignored --nocapture
~~~

自定义 SDK 可同时设置 GOLUTRA_AGENT_EGO_LIVE_SDK。测试使用临时目录和本地 HTTP 页面，
检查表单、截图、运行时重建后继续原 Space、iframe、popup、上传、下载、
handoff/takeover 和 finish，不访问用户业务页面或发送外部消息。
默认测试中此项标记为 ignored，不能计入真实浏览器通过数。
当前实机夹具默认保留结果 Space 供检查；测试完成后本地 HTTP 服务关闭，已加载页面仍可查看，
刷新需要重新运行测试；这些测试结果 Space 可由用户在 Ego 中手动关闭。
每次显式验收都会保留临时目录，控制台打印 acceptance evidence 路径；
各次工具结果写入 acceptance-调用ID.json，失败时也保留回执和已产生的截图。

### 2026-11-10 实机状态

首次尝试被 Ego 首次设置拦截：0 通过、1 失败，失败证据保留。
用户完成设置后，使用已安装的 /Users/skyseek/.local/bin/ego-browser
（Ego 0.6.2.9）通过真实 ToolRuntime 重跑：1 通过、0 失败，耗时 6.59 秒。

通过项目包括中文表单输入与结果验证、PNG 截图、运行时重建后恢复原 Space
及页面内容、iframe 点击、popup、文件上传、下载内容校验、handoff/takeover
和 finish。6 次工具调用均为 ok，Space 1 保持不变，最后为 finished；截图已人工检查。
本轮证据位于系统临时目录的 golutra-ego-acceptance-D6Hyzz，保留调用结果、
原生回执、截图和下载文件。

这证明当前版本可通过本地真实网页的基础验收，不代表任意第三方网站、登录验证、
验证码、复杂富文本或用户另外打包的浏览器均已验证；也不代表 npm 已发布该能力。

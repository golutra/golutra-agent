# Chrome / OpenCLI 与 Ego 原生接入

自动模式优先使用已安装的 Ego，没有 Ego 时使用 Chrome。Chrome 通过 OpenCLI 2.0.3
和官方扩展控制；Ego 使用原生 SDK。显式选择和禁用状态优先于自动发现。
两者共用 `browser_config/launch/status/help/run/handoff/finish`，会话、回执和图片由 Golutra 记录。
Agent 可以修改会话内的浏览器选择；执行失败不会自动切换浏览器或重放操作。

本次修改尚未发布 npm；使用本次构建的 CLI/TUI。历史自研 Chrome 扩展不再是默认后端，
旧 Native Messaging 入口暂留兼容，原实现与证据见 [历史记录](browser-native-chrome-history.md)。

## TUI 连接显示与诊断

`/connect` 沿用已保存的选择；`/connect auto` 才明确恢复自动选择。
连接成功后只显示最终状态，例如固定选择 Chrome 时：

```text
✓ 已连接 Chrome
沿用现有浏览器，保留登录状态。
浏览器选择：固定使用 Chrome · 已保存
```

自动模式会显示「自动（优先 Ego，其次 Chrome）」以及实际连接的浏览器。
`/connect status` 重新检查状态，不保存设置，因此不会显示「已保存」。
启动请求、安装完成与连接成功分别展示，只有探测结果为 `ready` 才显示已连接。
缺少扩展、连接组件、Ego 首次引导或 Profile 选择时显示对应操作；连接失败不覆盖原有选择。

连接不会自动改变执行权限。仅当当前会话尚未授权网页操作时，提示
`/permissions unrestricted`；该命令开放整个会话的执行权限，并非单独的浏览器授权。
已处于 unrestricted 时不重复提示，也不要求用户重新启动加 `--yolo`。

`/connect details` 查看最近一次已完成操作的原始诊断，包括程序路径、版本、探测结果及错误。
它只读缓存，不重新启动、探测浏览器或修改设置。默认成功输出不回显中间安装提示、工具调用说明或 JSON。
`/connect help` 显示用户命令。连接期间开始任务后，仍可使用 help、details、cancel；
改变浏览器配置仍要求本地空闲会话。取消后的迟到安装确认会被丢弃，已完成的操作按实际结果显示。

TUI 使用同目录 CLI 的 `golutra-agent browser --json <子命令>`，stdout 只输出一个版本为 1 的结果对象。
结果包含 `ok/state/backend/selection/saved/timed_out/diagnostics`；非零退出不能被解析成成功，
非法、截断或版本不匹配的输出也不能触发安装确认。`ok` 表示命令是否完成；例如 status 成功检查到
`setup_required` 时仍可为 true，网页连接是否就绪须看 state。下载进度保留在 stderr 诊断中。
已有 `golutra-agent browser status` 和 `browser chrome install` 的默认 JSON 输出保持兼容。

2026-11-16 本次显示层验证：CLI/TUI 全部 test targets 共 613 项通过、0 失败、2 项显式忽略
（外部 verifier 辅助入口、需凭据的 live provider 测试）。8 项真实 CLI 进程测试中，结构化输出
直接经过 TUI 消费端解析；9 项连接单元测试覆盖权限、选择、非法输出、安装确认和取消竞争。
另用真实 PTY 检查 guarded/unrestricted 两种显示、显式诊断与正常退出后的终端恢复。
PTY 使用隔离配置和 OpenCLI 协议测试程序，不接触真实浏览器；本次没有重测网页操作或下载安装。
原始日志与 PTY 字节保存在 `/tmp/golutra-connect-ux.nti4hD/`（`tests-final.log`、`pty-evidence.json`）。

## 自动选择与 Agent 可修改字段

无 `browser.json` 或配置为 `{"backend":"auto"}` 时自动发现。`/connect` 按已有配置启动并检查；
`/connect auto`（CLI：`golutra-agent browser connect auto --launch --wait 120`）重新启用自动模式。
已安装 Ego 但启动器/首次引导未就绪时返回 `setup_required`，不会悄悄改用 Chrome。
两者都没有时返回 `install_required`，先询问用户是否安装 Ego。

Agent 调用 `browser_config({})` 查看选择，调用以下任一形式修改当前工作区、session 的偏好：

```json
{"browser_backend":"auto"}
```

`browser_backend` 也接受 `ego`、`chrome`。字段通过 `browser_config` 工具传递，
不覆盖用户的全局 `browser.json`，不允许更改可执行路径、Profile 或其他 session。
CLI 的 `browser status` 显示全局默认；Agent 的工具结果显示所在 session 的实际选择。

| 返回字段 | 含义 |
| --- | --- |
| `browser_backend` | 偏好：`auto`、`ego`、`chrome`；用户禁用时为只读 `disabled` |
| `resolved_backend` | 当前调用使用的后端；尚未就绪/发现时可为 `null` |
| `active_backend` | 已绑定浏览器任务的后端 |
| `next_backend` | 没有活动绑定时，偏好会选择的后端 |
| `switch_pending` | 选择已改变，但当前任务仍保留原绑定 |
| `selection_scope` | 会话覆盖 `session` 或全局默认 `host_default` |

选择在开始任务前立即生效，工具合同无需等到下一轮对话重新加载。
Chrome 的 `browser_run` 使用 `action`，Ego 使用 `code` 或固定 `observation`；切换后先读 `browser_help`。
发送另一后端的参数会拒绝，不能默默忽略。

进行中的前台/后台调用持有同一会话锁；此时修改选择返回 `browser_busy`。
有活动、接管或未知结果的绑定时，修改偏好只更新下一选择。需要切换时先完成当前任务，
根据实际任务意图调用 `browser_finish({"release":true})`；释放可能清理任务页，先保存结果。
Ego 已确认结束且保留的结果会继续原 Space，除非 Agent 明确重新选择；重新选择不删除保留页面。
旧版本已存在的任务绑定会被继承；两个后端都有未结束的旧任务时要求先处理冲突。
用户已禁用浏览器工具时，Agent 不能通过修改字段恢复权限，需用户 `/connect auto/ego/chrome`。

## 两者都没有时安装 Ego

TUI `/connect` 检测到两者都没有，或执行 `/connect install-ego` 时，先显示安装确认。
用户输入 `/connect confirm-install-ego` 后才下载；`/connect cancel` 取消。
CLI `golutra-agent browser install-ego` 在交互终端询问；非交互调用返回
`install_confirmation_required`，不联网下载。Agent 已取得用户同意后可执行：

```bash
golutra-agent browser install-ego --yes
```

安装器使用官网提供的 HTTPS CDN：macOS arm64/x64 DMG、Windows x64 EXE。
macOS 只读挂载，复制到暂存目录，通过 codesign 和 Gatekeeper 后才移动并启动；
Windows 校验 Authenticode 后运行官方安装器，系统提示/许可需用户处理。
已有 Ego 时跳过安装，不覆盖应用、不修改系统安全设置。Linux 等未确认平台给出明确说明。
下载有大小和时间上限，安装可取消；macOS 正常取消会终止受管子进程并卸载安装卷，
卸载失败保留目录并报告。Windows 强制终止后需检查安装窗口和临时文件。
安装后首次引导仍需用户完成；Agent 可在同一轮重新检查 `browser_status`。

宿主可用 `GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR` 限定应用发现目录；macOS 自动安装也使用该目录。
这是非标准安装/隔离验收入口，不是 Agent 工具参数。

## Chrome 反复重连

Ego 不需要 OpenCLI 扩展。若同时在 Chrome 和 Ego 启用 OpenCLI，出现持续重连，
先停用 **Ego 中的 OpenCLI 扩展**，保留普通 Chrome 中的扩展，再检查 Chrome 连接。
本机曾观察到两边共用相同 contextId，新连接顶掉旧连接，导致约每 1–1.5 秒重连。
提示已加入工具结果、帮助和连接入口；不会擅自修改用户扩展或通过切换账号掩盖冲突。

## Chrome 安装

npm 包已声明固定依赖 `@jackwener/opencli@2.0.3`，需要 Node.js >= 20.18.1。
发布后通过 npm 安装 Golutra 会同时取得该依赖，启动器显式传入 OpenCLI 和 Node 路径。
缺失浏览器依赖不会阻止普通编码任务；使用浏览器时返回安装指引。

独立原生安装可先安装：

```bash
npm install -g @jackwener/opencli@2.0.3
golutra-agent browser chrome install
```

第二条命令显示官方扩展入口与本机 OpenCLI 路径，不再导出 Golutra 扩展。
加载 [OpenCLI 官方扩展](https://chromewebstore.google.com/detail/opencli/ildkmabpimmkaediidaifkhjpohdnifk) 后：

```bash
golutra-agent browser connect chrome --launch --wait 120
golutra-agent browser status
```

TUI 对应 `/connect chrome` 和 `/connect status`。
常规任务直接新建 OpenCLI 任务页，无需逐页授权；首次安装官方扩展仍通过 Chrome 完成。
原 Golutra 扩展不等于 OpenCLI 扩展，不能靠重载原扩展完成迁移。

需要指定路径或多个 Chrome Profile 时：

```bash
golutra-agent browser connect chrome --executable /absolute/path/to/opencli/dist/src/main.js --profile work
```

`--profile` 是 OpenCLI alias 或 contextId。首次连接选定后，Golutra 会固定任务的实际 contextId，
不因用户切换 OpenCLI 默认账号而把已运行任务路由到另一账号。没有唯一 Profile 或默认值时要求选择。
Windows 建议传 `dist/src/main.js`，不执行模型提供的 shell shim。

可通过宿主环境 `GOLUTRA_AGENT_OPENCLI_EXECUTABLE` 和 `GOLUTRA_AGENT_OPENCLI_NODE` 指定固定程序。
发现顺序：配置显式路径、宿主环境、用户目录内的固定版安装、PATH。仅接受绝对路径。
本机开发安装已放在 `~/.golutra-agent/browser/opencli`，不覆盖已有全局 1.7.2。

## Ego

通过上述安装命令或下载 [Ego](https://lite.ego.app/zh-cn)，完成首次引导后：

```bash
golutra-agent browser connect ego --launch --wait 120
```

或在 TUI 使用 `/connect install-ego`、`/connect ego`。Ego 不经过 OpenCLI，
继续使用 `ego-browser nodejs` 与原生 SDK。高级配置见 [Ego 工具说明](ego-browser.md)。
浏览器本身保留登录态，两个后端之间不复制 Cookie、token 或账号数据。
Ego 的 `browser_finish` 默认用 SDK keep=all 原生结束并保留结果页；显式 keep 标签或 release:true 控制清理。
继续同一目标时接回本会话已确认保留的 Space 并重新取得页面标签；新目标在 finish 后传 new_goal:true。
原生暂停仍需用户明确交回控制权。可能仍在执行的页面脚本需显式重载恢复，单次快照不会解除保护。
这是 Ego 原生结束能力，与下方 Chrome 暂停并暂时保留任务页的行为不同。
2026-11-12 19:04 已通过命令行安装官方 Ego 0.6.2.10 到 `/Applications/ego lite.app`，
原生实机验收 7 次调用全成功，结束后 p1/p2 保留；详情和证据见 Ego 工具说明。当时默认选择为 Chrome；本轮自动策略已调整。

## 会话与动作

Golutra 为每个工作区、session 和任务代次生成独立 OpenCLI session。
模型只能提供结构化动作，不能覆写程序、Profile 或 session，也不能传任意 CLI 命令。
参数通过 argv 传递，并用 `--` 隔开文本与选项，避免输入内容变成路由参数。

- `open` 创建前台任务；`navigate` 操作当前任务，明确释放后才能再次 open。
- `snapshot/frames/screenshot` 观察页面；PNG 经校验后进入既有图片链路。
- `click/hover/fill/type/select/press/scroll/back` 执行交互。`click/hover` 可用 `nth` 选择已观察到的匹配列表中的第几项（从 0 开始），不能猜测。
- `wait` 等待 selector、text 或 download；下载由网页动作触发，不声明原生同源下载接口仍存在。
- `evaluate` 执行页面 JavaScript，可传观察到的 `frame_index`；其他动作不能偷偷忽略 frame_index。
- `upload` 仅接收绝对文件路径。所有浏览器操作仍要求显式 unrestricted 权限。

`browser_status` 检查连接，不创建任务页。未开始任务直接 snapshot 会拒绝。
`browser_handoff` 暂停 Golutra 自动操作，页面保留；OpenCLI 调试连接可能继续存在。
只有用户确认后，才能用 `resume_after_user:true` 恢复。
`browser_finish` 默认在前台保留原结果页并暂停自动操作，不重开 URL，保留当前表单/页面状态。
这不等于解除调试连接或永久保留：OpenCLI 会在闲置约 10 分钟后回收任务页，交付前仍需保存截图和结果。
后续只有用户确认后才能用 `resume_after_user:true` 继续；明确清理时使用
`browser_finish({"release":true})`，通过有回执的 unbind 释放当前租用，可能回收/关闭任务页。
不会关闭其他会话或用户无关标签页。

取消会结束本次 CLI 子进程，但已经发出的浏览器动作可能继续。
重复 Golutra call ID 不再执行；输出异常、取消或未知结果会阻止后续写动作，必须先观察。
已核对 OpenCLI 2.0.3 中 `click/hover` 的目标解析错误发生在事件派发前：选择器不合法、无匹配、
匹配不唯一、序号越界及陈旧引用会返回 `action_dispatched:false`，可直接修正目标。
同名错误若来自任意 `evaluate` 不享受该豁免；仍不自动重试。
OpenCLI 自身仍有传输重试和命令日志，不能把它宣传成跨崩溃的严格 exactly-once。
`/connect disconnect` 禁用后续任务的浏览器工具；正在运行的任务应先取消。

旧 `{"backend":"chrome","connection_dir":"…"}` 配置会读作 OpenCLI Chrome，
不连接旧自研桥接。成功保存时去掉旧路径；Ego、disabled 与失败时的原配置保留。
旧桥接的运行中任务不会自动转移为 OpenCLI 任务。

## 验证

本轮真实链路为 Golutra ToolRuntime → OpenCLI npm 2.0.3 → 上游扩展 → Chrome for Testing。
独立端口/Profile/缓存，不修改用户 HOME，也不占用普通 OpenCLI daemon。
实际覆盖中文表单、参数字面值、点击、快照、PNG、暂停恢复、两会话并行隔离和释放。
两轮通过分别耗时 12.17 秒和 8.30 秒；这些数字仅为本地验收，不是与旧实现的同条件性能对比。

证据：系统临时目录 `golutra-opencli-live-hDPesD`、
`golutra-opencli-acceptance-Ev1dxW`；最终一轮为 `golutra-opencli-live-hS3UK9`、
`golutra-opencli-acceptance-LP8esd`。最终 PNG 已人工视觉检查，中文及参数字面值显示正确。
没有把协议替身计为真实浏览器测试。

回归结果：client 451 项、CLI 进程 5 项、浏览器工具 26 项（另 1 项既有压力测试忽略）、
TUI 连接 1 项、Node 10 项、npm 包装 Python 9 项通过；最终 OpenCLI 定向 5 项复测通过。
tools/CLI/client/TUI 严格 Clippy、格式检查和 CLI/TUI 编译通过。

17:01 检查时本机普通 Chrome 的状态为 `setup_required`：受管 OpenCLI 2.0.3 daemon 已运行，
但 `extensionConnected:false` 且 Profile 列表为空。需先安装上文官方扩展，再使用本次本地构建：

```bash
./target/debug/golutra-agent browser connect chrome --launch --wait 120
./target/debug/golutra-agent browser status
```

17:17–17:19 用户安装官方扩展后再次验收：首次仍未连接，重启受管 2.0.3 daemon 并启动普通
Chrome 后 ready，连接配置保存成功。Golutra Agent 使用现有 Profile 打开 Hacker News，
实际点击当时首条新闻「Humble Bundle just killed most of their security through vibecoding」，
进入 SpaceBattles 原文；短暂 Cloudflare 检查后正文加载，readyState=complete，文本 10036 字符。
截图已视觉核验，finish 成功，最终 status 仍 ready。

该次模型任务约 115.0 秒，15 次浏览器工具调用中 13 次成功、2 次错误并恢复：
首次点击选择器匹配 30 项，随后 evaluate 被未知结果保护拒绝；snapshot 后改用唯一行选择器成功。
任务 Completed / verification Pass，CLI exit 0；不宣称全程零错误。
首页快照被导航区及翻译扩展内容占据，模型改为定向读取标题链接；快照包含页面已有账号链接，
因此不把模型的“未读取账号”表述当作完整隐私保证。启动提示仍有旧 Golutra 扩展授权措辞，需另行收口。

证据根目录 `/tmp/golutra-opencli-user-smoke.ygERz1`，session `01a1cf0c-14d7-7683-a9eb-a67505cef648`；
PNG 为 `run/state/artifacts/01a1cf0d-9ef3-7102-9a05-44b664dfec77.blob`。
本轮没有新的 Ego 实机验收；Windows/Linux、复杂网站与长时间运行仍需分别验证。

18:40 收尾优化验证：默认 finish 保留原页并暂停，只有 `release:true` 回收；固定前台窗口模式，
不受 `OPENCLI_WINDOW` 继承值影响；统一 CLI/TUI OpenCLI 提示。浏览器回归 28 项通过、1 项既有压力测试忽略，
CLI 5 项、TUI 连接 1 项通过，严格 Clippy 和本地 CLI/TUI 编译通过。
真实隔离 Chrome 夹具 `golutra-opencli-live-Uw3UHl` / `golutra-opencli-acceptance-6fjxxZ` 用时 8.49 秒，
确认歧义点击未生效后可直接纠正、nth 选中首项、默认 finish 后原表单和结果仍在、暂停阻止后续操作、
确认恢复及显式清理均正常，PNG 已视觉核验。

普通 Chrome 模型交付验收 `/tmp/golutra-opencli-retain-smoke.cU3znQ`，
session `01a1cf59-03f8-7b52-9525-ce864c49af4a`：打开 HN、观察、截图、默认 finish，
5 次工具调用全成功、约 27.9 秒、Completed/Pass。回执 page_retained=true、lease_released=false，
PNG 已视觉核验；任务页保留供用户查看。任务比此前点击新闻测试更短，不能据此宣称性能提升。
仍受上游约 10 分钟闲置回收限制；无永久保留或 Ego 新实机验收结论。未 commit/push/npm。

```bash
cargo test -p golutra-agent-tools --lib browser
cargo test -p golutra-agent-cli --test browser_process
GOLUTRA_AGENT_CHROME_TEST_BINARY=/path/to/chrome-for-testing \
GOLUTRA_AGENT_OPENCLI_PACKAGE=/path/to/node_modules/@jackwener/opencli \
GOLUTRA_AGENT_OPENCLI_EXTENSION=/path/to/opencli/extension \
node extensions/opencli/live-test.mjs
```

测试扩展目录须包含上游编译好的 `dist/background.js`。
本轮检查上游源码提交 `9bc5f5c`；[项目](https://github.com/jackwener/opencli)。

### 后台任务隔离验收

在仓库根目录运行以下命令。使用 OpenCLI npm 2.0.3，以及该版本
[官方发布的扩展 1.0.27](https://github.com/jackwener/OpenCLI/releases/download/v2.0.3/opencli-extension-v1.0.27.zip)
解压目录（包含 `manifest.json` 和 `dist/background.js`）：

```bash
GOLUTRA_AGENT_CHROME_TEST_BINARY='/absolute/path/to/chrome-for-testing' \
GOLUTRA_AGENT_OPENCLI_PACKAGE='/absolute/path/to/node_modules/@jackwener/opencli' \
GOLUTRA_AGENT_OPENCLI_EXTENSION='/absolute/path/to/unpacked-opencli-extension-1.0.27' \
GOLUTRA_AGENT_OPENCLI_BACKGROUND_ACCEPTANCE=1 \
node extensions/opencli/live-test.mjs
```

脚本在新的临时目录复制扩展和 OpenCLI，使用独立端口、Profile 和本地测试页。
验收检查后台回执先返回、导航和按钮操作各执行一次、重复 call ID 不重放，以及 PNG 结果。
终端打印 `Finish manifest after inspection` 后，检查该 manifest 同目录下的
`summary.json`、`completed-*.json` 和 `artifact-*.png`；失败诊断可查看
`fixture.json` 与 `http-*.json`。脚本保持测试浏览器运行，检查完成后在原终端输入
`finish` 并回车，才会运行该任务的独立结束测试并关闭隔离测试进程。
这个人工检查流程要求保持标准输入打开。

若检查后应移除测试任务页，在运行命令时同时设置
`GOLUTRA_AGENT_BACKGROUND_LIVE_RELEASE=1`；结束调用会显式使用 `release:true`。
继续同一个 Ego 验收目标时，可以给 `ego_background_browser_live` 设置
`GOLUTRA_AGENT_BACKGROUND_LIVE_REUSE_MANIFEST=/absolute/path/to/manifest.json`，
复用先前已结束并保留的 Space。先备份原验收目录：复测沿用同一工作区和 session，
为新一轮生成 turn，并更新原目录中的 manifest、summary 和 HTTP 记录。

2026-11-16 普通 Chrome 后台验收复测仍在约 76 秒超时；证据目录
`golutra-background-browser-live-Ml3EjT` 中，首页与 favicon 的 HTTP 请求均已完整响应，
已排除测试服务等待开闸造成的阻塞。同期观察到 Chrome 与 Ego 同时连接普通 OpenCLI daemon，
扩展连接反复重建；“共享 contextId 导致连接相互替换”仍是假设，需要逐连接确认身份。
隔离环境通过也不能替代这一普通浏览器环境问题的验证。

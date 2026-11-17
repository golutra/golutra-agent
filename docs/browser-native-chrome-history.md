# 原生 Chrome 桥接历史验收

以下内容记录迁移 OpenCLI 前的实现与测试，不是当前安装指南。当前入口见 [浏览器连接](browser-connections.md)。

# 连接 Ego 与 Chrome

未配置浏览器时默认提供 Chrome 工具，不自动安装浏览器。已明确选择的 Ego 或 Chrome
配置优先；明确禁用后不注册浏览器工具。Chrome 连接不可用时，返回连接指引并推荐
下载 [Ego](https://lite.ego.app/)（`/connect install-ego`），由用户完成设置后选择
`/connect ego`，不会自动切换或重放结果不明的网页操作。
本功能已在源码实现，尚未发布到 npm；请使用同一次构建的 CLI/TUI。

## 打开用户原有浏览器

TUI 使用 `/connect launch` 打开当前所选浏览器（未配置时为 Chrome）；
`/connect launch-chrome`、`/connect launch-ego` 打开指定应用。
CLI 对应：

```bash
golutra-agent browser launch
golutra-agent browser launch chrome
golutra-agent browser launch ego
```

启动沿用该浏览器原有 Profile，不创建测试 Profile，不开启隐身或远程调试端口，
也不复制 Cookie/token。多账号时由用户选择账号所在的窗口；只有仍有效的登录状态
才能复用。Chrome 与 Ego 各自使用自己的登录数据，不在两个浏览器之间迁移账号。
Ego 的任务 Space 分隔页面，并非另建 Profile。

`launch` 只启动/唤起应用，不切换配置，不导航、不自动授权；`launch_requested`
仅表示系统收到启动请求。随后用 `browser status` 检查连接，Chrome 首次仍须扩展授权。
任务中的 `browser_launch` 工具使用同一路径，并拒绝重放相同调用。
取消任务不会关闭用户浏览器；启动结果不明时先观察，不自动重试。

非标准位置可用 `browser launch ego --application /absolute/path/to/application`
指定应用（macOS 必须是 `.app`）；它与 Ego 的 `--executable` SDK 启动器参数不同。
明确禁用后，无参数 launch 会拒绝启动；显式 launch chrome/ego 不会解除禁用。
默认发现支持 macOS 正式版应用及 Windows 常见安装目录，Linux 仅发现 Chrome。
本轮只在 macOS 实机验证，Windows/Linux 仍需平台验收。

## Ego：自动检测与配置

在 TUI 输入 `/connect` 查看入口，或直接输入 `/connect ego`。CLI 等价命令：

```bash
golutra-agent browser connect ego --launch --wait 120
golutra-agent browser status
```

连接器从 PATH 和用户的 `.local/bin` 寻找 Ego 启动器，检查原生版本回执，
成功才写入 Agent home 的 `browser.json`。失败、取消或未完成引导不覆盖旧配置。
已有自定义启动器可用 `--executable /absolute/path/to/ego-browser` 指定；
自定义 SDK 的高级配置见 [Ego 工具说明](ego-browser.md)。
再次连接时保留已配置的启动器和 SDK 路径；等待期间持续发现首次引导新注册的启动器。
非标准应用已经打开时可省略 CLI 的 `--launch`，只检查并保存连接。

未安装时，`/connect install-ego` 或 `golutra-agent browser install-ego` 打开
官方安装页。这里不会静默下载安装包、接受协议或绕过系统保护。
安装后完成 Ego 首次引导；不需要导入浏览器账号或设为默认浏览器。
`--wait` 最多等待 300 秒；TUI 默认等待 120 秒，期间可 `/connect cancel`。
Ego 引导可能输出说明并以 0 退出，只有实际版本回执才算连接成功。

## Chrome：安装后默认自动运行

```bash
golutra-agent browser chrome install
```

也可在 TUI 输入 `/connect install-chrome`。命令会：

1. 将内嵌扩展导出到 Agent home 的 `browser/chrome-extension`。
2. 将本机桥接程序部署到 `browser/native-host`，避免依赖仓库或 npm 临时路径。
3. 注册用户级 `com.golutra.browser` Native Messaging Host，只接受本扩展 ID。

目前扩展尚未上架 Chrome Web Store。第一次需要用户打开 `chrome://extensions`，
开启开发者模式，选择“加载已解压的扩展程序”，选中命令输出的扩展目录。
升级后点击扩展的重新加载按钮。扩展 0.4.0 默认自动连接本机并允许创建任务页，无需点击连接或开启按钮。
Agent 可在此 Profile 中新建任务页、复用有效登录状态。不会因此接管已有网页；
操作已有网页可展开“操作已有页面”，选择“使用当前标签页”。然后：

```bash
golutra-agent browser connect chrome --launch --wait 120
```

TUI 等价入口为 `/connect chrome`。已连接且开启自动任务，或已有获授权标签页，均可通过检查。
CLI 的 `golutra-agent browser connect` 省略后端参数时选择 Chrome；TUI `/connect`
显示连接说明。默认选择不等于已授权，首次仍需加载扩展；新建任务页无需额外授权按钮。
`activeTab/tabs` 用于授权入口和目标地址校验，`debugger` 用于页面操作和截图，
`downloads` 用于显式下载，`alarms` 用于连接失败后的分钟级重连；不声明全站 content-script 注入，也不导出 Cookie。
底层 Chrome 权限较广，实际操作范围由扩展的目标绑定和会话所有权限制。

默认注册 Google Chrome 正式版的用户级目录；自定义 `--user-data-dir`、Chromium、
Chrome for Testing 的注册目录可能不同。不要复制其他 Profile 的登录数据。
同一 OS 用户同时只接受一个扩展连接；另一个 Profile 必须先断开原连接。
Native Messaging 桥接目录固定在用户 home 的 `.golutra-agent/chrome-bridge`，
独立的 Agent home 共享这条浏览器连接，但仍按工作区和 session 隔离控制权。

升级这项功能后重新运行安装命令，并在 Chrome 扩展页点击重新加载，再授权目标页。
扩展安装与连接不会替换系统默认浏览器。

连接失败时 `browser status` 同时检查扩展资源是否缺失/过期、Native Messaging manifest
是否匹配扩展以及本机程序是否存在。`repair_required` 表示先重新安装；`installed` 仅表示
本地文件就绪，仍须在 Chrome 加载扩展并授权。Windows 注册表是否生效以真实连接为准。
扩展最低要求 Chrome 125；当前扩展版本 0.2.0，不等于 npm 包版本。

## 执行、恢复与停止

连接后，下一次 Agent 任务加载所选后端；使用浏览器工具仍需要显式 `--yolo`。
TUI 不允许在活动任务期间改变连接。没有自动切换后端或自动选择账号的逻辑。

两个后端共用 `browser_launch/status/help/run/handoff/finish` 六个工具、结果和图片链路。
`browser_help` 和输入 schema 随后端变化：Ego 运行 SDK 脚本；Chrome 使用结构化
`action`，不能把 Ego 的 Node 脚本原样发给 Chrome。

`browser_status.result.can_open_task:true` 表示可以使用 `browser_run {"action":"open","url":"https://…"}`
新建任务页。状态检查不会创建页面；`open` 不接受已有 tab ID，且只有前一个任务结束后才能再次使用。
同一会话连续任务和不同会话均默认支持新建任务页；同时只有一个任务持有控制权。

Chrome 支持新建任务页、快照、导航、点击、填写、按键、滚动、截图、页面表达式、文件上传和
同源 URL 下载。下载保存到 Chrome 的正常下载目录，返回文件路径；不会自动覆盖同名文件。
CSS 目标必须唯一。快照和点击支持可访问的同源 frame/open shadow root。
跨源 iframe 使用 `frames` 取得 `frame_id`，再交给 `snapshot/fill/click/evaluate`，
包括跨站进程中的 frame。显式 frame 的点击是 DOM click，不是可信鼠标事件；
frame 表达式在隔离 DOM 环境中执行，不能读取页面 JS 全局变量。依赖可信事件的网站需交回用户。
只读/禁用输入和被遮挡的点击目标会拒绝执行。系统窗口和验证码继续通过用户交接处理。

`browser_status` 报告直接子弹窗。Agent 先 `browser_handoff`，用户切到弹窗并点击扩展的
“在此关联页面继续当前任务”，即可保留原 session/binding 继续；同样可回到最近 32 个
已授权关联页面。无关标签页拒绝续接。普通“使用当前标签页”仍建立新的目标绑定。

用户在扩展点击“停止控制”或取消 Chrome 的调试连接后，自动任务授权会持久关闭，模型不能自行重新授权。
“断开连接”同时禁用自动重连；显式点击重新连接或授权后才恢复。意外断线可以重连通信，
但冻结中的旧任务不会自动恢复。用户选择“恢复自动运行”后可新建任务页，旧页面不会因此重新获授权。
`browser_handoff` 后，只有用户确认交回控制权才允许 `resume_after_user:true`。
`browser_finish` 释放会话，不关闭用户标签页；自动任务开启时，下次 `open` 无需重新授权。
断连、取消和超时可能发生在副作用之后；不会自动重试，先观察业务结果再决定。

`/connect disconnect` 或 `golutra-agent browser disconnect` 在 `browser.json` 写入
`{"backend":"disabled"}`，禁用后续任务的浏览器工具，避免缺少配置时又默认启用 Chrome。
重新连接成功后覆盖禁用标记。它不会中断已经发出的网页操作；立即停止请使用扩展按钮。
会话回执、截图、下载文件不会随断开配置删除。

## 验证与范围

浏览器协议/权限单测、Native Messaging 真实进程鉴权和 EOF 清理、Ego 配置失败不覆盖
旧配置、TUI 参数解析均有回归。Chrome 本地实机验收使用独立 Chrome for Testing Profile，
调用真正的扩展和 Rust ToolRuntime，不把 SDK 替身结果计为浏览器通过。

```bash
cargo build -p golutra-agent-cli
cargo build -p golutra-agent-tools --example chrome_fixture_host
node --test extensions/chrome/*.test.mjs
GOLUTRA_AGENT_CHROME_TEST_BINARY="/absolute/path/to/Chrome for Testing" \
  node extensions/chrome/live-test.mjs
```

自动启动夹具目前用于 macOS。其他平台可手动授权本地夹具页，再指定
`GOLUTRA_AGENT_CHROME_LIVE_URL=http://127.0.0.1:PORT/` 运行显式忽略的
`cargo test -p golutra-agent-tools --test chrome_browser_live -- --ignored --nocapture`。
测试启动器只对自己创建的 Chrome Profile 开放测试调试端口，桥接目录也独立，
不会占用普通 Chrome 的连接或修改 HOME；产品连接不依赖该端口。

实机覆盖中文表单、PNG、恢复原标签页、竞争会话拒绝、上传下载、交接/接管、结束、
扩展停止和断开。Windows/Linux 注册代码尚未实机验收，不能据 macOS 结果宣称全部平台通过。
复杂业务网站、验证码和系统权限窗口仍需针对性验证或用户处理。

### 本轮验收记录（2026-11-10）

- Ego 0.6.2.9：CLI 自动发现/保存/再次查询 ready；真实工具验收通过，5.01 秒。
  实机证据：系统临时目录 `golutra-ego-acceptance-jLauX9`。
- Chrome for Testing 158.0.8127.18：扩展界面授权、CLI 自动连接与状态、Rust 工具
  实机验收、同源 iframe 滚动后点击、停止与断开通过，Rust 场景 0.52 秒。
  证据：`golutra-chrome-live-FEpbSp` 和 `golutra-chrome-acceptance-B9VDxR`；PNG 已查看确认。
- CLI 37 项通过（另有 1 项既有忽略）；客户端 451 项通过；工具层独立全量 216 项通过；
  TUI 19+499+35+7 项通过（另有 1 项真实 provider 测试忽略）。
- 扩展状态机负向测试通过；相关四 crate Clippy `-D warnings`、格式和 diff 检查通过；
  CLI/TUI debug 构建通过。未 commit、push 或发布 npm。

默认策略追加回归：工具层 16 项、CLI 真实进程 3 项、TUI 入口 1 项通过，覆盖默认 Chrome、已有 Ego
配置保留、连接失败不保存替代配置以及断开后持续禁用；相关四 crate 严格 Clippy 通过。

组合回归中 Ego handoff 用例曾有一次错误，独立工具全量重跑通过；首次底层错误详情
未保留，尚不能确定原因，不把这次失败解释为已定位修复。真实浏览器通过范围仍仅为上述场景。

### 稳定性补齐（2026-11-11）

修复异步 attach/sendCommand 期间停止或切换目标的竞争：每个异步边界重新检查授权代次，
迟到的 attach 立即释放，旧请求不得写入新目标状态。断连前排队请求不能在重新授权后执行。
增加实际弹窗续接、跨站 frame 支持、安装诊断、点击遮挡和只读输入检查。

- macOS Chrome for Testing 实机覆盖上述功能、PNG、上传下载、竞争会话拒绝和停止，
  并执行 50 轮表单填写/点击/结果核对与 5 次交回/恢复；这是密集连续操作，非小时级耐久测试。
- Ego 协议夹具执行 30 轮、150 步生命周期压力测试，完整回执位于
  `golutra-ego-protocol-stress-IeBs48`，未复现此前偶发故障。当天本机 Ego 启动器及应用缺失，
  不能把此结果算作新的 Ego 实机验收，原偶发问题也不能宣称已查明根因。
- 工具层全量 217 项及后续新增取消回归 1 项、CLI 进程 4 项和扩展状态机回归通过；
  四 crate 严格 Clippy、fmt/diff、最终 CLI/TUI debug 构建通过。
  CI 新增扩展生命周期及三平台本机桥接进程检查，远端 CI 尚未执行。

最终 Chrome 证据目录：`golutra-chrome-live-pK3RpY`、`golutra-chrome-acceptance-uDA2cc`。
50 轮成功，连续表单部分 1730 ms，单轮 P50 30 ms / P95 50 ms / max 51 ms；
这是本地页面的工具操作耗时，不含模型推理或公网网站加载，不能当作真实业务端到端指标。
每次工具回执及 `soak.json` 均保留，PNG 已检查。

仍需真实 Windows/Linux 浏览器验证、复杂业务网站与长时间运行验收。
Chrome Web Store 上架涉及发布账号和外部审核，本轮保留本地加载流程。

### 原有 Profile 启动验收（2026-11-11）

新增共享 `browser_launch`、CLI `browser launch` 与 TUI 启动入口；`/connect chrome/ego`
先正常唤起应用，再等待连接。启动不保存后端选择，连接通过才保存；Ego 重连保留自定义 SDK。

真实 GPT-6-Astra Responses 任务仅调用一次 `browser_launch` 和一次 `browser_status`：
前者向 `/Applications/Google Chrome.app` 发出正常启动请求，回执耗时 64 ms；
后者返回 `chrome_disconnected`，模型如实结束，没有导航、重试或绕过授权。
任务约 8.5 秒完成，64 ms 不代表网页加载耗时。系统中确认存在无附加参数的普通 Chrome 主进程。
证据目录：`/tmp/golutra-user-browser-launch.xfiBnM/run`；
session：`01a1cb18-fa78-70d3-9604-111baaa08daf`。

该结果证明正常启动和未授权边界，尚未验证此普通 Profile 中已登录网站的操作。
本机未发现 Ego 应用，CLI 正确返回安装指引；本轮没有新的 Ego 原生启动验收。
专项工具 21 项通过（1 项显式压力测试忽略），CLI 进程 4 项通过，
覆盖取消、重复调用拒绝、restricted 拒绝、禁用配置及非法参数无启动副作用。
TUI 入口回归 1 项、相关四 crate 严格 Clippy 和最终 CLI/TUI debug 构建通过。

2026-11-12 复测：普通 Chrome 再次返回 `launch_requested`，但状态仍为
`chrome_disconnected`，所以没有继续执行 Hacker News 导航/点击。
安装诊断为 `installed`，manifest 有效、扩展资源无缺失或过期；仍需用户在 Chrome
加载/重新加载扩展并授权目标页。证据：`/tmp/golutra-browser-retest.I9qIH2/run`。
Ego 的 `.local/bin/ego-browser` 仍存在，但通过 `active_version_dir` 指向已不存在的
`/private/tmp/golutra-ego-live.2KPBzJ/ego lite.app`；这是一条失效的临时安装链接，
标准应用目录也未发现 Ego。需要正式安装并由 Ego 注册有效启动器，不能只凭链接存在判为可用。

同日用户加载扩展后完成普通 Profile 实测：授权页 ready，真实 Agent 自行导航 Hacker News、
点击首条并核对原文标题/URL/正文与 PNG，finish 释放控制。14 次工具调用中 12 成功、2 错误后恢复，
任务约 79.2 秒，CLI exit 0。详见 [普通 Chrome 验收](browser-chrome-user-smoke-2026-11-12.md)。
这补齐普通 Profile 网页操作验证；该次测试尚未包含后续的一次授权自动任务功能。

### 一次授权自动任务（2026-11-12，0.3.0 历史记录）

扩展 0.3.0 将通信连接与网页授权分开：默认自动连接本机，用户只需开启一次自动任务，
后续由 `open` 新建任务页；不自动接管已有网页。设置跨浏览器重启保留。
停止会撤销自动任务许可，断开会禁用自动重连；迟到握手、异步页面查询和创建都不能恢复旧授权。
关闭已完成任务页不撤销许可，关闭仍受控的任务页则停止。按项目开发规范补充这些状态转换回归。

- 扩展状态机 8 项通过；工具浏览器专项 21 项通过、1 项显式压力测试忽略；CLI 真实进程 4 项通过，
  包含无现有目标、仅自动任务许可时的就绪判定。
- 独立 Chrome for Testing 实机两次通过。最终验收：一次开启自动任务后，连续创建 3 个任务页，
  覆盖同 session 与新 session，原有夹具页保持不变；表单、PNG、跨站 frame、上传下载、
  弹窗续接、50 轮连续操作、停止与断开通过。Rust 验收耗时 3.12 秒，不含模型推理。
- 最终证据：系统临时目录 `golutra-chrome-live-g59iZx`、`golutra-chrome-acceptance-QFa0pE`；
  各调用回执、`screenshot.png`、`soak.json` 保留。首轮 `3V1ZXs/i0H9t3` 也通过，PNG 已查看。
- 四 crate 严格 Clippy、fmt/diff 检查、最终 CLI/TUI 构建通过。扩展与本机连接程序已导出到
  用户安装目录，并核对安装资源与源码一致。

当时用户的普通 Chrome 仍需在 `chrome://extensions` 点击“重新加载”，再开启一次
“允许自动创建任务标签页”。本轮未代替用户开启该许可，也未把独立 Profile 验收当作普通
Profile 新授权流程已完成。未提交、推送或发布 npm；Windows/Linux 和长时间耐久仍待实机验证。

### 默认统一自动运行（2026-11-12，0.4.0）

根据用户明确同意，首次加载后默认启用本机连接和新建任务页，移除常规流程中的连接/开启按钮。
弹窗默认只保留停止、断开；主动停止后才显示“恢复自动运行”。已有页选择和关联页续接收进折叠区。
升级保留显式保存的停止/断开设置，不能用新默认值覆盖用户的暂停决定。

实机使用全新独立 Chrome Profile，不点击任何开启或授权按钮，CLI 即 ready，连续创建三个任务页，
完成表单、截图、上传下载、跨站 frame、弹窗续接及 50 轮操作，最终停止和断开生效。
额外按钮只用于测试明确的页面交接和停止，不用于启动上述自动任务。
证据：`golutra-chrome-live-1yTE5x`、`golutra-chrome-acceptance-Nj7HBZ`；Rust 场景 3.04 秒。
扩展 9 项、工具专项 21 项（1 项压力测试忽略）、CLI 进程 4 项通过。
首次安装或升级仍须 Chrome 加载/重新加载扩展，这是浏览器的安装机制，不是任务授权开关。

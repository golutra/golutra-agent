# Golutra 桌面离线集成合同 v2

Agent 自有命令、环境变量和数据目录已独立改名，不提供旧名称回退；桌面 IPC 接口保留原名。见[命名空间与桌面改接清单](agent-namespace.md)。

桌面安装包直接携带从 **native release archive** 或 **npm 平台包**提取的原生程序及配套文件。普通 CLI/TUI 集成可在桌面构建时下载 npm 平台包，按下文校验后内置；需要 app-server 等完整工具集时使用原生归档。用户安装时无需 Node.js、npm、Rust、Python，也不需要下载 Agent。Python/npm/Cargo 只用于构建和验收。调用真实云端模型仍需要用户配置 provider、凭据及网络；离线安装不等于离线推理。

## 产物与版本锁定

原生归档与 npm 平台包使用同一次 Cargo 构建的二进制。`desktop_release.py` 再比较两种归档内 CLI/TUI、LICENSE、NOTICE 的实际 SHA-256，不同则阻止发布。

| 系统 | CPU | Rust target | npm 平台包名 | 原生格式 |
| --- | --- | --- | --- | --- |
| Windows | x64 | x86_64-pc-windows-msvc | `@golutra/agent-win32-x64` | zip |
| Windows | ARM64 | aarch64-pc-windows-msvc | `@golutra/agent-win32-arm64` | zip |
| macOS | x64 | x86_64-apple-darwin | `@golutra/agent-darwin-x64` | tar.gz |
| macOS | ARM64 | aarch64-apple-darwin | `@golutra/agent-darwin-arm64` | tar.gz |
| Linux GNU | x64 | x86_64-unknown-linux-gnu | `@golutra/agent-linux-x64` | tar.gz |
| Linux GNU | ARM64 | aarch64-unknown-linux-gnu | `@golutra/agent-linux-arm64` | tar.gz |

版本 `V` 的发布文件：

```text
desktop-release-vV.json
golutra-agent-vV-TARGET.tar.gz                 # Windows 为 .zip
golutra-agent-vV-TARGET.tar.gz.sha256
golutra-agent-vV-TARGET.tar.gz.manifest.json
golutra-agent-vV-TARGET.tar.gz.smoke.json
```

固定地址为 `https://github.com/OWNER/REPO/releases/download/vV/文件名`。CI 从 `GITHUB_REPOSITORY` 获取实际发布仓库，本地必须显式传 `--repository OWNER/REPO`。0.3.0 正式发布仓库为 `golutra/golutra-agent`；开发远端 `seekskyworld/golutra-agent` 不是此次产物下载源。清单提供版本、源码 commit、平台、CPU、URL、归档摘要、文件摘要、启动合同和实际验收系统。**只有对应 tag 的 release 成功发布后地址才可下载**；新版本不会覆盖已发布的 0.2.0 产物。

桌面项目把版本、target、URL 和归档 SHA-256 一起锁进源码；构建时下载并核验摘要，安全解压，再打进安装包。不要用 `latest`，不要在用户安装时下载。拒绝错误架构、checksum 不符、绝对路径、`..`、链接、Windows drive/ADS 等不安全条目。摘要应来自已审阅并锁定的清单；同一下载位置的 `.sha256` 不是独立真实性证明。

正式清单要求六个平台均有匹配归档摘要的**本机运行验收**。`--allow-partial --allow-development` 只用于本地开发，不允许进入桌面正式锁定清单。

## 从 npm 平台包内置 CLI/TUI

`@golutra/agent` 根包提供 JavaScript 启动器，并通过 `optionalDependencies` 选择平台包。桌面构建直接下载上表对应的**平台包**即可；不要只复制根包的 `bin/*.js` 或全局安装生成的命令 shim。平台包的 `vendor/bin/` 已含可独立运行的 CLI/TUI，不需要 Node.js。包元数据中的 `engines.node` 是 npm 工具链约束，不是这两个原生文件的运行依赖。

以下为 macOS ARM64、固定版本 `0.3.0` 的构建机示例；在专用下载目录执行，其他目标替换包名。选择依据是**桌面安装包的目标平台/架构**，不是构建机自身的平台；不要让根包的自动依赖选择决定交叉打包产物。

```sh
npm view @golutra/agent-darwin-arm64@0.3.0 name version dist.tarball dist.integrity --json
npm pack @golutra/agent-darwin-arm64@0.3.0 --ignore-scripts --json
```

示例中的 `npm view` 用于首次锁定或明确升级版本时获取元数据，常规构建不据此自动更新锁定值。带 `--ignore-scripts` 的 `npm pack` 下载包而不做全局安装、不执行包脚本；从 JSON 结果读取实际文件名，不假定它与 GitHub Release 里的 npm 归档命名相同。构建器也可直接下载已锁定的 `dist.tarball` URL，下载本身不依赖 npm 命令。

桌面接入按以下顺序处理：

1. **锁定来源。** 审阅版本 `V` 的 `desktop-release-vV.json`，选定与 target 对应的 `artifacts` 条目，将版本、源码 commit、target、`npm_package`、`npm_sha256`、逐文件 `files` 摘要及注册表的 tarball URL、`dist.integrity` 一起锁进桌面仓库。正常构建只消费锁定值，不临时查询 `latest` 或自动接受新摘要。正式发布继续要求 `complete:true`、`development_only:false`，npm 已上架不能替代桌面发布验收。
2. **验证归档。** 下载原始 `.tgz`，校验锁定的 SRI `dist.integrity`（通常为 SHA-512）和发布清单的 `npm_sha256`（整个 npm 归档的 SHA-256）。这两种摘要不能直接互相比较，也不能拿原生归档的 `sha256` 当 npm 包摘要。摘要或发布清单缺失、不一致时停止正式打包，重新确认发布来源；不通过重打包或更新锁值绕过。
3. **验证文件。** 按上一节的安全解压约束处理归档，核对 `package/package.json` 的 name/version/os/cpu，以及 `package/vendor/manifest.json` 的 package/version/target。对实际文件重新计算 SHA-256：npm 的 `package/vendor/bin/<程序>` 对应发布清单 `files` 中的 `bin/<程序>`，`package/LICENSE`、`package/NOTICE` 分别对应 `LICENSE`、`NOTICE`。同时核对 npm manifest 的二进制摘要；不能仅相信包内自带的 manifest。
4. **加入安装资源。** 保留下面的文件与布局，去掉归档最外层 `package/` 后放入桌面自己的资源目录，例如 `<resources>/agent/`。Unix 保留可执行权限，Windows 保留 `.exe` 后缀。npm 的 `vendor/manifest.json` 与原生归档的根 manifest 格式不同，保留其路径和语义，不将两者视为可互换清单。
5. **定位与启动。** 桌面解析出 `<resources>/agent/vendor/bin/golutra-agent[.exe]` 或 `golutra-agent-tui[.exe]` 的绝对路径，以参数数组启动。检测、版本查询和实际启动使用同一套路径选择；CLI 与 TUI 必须同目录。TUI 接入 PTY/ConPTY，cwd 指向用户项目，数据目录按下文 `GOLUTRA_AGENT_HOME` 合同处理。用户显式选择外部原生路径时，同一选择规则也用于检测。

```text
package/
  package.json
  LICENSE
  NOTICE
  vendor/
    manifest.json
    bin/
      golutra-agent[.exe]
      golutra-agent-tui[.exe]
```

`package.json` 保留来源与平台信息，不需要运行其中任何 npm 入口。当前平台包只有上述两个原生程序，不包含 app-server、vis、supervisor、launcher 或 eval-worker；需要这些独立入口时改用完整原生归档。

校验通过后仍须纳入桌面自身的签名、安装、升级和卸载流程。macOS 重新签名会改变二进制摘要：上游摘要在签名前验证，签名后的安装资源由桌面自己的产物清单记录。最终验收从实际安装目录用绝对路径启动，在无 Node/npm 的环境检查版本及 TUI，并验证已有 npm 安装可继续使用。**构建时下载一次，用户安装、启动和升级内置 Agent 时均不执行 `npm install -g`**；Agent 随桌面安装包升级，不改用户全局 PATH 或 npm prefix。

## 目录与启动

```text
golutra-agent-vV-TARGET/
  manifest.json
  LICENSE
  NOTICE
  bin/
    golutra-agent[.exe]        # 统一原生入口；Cargo package 为 golutra-agent-cli
    golutra-agent-tui[.exe]          # 原生交互 TUI
    golutra-agent-app-server[.exe]
    golutra-agent-vis[.exe]
    golutra-agent-supervisor[.exe]
    golutra-agent-launcher[.exe]
    golutra-agent-eval-worker[.exe]
```

当前核心 CLI/TUI 的代码、协议及默认配置随编译产物交付，不依赖仓库中的 README、图片或 JavaScript。保留完整原生归档最简单；桌面只使用 CLI/TUI 时，可从已经核验的归档选择这两个文件，同时保留 `manifest.json`、LICENSE、NOTICE。npm 平台包已有的 `vendor/bin/` 也是相同原生文件，不能仅复制 npm 根包的 JS 启动器。

桌面端负责选择并验证绝对路径，默认选择安装资源目录里的内置文件；用户显式指定外部原生文件时才用外部路径，不扫描 PATH 自动替换版本。使用 `spawn(executable, argv, {cwd, env})` 同等能力，**不用 shell 拼接字符串**，正确处理空格和中文。安装目录只读，工作目录设为用户项目。升级时停掉旧子进程后整体替换资源目录。

| 目的 | 可执行文件 | argv |
| --- | --- | --- |
| 查询原生版本 | `bin/golutra-agent` | `--version` |
| 帮助 | `bin/golutra-agent` | `--help` |
| 一次任务，JSONL 输出 | `bin/golutra-agent` | `--cwd /absolute/project exec --json "任务"` |
| 多行输入（不用 shell 转义） | `bin/golutra-agent` | `--cwd /absolute/project exec --json -`，写入 stdin 后关闭 |
| 已结束会话续跑 | `bin/golutra-agent` | `--cwd /absolute/project exec --json resume THREAD_ID "后续任务"` |
| 历史列表 | `bin/golutra-agent` | `--cwd /absolute/project thread list` |
| 终端界面 | `bin/golutra-agent` | `--cwd /absolute/project`；必须连接 PTY/ConPTY |

Windows 路径增加 `.exe`。原生 `golutra-agent` 无参数或带交互参数（例如 `--cwd`、`--yolo`、`--resume THREAD_ID`）启动同目录 `golutra-agent-tui`；npm 仅转发同一原生入口。`golutra-agent resume THREAD_ID` 也进入交互恢复。必须成套安装 CLI/TUI，不搜索 PATH，不回退旧名称。`exec` 新建 thread，`exec resume` 继续指定 thread。`exec --json` 的 stdout 是 JSONL，进度/错误在 stderr；持续排空两条管道，保留退出状态。权限请求需用户处理，不要为了自动化默认加 `--yolo`。

已有 app-server/driver 协议可供需要长连接的桌面 UI 另行接入，见 [运行入口](runtime-entrypoints.md)。本合同不需要常驻服务，不自动启动 `golutra-agent-launcher`/supervisor 的高级源码演进功能。

## 环境和系统依赖

只有需要指定共享或隔离数据目录时才需设置 `GOLUTRA_AGENT_HOME`，建议桌面传入**绝对路径**。不设置时延续 `$HOME/.golutra-agent`；本次增加原生 Windows 无 HOME 时的 `%USERPROFILE%\.golutra-agent` 回退。若机器的 npm shell 设置了自定义 HOME，桌面应显式选择相同 GOLUTRA_AGENT_HOME，避免两个“默认目录”。不需要 `GOLUTRA_AGENT_MANAGED_PACKAGE_ROOT` 或 `GOLUTRA_AGENT_PACKAGE_TARGET`；它们是现有 npm 启动器携带的提示，当前原生代码不据此执行更新。

真实工作保留用户 PATH，供 shell、Git、项目编译器及用户配置的 MCP 工具使用。Node/npm 不是 Agent 启动依赖，但 JavaScript 项目或 `npx` 型 MCP 服务仍会需要它们。首次运行不自动下载这些工具。不得把安装资源目录当 GOLUTRA_AGENT_HOME。

- macOS：使用系统动态库/Framework；本机 `otool -L` 未发现 Homebrew 或第三方 dylib 依赖。桌面发布需按自己的签名/notarization 流程处理嵌套程序，签名后更新桌面自身资源摘要；上游下载摘要用于签名前校验。守护 shell 的沙箱使用系统 `sandbox-exec`。
- Linux：当前提供 GNU/glibc 构建，**不宣称支持 musl/Alpine**。默认 runner 的 glibc 版本不代表可运行在更旧发行版；以清单中的实际验收系统为准。guarded shell 的 OS 隔离依赖 `bwrap` 及可用的 user namespace；缺少时遵循现有策略报错，不为了启动成功改成 unrestricted。要覆盖更旧 Linux，需换较旧 sysroot 并在目标系统实际验收。
- Windows：MSVC native 程序，发布构建固定 `-C target-feature=+crt-static`，npm/桌面复用同一份静态 CRT 产物，避免安装时联网补 VC runtime。仍依赖 Windows 系统 API。该配置和 CI runner smoke **不能证明**所有干净 Windows 版本均已验收；桌面发布还应检查 PE 依赖并在最低支持的干净 OS 上执行启动测试。ConPTY 验收属于桌面终端集成；本仓库 Windows native smoke 只覆盖 CLI/非交互启动。

六个平台 CI 在对应 CPU 原生 runner 执行，无模拟器/交叉编译冒充运行验收。真正的最低 OS、干净 OS 系统运行库和桌面签名验收由桌面发布门禁再补充；尚未运行的目标必须标为未验证。

## 共用配置、登录与历史

同一个本地用户、同一 GOLUTRA_AGENT_HOME、同一规范化工作区路径：

| 数据 | 位置 | 现有并发/格式保护 |
| --- | --- | --- |
| Provider 配置 | 全局 `provider.json` | 文件锁内读改写、原子替换，未知版本拒绝；项目级 Provider 配置不支持 |
| 非敏感运行设置 | 全局 `runtime.json`、项目 `.golutra-agent/runtime.json` | global → project → session 覆盖；桌面全局编辑用 revision 补丁，冲突拒绝 |
| 登录凭据 | `credentials.json` | owner-only 文件、跨进程锁、原子替换、未知版本拒绝 |
| OAuth 刷新 | 同 home 的凭据/refresh 锁 | 同凭据跨进程串行刷新，重读 revision；不是只锁最终写入 |
| Thread、事件、任务 | `state/runtime.sqlite` | WAL、busy timeout、当前 schema 7 事务初始化、精确 checksum/version 校验；`runtime.sqlite.usage.lock` 覆盖 store 使用期 |
| 产物 | `state/artifacts/` | 与数据库 identity 绑定，不应单独搬走或跨库复用 |
| 工作区记忆/评估/演进/索引 | `state/workspaces/<hash>/` | 各自文件锁，必须保留整套数据与锁文件 |
| 正在执行的会话 | `state/session-locks/` | OS 独占锁；不能由另一实例强行接管 |

不另建桌面专属登录库。配置变化经现有命令保存，下一次读取/启动生效；运行中任务可能持有已解析的配置快照，不承诺热同步所有设置。凭据是敏感本地数据，桌面也要遵循用户权限，不应上传或记录明文。

**共享支持边界：**桌面和 npm 都使用最新版的同次构建，可共用当前格式配置、凭据和历史并执行不同 thread。不要由两端同时恢复同一个运行中 thread，也不要删除锁文件。使用可靠本地磁盘，不使用不具备可靠锁语义的网络盘/云同步目录。

**不兼容旧格式。**新库在独占使用锁下直接创建 schema 7；已有库必须是当前唯一基线和精确 checksum。旧 schema 1–6、无账本旧库、未知格式均返回 `data_unsupported`，不自动迁移、修复或清空。Provider v1 也直接拒绝，当前 v2 正常共用。

旧测试目录不能靠升级程序继续使用。关闭旧实例后显式选择新的空 `GOLUTRA_AGENT_HOME` 并重新登录，两份最新版均指向这个目录。程序不删除旧目录，也不自动同步两套凭据或历史。旧程序不认识当前使用锁，不能让它继续接触新目录。详细接口与验收见[共享数据合同](shared-data-compatibility.md)。

## 更新、卸载与职责

- 内置程序随 Golutra 桌面升级；npm 安装继续由 npm 管理。原生 CLI/TUI 没有全局 npm 自更新入口，本次也不增加一个。桌面不调用 `npm install -g`，不写 npm prefix，不改变全局 PATH，不覆盖显式外部程序。
- 桌面卸载只删除自己的资源/安装目录；npm 卸载只删除 npm 安装。两者均不删除共享 GOLUTRA_AGENT_HOME、工作区 `.golutra-agent` 或历史。“清除数据”应作为独立、明确的用户操作。
- 本仓库提供 payload、摘要、启动合同、离线 native smoke；桌面项目负责资源定位、外部路径设置、进程/PTY 管理、签名、installer/updater/uninstaller。Agent 不新增桌面路径选项或常驻服务。

## 构建与自动验收

```sh
python3 scripts/package_release.py --target aarch64-apple-darwin --output-dir dist/desktop
python3 scripts/package_npm.py --package platform --target aarch64-apple-darwin \
  --binary-dir target/aarch64-apple-darwin/release --output-dir dist/desktop/npm
python3 scripts/smoke_native_package.py \
  --archive dist/desktop/golutra-agent-v0.3.0-aarch64-apple-darwin.tar.gz \
  --npm-archive dist/desktop/npm/golutra-agent-npm-darwin-arm64-0.3.0.tgz --require-pty
# 汇总六个平台的产物及 smoke 后，正式构建不传两个 allow 参数：
python3 scripts/desktop_release.py --dist release-assets --repository golutra/golutra-agent
```

版本号示例应替换为将要发布的唯一新版本。CI 会构建七个原生程序、npm 包，验证 npm 入口，再在无 Node/npm 的 PATH 中运行绝对路径 native、四进程离线任务、共享历史/配置、未来 DB/config 拒绝、真实 Unix PTY 和移除内置目录后的并存检查。另有 auth 独立进程并发写入及 future credential 格式回归。

Windows 已发布的 0.2.0 存在 SQLite URL 解析缺陷，空目录也无法启动。该平台的旧版基线明确记录为 `legacy_startup_unavailable`，不能宣称新旧程序双向格式验收通过。当前程序仍须完成全部原生运行与共享数据验收，并验证合成 schema 5 账本被拒绝且数据不变；旧程序失败启动也不能改写新版数据。Unix 继续使用真实 0.2.0 二进制进行双向拒绝测试。

本机执行结果、实际开发产物摘要及平台未验证项见 [桌面交付验收记录](desktop-acceptance.md)。开发构建和本地清单不应作为已发布正式版本交付用户。

# 整体验收 · 2026-11-16

本轮针对当前未提交工作区重新执行整体测试，覆盖 Rust 全工作区、IPC/TUI 进程、SDK、
协议生成、浏览器、Release 构建与隔离安装。沿用主代理加三个子代理的分工，
所有测试输出保留在 `/tmp/golutra-full-validation.3116hB/`。

## 环境与范围

- 本机为 Apple Silicon，macOS 27.2.1，Rust 1.93.0。
- JavaScript/Python 检查使用 Node 26.4.0、npm 11.17.0、Python 3.14.5。
- 这不是 Linux/Windows 或 CI Node 22/Python 3.11 矩阵的验收结果。
- 未配置显式 live-provider 测试所需凭据和预算；远程模型服务未实测。
  协议、流式响应和工具调用使用受控本地 provider 测试覆盖。
- 未 commit、push 或发布 npm；安装和配置验收仅使用临时目录。

## 测试结果

| 范围 | 本轮结果 |
| --- | --- |
| Rust 全工作区 | 最终 2,313 通过、0 失败、11 忽略；88 个 Cargo 测试目标均完成 |
| IPC 传输定向回归 | 16 通过，包含 3 个新增确定性回归 |
| TUI Driver 按 CI 串行运行 | 修复后 7 通过、1 个真实模型测试忽略 |
| TypeScript SDK | 类型检查通过，9 项测试通过 |
| Python SDK | 编译检查通过，14 项测试通过 |
| 扩展与 npm OpenCLI 包装 | 10 项测试通过；变量统一后启动器定向复测通过 |
| 发布脚本 | 修正命名后 39 项全部通过 |
| 协议与 SDK 生成 | Rust schema、TS、Python 三份生成结果与当前工作区逐字节一致 |
| npm audit | 0 漏洞 |
| 开源元数据 | 通过 |
| 严格 Clippy / rustfmt / diff 检查 | 通过 |
| Release 构建 | 配套 CLT 环境成功，7 个程序均通过 Mach-O 装载校验 |
| 原生运行合同 smoke | 6 项通过 |
| 原生压缩包 smoke | 11 项通过，包含真实 PTY、共享数据及卸载隔离 |
| npm 压缩包 smoke | 通过，包含真实 PTY |
| 临时 npm 真实安装 | 20 个包安装成功；双入口版本/help、OpenCLI 解析、Agent 运行与 TUI 真实终端均通过 |

Rust 统计取每个 Cargo 测试目标最后一条汇总，排除凭据进程测试打印的两条重复子进程
通过记录。原始所有汇总行相加为 2,315 通过；这里报告的 2,313 没有重复计算它们。
首轮同口径是 2,309 通过、1 失败、11 忽略；随后修正 IPC 并增加 3 个回归测试。

最终全量命令为 `cargo test --workspace --locked --no-fail-fast`，未通过筛选、
强制串行或删除失败用例取得通过。`rust-workspace-final.log` 与 `clippy-final.log`
保留完整结果。11 项忽略包含真实浏览器、真实模型与辅助入口，其中本轮另外执行了
Ego、隔离 Chrome 及各自结束任务的实测。

## 验收发现与修正

### 环境变量命名

发布检查发现新浏览器代码使用了项目命名空间之外的 `GOLUTRA_OPENCLI_*` 等名称。
生产适配器、npm 启动器、测试夹具与文档已统一到 `GOLUTRA_AGENT_OPENCLI_*`、
`GOLUTRA_AGENT_EGO_*`、`GOLUTRA_AGENT_CHROME_*` 和 `GOLUTRA_AGENT_BACKGROUND_*`。
没有放宽命名检查；现有浏览器配置 JSON 的路径和字段保持兼容。

### IPC 响应丢弃竞态

首轮 TUI 重启测试报 `Socket is not connected (os error 57)`。定向复跑虽然通过，
但独立 Unix socket 实验确定性复现了生产代码中的竞态：请求已经完整写入，服务端
回复并关闭，客户端的写端半关闭返回 `NotConnected`，随后仍可读到完整回复。

修正仅容忍这个半关闭错误，并继续严格校验响应头、内容及结束帧；不重复发送请求。
其他错误依旧失败。新增测试覆盖完整回复、缺失/截断回复及其他关闭错误。
完整诊断保留于 `tui-driver-ipc-diagnosis.md`。

### 本机 Release 工具链

默认 Xcode 27 环境两次构建在加载 `sqlx_macros` 时失败，报
`mis-aligned LINKEDIT string pool`；第二次已单独重建该包缓存，因此不能归因于
一次性的旧缓存。清理仅移除了 6 个可重建的构建文件，共 7.6 MiB。

检查发现旧 CLT linker 与新 Xcode SDK 混用也不兼容，因此使用本机已有的匹配组合
进行本次构建：CLT clang/ld 和 MacOSX26.4 SDK。只在命令环境中指定
`DEVELOPER_DIR`、`SDKROOT` 与目标 linker，没有修改系统 `xcode-select` 或项目编译设置。
原日志、产物检查和环境命令保留于 `build-environment.md`。

配套 CLT 构建用时 2 分 34 秒，7 个程序全部通过 `dyld_info -validate_only`。
宏库仍可携带相同字符串池偏移而在 CLT SDK 标记下通过校验，因此证据指向新工具链/
SDK 元数据相关的兼容校验，不能断言只有改变偏移才能修复，也不应改依赖掩盖这个问题。
默认 Xcode 环境失败与配套 CLT 环境成功分别记录；项目默认编译配置没有被改写。

## 打包与安装

使用本次新编译的 7 个程序生成 `aarch64-apple-darwin` 原生包及 npm root/platform 包，
全部产物在证据目录下的 `dist/`。原生包验证了不用 Node/npm 也能启动、中文与空格路径、
共享配置及历史、未来 schema 拒绝且保留数据、真实终端启动，以及分别移除桌面/npm
测试载荷后另一入口仍可用。npm 包也单独通过要求真实 PTY 的 smoke。

另在临时 prefix 中真实执行 npm 本地安装，使用独立 npm 配置与缓存，不使用真实
发布凭据，不全局安装，不发布包。安装后的入口及依赖检查另存于安装验收日志。

实测 `golutra-agent` 和 `golutra-agent-tui` 两入口版本均为 `0.3.9`，帮助正常；
安装的 OpenCLI 依赖解析和实际运行版本均为 `2.0.3`。安装后的 Agent 使用 mock
provider 完成一次任务并返回 verification pass，安装后的 TUI 在真实 PTY 中完成
启动、渲染和退出。版本号未递增，这些是本地验收产物，不代表 npm registry 已更新。
汇总见 `package-acceptance-summary.json`。

本机原生归档 SHA-256：
`a2fb3b4f6948d86d3295a6dc9861e9048977f39187e537671d4cd43304862da9`。
npm 平台包 SHA-256：
`bb852e5681f5310e6841475b103b9ab84d3e4159b331a6cfabfa1826dae11ed8`。

## 浏览器实测

| 环境 | 结果 | 观察 |
| --- | --- | --- |
| Ego 0.6.3.14 / SDK 2.2.1 | 通过，0.77 秒 | 后台回执 7 ms；导航 1 次、点击效果 1 次；PNG 显示 `Count: 1` |
| 独立 Chrome for Testing / OpenCLI 2.0.3 / 官方扩展 1.0.27 | 通过，3.23 秒 | 后台回执低于 1 ms；导航 1 次、点击效果 1 次；PNG 显示 `Count: 1` |
| 用户普通 Chrome / 当前 daemon | 未通过稳定性验收 | 30 秒观察到 24 次扩展重连；本轮没有重复页面动作 |

这些耗时来自单次本地测试，两个后端调用结构不同，不能用作性能排名。
两种成功环境均验证了相同调用 ID 不重复执行，先检查 PNG，再独立结束测试任务。
Ego 原测试空间和隔离 Chrome 受管页已清理；用户其他标签及配置未改动。

截至上述全量验收时，普通 Chrome 与 Ego 同时连接同一个 OpenCLI daemon，共享 contextId
导致连接相互替换尚未确认。不能将当时隔离 Chrome 通过等同于普通 Chrome 已稳定。
详细日志、截图及清理回执见证据目录中的 `browser-report.md` 和两组 evidence 子目录。

## 后续普通 Chrome 复测与选择入口

后续 lsof 连续采样观察到 Chrome 和 Ego 新连接交替；OpenCLI 按相同 contextId 替换旧连接。
用户停用 Ego 中的 OpenCLI 扩展后，60 秒内 61 次状态采样全部在线，新增连接为 0，
daemon/context/lastSeen 不变。普通 Chrome 后台验收随后通过（4.32 秒，导航/点击效果均为 1），
截图已查看，任务单独释放。证据：
`/var/folders/fv/2smpjc6d2fl4d73rdtdy12q00000gn/T/golutra-background-browser-live-Jvf5MR`。
这补充了前表中的失败，不代表完成长期稳定性测试。

再按用户要求实现 Ego 优先、缺浏览器时确认安装和 Agent 可写的 `browser_backend`。
本次增量测试、统一路由下两种真实浏览器验收及限制见
[浏览器选择与安装验收](browser-selection-validation-2026-11-16.md)。前面的全量计数属于当时版本，未据此声称增量后的全项目重新跑过。

之后的 [Agent 运行时审查](agent-runtime-review-2026-11-16.md) 继续修复后台结果归并、
大输出信息丢失、无历史摘要的压缩持久化和进程启动前取消。本阶段相关模块最终
1,044 项通过、8 项忽略；这是增量验证，不替换上面的全 workspace、SDK 和发布矩阵记录。

# Agent 命名空间分离验收

日期：2026-09-16；执行者：主 Agent 独立实施，未委派。只修改 Agent 项目，未修改 Golutra 桌面仓库，也未安装、卸载或覆盖桌面程序。以 `2400ad7` 后的工作树为基础，保留此前未提交的共享数据和发布工作。

收尾时间：2026-09-16 17:32。按开发规范明确资源所有权、保留失败证据并完成复测；最终 all-targets Clippy、格式检查和 112 项脚本复跑均退出 0。变更与排障记录保存在本仓库，不修改桌面项目。

## 实际原生验收

本机 Darwin 25.5.0 / macOS ARM64，dev 构建。版本字段仍为 0.2.0，以下是未发布开发产物，不能替换已发布的 npm 0.2.0；新合同必须使用新的正式版本发布。

| 验收 | 结果 |
| --- | --- |
| 空 PATH 中通过绝对路径查询版本、帮助 | 通过；没有 Node/npm；路径含空格和中文 |
| 统一主入口启动 TUI | 原生 `--yolo` 和 npm 无参数均在真实 PTY 通过 |
| 配套 TUI 缺失 | 明确失败，提示新名 sibling，不回退旧 `golutra-tui` |
| 全局和项目命名空间 | 创建 `.golutra-agent`；旧 GOLUTRA_HOME、旧 provider 环境变量、旧 `.golutra` 非法配置不影响执行且原文不变 |
| 新项目配置仍生效 | 新 `.golutra-agent/runtime.json` 非法时，实际 exec 拒绝并指出该文件 |
| npm 与内置包共用 | 两份 native 的 CLI/TUI 字节摘要一致；四个独立进程并发执行，历史一致，SQLite integrity_check 为 ok |
| 配置与登录并发 | 本地 HTTP fixture 的两次并发登录均保留 profile 和 credential；revision 冲突与非法字段拒绝 |
| 非当前格式 | 未来 SQLite、Provider v1/未来版本拒绝；拒绝后数据和凭据不变 |
| 真实旧 npm 原生包 | 强制两者指向同目录，两种先后顺序均拒绝不兼容读写；原创建者仍能读取 |
| 安装归属 | 分别移除临时 npm / 内置 payload 后，另一份可执行仍能访问共享配置和历史 |
| 发布清单 | 合同 v2、Agent 主入口/home；本机部分开发清单生成成功 |

旧包强制共用是负向验收，不是产品支持的使用方式。新的默认 home 已经与旧版和桌面目录分开。`thread list` 不需要加载项目模型设置，因此隔离测试使用真实 `exec` 来证明新配置生效、旧配置不生效。

## 产物证据

目录：`dist/agent-namespace/`。完整原生归档含七个程序、manifest、LICENSE、NOTICE。npm 平台包复用其中 CLI/TUI；根包只有 JavaScript 分发入口。

| 文件 | SHA-256 |
| --- | --- |
| `golutra-agent-v0.2.0-aarch64-apple-darwin.tar.gz` | `bc326f48cb33810484f3b5e36f0fbe98a3b8b68378f0a35fc62f1b58d1f133ca` |
| `npm/golutra-agent-npm-darwin-arm64-0.2.0.tgz` | `5f1427e7b0891a3d8916f9cd877864532c58e696b5c6c4902d7af70983b01545` |

同目录包含逐文件摘要、`.smoke.json`、`shared-versions-aarch64-apple-darwin.json` 和 `desktop-release-v0.2.0.json`。清单明确 `complete:false`、`development_only:true`；其 URL 是发布位置合同，不能声称本地开发包已上传。

## 回归与排障

已完成的门禁：workspace all-targets check、all-targets Clippy（`-D warnings`）、七程序构建、fmt/diff check；发布/基准脚本 112 项、Terminal-Bench adapter 37 项、TypeScript SDK 9 项、Python SDK 14 项通过。原生包和 npm 包真实 smoke 通过，SDK schema 的语义与仓库一致。

Rust 常规回归覆盖 1,815 项：原全仓运行 1,814 通过、1 项 PTY 失败；修正该测试的分块读取同步后，完整并发 PTY 组 15/15 通过。随后独立 `cargo test --workspace --locked --doc` 退出 0，所有文档目标通过。这里按失败目标复测后的覆盖统计，不宣称原始全仓命令一次通过，也不重复累计复跑用例。3 项默认 ignored 分别为人工 restart soak、显式 live driver smoke、由其他进程测试另行调用的 verifier helper。

首轮全仓测试的 OAuth 本地 fixture 出现 9 项超时，宿主设置了 HTTP/HTTPS 代理。隔离进程代理并设置 localhost NO_PROXY 后，auth 24 项复跑通过。保留首轮失败，不通过延长生产超时或降低断言掩盖。

一次新 smoke 误用 `thread list` 断言非法项目配置必然失败；检查代码确认该诊断命令不加载模型设置。已改为 `exec`，并要求错误明确包含新配置路径，防止以其他失败误判隔离成功。

全仓并发 PTY 组首次有 1 项草稿可见性断言失败，CI 串行条件下该用例通过。检查发现测试等到回复标记出现后立即断言输入框已画完，未覆盖 PTY 分块读取。测试改为使用已有的有界可见性等待检查精确草稿内容；不改 TUI 生产逻辑，不降低请求数量、合并内容或草稿不发送的断言。

PTY 原串行组 15/15 通过；同步修正后并发组 15/15 通过。重建 PTY 时另一个 Cargo 文档检查恰好引用正在重建的 client 库，产生一次 `extern location ... does not exist`；同一 target 的构建与文档验收改为顺序执行，失败记录不删除。部分首次启动还在 macOS dyld 中停留较久，采样显示尚未进入测试函数，未通过改变产品超时规避。

命名检查特别区分 Cargo package 与 binary、Agent 自有环境变量与桌面 IPC 变量、上游域名与本地数据目录。机械替换后用全目标编译、脚本、实际原生包验证，防止路径字符串和 Python 属性名被误改。

## 未验证范围

Windows x64/ARM64、macOS x64、Linux x64/ARM64 本轮未实机运行。六目标打包逻辑有自动化测试和对应 CI，但不能代替运行验收。未验证实际桌面安装器、ConPTY、签名、升级和卸载 UI；本轮未调用真实云端模型，使用 mock 与本地 HTTP fixture。截至上述验收时间，尚未 commit、push 或发布。

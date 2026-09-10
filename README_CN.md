<div align="center">
  <img src="assets/readme/golutra-logo.png" alt="Golutra 标志" width="128" />
  <h1>Golutra Agent</h1>
  <p><strong>简单操作、可靠后台、完整可观测的 Coding Agent。</strong></p>

  <p>
    <a href="https://github.com/golutra/golutra-agent/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/golutra/golutra-agent/ci.yml?branch=main&label=CI" alt="CI 状态" /></a>
    <a href="https://github.com/golutra/golutra-agent/releases"><img src="https://img.shields.io/github/v/release/golutra/golutra-agent?label=release" alt="最新版本" /></a>
    <a href="https://github.com/golutra/golutra-agent/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-2ea44f" alt="Apache-2.0 许可证" /></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.93%2B-f74c00" alt="Rust 1.93 或更新版本" /></a>
  </p>

  <p>
    <a href="README.md">English</a> ·
    <a href="README_CN.md">中文</a> ·
    <a href="docs/README.md">文档</a> ·
    <a href="CONTRIBUTING.md">贡献</a> ·
    <a href="SECURITY.md">安全</a> ·
    <a href="NOTICE">声明</a> ·
    <a href="https://github.com/golutra/golutra-agent/releases">发布</a>
  </p>
</div>

<p align="center">
  <img src="assets/readme/publicity_CN.png" alt="Golutra Agent 工作区" width="898" />
</p>

## 中文

Golutra Agent 是一个让目标直接变成可用代码的简单、local-first Coding Agent。安装后运行
`golutra`，用自然语言说明结果；Agent 会查看当前工作区、修改文件、运行检查，并如实反馈
发生了什么，不要求用户先学习命令目录。

它让人的操作保持简单，也让模型上下文保持聚焦：只需说明一次目标，由 Agent 自己选择必要
动作，最后得到有真实工作区证据支撑的清晰结果。

日常路径保持短小：

- **两个主要入口**：`golutra` 打开交互式 TUI；`golutra exec` 用于脚本和 CI 的无界面执行。
- **减少重复上下文**：默认 `coding` 工具面、有限上下文和稳定的 provider 前缀，减少无效往返，让 token 用在任务本身。
- **后台继续工作**：后台 shell session 和相互独立的 session 可以并行，受主机资源与策略限制；同一 session 内仍按顺序推进任务。
- **需要时完整可观测**：普通界面只展示进度和结果；显式 debug、JSON、run bundle 视图提供事件、token、工具和验证事实。
- **模型自主决策**：Agent 可以查看真实回合、后台进程、checkpoint 和验证状态，据此调整下一步；Runtime 保留失败证据，不伪造成功，也不跳过必要检查。

### 为什么选择 Golutra

一个有能力的模型只是好用 Coding Agent 的一部分。Golutra 用小而可靠的执行循环承接开放式
模型，让用户既能享受对话速度，也不会失去证据、恢复能力和控制权。

整个工作流保持聚焦：

- **只说目标**：模型负责规划和选择工具，用户不必按清单手动编排命令。
- **Token 用在任务上**：紧凑工具、有界上下文和稳定前缀减少重复输入，同时保留模型 reasoning 能力。
- **工作持续推进**：后台进程和独立 session 可与交互任务并行，生命周期和取消边界明确。
- **需要时看全貌**：普通界面保持易读，token、工具结果、事件和验证事实可按需查看。
- **以证据结束**：文件、命令、检查和失败都会被记录，完成不等于只得到一句自信的回复。

### 可治理观测链路

```text
用户输入
  -> Session Command Protocol
  -> RuntimeEvent 事实账本 + StateProjection
  -> Runtime OS control loop
  -> ModelInputEnvelope
  -> Provider / Tool loop
  -> VerificationRecord + LoopDecision
  -> User / Debug / Context 审计 / Evaluation 投影
```

这条链路把三类责任硬分离：Runtime control plane 管 session、turn、工具、副作用、
预算和终态；model boundary 只允许经过审批的消息与工具定义进入 provider request；
observation/governance plane 保存事实、artifact 和完整性结果，再按用途生成不同投影。

因此，对话 transcript 只是持久事实的一种用户视图。Debug 与治理信息不会因为“文本可读”
就自动回灌给模型；普通用户也不需要承受完整审计链路的噪声。需要排查时，系统仍能回答
模型当时看到了什么、工具实际做了什么、证据是否完整，以及任务为什么被判定为当前终态。

### 从事实到受治理改进

```text
任务执行
  -> RuntimeEvent 与 Evidence
  -> VerificationRecord
  -> 持久化任务后复盘
  -> ImprovementCandidate
  -> baseline/candidate 配对回归
  -> PromotionDecision
```

改进候选必须携带证据、风险、验证计划和回滚信息。缺失完整 trace 或配对执行时，结果保持
`NeedsReview`，不会把“没测到”解释为通过。runtime code、policy、sandbox 和兼容性等高风险
变更必须经过人工审查；普通 Runtime 无权自行发布新的 stable runtime。

### 快速运行

#### 日常使用

发布的 npm 启动器只需要 Node.js `18` 或更新版本，不需要 Rust 工具链：

```bash
npm install -g @golutra/agent
golutra
golutra exec "检查当前工作区并运行测试"
```

无参数执行 `golutra` 会进入 TUI。直接描述目标即可，Agent 自己判断需要哪些读取、修改、命令和检查；后台 shell session 会持续运行，TUI 在终态时展示真实结果。脚本或 CI 使用 `golutra exec`，需要结构化输出时加 `--json`：

```bash
golutra exec --json "总结当前改动"
```

默认交互使用紧凑的 `coding` 工具面和有界上下文；只有任务确实需要低频扩展时才使用
`--tool-profile full`，它不会改变模型的 reasoning 设置。普通界面保持简洁；显式 JSON、debug
和 run bundle 视图可用于查看 token、工具、事件和验证详情。

TUI 可以引导首次 provider 配置。非敏感默认值可存放在
`$GOLUTRA_HOME/runtime.json`（全局）或 `<workspace>/.golutra/runtime.json`（项目）；项目值覆盖全局值，session 控制只在内存中生效，显式参数优先。凭据只保存在 owner-only 凭据存储或环境引用中。

#### 从源码构建

源码构建需要 Rust `1.93` 或更新版本；Python `3.11` 用于发布检查和 Python SDK，Node.js `22` 用于 TypeScript SDK。

```bash
git clone https://github.com/golutra/golutra-agent.git
cd golutra-agent
cargo run -p golutra-tui
```

也可以运行一次性任务或本地 app-server：

```bash
cargo run -p golutra-cli -- chat "检查当前工作区"
cargo run -p golutra-cli -- --cwd "$PWD" exec "运行测试"
cargo run -p golutra-app-server -- --addr 127.0.0.1:47831
```

通过 Cargo 传递 TUI 参数时，需要在程序参数前加分隔符：

```bash
cargo run -p golutra-tui -- --yolo
```

`cargo run -p golutra-tui --yolo` 会被 Cargo 自己解析，因此会报
`unexpected argument '--yolo'`。

#### 发布与维护

根 npm 包只负责选择当前平台的原生包，安装过程不运行联网下载脚本；`golutra-tui` 仍是显式 TUI 别名。

当前 release workflow 发布 Linux x64/arm64、macOS x64/arm64 和 Windows x64/arm64。app-server、观测、supervisor 与 evaluation 入口仍随下面的完整平台归档分发。

### 代码、文档与贡献

- 架构总览：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- 观测与评估：[docs/evaluation-observability.md](docs/evaluation-observability.md)
- 改进闭环：[docs/agent-improvement-loop.md](docs/agent-improvement-loop.md)
- 文档索引：[docs/README.md](docs/README.md)
- 运行入口：[docs/runtime-entrypoints.md](docs/runtime-entrypoints.md)
- 贡献指南：[CONTRIBUTING.md](CONTRIBUTING.md)
- 安全策略：[SECURITY.md](SECURITY.md)
- 变更记录：[CHANGELOG.md](CHANGELOG.md)

项目当前处于 `0.2.0` 早期阶段，协议和运行时边界仍可能演进。欢迎提交代码、
测试、文档和可复现的 issue；涉及凭据、沙箱、网络或数据泄露的问题请按安全策略
私下报告。

本项目采用 [Apache License 2.0](LICENSE)。

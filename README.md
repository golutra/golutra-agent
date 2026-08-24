<div align="center">
  <img src="assets/readme/golutra-logo.png" alt="Golutra logo" width="128" />
  <h1>Golutra Agent</h1>
  <p><strong>A governable agent harness for coding.</strong><br />面向 Coding Agent 的可治理执行框架。</p>

  <p>
    <a href="https://github.com/golutra/golutra-agent/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/golutra/golutra-agent/ci.yml?branch=main&label=CI" alt="CI status" /></a>
    <a href="https://github.com/golutra/golutra-agent/releases"><img src="https://img.shields.io/github/v/release/golutra/golutra-agent?label=release" alt="Latest release" /></a>
    <a href="https://github.com/golutra/golutra-agent/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-2ea44f" alt="Apache-2.0 license" /></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.93%2B-f74c00" alt="Rust 1.93 or newer" /></a>
  </p>

  <p>
    <a href="#english">English</a> ·
    <a href="#中文">中文</a> ·
    <a href="docs/README.md">Docs</a> ·
    <a href="CONTRIBUTING.md">Contributing</a> ·
    <a href="SECURITY.md">Security</a> ·
    <a href="NOTICE">Notice</a> ·
    <a href="https://github.com/golutra/golutra-agent/releases">Releases</a>
  </p>
</div>

<p align="center">
  <img src="assets/readme/golutra-concept-hero.png" alt="Golutra Agent coding workspace" width="898" />
</p>

Golutra Agent is a Rust-first, local-first agent harness for coding. An LLM
generates tokens; Golutra turns those tokens into a durable, typed execution
loop that can use tools, survive interruption, prove outcomes, and expose the
right level of detail to each consumer.

The design goal is a **governable Runtime OS**: every meaningful execution step
becomes a `RuntimeEvent`, model input crosses an explicit `ModelInputEnvelope`
boundary, and task completion is decided from a `VerificationRecord` rather
than the model's own claim. The same facts can then drive the user interface,
debugging, replay, evaluation, and controlled improvement without mixing those
concerns into the conversation.

> Status: `0.1.0` is an early, actively evolving release. Runtime and protocol
> APIs may change before a stable compatibility policy is published.

## English

### Why an Agent Harness

A capable model is only one part of a coding agent. The harness determines what
the model sees, which actions it may take, when the loop stops, how failures are
recovered, what the user sees, and what can be proved afterward. Those runtime
choices often decide whether the same model behaves like a chatbot or a useful
engineering agent.

Golutra keeps the model's problem solving open-ended while making the execution
boundary dependable:

- **Durable execution:** sessions, commands, events, checkpoints, cancellation,
  recovery, and replay share one lifecycle across processes and clients.
- **Governed tools:** shell, files, code intelligence, MCP, delegation, and
  managed processes pass through explicit policy and result contracts.
- **Provider independence:** adapters normalize streaming, authentication,
  fallback, tool calls, and usage without changing the runtime loop.
- **Evidence-backed completion:** tool evidence, objective assertions, and
  policy results determine whether a workspace task passed, partially
  completed, failed, or remains unknown.
- **Typed public surfaces:** the TUI, CLI, app-server, Rust host, Python SDK,
  TypeScript SDK, and external drivers use one generated protocol contract.

### Runtime Model

```text
User input
  -> Session Command Protocol
  -> RuntimeEvent ledger + StateProjection
  -> Runtime OS control loop
  -> ModelInputEnvelope
  -> Provider / Tool loop
  -> VerificationRecord + LoopDecision
  -> User / Debug / Context-audit / Evaluation projections
```

This flow is split into three planes with different responsibilities:

| Plane | Responsibility |
| --- | --- |
| Runtime control | Owns sessions, lanes, turns, tools, verification, budgets, and terminal state. |
| Model boundary | Compiles only approved messages and tool definitions into the provider request. |
| Observation and governance | Preserves facts and artifacts, checks trace integrity, and builds purpose-specific projections. |

The separation is a security and correctness boundary, not just a UI choice.
Debug or governance records do not automatically become model context, and a
conversation transcript is only one projection of the durable facts.

### From Evidence to Improvement

Golutra can turn a failed or partial task into a controlled improvement path:

```text
Task execution
  -> RuntimeEvent and evidence
  -> VerificationRecord
  -> durable post-task review
  -> ImprovementCandidate
  -> paired regression
  -> PromotionDecision
```

Candidates carry evidence, risk, verification, and rollback information.
Incomplete traces or missing baseline/candidate pairs stay in review instead
of being treated as a pass. High-risk changes, including runtime code, policy,
sandbox, and compatibility changes, require human review; the normal runtime
cannot publish a new stable runtime by itself.

### Quick Start

Prerequisites:

- Rust `1.93` or newer
- Python `3.11` or newer for release checks and the Python SDK
- Node.js `18` or newer for the npm launcher; Node.js `22` or newer for the TypeScript SDK
- A configured provider (the TUI can guide first-time setup)

Run the TUI from a checkout:

```bash
git clone https://github.com/golutra/golutra-agent.git
cd golutra-agent
cargo run -p golutra-tui
```

Run a one-shot command or the local app-server:

```bash
cargo run -p golutra-cli -- chat "inspect this workspace"
cargo run -p golutra-cli -- --cwd "$PWD" exec "run the checks"
cargo run -p golutra-app-server -- --addr 127.0.0.1:47831
```

#### Install the CLI and TUI with npm

The published npm package is a lightweight launcher. npm resolves the matching
native package for the host platform, so no Rust toolchain or install-time
network download script is required:

```bash
npm install -g @golutra/agent
golutra
golutra --help
```

`golutra` opens the interactive TUI when no arguments are supplied. Use
`golutra exec "..."` for a headless turn in scripts or CI; `golutra-tui` remains
available as an explicit TUI alias.

TUI defaults can be kept without storing secrets in either
`$GOLUTRA_HOME/runtime.json` (global) or `<workspace>/.golutra/runtime.json`
(project). Project values override global values, session controls are
in-memory, and explicit `--execution-mode`/`--tool-profile` flags win over the
files. The accepted non-secret fields are `provider_profile`, `model`,
`execution_mode`, `verify_on_change`, `tool_profile`, and `reasoning_effort`.
New interactive turns expose the compact `coding` tool surface and keep
verification-on-change disabled unless enabled explicitly; pass
`--tool-profile full` when a task needs low-frequency extensions. Unknown
fields and secret-shaped values are rejected; credentials stay in the
owner-only credential store or environment references.

The current release workflow publishes Linux x64/arm64, macOS x64/arm64, and
Windows x64/arm64 native packages. The npm distribution contains the
interactive TUI and scriptable CLI; app-server, trace/observability,
supervisor, and evaluation entry points remain in the platform release archive
below.

Maintainers can build the npm artifacts locally from a release target:

```bash
python3 scripts/package_npm.py --package platform \
  --target aarch64-apple-darwin \
  --binary-dir target/aarch64-apple-darwin/release
python3 scripts/package_npm.py --package root \
  --targets aarch64-apple-darwin
```

Cargo consumes its own options before forwarding arguments to the binary. Use
the separator when passing TUI flags; `cargo run -p golutra-tui --yolo` is a
Cargo argument error, while this is correct:

```bash
cargo run -p golutra-tui -- --yolo
```

Provider setup and credential storage are documented in
[`docs/llm-provider-integration.md`](docs/llm-provider-integration.md).

### Clients and Protocol

All clients use the same command/query/event vocabulary:

```text
TUI / CLI / SDK / remote client
              |
       app-server or embedded host
              |
       RuntimeHost + AgentHarness
        /       |        \
   provider   tools    durable store
              |
       typed events, traces, and verification
```

Useful entry points:

| Surface | Start here |
| --- | --- |
| Interactive terminal | `cargo run -p golutra-tui` |
| Scriptable CLI | `cargo run -p golutra-cli -- --help` |
| Local/remote service | [`docs/runtime-entrypoints.md`](docs/runtime-entrypoints.md) |
| TUI driver | [`docs/tui-driver.md`](docs/tui-driver.md) |
| Python SDK | [`sdk/python`](sdk/python) |
| TypeScript SDK | [`sdk/typescript`](sdk/typescript) |
| Architecture | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| Observability and evaluation | [`docs/evaluation-observability.md`](docs/evaluation-observability.md) |
| Improvement loop | [`docs/agent-improvement-loop.md`](docs/agent-improvement-loop.md) |
| Versioned schema | [`schemas/sdk-protocol.schema.json`](schemas/sdk-protocol.schema.json) |

### Build and Verify

The repository keeps generated protocol clients checked in. Run the relevant
checks before opening a pull request:

```bash
just fmt-check
just clippy
just test
just schema
just ts-check
just py-check
just open-source-check
just release-package-smoke
```

Live provider tests and external benchmark runs are opt-in. They are not
required for an ordinary contribution and must never read a contributor's
normal credentials implicitly.

### Release Archives

Build a reproducible archive for the current host:

```bash
python3 scripts/package_release.py --output-dir dist
python3 scripts/package_release.py --verify dist/golutra-agent-v*-*.tar.gz
```

Unix targets produce `.tar.gz`; Windows targets produce `.zip`. Each archive
has a SHA-256 sidecar, an external manifest, and the same manifest inside the
archive. The archive also includes `LICENSE` and `NOTICE` so a binary release
retains its legal notices. Tag releases must match the workspace version and
are built by [`.github/workflows/release.yml`](.github/workflows/release.yml).

### Contributing and Security

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before submitting a change. Please
use the issue forms for bugs and feature requests, and use
[`SECURITY.md`](SECURITY.md) for vulnerabilities instead of posting sensitive
details in a public issue.

Golutra Agent is distributed under the [Apache License 2.0](LICENSE).
Contributions are accepted under the same license as described in Section 5;
there is no separate CLA requirement in this repository at present. See
[NOTICE](NOTICE) for dependency and README asset notices.

## 中文

Golutra Agent 是一个 Rust-first、local-first 的 Coding Agent Harness。模型负责开放式
思考与生成，Golutra 负责把这些 token 变成可执行、可恢复、可验证的工程任务。

### 为什么是 Agent Harness

模型本身只会生成 token。一个真正可用的 Coding Agent 还需要回答：模型每轮看到什么、
可以做什么、循环何时停止、失败如何恢复、用户看到什么，以及事后能够证明什么。同一个
模型在不同 Harness 中会表现出明显差异，因为上下文纪律、工具契约和完成判定同样决定
Agent 的能力上限。

Golutra 的目标不是把更多控制逻辑写进 Prompt，而是提供一个**可治理的 Runtime OS**：

- **持久执行**：session、command、event、checkpoint、取消、恢复和 replay 使用同一套生命周期；
- **受治理工具**：shell、文件、代码索引、MCP、子代理和托管进程经过统一 policy 与结果契约；
- **Provider 解耦**：流式输出、认证、fallback、tool call 和 token usage 在适配层归一化；
- **证据判定完成**：工作区任务由工具证据、目标断言和策略结果共同判定，不采信模型自述；
- **统一协议**：TUI、CLI、app-server、Rust host、Python SDK、TypeScript SDK 和外部驱动共享
  command/query/event 契约。

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

```bash
git clone https://github.com/golutra/golutra-agent.git
cd golutra-agent
cargo run -p golutra-tui
```

如果要传递 TUI 参数，必须使用 Cargo 与程序参数之间的分隔符：

```bash
cargo run -p golutra-tui -- --yolo
```

`cargo run -p golutra-tui --yolo` 会被 Cargo 自己解析，因此会报
`unexpected argument '--yolo'`。

也可以直接通过 npm 安装 CLI 和 TUI。根包只负责选择当前平台的原生包，安装过程不运行
联网下载脚本：

```bash
npm install -g @golutra/agent
golutra
golutra --help
```

无参数执行 `golutra` 会进入交互式 TUI；脚本和 CI 使用
`golutra exec "..."` 保持无界面执行，`golutra-tui` 仍作为显式 TUI 别名保留。

TUI 的非敏感默认配置按 `$GOLUTRA_HOME/runtime.json` →
`<workspace>/.golutra/runtime.json` → 当前 session 内存覆盖合并；显式
`--execution-mode` 和 `--tool-profile` 参数优先级最高。配置文件不允许未知
字段或 secret，key/token 仍只通过 credentials 文件或环境引用提供。

当前 release workflow 发布 Linux x64/arm64、macOS x64/arm64 和 Windows x64/arm64。app-server、
观测、supervisor 与 evaluation 入口仍随下面的完整平台归档分发。

### 代码、文档与贡献

- 架构总览：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- 观测与评估：[docs/evaluation-observability.md](docs/evaluation-observability.md)
- 改进闭环：[docs/agent-improvement-loop.md](docs/agent-improvement-loop.md)
- 文档索引：[docs/README.md](docs/README.md)
- 运行入口：[docs/runtime-entrypoints.md](docs/runtime-entrypoints.md)
- 贡献指南：[CONTRIBUTING.md](CONTRIBUTING.md)
- 安全策略：[SECURITY.md](SECURITY.md)
- 变更记录：[CHANGELOG.md](CHANGELOG.md)

项目当前处于 `0.1.0` 早期阶段，协议和运行时边界仍可能演进。欢迎提交代码、
测试、文档和可复现的 issue；涉及凭据、沙箱、网络或数据泄露的问题请按安全策略
私下报告。

本项目采用 [Apache License 2.0](LICENSE)。

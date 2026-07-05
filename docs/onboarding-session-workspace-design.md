# Onboarding、Session 与 Workspace 设计

## 文档定位

本文档回答三个实现问题：

- Golutra 首次进入是否应该要求用户登录或输入 key。
- LLM provider 凭据、配置和运行时状态应该持久化在哪里。
- resume、多 session、多工作区应该如何设计，避免 CLI/TUI/Web 各自维护状态。

结论：Golutra 已具备 provider onboarding 与 thread resume/fork 的最小闭环。默认仍可使用 `mock` provider；真实 provider 可通过 `golutra provider login` 写入用户级或 workspace 级 `provider.json`，运行时会把该配置与进程环境变量合并后解析。TUI 首屏会展示 provider 状态和缺失字段，但完整交互式 provider setup 表单仍是后续增强。

## qwen-code 参考结论

参考路径：`/Users/skyseek/Desktop/project/open/golutra-agent/project/qwen-code`。

### 首次认证触发

qwen-code 的 CLI 认证状态由 `config.getAuthType()` 决定：

- 未设置 auth type 时，`useAuthCommand` 将 `isAuthDialogOpen` 初始化为 `true`。
- `AuthDialog` 首屏不是直接输入 key，而是先选择 provider 分组：Alibaba ModelStudio、Third-party Providers、Custom Provider。
- provider setup 是配置驱动流程，步骤包括 protocol、baseUrl、apiKey、models、advancedConfig、review。
- Escape 不能绕过首次认证；没有 auth type 时会提示必须 connect provider。

Golutra 应吸收这个体验：首次进入 TUI 时，如果没有可用 live provider，不直接进入空白聊天界面，而是展示 provider setup。CLI 仍要支持脚本模式，因此 `golutra chat` 默认 mock 不应被交互弹窗阻塞；但 `golutra tui`、Web 和显式 `golutra provider login` 应进入 setup flow。

### 配置和凭据持久化

qwen-code 的关键路径：

| 类型 | qwen-code 路径 |
| --- | --- |
| 全局目录 | `QWEN_HOME`，否则 `~/.qwen` |
| 运行时目录 | `QWEN_RUNTIME_DIR`，否则 settings 指定目录，否则全局目录 |
| 全局设置 | `~/.qwen/settings.json` |
| workspace 设置 | `<workspace>/.qwen/settings.json` |
| OAuth 文件 | `~/.qwen/oauth_creds.json` |
| MCP OAuth | `~/.qwen/mcp-oauth-tokens.json` |
| 项目运行数据 | `<runtimeBase>/projects/<sanitized-cwd>` |

qwen-code 的 settings 写入要点：

- `settings.json` 允许 JSONC 风格读入，但写出为标准 JSON。
- 写入采用 temp file + rename，避免半写文件。
- 文件权限收紧为 owner-only，避免 API key 短暂暴露。
- `ProviderInstallPlan` 一次性描述 env、modelProviders、authType、legacyCredentials、modelSelection 和 providerState。
- 应用 install plan 时先备份 settings 和 process env；persist、reload、refreshAuth 任一步失败都会 rollback settings、env 和 runtime provider registry。
- 禁止 install plan 写 `NODE_OPTIONS`、`LD_PRELOAD`、`PATH`、`HOME` 等进程劫持 env。

Golutra 不应照搬把明文 key 默认写入 workspace。推荐持久化分层：

| 类型 | Golutra 路径 | 允许内容 |
| --- | --- | --- |
| 全局 home | `GOLUTRA_HOME`，否则 `~/.golutra` | 用户配置、secret ref、provider catalog |
| 全局 provider 配置 | `$GOLUTRA_HOME/provider.json` | provider catalog、默认 selection、envKey、secretRef、脱敏 fingerprint |
| workspace provider 配置 | `<workspace>/.golutra/provider.json` | 团队可共享 provider catalog、默认 model、envKey 名称；禁止默认保存 secret value |
| workspace runtime | `<workspace>/.golutra/runtime.sqlite` | session/task/event/projection/thread index，不保存明文 secret |
| session rollouts | `<workspace>/.golutra/sessions/YYYY/MM/DD/*.jsonl` 或 `$GOLUTRA_HOME/sessions/...` | append-only 历史，已脱敏 provider payload |
| secrets | OS keychain 或用户显式 opt-in 的 `$GOLUTRA_HOME/secrets.json` | 明文 key/token，仅 owner-only 权限；workspace 禁止 |

P1 可以先实现 env-api-key：用户输入 key 后，默认只写到 user-level provider 配置的 `env` 或 keychain；如果选择写入文件，必须提示保存位置，并保证 owner-only 权限和原子写。

## Codex 参考结论

参考路径：`/Users/skyseek/Desktop/project/open/golutra-agent/project/codex`。

### Thread / session 分层

Codex SDK 暴露 `startThread()` 和 `resumeThread(id)`；文档明确 thread 持久化在 `~/.codex/sessions`。Rust 实现里，完整历史主要是 rollout JSONL，SQLite state 维护 thread 元数据和列表索引。

核心设计点：

- thread 是用户可恢复的会话单位，不等同于单个 turn。
- rollout JSONL 是可重建历史；SQLite 是索引、列表、状态和查询加速。
- thread 元数据包含 `id`、`rollout_path`、`created_at`、`updated_at`、`recency_at`、`source`、`model_provider`、`model`、`cwd`、`title`、`preview`、sandbox、approval、tokens、git info、archived 状态。
- thread/list 支持分页 cursor、cwd 过滤、model provider 过滤、source 过滤、archived 过滤、search、parent/ancestor 关系。
- TUI resume picker 默认按当前 cwd 过滤，可切换 All；支持 Resume 与 Fork。
- resume 对正在运行的 thread 走 listener attach，避免重建；对已卸载 thread 从 rollout/history 重建并重新创建 runtime。
- fork 从已有 rollout 截断或复制历史，生成新的 thread；可以继承名称和配置，但拥有独立运行时和 rollout。
- app-server 线程 listener 有卸载延迟：无订阅且非 active 一段时间后卸载，避免长期占用内存。

Golutra 应吸收 thread 视图，而不是继续只有 workspace 默认 session。`SessionId` 可以保留为 runtime lane scope，但用户入口应面向 `ThreadId` 或 `ConversationId`，并允许一个 workspace 有多个 thread。

## Golutra 目标模型

### Provider onboarding gate

新增 `ProviderOnboardingState`：

```text
ProviderOnboardingState
  configured: bool
  active_protocol
  active_provider_id
  active_model_id
  credential_status: missing | present | invalid | expired | unsupported
  setup_required_reason
  suggested_actions
```

入口行为：

| 入口 | 没有 live provider 时 |
| --- | --- |
| `golutra tui` | 打开 provider setup；允许选择 Continue with mock |
| Web | 打开 connect provider modal；允许跳过到 mock workspace |
| `golutra provider current` | 输出脱敏诊断，不交互 |
| `golutra provider login` | 强制进入交互式 setup |
| `golutra chat` | 默认继续 mock；如果设置了 live protocol 但缺 key，返回结构化错误 |
| CI / 非 TTY | 不弹窗，返回 missing env 或 adapter_not_implemented |

Provider setup 步骤：

1. 选择 provider 分组：OpenAI-compatible、Anthropic、Gemini/Vertex、Custom endpoint。
2. 选择协议：`openai-compatible`、`anthropic`、`gemini`、`vertex-ai`、`genai`。
3. 输入或选择 baseUrl。
4. 输入 API key，或选择 envKey / secretRef。
5. 输入 model id 列表，默认选第一个。
6. probe；成功后应用配置，失败则保留表单但不污染 active selection。

应用配置必须走 `ProviderInstallPlan` 等价结构：

```text
ProviderInstallPlan
  provider_id
  protocol
  selection: model_id + base_url
  credential: env_key | secret_ref | inline_secret_once
  provider_catalog_patch
  user_config_patch
  workspace_config_patch
  runtime_reload
  probe_policy
  rollback_snapshot
```

实现约束：

- `inline_secret_once` 只能在 install plan 内存中出现，不能进入 runtime event。
- workspace 配置默认只写 envKey/secretRef，不写 secret value。
- 写入 user config 或 secrets 必须 temp file + rename，并收紧权限。
- refresh/probe 失败必须 rollback active selection 和 runtime provider registry。
- 未实现 adapter 的协议可以被保存为 catalog entry，但不能被设为 ready active provider。

### Session / thread 存储

建议新增用户可见 `ThreadId`，保持已有 `SessionId` 作为 runtime lane 和协议兼容字段：

```text
WorkspaceId
  -> ThreadId
     -> SessionId
        -> TaskId
           -> TurnId
```

存储建议：

| 表/文件 | 作用 |
| --- | --- |
| `threads` | 用户可见会话列表和 resume/fork 元数据 |
| `runtime_events` | 现有 event log，继续作为 runtime 事实 |
| `thread_events` 或 rollout JSONL | append-only transcript/replay 历史 |
| `thread_spawn_edges` | future multi-agent / fork / child agent 关系 |
| `.golutra/default-thread` | 当前 workspace 最近活跃 thread，替代单一 default-session 心智 |
| `.golutra/default-session` | 兼容旧入口，后续由 default-thread 派生 |

`threads` 最小字段：

```text
thread_id
workspace_id
session_id
rollout_path
created_at
updated_at
recency_at
source: cli | tui | web | sdk | app_server
provider_id
protocol
model_id
cwd
title
preview
status
archived_at
git_sha
git_branch
git_origin_url
last_task_id
tokens_used
```

索引：

- `(workspace_id, archived_at, recency_at desc, thread_id desc)`
- `(cwd, archived_at, recency_at desc, thread_id desc)`
- `(provider_id, archived_at, recency_at desc)`
- `(session_id)`

### Resume / fork 语义

新增命令语义：

```bash
golutra thread list [--workspace PATH] [--all-workspaces] [--provider ID] [--archived] [--cursor CURSOR]
golutra resume [THREAD_ID] [--workspace PATH]
golutra fork THREAD_ID [--from-turn TURN_ID]
golutra tui --resume THREAD_ID
golutra tui --picker
```

Resume 规则：

- 如果 thread 正在当前 RuntimeHost 内运行：只 attach listener，不重建 AgentLoop。
- 如果 thread 已卸载但 rollout/runtime_events 完整：重建 projection、恢复 session lane，并等待用户下一次 turn。
- 如果只有 SQLite 元数据没有 rollout：允许只展示摘要，但继续任务前要求 repair 或报错。
- 如果 cwd 不存在：进入 blocked resume，允许用户选择新 cwd。
- 如果用户提供 model/provider/cwd override：记录 `ResumeOverrideApplied`，同时保留原 thread metadata。

Fork 规则：

- fork 创建新 `thread_id` 和新 rollout。
- 默认复制历史到指定 turn；不复制 active tool 状态。
- fork 可继承 provider/model/cwd，也可覆盖。
- fork 与 parent 建 `thread_spawn_edges`，用于 UI 展示关系。

### 多工作区

Golutra 当前 workspace SQLite 适合 P0，但多工作区需要全局索引：

```text
$GOLUTRA_HOME/index.sqlite
  workspaces(workspace_id, canonical_path, last_seen_at, default_thread_id)
  threads(thread_id, workspace_id, cwd, title, preview, recency_at, provider_id, model_id, archived_at)
```

原则：

- workspace 内 `.golutra/runtime.sqlite` 仍是该 workspace 的事实来源。
- `$GOLUTRA_HOME/index.sqlite` 只做跨工作区列表、最近使用、搜索入口和 repair 指针。
- 每次 workspace runtime 写入 thread 元数据后，异步 upsert 到全局 index。
- TUI resume picker 默认当前 workspace；按键切换 All workspaces。
- 全局 index 失效时可以从各 workspace `.golutra/runtime.sqlite` 和 rollouts 重建。

### P1 落地顺序

1. 增加 `golutra-config` provider user/workspace 配置读写，支持 owner-only 原子写。
2. 增加 `ProviderInstallPlan` 和 `provider login/use/set-key` CLI。
3. TUI 首次进入接 `ProviderOnboardingState`，支持 mock 跳过和 provider setup。
4. 增加 `threads` 表、`default-thread`、`thread list/resume/fork` 最小命令。
5. TUI 增加 resume picker：当前 workspace / all workspaces、resume / fork、预览 transcript。
6. 增加 `$GOLUTRA_HOME/index.sqlite`，支持跨 workspace 最近 thread 列表。
7. 将 app-server 协议扩展为 `thread/start`、`thread/list`、`thread/resume`、`thread/fork`，CLI/TUI/Web 统一消费。

## 当前差距

- 当前 TUI 首屏会显示 provider onboarding 状态，但还没有 qwen-code 风格的多步骤交互式 AuthDialog。
- 当前 provider 配置已支持 `$GOLUTRA_HOME/provider.json` 和 `<workspace>/.golutra/provider.json`，workspace 配置禁止保存明文 key；但 OS keychain、OAuth 和 secret-ref 还未实现。
- 当前 `threads` 表、`.golutra/default-thread`、`golutra thread list`、`golutra resume [THREAD_ID]` 和 `golutra fork THREAD_ID` 已可用；fork 当前复制元数据并创建新 session，还未复制/截断 rollout JSONL 历史。
- 当前多工作区仍以 workspace SQLite 为事实来源；`$GOLUTRA_HOME/index.sqlite` 的跨 workspace 全局索引还未实现。
- 当前 TUI 没有 provider setup modal。
- 当前只有 workspace 默认 session，缺少用户可见 thread list/resume/fork。
- 当前 session 事实在 workspace SQLite，缺少 rollout JSONL 和跨 workspace index。

这些差距不影响当前 P0 mock/live OpenAI-compatible smoke，但会影响真实用户首次使用、恢复历史任务和多项目日常使用。

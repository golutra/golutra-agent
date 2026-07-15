# Onboarding、Session 与 Workspace 设计

## 文档定位

本文档回答三个实现问题：

- Golutra 首次进入是否应该要求用户登录或输入 key。
- LLM provider 凭据、配置和运行时状态应该持久化在哪里。
- resume、多 session、多工作区应该如何设计，避免 CLI/TUI/Web 各自维护状态。

结论：Golutra 已具备 provider onboarding、SecretRef/OAuth、thread resume、完整 fork、rollout export 和 cwd rebind 闭环。默认仍可使用 `mock` provider；真实 provider 通过全局 `$GOLUTRA_HOME/provider.json` v2 保存 selection 和 credential ref，运行时再从 owner-only `$GOLUTRA_HOME/credentials.json` 或进程 env 动态解析 secret。TUI 首次进入会检查 active provider profile；没有显式配置时打开 provider setup，支持 qwen-code 风格的 provider 分组、第三方 provider 选型、OpenAI-compatible/OpenAI Responses/Anthropic/Gemini/Vertex AI/genai 协议、base URL、disk/env 凭据、model/advanced config、保存前 review，或选择 mock provider。OpenAI、xAI、GitHub Copilot 会按 opencode 风格展示各自受审计的 browser/device OAuth 方法，Custom Provider 不推断 OAuth。已有 provider 时首屏不打断，输入 `/auth` 或 `/auth setup` 可随时重新打开并覆盖同名 profile；显式 `/auth oauth-login` 和 `/auth logout` 管理扩展 descriptor 与 OAuth token set。

## qwen-code 参考结论

参考路径：`/Users/skyseek/Desktop/project/open/golutra-agent/project/qwen-code`。

### 首次认证触发

qwen-code 的 CLI 认证状态由 `config.getAuthType()` 决定：

- 未设置 auth type 时，`useAuthCommand` 将 `isAuthDialogOpen` 初始化为 `true`。
- `AuthDialog` 首屏不是直接输入 key，而是先选择 provider 分组：Alibaba ModelStudio、Third-party Providers、Custom Provider。
- provider setup 是配置驱动流程，步骤包括 protocol、baseUrl、apiKey、models、advancedConfig、review。
- Escape 不能绕过首次认证；没有 auth type 时会提示必须 connect provider。

Golutra 应吸收这个体验：首次进入 TUI 时，如果没有可用 live provider，不直接进入空白聊天界面，而是展示 provider setup。CLI 仍要支持脚本模式，因此 `golutra chat` 默认 mock 不应被交互弹窗阻塞；`golutra tui` 和显式 `golutra provider login` 进入 setup flow。Web 首次 provider onboarding 不在当前产品范围，已有 Web attach 页面继续只消费 projection 和 event stream。

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
- Custom Provider 的 envKey 由 `(protocol, baseUrl)` 加 hash 后缀派生，避免 `api.example.com`、`api-example.com` 这类规范化碰撞覆盖同一份 key。

Golutra 不应照搬把明文 key 默认写入 workspace。推荐持久化分层：

| 类型 | Golutra 路径 | 允许内容 |
| --- | --- | --- |
| 全局 home | `GOLUTRA_HOME`，否则 `~/.golutra` | 用户配置、secret ref、provider catalog |
| 全局 provider/auth 配置 | `$GOLUTRA_HOME/provider.json` | provider catalog、默认 selection、`credential_ref`、随机 credential revision 和 OAuth 非敏感 metadata |
| 全局 runtime facts | `$GOLUTRA_HOME/state/runtime.sqlite` | 所有 cwd 的 session/task/event/projection/thread index，不保存明文 secret |
| cwd 分区状态 | `$GOLUTRA_HOME/state/workspaces/<cwd-hash>/` | checkpoint、memory、evaluation、rollout |
| thread rollouts | `$GOLUTRA_HOME/state/workspaces/<cwd-hash>/rollouts/<thread-id>.jsonl` | 从 SQLite facts 物化的 versioned、checksum、脱敏历史 |
| 全局凭据文件 | `$GOLUTRA_HOME/credentials.json`；CI 可使用进程 env | owner-only 明文 API key、OAuth access/refresh token set；项目目录和 provider config 禁止 secret |

当前实现使用 v2，并删除明文 `provider.json.env`：交互输入默认写 `$GOLUTRA_HOME/credentials.json`，CI/非交互模式可保存 env ref，profile 只保存 `credential_ref` 和非敏感 OAuth descriptor。凭据文件使用独立锁、大小上限和原子替换，Unix 下目录为 `0700`、文件为 `0600`。首次读取 v1 时在 provider settings lock 内把明文 env map 原子迁移到 disk SecretRef；失败会恢复 secret snapshot 并保留原配置，整个过程不访问 OS keychain。若显式 `/auth` 或 provider login 遇到已删除 backend 导致的不可读 JSON 配置，Review 会标明替换计划；保存成功后只保留新 profile，probe 失败则原样恢复旧文件和 secret snapshot，同样不会读取已删除 backend。

## Codex 参考结论

参考路径：`/Users/skyseek/Desktop/project/open/golutra-agent/project/codex`。

### Thread / session 分层

Codex SDK 暴露 `startThread()` 和 `resumeThread(id)`；文档明确 thread 持久化在 `~/.codex/sessions`。Rust 实现里，完整历史主要是 rollout JSONL，SQLite state 维护 thread 元数据和列表索引。

核心设计点：

- thread 是用户可恢复的会话单位，不等同于单个 turn。
- rollout JSONL 是可重建历史；SQLite 是索引、列表、状态和查询加速。
- thread 元数据包含 `id`、`rollout_path`、`created_at`、`updated_at`、`recency_at`、`source`、`model_provider`、`model`、`cwd`、`title`、`preview`、sandbox、approval、tokens、git info、archived 状态。
- thread/list 支持分页 cursor、cwd 过滤、model provider 过滤、source 过滤、archived 过滤、search、parent/ancestor 关系。
- Codex TUI resume picker 默认按当前 cwd 过滤并允许切换 All；Golutra 只采纳当前 cwd 的 Resume/Fork，不采纳 All Workspaces 入口。
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
| `golutra provider current` | 输出脱敏诊断，不交互 |
| `golutra provider login` | 强制进入交互式 setup |
| `golutra chat` | 默认继续 mock；如果设置了 live protocol 但缺 key，返回结构化错误 |
| CI / 非 TTY | 不弹窗，返回 missing env、无效 endpoint/model 或结构化 provider 错误 |

目标 Provider setup 步骤：

1. 选择 provider 分组：Golutra API、Third-party Providers、Custom Provider、mock fallback。
2. 选择 provider preset 或协议：当前 TUI 已覆盖 OpenAI-compatible preset，以及 `anthropic`、`gemini`、`vertex-ai`、`genai` 自定义协议。
3. 输入或选择 baseUrl；TUI 交互要求 `http://` 或 `https://` 开头。
4. 选择 Local disk 或 env ref；disk 模式输入脱敏 API key并写入 `$GOLUTRA_HOME/credentials.json`，env 模式只填写已有变量名。
5. 选择推荐 model，或输入自定义 model id。
6. 填写 advanced config：Thinking、Reasoning effort、Context window、Max output tokens。
7. review：展示脱敏 install plan、保存路径、scope、是否覆盖同名 profile。
8. probe；成功后应用配置，失败则保留表单但不污染 active selection。

应用配置必须走 `ProviderInstallPlan` 等价结构：

```text
ProviderInstallPlan
  provider_id
  protocol
  selection: model_id + base_url
  credential: env_key | secret_ref | inline_secret_once
  provider_catalog_patch
  user_config_patch
  global_config_patch
  runtime_reload
  probe_policy
  rollback_snapshot
```

实现约束：

- `inline_secret_once` 只能在 install plan 内存中出现，不能进入 runtime event。
- workspace `.golutra` 不参与 provider/auth 配置；provider envKey 和 secretRef 均属于全局用户配置，明文 secret 只能位于进程内存或 owner-only `$GOLUTRA_HOME/credentials.json`。
- 写入 user config 或 secrets 必须 temp file + rename，并收紧权限。
- refresh/probe 失败必须 rollback active selection 和 runtime provider registry。
- profile 只有在对应 runtime adapter 可用且 probe 成功后才能成为 enabled/ready active provider。

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
| rollout JSONL | 从 `runtime_events` 物化的 append/replay/export 历史 |
| `thread_spawn_edges` | future multi-agent / fork / child agent 关系 |
| cwd 查询索引 | 从全局 `threads(workspace_root, recency_at)` 选择当前 cwd 最近 thread；无历史时不写 placeholder |

当前实现同时保留 conversation projection 和完整 rollout：

- `runtime_events` 仍是事实来源，不新增旁路状态。
- `TaskCreated` 中的 `payload.prompt` 作为用户消息。
- `ToolCompleted` 的 `summary` 和 `changed_files` 作为用户可见工具结果摘要。
- `AssistantMessage` 持久化最终回复，`UserProjection.final_message` 从该事件 reducer 得到，TUI resume 后可恢复显示。
- 下一轮 prompt 构造 provider context 时，会从当前 session 历史事件提取用户输入、工具摘要、最终回复和任务终态，压缩成 `conversation_history` system contributor。这样 resume 后继续任务能看到前文关键上下文，但不会把完整 debug trace 塞回模型。
- `runtime_events` 是 canonical facts；每个持久化事件同步物化为 rollout envelope，包含格式版本、thread/session/sequence、脱敏后的完整事件和 SHA-256 checksum。
- rollout 单行上限 20 MiB，目录/文件分别使用 owner-only 权限；增量 append 与原子重建共享跨进程锁，启动和显式 export 可从 SQLite 修复缺失或陈旧文件。

rollout 是可删除、可重建的导出层，不反向覆盖 SQLite。这样保留 Codex 式可携带历史，同时不引入 SQLite/JSONL 双主真相。

`threads` 最小字段：

```text
thread_id
parent_thread_id
forked_from_turn_id
forked_from_sequence_no
workspace_id
session_id
rollout_path
rebound_from_workspace_root
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
golutra --cwd PATH thread list [--provider ID] [--archived] [--cursor CURSOR]
golutra --cwd PATH resume [THREAD_ID]
golutra fork THREAD_ID [--from-turn TURN_ID]
golutra tui --resume THREAD_ID
golutra tui --picker
```

Resume 规则：

- 如果 thread 正在当前 RuntimeHost 内运行：只 attach listener，不重建 AgentLoop。
- 如果 thread 已卸载但 SQLite history 完整：从 facts 重建 projection，按需重建 rollout，并等待用户下一次 turn。
- 如果 rollout 缺失或损坏：启动或显式 export 从 SQLite 原子重建，不让派生文件阻断 resume。
- CLI/TUI resume 永远要求 thread 属于当前 canonical cwd；不提供隐式跨 cwd 恢复。
- cwd 移动后必须在新 cwd 显式执行 `thread rebind THREAD_ID --from OLD_PATH`，不能用普通 resume 偷换执行边界。

Fork 规则：

- fork 创建新 `thread_id`、`session_id` 和 rollout；active parent 禁止 fork。
- 默认复制全部历史，`--from-turn` 截断到该 turn 最后一个事件；若截断投影仍 active，会追加 synthetic terminal boundary。
- SQLite 在一个事务中复制历史并重新生成 EventId/TaskId/TurnId，同时递归替换 payload 内相关 ID，父子后续写入互不影响。
- artifact/evidence blob 不复制，child event 保留 immutable artifact refs，DebugProjection 可按全局 ArtifactId 读取 lineage。
- fork 只继承当前 cwd；CLI/TUI 不允许借 fork 跨越 workspace 权限边界。

Rebind 规则：

- 只能由目标 cwd 的 RuntimeHost 发起，并显式提供数据库中精确匹配的旧 canonical cwd。
- active thread 或仍被其他 runtime 持有 session lease 的 thread 必须拒绝。
- 更新 thread cwd/rollout path 后，在新 cwd 分区重建 rollout 并记录 `ThreadRebound`；旧 rollout 删除。
- checkpoint 只作为 `historical_only` 证据保留；memory/evaluation 不盲目迁移，因为它们绑定原 cwd 的文件与评测语义。

### 多工作区

Golutra 当前直接使用一个全局事实库，不再维护“workspace DB + 全局二级 index”双写模型：

```text
$GOLUTRA_HOME/state/runtime.sqlite
  threads(thread_id, session_id, workspace_root, title, preview, recency_at, archived)
  runtime_events / projections / artifacts

$GOLUTRA_HOME/state/
  session-locks / command-locks

$GOLUTRA_HOME/state/workspaces/<cwd-hash>/
  checkpoints / memory / evaluation / rollouts
```

原则：

- canonical cwd 写入 thread 元数据，并作为执行权限与默认 resume 过滤边界。
- CLI/TUI 只列当前 canonical cwd；这是稳定的产品边界，不提供跨工作区 resume 入口。全局库只负责多 cwd 事实持久化、隔离和 daemon 路由。
- cwd hash 只用于文件分区，用户可见身份和历史仍以 canonical cwd、ThreadId、SessionId 为准。
- 项目删除或移动不会污染代码仓库；历史重绑定必须走显式 rebind，并保留旧路径审计信息。

### P1 落地状态

1. 增加 `golutra-config` 全局 user provider 配置读写，支持 owner-only 原子写。
2. 增加 `ProviderInstallPlan` 和 `provider login/use/set-key` CLI。
3. TUI 首次进入接 `ProviderOnboardingState`，支持 mock 跳过和 provider setup。
4. 增加全局 `threads` 表和按 cwd 的 `thread list/resume/fork` 最小命令。
5. TUI 增加当前 workspace resume picker：resume / fork、预览 transcript。
6. 使用 `$GOLUTRA_HOME/state/runtime.sqlite` 统一承载跨 cwd thread index。
7. 将 app-server 协议扩展为 `thread/start`、`thread/list`、`thread/resume`、`thread/fork`，CLI/TUI/Web 统一消费。
8. 增加 rollout export、指定 turn fork、cwd rebind 和跨 daemon restart 验收。以上本地 runtime/thread 项均已落地。

### TUI slash command 层

TUI 输入框现在先经过 slash command parser：

| 命令 | 行为 |
| --- | --- |
| `/help` | 在 transcript 中展示可用 slash commands |
| `/new` | 创建新的本地 thread/session，清空当前 TUI 可见状态，首个 prompt 时再持久化 |
| `/resume [thread-id]` | 无参数时打开当前 workspace session 列表；带 thread id 时恢复指定 thread 并切换当前 session |
| `/threads [limit]` | 列出当前 workspace 最近 threads |
| `/fork <thread-id> [--from-turn <turn-id>]` | fork 全部历史或截断到指定 turn，创建新 thread/session 并切换 |
| `/auth`、`/auth setup` | 打开 provider setup |
| `/auth status` | 展示 provider onboarding 状态 |
| `/auth protocols` | 展示注册 provider protocols |
| `/auth mock` | 将全局 provider 切换为 mock |
| `/auth login [--protocol <protocol>] --base-url <url> --model <model> [--api-key <key>\|--api-key-env <env>] [--store disk\|environment] [--enable-thinking] [--reasoning-effort low\|medium\|high\|xhigh] [--context-window-size <n>] [--max-tokens <n>] [--scope user]` | secret/config/probe 事务成功后保存 provider v2；交互 key 默认进入 credentials file |
| `/auth oauth-login --descriptor <json> --flow browser\|device --base-url <url> --model <model> [--profile <name>] [--protocol <protocol>]` | 在后台执行 PKCE/device OAuth、保存安全 token set并 probe 后激活 profile |
| `/auth logout [profile]` | revoke（provider 支持时）并删除本地 credential，禁用 profile；省略 profile 时退出 active profile |
| `/auth use <profile> [user]` | 激活已保存的全局 provider profile |
| `/status`、`/debug`、`/abort`、`/clear`、`/quit` | 本地状态、debug、abort 和退出控制 |

输入体验对齐 Codex：

- 输入 `/` 或 `/auth ` 时，底部输入框下方显示候选命令列表，而不是只显示一行 help 文案。
- Up/Down 或 Tab 可移动候选，Enter 会启动可直接执行的命令；需要参数的命令会先补全命令文本并等待用户继续输入。
- `/resume` 选择 session 后会清空当前 TUI 的本地 command messages、event cursor、输入框和 transcript scroll 状态，再 replay 目标 session 的历史；这样不会把旧 session 的提示或历史混到新 session。
- 普通 transcript 默认跟随最新内容；PageUp/PageDown 按页翻历史，Home/End 跳到最旧/最新。TUI 默认不捕获鼠标，优先保留终端选择复制能力。
- 普通 `q` 是文本输入，不作为全局退出键。
- Ctrl+C 第一按用于中断当前运行任务并展示退出提示；在短时间内第二次按 Ctrl+C 才退出 TUI。
- Esc 用于关闭局部 picker/dialog 或清空当前输入，不作为常规退出路径。

## 当前差距

- 当前 TUI 已有 qwen-code 风格 provider setup：Golutra API、Third-party Providers、Custom Provider、mock 分组选择；第三方内置 OpenAI、OpenRouter、DeepSeek、Qwen/DashScope compatible、xAI、GitHub Copilot 和本地 OpenAI-compatible preset；OpenAI 展示 ChatGPT browser/headless OAuth/API key，xAI 展示 browser/device OAuth/API key，GitHub Copilot 只展示 device OAuth。Custom Provider 可选 OpenAI-compatible、Anthropic、Gemini、Vertex AI、genai，但不自动获得 OAuth。setup 的 API key 路径按 protocol/baseUrl -> credential storage/API key 或 envKey -> model -> advanced config -> review -> install 执行；OAuth 路径直接启动后台授权并在成功后 verified probe/install。review 展示脱敏 `ProviderInstallPlan`、保存路径和同名 profile 覆盖提示；secret/config/probe 失败自动 rollback，成功覆盖会删除旧 credential。
- 当前 provider/auth 持久化已收敛为全局用户级 `$GOLUTRA_HOME/provider.json` v2和 disk/env SecretRef；磁盘 secret 位于 `$GOLUTRA_HOME/credentials.json`，OAuth browser/device、refresh、revoke/logout 已接通。项目 `.golutra` 不参与 provider 或 runtime 持久化。
- 当前全局 `threads` 表、`golutra thread list`、`golutra resume [THREAD_ID]`、`golutra fork THREAD_ID [--from-turn TURN_ID]`、`golutra thread export` 和 `golutra thread rebind --from` 已可用；默认按当前 canonical cwd 过滤，每个显式新 session 使用独立 thread 主键，daemon 重新 attach 会刷新最近 thread/session。
- 当前完成任务会写入 `AssistantMessage`，`UserProjection.final_message` 和 TUI transcript 可在 resume 后恢复最终回复；下一轮 prompt 会携带当前 session 的压缩历史摘要。
- 当前全局 SQLite 已覆盖多 cwd 事实与索引；TUI 按产品边界只展示当前 canonical cwd 的 session。
- 当前 TUI provider setup 直接写本地 provider config；后续应改为通过 RuntimeHost/config service 返回 `ProviderConfigured`、`ProviderProbeCompleted` 或 `ProviderAuthFailed`。
- 当前受审计 OAuth catalog 已内置 OpenAI ChatGPT browser/headless、xAI browser/device、GitHub Copilot device，并绑定各自实际模型 adapter；其他 provider 仍需要显式 descriptor/registry 扩展。运行中跨客户端 `ProviderAuthRequired` 协议仍后置，Web 首次 provider onboarding 不在范围内。
- 当前 session 事实位于全局 SQLite，rollout/fork/rebind 已闭环；剩余 session 方向主要是超长历史分页/虚拟化与更丰富的全文检索。

这些剩余差距不影响当前 mock/live provider、历史恢复、fork 和项目路径迁移主链。

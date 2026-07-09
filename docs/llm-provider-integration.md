# Golutra LLM 接入设计

## 文档定位

本文档定义 Golutra 后续接入真实 LLM provider 时的配置、认证、凭据存储和运行时事件边界。

它补齐 `initial-implementation-plan.md` 中的 `TODO(provider-config)`，但不替代 `runtime-contracts.md` 的 provider 硬契约。实现时优先遵守：

- runtime 只依赖 Golutra 自己的 `ProviderContract`。
- provider native SDK / wire type 只能存在于 adapter 内。
- 默认仍使用 deterministic mock provider；真实 provider 必须显式启用。
- secret 不进入 runtime event、artifact 摘要、projection、SQLite payload 或日志。

首次 provider onboarding、resume、多 session 和多工作区的完整 UX 见 `onboarding-session-workspace-design.md`。

## qwen-code 调研结论

参考路径：`/Users/skyseek/Desktop/project/open/golutra-agent/project/qwen-code`。

可吸收的设计点：

- `ProviderConfig` 是声明式 provider 注册表，描述 provider 如何展示、使用什么协议、有哪些 base URL 选项、使用哪个 envKey、安装哪些模型，以及是否允许用户编辑模型列表。
- API key、OAuth、订阅套餐、自定义 endpoint 都不是独立配置架构；它们最终都收敛成一个 provider install plan，再统一写配置。
- `modelProviders` 保存模型目录和 envKey 引用，真正的 key 优先从环境变量读取；把 key 直接写入 settings 只作为低优先级兼容路径。
- 自定义 provider 的 envKey 要由协议和 baseUrl 派生，并带 hash 后缀，避免不同 endpoint 折叠到同一个环境变量名。
- provider 模型配置是原子包。选中 provider model 后，它的 baseUrl、envKey、generation config 应整体生效，不能被低优先级配置半覆盖。
- 运行中凭据请求要用统一结构表达 `bearer`、`basic`、`header`、`query`、`multi-header`，UI 只负责采集和取消，不持久化业务状态。
- 安装 provider 时要能 rollback settings、process env 和 runtime model registry，避免 auth refresh 失败后留下半更新状态。

Golutra 不直接复制 qwen-code 的配置文件形状。Golutra 的核心仍是 Rust runtime、event log 和 `ProviderContract`，qwen-code 只作为 provider 接入与用户认证 UX 的参考。

## 当前状态

截至 2026-07-09：

- `golutra-llm` 已有 `MockProvider` 和 OpenAI-compatible live adapter。
- 默认 provider 是 mock。
- CLI 已支持 `golutra provider login`、`golutra provider set-key` 和 `golutra provider use`；`provider login` 可填写 `--enable-thinking`、`--reasoning-effort low|medium|high|xhigh`、`--context-window-size <n>`、`--max-tokens <n>`。TUI 首次进入会检查 provider onboarding 状态。
- 如果当前用户和 workspace 都没有 active provider profile，TUI 会打开 provider setup；用户可以先选 Golutra API、Third-party Providers、Custom Provider 或 mock，再按 qwen-code 风格填写 OpenAI-compatible base URL、API key、推荐或自定义 model、高级生成配置，最后在 review 页确认脱敏 install plan 后保存。
- Custom Provider 的 API key envKey 已按 qwen-code 规则由 `(protocol, baseUrl)` 派生：`GOLUTRA_CUSTOM_PROVIDER_API_KEY_{PROTOCOL}_{NORMALIZED_BASE_URL}_{12_HEX_HASH}`。同一个 endpoint 的尾随 `/` 不会生成不同 key，不同协议或不同 endpoint 不会共享固定 `GOLUTRA_PROVIDER_API_KEY`。
- 真实联网调用必须显式选择协议并配置凭据。推荐设置：
  - `GOLUTRA_PROVIDER_PROTOCOL=openai-compatible`
  - `GOLUTRA_PROVIDER_API_KEY`
  - `GOLUTRA_PROVIDER_MODEL`
  - 可选 `GOLUTRA_PROVIDER_BASE_URL`
- 兼容入口仍支持：
  - `GOLUTRA_PROVIDER_MODE=live`
  - `GOLUTRA_PROVIDER_API_KEY`
  - `GOLUTRA_PROVIDER_MODEL`
  - 可选 `GOLUTRA_PROVIDER_BASE_URL`
- OpenAI-compatible adapter 已支持 `OPENAI_API_KEY`、`OPENAI_MODEL`、`OPENAI_BASE_URL` 作为兼容 fallback。
- provider protocol catalog 已注册 `mock`、`openai-compatible`、`anthropic`、`gemini`、`vertex-ai` 和 `genai`。
- 当前只有 `mock` 与 `openai-compatible` 可执行；`anthropic`、`gemini`、`vertex-ai`、`genai` 已具备协议目录、env 映射和脱敏诊断，但安装层会拒绝把它们保存为 enabled/ready active provider，直到对应 live adapter 接入。
- CLI/env base URL 会做 P0 规范化：例如 `api.golutra.cn` 会解析成 `https://api.golutra.cn/v1`。TUI provider setup 为了对齐 qwen-code 的交互校验，要求用户输入 `http://` 或 `https://` 开头的 endpoint；Golutra 官方 preset 默认填入 `https://api.golutra.cn/v1`。
- CLI 已提供 `golutra provider protocols`、`golutra provider current` 和 `golutra provider probe`，输出只包含协议目录、脱敏配置与 probe 结果，不输出 API key。
- provider 配置已可持久化到 `$GOLUTRA_HOME/provider.json` 或 `<workspace>/.golutra/provider.json`；workspace 配置禁止保存明文 key，用户级配置使用原子写和 owner-only 权限；当前实现已对齐 qwen-code 的 install 思路，把用户级 API key 持久化到 `provider.json` 的 `env` map，profile 只保存 `api_key_env` 引用；旧 JSON 中的 profile 内联 `api_key` 会被忽略，用户需要重新 `/auth` 或 `provider login` 写入新格式。
- 高级生成配置跟随 active profile 保存为 `generation_config`，运行时序列化到 `GOLUTRA_PROVIDER_GENERATION_CONFIG`。OpenAI-compatible adapter 当前会下发 `extra_body.enable_thinking`、`reasoning_effort` 和 `max_tokens`；`context_window_size` 作为上下文预算元数据保留，不写入 Chat Completions 请求体。
- live 模式下配置缺失会显式失败，不再静默回退到 mock。
- 这套 env 入口保留为 P0 兼容路径，后续 provider catalog / secretRef / OAuth 配置系统必须能包住它，而不是破坏现有 smoke 和 CLI 行为。
- TUI 已有 provider onboarding gate 和 `/auth` setup；已有 active provider 时不会首屏打断，但输入 `/auth` 可随时重新打开同一套选型并覆盖同名 profile。Web 首次 connect provider flow 仍待补齐，CLI 非交互场景保持结构化诊断。

## 目标架构

LLM 接入链路收敛为：

```text
CLI / TUI / Web / SDK
-> golutra-client
-> RuntimeHost
-> ProviderResolver
-> ProviderAdapter
-> ProviderContract
-> AgentLoop
-> RuntimeEvent / Artifact / Projection
```

职责边界：

| 模块 | 职责 |
| --- | --- |
| `golutra-config` | 读取用户级、workspace 级和 env provider 配置，输出脱敏后的 `ResolvedProviderConfig` |
| `golutra-llm` | 定义 provider trait、adapter、catalog、capability、usage 和错误归一化 |
| `golutra-runtime` | 只消费 `ProviderContract`，负责 fallback、retry、verification 和事件写入 |
| `golutra-client` | 提供 provider command/query，透传 auth required / probe 结果 |
| CLI/TUI/Web | 采集凭据、展示状态、触发 probe，不保存 runtime 真相 |

禁止方向：

- 不让 runtime core 依赖 OpenAI、Anthropic、Gemini、DashScope 等原生类型。
- 不让 adapter 私自 fallback 到别的 provider。
- 不把 provider key 写进 `.golutra/runtime.sqlite` 的 event payload。
- 不让 TUI / Web 自己维护 provider 连接状态机。

## Provider 协议分类

Golutra 第一阶段按协议能力分类，不按品牌分叉 runtime：

| 协议类 | 说明 | P0/P1 策略 |
| --- | --- | --- |
| `mock` | deterministic provider，用于本地 smoke、replay、测试 | 默认启用 |
| `openai-compatible` | OpenAI Chat Completions 兼容 endpoint，包括 OpenAI、OpenRouter、DashScope compatible、Ollama/vLLM/LM Studio | P0 已有最小 adapter，P1 扩展配置 |
| `anthropic` | Anthropic native Messages API | 目录、env 映射和诊断已就绪，adapter 待接 |
| `gemini` | Google Gemini API | 目录、env 映射和诊断已就绪，adapter 待接 |
| `vertex-ai` | Google Vertex AI | 目录、env 映射和诊断已就绪，adapter 待接 |
| `genai` | `rust-genai` 聚合 adapter，覆盖 Anthropic、Gemini、Ollama、OpenRouter、DeepSeek 等 | P1/P2 推荐默认多 provider 接入层，adapter 待接 |

常见 provider 可以先按协议接入：

| provider | 推荐协议 | 备注 |
| --- | --- | --- |
| OpenAI | `openai-compatible` | `OPENAI_*` env 可作为 fallback |
| OpenRouter | `openai-compatible` | 设置自定义 baseUrl 和 model |
| DashScope / Qwen compatible | `openai-compatible` | 优先走兼容 endpoint |
| Ollama / vLLM / LM Studio | `openai-compatible` | 作为本地或私有 baseUrl 变体 |
| Anthropic | `anthropic` 或未来 `genai` | 当前只做配置诊断，不执行 live 请求 |
| Gemini / Vertex AI | `gemini` / `vertex-ai` 或未来 `genai` | 当前只做配置诊断，不执行 live 请求 |

新增 provider 时先问两个问题：

1. 能否通过 `openai-compatible` 或 `genai` 表达？
2. 不能表达的差异是 provider wire detail，还是 Golutra runtime contract 需要新增能力？

只有第二种情况才修改 core contract。

## Provider 配置模型

建议引入声明式 provider catalog：

```jsonc
{
  "providers": {
    "openai": {
      "protocol": "openai-compatible",
      "label": "OpenAI",
      "baseUrl": "https://api.openai.com/v1",
      "models": [
        {
          "id": "gpt-4.1",
          "envKey": "OPENAI_API_KEY",
          "contextWindowSize": 1047576,
          "toolCalling": true,
          "streaming": true
        }
      ]
    },
    "custom-local": {
      "protocol": "openai-compatible",
      "baseUrl": "http://localhost:11434/v1",
      "models": [
        {
          "id": "qwen2.5-coder",
          "envKey": "GOLUTRA_CUSTOM_API_KEY_OPENAI_LOCALHOST_11434_7F3A91C2B501"
        }
      ]
    }
  },
  "selection": {
    "providerId": "openai",
    "modelId": "gpt-4.1"
  }
}
```

字段原则：

- `providerId` 是用户和 UI 可识别的 provider 身份。
- `protocol` 是 adapter 路由身份，不等于 provider 品牌。
- `envKey` 是 secret 引用，不是 secret 值。
- `baseUrl` 和 `modelId` 共同定位模型；同一个 modelId 可以出现在多个 baseUrl 下。
- `generationConfig` 是 provider model 的原子配置包，选中后整体生效。
- `capabilities` 记录 tool calling、streaming、vision、json/schema、reasoning、context window、max output、rate limit hints。

自定义 provider 的 envKey 生成规则：

```text
GOLUTRA_CUSTOM_PROVIDER_API_KEY_{PROTOCOL}_{NORMALIZED_BASE_URL}_{SHA256(protocol + NUL + canonical_base_url)[0..12]}
```

这样用户能大致看懂来源，也能避免不同 URL 规范化后碰撞。`canonical_base_url` 会去掉尾随 `/`，所以 `https://api.example.com/v1` 与 `https://api.example.com/v1/` 指向同一 envKey。

## 配置优先级

运行时按字段解析，但 provider model 一旦命中 catalog，模型相关配置必须按原子包处理。

推荐优先级：

| 优先级 | 来源 | 说明 |
| --- | --- | --- |
| 1 | 命令行显式参数 | `--provider`、`--model`、`--base-url`、`--api-key-env`、临时 API key |
| 2 | workspace 配置 | `.golutra/provider.json`，适合团队共享模型目录和默认选择 |
| 3 | 用户配置 | `$GOLUTRA_HOME/provider.json` 或系统配置目录，适合个人 provider catalog |
| 4 | 环境变量 | `GOLUTRA_PROVIDER_*`、`OPENAI_API_KEY` 等 |
| 5 | 内置默认 | mock provider |

特殊规则：

- workspace 配置可以声明 provider catalog，但默认不能保存 secret value。
- user 配置也优先保存 envKey / secretRef；直接保存 secret 只作为显式 opt-in 兼容能力。
- 当前 `GOLUTRA_PROVIDER_MODE=live` 继续作为快捷开关；如果 catalog 已明确选择 provider，则 catalog 优先。
- provider model 命中后，`generationConfig` 不向低优先级配置做深合并，避免温度、reasoning、extra body、headers 被半覆盖。

## 认证模式

Golutra 需要支持以下认证形态：

| 模式 | 适用场景 | 配置表达 |
| --- | --- | --- |
| `env-api-key` | CI、shell、开发者本地 | `envKey` 指向环境变量 |
| `secret-ref` | OS keychain、pass、1Password、企业 secret manager | `secretRef` 指向外部 secret |
| `oauth-device` | 支持 device flow 的平台 | 保存 token ref、expiry、scope，不写明文 token |
| `oauth-browser-code` | 桌面/TUI 引导浏览器后粘贴 code | 事件驱动 auth session |
| `bearer` | 单 token header | 运行时 credential request |
| `basic` | username/password | 运行时 credential request |
| `header` | 单自定义 header | 运行时 credential request |
| `query` | query 参数 key | 仅对必须如此的 legacy API 开启 |
| `multi-header` | 例如同时要求 API key 和 application key | 结构化 headers map |

P0 只要求 `env-api-key` 稳定。P1 开始补 `provider login`、`set-key` 和 TUI connect modal。OAuth 不应阻塞 P0 真实 provider smoke。

## 凭据存储规则

强制规则：

- `.golutra/runtime.sqlite` 不保存明文 API key、OAuth access token、refresh token、basic password 或 multi-header value。
- runtime event payload 只保存 `credential_ref`、`envKey`、脱敏 fingerprint、auth mode、providerId、modelId、baseUrl hash。
- provider raw response 可以写 artifact，但必须先经过 secret redaction。
- CLI/TUI/Web 展示错误时不得回显 key；只展示 envKey、secretRef 或 key fingerprint。
- test fixture 不提交真实 key；golden fixture 使用固定假 key 并通过 redaction 验证。

推荐存储分层：

| 存储 | 允许内容 |
| --- | --- |
| shell env / `.env` | 明文 key；文件必须 gitignore |
| OS keychain | 明文 key/token |
| user provider config | envKey、secretRef、provider catalog、默认模型 |
| workspace provider config | provider catalog、团队默认模型、envKey 名称 |
| runtime.sqlite | 脱敏后的运行事实、credential fingerprint、probe 结果 |

key fingerprint 推荐：

```text
sha256(provider_id + ":" + envKey + ":" + secret_value)[0..12]
```

fingerprint 只用于判断“是否换了凭据”，不能用于认证。

## 用户体验

### CLI

建议命令：

```bash
golutra provider protocols
golutra provider current
golutra provider probe
golutra provider login <provider-id>
golutra provider set-key <provider-id> --env-key OPENAI_API_KEY
golutra provider set-key <provider-id> --store keychain
golutra provider use <provider-id> --model <model-id>
golutra provider add-custom --protocol openai-compatible --base-url http://localhost:11434/v1 --model qwen2.5-coder
```

行为：

- `provider login` 负责交互式 OAuth 或 guided API key setup。
- `provider set-key` 不把 key 写入 runtime event；只写 envKey/secretRef，或交给 keychain。
- `provider protocols` 输出内置协议、env key、baseUrl key、model key、probe 能力和 adapter 状态。
- `provider probe` 执行最小健康检查并写脱敏 event。
- 非交互环境下，如果缺凭据，返回可执行错误：缺哪个 envKey、当前 provider/model 是什么、如何设置。

首次进入策略：

- `golutra tui` / Web：如果没有 ready live provider，打开 provider setup，并提供 Continue with mock。
- `golutra tui` 当前已实现 qwen-code 风格 provider 分组和 OpenAI-compatible API key setup；流程为 group -> provider preset -> baseUrl -> apiKey -> model -> review -> install。官方和第三方 preset 继续使用固定 `GOLUTRA_PROVIDER_API_KEY`，Custom Provider 使用派生 envKey；API key 默认保存到用户级 `provider.json` 的 `env` map，workspace config 仍禁止保存明文 key。
- `golutra chat`：默认 mock；如果用户显式设置 live protocol 但缺 key/model，返回 missing env 错误。
- `golutra provider login`：强制进入 provider setup。
- CI / 非 TTY：永远不弹交互 UI，只输出结构化错误。

### TUI

TUI provider connect modal 按三组展示：

- Golutra API / 内置常用 provider
- OpenAI-compatible 聚合/第三方 provider
- Custom endpoint / mock fallback

当前已实现字段：

- provider group：Golutra API、Third-party Providers、Custom Provider、mock fallback
- provider preset：OpenAI、OpenRouter、DeepSeek、Qwen/DashScope compatible、本地 OpenAI-compatible
- baseUrl input：必须以 `http://` 或 `https://` 开头
- API key input：脱敏显示，保存到 user provider config
- model：内置推荐模型选择，或输入自定义 model id
- advanced config：支持 Thinking、Reasoning effort、Context window、Max output tokens；字段保存到 profile 并随 active provider 生效
- review：展示 profile、baseUrl、model、advanced config、scope、保存路径、是否覆盖同名 profile，以及脱敏后的 `ProviderInstallPlan`

尚未实现字段：

- protocol selector：Custom Provider 可选择 OpenAI-compatible、Anthropic-compatible、Gemini-compatible；当前只有 OpenAI-compatible 有 live adapter，未实现 adapter 的协议会在 review/安装前阻止保存
- secretRef / envKey 选择：当前 TUI 明文 key 只写用户级 provider config；Custom Provider 的 envKey 自动派生，但还没有 UI 让用户手动选择已有 envKey 或 secretRef
- custom headers / streaming override：当前还没有 UI 与 profile 字段

目标架构中，TUI 不直接写 runtime 状态，而是发送 provider command，由 RuntimeHost / config service 返回 `ProviderConfigured`、`ProviderProbeCompleted` 或 `ProviderAuthFailed`。当前 TUI setup 为了先闭环本地体验，会直接应用 `ProviderInstallPlan` 写入 provider config；这条路径必须继续保持脱敏 review 和 workspace 禁止保存明文 key 的约束。

### app-server / Web / SDK

app-server 需要支持运行时 auth required 事件：

```text
ProviderAuthRequired
-> client 展示 CredentialRequest
-> client 提交 CredentialResponse
-> RuntimeHost 绑定 credential_ref 并重试安全边界内的 provider call
```

CredentialRequest 统一支持：

- `bearer`
- `basic`
- `header`
- `query`
- `multi-header`

所有响应必须可取消。取消写入 `ProviderAuthCancelled`，任务不能假装成功。

## Runtime 事件

新增或明确以下事件类型：

| 事件 | durable | 说明 |
| --- | --- | --- |
| `ProviderAuthRequired` | 是 | 缺凭据或凭据过期，需要用户输入或登录 |
| `ProviderAuthSubmitted` | 是 | 用户提交凭据响应，只保存 ref/fingerprint |
| `ProviderAuthCancelled` | 是 | 用户取消凭据输入 |
| `ProviderConfigured` | 是 | provider selection/catalog 已更新 |
| `ProviderProbeStarted` | 是 | 开始健康检查 |
| `ProviderProbeCompleted` | 是 | probe 成功，记录 capabilities/latency/rate limit hints |
| `ProviderAuthFailed` | 是 | 认证失败，记录脱敏错误 |
| `ProviderRateLimited` | 是 | rate limit，记录 reset hint |
| `ProviderCredentialRefreshed` | 是 | OAuth token refresh 成功，记录 token ref 和 expiry |

事件 payload 禁止包含：

- API key 明文
- OAuth access token / refresh token
- basic password
- 自定义 header value
- provider request 原始 Authorization header

## Probe 与健康检查

`provider probe` 不只是 ping。它至少检查：

- baseUrl 格式和 TLS/HTTP 可达性。
- credential 是否存在且可用于认证。
- model 是否存在或是否能被 endpoint 接受。
- streaming 是否可用。
- tool calling 是否可用。
- JSON/schema response 是否可用。
- usage 是否完整返回；缺失时 usage source 标记为 `unknown` 或 `estimated`。
- rate limit / quota 错误是否能归一化。
- provider 错误是否能映射为 `NotConfigured`、`RateLimited`、`ProviderFailed` 或 `MalformedResponse`。

probe 结果进入 capability matrix，但不能把一次 probe 成功当作永久保证。运行时仍要处理 401、403、429、5xx、timeout、malformed stream 和 truncated stream。

## 实施计划

### P0 兼容层

- 已保留 `GOLUTRA_PROVIDER_MODE=live` 快捷入口。
- 已新增 `GOLUTRA_PROVIDER_PROTOCOL`，协议选择优先于旧 mode 快捷入口。
- 已将 env 读取集中到 OpenAI-compatible 配置解析入口，避免运行时散落读取 key/model/baseUrl。
- 已支持 `golutra provider protocols`、`golutra provider current` 和 `golutra provider probe` 作为 P0 脱敏检查入口。
- 已注册 `mock`、`openai-compatible`、`anthropic`、`gemini`、`vertex-ai`、`genai` protocol catalog；未实现 adapter 的协议会返回 `adapter_not_implemented` 诊断，不会静默 fallback，也不能通过 CLI/TUI/config install 保存为 ready active provider。
- 增加 provider config schema 草案和脱敏 snapshot。
- provider 错误统一写入 runtime event，避免只在 stderr 出现。

### P1 provider catalog

- 新增 user/workspace provider config 文件。
- 实现 `provider list/current/use/probe/login/set-key`。
- 引入 `ProviderCatalog`、`ProviderSelection`、`ResolvedProviderConfig`。
- 引入 `ProviderInstallPlan`，确保 settings/env/runtime reload/probe 失败时可 rollback。
- TUI/Web 接入首次 provider setup，不再让真实用户面对空白 mock 前端。
- 支持 OpenAI-compatible 自定义 endpoint。
- 给 OpenAI-compatible adapter 增加 streaming/tool calling/capability probe 测试。

### P1 凭据 UX

- 实现 `provider set-key`，支持 envKey 和 secretRef。
- TUI 增加 provider connect modal。
- app-server 增加 `ProviderAuthRequired` / `CredentialResponse` 协议。
- 所有日志和 event 加 secret redaction 测试。

### P2 多协议与 OAuth

- 接入 `genai` adapter，保持 adapter 反腐边界。
- 增加 OAuth device/browser-code flow。
- 支持 token refresh 和 credential renewal。
- 支持 provider model catalog auto-update，但更新必须可 diff、可 rollback、不可覆盖用户自定义模型。

## 验收标准

最低可验收场景：

- 默认不配置任何 key 时，CLI/TUI/app-server 仍使用 mock provider。
- 设置当前 P0 env 后，可以通过 OpenAI-compatible endpoint 跑一个真实任务。
- 缺 key 时错误指出缺哪个 envKey，不泄露任何 secret。
- provider probe 失败不会修改当前 active provider selection。
- TUI/Web 看到同一个 `ProviderAuthRequired`，任一端提交或取消后，其他端看到同一状态。
- runtime event、artifact、projection、debug export 中没有明文 key。
- 重启后 provider selection 可恢复，但 secret 仍从 env/keychain/secretRef 解析。

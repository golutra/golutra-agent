# Golutra LLM 接入设计

## 文档定位

本文档定义 Golutra 真实 LLM provider 的配置、认证、凭据存储和运行时事件边界。

它固化 `initial-implementation-plan.md` 的 provider-config 决策，但不替代 `runtime-contracts.md` 的 provider 硬契约。实现时优先遵守：

- runtime 只依赖 Golutra 自己的 `ProviderContract`。
- provider native SDK / wire type 只能存在于 adapter 内。
- 默认仍使用 deterministic mock provider；真实 provider 必须显式启用。
- secret 不进入 runtime event、artifact 摘要、projection、SQLite payload 或日志。

首次 provider onboarding、resume、多 session 和多工作区的完整 UX 见 `onboarding-session-workspace-design.md`。

## qwen-code 调研结论

参考项目：[Qwen Code](https://github.com/QwenLM/qwen-code)。

可吸收的设计点：

- `ProviderConfig` 是声明式 provider 注册表，描述 provider 如何展示、使用什么协议、有哪些 base URL 选项、使用哪个 envKey、安装哪些模型，以及是否允许用户编辑模型列表。
- API key、OAuth、订阅套餐、自定义 endpoint 都不是独立配置架构；它们最终都收敛成一个 provider install plan，再统一写配置。
- `modelProviders` 保存模型目录和 envKey 引用，真正的 key 优先从环境变量读取；把 key 直接写入 settings 只作为低优先级兼容路径。
- 自定义 provider 的 envKey 要由协议和 baseUrl 派生，并带 hash 后缀，避免不同 endpoint 折叠到同一个环境变量名。
- provider 模型配置是原子包。选中 provider model 后，它的 baseUrl、envKey、generation config 应整体生效，不能被低优先级配置半覆盖。
- qwen-code 的 `generationConfig.extra_body` 是配置容器；OpenAI-compatible provider 的 `buildRequest()` 会把其中字段展开到最终 HTTP JSON 顶层。Golutra 直接构造 wire body，因此不能把 `extra_body` 容器原样发送。
- 运行中凭据请求要用统一结构表达 `bearer`、`basic`、`header`、`query`、`multi-header`，UI 只负责采集和取消，不持久化业务状态。
- 安装 provider 时要能 rollback settings、process env 和 runtime model registry，避免 auth refresh 失败后留下半更新状态。
- qwen-code desktop 的 credential manager 把 `CredentialId`、`StoredCredential` 和 backend 分离，并按 credential id 合并并发 refresh；OAuth browser flow 使用 PKCE、state、loopback callback 和 refresh-token rotation。Golutra 应复用这些边界，不复制 TypeScript/Electron 实现。

Golutra 不直接复制 qwen-code 的配置文件形状。Golutra 的核心仍是 Rust runtime、event log 和 `ProviderContract`，qwen-code 只作为 provider 接入与用户认证 UX 的参考。

## Codex 调研结论

参考项目：[OpenAI Codex](https://github.com/openai/codex)。

- Codex 在 `codex-rs/login/src/auth/storage.rs` 用统一 `AuthStorageBackend` 隔离 file、keyring、auto 和 ephemeral storage；keyring account 包含 canonical `CODEX_HOME` hash，避免替代 home 读到真实用户凭据。
- `AuthCredentialsStoreMode` 明确区分 file/keyring/auto/ephemeral，OAuth token 的存储选择不进入 model provider adapter。
- browser/device login、token exchange、refresh 和 storage 归 login/auth 层；core runtime 消费已解析 auth，不自行读取 auth 文件。
- Golutra 采纳 storage trait、home 隔离和 auth service 分层，并按当前产品决策使用显式 disk backend：交互式本地 secret 默认进入 owner-only `$GOLUTRA_HOME/credentials.json`，CI/headless 环境可使用 env ref；不访问 OS keychain。

## opencode 调研结论

参考项目：[OpenCode](https://github.com/anomalyco/opencode)。

- opencode 的 OAuth 不是从任意 endpoint 猜测授权地址，而是 provider auth plugin 注册认证方法和 request loader：OpenAI 使用 browser PKCE 和私有 headless device-auth，xAI 同时提供 browser/device，GitHub Copilot 使用 device flow，GitLab/Poe 等可由外部 plugin 扩展。
- 授权成功不等于 provider 可用。OpenAI ChatGPT subscription token 必须改走 Codex Responses SSE endpoint并携带 `ChatGPT-Account-Id`；GitHub Copilot 请求必须增加 GitHub API version、intent、initiator 等 header。Golutra 因此把 OAuth method 与固定 runtime adapter 一起注册。
- `/connect` 动态展示当前 provider 的 auth methods。Golutra 对应为 TUI `/auth` 的认证方式选择和 CLI `provider auth-methods`，Custom Provider 仍只展示 API key/env ref，除非用户显式提供受审计 descriptor。
- opencode 默认把 token 明文放在 owner-only `auth.json`。Golutra采用相同的本地文件威胁模型，但把 secret 独立放在 `$GOLUTRA_HOME/credentials.json`，`provider.json` 仍只暴露非敏感 account id、expiry 和 SecretRef。

## 当前状态

截至 2026-08-21：

- `golutra-llm` 已有 `MockProvider`、独立 OpenAI-compatible live adapter、基于固定 `rust-genai 0.7.0-beta.12` 的 `GenaiProviderAdapter`，以及显式使用其 `OpenAIResp` adapter 的 Responses 薄适配。
- 默认 provider 是 mock。
- CLI 已支持 `golutra provider login`、`set-key`、`oauth-login`、`logout` 和 `use`；`provider login` 可填写 `--enable-thinking`、`--reasoning-effort low|medium|high|xhigh`、`--context-window-size <n>`、`--max-tokens <n>`。TUI 首次进入会检查 provider onboarding 状态。
- 如果全局用户配置没有 active provider profile，TUI 会打开 provider setup；用户可以先选 Golutra API、Third-party Providers、Custom Provider 或 mock，再按 qwen-code 风格选择协议、base URL、凭据存储、推荐或自定义 model 和高级生成配置，最后在 review 页确认脱敏 install plan 后保存。交互输入的 API key 默认进入 `$GOLUTRA_HOME/credentials.json`，也可只保存已有 envKey 引用。
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
- provider protocol catalog 已注册且可执行 `mock`、`openai-compatible`、`openai-responses`、`anthropic`、`gemini`、`vertex-ai` 和 `genai`。
- `anthropic` 强制使用 Anthropic Messages wire，`gemini` 使用 generateContent，`vertex-ai` 使用 Vertex generateContent 和 bearer/OAuth token，`genai` 根据 model namespace 选择 rust-genai adapter；它们统一映射 tool round-trip、usage、reasoning effort、finish reason 和脱敏错误。
- live HTTP 调用使用 10 秒 connect timeout 和 120 秒总 timeout；OpenAI-compatible 已使用 SSE 增量读取并按顺序产生 text/tool/usage stream event，truncated/malformed stream 显式失败。SSE 与 genai captured raw metadata 都执行 16 MiB 响应边界，assistant message、tool id/name/arguments 另有更小字段上限。
- CLI/env base URL 会做 P0 规范化：例如 `api.golutra.cn` 会解析成 `https://api.golutra.cn/v1`。TUI provider setup 为了对齐 qwen-code 的交互校验，要求用户输入 `http://` 或 `https://` 开头的 endpoint；Golutra 官方 preset 默认填入 `https://api.golutra.cn/v1`。
- CLI 已提供 `golutra provider protocols`、`golutra provider current` 和 `golutra provider probe`，输出只包含协议目录、脱敏配置与 probe 结果，不输出 API key。
- provider/auth 配置持久化到 `$GOLUTRA_HOME/provider.json` v2；workspace `.golutra` 不再作为 provider 配置来源。v2 使用原子写和 owner-only 权限，只保存 `credential_ref`、OAuth descriptor 与非敏感 provider metadata，不保存 API key 或 token。交互 secret 进入独立的 owner-only `$GOLUTRA_HOME/credentials.json`，CI/headless 配置可保存只读 env ref；v1 `env` map 会在 provider settings lock 内一次性迁移到 disk SecretStore，迁移失败会恢复 secret 并保留原配置。
- 高级生成配置跟随 active profile 保存为 `generation_config`，运行时序列化到 `GOLUTRA_PROVIDER_GENERATION_CONFIG`。对齐 qwen-code provider 展开语义后，OpenAI-compatible adapter 会在最终 Chat Completions JSON 顶层下发 `enable_thinking`、`reasoning_effort` 和 `max_tokens`；`context_window_size` 不写入 provider 请求体，但会收紧 `ContextBuilder` 的 context window、reserved output 和输入预算。
- `provider current`、运行时 resolver 与 `provider probe` 在没有任何配置时都一致解析为 deterministic mock；显式 live 配置损坏或缺失仍返回错误，不静默 fallback。
- live 模式下配置缺失会显式失败，不再静默回退到 mock。
- env 入口继续作为非交互配置协议，并已由 SecretRef 层作为只读 credential source 使用；明文值不会复制进 provider 配置。`golutra-auth` 已实现 browser PKCE、RFC 8628 device flow、OpenAI headless device-auth、token refresh/revoke/logout，LLM adapter 在 401 时只强制刷新并重试一次。受审计 catalog 已内置 OpenAI ChatGPT browser/headless、xAI browser/device 和 GitHub Copilot device；自定义 OAuth 仍要求显式 descriptor，不会从任意 OpenAI-compatible base URL 自动推断授权端点。
- OpenAI ChatGPT OAuth 使用 `openai-responses` 薄适配：固定 `rust-genai::OpenAIResp` 后发往 Responses SSE endpoint，不按模型名重新推断协议；account id 优先从 token response 的 `id_token` 安全提取，调用时携带 `ChatGPT-Account-Id`/`session-id`，并在 `store=false` 的多轮工具调用中保留/回送 encrypted reasoning item。流建立阶段的 401 只允许在首个业务事件前刷新重建一次。GitHub Copilot adapter 增加其要求的 API version、intent、initiator 和 User-Agent header。
- 已提交六组 provider golden fixture，通过本地 HTTP 捕获实际 adapter wire，覆盖完整 message/tool result 序列化、Responses SSE/reasoning replay、文本和 tool-call 响应、usage/finish reason、auth header 与 401。`just provider-live-smoke` 只读取专用 `GOLUTRA_LIVE_*` 环境变量，不读取正常用户凭据，变量不全时安全跳过。
- provider capability 已有 declared/discovered 两级来源。OpenAI-compatible probe 从 `/models` 的 supported parameters、context window、max output 和 input modalities 更新 streaming/tools/JSON Schema/reasoning/vision；无法发现的字段保留 declared/unknown，不伪造能力。
- Custom Provider 的 CLI/TUI/profile 已支持 literal 与 env-ref custom headers；Authorization、Host、Content-Length 等 transport header 和疑似明文 secret 会在保存前拒绝，runtime debug 只暴露 header 名称。
- 任务执行中遇到缺失/损坏 provider 配置会进入 durable `ProviderAuthRequired` 和 `WaitingAuthentication`；任一已 attach 客户端可在完成 verified install 后提交 `ProviderConfigured/ProviderAuthSubmitted` 使原任务从安全边界继续，也可提交 `ProviderAuthCancelled` 取消。该链路已有 embedded 与跨进程 daemon 回归。
- TUI 已有 provider onboarding gate 和 `/auth` setup；已有 active provider 时不会首屏打断，但输入 `/auth` 可随时重新打开同一套选型并覆盖同名 profile。Web 首次 provider onboarding 不在当前产品范围，CLI 非交互场景保持结构化诊断。

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
| `golutra-auth` | 定义 `CredentialRef`/`SecretStore`，封装 owner-only disk、env、临时测试存储及 OAuth login/refresh/revoke |
| `golutra-config` | 读取全局用户级 provider v2 和 process env，执行 v1 到 disk 迁移、verified install/probe/rollback，输出动态 credential provider |
| `golutra-llm` | 定义 provider trait、adapter、catalog、capability、usage 和错误归一化 |
| `golutra-runtime` | 只消费 `ProviderContract`，负责 fallback、retry、verification 和事件写入 |
| `golutra-client` | 提供 provider command/query，透传 auth required / probe 结果 |
| CLI/TUI | 采集凭据、展示状态、触发 probe，不保存 runtime 真相 |
| Web/SDK | 当前只消费 runtime projection/event；不实现首次 provider onboarding |

禁止方向：

- 不让 runtime core 依赖 OpenAI、Anthropic、Gemini、DashScope 等原生类型。
- 不让 adapter 私自 fallback 到别的 provider。
- 不把 provider key 写进 `$GOLUTRA_HOME/state/runtime.sqlite` 的 event payload。
- 不让 TUI 自己维护 provider 连接状态机。

## Provider 协议分类

Golutra 第一阶段按协议能力分类，不按品牌分叉 runtime：

| 协议类 | 说明 | P0/P1 策略 |
| --- | --- | --- |
| `mock` | deterministic provider，用于本地 smoke、replay、测试 | 默认启用 |
| `openai-compatible` | OpenAI Chat Completions 兼容 endpoint，包括 OpenAI、OpenRouter、DashScope compatible、Ollama/vLLM/LM Studio | 独立 adapter，已可用 |
| `openai-responses` | OpenAI Responses SSE；当前用于 ChatGPT subscription OAuth 和显式 Responses 网关 | `rust-genai::OpenAIResp` 薄适配，支持 encrypted reasoning replay，已可用 |
| `anthropic` | Anthropic native Messages API | `GenaiProviderAdapter` 强制 Anthropic wire，已可用 |
| `gemini` | Google Gemini API | `GenaiProviderAdapter` 强制 Gemini wire，已可用 |
| `vertex-ai` | Google Vertex AI | `GenaiProviderAdapter` 强制 Vertex wire，需完整 project/location base URL 和 bearer/OAuth token |
| `genai` | `rust-genai` 聚合路由，覆盖 DeepSeek 等模型 namespace | 按 model namespace 路由，已可用 |

常见 provider 可以先按协议接入：

| provider | 推荐协议 | 备注 |
| --- | --- | --- |
| OpenAI API key | `openai-compatible` | `OPENAI_*` env 可作为 fallback |
| OpenAI ChatGPT OAuth | `openai-responses` | 固定走 ChatGPT Codex endpoint，不与 API key adapter 混用 |
| xAI | `openai-compatible` | 支持 API key，以及受审计的 browser/device OAuth |
| GitHub Copilot | `openai-compatible` | 只展示 GitHub device OAuth，并增加 Copilot 专用 header |
| OpenRouter | `openai-compatible` | 设置自定义 baseUrl 和 model |
| DashScope / Qwen compatible | `openai-compatible` | 优先走兼容 endpoint |
| Ollama / vLLM / LM Studio | `openai-compatible` | 作为本地或私有 baseUrl 变体 |
| Anthropic | `anthropic`，或有明确 model namespace 时使用 `genai` | native Messages wire |
| Gemini / Vertex AI | `gemini` / `vertex-ai` | native generateContent wire；Vertex base URL 必须包含 project/location |

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
| 2 | 用户配置 | `$GOLUTRA_HOME/provider.json`，Codex 风格全局 provider/auth 配置 |
| 3 | 环境变量 | `GOLUTRA_PROVIDER_*`、`OPENAI_API_KEY` 等 |
| 4 | 内置默认 | mock provider |

特殊规则：

- provider catalog/profile 只允许写入全局用户配置；cwd 不覆盖 provider 或 secret。
- user 配置只保存 envKey / SecretRef；当前 v2 不允许直接保存 secret。
- 当前 `GOLUTRA_PROVIDER_MODE=live` 继续作为快捷开关；如果 catalog 已明确选择 provider，则 catalog 优先。
- provider model 命中后，`generationConfig` 不向低优先级配置做深合并，避免温度、reasoning、extra body、headers 被半覆盖。

## 认证模式

Golutra 需要支持以下认证形态：

| 模式 | 适用场景 | 配置表达 |
| --- | --- | --- |
| `env-api-key` | CI、shell、开发者本地 | `envKey` 指向环境变量 |
| `secret-ref` | owner-only `$GOLUTRA_HOME/credentials.json`；后续可扩展企业 secret manager | `credential_ref` 指向磁盘凭据记录 |
| `oauth-device` | 支持 device flow 的平台 | 保存 token ref、expiry、scope，不写明文 token |
| `oauth-browser-code` | 桌面/TUI 引导浏览器并接收 loopback callback | PKCE + state auth session |
| `bearer` | 单 token header | 运行时 credential request |
| `basic` | username/password | 运行时 credential request |
| `header` | 单自定义 header | 运行时 credential request |
| `query` | query 参数 key | 仅对必须如此的 legacy API 开启 |
| `multi-header` | 例如同时要求 API key 和 application key | 结构化 headers map |

`env-api-key`、disk SecretRef、TUI API key setup、内置 provider auth catalog，以及显式 descriptor 扩展的 browser/device OAuth 均已落地。`basic`、`query`、`multi-header` 等运行时动态 credential request 仍属于独立扩展，不影响当前 API key/OAuth provider 主链。

## 凭据模型与存储

### Provider config v2

`provider.json` v2 只保存非敏感 provider 配置和 `CredentialRef`，删除 v1 的明文 `env` map。认证方式和存储位置分开建模：OAuth 是获取/更新凭据的协议，disk/env 是凭据来源，二者不能混成一个枚举分支。

```text
ProviderProfile
  provider_id
  protocol
  model_id
  base_url
  credential_ref: CredentialRef?

CredentialRef
  id: CredentialId
  source: environment | disk | ephemeral
  secret_kind: api-key | bearer | oauth-token-set | structured-headers
  revision: random non-secret version id
```

示例：

```json
{
  "version": 2,
  "active_profile": "custom",
  "profiles": [
    {
      "name": "custom",
      "protocol": "openai-compatible",
      "model_id": "gpt-5.5",
      "base_url": "https://api.example.com/v1",
      "credential_ref": {
        "id": "cred_01...",
        "source": { "kind": "disk" },
        "secret_kind": "api-key",
        "revision": "rev_01..."
      }
    }
  ]
}
```

不要从 secret 派生持久 fingerprint。短 hash 会为低熵密码提供离线猜测信号；每次写凭据时生成随机 `revision`，足以判断配置是否换过。env ref 只报告变量是否存在，不持久化其值或 hash。

### SecretStore 边界

`golutra-auth` 提供独立于 provider adapter 的 `SecretStore`：

```text
SecretStore
  get(ref) -> SecretValue
  set(ref, SecretValue)
  delete(ref)
  health_check()
```

- `DefaultSecretStore` 的 `disk` source 是交互式本地登录默认实现，固定写入 `$GOLUTRA_HOME/credentials.json`；文件按 credential id 保存 `secret_kind` 和明文值，`provider.json` 只保存引用。
- 凭据目录和文件使用独立 `credentials.lock` 做跨进程互斥，写入经过大小校验、临时文件、`fsync` 和原子替换；Unix 下 home 目录收紧为 `0700`，凭据与锁文件为 `0600`，并拒绝符号链接和非普通凭据文件。
- disk backend 是明确的本地明文存储选择，不承诺静态加密；其安全边界是当前操作系统用户和文件权限。需要外部 secret manager 的部署应使用 env ref 或后续独立 backend。
- environment source 只读，服务于 CI、容器和已有 shell 配置；`provider set-key --env-key` 只保存变量名。
- ephemeral source 只用于一次性命令和测试，进程退出即失效。
- 运行时 secret 使用 `secrecy` 包装，不通过 `Debug`/provider config/runtime event 序列化；只有 disk backend 持久化和 provider adapter 构造 Authorization/header 的最末端可以暴露字符串切片。
- provider resolver 只持有 `Arc<dyn CredentialProvider>`，每次请求在构造 Authorization/header 的末端动态解析 secret；`RuntimeHost`、event store、rollout 和 projection 都不接触可序列化 secret。

强制规则：

- `$GOLUTRA_HOME/state/runtime.sqlite` 不保存明文 API key、OAuth access token、refresh token、basic password 或 multi-header value。
- runtime event payload 只保存 credential id/revision、auth mode、provider id、model id、base URL hash 和 expiry，不保存 secret 或 Authorization header。
- provider raw response 可以写 artifact，但必须先经过递归 secret redaction。
- CLI/TUI 展示错误时不得回显 key；只展示 env key、credential id 后缀或 revision。
- test fixture 不提交真实 key；golden fixture 使用固定假 key，并验证 request capture、错误和 rollout 均已脱敏。

### v1 到 v2 迁移

实现不保留 v1/v2 runtime 双读。升级入口在 provider settings lock 内执行一次独立的 disk 迁移：

1. 读取 v1 `env` map，为每个被 profile 引用的值写入确定性 disk credential id。
2. 构造只含 `credential_ref` 的 v2 文件，temp write、fsync、atomic rename。
3. 任一步失败时不替换 v1 文件，并恢复或删除本次修改的 disk credential；下次可幂等重试。
4. 凭据文件不可写、损坏或超过大小上限时停止迁移，要求用户修复 `$GOLUTRA_HOME` 或重新 `/auth`，不继续用明文 map 启动 live provider。
5. 迁移不包含 keyring backend、`OsKeychain` source 或 `os-keychain` wire alias，不访问系统钥匙串。

## OAuth 设计

OAuth 不能做成“任意 provider 通用登录”。OpenAI-compatible 只描述 wire 协议，不代表 endpoint 提供 OAuth。每个支持 OAuth 的 provider 必须注册受审计的 `OAuthProviderDescriptor`：

```text
OAuthProviderDescriptor
  provider_id
  flows: browser-pkce | device-code | openai-device-auth
  authorization_endpoint
  token_endpoint
  device_authorization_endpoint?
  revocation_endpoint?
  client_id
  scopes
  audience?
  browser_redirect_uri?
  authorization_params
  authorization_nonce
  openai_device_authorization?
```

- endpoint 必须使用 HTTPS，测试只允许 loopback HTTP。CLI/TUI 对受审计的内置 catalog 直接展示认证方式；外部 provider 仍须显式传入并校验 descriptor JSON。普通 Custom Provider setup 只支持 API key/env ref，不会从任意 base URL 推断 authorization/token endpoint。
- 浏览器流程使用 Authorization Code + PKCE S256。provider 未注册固定 callback 时绑定 `127.0.0.1:0`；OpenAI/xAI 等要求 allowlist callback 的 provider 使用固定 loopback host/port/path。两种路径都校验不可预测的 `state`、单次 code、redirect URI、callback path 和总超时，收到一次有效回调后立即关闭；无法打开浏览器时展示 URL。
- 只有 provider 明确支持 RFC 8628 时才展示 device flow；轮询遵守服务端 interval、`slow_down`、取消和总 deadline。
- 使用成熟 `oauth2` crate 完成 PKCE、code exchange 和 device flow，不手写协议参数或 token 解析。
- PKCE verifier、state 和 device code 只存在于 `AuthService` 内存，不写 runtime event、SQLite 或 rollout。
- access token 与 refresh token 作为一个 `oauth-token-set` 写入安全 `SecretStore`，从而让 CLI、TUI 和多个 Embedded 进程可在重启后复用；热路径另有带 expiry/revision 的进程内 access-token cache。没有 refresh token 时，access token 到期后明确要求重新登录。
- OpenID provider 返回的 `id_token` 不写入 provider config；auth 层只提取调用所需的非敏感 account id并作为 credential metadata 提供给 adapter。原始 access/refresh/id token 均不得进入 runtime event 或 rollout。

统一状态机由 `AuthService` 持有，CLI/TUI `/auth` 只驱动状态和渲染：

```text
Idle
-> AuthorizingBrowser | AuthorizingDevice
-> Exchanging
-> PersistingSecret
-> ProbingProvider
-> Ready | Failed | Cancelled
```

refresh 规则：

- access token 距过期不足 5 分钟时提前刷新；provider 返回 401 时允许强制刷新并重试原请求一次，禁止无限认证重试。
- 同一进程按 credential id single-flight；多 Embedded 进程再使用 `$GOLUTRA_HOME/auth/refresh/<credential-id>.lock` 串行化。获得跨进程锁后先重读磁盘 SecretStore，避免重复使用已轮换的 refresh token。
- provider 返回新 refresh token 时先覆盖安全存储中的完整 token set，再发布新的内存 access token；若未返回新 refresh token则保留已轮换记录。`invalid_grant` 会删除失效 token set并转为 `reauth_required`。
- `/auth logout` 在 provider 支持时先 revoke，再删除 disk credential 和 profile ref；revoke 失败也不能在本地继续保留可用凭据，错误以脱敏诊断返回。

内置 OAuth catalog 当前包含：OpenAI ChatGPT browser PKCE/headless device-auth、xAI browser PKCE/device code、GitHub Copilot device code。OpenAI headless 按 opencode 的私有 user-code -> poll authorization-code -> PKCE token exchange协议实现，不伪装成 RFC 8628。Vertex AI 优先复用 Google Application Default Credentials 或显式官方 descriptor；OpenAI-compatible、Anthropic 和 Gemini API key 模式不因为共用 HTTP adapter而自动获得 OAuth 选项。DigitalOcean implicit flow 未内置，因为 fragment token flow 不符合当前安全存储/授权码边界；GitLab/Poe 等继续通过 descriptor/registry 扩展，不伪装成通用 OAuth。

## 用户体验

### CLI

当前命令：

```bash
golutra provider protocols
golutra provider auth-methods [--provider openai-chatgpt|xai|github-copilot]
golutra provider current
golutra provider probe
golutra provider login --profile custom --base-url https://api.example.com/v1 --model model-id --api-key <key>
golutra provider login --profile ci --base-url https://api.example.com/v1 --model model-id --api-key-env PROVIDER_API_KEY
golutra provider set-key --profile custom --api-key <key>
golutra provider set-key --profile custom --env-key PROVIDER_API_KEY
golutra provider oauth-login --provider openai-chatgpt --method browser
golutra provider oauth-login --provider openai-chatgpt --method headless
golutra provider oauth-login --provider xai --method browser
golutra provider oauth-login --provider xai --method device
golutra provider oauth-login --provider github-copilot --method device
golutra provider oauth-login --descriptor oauth.json --flow browser --profile custom --base-url https://api.example.com/v1 --model model-id
golutra provider logout [--profile custom]
golutra provider use custom
```

行为：

- `provider login --api-key` 和 `provider set-key --api-key` 默认把输入写入 `$GOLUTRA_HOME/credentials.json` 并只在 `provider.json` 保存 SecretRef；不传 `--api-key` 的 login 使用 `--api-key-env` 只读引用，`set-key --env-key` 也只保存变量名。两条路径都不把 key 写入 provider config 或 runtime event。
- `provider auth-methods` 输出受审计的内置 provider auth registry。`provider oauth-login --provider/--method` 使用注册的 flow、协议和 API endpoint；内置方法不允许改成不匹配的协议或把 subscription token 发往其他 endpoint。`--descriptor` 继续作为自定义扩展入口并要求显式 flow/base URL/model。
- browser flow 自动打开浏览器或输出 URL，device flow 输出 verification URL/code。授权成功后先 probe，再激活 profile；失败会删除新 token set并恢复旧 profile/secret。
- `provider logout` 默认退出 active profile，也可指定 profile；支持 revocation 时先尝试 revoke，本地 credential 和 profile ref始终清理。
- `provider protocols` 输出内置协议、env key、baseUrl key、model key、probe 能力和 adapter 状态。
- `provider probe` 执行最小健康检查并只输出脱敏结果。
- 非交互环境下，如果缺凭据，返回可执行错误：缺哪个 envKey、当前 provider/model 是什么、如何设置。

首次进入策略：

- `golutra tui`：如果没有 ready live provider，打开 provider setup，并提供 Continue with mock。Web 首次 provider setup 不在当前范围。
- `golutra tui` 当前已实现 qwen-code 风格 provider 分组和 API key setup；流程为 group -> provider preset/protocol -> baseUrl -> credential storage -> API key/envKey -> model -> advanced -> review -> install。交互 key 默认写 `$GOLUTRA_HOME/credentials.json` 并保存 SecretRef，用户也可选择只读 env ref；workspace `.golutra` 不参与 provider/auth 配置。
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
- provider preset：OpenAI、OpenRouter、DeepSeek、Qwen/DashScope compatible、xAI、GitHub Copilot、本地 OpenAI-compatible
- auth method：OpenAI 展示 ChatGPT browser OAuth/headless OAuth/API key；xAI 展示 browser OAuth/device OAuth/API key；GitHub Copilot 只展示 device OAuth。选择 OAuth 后直接在后台启动授权，不再要求用户手写 descriptor。
- baseUrl input：必须以 `http://` 或 `https://` 开头
- credential storage：owner-only local disk 或已有 envKey；生产入口不提供持久化 ephemeral profile
- API key input：脱敏显示；默认保存到 `$GOLUTRA_HOME/credentials.json`，user provider config 只保存 SecretRef
- model：内置推荐模型选择，或输入自定义 model id
- advanced config：支持 Thinking、Reasoning effort、Context window、Max output tokens；字段保存到 profile 并随 active provider 生效
- custom headers：CLI 支持 `--header Name=Value` / `--header-env Name=ENV_KEY`，TUI Advanced Config 支持同一语义；profile 保存 literal 非敏感值或 env ref，runtime 只在请求末端解析
- review：展示 profile、baseUrl、model、advanced config、scope、保存路径、是否覆盖同名 profile，以及脱敏后的 `ProviderInstallPlan`
- slash auth：`/auth oauth-login` 在后台执行 browser/device OAuth并展示 URL/code，Ctrl+C 可取消；`/auth logout` 清理指定或 active profile

Custom Provider 的 protocol selector 已支持 OpenAI-compatible、Anthropic、Gemini、Vertex AI 和 genai；Custom 模式不预填官方 base URL，避免把私有 endpoint 误连到协议官网。

目标架构中，TUI 不直接写 runtime 状态，而是发送 provider command，由 RuntimeHost / config service 返回 `ProviderConfigured`、`ProviderProbeCompleted` 或 `ProviderAuthFailed`。当前 TUI setup 为了先闭环本地体验，会直接应用 `ProviderInstallPlan` 写入全局 provider config；这条路径必须继续保持脱敏 review 和 owner-only 权限约束。

### app-server / SDK

app-server 已支持运行时 auth required 事件，供已连接的 CLI/TUI/SDK 处理；这不是 Web 首次 onboarding：

```text
ProviderAuthRequired
-> client 根据 supported_methods 打开 API key 或 OAuth 安装流程
-> config/auth service 将 secret 写入 SecretStore，并完成 verified install/probe
-> client 只提交 request_id 与 ProviderConfigured/ProviderAuthSubmitted
-> RuntimeHost 重新解析 credential_ref，并从安全边界继续原 task
```

runtime command 不承载 bearer/basic/header/query 等明文 credential value。认证形状由受审计 provider profile、custom header env ref 和 OAuth descriptor 表达；所有 secret 都先进入 disk/env SecretRef，再由 provider adapter 在请求末端解析。响应可取消，取消写入 `ProviderAuthCancelled`，任务不能假装成功。

## 运行时动态认证事件

以下事件已用于“任务执行中缺少或需要重新配置凭据”的 app-server/SDK 协议，不属于 Web 首次 Connect Provider Flow：

| 事件 | durable | 说明 |
| --- | --- | --- |
| `ProviderAuthRequired` | 是 | 缺凭据或凭据过期，需要用户输入或登录 |
| `ProviderAuthSubmitted` | 是 | 用户提交凭据响应，只保存 credential id/revision |
| `ProviderAuthCancelled` | 是 | 用户取消凭据输入 |
| `ProviderConfigured` | 是 | provider selection/catalog 已更新 |
| `ProviderProbeStarted` | 是 | 开始健康检查 |
| `ProviderProbeCompleted` | 是 | probe 成功，记录 capabilities/latency/rate limit hints |
| `ProviderAuthFailed` | 是 | 认证失败，记录脱敏错误 |
| `ProviderRateLimited` | 是 | rate limit，记录 reset hint |
| `ProviderCredentialRefreshed` | 预留 | 当前 refresh 在 SecretStore/AuthService 内原子完成，不产生含 token 的 runtime payload；需要跨客户端刷新可见性时再启用仅含 credential ref/revision/expiry 的事件 |

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
- cache breakdown 按所选协议解释：Responses 的冷缓存零值必须保留；Chat Completions 或通用适配器未返回分项时保持 `unknown`，不能仅凭相同 base URL 假定 usage shape 相同。
- rate limit / quota 错误是否能归一化。
- provider 错误是否能映射为 `NotConfigured`、`RateLimited`、`ProviderFailed` 或 `MalformedResponse`。

probe 结果进入 capability matrix，但不能把一次 probe 成功当作永久保证。运行时仍要处理 401、403、429、5xx、timeout、malformed stream 和 truncated stream。

## 实施计划

### P0 兼容层

- 已保留 `GOLUTRA_PROVIDER_MODE=live` 快捷入口。
- 已新增 `GOLUTRA_PROVIDER_PROTOCOL`，协议选择优先于旧 mode 快捷入口。
- 已将 env 读取集中到协议配置解析入口，避免运行时散落读取 key/model/baseUrl。
- 已支持 `golutra provider protocols`、`golutra provider current` 和 `golutra provider probe` 作为 P0 脱敏检查入口。
- 已注册并实现 `mock`、`openai-compatible`、`openai-responses`、`anthropic`、`gemini`、`vertex-ai`、`genai` protocol catalog；adapter 只执行所选协议，不静默 fallback。
- 增加 provider config schema 草案和脱敏 snapshot。
- provider 错误统一写入 runtime event，避免只在 stderr 出现。

### P1 provider catalog

- 新增 Codex 风格全局 provider/auth config 文件。
- 实现 `provider list/current/use/probe/login/set-key`。
- 引入 `ProviderCatalog`、`ProviderSelection`、`ResolvedProviderConfig`。
- 引入 `ProviderInstallPlan`，确保 settings/env/runtime reload/probe 失败时可 rollback。
- TUI 接入首次 provider setup，不再让真实用户面对空白 mock 前端。
- 支持 OpenAI-compatible 自定义 endpoint。
- 已给 OpenAI-compatible 与四类 genai native 路由增加 tool calling、probe 和 wire golden tests；OpenAI-compatible SSE streaming 与模型目录 capability discovery 已进入 runtime event 主链。

### P1 SecretRef（已完成）

1. `golutra-auth` 已提供 `CredentialRef`、`SecretStore`、disk/environment/ephemeral backend 和内存 fake backend；disk backend 使用 owner-only 文件、跨进程锁和原子替换，内存中的 secret 使用 `secrecy` 包装。
2. `ProviderSettings` 已升级为 v2，profile 使用 `credential_ref` 且没有明文 `env` map；v1 只在持锁的一次性 disk 迁移模块中读取，常规 resolver 不双读。
3. `provider login/set-key/current/probe` 与 TUI review/install 已改为交互 API key 默认写 disk、CI 使用 env ref；secret/config/probe 处于同一 rollback 边界，替换成功会删除旧 disk credential。
4. `golutra-llm` 通过动态 `CredentialProvider` 在请求末端取值，401 最多进行一次强制凭据刷新和请求重试；runtime event/config 中不传递明文 secret。
5. 测试已覆盖替代 `GOLUTRA_HOME` 隔离、磁盘持久化、owner-only 权限、symbolic-link/损坏/超大文件拒绝、v1 disk 迁移/rollback、并发、credential replace/logout、缺失 secret、序列化和 redaction。

### P2 OAuth（已完成）与动态能力

1. 已增加 provider-specific `OAuthProviderDescriptor`、受审计的内置 auth catalog 和 `AuthService`，本地 fake OAuth server 覆盖动态/固定 callback、browser PKCE、额外 authorize params、nonce 与 device flow；普通 Custom Provider 不自动获得 OAuth。
2. 已实现持久 token set、内存 access-token cache、提前刷新、401 单次强制刷新、进程内 single-flight、跨进程 refresh lock 和 refresh-token rotation。
3. CLI `provider auth-methods/oauth-login/logout` 与 TUI `/auth` provider method picker、显式 `/auth oauth-login`、`/auth logout` 复用 `AuthService` 和 verified install/logout；OpenAI ChatGPT、xAI、GitHub Copilot使用内置 method，其他 provider 使用显式 descriptor 扩展。
4. 已覆盖取消、state/PKCE 校验、loopback callback、deadline、`invalid_grant`、revoke、重启读取、并发 refresh、OpenID account id、Responses SSE/reasoning replay、Copilot headers 和 secret/config 泄漏测试。
5. `genai` adapter 与 native protocol route 已完成，继续保持 adapter 反腐边界；provider model catalog auto-update 后置，更新必须可 diff、可 rollback、不可覆盖用户自定义模型。
6. 运行中动态认证已完成：task 进入 `WaitingAuthentication`，客户端执行 SecretRef/OAuth verified install 后仅提交 request id，runtime probe 成功再恢复；取消会终止等待任务。不会通过 command/event 传输明文 credential。

## 验收标准

最低可验收场景：

- 默认不配置任何 key 时，CLI/TUI/app-server 仍使用 mock provider。
- 设置对应协议 env 后，可以通过 OpenAI-compatible、Anthropic、Gemini、Vertex AI 或 genai endpoint 跑真实任务。
- 缺 key 时错误指出缺哪个 envKey，不泄露任何 secret。
- provider probe 失败不会修改当前 active provider selection。
- 本地 CLI/TUI OAuth、SecretRef 与跨 CLI/TUI/SDK 的 `ProviderAuthRequired` 等待/恢复/取消协议均可验收；Web 首次 provider onboarding 不在范围内。
- runtime event、artifact、projection、debug export 中没有明文 key。
- 重启后 provider selection 可恢复，secret 从 environment 或 `$GOLUTRA_HOME/credentials.json` 中的 SecretRef 解析。
- committed golden tests 不读取用户配置；专用 live env 配齐时，`just provider-live-smoke` 对目标 endpoint 发起最小真实请求。

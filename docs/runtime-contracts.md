# Runtime Contracts 架构规格

## 文档定位

本文档定义 Golutra runtime 最容易失真的硬契约：工具、provider、终止、取消、重试、回退和副作用边界。

主架构见 `ARCHITECTURE.md`。
第一阶段实现范围见 `implementation-blueprint.md`。

## 当前实现状态

截至 2026-07-15，第一阶段硬契约及扩展执行边界已进入可运行链路：

- `ToolContract` 使用唯一的 JSON Schema 校验输入；必填 path/pattern/edit search 拒绝空串，校验错误隐藏实例值，summary、structured facts 和 raw output 在持久化前统一脱敏；policy、workspace guard、approval、execution、artifact/evidence 顺序固定。
- shell 使用 `shlex` 解析结构化 argv，不经过 shell 解释器；policy 会阻断敏感路径、`find -exec/-delete`、`rg --pre` 等执行型参数，把 `sed -i`、`cargo run`、未知脚本等降为 Ask；执行器支持 timeout、`CancellationToken`、每管道 2 MiB 上限，并在 Unix 上终止整个进程组后排空管道。`golutra-sandbox` 在 macOS 使用 Seatbelt、Linux 检测 bubblewrap，外部 MCP 没有 OS-enforced sandbox 时拒绝执行。
- `RuntimeHost` 保存 task handle、pending turn queue 和 durable command ack；pause/resume/abort 影响真实执行，不是 UI 标记。终态 lane 拒绝控制转换；若 owner 已退出且新 host 成功取得 session lease，则先以 durable `TaskAborted` 回收孤儿状态，再把 `TurnQueued` 中尚未出现 `TurnStarted` 的输入转移到 recovery task。
- `AgentLoop` 支持多轮 assistant/tool message、LoopGuard、有限 retry/fallback 和 verification-backed terminal state。初始或工具消息累积导致的 context overflow 会产生 `LoopGuardTriggered` 和 Blocked/AskUser `LoopDecision`，不会降级成笼统执行错误。
- 文件工具和 shell 等可产生工作区副作用的工具在修改前捕获并持久化有界 before-image；checkpoint manifest 与 owner-only artifact blob 带 checksum、redaction 状态和 rollback metadata，持久化失败时不执行文件副作用。无法覆盖完整工作区时仍记录 checkpoint，但明确标记 `before_image_complete=false`，不得把它当作完整回滚保证。
- Embedded、Unix IPC 与 HTTP/SSE transport 使用同一 `SessionCommand`、`RuntimeQuery`、`RuntimeEvent` 语义和 protocol version；包含 `ToolProgress` 的当前 runtime protocol 为 v4，v3 reader 不与新事件流协商。用户级 daemon 通过 attachment 路由多个 cwd，Unix socket owner-only，HTTP 仅允许安全 endpoint 并校验 bearer/Host/Origin。SQLite 在 event append 事务内原子分配全局 sequence，host 再按提交顺序 publish；command lease 与 durable ack 负责重试去重，但 command ack 与后续业务事件仍不是一个跨运行时事务。
- 外部 MCP 工具必须来自 checksum 未变化的 reviewed/enabled plugin revision，远端 `tools/list` schema 必须与 manifest 一致；默认 Ask，批准前不启动进程，批准后继续经过 timeout/cancel/redaction/artifact/evidence，远端 annotation 不参与权限决策。
- provider 缺失会把 lane 置为 `WaitingAuthentication` 并产生 durable `ProviderAuthRequired`；客户端只提交 verified provider config 的 request id，secret 不通过 runtime command/event 传递。取消或 probe 失败都有显式终态。
- checkpoint 每 workspace 保留最近 20 个；artifact 维护按 retention/expiry 清理 blob，并保护仍被 lineage、verification 或 rollback 引用的记录。
- 工具生命周期统一使用稳定 `tool_call_id`：`ToolStarted` 写入脱敏且有界的展示参数，`ToolProgress` 只写可丢失的采样诊断，`ToolCompleted` 写入终态 envelope 和执行指标；硬执行错误也必须转换为 `ToolCompleted(error)`。展示参数保留 `path`、`command`、`pattern`、`query`、`symbol`、`timeout_ms` 等定位字段，`content`、`search`、`replace` 等正文只保留长度摘要，序列化结果硬限制为 8 KiB。shell 的 stdout/stderr 会完整 drain，管道消息队列、保留输出、diff preview 和 workspace before/after scan 均有硬上限。
- shell 扫描发现的新增、修改、删除文件进入 `changed_files`、before-image 和执行时捕获的 after-image；扫描越过文件数/内容预算、遇到排除目录或无法读取时写 `workspace_changes_known=false`。完整 unified diff 以 redacted、checksum、最多 2 MiB 的 `workspace_diff` artifact 持久化，结构化 diff preview 还有跨文件总预算，不能把未知状态解释为零变更。

## 核心原则

```text
工具多不等于 runtime 强。
契约不硬，状态就会漂，评估就会失真，回放就会断链。
```

## 第一阶段必做

第一阶段至少要把这些契约做成稳定结构：

- `ToolContract`
- `ProviderContract`
- `TerminalStateContract`
- `CancellationContract`
- `RetryContract`
- `FallbackContract`
- `SideEffectPolicy`
- `RuntimeLaneContract`
- `LoopGuardContract`
- `WorkspaceCheckpointContract`

第一阶段不要求把所有策略自动化做满，但必须有一致字段和明确语义。

## ToolContract

每个工具都要先有契约，再有实现：

```text
ToolContract
  tool_name
  input_schema
  output_schema
  error_schema
  side_effect_type: none | file | process | network | external_system
  idempotency_key_policy
  timeout_policy
  cancellation_policy
  retry_policy
  artifact_policy
  permission_policy_ref
```

最低要求：

- 输入输出必须可结构化验证。
- 错误不能只返回自然语言。
- 有副作用工具必须声明幂等与重试边界。
- raw output 默认进入 artifact，不直接进入模型。

### ToolExecutionObservationContract

```text
ToolStarted
  tool_call_id
  tool_name
  redacted_arguments

ToolProgress (sampled, diagnostic)
  tool_call_id
  phase: started | output | completed
  elapsed_ms
  output_bytes
  output_lines

ToolCompleted (durable, terminal)
  tool_call_id
  ToolResultEnvelope.status
  ToolExecutionMetrics
  changed_files / file_changes
  diff_previews / workspace_diff artifact ref
  workspace_changes_known
```

`ToolProgress` 不能替代终态，也不能单独触发 verification 或 promotion。消费者应按 `tool_call_id` 合并生命周期，并对缺失 progress 保持可用；只有终态事件和 artifact/evidence 才进入可审计完成判断。

## ProviderContract

Provider 是反腐层，不是简单 HTTP 封装：

```text
ProviderContract
  provider_id
  model_id
  native_protocol
  stream_event_mapping
  tool_call_mapping
  usage_mapping
  reasoning_mapping
  finish_reason_mapping
  error_mapping
  rate_limit_mapping
  cost_model
  capability_matrix_ref
  golden_fixture_refs
```

最低要求：

- 统一 stream event、tool call、usage、finish reason 和 error 语义。
- provider 原始字段要进入 debug / replay 上下文，而不是被静默吞掉。
- fallback 只能由 loop 层触发，provider adapter 不允许私自切换任务语义。
- OpenAI-compatible、OpenAI Responses 与 native provider wire 都必须经过 committed golden fixture；升级 HTTP client、SSE parser 或 rust-genai 时，wire diff 必须显式审查。
- live smoke 只能读取专用测试 env，不能隐式读取 `$GOLUTRA_HOME/provider.json` 或正常用户凭据。

## TerminalStateContract

任务终止必须结构化：

```text
TerminalStateContract
  stop_success
  stop_partial
  stop_failed
  blocked
```

要求：

- `stop_success` 必须绑定 `VerificationRecord`。
- `stop_partial` 必须说明缺失证据或残余风险。
- `blocked` 必须保留恢复入口，不允许伪装成失败完成。

## CancellationContract

取消不是 UI 动作，而是 runtime 状态转换：

```text
CancellationContract
  abort
  pause
  resume
  cancel_tool_call
```

要求：

- `abort` 后不得继续产生新的外部副作用。
- `pause` / `resume` 不能破坏 event 顺序。
- 工具取消必须有明确 `cancelled` 状态，而不是模糊错误。

## RuntimeLaneContract

运行中输入必须由 runtime 统一裁决：

```text
RuntimeLaneContract
  lane_scope: workspace_id + session_id + task_id
  busy_policy: append | inject | interrupt | reject
  active_controller_required
  safe_injection_points
  event_ordering_policy
```

要求：

- 同一 task 的 turn 默认串行，不能由多个入口并发推进。
- `append`、`inject`、`interrupt`、`reject` 都必须产生 runtime event。
- `inject` 只能发生在 provider call 前或工具安全间隙，不能中断副作用。
- 非 active controller 的控制动作必须走 `takeover` 或被拒绝。

## RetryContract

重试不能默认发生在隐式层：

```text
RetryContract
  retryable
  max_attempts
  backoff_policy
  idempotency_required
  retry_reason
```

要求：

- 只允许对显式声明可重试的步骤做重试。
- 有副作用的 tool retry 必须依赖幂等 key 或显式阻断。
- 每次 retry 都要写 `RuntimeEvent`。

## FallbackContract

Fallback 也必须结构化：

```text
FallbackContract
  trigger
  from_provider
  to_provider
  semantic_risk
  reason
```

要求：

- fallback 由 `LoopDecision` 记录。
- 不能因为 provider 适配细节不同而默默改变能力边界。

## LoopGuardContract

LoopGuard 是防止 agent 无界循环、无意义烧 token 和污染上下文的硬边界：

```text
LoopGuardContract
  repeated_tool_failure
  empty_response_recovery
  context_overflow_recovery
  max_iteration_policy
  retry_cost_policy
  oversized_tool_output_policy
```

要求：

- 同一工具连续确定性失败达到阈值后，必须改变策略、询问用户或 blocked。
- provider 空回复只能有限恢复，恢复用的 synthetic message 不能进入长期历史。
- context overflow 优先裁剪旧工具输出和低价值上下文；裁剪失败必须产生 `LoopGuardTriggered`，并进入 Blocked/AskUser `LoopDecision`。
- max iteration 后必须产生 `stop_partial`、`stop_failed` 或 `blocked`，不能无声结束。
- retry / fallback 的 token 和成本必须进入预算判断。

## SideEffectPolicy

副作用不是工具实现细节，而是核心治理对象：

```text
SideEffectPolicy
  resource_type
  risk_level
  approval_mode
  rollback_expectation
  halt_on_failure
```

最低要求：

- 文件、进程、网络、外部系统副作用都要显式标记。
- 高风险动作要能触发 approval gate。
- side effect 失败后是否中止任务，必须提前定义。

## WorkspaceCheckpointContract

coding agent 只要会改文件，就必须有工作区恢复边界：

```text
WorkspaceCheckpointContract
  checkpoint_type: shadow_git | snapshot | external
  changed_files
  ignored_patterns
  secret_exclusion_policy
  restore_hint
  retention_policy
```

要求：

- checkpoint 只能作为 Golutra 恢复网，不能修改用户自己的 `.git` 历史。
- checkpoint 和工具 policy 必须排除 `.git`、`.golutra`、`.gitignore` 命中项及 secret path；新文件也要在创建前形成 `existed=false` 恢复记录。
- 默认遵守 `.gitignore` 和 policy 排除规则，避免保存依赖目录、构建产物和敏感文件。
- checkpoint 失败不能让任务成功假象化；必须写入 event，并在必要时降级为 residual risk。
- 非文件副作用不能伪装成可回滚，必须单独记录补偿或不可回滚风险。

## Rollout、Fork 与 Rebind Contract

SQLite `runtime_events` 是 canonical facts，rollout JSONL 是可重建的历史物化层：

```text
RolloutEnvelope
  version
  thread_id
  session_id
  sequence_no
  checksum
  redacted_event
```

要求：

- 每行必须有版本和基于脱敏事件字节的 SHA-256 checksum；凭据字段和值要递归脱敏，但 token usage 等非凭据计数不能被破坏。
- rollout 目录和文件必须 owner-only；append、export 和启动重建共享跨进程锁，重建使用临时文件、fsync 和原子替换。
- fork 必须在一个 SQLite 事务内复制截止点历史，重新生成 EventId/TaskId/TurnId，并让 parent/child 后续事件相互独立。
- fork 不复制 immutable artifact blob；child 保留 artifact/evidence 引用，debug projection 必须能沿 lineage 读取。
- rebind 必须显式校验旧 canonical cwd，拒绝 active 或被其他 runtime 持有的 thread；checkpoint 只能标记为 `historical_only`，memory/evaluation 不自动迁移。

## P0 验收口径

第一阶段至少要验证这些契约没有漂：

- tool success / error / timeout / cancelled 都有稳定 envelope。
- provider truncated / malformed / rate limit 都能映射成结构化错误。
- abort 后没有后续 side effect。
- retry 不会重复制造副作用。
- fallback 不会绕过 `LoopDecision`。
- stop_success 不会绕过 `VerificationRecord`。
- running task 中的新输入不会绕过 `RuntimeLaneContract`。
- loop guard 不允许重复工具失败、空回复、context overflow 或 max iteration 形成无界循环。
- workspace checkpoint 不污染用户 `.git`，且敏感文件排除策略可验证。

# Runtime Contracts 架构规格

## 文档定位

本文档定义 Golutra runtime 最容易失真的硬契约：工具、provider、终止、取消、重试、回退和副作用边界。

主架构见 `ARCHITECTURE.md`。
第一阶段实现范围见 `implementation-blueprint.md`。

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
  sandbox_policy_ref
```

最低要求：

- 输入输出必须可结构化验证。
- 错误不能只返回自然语言。
- 有副作用工具必须声明幂等与重试边界。
- raw output 默认进入 artifact，不直接进入模型。

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

## P0 验收口径

第一阶段至少要验证这些契约没有漂：

- tool success / error / timeout / cancelled 都有稳定 envelope。
- provider truncated / malformed / rate limit 都能映射成结构化错误。
- abort 后没有后续 side effect。
- retry 不会重复制造副作用。
- fallback 不会绕过 `LoopDecision`。
- stop_success 不会绕过 `VerificationRecord`。

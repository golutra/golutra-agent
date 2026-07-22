# Runtime Stability and Recovery

## 目标

Golutra 的长任务稳定性不是靠无限重试，而是靠可恢复的事实链和明确的不变量：

```text
SessionCommand
-> RuntimeHost / StepMachine
-> provider + tool/process supervision
-> durable RuntimeEvent
-> terminal VerificationRecord
-> restart recovery / explicit reconciliation
```

CLI、TUI、App Server、SDK 和 MCP 只改变 transport 与展示方式，不各自维护任务状态机。App Server 可以长期运行并 attach 多个 canonical cwd；每个 cwd 仍是文件权限、历史和状态分区，不是独立 daemon。

## 稳定性不变量

1. `RuntimeEvent.sequence_no` 在同一持久化 store 中严格递增，消费者按 cursor 去重和续订。
2. 一个 task 只能有一个 durable terminal fact。`Completed` 必须来自 runtime verification，不由模型最终文本决定。
3. 已产生 `TurnStarted` 的 turn 在进程重启后不会自动 replay。
4. 未闭合的只读 tool 使旧任务进入 `Interrupted`；可能产生副作用的 tool/process 使其进入 `Uncertain`。
5. `Uncertain` 会阻断新 prompt 和 pending turn，直到显式写入 `TaskReconciliationRecord`。
6. reconcile 只能把旧任务变为 `Interrupted` 或 `Cancelled`，不能补写伪造的 `Completed`。
7. pending turn 只有在旧任务可安全结束或完成 reconcile 后才启动，并且每个 turn 只启动一次。
8. provider retry、stream reconnect 和 fallback 不得绕过 cancellation、预算或 verification。
9. shell/background process 的 stdout/stderr、运行时间和 journal 均有上限；timeout、cancel 和 terminate 必须形成终态。
10. SDK 的 terminal result 携带可选 `VerificationRecord`。旧历史没有该字段时保持兼容，但不能因此推断为 Pass。

## 长任务执行层

| 层 | 责任 | 失败处理 |
| --- | --- | --- |
| `StepMachine` | step checkpoint、无进展检测、总预算 | 不使用固定的短轮数上限；重复无进展或预算耗尽后结构化停止 |
| Context guard | token 预算、自动 compaction、tool pair 保留 | 无法保留最低上下文时阻断，不发送截断后语义不完整的请求 |
| `ProviderSession` | request timeout、stream idle timeout、retry、transport fallback、provider fallback | retry 带退避且可取消；只在契约允许时 fallback |
| Tool executor | policy、approval、JSON Schema、artifact/evidence | 所有成功、失败、timeout、cancel 都写 terminal tool fact |
| `ProcessSupervisor` | process id、bounded output journal、poll/stdin/terminate、process group | host drop、timeout 和 cancel 终止进程组；cursor loss 显式返回 |
| Runtime recovery | orphan task 分析、pending turn 恢复 | 自动恢复只针对从未启动的 pending turn；未知副作用必须人工对账 |

## 崩溃分类

| 崩溃点 | 重启结果 | 自动行为 |
| --- | --- | --- |
| command 已持久化但 turn 尚未开始 | durable pending turn | 可恢复并启动一次 |
| provider/read-only tool 执行中 | `Interrupted` | 不重放旧 turn；可继续 pending turn |
| write/shell/external tool 执行中，缺 terminal fact | `Uncertain` | 阻断队列，等待 reconcile |
| task 已有 terminal fact | 原终态 | 不创建第二个 terminal fact |
| post-task evaluation 执行中 | durable job lease recovery | 同 cwd worker 按 retry budget 接管 |

command journal receipt 与命令产生的全部业务事实当前不是单一跨模块事务，因此入口语义仍是 at-least-once。幂等 key、防重复 task、started-turn no-replay、checkpoint 和 reconciliation 共同约束这个窗口；不能把它表述成全局 exactly-once。

## 对账接口

三种 decision：

- `no_side_effect_observed`：外部检查确认副作用没有发生；旧任务记为 `Interrupted`，释放 pending turn。
- `side_effect_observed`：确认副作用已经发生；旧任务仍记为 `Interrupted`，避免再次执行同一 tool。
- `abandon`：放弃旧任务和后续恢复意图；旧任务记为 `Cancelled`。

入口保持同一语义：

```text
CLI:        golutra reconcile --decision <decision>
JSON-RPC:   task/reconcile
Rust:       AgentThread::reconcile_task
Python:     Thread.reconcile_task
TypeScript: Thread.reconcileTask
```

## 验收

常规回归：

```bash
cargo test --workspace --all-targets
just schema
just py-check
just ts-check
cargo check --workspace --all-targets
```

真实进程崩溃场景：

```bash
cargo test -p golutra-app-server --test cross_process \
  daemon_crash_holds_pending_turn_until_uncertain_task_is_reconciled \
  -- --test-threads=1
```

该测试在 `ToolStarted` 后终止 daemon，验证重启后的 `Uncertain`、对账前队列冻结、对账后 pending turn 恰好执行一次，以及旧副作用 tool call 不被 replay。

可配置 restart soak 默认 ignored，避免拖慢普通 CI：

```bash
GOLUTRA_SOAK_ROUNDS=100 \
GOLUTRA_SOAK_RESTART_EVERY=5 \
cargo test -p golutra-app-server --test cross_process \
  daemon_restart_soak_preserves_event_invariants \
  -- --ignored --nocapture --test-threads=1
```

soak 检查多轮 task/turn 数量、周期 daemon 重启、sequence 单调、每个 task 唯一 terminal fact，以及每个 turn 唯一 start fact。线上更长时间的稳定性还应记录进程 RSS、打开文件数、SQLite 大小、provider retry/fallback、cursor reconnect、post-task backlog 和 orphan recovery 次数；这些属于部署监控，不应靠单元测试伪装覆盖。

## 当前边界

- 前台 shell 在 daemon 被 `SIGKILL` 时无法证明外部副作用是否发生，因此必须进入 `Uncertain`，不能自动猜测。
- 多文件 rollback 会先校验全部 manifest，但不是跨文件系统事务。
- provider fallback 只能保证协议层终态和 attribution，不能保证第三方服务本身没有执行过请求。
- 常规测试验证分钟级行为；小时/天级资源泄漏、磁盘增长和第三方限流需要 opt-in soak 与部署观测共同验收。

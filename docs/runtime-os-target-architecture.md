# Runtime OS 目标架构与迁移设计

本文是 Golutra 当前 Runtime OS 的结构化目标定义。它把“运行时有很多事件类型”提升为一个可验证的应用架构：所有外部入口通过同一组应用服务提交 command、读取 projection、查看 trace；所有治理结论都引用 canonical facts，不能由模型自述或某个前端的临时状态替代。

## 1. 设计不变量

1. `RuntimeEvent`、artifact、evidence、context snapshot、checkpoint 和 durable job 是事实层；projection 可以删除后重建，不能反过来成为事实源。
2. 一个任务只有一个 `RuntimeHost` 执行所有 lane、provider、tool、approval、取消和事件提交；CLI、TUI、HTTP、IPC、SDK 不复制执行状态机。
3. 应用服务是前端唯一用例入口。前端不能直接访问 `RuntimeStore`、memory/evaluation 文件或 `AgentLoop`。
4. 任务终态和治理终态分离：任务可以先完成，`PostTaskJob` 再生成 review/candidate；candidate 必须经过独立 baseline/candidate execution 才能产生 promotion evidence。
5. 普通用户投影、开发者 trace、评估投影和原始 artifact 是不同 disclosure level；UI 不因为展示方便而把所有调试事实塞进对话历史。
6. workspace 是 cwd、权限和历史过滤边界，不是 runtime 进程边界。默认 embedded host 与显式用户级 daemon 共享同一 durable store 语义。
7. candidate 生成与 candidate 晋升必须分权。命令 producer 只能获得 Supervisor 管理的 worktree、脱敏 `CandidateRequest` 和独立 scratch；没有 OS-enforced sandbox 时拒绝启动，且永远不能直接访问网络、provider/release 凭据、evaluator 或 stable pointer。

### 1.1 三个平面不是同一个投影

Runtime OS 的“运行事实”“模型输入”和“开发者观测”必须明确分开。`RuntimeEvent` 是
canonical fact，不是 prompt；`ContextProjection` 是对实际模型输入的审计读模型，也不是
模型收到的 JSON；`DebugProjection`、`EvaluationProjection` 和导出 bundle 都在模型边界之外。

```text
Runtime OS control plane
  owns session/lane/turn/tool/verification/recovery and terminal state
  decides which bounded facts may become model input

Model boundary
  RuntimeHost -> compile_model_input -> ModelInputEnvelope -> Provider
  provider receives only the approved ProviderRequest and tool contracts
  it cannot query RuntimeEvent, DebugProjection, EvaluationProjection or promotion state

Observation / governance plane
  RuntimeEvent + artifacts -> State/User/Debug/Context/Evaluation/Trace projections
  post-task review -> candidate -> regression -> promotion runs out of band
  its failures are diagnostic facts and never rewrite the active task result
```

`ModelInputVisibility` 是消息级的硬门禁：只有 `ModelVisible` 能进入
`ModelInputEnvelope`；`ObservationOnly` 与 `GovernanceOnly` 即使 payload 中有可读文本也会被
拒绝。验证反馈进入模型时只能通过当前 turn 的有界 correction envelope，不能把整个治理投影
回灌为上下文。事件的 `RuntimeEventClass` 是由 `event_type` 派生的路由/统计分类，不能替代
实际 projection 的 allowlist，也不等于披露权限；最终权限由显式 projection allowlist 和消息
visibility 决定。`compile_model_input` 还要求 message 与 source 一一对应；缺失 source 时直接
阻断 provider 调用，不能为未知消息推断一个 model-visible 来源。

## 2. 分层架构

```text
Interaction Plane
  CLI / TUI / HTTP / Unix IPC / TypeScript SDK / Python SDK
                         |
                         v
Application Plane
  RuntimeApplication (GovernedRuntime facade)
    RuntimeCommandService      RuntimeQueryService
    RuntimeSessionService      TaskTraceService
    RuntimeGovernanceService   PostTaskCoordinator
                         |
                         v
Execution Plane
  RuntimeHost
    RuntimeLane + AgentLoop + Cancellation/Approval
    ContextBuilder + ProviderRouter + ToolExecutor + Policy
    RuntimeVerificationService + trace adapter
                         |
                         v
Canonical Fact Plane
  RuntimeEventLog       Artifact/Evidence repositories
  ContextSnapshot       Checkpoint repository
  Durable PostTaskJob   Thread/Session repository
                         |
              +----------+-----------+
              v                      v
Projection Plane                 Improvement Plane
  State / User / Debug             PostTaskCoordinator
  Context / Evaluation             ImprovementCandidate
  Trace / Audit                    RegressionService
                                   PromotionDecision
                                          |
                                          v
                              Optional P3 Control Plane
                              Supervisor / CandidateBroker
                              OS-sandboxed producer / TrustedBuilder
                              immutable release / canary / rollback
```

### 2.1 Interaction Plane

入口只做输入、认证、协议编码和渲染：

- TUI 只消费 `UserProjection`；`--debug` 和显式 trace 入口才读取有界 Debug/Trace 数据。
- CLI/SDK 发 `SessionCommand`、`RuntimeQuery` 和 `TaskTraceRequest`，不自己推断任务成功。
- app-server 提供同一套 command/query/trace/SSE 语义；客户端先通过认证后的 `/runtime/info` 协商协议版本，SSE 先按 cursor replay，再订阅 live event。
- embedded、Unix IPC、HTTP/SSE 只是 transport 差异，不能演化成三套 runtime 逻辑。

### 2.2 Application Plane

`golutra-client::RuntimeApplication` 是当前实现的 in-process facade，`GovernedRuntime` 是文档中的稳定别名。它组合以下服务：

| 服务 | 责任 | 不负责 |
| --- | --- | --- |
| `RuntimeCommandService` | 校验、幂等、提交 command | 直接执行 provider/tool |
| `RuntimeQueryService` | state/user/debug/replay/page 查询和 live subscribe | 修改 lane 或 memory |
| `RuntimeSessionService` | cwd 绑定、thread list/resume/fork/rebind、recovery | 自己复制 session 状态机 |
| `TaskTraceService` | 分页拼装 event/context/artifact/evidence/job，集中执行全页聚合、summary/full/forensic disclosure 和完整性判断 | 修改任务或生成 promotion 结论 |
| `RuntimeGovernanceService` | 将治理命令和 trace 读取统一纳入应用边界 | 绕过 verification/promotion gate |
| `PostTaskCoordinator` | durable job 的 enqueue/claim/retry/recovery | 占用 active task lane |

现有 `EmbeddedTransport` 已通过 `RuntimeApplication` 的 command/query/session/trace 路径工作；它保留 `RuntimeHost` 引用只用于同 crate 的兼容测试和 host 生命周期管理，前端公开入口不应依赖该引用。

### 2.3 Execution Plane

`RuntimeHost` 是执行 owner：

- 持有 `RuntimeLaneManager`、任务 handle、`CancellationToken`、pending turn queue、approval waiter 和 EventBus。
- 在执行前校验显式 `TaskContract`，包括 workspace change、delivery path、verification independence 和 correction 上限；兼容旧客户端的 prompt 推断只存在于 application adapter，不能进入核心终态判断。
- 将每个 provider/tool/context/verification 阶段转成 `AgentLoopTraceEvent`，由 host adapter 写入 `RuntimeEvent` 和 artifact。
- `RuntimeVerificationService` 只接受结构化 `VerificationInput`，由 `golutra-verify` 产生 assertion status；模型不能直接写 Pass。
- context guard、completion policy、provider retry/fallback 和 trace adapter 在 `golutra-runtime` 的独立模块中，loop orchestration 只负责顺序和控制流。

### 2.4 Canonical Fact Plane

`golutra-store::RuntimeRepositories` 将 SQLite 逻辑边界明确为五个 repository：

- `EventRepository`：append、cursor page、integrity、sequence。
- `ProjectionRepository`：state、user、debug projection。
- `ArtifactRepository`：blob、range read、context snapshot、verification plan、evidence；读取 blob 前必须先由 application 层用 artifact session 校验 cwd ownership。
- `DurableJobRepository`：按 workspace 原子 claim 的 post-task lease、retry、recovery、terminal result；一个 Host 不能消费其他 cwd 的 job。
- `ThreadRepository`：thread/session index、fork、workspace ownership。

这些 repository 当前共享一个 `RuntimeStore` 连接和事务实现，目的不是制造第二个数据库，而是阻止 application service 依赖 SQL pool 或把所有事实访问集中到一个无边界对象。后续需要拆物理存储时，服务接口无需变化。

### 2.5 Projection Plane

同一组 canonical facts 必须按消费目的投影，不能让一个大 JSON 同时承担控制、模型输入、用户展示和评估：

| Projection | 消费者 | 完整性边界 |
| --- | --- | --- |
| `StateProjection` | Runtime control、CLI status | 当前 lane、task status、controller、verification；可从 event 重建 |
| `UserProjection` | 普通 TUI/Web 对话 | 只含用户可理解的步骤、终态、回复和 residual risk |
| `ModelInputEnvelope` | provider 边界 | 唯一实际发送给模型的 `ProviderRequest`；只含通过 visibility 和 context budget 的消息与 tool contract |
| `ContextProjection` | 模型输入审计 | 对实际 request 的类型化 `ContextSnapshot`、digest、贡献者/消息/tool schema manifest；它不自动进入模型 |
| `DebugProjection` | 交互式开发调试 | 最近有界事件窗口，不承诺完整历史 |
| `EvaluationProjection` | post-task 与 promotion 治理 | review/result/candidate/regression/decision/job 的类型化生命周期，不解析事件文案 |
| `TaskTracePage` | CLI/SDK/Audit/Supervisor | 按 cursor 关联完整事实并返回 `TraceIntegrity`；summary/full/forensic 是披露级别，不是三套事实 |

`summary` 省略 context、artifact 和 evidence 明细并净化 event payload；`full` 返回脱敏 manifest；`forensic` 只允许 owner-only local IPC 或 embedded 调用。restricted capture 未启用或数据被 retention 清理时，必须在 `retention_losses` 中说明并令 `complete=false`。

## 3. 一次任务的完整生命周期

```text
command
  -> command journal / SessionCommandReceived
  -> RuntimeLane start or queue turn
  -> ContextBuilder + ContextSnapshot (redacted request artifact)
  -> Provider stream / retry / fallback
  -> Policy + approval
  -> before-image checkpoint
  -> Tool artifact + structured evidence + model excerpt
  -> VerificationPlan + VerificationRecord
  -> bounded correction envelope or terminal LoopDecision
  -> TaskCompleted / TaskAborted (Runtime OS terminal result)
  -> post-terminal memory quarantine + minimal evaluation
  -> durable PostTaskJob scheduling barrier, then deep evaluation
     (startup reconstructs a missing job from a pending terminal fact)
  -> UserProjection (ordinary UI) / Debug-Trace-Evaluation projections (out of band)
  -> PostTaskReview / ImprovementCandidate
  -> task-level per-case isolated baseline/candidate RegressionResult
  -> version-level stable/candidate eval-worker paired execution
  -> PromotionDecision (human/control-plane gate)
```

provider/runtime 失败也必须在运行时终态决策前生成固定失败 `VerificationPlan` 和
`VerificationRecord(Fail)`，再落下 `TaskAborted`/`TaskCompleted` 等 runtime terminal fact；
不能因为 loop 提前返回错误而跳过验证。Task terminal fact 一旦持久化，后置 memory/evaluation
失败只能记录 `PostTaskStageFailed`、`PostTaskJobFailed` 或 integrity warning，不能改写用户任务状态。settled trace
会等待本地 supervisor 完成“治理已调度或调度失败”的屏障，再判断 job/evaluation 是否完整。
若进程在 terminal fact 与 job enqueue 之间退出，Host 启动扫描必须按 workspace 找出 pending
terminal fact，并通过原子幂等 enqueue 补建 job；不能把这个崩溃窗口误判成治理已完成。
每个已完成 regression 都必须形成显式 `PromotionDecision`，包括 `Reject` 和
`NeedsHumanReview`，不能只靠 candidate status 暗示治理结论。

任何箭头缺失都必须在 `TaskTracePage.integrity` 中表现为 `missing_sections`、`unresolved_refs` 或 `retention_losses`，不能返回看起来完整的成功 JSON。post-task job 尚未进入 `Succeeded/Failed/Cancelled` 或 evaluation projection 尚未 terminal 时，最后一页必须保持 `complete=false`。每个引用 artifact 不只检查 manifest：trace 读取还会验证实际 blob 的 size/checksum；retention 已删除的 blob 进入 `retention_losses`，磁盘篡改直接报 integrity failure。

## 4. 改进闭环与反过拟合

改进不是“失败后让模型再写一版 prompt”，而是独立控制面上的可审计状态机：

```text
TaskTraceBundle
  -> FailureTaxonomy / PostTaskReview
  -> ImprovementCandidate (frozen digest)
  -> RegressionCampaign
       each case_ref -> baseline execution + candidate execution
       held-out cases + budget + environment recipe
  -> RegressionResult
  -> PromotionDecision
  -> immutable release -> canary -> rollback
```

必须同时满足：

- source task、holdout task、generated task 分离，holdout disclosure 有预算且不可由 candidate 读取答案。
- candidate 不能修改 evaluator、sandbox、signer、promotion control plane；TrustedBuilder 只读 candidate source，使用独立 scratch target 和 artifact staging。
- promotion 比较质量、成本、延迟、失败率、资源和安全 gate；单个测试集通过不能晋升。
- observation cluster 和 epoch 都绑定 source release；当前 stable pointer 已变化时，旧版本 opportunity 必须重新观察，不能直接在新版本上生成候选。
- 每个候选保留 candidate digest、campaign、逐 case baseline/candidate trace ref、verification ref 和 decision ref；子 runtime 的完整 trace 与引用 blob 在 TempDir 回收前打包成父 workspace 的 content-addressed `regression_trace_bundle`，任一 case 缺 durable pair 只能 `NeedsReview`。
- RuntimeHost 内的 `regression_trace_bundle` 用于 prompt/config/tool 等任务级候选；涉及 runtime 源码的版本候选不能复用同一个已编译 Host。Supervisor 必须从 stable release 和 candidate evaluation build 分别启动 sealed `golutra-eval-worker`，在独立 home/workspace 中执行相同 case，并把完整 trace、artifact blob、binary checksum、workspace digest 和外部 assertion 结果持久化为 `artifact://supervisor-evaluation/...`。
- memory 只从独立 evidence 进入 quarantine，不能把候选修改或单次成功直接写成 active memory。

### 4.1 观测不能反向污染模型

观测链路可以完整，但完整不代表全部回到 prompt。每次 provider 调用至少有两条结果：

1. `ModelInputEnvelope.provider_request`：实际发送的、经过 allowlist/visibility/budget 检查的输入。
2. `ContextProjection` / `TaskTracePage`：供开发者、审计和评估读取的脱敏事实及完整性信息。

后者可以包含 event、artifact ref、verification、token、job 和治理状态，但这些字段不会因为
被持久化或被 TUI 展开而自动成为下一轮的历史。下一轮 context 只从明确允许的 user/assistant/
tool facts、项目指令、受治理 memory 和当前 turn correction 重新编译；hidden visibility 在
compaction summary 中也必须保持 hidden。

## 5. crate 映射

| 层 | crate | 核心边界 |
| --- | --- | --- |
| domain facts | `golutra-core`, `golutra-protocol` | ID、event、command/query、verification/eval schema |
| fact storage | `golutra-store` | SQLite migrations 和五类 repository |
| execution | `golutra-runtime` | lane、AgentLoop、control、trace adapter、verification service |
| application | `golutra-client` | RuntimeApplication、RuntimeHost 生命周期、post-task/evolution/regression use cases |
| provider | `golutra-llm`, `golutra-auth`, `golutra-config` | protocol adapter、OAuth、disk SecretRef、probe/rollback |
| tools/policy | `golutra-tools`, `golutra-policy`, `golutra-sandbox`, `golutra-mcp` | contract、approval、sandbox、artifact/evidence |
| governance | `golutra-memory`, `golutra-eval`, `golutra-evolution`, `golutra-governor`, `golutra-verify` | memory lifecycle、review、regression、candidate、promotion |
| control plane | `golutra-supervisor`, `golutra-release` | candidate freeze、trusted build、release pointer、canary、rollback |
| interaction | `golutra-cli`, `golutra-tui`, `golutra-app-server`, SDKs | transport client、render、HTTP/SSE、generated protocol |

## 6. 痛点与强制机制

| 目标痛点 | 架构机制 | 可验收结果 |
| --- | --- | --- |
| agent 黑盒 | canonical RuntimeEvent + TaskTrace integrity | 任一步都能定位 sequence、source、artifact/evidence ref；缺失不会静默 |
| 上下文膨胀 | ContextBuilder budget、durable summary、ContextSnapshot | provider request 不随历史线性增长，被裁剪内容仍可追溯但不会自动回灌 |
| 工具输出污染 | ToolResultEnvelope 三分：summary / structured facts / raw artifact | 模型只接收有界 excerpt，完整输出按 checksum/range 读取 |
| 完成误判 | VerificationPlan/Assertion/Record + completion hard gate | 模型自然语言不能直接产生 Pass；失败路径也有 Fail record |
| 用户与调试混杂 | User/Debug/Context/Evaluation/Trace 独立投影 | 普通 TUI 不展示治理噪声，开发入口仍可获得完整事实 |
| 凭感觉改 prompt | PostTaskReview -> frozen candidate -> paired regression | candidate 必须引用独立 baseline/candidate trace 与 verification |
| 长期记忆污染 | structured MemoryClaim + quarantine/expiry/invalidation | 单次任务不进入 active retrieval，错误反馈可回滚 |
| 自动改进失控 | sealed control plane + PromotionDecision + trusted build/canary/rollback | candidate 不能修改 evaluator/sandbox/pointer，未经决策不能发布 |

## 7. 当前重构状态与稳定边界

本轮已完成：

- `RuntimeApplication/GovernedRuntime` facade 和 command/query/session/trace/governance service seam。
- Embedded transport 主路径迁移到 facade。
- `TaskTraceService` 从 client 巨型 host 文件中独立出来，并通过 repository 读取事实。
- `RuntimeRepositories` 五类逻辑边界，以及 post-task/trace 对 repository 的接入。
- `golutra-runtime` 的 context guard、completion policy、provider retry、trace adapter、verification service 文件拆分。
- `golutra-client` 已按 application、command、query、session、execution、execution trace、post-task、governance、regression 和 task trace 拆分；`RuntimeHost` 根模块只保留 owner 状态和共同基础设施。
- `ContextProjection`、`EvaluationProjection` 和 `TaskTracePage` 已进入 Rust protocol、schema、TypeScript/Python SDK。
- `ModelInputEnvelope` 已成为 Runtime OS 到 provider 的唯一边界；`compile_model_input` 对 typed visibility 和 legacy hidden labels 双重拒绝，`ContextProjection` 只作为实际请求的审计投影，不是模型上下文。
- `RuntimeEventClass` 由 `event_type` 派生并作为 control/execution/memory/evaluation/governance 的统计/路由分类；模型历史和普通用户投影仍使用更窄的显式 allowlist，不能用 event class 代替披露权限。
- `EvaluationProjection` 会逐条核对 review/result/candidate/regression/decision 是否存在对应 canonical event；secondary governance store 已落盘但事件提交中断时会产生 integrity warning，`TaskTrace.complete` 不会静默成功。regression/decision 事件绑定 source task，trace 会解析其受控 artifact refs，将完整 child trace bundle manifest 纳入同一审计页。
- CLI、Rust client、TypeScript SDK 和 Python SDK 都提供 bounded all-pages trace 聚合；cursor 不前进、页身份不一致或超过页数上限会显式失败，regression 也不再把第一页冒充完整 trace。
- success/failure facade 端到端测试覆盖 command -> task -> verification -> durable post-task -> candidate -> regression -> explicit promotion decision；失败任务同时断言不会污染 memory。
- app-server attach 不再硬编码协议版本；HTTP forensic disclosure 被拒绝，owner-only Unix IPC 由 server-side transport marker 识别。
- durable post-task worker 的 claim 已纳入 workspace partition；跨 workspace 压力回归同时验证 foreign job 未增加 attempt 且 thread rollout path 不被其他 Host 改写。
- RegressionCampaign 按每个 durable `case_ref` 建立独立 baseline/candidate workspace 与 home，`RegressionExecution.case_ref` 固定归属；完整子 trace/artifact blob 被持久化为 governance artifact 后临时 home 才可回收，缺任一执行 pair 会持久化 `NeedsHumanReview` decision。
- Supervisor 只公开 `observe_trace` ingestion；直接提交 `ObservationBundle` 的 CLI/API 状态入口已删除。launcher 注入的 release ID 同时写入任务起始事件和 `TaskTracePage`，ingestion 不接受外部 source-version 标注，并重算 event count、sequence range 和 chain digest，拒绝 runtime identity 不一致、summary、未聚合分页、task/session 错配以及缺 verification/context/job 的 trace，不能信任调用方声明的 `TraceIntegrity`。
- internal/external command producer 在执行前核对 frozen epoch/opportunity 和 canonical worktree ownership；进程强制使用 macOS Seatbelt 或 Linux bubblewrap、关闭网络、清空敏感环境并使用独立 scratch。stdout/stderr 并发排空且分别限制为 2 MiB，超时或超限时终止整个进程组；process-only backend 直接拒绝。
- Supervisor 通过 `prepare-worktree` 从 epoch 的 immutable parent release source 创建候选目录；worktree 请求在任何 untrusted producer code 启动前完成 containment、symlink 和规模检查，producer 输出的 epoch/worktree/kind 仍由控制面覆盖。冻结时控制面逐文件比较 parent source，真实 changed paths 必须全部通过 allowlist 且被声明；声明值不能掩盖 sealed 文件的新增、修改或删除。
- 版本级 `evaluate` 不再接受调用方生成的 `EvaluationInput`。输入是包含五类 partition、fixture 和可信 assertion 的 `RuntimeEvaluationSuite`；assertion、partition、真实 case id 和 holdout 答案留在 Supervisor，stable/candidate worker 只收到 objective、payload 和 fixture。Supervisor 外部重算 workspace digest、验证完整 trace/VerificationRecord 和 artifact blob 后，才内部生成 gate input。
- `build` 不再接受外部 `BuildReport`；`evaluate` 运行有界 TrustedBuilder，并把同一组 binary/report 同时用于 candidate execution 和后续 release，避免重新编译未评测 binary。独立 `check` 使用临时 staging，不能覆盖评测产物。TrustedBuilder timeout/输出超限会终止整个进程组。
- Supervisor 的 `pending.json -> control.jsonl fsync -> state.json atomic replace` journal 与 release 的 `deployment.pending.json -> deployment.jsonl -> pointer materialization` journal 可恢复日志/指针任一阶段的崩溃和末尾 partial append；source/bin 通过同文件系统 staging 原子发布，bin 已 rename 但 manifest 未更新时可按 trusted report 恢复。release manifest 读取有大小上限，并复核目录 identity、source tree、可执行位和全部 binary artifact。
- promotion 要求当前 release 至少一条且全部健康的 canary observation；rollback 只允许当前 canary 或在无 active canary 时当前 stable 所属候选。

保留在 `RuntimeHost` 的内容是有意的稳定边界：lane/worker 生命周期、事件事务锁、task supervision、EventBus 和 sequence owner 必须保持同一 owner，不能为了文件变小而拆成可互相竞态的对象。`PostTaskCoordinator`、regression 和 memory governance 已通过独立模块/服务编排，但写操作仍经过 command/event/repository，不得新增旁路状态。

## 8. 验收命令

```text
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo fmt --all -- --check
just schema
just fixture
just ts-check
just py-check
```

治理链的最小验收必须同时检查：

1. event sequence 单调且 trace integrity 可解释。
2. context snapshot 有脱敏 request artifact，工具大输出只通过 artifact/excerpt 进入模型。
3. verification plan/record 存在，任务状态不由模型最终文本决定。
4. post-task job 在 host 退出后可 claim/retry/recover。
5. candidate 的每个 case 都有独立 baseline/candidate execution refs；缺 pair 形成显式 review decision，memory 保持 quarantine。
6. summary/full/forensic 披露符合 transport 权限，协议版本由 runtime info 协商而不是前端常量漂移。
7. Supervisor 重新验证 trace 完整性；candidate producer 无网络、无控制面文件访问、无 process-only fallback，并且 timeout/output overflow 后不存在遗留进程。
8. runtime 源码 candidate 的 paired refs 来自两个不同 checksum 的 stable/candidate eval-worker 执行证据；伪造 evaluation/build report 没有公开提交入口。
9. Supervisor 中断事务可由 pending journal 恢复，release source/manifest/binary 任一磁盘篡改都会阻止 launcher、preview、promotion 或 rollback。
10. 模型 boundary 测试证明 Evaluation/Governance facts、DebugProjection 和 hidden context 不会进入 provider request；compaction 不会把 hidden fact 升格为 model-visible。
11. `TaskCompleted` 先于 post-task governance 持久化；治理调度/worker 失败不会改写已验证的 runtime terminal status，settled export 不会因竞态丢失后置 job。

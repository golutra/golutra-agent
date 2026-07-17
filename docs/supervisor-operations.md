# Golutra Supervisor 运维与自进化流程

## 目的

`golutra-supervisor` 是独立于普通 RuntimeHost 的控制面。它只消费完整、脱敏的 `TaskTracePage`，在隔离 worktree 接收 internal/external producer 的代码候选，并负责密封评测、可信构建、preview/canary、稳定指针和回滚。

普通 CLI/TUI 不启动 Supervisor，也不展示这些治理状态。当前任务始终由启动它的版本完成；`golutra-launcher` 只在下一次启动时读取新的 stable pointer，不在 active task 中途替换进程。

## 持久化位置

建议使用：

```text
$GOLUTRA_HOME/evolution/supervisor/
  state.json
  control.jsonl
  pending.json                  # 仅在中断事务恢复期间存在
  supervisor.lock
  worktrees/<candidate_id>/
  artifacts/
  evaluation-runs/              # 临时 baseline/candidate workspace，完成后删除

$GOLUTRA_HOME/releases/
  releases/<release_id>/release.json
  releases/<release_id>/source/
  releases/<release_id>/bin/
  stable
  previous-stable
  preview
  canary
  deployment.jsonl
  deployment.pending.json       # 仅在指针事务恢复期间存在
```

所有状态和指针在 Unix 上使用 owner-only 权限。Supervisor 事务先写 `pending.json`，再 fsync `control.jsonl`、原子替换 `state.json` 并删除 pending；release pointer 事务同样通过 `deployment.pending.json -> deployment.jsonl -> pointer materialization` 恢复。两套 journal 都能识别并修复中断的末尾 append；Supervisor 重启时还会以已验证的 deployment log/pointer 为准修复 candidate/epoch 镜像状态。release source 和 binary 先在同文件系统 staging 中完整复制、复算 checksum/size/可执行位，再原子发布；若崩溃发生在 bin rename 后、manifest 更新前，下一次 checked build 只在全部 binary 与 trusted report 一致时接管该目录。`control.jsonl` 与 `deployment.jsonl` 都是 SHA-256 前向 hash chain；候选 worktree 的 allowlist 不包含 Supervisor、release、eval-worker、evaluator、protocol、verifier、policy、sandbox、CI、签名和 stable pointer。

## 有限状态流程

```text
完整 TaskTrace
-> observe / opportunity
-> start-epoch
-> internal 或 external producer
-> frozen candidate digest
-> stable eval-worker 与 candidate eval-worker 真实配对执行
-> Supervisor 外部断言 + sealed/fresh/security/migration gate
-> OS-enforced trusted build
-> content-addressed release
-> preview
-> canary
-> promote 或 rollback
-> 下一次 launcher 启动稳定版本
```

每个 epoch 都有 `max_candidates`、generation/token/cost/build/holdout/canary 预算和 deadline。没有重复问题、没有质量提升、holdout 预算耗尽、结果不确定或任一 hard gate 失败时必须进入终态，不会自旋。

## 命令入口

以下命令都需要显式控制面目录：

```text
SUPERVISOR_ARGS="--root $GOLUTRA_HOME/evolution/supervisor --releases $GOLUTRA_HOME/releases"
```

查看状态和校验控制日志：

```text
golutra-supervisor $SUPERVISOR_ARGS status
golutra-supervisor $SUPERVISOR_ARGS verify-log
```

第一次运行控制面时，先把当前受信源码构建为初始 stable release。该命令只允许执行一次，并要求 Seatbelt/bubblewrap、离线 Cargo、完整测试和 `golutra-eval-worker` 产物全部通过：

```text
golutra-supervisor $SUPERVISOR_ARGS bootstrap /path/to/golutra-agent
```

CLI 的完整 trace JSON 可以直接进入观察器：

```text
golutra --cwd /workspace trace --task-id <task-id> --full --wait-evaluation > task-trace.json
golutra-supervisor $SUPERVISOR_ARGS observe-trace task-trace.json \
  --independent-group <run-id>
```

`golutra-launcher` 会把实际 stable release ID 写入进程环境；RuntimeHost 将该 identity 固定到任务起始事件和 `TaskTracePage`。Supervisor 从 event chain 验证 identity，不接受调用方填写 `--source-version`。只有同一个 source release 上跨独立任务重复的 failure cluster，或该 release 上单次确定性 security/integrity/recovery 缺陷，才会生成 opportunity。单条普通失败只记录 observation；source version 会进入 cluster identity，且 `start-epoch` 只接受与当前 stable pointer 相同的 source release，旧版本证据不能直接驱动新 stable 的代码修改。

启动有限 epoch，并由 producer 生成候选：

```text
golutra-supervisor $SUPERVISOR_ARGS start-epoch <opportunity-id>
golutra-supervisor $SUPERVISOR_ARGS prepare-worktree <epoch-id> <candidate-id>
golutra-supervisor $SUPERVISOR_ARGS produce candidate-request.json \
  --kind internal --program /path/to/producer --arg <producer-arg>
```

`prepare-worktree` 从 epoch 绑定的 immutable stable release source 原子创建候选目录；producer 不得自行拼装一个不完整源码树。producer 从 stdin 接收 `CandidateRequest` JSON，只能向 stdout 返回 `CandidateProposal` JSON。Supervisor 在启动任何 producer code 前先核对 epoch/opportunity、source version，并要求 canonical worktree 位于自己的 `worktrees/` 下；输出中的 epoch/worktree/producer kind 会被控制面覆盖。冻结时 Supervisor 逐文件比较 stable source 与候选目录，自己计算真实 changed paths；每个实际变化都必须属于 allowlist 并被 proposal 声明。仅伪造 `target_paths`、暗改 sealed 文件、声明未变化路径、删除 sealed 文件、symlink、文件数/总大小越界或 tree digest 不一致都会被拒绝。

命令 producer 必须运行在 macOS Seatbelt 或 Linux bubblewrap 中，process-only fallback 会直接失败。sandbox 只把 candidate worktree 以读写方式挂载，并提供独立 scratch；网络关闭，继承环境会移除 key、token、credential、password、authorization 和 `$GOLUTRA_HOME`。stdout/stderr 会并发持续排空且每条管道最多保留 2 MiB，任一管道超限或达到 timeout 都会终止整个进程组。需要远端模型的 internal evolver 应由受信 RuntimeHost 持有 provider credential，并只把脱敏请求和受限 worktree 工具暴露给 candidate；不能把长期 OAuth/API key 注入命令 producer。

当前 `InternalCommandProducer` 是离线 command adapter，不会自己调用已配置 LLM。常驻 scheduler、RuntimeHost-backed internal evolver、指标采集、active-process drain 和自动 rollback orchestrator 尚未作为 Supervisor 命令实现；现阶段这些步骤由受信部署调用方显式推进，文档和状态机不会把它们报告为已自动完成。

评测、构建和发布：

```text
golutra-supervisor $SUPERVISOR_ARGS evaluate evaluation-suite.json
golutra-supervisor $SUPERVISOR_ARGS check <candidate-id>
golutra-supervisor $SUPERVISOR_ARGS build <candidate-id>
golutra-supervisor $SUPERVISOR_ARGS preview <candidate-id>
golutra-supervisor $SUPERVISOR_ARGS canary <candidate-id>
golutra-supervisor $SUPERVISOR_ARGS canary-observation canary-observation.json
golutra-supervisor $SUPERVISOR_ARGS promote <candidate-id> --reason "healthy canary"
```

`evaluation-suite.json` 只包含 case 输入、fixture、partition 和 Supervisor 侧 assertion。assertion、partition、真实 case id 与 sealed 答案都不会传给 worker。每个 case 分别启动 stable release 和 candidate evaluation build 中的 `golutra-eval-worker`；二者使用独立 home/workspace、相同输入、随机目录、无网络 OS sandbox 和 deterministic mock provider。Supervisor 在进程外复核 workspace 文件、完整 TaskTrace、VerificationRecord 以及每个 artifact blob 的 checksum/size，并把含 blob 的配对 evidence 持久化为 `artifact://supervisor-evaluation/...`。缺少 development/security/migration/sealed/fresh 任一 partition 的 suite 会被拒绝。

`evaluate` 先运行一次 TrustedBuilder，并把该 BuildReport 与全部 binary artifact 作为 paired evaluation 的冻结输入；candidate worker 正是其中的 binary。BuildReport、update metadata、paired trace 和 blob 都落为 owner-only content-addressed Supervisor artifact，release manifest 引用这些真实对象并记录实际 `Cargo.lock`/`rust-toolchain` digest。`build` 只消费 Supervisor state 中这份同源 report/artifact，避免评测后重新编译出另一个未被运行过的 binary。`check` 在独立临时 staging 中重新构建，只输出诊断报告，不能覆盖 evaluation artifact，也不能作为 build 输入；CLI 不接受外部 report 文件。候选 source 以只读方式挂载，Cargo target 和临时文件写入独立 scratch，网络同时由 sandbox 与 Cargo offline 模式关闭；fmt、workspace check、workspace test 和 release binary build 的 stdout/stderr 使用总量 4 MiB 的并发有界 drain，超时或超限会终止整个进程组。构建前后会核对 frozen source digest，release 读取时还会复算 manifest identity、source tree、binary checksum 和大小。

canary 越界时执行：

```text
golutra-supervisor $SUPERVISOR_ARGS rollback <candidate-id> --reason "canary health regression"
```

下一次启动稳定版本：

```text
GOLUTRA_RELEASE_HOME="$GOLUTRA_HOME/releases" golutra-launcher golutra-cli chat
```

launcher 在 exec 前复验 stable pointer、manifest、binary size 和 checksum。已运行的旧进程继续完成当前任务；进程 drain 和新 attachment 路由由部署调用方在 pointer 切换前完成，Supervisor 不会强杀 active task。

## 晋升门禁

自动进入 release 的候选必须同时满足：

- `TaskTrace.complete=true`，且无 unresolved refs。
- baseline/candidate 是两个不同的真实 runtime execution refs。
- development、security、migration、sealed 和 fresh gate 全部 Pass。
- sealed 只返回阈值级 verdict，没有向 producer 暴露逐 case 反馈。
- quality delta 为正，cost/latency 不超 epoch 预算。
- target path 不触碰 sealed control plane。
- candidate producer 使用 OS-enforced sandbox，且没有网络、provider/release secret 或 Supervisor state 访问权。
- runtime 代码候选由 stable/candidate 两个不同的 eval-worker binary 执行；同一个已编译 RuntimeHost 的 workspace replay 不能充当版本级配对证据。
- candidate risk 为 low 或 medium；high/critical 停在人工 review 边界。
- TrustedBuilder 使用 OS-enforced sandbox、只读 candidate source 和独立 artifact staging，并生成通过的构建报告。
- 当前 candidate/release 至少存在一条 canary observation，且全部 observation 都不含 security violation、rollback signal 或健康预算越界。

`Fail` 和 `Inconclusive` 都不能默认成功。任一阶段产生的新 patch 都是新的 candidate，需要重新冻结、评测和构建。

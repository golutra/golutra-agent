# Golutra 与 Codex 子代理对比（2026-09-16）

本轮由同一个执行者独立完成源码检查、夹具、产品修改及复测；没有派发给协作成员。被测产品按夹具启动自己的真实子代理。

## 测试范围与结论边界

测试覆盖独立/继承上下文、并发、同一子代理续跑、长上下文、取消与恢复、真实文件修改和严格验证、10 子代理结果汇总。确定性回归另覆盖准入、重连、工作树、权限、失败、分页和执行身份。

共保留 **45 个真实样本，完成三轮产品迭代**。最终构建 Golutra 的 8 个样本全部通过；同期 Codex legacy 的 6 个成功场景对照也全部通过。Golutra 的全会话 token 和调用开销较低，但编码 E2E 存在反例，三次编码均值仍略慢。不能据此宣称在所有任务、上游、平台或功能维度全面超过 Codex。失败样本不参加成功性能排名，少量样本不计算可靠 P95。

最终构建的成功对照摘要（原始个别样本及失败见下文与 [45 个样本的去敏数据](subagents-20260916.json)）：

| 指标 | Golutra | Codex legacy |
| --- | --- | --- |
| 十子代理，2 次通过 | 2/2 | 2/2 |
| 十子代理 E2E 中位数 | 36.35 秒 | 72.37 秒 |
| 十子代理 total 中位数 | 68,200.5 | 390,609.5 |
| 十子代理调用范围 | 21 | 26–33 |
| 编码，3 次严格通过 | 3/3 | 3/3 |
| 编码 E2E 中位数 | 40.65 秒 | 59.82 秒 |
| 编码 E2E 均值 | **58.01 秒** | **55.80 秒** |
| 编码 E2E 范围 | 35.50–97.88 秒 | 47.24–60.33 秒 |
| 编码 total 中位数 | 54,453 | 304,107 |
| 编码调用范围 | 12–13 | 17–20 |
| 取消恢复，单次 E2E / 调用 | 24.79 秒 / 7 | 60.51 秒 / 13 |

表中的中位数只是这些固定样本的描述，不是显著性检验；同时报告编码均值和最慢样本，避免用中位数掩盖尾部问题。

## 环境与口径

- Golutra 基线：`3d6c1e7c56c687374387add69acad3412fed747b`，本轮修改在工作区；本地 debug 构建。
- Codex：已安装 `codex-cli 0.154.0`。默认启用的 legacy multi-agent 与显式开启的 `multi_agent_v2` 分开测。
- 本地参考源码：`project/codex` 的 `fc269b66adc37f3c855df222ad80b02733355c46`（2026-09-15）。没有修改 Codex；安装包版本不等于该源码提交。
- 同一上游 `https://api.golutra.cn/v1`，Responses，`gpt-5.5`，medium reasoning，同一凭据，子代理不降级模型或推理。隔离配置、工作区和用户规则。
- 保留两者原生系统提示、工具面和上下文/输出预算策略；这些属于产品差异，不声称输入请求逐字相同。未另行统一 native 输出上限，当前成功样本未观察到输出截断造成的验收缺失。
- 两者并发上限均显式配置 10。源码原生默认：Golutra 10；Codex legacy 6、v2 4。配置上限不等于实际并发，另核验执行时间交集。
- 统计父代理和所有子代理。Codex 按 `(thread_id, response_id)` 对原生 `token_usage_record` 去重，避免 fork 文件复制父记录造成重复计费。
- Token 是实际报告值，包含输入、缓存读取和输出；不等于收费金额。未缓存输入另列。Codex 请求覆盖无法完全证明，`usage_complete` 为 unknown；中断/失败执行的完整总量为 unknown，仅保留已知小计。cache-write、Codex provider TTFT 缺少证据时也是 unknown。
- 工具调用计数包含实际子代理读取/修改/测试与父代理控制动作。工具粒度不同，原始次数不是等价工作量。编码夹具要求每个子代理读文件、测试自己模块，父代理运行整体测试；外部严格验收读取不混入模型工具数。
- 固定功能目标，允许接口专用参数提示。批量和后续串行样本使用独立进程；早期两套同时运行的探针只用于功能诊断，不用于受控延迟排名。

可复现命令（输出目录必须是新的；含凭据的临时 home 自动清理，原始证据目录仅所有者可读）：

```sh
python3 scripts/compare_subagents.py --scenario fanout \
  --engines golutra codex --repeats 2 \
  --output /tmp/golutra-subagents-new-run
```

场景还包括 `lifecycle`、`coding`、`long-fork`、`cancel`。完整计划见 [comparison plan](subagents-comparison-plan-20260916.md)。

## 源码对照

Codex 参考路径：`codex-rs/core/src/tools/handlers/multi_agents*`、`session/multi_agents.rs`、`agent/role.rs`、`config/mod.rs`、`protocol/src/models.rs`；官方能力说明：[Subagents](https://developers.openai.com/codex/subagents/)。

| 方面 | Golutra | Codex 的可借鉴之处与本轮边界 |
| --- | --- | --- |
| 控制界面 | 一个 subagent 工具，spawn/status/wait/resume/cancel 等操作 | v2 分离 followup_task、send_message、interrupt，任务启动与纯消息语义更明确 |
| 上下文 | independent/fork；新 execution 明确最新任务边界 | v2 支持 none/all/最近 N turn，选择性继承更灵活；Golutra 尚无最近 N turn 模式 |
| 并发 | 默认 10 可配置，完成清理与结算后释放槽位 | 原生多代理控制与等待机制成熟；本轮两者实际 fanout 均达到 10 |
| 结果 | durable 全量证据；模型收到有界结果和分页句柄 | 学习精简结果交付，避免把治理信息反复传回模型 |
| 恢复 | 执行 ID 绑定观察结果、取消确认和通知；同 child 续跑 | 保留最新任务与历史分离，不靠关闭/重建子代理丢弃事实 |
| 协作范围 | 单层、父代理拥有子代理；共享工作区或显式 worktree | v2 的协作路径、消息和可配置嵌套能力更丰富；本轮未复制这套复杂度 |
| 隔离与验证 | 原有只读/工作树/真实失败门禁保持 | 工作树不是安全沙箱；未进行跨平台或攻击性安全对抗排名 |

## 第一轮：发现并修复真实故障

1. **输入用量错扣输出准入额度。** 长 fork 的历史输入让累计 total 达到 58,160，超过 40,960 的输出准入额度，合法续跑被拒绝。现将 `spent_output_tokens` 与全量 `spent_tokens` 分开，保留成本上限和未知用量的保守扣账。旧 checkpoint 没有输出字段时仍按旧总量保守恢复。
2. **取消后重跑旧任务。** 恢复后的子代理重新执行旧 90 秒脚本。给新 execution 明确最新分配任务边界，旧任务只作历史，不降低推理、不伪造执行。模型仍可按用户新任务要求重跑。
3. **取消控制与任务成功混淆。** 父代理明确要求的取消可作为控制操作成功，但子任务仍保留 cancelled/interrupted、`completed:false` 和完整诊断。确认必须对应同一 task，超时、真实错误及非主动中断不转成成功。重连使用持久化取消证据。
4. **批量等待观察竞态。** 原代码丢弃已观察终态再取最新状态，可能被并发 resume 替换。现在保留本次 wait 已观察的 execution，仅刷新仍待完成项。
5. **锁顺序。** 先释放 watch 读锁再解释 lifecycle，避免与结果发布路径的 lifecycle→watch 顺序相反。

第一轮串行对照（全会话）：

| 场景 | Golutra 成功/秒/调用/token | Codex legacy 成功/秒/调用/token |
| --- | --- | --- |
| 两子代理 + fork + 同 child 续跑 | PASS / 21.48 / 7 / 41,213 | FAIL / 72.62 / 18 / 333,540 |
| 长上下文 fork + 续跑 | PASS / 26.57 / 7 / 207,974 | FAIL / 97.02 / 19 / 816,669 |
| 真实进程取消 + 同 child 恢复 | PASS / 24.60 / 7 / 33,893 | PASS / 67.19 / 17 / unknown（已知 306,005） |
| 双子代理编码 + 严格测试 | PASS / 32.88 / 14 / 58,151 | PASS / 64.56 / 22 / 359,091 |
| 十子代理并发读取 | PASS* / 50.04 / 31 / 78,664 | PASS / 55.41 / 24 / 359,955 |

*十子代理原报告误判父回答不完整：通用显示摘要只保留 512 字符，实际最终回答 1,086 字符且含全部十个事实。原始报告没有覆盖，另存 `.audit.json` 记录纠正。验收器现直接读取执行绑定的完整最终回答，并有超 512 字符回归。

两种 fork 场景中 Codex 子代理混入父代理调度任务，未返回右文件事实。记录为该版本、模型和网关组合的观察结果，不推断 Codex 所有环境均有此问题。

第一轮编码成功对照中 Golutra token 少 83.8%，E2E 少 49.1%，调用少 36.4%。长 fork 修复后 token 高于失败基线，是因为此前被拒绝的续跑现在真的执行了，不能宣传为该场景 token 降低。

## 第二轮：批量结果交付

十个子代理短答案外围包含重复的任务、治理、验证和计费元数据，预算压缩后答案被挤掉，父代理补查十次 status。修复只改变模型可见投影：每项优先保留执行身份、状态、短结果、失败诊断、分页和工作树事实，完整 envelope 仍持久化。没有禁止 status/read、删除验证或自动创建结果。

极紧预算仍可能需要分页；这类必要读取不是浪费。回归验证十个短事实可在 2,048 token 预算内完整交付，失败和 CJK 游标仍保留。

第二轮两组串行复测，顺序 Golutra→Codex、Codex→Golutra：

| 样本 | 结果 | E2E 秒 | 工具调用 | 全会话 token | 未缓存输入 | cache read | 峰值子执行 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Golutra 00 | FAIL（创建时误填自选句柄） | 13.92 | 8 已启动 / 10 模型请求 | 4,003 | 3,005 | 0 | 0 |
| Codex 00 | PASS | 57.46 | 25 | 368,466 | 70,525 | 293,248 | 10 |
| Codex 01 | PASS | 57.71 | 25 | 364,735 | 52,652 | 307,328 | 8 |
| Golutra 01 | PASS | 32.04 | 21 | 67,560 | 11,334 | 52,992 | 10 |

成功的 Golutra 样本为 10 spawn + 10 read_file + 1 wait，没有十次补查 status；短答案全部保留。与第一轮 31 次相比，目标开销已消除。但这一构建两次只有一次通过，不能只报成功样本。

同组成功样本的用量分解解释了优势来源：Golutra 父会话 3 次 provider 完成、15,313 输入 token，各子会话都是 2 次完成、约 4,844–4,945 输入；Codex 父会话 6 次完成、109,087 输入，各子会话也是 2 次完成、约 25,082–25,122 输入。主要差异是每次请求的上下文/工具说明体积和父代理调度往返，不是让子代理少读文件。这里只能确认体积差异，不能仅凭 token 总量把全部差额归因于某一条提示词。

该样本 cache-read/输入比例约为 Golutra 82.4%、Codex 85.4%，但未缓存输入分别只有 11,334 与 52,652。缓存比例更高不一定更省：反复发送较大前缀也会提高比例。优化优先看成功率、实际未缓存用量及端到端耗时，不为了比例增加输入。这是 token 占比，不是“有命中的请求数/请求总数”。

## 第三轮：澄清创建句柄的契约

第二轮失败中，模型将任务标签 c0…c9 误填进 `child_session_id`；runtime 严格拒绝，没有创建假子代理。原说明只有“reuse child_session_id”，且字段没有说明，并不明确表示不支持自选名字。

现在工具描述和该字段 description 明确：spawn 自动分配句柄、创建时省略；其他操作使用真实返回值。错误信息给出相同改法。类型、required、权限和错误处理保持不变，不自动忽略参数或放松校验。补充非法名字仍被拒绝的回归。描述变化会改变 wire digest，接受一次 cache cold start。

第三轮十子代理复测（相同夹具，反转顺序，两组全部通过）：

| 样本 | E2E 秒 | 全会话工具调用 | provider total | 未缓存输入 | cache read |
| --- | --- | --- | --- | --- | --- |
| Golutra 00 | 34.75 | 21 | 68,211 | 18,510 | 46,336 |
| Codex 00 | 70.55 | 33 | 378,552 | 60,693 | 313,344 |
| Codex 01 | 74.20 | 26 | 402,667 | 56,283 | 339,840 |
| Golutra 01 | 37.95 | 21 | 68,190 | 24,560 | 40,192 |

这两组样本中 Golutra 的成功率、token、调用数和 E2E 达到本夹具的目标；不据此推断所有子代理任务全面领先。第二轮失败也保留在 `/tmp/golutra-subagent-compare-20260916-round2-fanout`，没有排除后再计算“从未失败”。

第三轮编码出现一个必须保留的反例：Golutra **97.88 秒、13 调用、59,941 token**，Codex **47.24 秒、17 调用、277,256 token**，两者严格验证均通过。Golutra 的工具和整体测试在约 36 秒完成，最后请求从 `17:51:18.536Z` 到 `17:52:19.320Z` 耗时约 60.78 秒。transport diagnostics：handle-ready 2 ms、first-business-event 57,421 ms、terminal 60,835 ms、attempt_count 1；实际 shell 验证约 52 ms。证据把长等待定位在 provider 传输/服务区间，不能精确区分网关排队与网络，也不能归因为后台等待或重跑测试。此样本 token/调用领先、E2E 落后，不删除、不通过更短超时重试制造更好数字。

该编码样本另有一次格式不合法的 apply_patch，随后真实 edit_file 和完整测试成功，原错误保留。没有把无验证的修补算作成功。

第三轮取消恢复：Golutra 24.79 秒、7 调用、34,424 token；Codex 60.51 秒、13 调用、完整 token unknown（已知小计 241,154）。两者都通过进程已停止、无 finished.txt、同一子代理新执行返回事实的检查。取消本身不是让原子任务“成功完成”。

第三轮 Golutra 长 fork 27.26 秒、7 调用、202,119 token；普通 fork/同 child 续跑 22.81 秒、7 调用、35,814 token，均通过。此处复测的是最终 Golutra 构建；Codex 对应场景的既有失败记录见第一轮，未把它们混写成第三轮同步对照。

为判断 97.88 秒是否持续，补跑两组同夹具编码对照，顺序 Codex→Golutra、Golutra→Codex，没有改产品代码或减少测试：

| 样本 | 严格验收 | E2E 秒 | 工具调用 | 全会话 total | 未缓存输入 | cache read |
| --- | --- | --- | --- | --- | --- | --- |
| Codex repeat 00 | PASS | 59.82 | 19 | 304,107 | 120,433 | 178,176 |
| Golutra repeat 00 | PASS | 40.65 | 12 | 50,683 | 28,217 | 19,200 |
| Golutra repeat 01 | PASS | 35.50 | 13 | 54,453 | 24,798 | 26,496 |
| Codex repeat 01 | PASS | 60.33 | 20 | 341,668 | 89,425 | 247,040 |

补测没有复现那次 57 秒的首业务事件等待，但并不证明尾延迟已经修复。当前没有足够证据支持再改调度代码或自动发起重复请求；下一步有价值的是细分请求传输阶段并做上游受控对照。

## 保留的失败与限制

- 基线长 fork：Golutra 29.60 秒、6 调用、174,176 token，因输出准入失败；Codex 132.19 秒、22 调用、942,399 token，fork 事实失败。
- 基线取消：Golutra 142.77 秒、12 调用、49,217 token，恢复重跑且生成 finished.txt；Codex 76.54 秒、17 调用，已知 310,626 token。原 Golutra 验收器也漏认 task_aborted，不能把“取消完全没执行”当作原因。
- 取消原夹具字段名 `recovery_token` 被合理脱敏，后改为非敏感标记 `recovery_marker`；两者都使用新夹具复测，原记录保留。没有降低产品脱敏规则；不把不同夹具的前后结果当成严格速度改进比例。
- Codex v2 三次生命周期探针在当前网关出现 HTTP 422。源码包含新 `agent_message` 类型，但尚未做单变量 wire 实验证明它就是原因。因此保留兼容性故障，不能把早退耗时当作比 Golutra 更快，也不能推断原生 OpenAI 服务相同行为。
- Codex legacy 部分样本出现 `timeout_ms:120000.0` 导致整数解析错误。参考源码 `multi_agents_spec.rs` 对该参数使用 `JsonSchema::number`，而执行端要求 i64，存在声明与解码类型不一致；不能只归咎于模型。第三轮首个 fanout 的 33 调用包括 2 次此类解析失败和随后 10 次单独 wait。真实额外调用与失败保留，未修改 Codex 或给其测试夹具添加特殊纠错提示。
- 原始 probe 的 Codex评价曾标 pending，早期失败 usage 未按完整执行覆盖置 unknown；汇总应以原生事件重新核验，不直接复制父会话 CLI total。
- 早期 coding 与 lifecycle-repeat 套件有重叠负载。编码 Golutra 两次 39.91/41.17 秒、12/14 调用、52,497/55,685 token；Codex 63.31/59.46 秒、21/19 调用、358,077/338,854 token，均通过，仅作功能辅助证据。
- 当前结果只涉及本机 macOS、此网关及 gpt-5.5 medium；未覆盖 Windows、Linux、跨主机协作、几百轮项目、网络故障注入或所有工作树冲突。TTFT/P95 不作结论。

## 剩余改进方向

| 方向 | 为什么仍值得做 | 不能采用的捷径 |
| --- | --- | --- |
| 扩大固定任务样本与故障注入 | 短夹具和少量重复不能证明复杂任务完成率、尾延迟或取消恢复始终可靠 | 不删除失败样本，不只公布最好的一轮 |
| 选择性上下文继承 | Codex v2 的最近 N turn 对部分跟进任务更灵活；需验证前置事实、工具配对与缓存前缀 | 不盲目丢历史来换 token 数字 |
| v2 上游兼容诊断 | 需要用相同 wire 请求的单变量实验区分网关、SDK、模型支持问题 | 不把当前网关的 422 推广为 Codex 产品全面失败 |
| 跨平台、长驻进程与工作树冲突 | 本轮真实取消只在本机运行；资源清理和隔离需要目标平台证据 | 不以单元测试替代 Windows/Linux 实机验收 |

本轮优先解决已复现的正确性和重复往返，未为了增加功能数量复制嵌套协作体系。严格失败、真实验证和合法续读保持不变；不能将“全部指标已全面超越”作为本报告结论。

## 证据与构建

原始样本保留在 `/tmp/golutra-subagent-compare-20260916-{probe,coding,lifecycle-repeat,long-baseline,cancel-baseline,after-long,after-cancel,after-lifecycle,after-coding,after-fanout}`。每份含私有 JSON、原始事件、stdout/stderr；编码场景还保留最终源码和严格 verifier 输出。临时目录不是长期归档，仓库内报告保留去敏指标及解释。

后续目录为 `round2-fanout`、`round3-fanout`、`round3-coding`、`round3-coding-repeat`、`round3-cancel`、`round3-long-fork`、`round3-lifecycle`，使用相同前缀。仓库 JSON 保存每份原报告与 events 的 SHA-256、原判定/审计判定、失败项、完整/部分用量和耗时。审计判定补齐早期 pending 并纠正 512 字符误报；原报告内容不变。provider timing 只保留数值，Codex 未观测项保持 null。

Golutra CLI SHA-256：

- 基线：`77f6b89cc4f3fb121e2fb377bbf06945f4c96f32595fee1c58e3dc673e67c42b`
- 第一轮：`4bdd966a3e54e8862862358fc4b7b2cb69fed0f15d1433e4698d1930a4eba617`
- 第二轮：`b83c1e7093217b120b8c835780a865c5e07bfc1acea9dfa3e1ca4bccd09baa5b`
- 第三轮：`98e3a88aba295167e6ae122462ca3d099d70ebb085c2ef27fd59bdf1879c49b0`

早期 JSON 的 Codex `binary_sha256` 实际散列 npm launcher，不能当作 native binary 指纹；新 harness 明确命名为 `executable_or_launcher_sha256`，同时记录 CLI 版本和夹具散列。

本轮门禁：client 419、runtime 193、tools 174、LLM 84（合计 870 个 Rust 单元测试）、Python scripts 101 通过；相关 crate all-targets Clippy `-D warnings`、fmt、diff check 通过。完整前一轮 workspace 门禁不能冒充本轮全仓重跑。

门禁原始日志为 `/tmp/golutra-subagent-20260916-{tests,tests-final,round2-tests,round3-tests,python-final,clippy,round3-clippy,round3-build}.log`。第一次 client 运行的旧断言要求主动 cancel 的控制结果本身为 Cancelled，调整为新的“控制确认 Ok、子任务仍 cancelled/completed:false”语义后全量通过；没有删掉断言或隐藏子任务失败。早期一次 rustc 在宿主加载阶段无进展，终止本轮拥有的进程后重跑成功，未清理用户进程或广泛删除构建目录。

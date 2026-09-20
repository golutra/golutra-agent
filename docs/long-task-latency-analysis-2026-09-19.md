# 长任务对照耗时复核

本次只做原因分析，没有修改生产代码，也没有新增真实模型跑分。依据为最终四阶段实跑的逐条事件、工具原始参数、provider 时序、运行包，以及本地 Golutra/Codex 源码。原始报告：`/tmp/golutra-continuous-comparison-final-20260919.json`；持久化汇总见 [三轮数据](benchmarks/2026-09-19-continuous-tasks.json)。

## 结论

366.6125 秒对 318.4625 秒的差距不能简单归为网络或 Rust 调度。已确认的可改善因素是：工具 schema 压缩隐藏硬边界、重复命令表示导致参数冲突、进程操作字段混用、补丁恢复提示误分类，以及测试专用完整导出的收尾开销。正常完成后的默认纠偏没有在这轮触发。

只有一组修复后的样本；Codex 没有同口径逐请求耗时，因此不能精确把全部 48.15 秒差距分配到这些因素，也不能把失败请求耗时全部视为可消除时间。

## 时间拆分

| Golutra 区间 | 总计 | 口径 |
| --- | ---: | --- |
| Provider 请求开始至结束 | 338.473 秒 | 32 个顺序请求的事件时间差之和，92.32% |
| 其中首 token 前 | 227.528 秒 | 已包含在 provider 区间；可能含上游排队、预填充、推理和传输 |
| 首 token 后至结束 | 110.945 秒 | 已包含在 provider 区间 |
| 执行循环内、provider 区间外 | 11.259 秒 | 首个 step_started 至 stop_success 减去 provider 时长；含工具、持久化、上下文与其他调度 |
| 上述循环之外 | 16.881 秒 | 启动、终态之后导出/退出及采集收尾等，未单独插桩，不全部归为导出 |
| 进程总时间 | 366.613 秒 | 包含测试所请求的运行包导出 |

四阶段 candidate_ready 至 stop_success 合计约 129 毫秒。每阶段仅一次候选、一次验收、一次 stop_success，无验收纠偏续轮。32 个请求对应 32 次传输尝试，无传输重试。缓存关系为 1 次 cold_start、31 次 append_only；不存在这轮不断改写前缀造成的大量冷缓存证据。

28 个有测量值的工具就绪至响应终态区间合计 843 毫秒。因此工具提前执行不太可能解决本次几十秒差距。

## 工具错误仍然制造模型往返

最终成功的任务包含 **11 次内部工具失败**：9 次准入参数拒绝、1 次不存在文件读取、1 次补丁上下文歧义。

| 错误 | 次数 | 实际参数/原因 |
| --- | ---: | --- |
| 空 command 与有效 argv 同时填写 | 1 | `command: ""`，argv 为合法 find 命令；schema 提前拒绝空字符串 |
| yield_time_ms 超界 | 3 | 模型填写 120000，运行时最大 30000；三项属于同一回复 |
| command 与 argv 冲突 | 4 | 两份脚本的换行、分号或转义不同；运行时要求解析后参数相符 |
| list 带入无效进程字段 | 1 | `action: list, process_id: "", authoritative_pid: 0`；不需要这些字段却被整体 schema 拒绝 |
| 猜测不存在的项目文件 | 1 | 读取不存在的 pyproject.toml |
| 补丁位置歧义 | 1 | checkpoint.py 的匹配上下文不唯一，安全拒绝后下一轮用准确上下文成功 |

完全没有成功工具的 provider 轮次（step 为零起点）：

| 阶段 / step | 请求耗时 | 失败原因 |
| --- | ---: | --- |
| 1 / 4 | 12.685 秒 | 三个 shell 的等待值超界 |
| 1 / 6 | 8.763 秒 | 重复命令表示仍冲突 |
| 3 / 1 | 14.377 秒 | command 中分号脚本与 argv 中换行脚本不相符 |
| 4 / 2 | 9.383 秒 | 补丁上下文歧义 |
| 合计 | 45.208 秒 | 是失败轮次的请求耗时，不是保证可节省的时间 |

成功调用本身同样需要模型生成与等待；不能从总耗时直接减去 45.208 秒并宣布超过 Codex。混合成功/失败的轮次也会带来额外内容，但无法仅凭本次轨迹给出净损失。

### 1. 精简 schema 与执行合同不一致

`crates/golutra-agent-llm/src/lib.rs` 的 `PROVIDER_SCHEMA_BOUNDARY_KEYS` 会删除 minimum/maximum/minLength/minItems 等字段。`tools/src/builtin.rs` 的 shell 合同保留 30000 上限，而 yield_time_ms 描述只有默认值、没有上限。模型看不到该数值边界，执行时却遭拒绝。这是此前减少 schema token 的明确代价。

存储的 request snapshot 含内部完整合同，不能把该快照直接当成线上 wire schema；适配器通过 `provider_tool_schema_for_contract` 投影后才发送。Responses 可选字段 schema 会显式关闭 strict，本地代码没有强制模型填满所有字段的证据；填入空值/零值是本次实际生成行为。

Codex `core/src/unified_exec/mod.rs::clamp_yield_time` 与 `process_manager.rs` 会把初次等待约束到允许范围。等待时长与进程硬超时分离，越界等待值不必使整个命令失败。

建议按参数语义处理：软等待和展示预算可透明限幅；权限、路径、执行目标和硬超时仍严格校验。不能对所有数字统一静默纠正。provider 支持时保留必要数值边界，否则把关键边界转成简洁描述，且不依赖上游替代本地检查。

### 2. shell 有两份权威命令

Golutra 同时向模型暴露 command 与 argv，描述为“prefer only one”；`shell_command_for_request` 检查两者相符，否则拒绝。本次有模型把同一段 Python 写成分号与换行两个版本，重复生成大段脚本，还需花回合修复。

Codex `core/src/tools/handlers/unified_exec.rs::ExecCommandArgs` 的模型入口是单个 cmd，由宿主解析成实际 argv，不要求模型重复描述。

建议模型侧保留一个权威命令入口；若内部 API 需要 argv 可继续保留，但不要让模型重复填写同一意图。不能通过“冲突时随便选一个”解决，否则可能改变实际执行内容。优先减少出错空间，不继续堆叠提醒性提示词。

### 3. 进程动作的参数边界不明确

shell_session 把 list/wait/read/write/terminate 全部字段放在一个对象中。list 不需要 process_id/PID，模型却填空值和零。建议提供按 action 的参数投影/校验，明确无关字段的省略语义；也可用少量独立的模型操作面。等待、输入和终止仍必须验证所属会话及进程身份，不能统一忽略无效 ID。

### 4. 补丁错误还有残留误分类

`crates/golutra-agent-tools/src/lib.rs::execution_error_facts` 使用 `reason.contains("checkpoint")` 分类。真实报错包含文件名 `jobledger/checkpoint.py`，因而残留 `action_required: inspect_checkpoint_error`；随后虽然 error_kind 被补丁恢复事实改回 patch_context_ambiguous，错误动作提示仍在。本次模型正确重试，没有再次跑偏，但这个代码缺口仍然存在。

建议按错误类型生成恢复动作，避免从包含业务路径的错误字符串猜测故障域。补丁歧义的安全拒绝应保留；向模型提供明确锚点/上下文比直接选第一个匹配更稳妥。

Codex `apply-patch/src/seek_sequence.rs` 顺序查找，允许尾部空白、首尾空白及部分字符归一化，并取首个匹配；Golutra `model_patch.rs::locate_hunk` 要求唯一匹配（或明确位置）。这是成功率与误改风险的取舍，不应不加区别地复制。

## 测试口径也有差异

1. Golutra 使用 `target/debug/golutra-agent`，Codex 使用已安装 0.154.0 二进制，构建优化级别不统一。Codex 源码参照仍是 fc269b66，并非已证明与安装包一一对应。
2. 比较脚本给 Golutra 传入 --run-dir/--run-bundle，任务结束后 `main.rs::export_exec_run_bundle` 同步等待完整导出；Codex 没有对应的全量调试导出要求。
3. 四阶段执行循环外时间依次约 1.707、3.078、4.624、7.472 秒，随历史增长。源码在导出时收集会话、写 observations 并遍历 artifact；最终包包含 308 个 artifact。第4阶段 stop_success 为 12:45:53.615 UTC，debug manifest 生成为 12:45:59.389 UTC，相距约 5.774 秒。证据支持导出是收尾因素，但它不是对纯导出耗时的独立计时。
4. 报告的 65 次 Golutra 工具调用按内部单工具统计。按原始 provider_tool_call_id 去重后为 **44** 次，批量读文件会展开。这再次说明不能直接拿 65 对 Codex 的 27 项输出事件评价工具效率。
5. Golutra 输出 token 比 Codex 多 12.30%，未缓存输入多 14.65%。与失败重试、重复脚本等观察相符，但单样本无法给出各原因的 token 因果占比。

## 优先级与验证方式

1. **先修工具合同可用性**：单一命令表示、软等待有界归一化、保留必要参数边界、按进程动作明确字段。用本次9个真实拒绝案例做不执行副作用的合同回归，同时保留冲突命令、越权和错误句柄拒绝测试。
2. **清理 typed 错误归因**：业务文件名不能触发 checkpoint 故障类别；用 checkpoint.py 的实际歧义补丁验证恢复提示一致。
3. **修正性能测量**：使用 Golutra release 构建；分别记录任务终态与进程退出，完整导出单列；Codex 缺失的请求数与时序继续标为未知，不能用事件数冒充请求数。
4. **然后重复同题交替对照**：独立工作区、相同模型/effort/网关/验收，至少多组结果同时报告正确率、中位数和范围；改动前后保留所有失败样本。小样本不宣称稳定 P95。

当前证据不支持优先重写长任务循环、放松正式测试要求、减少审计持久化，或为数十秒差距立即增加 WebSocket/工具提前执行。本轮只诊断，以上建议尚未实施。

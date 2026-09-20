# 当前 Golutra 与 Codex：同模型持续任务对照

2026-09-20，使用当前未发布的 Golutra 工作区构建与 Codex CLI 0.155.1，重新执行三轮配对四阶段任务。Golutra 完整通过 3/3，Codex 完整通过 2/3。双方成功的前两轮中，Golutra 累计耗时少 22.6%、未缓存输入少 38.0%、输出少 24.4%；Codex 原生工具调用更少。该结论仅适用于本组任务，不能推导数小时自治或全面领先。

## 对照条件

- 双方请求 `gpt-5.6-sol`、`medium`，同一 `https://api.golutra.cn/v1` Responses 上游、同一凭据来源。控制的是请求的模型标识与推理强度，网关内部实际路由不可独立确认。
- 同一任务提示（SHA-256 `286d222a450fd59ccbb5058adcba688c2cca7988f0cbac3b4d2b66b48d1cfa68`，4095 字节）、同一初始工程（SHA-256 `99be54e3f414a59ba885ffbfeda525e351cd4f7405cbdc2fedec9e992f1657c3`）。系统提示和工具合同保留各产品实现。
- 每次使用隔离 home/workspace，不修改用户配置，不向模型追加修复轮，禁用子代理。先后顺序为 Golutra/Codex、Codex/Golutra、Golutra/Codex；整个对照中不修改生产代码或判题标准。
- Codex 临时模型目录保留模型元数据，仅关闭 Responses Lite，以使用当前网关的常规 Responses。配置项依据[官方配置参考](https://developers.openai.com/codex/config-reference/)核对；这不是其他上游或其他协议的实测结果。
- 每项任务由宿主设置 600 秒截止，Golutra 任务不注入隐式期限。没有清除上游 prompt cache；独立本地目录不等于冷上游缓存。
- 一次任务自主完成全部四阶段：多文件 API 实现、严格/容错加载与事务合并、原子写入、校验和 checkpoint、后台探针及恢复计数。分别执行全部四个独立判题器，检查原始测试/探针摘要和后台生命周期。
- 实机为 macOS 27.0 arm64，Python 3.14.5。没有在本轮注入断网、强制压缩或验证 Windows/Linux。

Golutra 显示版本仍为 `0.3.1`，但包含未发布修改；不能用 npm 上同版本号的产物替代本次二进制。SHA-256：

- Golutra：`c961c0651e7d6e7404382d8d8b0512e397a8a1ba383b966af820caf3cfcf7967`。
- Codex 原生程序：`8eaf1ad12fe6bf89b1710330f58900014322c7c5af677e43be116d8ac5fc0a9e`，测试前后相同。

## 逐轮结果

| 轮次 | Golutra | Codex | 原生工具调用（Golutra / Codex） |
| --- | --- | --- | ---: |
| 1，Golutra 先行 | 134.4 s，4/4 通过 | 231.7 s，4/4 通过 | 24 / 14 |
| 2，Codex 先行 | 157.5 s，4/4 通过 | 145.3 s，4/4 通过 | 24 / 9 |
| 3，Golutra 先行 | 160.1 s，4/4 通过 | 188.8 s，3/4 通过 | 25 / 12 |

六次原始测试和探针摘要均保持不变。运行时都正常结束，但独立交付成功为 Golutra 3/3、Codex 2/3。Golutra 三轮均未观察到运行时纠偏、重试调度或压缩事件；这说明本轮没有触发这些恢复路径，不能用来证明断网或压缩能力优于 Codex。

第三轮 Codex 的探针启动、释放和退出本身成功；随后 `item_14` 再次检查测试及探针进程已退出，`item_15` 又修改 `jobledger/checkpoint.py`，`item_16` 再运行测试。文件最后修改时间晚于探针结束 **35.677 秒**，因此按双方一致的阶段时序规则判失败。

这与先前 Golutra 曾出现的“关闭依赖资源后才再次修改受约束文件”属于相同验收类别。保留失败，不追加一次人工修复或重跑来覆盖它。Codex CLI 摘要只保存这次修改的路径，无法据此还原最后补丁的具体语义；这里证明的是文件时序违约，不声称定位了某个算法错误。第四阶段判题先检查生命周期，未通过该门槛后没有继续判定该阶段所有功能。

## 性能：仅比较双方都成功的前两轮

| 指标（两轮合计） | Golutra | Codex |
| --- | ---: | ---: |
| 耗时 | 291.9 s | 377.0 s |
| 输入 token，含缓存 | 229755 | 647624 |
| 未缓存输入 token | 34859 | 56184 |
| 输出 token | 20397 | 26983 |
| 输入缓存命中比例 | 84.83% | 91.32% |
| 原生工具调用 | 48 | 23 |

第二轮 Codex 更快，不能将累计优势写成每次都快。Codex 缓存比例更高，但总输入也更大，所以不能只靠缓存比例判断消耗。没有提供费用估算；真实账单还依赖上游计价。

原生工具计数粒度不同：一条 shell 可以读取多个文件、执行多个命令，独立文件工具则分别计数。该表不能证明 48 次是 23 次的两倍工作量，也不能把工具数当模型请求数。Codex 当前 CLI 输出没有 provider 请求数或真实 TTFT；缺失值在记录中为 null。首个输出字段使用 CLI 可观测事件代理口径，不能作为跨产品真实首 token 延迟，更不以三次样本给出可靠 P95。

三轮全部尝试的耗时、用量也完整保存在 JSON 中，但不将第三轮 Codex 的失败耗时作为成功交付提速依据。

## 当前长程能力判断

本组持续多阶段任务的交付表现已不落后于 Codex，Golutra 的输入量及双方成功配对的累计耗时更低。这与“所有长任务或数小时连续执行已全面超过 Codex”是不同强度的结论。

此前已经实施并有回归的能力包括：默认任务累计预算和纠偏次数不限、类型化连接故障等待恢复、不重放已完成写入、压缩/resume 保留来源校验过的用户目标、依据快照处理派生缓存的新鲜度。权限、取消、显式预算、真实验证失败和协议边界继续生效。它们的实现与测试见[持续执行](long-task-unlimited-2026-09-20.md)、[连接恢复](long-task-recovery.md)和[本轮根因修复](stage-review-optimization-2026-09-20.md)。

仍有明确限制：复合 shell 的等价验证身份会引发额外复验；数小时真实编码、跨多次上下文压缩，以及其他平台/真实协议上游的长期验收尚未在本轮完成。当前证据不足以新增工具白名单或额外模型监督，也不足以宣称所有指标领先。

## 复核材料

脱敏逐轮结果、全部阶段判题、摘要、失败时序及汇总：[JSON 数据](benchmarks/2026-09-20-current-codex-comparison.json)。原始记录保存在 `/tmp/golutra-codex-current.Gi7MEs/round-{1,2,3}.json` 及对应 log 中；原始工作区位置保留在这些本机报告内。

复现命令如下；第二轮交换 `--engines` 顺序，三轮使用不同输出路径：

```sh
python3 scripts/compare_continuous_tasks.py \
  --single-task --engines golutra codex \
  --golutra /tmp/golutra-stage-review.3Ay29a/final \
  --codex-model-catalog /Users/skyseek/.codex/models_cache.json \
  --model gpt-5.6-sol --reasoning-effort medium --timeout 600 \
  --output /tmp/golutra-codex-paired.json
```

本轮只测试、分析并新增报告；未修改生产实现，未提交、推送或发布。

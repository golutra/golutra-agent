# Golutra Docs

## 推荐阅读顺序

1. `ARCHITECTURE.md`：主架构规格，作为实现时的架构真相。
2. `runtime-os-target-architecture.md`：RuntimeApplication、五类 repository、治理闭环和本轮重构迁移设计。
3. `implementation-blueprint.md`：第一阶段实现蓝图、最小 schema、同步/后台/离线边界。
4. `initial-implementation-plan.md`：从工程骨架到 P0 runtime 闭环的详细实施计划。
5. `runtime-governance-completion-design.md`：P2.5 完整任务事实、durable evaluation、语义验证、真实回归和 memory quarantine 实施记录。
6. `runtime-contracts.md`：runtime 硬契约，包括 tool/provider/terminal/cancel/retry/fallback。
7. `llm-provider-integration.md`：LLM provider 配置、认证、凭据存储和接入 UX。
8. `onboarding-session-workspace-design.md`：首次 provider onboarding、resume、多 session 和多工作区设计。
9. `artifact-evidence-ledger.md`：artifact 和 evidence 的事实层规格。
10. `benchmark-hardening.md`：benchmark 防污染、防跑分和元数据要求。
11. `agent-runtime-technology-selection.md`：语言、crate、workspace 和库选型。
12. `context-memory.md`：context、token、compaction、memory governance。
13. `evaluation-observability.md`：观测、验证、复盘、benchmark。
14. `agent-improvement-loop.md`：失败轨迹如何变成可验证、可回滚的 agent 改进。
15. `agent-open-endedness-design.md`：开放式探索、技能晋升和 Promotion Gate。
16. `self-evolving-runtime-design.md`：内部/外部代码自进化、密封评测、连续发布和回滚的 P3 架构与实施状态。
17. `supervisor-operations.md`：Supervisor、TrustedBuilder、release、canary、launcher 和 rollback 实际操作。
18. `research-self-evolving-agent-systems.md`：自修改 agent、防过拟合和发布完整性的一手资料研究。
19. `extensions-sdk-delivery.md`：Plugin/MCP、IPC、TypeScript/Python SDK 和安装交付。
20. `tui-driver.md`：agent 可控的原生离屏 TUI、NDJSON/Unix socket 协议、快照和安全边界。
21. `framework-comparison.md`：六个外部 agent 项目对 Golutra 的影响。
22. `runtime-entrypoints.md`：exec、App Server、Python/TypeScript SDK、MCP 和 Remote TUI 的进程模型、协议边界和验收方式。
23. `runtime-stability.md`：长任务执行、provider/process 监督、崩溃恢复、任务对账和 restart soak 验收。

## 文档分工

| 文档 | 作用 |
| --- | --- |
| `ARCHITECTURE.md` | 主架构真相 |
| `runtime-os-target-architecture.md` | RuntimeApplication、repository 和治理闭环迁移设计 |
| `implementation-blueprint.md` | 第一阶段实现蓝图 |
| `initial-implementation-plan.md` | 详细实施计划和 P0 里程碑 |
| `runtime-governance-completion-design.md` | P2.5 治理事实完整性、持久作业、语义验证、真实回归和 memory quarantine |
| `runtime-contracts.md` | runtime 硬契约 |
| `llm-provider-integration.md` | LLM provider 配置、认证和凭据接入 |
| `onboarding-session-workspace-design.md` | 首次 onboarding、resume、多 session 和多工作区 |
| `artifact-evidence-ledger.md` | artifact / evidence 事实层规格 |
| `benchmark-hardening.md` | benchmark 防污染与元数据规范 |
| `agent-runtime-technology-selection.md` | 技术栈和模块选型 |
| `context-memory.md` | 上下文、压缩、记忆 |
| `evaluation-observability.md` | 观测、验证、复盘、评估 |
| `agent-improvement-loop.md` | agent 改进闭环 |
| `agent-open-endedness-design.md` | 开放式能力和演进门禁 |
| `self-evolving-runtime-design.md` | 自进化代码候选、密封评测与连续发布架构和实施状态 |
| `supervisor-operations.md` | P3 本地控制面的持久化、命令、可信构建和版本切换 |
| `research-self-evolving-agent-systems.md` | 自修改系统与反过拟合一手资料研究 |
| `extensions-sdk-delivery.md` | Plugin/MCP、transport、SDK 和交付 |
| `tui-driver.md` | 原生 TUI Driver、离屏快照、NDJSON/Unix socket 和进程级验收 |
| `framework-comparison.md` | 外部项目调研结论 |
| `runtime-entrypoints.md` | 五类运行入口、共享 Runtime 分层、进程模型和跨进程验收 |
| `runtime-stability.md` | 长任务稳定性不变量、故障分类、恢复对账和 soak 验收 |

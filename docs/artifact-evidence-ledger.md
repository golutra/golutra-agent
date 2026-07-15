# Artifact 与 Evidence Ledger 规格

## 文档定位

本文档定义 Golutra 的事实层：artifact 如何保存，evidence 如何建立，二者如何支撑 replay、verification、memory 和 benchmark。

## 当前实现状态

截至 2026-07-15：

- artifact metadata 位于 SQLite，blob 位于 owner-only `$GOLUTRA_HOME/state/artifacts`；写入前统一 redaction，记录 SHA-256、size、created_at、retention policy、expiry 和 blob deletion state。
- tool raw output、provider raw metadata、checkpoint before-image 与外部 MCP 输出都通过同一 ArtifactRecord/EvidenceRecord 链路，不直接进入 prompt；structured facts 递归脱敏。
- rollout/fork 只复制 immutable artifact lineage，不复制 blob；DebugProjection 和 verification 可沿引用读取。
- storage maintenance 会统计 live/expired blob，按 retention 清理过期与 temporary artifact，并保护仍被 rollback/lineage/verification 引用的内容；checkpoint 每 workspace 保留最近 20 个。
- 普通 TUI 不展示 artifact 原文；developer/debug、`golutra-vis` 和 SDK query 才按需展开审计事实。

## 核心原则

```text
没有 artifact provenance，
就没有可信 replay、可信 verification、可信 benchmark。
```

## 第一阶段必做

第一阶段至少要固定：

- `ArtifactRecord`
- `EvidenceRecord`
- artifact scope
- checksum / provenance
- redaction / retention
- artifact 与 verification 的关联

## ArtifactRecord

```text
ArtifactRecord
  artifact_id
  run_id
  session_id
  turn_id
  tool_call_id
  artifact_type
  uri
  checksum
  size_bytes
  created_at
  producer
  redaction_status
  retention_policy
  provenance_refs
```

建议范围：

- `run_id`：一次 task 执行级别
- `session_id`：长期会话级别
- `turn_id`：单轮推进级别
- `tool_call_id`：单工具调用级别

## Artifact Scope

artifact 至少区分这些 scope：

- `session`
- `turn`
- `tool_call`
- `run`
- `benchmark`

作用：

- `session`：长期上下文或导出产物
- `turn`：单轮中间产物
- `tool_call`：工具原始输出、日志、下载文件
- `run`：任务级总产物，如 patch、test report
- `benchmark`：评测输入输出和判分证据

## EvidenceRecord

```text
EvidenceRecord
  evidence_id
  claim
  artifact_refs
  source_event_refs
  evidence_strength
  verifier
  confidence
  limitations
```

要求：

- 每条 evidence 要能追溯到 artifact 或 runtime event。
- `claim` 必须是可判断的陈述，不是自由散文。
- `limitations` 不能省略，否则容易把弱证据误当强证据。

## Provenance / Redaction / Retention

每个 artifact 至少要回答：

- 谁产生的
- 何时产生的
- 是否脱敏
- 保留多久
- 是否允许进入 benchmark / memory / export

最低字段：

```text
provenance_refs
redaction_status
retention_policy
```

## Linkage

第一阶段建议至少固化三类关联：

- artifact-to-verification
- artifact-to-memory
- artifact-to-benchmark

目的：

- verification 能解释“为什么判定成功/失败”
- memory 能解释“为什么这条经验可以晋升”
- benchmark 能解释“为什么这次分数可信”

## 第一阶段落地建议

- artifact blob 默认存文件系统或对象目录。
- SQLite 只保留索引、checksum、路径、大小、类型和引用关系。
- 模型默认只看 artifact 摘要与必要 excerpt，不看原始全文。
- debug / replay 能从 artifact ref 追到原始事实。

## P0 验收口径

- 任意大工具输出不会直接污染 prompt。
- 任意关键 claim 都能追溯到 evidence。
- replay 能找到对应 artifact。
- verification 能引用 artifact/evidence，而不是只引用模型自然语言。
- benchmark 记录能标记 artifact delivery 是否成功。

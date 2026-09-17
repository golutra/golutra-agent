# 桌面与 npm 共享数据兼容合同 v1

桌面和 npm 使用相同原生内核及 `GOLUTRA_AGENT_HOME`。程序安装位置与数据归属独立；不新增服务，不按软件版本默认拆分数据。完整 home 包含配置、凭据、SQLite、artifact、工作区辅助数据及锁，不能只分离 SQLite 后宣称完整隔离。

## 目录与观测

已有 `golutra-agent --cwd /absolute/project storage status` 和 RuntimeQuery `storage_status` 增加两个字段，原有统计字段保留：

- `identity`：`contract_version`、`binary_version`、`persistence_mode`（`durable` / `isolated` / `memory`）、`config_home`、`runtime_home`、`runtime_db`、`workspace`、`workspace_hash`。
- `compatibility`：`contract_version`、`binary_version`、`schema_version`、`supported_schema_version`、`status`、`reason`。

诊断来自正在服务请求的 runtime 已解析路径；连接 app-server 时反映服务端目录，不使用客户端环境猜测。只在显式查询时返回路径，不增加生产日志、不读取凭据正文。直接把 home 指向 symlink 会拒绝；父路径别名、cwd 的 `.` 等经 canonical 解析。Windows HOME 优先、USERPROFILE 回退保持不变。`--ephemeral` / `--run-dir` 保留全局配置而隔离 runtime，诊断分别显示两个 home。

成功打开的 store 返回 `supported`；全空库的 `uninitialized`、旧/未知格式的 `unsupported` 可通过库的只读 `inspect_data_compatibility` 查看。无法打开的 CLI 返回错误及非零退出码，不能依赖成功的 StorageStatus 来探测一个已经拒绝打开的库。

## 桌面设置写入

已有 app-server 的 stdio/HTTP/WebSocket JSON-RPC 共用两个方法；远程方式沿用现有认证边界，stdio 由启动进程的本地用户拥有。

```json
{"jsonrpc":"2.0","id":1,"method":"config/runtime/read","params":{}}
```

结果为 `{ "revision": "sha256:…", "settings": { … } }`，只含全局非敏感层，不包含项目/session 覆盖。保存时提交改动字段：

```json
{"jsonrpc":"2.0","id":2,"method":"config/runtime/patch","params":{"expected_revision":"sha256:…","patch":{"subagent_max_concurrent":10}}}
```

`null` 清除该全局字段，让后续层级/默认值接管。所有未知字段、错误类型、无效值均拒绝。revision 比较、合并、校验、原子保存处于同一文件锁内；另一个进程已更改时返回 JSON-RPC `-32009` / `config_version_conflict`，原文件不变。桌面应重新读取并让用户确认冲突，不自动用新 revision 重放整个旧快照。校验错误为 `-32602`。

Provider 编辑使用已有 verified API；读取 `ProviderSettings::revision()`，保存走 `update_provider_settings_at_revision_verified()`。它在已有跨进程事务锁中先比较 revision，再运行字段修改、真实 provider 探测及失败回滚。原先的程序性 read/modify/write API 保留；新接口用于携带旧快照的 GUI。凭据继续走登录/OAuth API，不接受通用 JSON 补丁。运行中任务使用已解析快照，不承诺更改立即影响现有任务。

## 当前格式与运行期边界

`runtime.sqlite.usage.lock` 是稳定的旁路锁文件，路径由数据库 canonical 身份决定，不随数据库 rename 自动另建。正常 store 持有共享锁，clone 共享其生命周期；最后一个 store 释放或进程退出，OS 才释放锁。

| 数据状态 | 处理 |
| --- | --- |
| 当前 schema 7，唯一基线记录与 checksum 正确 | 当前桌面/npm 同次构建共享打开 |
| 全空数据库 | 取得独占锁后一次事务创建当前完整 schema |
| 初始化时还有活跃使用者 | 有界约 5 秒内拒绝 `data_in_use`，不终止其他进程 |
| 旧 schema 1–6、无账本但已有表、空账本、未知版本或 checksum 错误 | 拒绝 `data_unsupported`，不迁移、不重置、不降级、不修复 |
| 独占初始化完成 | 取得共享锁后再校验，关闭锁降级间隙中的版本竞争 |

项目当前尚无需要承接的用户数据，按产品决定只支持当前格式。删除旧 migration runner 的字段补齐、线程去重、artifact 回填、历史 checksum 接受/刷新，改为单一 schema 7 基线。不得复用旧版本号宣称旧数据可读；正式发布必须使用新版本并锁定二进制 SHA-256。

Provider 仅支持当前 v2；v1 env-map 配置直接拒绝，不迁移密钥、不改写配置。读取配置不再依赖 SecretStore。当前格式凭据和配置可以共用，不能通过删除版本标记或补字段来导入旧格式。

旧测试 home（包括此前开发 schema 6）无法继续打开。需要关闭旧实例后显式指定新的空 `GOLUTRA_AGENT_HOME`，重新配置登录；程序不会自动删除、备份、隔离或同步旧目录。本轮未清理开发者已有数据。以后格式发生不兼容变化时仍应明确拒绝，不提供静默转换。

会话锁与 schema 使用锁职责不同：共享历史查询不取得会话执行权；继续任务必须取得现有 session lease。争用不强行接管；进程崩溃释放 OS 锁，原有 orphan/不确定副作用恢复规则继续生效。不同 thread 共用数据库不等于可以无冲突地修改同一个项目文件。

## 验收与发布

```sh
cargo test --locked -p golutra-agent-config --test shared_settings_process
cargo test --locked -p golutra-agent-store --test shared_data_process
cargo test --locked -p golutra-agent-client shared_home_tests
python3 scripts/smoke_shared_versions.py --baseline-cli /absolute/old/golutra-agent --candidate-cli /absolute/new/golutra-agent --output dist/shared-versions.json
```

同次构建的桌面/npm 共享由 `smoke_native_package.py` 验证：四个并发任务、共同历史、并发配置/凭据更新、格式拒绝与无 Node 启动；它同时验证新默认 home 与旧目录分离。`smoke_shared_versions.py` 在旧基线可运行时测试新旧程序双向拒绝，在显式指定的同一隔离 home、空 PATH 和两种首开顺序下确认拒绝前后数据库逻辑不变、原创建者仍能读取，报告 `expected_compatibility:reject`。

Windows npm 0.2.0 在打开数据库前就因 SQLite URL 解析失败，无法作为可运行旧基线。只有这一固定版本和明确错误可以报告 `expected_compatibility:legacy_startup_unavailable`；报告保留真实 stderr，并单独验证失败的旧程序未改写当前数据、新版拒绝合成 schema 5 账本且无数据改写。此时没有验证旧程序成功创建的数据，也没有完成双向格式边界运行。两种报告均为 `shared_data_compatible:false`，不提供升级或续跑旧历史的模式，其他启动错误仍会阻止发布。

六个平台 release runner 执行 native smoke、配置/凭据多进程测试、数据使用期锁和 session ownership 测试，并下载固定 `@golutra/agent-<platform>@0.2.0` 仅作拒绝用负向夹具（构建时下载、禁用安装脚本，不随产品交付）。`shared-versions-<target>.json` 记录包及二进制摘要，正式清单拒绝缺失、摘要不匹配或仍声明升级兼容的报告。桌面项目负责 installer、签名、ConPTY、更新和卸载验收。内置 Agent 随桌面升级；npm 自行管理；任一卸载不得删除共享 home。未运行平台必须标未验证。

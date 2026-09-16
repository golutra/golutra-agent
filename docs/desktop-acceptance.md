# 桌面原生交付验收记录

> 命名空间分离后本页均为历史证据，保留当时摘要和旧命名。当前合同为 v2，最新结果见 [Agent 命名空间验收](agent-namespace-acceptance.md)。

> 以下原记录属于此前 schema 6 方案。2026-09-16 产品决定取消旧格式兼容后，该升级方案及其产物已作废；原测试结果仅作历史证据，不能用于当前代码验收。当前合同见 [共享数据合同](shared-data-compatibility.md)，当前测试结果另列于本文末尾。

日期：2026-09-16。执行者：主 agent 独立实施，未委派成员。

## 实际执行范围

本机 **macOS 26.5 / Darwin 25.5.0 / ARM64**。以 `2400ad7` 为基础的当前工作树构建，workspace 版本仍为 **0.2.0**。以下文件是未发布的 **dev profile 验收产物**，不是重新发布 0.2.0；其他平台尚未在本次环境执行。

| 项目 | 结果 |
| --- | --- |
| 七个原生程序 `cargo build --locked` | 通过 |
| 配置 crate 单元/独立进程测试 | 48 + 3 通过，含过期 revision 冲突与 Provider 事务锁内校验 |
| 存储 crate 单元/独立进程测试 | 61 + 3 通过，含 migration 6、活跃迁移拒绝、崩溃释放及未知格式保留 |
| app-server 单元测试 | 54 通过，含真实 RPC 设置补丁和并发 revision 冲突 |
| Auth 独立进程/未来格式测试 | 3 通过（含子进程入口）；并发写入 40 份 fixture 凭据，无丢失 |
| 目录身份与跨进程 session ownership | 3 通过；另有活跃元数据保护、orphan 恢复各 1 项通过 |
| Python 脚本完整回归 | 106 通过 |
| config/store/client/app-server 全 targets Clippy | 通过，`-D warnings`；首轮 auth 检查亦通过 |
| 格式、diff whitespace、开源元数据 | 通过 |
| npm JS 包及真实 PTY smoke | 通过 |
| 原生无 Node/npm 完整 smoke | 通过，含 stdio 设置接口、真实 PTY、双向删除 payload 后数据保留 |
| 真实已发布 npm 0.2.0 兼容矩阵 | 安全验收通过；**不兼容共用**，新版可升级/续跑旧历史，旧版在入口明确拒绝新格式 |

无 Node/npm 验收将 PATH 指向空目录，通过**解压后的绝对路径**运行程序，不修改用户的系统 PATH，不卸载用户的 Node，不访问用户真实 GOLUTRA_HOME。临时路径包含空格及中文。执行覆盖：

1. 原生 CLI、TUI、npm 平台包中的原生 CLI 独立输出正确版本和帮助。
2. 四个独立进程同时打开新数据库并执行 mock 任务，JSONL stdout 可解析；桌面/native npm 看到相同历史，SQLite `integrity_check=ok`。
3. 两个进程通过 localhost provider fixture 并发登录，两个新 profile 和两份凭据均保留；使用 mock 是离线执行测试，localhost fixture 是登录/probe 协议测试。
4. 注入未知 migration 后，两端打开均失败，数据库逻辑 dump 不变；注入未知 provider 配置版本后拒绝登录且原文不变。
5. 凭据 crate 验证未知 credentials version 的读、写、删除均拒绝，原文件不变。
6. 无 Node PATH 下直接启动 TUI，真实 PTY 初始画面与退出正常。
7. stdio 设置 read/patch 返回 revision；旧 revision 返回 `-32009`，未知/非法字段返回 `-32602`，文件不变；CLI StorageStatus 指向同一 home。
8. 分别删除临时 npm / 内置 payload，另一份原生程序仍可读取共享历史，配置和数据库保留。

首轮夹具问题：mock profile 名称固定，已改 localhost `/models` + completion fixture 验证不同 profile 的并发登录。追加测试修正了错误文本按终端宽度折行的断言：优先识别 `data_unsupported`，仍要求非零退出和完整逻辑 dump 不变。一次解压旧二进制 `--version` 超过 15 秒，改为与完整 native smoke 相同的有界 90 秒；这不是启动性能基准。

## 真实旧包发现及修复

从 npm 取得已发布 `@golutra/agent-darwin-arm64@0.2.0`（禁用脚本，没有修改全局安装）。首次尝试新旧混用失败：旧版恢复历史时出现 `unknown variant user_step`，尽管两者此前都使用 schema 5。保留这一失败结论，没有删除新事件或把未知类型当成功。

修复增加 migration 6，明确持久化事件读取格式边界，不改写旧事件。重新测试三种情形：旧程序先创建历史、新程序先创建历史、默认 HOME 下升级。新版均能保留原事件字节、续跑已有会话；旧版 `thread list` 和 `exec` 均在 migration 入口拒绝，拒绝前后 SQLite 逻辑 dump 相同，`integrity_check=ok`。

| 二进制 | 版本输出 | SHA-256 | 结论 |
| --- | --- | --- | --- |
| 已发布 npm 基线 | `golutra 0.2.0` | `35d626ec1a66be4f467962396ea5c48c1e48f0ede7e9d5226dc66f0726401664` | 不认识新事件/新 schema，必须升级或隔离 home |
| 当前开发 CLI | `golutra 0.2.0` | `b86595acd9dfabe6f07816e3cb3390e37100b4a9e0528f71781bd3a2803e31d2` | 支持 schema 6；同次构建的桌面/npm 可共用 |

机器报告 `shared-versions-aarch64-apple-darwin.json` 明确 `passed:true`、`expected_compatibility:upgrade_then_reject`、`shared_data_compatible:false`。**安全拒绝通过不等于共享通过。**版本号相同不等于数据兼容；正式交付须使用新的唯一版本，同时提供支持新格式的 npm 包。迁移前必须关闭不认识 usage 锁的旧进程，本轮没有宣称运行中的旧版可被新锁保护。

## 本地产物清单

当前目录：`dist/desktop-shared/`（Git 忽略，不提交二进制）。首轮 `dist/desktop-dev/` 保留作历史开发产物，不代表本轮实现。

| 文件 | SHA-256 |
| --- | --- |
| `golutra-agent-v0.2.0-aarch64-apple-darwin.tar.gz` | `4dc1c55cfe21688fa800ed7e5243116054230522371c7cdac0f78c9e679b4605` |
| `npm/golutra-agent-npm-darwin-arm64-0.2.0.tgz` | `3ddea31adb76d29cfe817c0b0c0c64f5b73d26a0b42f99ad6c9f0a4a195390c0` |

同目录提供 `.manifest.json`、`.sha256`、`.smoke.json`、真实旧包兼容矩阵和 `desktop-release-v0.2.0.json`。清单包含兼容结论，明确 `development_only: true`、`complete: false`。原生归档约 168 MiB，npm 平台包约 70 MiB，均为带调试信息的开发构建，不作为正式安装包体积数据。

固定 URL 由实际发布仓库和新版本生成；本地产物未上传，不可用本地清单中的 0.2.0 URL 拉取并假定得到这些修改。当前已发布版本与工作树版本相同不等于二进制相同，桌面必须锁定摘要。

## 未验证与正式发布门禁

- Windows x64/ARM64、Linux x64/ARM64、macOS x64：本次未实机验证；release workflow 已要求对应原生 runner 执行 native smoke。Windows 静态 CRT 构建参数已配置，尚不能声称实际产物已在干净 Windows 上验证。
- 本次没有构建/运行 Golutra 桌面 installer、签名/notarization、ConPTY 或桌面升级/卸载程序；上述删除目录测试验证数据布局隔离，不替代真实 installer 验收。
- 同构建共享和真实 npm 0.2.0 升级/拒绝已分别验证。任意其他历史版本、运行中不认识 usage 锁的旧版本、未来事件/配置/辅助 JSON 格式都未泛化承诺。不同版本共享限制见[兼容合同](shared-data-compatibility.md)。
- 没有调用真实云端模型，不声称验证离线推理或所有项目工具链。用户配置的 shell/Git/Node/MCP 等仍有各自系统依赖。
- 正式发布必须使用新的唯一版本、干净 release 构建、六平台 native smoke 汇总和摘要锁定；本任务没有发布 npm/GitHub Release，也没有 push 新修改。

## 当前验收：取消旧格式兼容（2026-09-16 15:55）

本节替代上面的 schema 6 方案和旧产物。用户确认尚无需要承接的用户数据，仅支持最新版，当前使用单一 schema 7 初始化基线。旧数据库、无账本旧表和历史 checksum 全部拒绝；Provider v1 不再迁移到 v2，读取配置不再迁移凭据。旧目录原样保留，须显式指定新的空 GOLUTRA_HOME 并重新登录。未改动开发者已有数据目录。

| 验证 | 当前结果 |
| --- | --- |
| Config | 48 单元 + 3 独立进程通过 |
| Store | 59 单元 + 4 独立进程通过；空库独占初始化、旧/未来版本拒绝、记录不去重不回填、退出释放锁 |
| App-server | 54 单元 + 13 进程集成通过；1 项手动 restart soak 保留 ignored |
| Client shared-home | 3 项通过，共享历史与 session 所有权保持 |
| Python | 107 项通过；拒绝用旧升级报告满足当前发布门禁 |
| 质量检查 | 四个相关 crate 全 targets Clippy、fmt、diff whitespace 通过 |
| 当前原生包 | macOS ARM64 无 Node/npm、四进程共享配置历史、并发登录、旧/未来配置拒绝、stdio revision、真实 PTY、双向移除 payload 均通过 |
| 真实 npm 0.2.0 | 两种首开顺序及默认 home 下双向拒绝；SQLite dump 不变，原创建者仍可读取，integrity_check=ok |

Rust 合计 184 项通过。首轮 App-server 单元测试暴露其临时 workspace 仍访问默认旧 home，已把 RPC 测试夹具改成独立临时 home；回归不依赖本机开发数据，不放松格式拒绝条件。

当前未发布 dev 产物位于 `dist/desktop-current-only/`：

| 文件 | SHA-256 |
| --- | --- |
| `golutra-agent-v0.2.0-aarch64-apple-darwin.tar.gz` | `68ed0ddbb0b92adae787f06beeb46cdd87810a55e4447603b1d44e946b75175e` |
| `npm/golutra-agent-npm-darwin-arm64-0.2.0.tgz` | `c346aa9ebb0410d94d00c00178ff3ab141677bf33800cec59e0fa37c55a54bc4` |
| 当前 CLI | `44ef1a191eb06129ddd7df119fd88cf1162e14a617dd3ab8be698d0fb87ee8ba` |

同目录含 native smoke、`shared-versions-aarch64-apple-darwin.json` 与清单，后者明确 `expected_compatibility:reject`、`shared_data_compatible:false`，不是旧版共用或升级承诺。桌面/npm 当前同次构建的共享证据来自 native smoke。清单为 `development_only:true`、`complete:false`；未重新发布 0.2.0，也未 commit/push。

其他五个平台、实际桌面 installer/ConPTY/签名/更新卸载仍未实机验证；本次使用离线 mock 和本地登录 fixture，不代表云端模型或用户工具链验收。

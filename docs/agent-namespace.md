# Golutra Agent 独立命名空间

本合同从 Agent 0.3.0 起生效。Golutra 桌面应用的源码、安装和数据不由 Agent 改名或迁移。npm 0.2.0 不具备新合同；桌面必须锁定 0.3.0 或后续版本的产物与摘要。

## 所有权

| 对象 | Agent 当前名称 | 边界 |
| --- | --- | --- |
| 主命令和原生程序 | `golutra-agent[.exe]` | 不安装 `golutra` 或 `golutra-cli` 别名 |
| 辅助程序 | `golutra-agent-tui`、`golutra-agent-app-server` 等 | 从同一包的绝对路径启动，不在 PATH 回退查找旧程序 |
| Cargo package / crate / 路径 | `golutra-agent-*` / `golutra_agent_*` / `crates/golutra-agent-*` | CLI package 是 `golutra-agent-cli`，主 binary 是 `golutra-agent` |
| npm 包 | `@golutra/agent`、`@golutra/agent-<platform>-<arch>` | 已有 Agent 产品域；bin 映射使用新命令 |
| SDK | `@golutra/agent-sdk`、`golutra-agent-sdk` | Python import 使用 `golutra_agent_sdk`；不提供旧模块别名 |
| 全局 home | `GOLUTRA_AGENT_HOME`，默认 `~/.golutra-agent` | Windows 无 HOME 时使用 USERPROFILE；不读取旧 `GOLUTRA_HOME` |
| 项目配置 | `<workspace>/.golutra-agent/runtime.json` | 不读取旧 `.golutra/runtime.json` |
| Agent 环境变量 | `GOLUTRA_AGENT_*` | Provider、凭据引用、进程、诊断和发布变量一致改名 |
| 后台服务 | `golutra-agent-<workspace>-<service>` | tmux / Compose / systemd 不接受旧资源标识；长服务名截断并加摘要 |
| HTTP 客户端身份 | `golutra-agent/<version>`、Agent originator/referrer | 实际 provider 域名和 provider ID 不改 |
| Agent 自有传输 | `x-golutra-agent-*` header、`stdio://golutra-agent-app-server`、Agent 命名的 IPC 文件 | SDK 和服务端一致更新，不保留旧 header 别名；桌面 IPC 协议不改 |
| 原生启动合同 | `desktop_launch.contract_version = 2` | 旧合同清单不能通过当前桌面交付门禁 |

`.golutra` 仍作为外部敏感目录受保护或被扫描排除。这不表示配置回退、数据迁移或目录所有权。基准报告中的历史 engine key `golutra` 只标识测量对象，不用于寻找程序或数据。

## 保留的外部桌面接口

桌面 IPC CLI 仍叫 `golutra-cli`，它不是 Agent CLI。以下环境变量属于宿主桌面通信合同，Agent shell 按既有环境继承规则传递：

- `GOLUTRA_COMMAND_IPC_ADDR`、`GOLUTRA_COMMAND_IPC_PATH`
- `GOLUTRA_COMMAND_SCOPE_TOKEN`
- `GOLUTRA_RUNTIME_PROFILE`、`GOLUTRA_RUNTIME_HOST_KIND`

它们不选择 Agent home，不加载 Agent provider，不控制 Agent 安装更新。第三方标准变量（例如 OPENAI_API_KEY）、上游 `api.golutra.cn` 和 provider ID `golutra` 也保持原协议。

## 启动、并存和数据

```sh
golutra-agent --version
golutra-agent --cwd /absolute/project
golutra-agent --cwd /absolute/project exec --json "检查项目"
golutra-agent resume THREAD_ID

# 源码开发入口；-- 后才是程序参数
cargo run --locked -p golutra-agent-tui -- --yolo
cargo run --locked -p golutra-agent-cli -- exec "检查项目"
```

原生主程序将无参数及交互参数交给同目录 TUI；CLI 子命令直接执行。单独 `cargo run -p golutra-agent-cli` 不会自动构建 TUI，交付时必须包含完整原生包。缺少配套 TUI 时明确报错，不启动 PATH 中的另一份安装。

桌面内置 Agent 和 npm Agent 是同一产品的两份安装，最新版相同构建默认共享 `~/.golutra-agent` 的配置、凭据和历史；它们与 Golutra 桌面自身的 `.golutra` 分离。可显式设置不同 `GOLUTRA_AGENT_HOME` 隔离测试，不按安装来源另造默认数据。

不迁移旧 home，不自动复制旧凭据，不兼容旧 schema。空目录初始化当前 schema 7；非当前版本/checksum 或 Provider v1 明确拒绝并保留数据。并发配置写入、revision 冲突、SQLite 和运行会话锁见[共享数据合同](shared-data-compatibility.md)。共享历史不允许两端同时接管同一运行中会话。

内置 Agent 随桌面更新，npm Agent 由 npm 更新。任一安装卸载都只处理自己的 payload，不删除共享 home 或项目目录。Agent 不触碰桌面应用、桌面 `golutra` 命令和其数据，也不自动卸载旧命令。

## 桌面项目后续接入清单

本轮未修改桌面项目，后续需独立处理：

1. 前端终端目录和命令健康检查目前使用 `golutra-tui`，改为锁定包中的 `golutra-agent` 绝对路径。入口包括 `src/shared/constants/terminalCatalog.ts`、`src/shared/types/terminal.ts`、`src/stores/terminal/terminalCliHealthStore.ts`。
2. 后端 `backend/crates/golutra-terminal-engine/src/default_members/golutra_agent.rs` 和 `session/{state,mod}.rs` 使用同一程序选择结果：默认内置，显式外部路径覆盖，不用 PATH 自动挑旧包。
3. 如显式指定 Agent home，传 `GOLUTRA_AGENT_HOME`；桌面自己的 `.golutra`、bundle identifier 和 IPC 变量不改。
4. 构建锁定合同 v2 的新版本、平台、架构、URL 和 SHA-256，完整打包；更新和卸载只处理内置原生目录。
5. 在桌面 PTY/ConPTY 验证启动、恢复、退出和 npm 并存。Agent 原生 smoke 不代替桌面安装器或其他 OS 运行验收。

完整平台矩阵及发布方式见[桌面集成](desktop-integration.md)，当前结果见[命名空间验收](agent-namespace-acceptance.md)。

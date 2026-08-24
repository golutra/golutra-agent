# Runtime Entry Points

## Purpose

Golutra exposes several execution surfaces, but there is only one agent
runtime. Every surface eventually uses the same `SessionCommand`,
`RuntimeEvent`, `RuntimeApplication`, `RuntimeHost`, and
`AgentEventProjector` contracts. An entry point may translate framing or
render a projection; it must not create a second task state machine.

The current implementation deliberately supports both a simple local mode and
a long-lived shared mode:

```text
CLI / TUI / SDK / MCP adapter
        |
        v
  RuntimeTransport
        |
  RuntimeApplication
        |
  RuntimeHost (one execution owner)
        |
  RuntimeEvent -> state/context/user/debug/evaluation projections
```

## Process Model

| Surface | Default process | Workspace behavior | Primary use |
| --- | --- | --- | --- |
| `golutra exec` | One short-lived client process; embedded host by default | `--cwd` is the host cwd and permission boundary | scripts, CI, other agents |
| App Server | One explicit long-lived user-level process | one server attaches many canonical cwd values; each attachment has its own session/workspace routing | IDEs, multiple clients, durable service |
| Python/TypeScript SDK | Library process; connects to an App Server | each client creates an authenticated cwd attachment | application integration |
| `golutra mcp-server` | MCP stdio process; connects to the local daemon by default | tool calls may select a workspace; clients are cached per cwd | expose Golutra to another agent |
| Remote TUI | TUI process separate from the App Server | TUI attaches to the selected remote cwd | interactive UI over a remote/shared runtime |

Therefore, a workspace does not require its own daemon. In shared mode one
App Server owns multiple workspace attachments. The workspace still controls
cwd, file policy, state partition, default session resolution and history
visibility. It is not a process boundary.

The ordinary no-daemon path remains intentionally simple: `golutra` without
arguments and the compatibility alias `golutra-tui` construct an
`EmbeddedTransport`, so a single invocation owns its local `RuntimeHost`. Use
`golutra exec` for a headless turn, and use the daemon or App Server surfaces when another
process must observe or control the same running task.

### Interactive defaults and runtime settings

After npm installation, `golutra` with no arguments opens the TUI. The
explicit `golutra exec ...` subcommand remains headless and is suitable for
scripts and CI. TUI runtime defaults are read once at startup from two
non-secret JSON layers:

```text
$GOLUTRA_HOME/runtime.json       # global defaults
<workspace>/.golutra/runtime.json # project overrides
session controls                  # in-memory overrides for this TUI session
```

Project values override global values; session controls override both. An
explicit CLI flag such as `--execution-mode` or `--tool-profile` wins over the
loaded files. Allowed fields are `provider_profile`, `model`,
`execution_mode` (`open`/`strict`), `verify_on_change` (`auto`/`off`/`never`),
`tool_profile` (`coding`/`full`) and `reasoning_effort`
(`default`/`low`/`medium`/`high`/`xhigh`). Unknown fields, symlinks, malformed
JSON, secret-shaped fields and files larger than 64 KiB are rejected; API keys
and tokens remain in the credential store or environment references.

## 1. Headless Exec

`exec` is the non-interactive equivalent of one agent turn. It does not open a
TUI and can be used from a shell or an automation runner:

```bash
golutra --cwd "$PWD" exec "inspect the workspace"
golutra --cwd "$PWD" exec - < prompt.txt
golutra --cwd "$PWD" exec --json "run the checks"
golutra --cwd "$PWD" exec --output-last-message /tmp/answer.txt "summarize the change"
golutra --cwd "$PWD" exec resume <thread-id> "continue the same task"
golutra --cwd "$PWD" exec \
  --run-dir /absolute/path/to/golutra-run \
  --approval-mode auto "run the benchmark task"
golutra --cwd "$PWD" exec \
  --yolo "run in unrestricted modification mode"
golutra --cwd "$PWD" exec \
  --run-dir /absolute/path/to/golutra-run \
  --allow-network --approval-mode auto "run a task that needs a configured proxy"
golutra --cwd "$PWD" exec \
  --completion-criterion "tests pass" \
  --verify-program cargo --verify-arg test --verify-arg --workspace \
  "implement the requested change"
golutra --cwd "$PWD" exec \
  --no-project-verifier-discovery \
  "make the requested change without running a discovered project check"
golutra --cwd "$PWD" exec \
  --task-contract /absolute/path/to/task-contract.json \
  "implement the requested change"
golutra --cwd "$PWD" exec \
  --execution-mode strict --tool-profile full \
  "run a contract-driven task with managed tools"
```

Output rules:

- normal progress and bounded runtime/tool observations go to `stderr`;
- the final assistant message goes to `stdout`;
- `--json` emits normalized lifecycle/item/runtime events as JSONL on
  `stdout`, with no human progress stream;
- `-` or a non-terminal stdin supplies the prompt;
- `exec resume` resolves a durable thread and sends a new turn through the
  same runtime lane;
- `--ephemeral` uses an isolated embedded runtime and cannot be combined with
  `--daemon` or `--connect`.

New CLI, TUI, App Server and SDK turns use `open` execution with the compact
`coding` tool profile by default. `open` leaves planning, tool order and
stopping to the provider while retaining policy, cancellation, budgets and
audit facts. The `full` profile exposes every registered extension; use
`--tool-profile full` when a caller needs low-frequency process, code-graph or
extension tools. Use
`--execution-mode strict` when an unstructured prompt must be translated into a
deterministic completion contract. The interactive TUI accepts both switches,
for example `golutra-tui --execution-mode strict --tool-profile coding`.
Explicit task contracts and external verifiers remain authoritative completion
signals even if the caller selected `open`; unrelated payload metadata never
changes the execution mode. Verification-on-change is off by default in the
interactive UI and can be enabled explicitly through runtime settings.

Raw `SessionCommand` payloads persisted before these fields existed keep the
legacy completion adapter and full tool surface. This compatibility rule is
limited to omitted fields; sending `execution_mode: "open"` opts into the new
model-owned path. Python uses `execution_mode` and `tool_profile`; TypeScript
uses `executionMode` and `toolProfile`. Ordinary turns normalize the values into
the durable task payload before execution or queueing.

Steering is a continuation of the active task: it keeps the execution mode,
task contract, verifiers, elapsed budget and current tool profile while
appending the new user instruction at the next complete tool-batch boundary. A
raw steering command may include `tool_profile` to change only the model-visible
tool surface. The Rust, Python and TypeScript steering APIs expose the same
optional override. Inherited fields are omitted from the durable queued payload
so a preceding queued turn remains authoritative when the steer reaches the
runtime boundary.

`--yolo` is an explicit per-task full-access mode. It is
available to embedded, `--daemon` and `--connect` exec transports, including
`exec resume`. `golutra-tui --yolo` applies the same capability to prompts from
the interactive, `remote`, `inspect` and `driver` TUI entrypoints. It bypasses
workspace and sensitive-path checks, shell/P0 blocks, approval requests and
child-tool OS sandboxing. In an embedded runtime it also requests and enables
the network capability. In daemon or remote mode the request is still subject
to the app-server host's capability configuration. It does not disable tool
argument validation, remove timeout or cancellation bounds, skip verification,
or suppress observations. Because the child runs process-only, callers must not
treat `network.enabled=false` as OS-enforced network isolation in yolo mode. A
queued turn inherits the active task's mode and cannot switch it. Outside-workspace
changes are observed when a tool reports them, but they are intentionally
omitted from workspace rollback checkpoints and make that checkpoint explicit
as incomplete. The MCP tool schema does not expose this switch because those
arguments may be generated by another model rather than a trusted operator.

Network access is a two-party capability. Child tools are isolated by default;
`--allow-network` both grants the embedded host capability and requests it for
this turn; `--yolo` makes the same request as part of its full-access profile.
The durable `TaskCreated` event records `requested`, `enabled` and a reason
under `execution_capabilities.network`, while process tool facts record the
capability actually used. A turn cannot enable network access when the host
did not grant it. The explicit `--allow-network` flag is intentionally rejected for `--daemon`,
`--connect` and persisted-bundle inspection because those runtimes are owned by
another process.

Ordinary `--ephemeral` discards its state when the process exits. `--run-dir`
creates one new absolute, owner-only run directory for an isolated invocation;
it therefore implies `--ephemeral`. The legacy spelling
`--ephemeral-state-dir` remains accepted. `--run-dir` cannot be combined with
`--daemon` or `--connect`, because a benchmark run must not join a shared
runtime store.

After a turn reaches a terminal result, including a failed verifier result,
Golutra writes this layout:

```text
<run-dir>/
  manifest.json
  state/
    runtime.sqlite
    artifacts/
    workspaces/<workspace-hash>/{checkpoints,rollouts,memory,evaluation,...}
  observations/
    manifest.json
    sessions/<session-id>/
      thread.json
      events.jsonl
      conversation.jsonl
      tasks/<task-id>/trace.json
  debug-export/
    manifest.json
    ... redacted handoff files
```

`state/` is the canonical raw runtime state. `observations/` is a stable,
full-owner-only projection built from the same `RuntimeEvent` and
`TaskTraceService` facts: `events.jsonl` contains the full event stream,
`conversation.jsonl` contains the user/assistant history, and `trace.json`
contains context snapshots, artifact/evidence metadata, verification plan and
record, post-task jobs, evaluation and integrity facts. Its manifest lists
file checksums and explains incomplete or retained-away data. This lets a test
harness consume structured observations without querying SQLite.

`debug-export/` is separately redacted and suitable for handoff. Failure to
build that optional portable export is recorded in the top-level manifest but
does not discard the raw state or structured observations. Before freezing a
run bundle, Golutra gives each discovered durable post-task evaluation a
bounded opportunity to reach a terminal state, then reloads the event boundary
and task trace. A job that exceeds that bound remains explicitly pending and
marks the observation manifest incomplete.

For an active `exec --run-dir` turn, Golutra first writes an atomic checkpoint
whose top-level terminal outcome is `in_progress`. This checkpoint is not a
success claim: it records the session/task identity, the event prefix and the
raw state needed for recovery. The normal terminal export replaces it with a
`result` or `error` outcome. If a supervisor or benchmark harness kills the
CLI before that export, the checkpoint remains reopenable through
`--run-bundle`; runtime recovery may append interruption facts, and a later
evaluator can refresh the observations without guessing the missing terminal
result. Such a bundle remains explicitly non-terminal until its manifest is
refreshed.

Golutra continues to read the active provider profile and credentials from
the configured global `GOLUTRA_HOME`, and never copies them into the run
directory. Raw state and `observations/` may contain workspace content,
prompts and tool output, so keep the whole run directory owner-only and treat
it as sensitive.

`--completion-criterion` may be repeated. `--verify-program` and repeated
`--verify-arg` values declare an argv-based external verifier that runs after
the model stops and before the terminal decision. No shell parses this command.
Its cwd must remain inside the attached workspace; network remains disabled by
the runtime sandbox; timeout and retained output are bounded. The verifier
produces the same artifact, evidence, tool event and `VerificationRecord` facts
as built-in checks. A failed verifier prevents `completed`, even when the model
claims success.

When no explicit verifier field is supplied and the task contract requires
objective validation or workspace evidence, Golutra conservatively discovers
one project check from trusted, workspace-local manifests. Supported project
families are Cargo, Node, Python and Go; discovery never scans arbitrary shell
scripts, follows manifest symlinks or reads oversized manifests. Automatic
discovery only runs when exactly one supported project family is present at the
workspace root. A mixed or ambiguous root returns no implicit verifier and
requires the caller to choose the intended check explicitly, so an unrelated
sibling project cannot affect the task. An explicit verifier always wins. An
explicit empty verifier list disables discovery, as does `exec
--no-project-verifier-discovery`. This omission-versus-empty rule is the same
through App Server and both SDKs. MCP sends an explicit empty list so an agent
invoking Golutra cannot cause project code to run implicitly.
Discovery happens before the turn is queued: the command boundary serializes
the selected verifier list and the normalized `TaskContract` into the durable
task/queued-turn payload. The execution worker therefore consumes an
independent contract snapshot for every turn, including legacy callers, rather
than rediscovering or inheriting verifier state halfway through execution.

`--task-contract` accepts a JSON object that makes the completion boundary
explicit for CI, SDK and other-agent callers:

```json
{
  "schema_version": 1,
  "workspace_change": "required",
  "required_paths": ["crates/example/src/lib.rs"],
  "required_file_contents": [
    {"path": "crates/example/src/generated.txt", "content": "ready\n"}
  ],
  "completion_criteria": ["the workspace tests pass"],
  "require_objective_validation": true,
  "verification": "independent",
  "max_correction_rounds": 1
}
```

Required paths must be portable workspace-relative paths; absolute paths,
drive-prefixed paths and parent traversal are rejected before execution. The
`required_file_contents` entries require an exact bounded file value. Runtime
verification resolves them through workspace policy, rejects symlink escapes
and never performs an unbounded read. The
same `task_contract` field is available through App Server, MCP, Rust,
TypeScript and Python turn options. Older callers remain supported by an
application-boundary compatibility adapter, while the runtime terminal gate
always evaluates a validated `TaskContract`.

External verifiers are trusted caller configuration, not model output. They may
execute workspace code and must therefore only come from the user, CI harness or
authenticated SDK caller. Model-generated tool calls keep the ordinary policy
path. `--approval-mode prompt` is the default and denies requests when stdin is
not interactive; `on-request` and `granular` are accepted aliases. `deny`
always denies and `never` is an accepted alias. Explicit
`--approval-mode auto` approves only requests the runtime already classified as
`Ask`. These guarded modes cannot override `Block` or `Deny`, shell
metacharacter guards, sensitive paths, workspace boundaries or the no-network
sandbox. `--yolo` is the explicit full-access escape hatch and therefore
overrides the guarded approval path for that turn.

When an external verifier is declared, the runtime runs it after the model
stops and does not ask the model to duplicate the same check through `shell`.
Without one, a workspace mutation must be followed by a fresh objective check;
if the model tries to finish first, the runtime returns a bounded verification
request to the model and records that retry in the canonical event stream.

The final exit status is non-zero unless the runtime returns a verified
`completed` turn. A model saying it is finished is not sufficient.

## 2. App Server

Start the long-lived server explicitly:

```bash
golutra app-server --addr 127.0.0.1:47831
# or
cargo run -p golutra-app-server -- --addr 127.0.0.1:47831
# stdio JSON-RPC supervisor mode
golutra app-server --stdio
```

The server publishes endpoint metadata and an owner-only transport token under
`$GOLUTRA_HOME/app-server/`. It provides:

- authenticated HTTP command/query and cursor-based SSE;
- owner-only Unix IPC for local clients, including `/rpc` through the shared
  bounded HTTP-like IPC envelope;
- JSON-RPC over HTTP, WebSocket and newline-delimited stdio;
- thread start/resume/fork/list;
- turn start, steer, interrupt, crash reconciliation and approval resolution;
- `agent/event` notifications projected from the durable event stream;
- attachment routing from canonical cwd to one shared runtime registry.

The JSON-RPC methods are intentionally small and transport-neutral:

```text
runtime/info       runtime/attach
thread/start       thread/resume       thread/fork       thread/list
turn/start         turn/steer          turn/interrupt    turn/takeover
task/reconcile     approval/resolve    turn/status       runtime/events/replay
```

HTTP and WebSocket clients must send the bearer token and the negotiated
`x-golutra-protocol-version`. Remote HTTP connections use
`GOLUTRA_TRANSPORT_TOKEN`; local daemon discovery reads the owner-only token
file. The server binds loopback by default and validates local Host/Origin
headers.

`runtime/attach` creates a server-issued attachment capability and binds one
runtime actor to it. That binding, rather than a caller-provided HTTP header,
is authoritative for steer, interrupt, approval and takeover checks.
`x-golutra-actor-id` may be sent as client metadata, but changing or spoofing
it cannot change control ownership. WebSocket, stdio, HTTP and Unix IPC all
resolve commands through the attachment actor, so a second client must call
`turn/takeover` before controlling an active lane.

After a host restart, `turn/status` can report `interrupted` or `uncertain`.
`uncertain` means an incomplete side effect or background process could not be
proven complete. The runtime rejects new work until an attached controller
calls `task/reconcile` with `no_side_effect_observed`,
`side_effect_observed`, or `abandon`; the equivalent local command is
`golutra reconcile --decision <decision>`. Reconciliation is durable and can
never convert the old task to `completed`.

JSON-RPC messages without an `id` are notifications: HTTP returns `204 No
Content`, while WebSocket and stdio send no response. Turn streams publish
`agent/event`; if their projector or transport terminates before a runtime
terminal fact, WebSocket and stdio publish an `agent/error` notification
instead of silently dropping the stream. Unix IPC does not add a second
raw-line JSON-RPC protocol; clients post the same JSON-RPC body to `/rpc`
inside `IpcHttpRequest`, and the shared Axum router applies the same
authentication, attachment and notification rules.

## 3. Python and TypeScript SDK

Both SDKs are generated from the Rust protocol schema for data types and share
the same high-level lifecycle:

```python
client = GolutraClient(base_url, cwd, transport_token)
thread = client.start_thread()
turn = thread.run("inspect the workspace")
for event in turn.events():
    ...
result = turn.wait()
```

The TypeScript API has the corresponding `startThread`, `Thread.run`,
`TurnHandle.events()` and `TurnHandle.wait()` methods. Both SDKs also expose
`steer`, `interrupt`, `takeover`, approval resolution, `resume`, bounded
history/event pages, event replay, task reconciliation, task trace and artifact
range reads. Python exposes `Thread.reconcile_task`; TypeScript exposes
`Thread.reconcileTask`. Terminal turn results include the correlated optional
`VerificationRecord`, including failed verification, rather than asking SDK
callers to infer success from the final message. They use the same Agent SSE
projector as `exec` and MCP, so command/turn correlation and terminal status do
not vary by language.

### Execution defaults and compatibility

The new high-level methods intentionally opt into the model-owned surface:

```text
Python:     Thread.run / run_streamed       -> execution_mode=open, tool_profile=coding
TypeScript: Thread.run / runStreamed         -> execution_mode=open, tool_profile=coding
```

Callers that need the pre-profile behavior can use `Thread.run_legacy` and
`run_legacy_streamed` in Python, or `Thread.runLegacy` and
`runLegacyStreamed` in TypeScript. Those methods omit both fields so the server
keeps interpreting the turn as the legacy completion adapter with the full
tool surface. The same omission rule applies to older raw `SessionCommand`
payloads and persisted tasks; it is not a global mutable default. A raw caller
opts into the new behavior only by sending `execution_mode: "open"` (and may
send `tool_profile: "coding"` or `"full"`).

This distinction is intentional for compatibility: upgrading the SDK changes
the default only for its named high-level API, while existing integrations that
construct protocol payloads directly retain their previous semantics until
they opt in.
`Thread.run` also accepts `external_verifiers` in Python and
`externalVerifiers` in TypeScript. These fields use the generated
`ExternalVerificationSpec` contract and reach the same Runtime verifier path as
headless exec and App Server JSON-RPC. Omitting the field enables conservative
project verifier discovery for qualifying task contracts. Passing an empty
list disables it. Python also exposes `discover_project_verifiers=False`, and
TypeScript exposes `discoverProjectVerifiers: false`, as explicit opt-outs.

SDK connection steps are fixed:

```text
validate absolute cwd and token
-> GET /runtime/info and negotiate protocol range
-> POST /runtime/attach
-> use command/query/thread/agent-event APIs
-> reattach only after an explicit 410 Gone
```

SDKs do not own the runtime loop or persist a competing conversation history.
Each SDK client may send an actor identity as diagnostic metadata, but control
ownership is always the server-issued attachment actor. This keeps turn
control isolated even when several SDKs share the same workspace attachment;
spoofing the client actor header cannot grant control. Agent SSE subscriptions
retain both the immutable
`start_cursor` returned by `turn/start` and the latest consumed `cursor`.
After a disconnect, the server replays from `start_cursor` to rebuild the
command-to-turn projector state, suppresses facts through the consumed cursor,
and then resumes delivery. This avoids losing turn correlation when the
binding event predates the reconnect cursor. Transient transport errors and
retryable HTTP statuses reconnect with bounded backoff; permanent 4xx statuses
end the subscription and surface an SDK error. A `TurnHandle` also observes
the subscription completion promise, so a clean stream close without a
terminal event fails promptly instead of waiting forever.

## 4. MCP Server

`golutra mcp-server` is a stdio MCP adapter. It translates MCP framing into
the shared `AgentClient` API and exposes two tools:

- `golutra`: create or resume a durable thread and execute one turn;
- `golutra-reply`: continue a supplied `thread_id`.

Examples:

```bash
# default: connect to the user-level daemon
golutra --cwd "$PWD" mcp-server

# connect to an explicit App Server
golutra --cwd "$PWD" --connect http://127.0.0.1:47831 mcp-server

# deliberately isolate the adapter in its own embedded runtime
golutra --cwd "$PWD" mcp-server --embedded
```

The default is daemon-backed so an MCP caller can share state with TUI, CLI
and SDK clients. `--embedded` is an explicit isolation choice. Workspace
clients are cached by canonical cwd, non-interactive approvals are denied by
default, and the adapter returns the verified turn result plus an optional
bounded event sample.

## 5. Remote TUI

Remote TUI separates rendering from execution:

```bash
export GOLUTRA_TRANSPORT_TOKEN="$(<\"$GOLUTRA_HOME/app-server/transport.token\")"
golutra-tui --cwd "$PWD" remote --url http://127.0.0.1:47831
```

The TUI process owns terminal input, scrolling and rendering only. The remote
App Server owns sessions, tools, cancellation, approvals and durable events.
The existing compatibility form remains available:

```bash
golutra-tui --cwd "$PWD" --connect http://127.0.0.1:47831
```

`remote` cannot be combined with `--daemon` or `--connect`; this keeps the
transport choice explicit. A remote TUI sees the same user projection and
event cursor as any other attached client, while developer/debug views remain
an opt-in projection. Its provider footer/status is read from the Runtime's
redacted `ProviderState` query. Remote mode never reads or writes the TUI
machine's provider files and does not open a local OAuth/setup dialog; provider
credentials must be configured on the App Server host.

## 6. Run Bundle 与外部评估

`--run-dir` 生成的是可延续的 owner-only observation bundle，而不是一次性
日志目录。任务结束后，外部 harness 可以读取：

```text
<run-dir>/manifest.json
<run-dir>/observations/manifest.json
<run-dir>/observations/sessions/.../events.jsonl
<run-dir>/observations/sessions/.../tasks/.../trace.json
<run-dir>/state/runtime.sqlite
```

评估器把结果写成 `ExternalEvaluationRecord`，然后在同一个 bundle 上执行：

```bash
golutra --run-bundle /absolute/run-dir eval ingest /absolute/evaluation.json
# When evidence lives outside the evaluation JSON directory:
golutra --run-bundle /absolute/run-dir eval ingest \
  --artifact-base /absolute/evaluator-output /absolute/evaluation.json
```

命令会验证 source trace digest、runtime identity 和 canonical result digest，
默认以 evaluation JSON 所在目录作为相对 evidence ref 的根目录；外部 harness
也可以通过 `--artifact-base` 显式声明独立的 evaluator 输出根目录。普通文件会在
固定大小预算内复制为 owner-only `external_evaluator_evidence` artifact，记录
SHA-256/size，并为 assertion 生成 `EvidenceRecord`；原路径后续被修改不会改变
已导入事实。外部失败会在追加事件后重新计算 failure episode、诊断切片和改进
候选。成功后原子刷新 `observations/` 与 `debug-export/`。刷新允许合法的 evaluator
事件追加，但会验证旧 task event prefix 与 prior trace boundary 没有被修改；
每次打开 bundle 还会先验证 trace 文件的 manifest checksum、SQLite 中的
source event prefix，以及 prior trace 引用的 artifact metadata/blob checksum。
物理 SQLite/WAL checksum 的变化本身不等于篡改。输入、collector、trace 或
evaluator 只完成一部分时，bundle 会保留 `*.pending.json` 和具体缺失原因，
不会生成假 complete。

## 7. Terminal-Bench 适配边界

Terminal-Bench 只通过 `tools/terminal_bench/golutra_tbench_adapter.py`
适配，不修改上游 harness。每个 trial 的 Golutra invocation 都使用独立
`--run-dir /logs/golutra-runtime`；适配器优先读取 `<trial>/golutra-runtime`，
兼容旧的 `sessions/golutra-runtime`，并把
`terminal-bench-evaluation.json` 交给 `eval ingest`，并以 trial 根目录作为
`--artifact-base`，使结果、pane 和命令记录可以被导入为不可变 evidence。找不到结果、manifest、
collector 或 trace 时，保留 `golutra-evaluation.pending.json`，而不是丢掉
原始 observation。

当 adapter 配置 `proxy_url` 或 `GOLUTRA_TBENCH_PROXY` 时，它会把代理变量传入
容器，并给嵌入式 `exec` 加上 `--allow-network`；没有代理配置时保持默认无网络。
collector 的显式路径优先，自动选择时只考虑仓库内最新且不早于当前 Rust 源码的
可执行 `golutra-cli`，避免用旧二进制解释新 observation。

适配器在等待 agent 命令之前就启动 host-side collector。这样即使
Terminal-Bench 的外部 timeout 放弃了正在运行的线程，后续写入
`results.json` 后仍能找到 active checkpoint、从 observation index 恢复
session/task identity，并让 `eval ingest` 对重新打开后的最终 trace 使用
`auto` digest。旧版本没有 checkpoint 时仍会保留 pending 文件，不把缺失的
bundle 伪装成成功。

## Shared Event and Projection Rules

All five surfaces follow the same lifecycle:

```text
start/queue command
-> durable TaskCreated/TurnStarted facts
-> provider/tool/approval/progress facts
-> AssistantMessage and VerificationCompleted
-> TaskCompleted or TaskAborted
-> AgentEventProjector emits terminal turn result
```

`AgentEventProjector` filters by session, task, turn and command correlation.
An old task's terminal event cannot complete a newly queued turn. The durable
`RuntimeEvent` stream remains the source of truth; Agent stream events are a
bounded adapter contract for clients.

## Verification Matrix

The implementation is exercised at each boundary:

```bash
cargo test -p golutra-client agent_projection::tests
cargo test -p golutra-app-server --test rpc_process
cargo test -p golutra-cli --test mcp_server_process
cargo test -p golutra-cli --test exec_process
cargo test -p golutra-tui remote_transport_attaches_to_the_real_app_server
python3 -m unittest discover -s sdk/python/tests -v
cd sdk/typescript && npm test
```

These tests cover lifecycle correlation, stable tool item IDs, HTTP/SSE,
WebSocket and stdio JSON-RPC notification semantics, multi-client actor
isolation, MCP process execution, independent `exec`/`exec resume` processes,
SDK high-level handles and a real remote TUI attachment. They do not claim that a
remote deployment fleet or browser-based Web onboarding is part of this
runtime entry-point layer.

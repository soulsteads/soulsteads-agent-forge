# Harness-backed agent workloads

A `harness` work item (or workflow step) runs an agent workload in an explicitly selected **native agent harness** (Codex CLI, Claude Code, OpenCode, Soulsteads) as part of a durable DBOS workflow. DBOS owns ordering, checkpoints, retry and dispatch. The harness owns its own session and tool loop. Forge owns admission, scope, effect fences, output validation and the user-visible outcome. There is no second scheduler or agent framework, and no silent fallback to another runner or to a direct model call.

## Runner profiles (operator authority)

Only an operator-approved **runner profile** in the private `config.json` can name an executable. Workflow definitions and run parameters select a profile *by name*. Edit `runners` while the service is stopped:

```json
"runners": {
  "codex-ws": {"adapter":"codex","executable":"/abs/path/codex","permission_mode":"workspace-write","timeout_seconds":300},
  "opencode-qwen": {"adapter":"opencode","executable":"/abs/path/opencode","provider":"qwen","model":"qwen38",
    "permission_mode":"workspace-write","timeout_seconds":300,
    "env":{"OPENCODE_CONFIG":"/abs/path/opencode.json"}}
}
```

| Field | Meaning |
| --- | --- |
| `adapter` | One-shot: `codex`, `claude`, `opencode`, `soulsteads`. Streaming transports: `opencode-acp`, `soulsteads-acp`, generic `acp`, `pi` (see [ACP and Pi transports](#acp-and-pi-transports)). `stdio` is a test-only generic contract and never qualifies a harness |
| `executable` | Absolute path; no `PATH` lookup |
| `args` | Fixed extra operator arguments. Flags that would widen authority the adapter derives from `permission_mode` (sandbox, approval, permission-mode, tool allow lists, `--yolo`, working-directory overrides, model overrides) are rejected at load |
| `model`, `provider` | Requested model identity in harness-native spelling; absent means the harness native default |
| `permission_mode` | `read-only` or `workspace-write`: the ceiling. The adapter derives its non-interactive permission flags from it. Interactive approval is never granted |
| `timeout_seconds` (1-3600), `max_output_bytes` (1 KiB-4 MiB) | Wall-clock and captured-output bounds |
| `env` | Static non-secret values (names containing KEY/TOKEN/SECRET/PASSWORD/AUTH/CREDENTIAL are rejected) |
| `allow_unconfined` | `pi` only, and mandatory for it: the operator's explicit acknowledgement that Pi does not confine file access to the workspace and can read the provider credentials in its agent directory. Refused for every other adapter |
| `sandbox` | `pi` only: `{"kind":"bwrap","executable":"/abs/bwrap"}` wraps the Pi launch in bubblewrap (read-only filesystem, user HOME and the `/run/user` and `/run/dbus` runtime directories hidden, writable invocation workspace and Pi home only). The executable must report `bubblewrap` from `--version`, otherwise the profile is unavailable and refused before dispatch. Required for `permission_mode: workspace-write` on `pi` |
| `env_passthrough` | Names copied from the Forge process (default `HOME`, `PATH`, `USER`, `LANG`; `pi` never receives `HOME`). The child environment is otherwise cleared, so Forge tokens never reach the harness |

Authentication stays in the harness's native home (`~/.codex`, `~/.claude`, ...). Forge never copies, symlinks or logs it, and scrubs configured secret values and token-shaped strings from everything it retains. The profile configuration is bound into the run's admission digest: changing a profile while a run is in flight refuses further dispatch.

Dispatch freshly probes executable availability/version before recording an effect intent; the runner table's short-lived cache is not dispatch authority. Missing authentication may only become observable when the native harness contacts its provider: that failure remains an uncertain attempt, never a fallback. The executable path and profile are operator authority, not an immutable installation: Forge does not pin the executable's bytes, native configuration files, or provider deployment. Keep those stable while work is admitted/running, record their versions with qualification evidence, and requalify after upgrades. A version probe is not proof of a served model; unsupported required served-model observation refuses before dispatch.

Existing homes must add `harness` to the actor's allowed `types`; new homes include it. `agent-forge runners list`, MCP `forge_runners`, the dashboard and `snapshot.runners` list profiles read-only with version probe, availability and the adapter capability matrix. `agent-forge runners test NAME` (MCP `forge_runner_test`, the Providers **Test** button) re-probes one profile now, uncached: executable, sandbox launcher, `--version`, and which passed-through environment variables are set. It sends no prompt and uses no model; it is not a substitute for a real harness run.

## Soulsteads native adapter

The production adapter name is `soulsteads`; the executable is an operator-installed
`soulsteads-harness`. It uses the harness CLI and its TOML/JSONL contract without a native library dependency or ACP wrapper. Configure an explicit model and endpoint:

```json
"native-files": {
  "adapter": "soulsteads",
  "executable": "/abs/path/soulsteads-harness",
  "model": "your-model",
  "provider": "openai-compat",
  "permission_mode": "workspace-write",
  "timeout_seconds": 300,
  "env": {"HARNESS_BASE_URL": "http://127.0.0.1:8000/v1"},
  "env_passthrough": []
}
```

This narrow adapter accepts no `args`. `model` is mandatory; `provider` is absent
or `openai-compat`. The only static environment entry is `HARNESS_BASE_URL`, which
is required and may not contain userinfo, a query or fragment. Passthrough defaults
to empty; only `PATH`, `LANG` and the optional `HARNESS_API_KEY` are accepted.
Selecting `HARNESS_API_KEY` requires HTTPS and makes the native provider require
that credential at launch; no credential value is copied into configuration.
Forge supplies its usual minimal PATH when absent. No native user home/config,
MCP servers, skill roots, instruction files, proxy environment or new grants are
inherited. Broader native configurations and interactive approvals are unsupported.

Forge generates `native.toml` and a private `native-home/` next to the invocation's
workspace, then runs `--config PATH --home PATH run --format jsonl -` with the
existing prompt on stdin. The effective workspace grant is exactly that invocation's
workspace. `read-only` grants `read_file` and `list_dir`; `workspace-write` also
grants `write_file`. The workload permission must fit the profile ceiling. These
are native built-in file-tool restrictions, **not an OS sandbox**. No approval
bypass, session resume, grant-change acknowledgment or replacement launch is used.
The native engine's default turn/tool limits remain in force; Forge supplies the
profile deadline/output bounds and a 4 MiB per-write bound.

Only `structured_output`, `tool_use` and `session_correlation` are declared. Usage
and served-model reporting are not mapped in this first packet, even when native
events contain them; requiring either capability refuses before dispatch.

Success requires strict UTF-8 JSONL with ordered sequence numbers, a new session
bound to this Forge invocation's agent/model, one native run, paired tool events
within the admitted grants, and one completed terminal matching the final assistant
message. Missing/foreign/duplicate terminals, unsupported events, error/denial/
approval/interruption events, unsuccessful tool results and noncompleted terminal
states fail closed, including when the executable returns zero. A completed
terminal with a truncation/filter reason is also refused. The final text then
passes through Forge's existing JSON extraction/schema and artifact validators.
The existing receipt/capture records include `native_run_id` and `config_sha256`
alongside the native session ID and Forge invocation ID. The admitted action digest
binds the profile, including the endpoint; the config digest binds generated grants.
Native journals are retained for inspection, never adopted as Forge receipts.

Deterministic fixtures establish protocol handling only. The native CLI emulator
in `cargo test` uses Python 3.11+ (`tomllib`); it makes no model calls and executes
no native harness. Those fixtures do not qualify all crash windows or native permission behavior.
The existing containment policy applies, including scan-mode limits. With scan
containment, Forge terminates the discovered process tree and lets the harness
controller reap its direct child. When Forge observes cancellation and confirms
teardown, it records the attempt as `cancelled`; if the deadline wins first,
the attempt is `timed_out`. The external effect remains `uncertain` and must be
reconciled before any relaunch.

## ACP and Pi transports

Besides the one-shot adapters (process exit is completion), Forge can drive a harness over a **streaming protocol**. This changes the transport only. DBOS still owns the workflow: the agent run is one checkpointed `forge.flow.<slot>` step, the effect intent is recorded `uncertain` before launch with a stable effect key, and `completed` only after the validated result is receipted. There is no second scheduler, retry, queue or resume engine.

| Adapter | Launch (adapter-owned) | Notes |
| --- | --- | --- |
| `opencode-acp` | `opencode acp` in the invocation workspace | Same `provider`/`model` fields as `opencode`; the native permission block is `ask` for edit, bash and external directories and `deny` for web fetch, so Forge's policy (below) decides |
| `soulsteads-acp` | `soulsteads-harness --config native.toml --home native-home acp` | Same generated per-invocation config, validation and no-inherited-configuration rules as `soulsteads`; opt-in alternative to the JSONL adapter |
| `acp` | the profile's executable and fixed `args` (for example `copilot --acp`) | Generic ACP agent. Requires `args`; `model` is refused (select the model through the fixed args). Forge cannot enforce native tool scope for an arbitrary agent beyond the permission policy and the capabilities it does not advertise |
| `pi` | `pi --mode rpc --no-themes --no-session --no-extensions --no-skills --no-prompt-templates --no-context-files --no-mcp --tools <derived>` | `provider` and `model` come together. `--no-themes` keeps Pi from loading user theme files (they are user-controlled input outside the workspace). `--tools` is `read,grep,find,ls` (plus `edit,write` for `workspace-write`); a shell tool is never granted. Pi has no native approval prompt, so its permission control is the tool allowlist. **Pi does not confine file tools to the workspace**: see [Pi confinement gate](#pi-confinement-gate). Authentication and `models.json` stay in Pi's own agent directory (set `PI_CODING_AGENT_DIR` through the profile `env`) |

Profile `args` cannot carry flags that widen authority or select tools/sessions/models (`--yolo`, `--allow-all-tools`, `--allow-tool`, `--tools`, `--extension`, `--session`, `--model`, ...); these are refused at load.

**ACP client behavior** (Agent Client Protocol v1 over stdio, `agent-client-protocol` 3.3.0): `initialize` advertises no filesystem or terminal client capability, `session/new` binds `cwd` to the invocation workspace with no MCP servers, and one `session/prompt` turn runs. Streamed `session/update` notifications are retained (scrubbed, bounded) in the private `state/harness/<id>/updates.jsonl`; the final message after the last tool call is the run output. Agent thoughts are not retained. Any other server-to-client request is answered with JSON-RPC method-not-found, never left hanging.

**Permission requests are answered deterministically, deny by default.** Forge allows (selecting the agent's one-shot `allow_once` option, never an `allow_always` option) only a workspace-confined `read`/`search`, and a workspace-confined `edit` only under a `workspace-write` ceiling. Paths must be absolute, free of `..`, and resolve inside the workspace after following symlinks. Everything else (`execute`, `fetch`, `delete`, `move`, unknown kinds, a request without locations, an outside path) selects the agent's `reject_once` option, or `cancelled` when none is offered. Each decision is recorded in the attempt (`permission_decisions`) and the output `harness.permission_decisions`. Unattended work never silently approves an interactive request. Note that OpenCode ends the turn with stop reason `cancelled` after a rejected permission; Forge records that as a failed attempt and the effect stays `uncertain` (an earlier allowed edit may have happened).

### ACP permission policy limits

The deterministic policy trusts what the agent reports. The `kind` and `locations` in a `session/request_permission` are agent-supplied: an agent that mislabels an operation (for example reports `read` for something that writes, or lists a harmless path while operating on another) is decided on its claim, and Forge cannot verify the real operation. Deny-by-default and the `reject_once`/`cancelled` answers only constrain honest or mistaken agents; they are not a sandbox. A path is judged at decision time: symlinks are expanded component by component (including dangling ones, whose target must itself resolve inside the workspace) and a loop refuses, but the agent performs the operation afterwards, so a path can be replaced between the decision and its use (a decision-versus-use race that a real OS-level confinement would close). Treat ACP agents as workspace-trusted code and use the profile ceiling (`read-only`) when the workload does not need edits.

### Pi confinement gate

Pi's file tools are not confined to the workspace, and Pi needs read access to its provider credentials (`models.json` in the agent directory). Therefore:

- A `pi` profile is refused at load unless it sets `"allow_unconfined": true`.
- A `pi` profile with `permission_mode: workspace-write` is refused unless `sandbox` names bubblewrap, and the launcher must actually be bubblewrap: its `--version` output must start with `bubblewrap`, checked when the runner is listed and again before every dispatch (an executable that only carries the name makes the runner unavailable). **The sandbox is a write jail and a HOME jail, not a read jail.** Under it the filesystem is read-only except the invocation workspace and Pi's home, the invoking user's HOME is replaced by an empty tmpfs, `/tmp` is private, and the user session runtime directory `/run/user` and the system bus directory `/run/dbus` are replaced by empty tmpfs mounts (so the D-Bus session socket, keyring and agent sockets are not reachable); the executable's directory is read-only. Everything else on the filesystem stays **readable** (system files, other users' world-readable data, other paths under `/run` such as DNS stubs), and the **network is not unshared** because Pi must reach its model endpoint. Do not rely on it to keep data from a Pi agent that can read it; run Pi as a dedicated OS user or add an operator wrapper when read isolation matters.
- The Pi agent directory (`PI_CODING_AGENT_DIR` in the profile `env`) is bound **read-write** into the sandbox because Pi must lock and update files there. It must therefore be an existing, dedicated directory (a real directory, not a link or file, absolute, no `..`) that holds only what Pi needs: it may not be `/`, the user's HOME, the Forge home or any configured Forge path (`paths.*`, including a relocated state directory), and it may not contain any of them (an ancestor) or lie inside any of them. A profile that violates this is refused at configuration load and by provider edits.
- Pi never receives the user's `HOME`: Forge supplies an empty private `pi-home` directory in the invocation directory, and `HOME` cannot be set through `env` or `env_passthrough`. Use a dedicated agent directory holding only the credential the endpoint needs.
- The capability matrix has a `workspace_confinement` capability (a workload may `require` it). The ACP family and the native soulsteads adapter have it; `pi` does not, so a workload that requires it is refused for a Pi profile.

**Completion, cancel, timeout.** Success requires stop reason `end_turn` and a final message that passes the JSON extraction and `output_schema` checks. The profile deadline, `runs cancel` fence and output bound send `session/cancel` (or Pi `abort`), then terminate the whole contained process tree. Process exit alone is never success. A lost connection, crash, timeout or cancellation is an `uncertain` attempt: attempt states `turn_completed` and `protocol_error` are added to the existing set, and the usual restart policy (terminate, never adopt) applies.

**Session resume is not implemented** for any adapter, including ACP `session/load` and Pi `--session`. A recovery-required run is reconciled by an operator; Forge never starts a replacement session.

## Workload definition

```json
{"type":"harness","input":{
  "profile":"codex-ws",
  "permissions":"workspace-write",
  "require":["structured_output","tool_use"],
  "instructions":"...", "prompt":"Task text, may use ${/steps/seed/output/text}",
  "inputs":{"seed":{"$ref":"/steps/seed/output/text"}},
  "output_schema":{"type":"object","properties":{"file":{"const":"evidence.txt"}},"required":["file"],"additionalProperties":false}}}
```

The schema is closed: there is no field for an executable, arguments or environment. `permissions` (default `read-only`) cannot exceed the profile; `require` lists capabilities (`structured_output`, `tool_use`, `mcp_tools`, `session_correlation`, `usage_reporting`, `served_model_observation`) and an adapter lacking one refuses **before dispatch**. An unknown profile, a profile name containing anything but identifier characters (for example a path), an unavailable executable, or a missing capability fails closed and creates no workspace or process. The profile name may be a `$ref` to workflow input; the name is resolved and the profile bound again at dispatch.

`output_schema` is a literal local JSON Schema (no remote `$ref`/`$id`). It is not rendered as a template. Inside a workflow, `prompt`, `instructions` and `inputs` render earlier-step references like every other step.

## Execution and result

Each invocation gets a stable identity `forge-harness-<hash(run, step slot, attempt)>` (attempt is always 1; a linked retry is a new run), a private per-run directory under `state/harness/<id>/` with its own `workspace/` (the harness working directory, with `inputs.json`), `attempt.json` and `captured.json`. The harness sees the task, inputs and output contract in one prompt and must end with one JSON object.

Forge extracts that object, validates it against `output_schema`, and returns `{result, text, artifacts, harness, diagnostics}`: `result` is the validated object, `artifacts` the bounded workspace manifest (path, bytes, sha256, small text content) that dependent steps may read, `harness` the profile, adapter, version, requested/served model, native session ID, attempt, timings, usage and tool-event count. A zero exit code alone is never success; missing, non-JSON or schema-invalid final output fails.

## Durability, recovery and limits

- The result is a flow effect receipt (`forge_flow_effects`): intent is recorded `uncertain` **before** launch and `completed` only after the validated result is durably receipted. Replay and recovery reuse the receipt and **never relaunch** the harness.
- Timeout, cancellation (`runs cancel` fences the run; Forge terminates the harness within about a second), output overflow, non-zero exit, malformed output and process loss leave the effect `uncertain` -> `RECOVERY_REQUIRED`. There is no blind retry and no replacement session. The exact attempt (`harness_attempt` in the effect observation) shows the state (`prepared`, `running`, `exited`, `timed_out`, `cancelled`, `output_exceeded`, `launch_failed`, and after a Forge restart `orphan_terminated`, `process_lost`, `interrupted_before_launch`, `orphan_unresolved`), containment kind, pid, timings, the validation verdict (`result`: `validated` or `rejected` plus a reason) and a scrubbed stderr tail. An operator inspects the workspace and settled evidence and reconciles with `runs reconcile --effect-id`.
- A harness tool call performed before a failure may have happened. DBOS checkpoints around the invocation do not checkpoint the harness's internal tool calls, cannot undo external actions and do not prove exactly-once effects.

### Containment of the whole descendant tree

Each invocation is contained without privileges. Profile field `containment`: `auto` (default), `cgroup`, `scan`.

- **cgroup v2 (preferred).** Forge creates a leaf cgroup `forge-<invocation>` under its own cgroup (user-delegated under systemd user sessions), and the child joins it before `exec`. Membership is inherited through fork, `setsid`, `setpgid` and double-fork, and `cgroup.kill` terminates the whole set atomically, so a `setsid` or daemonized descendant does not escape. Requires a writable cgroup v2 subtree for the Forge user; if unavailable under `auto`, Forge falls back, and under `cgroup` it refuses before dispatch.
- **scan (fallback).** Forge becomes a child subreaper and finds descendants by parent ancestry plus the `FORGE_INVOCATION_ID` environment marker, killing repeatedly until none remain. Residual limit: a descendant that scrubs its environment **and** detaches from the ancestry (double fork) can escape; use a cgroup-capable host for strict containment. The attempt record states which mode ran.
- Residual limit in both modes: Forge cannot contain processes it did not start, effects already performed on external systems, or a harness's own remote daemons.

### Forge crash and restart (orphan policy)

At startup, before DBOS recovery can run, Forge reconciles every attempt recorded `prepared` or `running` by an earlier process. Policy: **terminate, never adopt.** A surviving tree is killed, identified by cgroup membership, or (scan mode) by a pid whose recorded start time still matches plus the environment marker; a bare pid number is never trusted, so a recycled pid is left alone. The attempt is rewritten truthfully (`orphan_terminated`, `process_lost` or `interrupted_before_launch`, `orphan_unresolved`) and the effect stays `uncertain`: the run is `RECOVERY_REQUIRED`, the result is never adopted, and no second harness is launched. Termination is checked after signalling: a still-live or unreadable cgroup leaves `orphan_unresolved` and refuses service startup before DBOS recovery. Resolve the surviving process tree, then restart; no completed result is invented. Until the restart reconciliation runs, a harness orphaned by a Forge crash keeps running (it holds no Forge authority but can still change its workspace or external state), so restart the service promptly after a crash and inspect before reconciling `not-applied`. Reboot invalidates recorded pids (the boot id is checked).

### Isolation and backup rules

- Forge does not sandbox the harness: `permission_mode` selects what each harness enforces natively (Codex sandbox, OpenCode permission block, Claude restricted tools). The harness runs as the Forge operating user, so it can read the Forge home (including `config.json`), and its native configuration applies for external adapters. Soulsteads uses only its generated config. Codex loads the user's native MCP servers. `mcp_tools` is therefore not a declared Forge capability for any adapter: Forge neither scopes nor verifies MCP availability. Run Forge as a dedicated OS user, or use an operator wrapper, when stronger confinement is required.
- Backups include `state/harness/<id>/workspace/**` and the native Codex `state/harness/<id>/result.txt` as normalized private regular files and directories; symlinks, special files and unreadable entries in those native output paths are skipped (never followed, never fatal). All other paths keep the strict private-tree checks. New Codex result files are created privately before dispatch; normalization also supports homes containing older native output files.
- The artifact manifest is bounded (64 artifact rows plus one truncation marker, 1,024 visited entries, depth 32, 4 MiB per file); excess is recorded in the manifest as `truncated`/`oversize` entries and never converts a validated successful run into a failure. `tool_events` is the number of tool-use events the adapter observed (Codex, OpenCode) or `null` when the harness does not report it (Claude).
- Runner listings (CLI, MCP, dashboard, snapshot) are readable by any actor with `read`. They show profile name, adapter, executable **file name only**, model/provider, permission mode, limits, containment mode, argument *count*, environment variable *names* only, observed version and availability. Absolute paths, arguments and values are not listed.
- Native recovery of a resumable session is not implemented: no harness is resumed after an interrupted attempt.

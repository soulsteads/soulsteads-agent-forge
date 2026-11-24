# Workflows and tool-using agents

Choose a `workflow` when you can specify the steps and how data moves between them. Choose an `agent` when a configured model needs to decide which permitted tools to call. Both use immutable work definitions and can run on demand or on a native DBOS schedule. The dashboard, CLI, HTTP API, and MCP use the same actor policy.

The actor needs access to the top-level work type and every nested work type it uses. New operator homes include `workflow` and `agent`, but never install or grant plugins automatically. Operators configure model aliases and trusted executables; a work definition cannot supply new endpoints, paths, credentials, actors, or tool permissions.

## Deterministic data flow

Start with [`report.json` in the quick start](../README.md#2-run-a-two-step-workflow). The first step, `produce`, reads `/params/task`. The second, `consume`, declares `needs: ["produce"]` and reads its output. To author the same definition in the dashboard, select `workflow` and enter the JSON object's `input`.

`{"$ref":"/params/task"}` preserves the referenced JSON type. `${/steps/produce/output/text}` interpolates a JSON Pointer into a string. Scalars use their textual value; objects and arrays use JSON text. Referenced output is data and is never evaluated again as a template. The only reference roots are `/params` and `/steps/ID/output`; a step can read only earlier steps listed in its `needs`. The final output mapping can read all completed steps. Missing paths and invalid resolved input schemas fail before the corresponding action.

Actions can be inline `echo`, `fail`, configured `model`, configured `plugin.*`, `agent`, or nested `workflow` definitions. A reusable named action is `{"work_id":"reviewer","params":{"task":{"$ref":"/steps/produce/output/text"}}}`. Named work must belong to the same actor and remain enabled. Its immutable definition and configured destinations are bound into admission. For inline nested agents/workflows, `action.input.params` is the explicit boundary for parent references; child prompts and steps use the child's fresh parameter/step context. Named ordinary leaves render their authored input once against the passed parameters. Model-selected tool arguments are always literal JSON.

A workflow returns `{"steps":{"produce":{"output":...},"consume":{"output":...}},"output":...}`. Without an output mapping, `output` is the last step's output. All actions run sequentially in authored order; dependencies constrain data access. Unsupported gates, foreach, arbitrary expressions, failure-continuation rules, shell scripts, or legacy reference fields are rejected. Explicit executable plugins remain the extension boundary.

## Workflow credentials

`credential_check` sends a Bearer-authenticated `GET` to a fixed, operator-configured
URL and returns only its successful HTTP status, for example `{"status":200}`.
Use it for a single read-only check, not for publishing, arbitrary HTTP, feeding a
response body into a workflow, or authenticating an agent or plugin. A grant
names one actor, immutable workflow ID, and direct step ID. The workflow supplies
only a literal alias. Request URLs, headers, a body, and template substitutions
are not accepted from work definitions or invocation parameters.

Use a dedicated Forge home with no configured or installed executable plugins
and no native harness runners. Forge refuses to combine them with workflow grants
in the same home. Run the service under an OS account trusted with the credentials;
systemd credentials are **not** isolated from other processes of that same user
or from root. Use a separate service account if other same-user workloads are not
trusted. Homes without grants keep their existing execution behavior; Forge never
passes `CREDENTIALS_DIRECTORY` to executable plugins.

While Forge is stopped, add `credential_check` to the intended actor's `types`
in the private `config.json`. Add an operator-managed `workflow_credentials`
object (this example assumes an existing actor named `operator`):

```json
{
  "release-read": {
    "actor": "operator",
    "work_id": "release-readiness",
    "step_id": "check-api",
    "url": "https://api.example.com/release/status"
  }
}
```

The object above is the **value of** `workflow_credentials`. Each key is also the
systemd credential ID. The URL must use HTTPS with normal certificate validation,
or HTTP to the literal loopback addresses `127.0.0.1` or `[::1]` for a trusted
local endpoint. User info, query strings, fragments, proxies, redirects, and HTTP
retries are not used. Loopback HTTP trusts whichever process binds the configured
port; use it only for a trusted local service. Choose a read-only, narrowly scoped
token for the endpoint. Keep secrets out of the URL path as well: the URL is part
of the grant metadata retained in backups.

Keep both the encrypted files and the systemd credential directory outside the
Forge home, including through bind mounts or alternate paths. Never put the
encrypted files in the repository. For a systemd user service named
`soulsteads-forge`, one way to provision an encrypted file
without putting the token in a command argument or shell history is:

```sh
install -d -m 700 "$HOME/.config/agent-forge-credentials"
systemd-ask-password 'Release API bearer token' |
  systemd-creds encrypt --user --name=release-read - \
    "$HOME/.config/agent-forge-credentials/release-read.cred"
systemctl --user edit agent-forge.service
```

Add the following drop-in, substituting the actual absolute encrypted-file path
(only a path, never a token). Start or restart the service to load it:

```ini
[Service]
LoadCredentialEncrypted=release-read:/absolute/private/path/release-read.cred
```

Save this definition as `release-readiness.json`:

```json
{
  "type": "workflow",
  "input": {
    "steps": [
      {"id": "check-api", "action": {"type": "credential_check", "input": {"credential": "release-read"}}}
    ]
  }
}
```

```sh
soulsteads-forge workflows create release-readiness --file release-readiness.json
soulsteads-forge workflows run release-readiness --key first-check --wait
```

The same grant is enforced when the run starts manually or from a schedule. A
missing file or mismatched grant fails before the outbound request. Audit records
identify the actor, run, step and alias; the credential and response body never
enter run output, workflow exports, or normal Forge backups. A non-success status
or transport failure stops the step; Forge retains the standard unresolved effect
receipt rather than automatically replaying the request. For rotation, replace
the encrypted file safely and restart the unit. For revocation, disable the token
at the upstream service, remove the grant and systemd binding, and restart Forge.
Forge backup and restore preserve grants, not credentials: arrange separate
reprovisioning through the issuer. Encrypted systemd files may be bound to the
original account and host.

## Real agent tool loops

An operator might configure `plugin.lookup` with an input schema accepting a `query` string. An agent definition can then be:

```json
{
  "type": "agent",
  "input": {
    "model": "local",
    "instructions": "Use the lookup tool for evidence. Return a JSON object with a supported summary.",
    "prompt": "Investigate ${/params/task}",
    "params": {"task": "the current status"},
    "tools": [{"name": "lookup", "type": "plugin.lookup"}],
    "max_rounds": 4,
    "max_tool_calls": 8,
    "output_schema": {
      "type": "object",
      "properties": {"summary": {"type": "string"}},
      "required": ["summary"],
      "additionalProperties": false
    }
  }
}
```

Register it as `reviewer`, run it directly, schedule it, or reference it from a workflow action. The model must support OpenAI-compatible function tool calls. The host offers only the explicitly named configured plugins. Every returned name, call ID and JSON argument object is validated before dispatch; the actor must still have execution permission for the agent, model and actual plugin. Duplicate IDs, unknown tools, malformed arguments, schema violations and exhausted budgets stop the flow without dispatching those calls.

Each tool result is sent back with its original tool-call ID. The next model round sees retained assistant/tool history. A final answer returns `text`, optional parsed `json`, served `model`, actual `rounds`, tool-call input/output evidence and `messages`. When `output_schema` is present, final text must parse as JSON and validate. Schema references must remain local; remote schema fetches and `$id` rebasing are refused.

Defaults are no tools, at most 8 model rounds and 32 tool calls. Explicit bounds can lower those maxima. A model asking for more tools on its final permitted round fails before those tools execute. No hidden cloud fallback, autonomous tool installation, prompt repair or automatic effect retry occurs.

## Parameters, schedules and control

```sh
printf '%s\n' '{"task":"new evidence"}' > params.json
soulsteads-forge work run report --key report-request-1 --params params.json
soulsteads-forge runs inspect ACTUAL_RUN_ID
soulsteads-forge schedules create daily-report --work-id report --cron '0 9 * * *' --tz America/Chicago --params params.json
soulsteads-forge schedules list
```

`--params -` reads one JSON object from stdin. Explicit values shallowly override definition defaults. The canonical `run` and `schedule` API/MCP methods accept the same optional `params` object; the dashboard has corresponding JSON fields. Parameter bytes are stored immutably and digest-bound to admission. Reusing a request key with different parameters is refused. Nonempty invocation parameters require a root workflow or agent; ordinary root leaves keep their existing behavior.

Use the returned canonical schedule name for pause, resume and delete. Verify an actual due run and its terminal output with `soulsteads-forge schedules occurrences NAME` (MCP `forge_schedule_occurrences`, API `schedule_occurrences` with `name` and optional `limit` 1–100). Each entry carries the native occurrence identity `sched-NAME-TIME`, its `scheduled_time`, the native `handoff` status of the firing, and the single admitted `run_id` with a bounded summary of that run's own status and output; one occurrence admits at most one run. The run is resolved from the occurrence's deterministic admission identity, not from the handoff's acknowledgement, so a handoff that failed or is still in progress after admitting still reports the run that maintenance submits and executes. `run_id: null` means no admission exists for that occurrence (for example a refusal before admission, or a handoff not yet admitted). Readback uses retained native firings and admissions, so it survives service restart, pause and delete. It requires `read` authority and is limited to the schedule's own actor. Creating a schedule or admitting a run does not prove completion.

## Reading workflow results

`inspect` (CLI `runs show RUN_ID --json`, MCP `forge_inspect`) adds `type`, `actions` (`cancel`/`retry`/`reconcile` currently applicable to the status; the service still re-checks every request) and, for workflows, `workflow.steps[]` in authored order with `state` (`completed`, `failed`, `running`, `pending`, `not_reached`, `cancelled`, `unknown`), timing, bounded `output`, and `reason` for the failing step, plus `workflow.failure {location, step, index, reason}`. Reasons are value-free: a step location such as `root/step:consume` and a fixed cause such as `input reference failed: flow reference target is missing` or `explicit flow failure`. A step is `completed` only with a recorded output or published workflow result. `failed` and `not_reached` are asserted only from compatible evidence (an error checkpoint or a located refusal consistent with the native checkpoints); a step with a recorded checkpoint is never `not_reached`. When the checkpoints and refusal audit are missing or contradict each other, the affected steps are `unknown`, `workflow.uncertain` is true, and `failure.location` is `unknown` with no step, the refusal details listed as `unverified_refusals`, and a pointer to the raw native checkpoints and audit; `failure.contradicted` marks a located failure that other native evidence conflicts with.

Invalid definitions are refused before storage with the authored step position and ID, for example `invalid flow definition: step 2 (consume): needs must contain unique earlier step IDs`. `validate` (CLI `workflows validate --file F`, MCP `forge_validate`) runs exactly the registration checks without storing anything.

`runs` (CLI `runs search`, MCP `forge_runs`) searches the actor's own runs newest first with optional `work_id`, `status` and `limit` (1–100, default 25). Each page scans at most 500 admissions; continue with the returned `next_before` cursor. Entries carry the run summary, `type`, `failure` and `actions`.

## Durability and effect evidence

Orchestration runs in the native DBOS workflow body. Every deterministic action, model round and tool call has a stable separate DBOS checkpoint. Echo delays use native durable sleep outside the action step. No parent waits for a child queued behind it; all flows share the existing two-slot native queue.

Each external call has its own immutable effect identity, resolved action/destination digest, intent, receipt and output. Inspect exposes an `effects` array with exact `effect_id` values. The snapshot contains bounded run/work summaries; read a full definition with `work show ID` (MCP `forge_work`, API `work`). Default inspection keeps the final output and summarizes nested definition, parameter, step-output, effect-action and effect-output values larger than 2048 bytes as `{summary:true,bytes,preview}`. Read an exact effect with `runs inspect RUN_ID --effect-id EFFECT_ID`, or one native step with `--step-id NUMBER`; the selectors are mutually exclusive. The corresponding MCP/API `inspect` parameters are `effect_id` and `step_id`. Selected responses contain run identity/status plus `effect` or `step`, without duplicating the entire transcript. Recent audit history is bounded to 100 entries. Completed checkpoints are replayed from DBOS, and publication verifies retained receipts and current authority. A crash during a call leaves uncertainty and prevents automatic resend. Cancellation persistently fences future flow calls; it cannot undo an active external operation. A completed receipt remains retained even when cancellation prevents normal workflow publication.

An authorized operator can record settled provider evidence:

```sh
soulsteads-forge runs reconcile RUN_ID --effect-id EFFECT_ID --decision completed --note 'Provider receipt verified after settlement' --settled --output verified-output.json
```

Use `not-applied` only with explicit provider evidence that no operation occurred. An omitted selector is allowed only when exactly one unresolved flow effect exists. Reconciliation records the exact effect decision and permanently fences the parent; it does not resume a cached native failure or change that terminal execution into success. This MVP refuses linked retries of any flow with external-effect history, including completed and reconciled calls. Native recovery of interrupted pending workflows remains supported. This deliberate limit avoids redoing earlier operations when a later action fails.

Bounds: 32 steps per workflow, nested depth 4 beyond the root, and a total budget of 64 authored nodes plus model turns and tool calls. Strings are at most 64 KiB; runtime context is at most 1 MiB. These are safety limits, not performance claims. The pinned DBOS Rust SDK remains pre-production; qualification applies to the tested paths, configured model and fixtures.

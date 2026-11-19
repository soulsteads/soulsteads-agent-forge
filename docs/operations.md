# Operating Forge

Use this guide to configure the service and operate it from the dashboard, CLI, HTTP API, or MCP. For a complete first run with no optional integrations, follow the [quick start](../README.md#quick-start).

## Install and run

Use Linux, Rust 1.97 or later (via rustup), a C compiler (`cc`, needed to build the bundled SQLite and TLS crypto), and a dedicated, empty PostgreSQL database. Backup and restore also require `psql`, `pg_dump`, and `pg_restore` matching the database server's major version on `PATH`. The optional token snippet in [Operational CLI](#operational-cli) uses `jq`; any other private way to set `FORGE_TOKEN` works without it. See [tested preview path](support.md#tested-preview-path) for the exact versions exercised.

From the root of a source checkout or extracted source archive:

```sh
cargo install --locked --path .   # installs agent-forge into ~/.cargo/bin (add it to PATH)
export FORGE_HOME=./forge-home     # or pass --home; see Configuration below for the default
agent-forge init
export FORGE_DATABASE_URL='postgresql://user@127.0.0.1:5432/forge'
agent-forge serve --port 8110
```

The home defaults to `$XDG_DATA_HOME/forge` (`~/.local/share/forge` when `XDG_DATA_HOME` is unset), not `./forge-home`; there is no default database, and `serve`, `backup` and `restore` refuse to start without one. Set database credentials privately when required (for example in `~/.pgpass` or a private environment). The connection must specify its host, port, user, and database. Do not put credentials in a committed configuration or command history. `serve` runs in the foreground; stop it with Ctrl-C or SIGTERM. Restarting it with the same home and database resumes the same work, runs and schedules.

Open <http://127.0.0.1:8110> and enter the scoped token from the Forge home's private `config.json` (`actors.operator.token` in a new home). The listener accepts numeric loopback addresses only. Every API operation, including reads, requires authentication. Work definitions and run history belong to the actor that owns them.

`config.json` contains the stable `home_id`, `actors`, `models`, `plugins`, and harness `runners`. An actor has a private `token`, `capabilities`, and allowed work `types`. Capabilities are `read`, `register`, `execute`, `schedule`, `reconcile`, `plugins` (install, enable, disable and uninstall [installed plugins](plugins.md)), `approve` (decide approval steps), `configure` (edit settings, model aliases and runner profiles) and `transfer` (start online backups). New homes grant all of them, and the `approval` work type, to `operator`; existing homes add them while the service is stopped. Configure these while the service is stopped; changes take effect on restart. Keep the home and its files private to the operating user.

A model alias maps to an explicit OpenAI-compatible endpoint:

```json
{"base_url":"http://127.0.0.1:8000/v1","model":"your-model","key_env":null}
```

Set `key_env` to the name of a private environment variable when authentication is required. Authenticated remote endpoints require HTTPS. No provider, model endpoint, or cloud fallback is selected implicitly. Model requests have bounded time and response size; qualify each backend you configure.

## Configuration

For settings that support multiple sources, precedence is **command-line flag → environment variable → `config.json` → default**. The home is resolved before `config.json` is read.

| Setting | Flag | Environment | `config.json` | Default |
| --- | --- | --- | --- | --- |
| Home | `--home` | `FORGE_HOME` | n/a (it locates the file) | `$XDG_DATA_HOME/forge`, else `~/.local/share/forge` |
| Database URL | `--database-url` | `FORGE_DATABASE_URL` | `database.url`, or `database.url_env` (name of a variable holding the URL) | none: refused with instructions |
| Pool / connect timeout | | | `database.max_connections` (2 to 100), `database.connect_timeout_seconds` (1 to 300) | 8 / 5 |
| Listener | `serve --host`, `serve --port` | `FORGE_HOST`, `FORGE_PORT` | `server.host`, `server.port` | `127.0.0.1`, `8110` (loopback only) |
| Client service URL | `--url` | `FORGE_URL` | `server.url` | `http://<host>:<port>` of the listener settings |
| DBOS identity | | | `dbos.application_prefix`, `dbos.schema_prefix` | `forge`, `forge` (names are `<prefix>-<home_id>` and `<prefix>_<home_id>`) |
| DBOS capacity | | | `dbos.work_concurrency`, `dbos.model_concurrency`, `dbos.server_concurrency` | 2, 2, 1 |
| Harness default `PATH` | | `FORGE_HARNESS_PATH` | `harness.default_path` | `/usr/local/bin:/usr/bin:/bin` |
| cgroup root for containment | | `FORGE_CGROUP_ROOT` | `containment.cgroup_root` | `/sys/fs/cgroup` |

The `harness.default_path` applies only to a runner that neither passes `PATH` through nor sets `env.PATH`. Prefer `database.url_env` so the credential stays out of `config.json`; a literal `database.url` works but makes `config.json` a secret of its own (it already holds actor tokens, so keep it private). Prefer the environment to `--database-url`, because command-line values are visible to other local users. `database.url` and `database.url_env` are mutually exclusive, and error messages name a setting but never print its value.

The `paths` section relocates runtime directories. A relative value resolves against the home; an absolute value is used as given; `..` is refused. `init` creates only the home and `config.json`; `serve` creates every runtime directory privately (mode 0700, including each missing intermediate level, below the home or outside it). A symbolic link or non-directory anywhere on a configured path is refused naming the setting. The first `serve` records the state location in `.state-location`; a later change of `paths.state` / `FORGE_STATE_DIR` on that home is refused with the recorded and configured locations, because the admission store and DBOS identity live there (move the directory yourself, then replace the marker). `init` deliberately leaves no `state/owner.lock`.

| `paths` key | Environment | Default |
| --- | --- | --- |
| `state` | `FORGE_STATE_DIR` | `<home>/state` |
| `outputs` | `FORGE_OUTPUTS_DIR` | `<state>/outputs` |
| `effect_locks` | `FORGE_EFFECT_LOCKS_DIR` | `<state>/effect-locks` |
| `harness` | `FORGE_HARNESS_DIR` | `<state>/harness` |
| `logs` | `FORGE_LOGS_DIR` | `<home>/logs` |
| `plugins` | `FORGE_PLUGINS_DIR` | `<home>/plugins` (installed plugin releases under `releases/`, install staging under `staging/`; archived by backup, so it must lie inside the home) |
| `cache` | `FORGE_CACHE_DIR` | `<home>/cache` (disposable; excluded from backups) |
| `dbos_identity` | | `<state>/dbos-identity.json` |
| `dbos_admission` | | `<state>/dbos-admission.sqlite` |

Dependent defaults follow a relocated `state`. `config.json` itself and the `.restore-incomplete` marker always sit directly in the home. An existing `config.json` without any of these sections loads and behaves as before; the sections are strict, so unknown keys are refused. Example:

```json
{"database":{"url_env":"MY_FORGE_DB","max_connections":6},
 "server":{"port":8201},
 "paths":{"state":"data","logs":"/var/log/forge","cache":"/var/cache/forge"},
 "harness":{"default_path":"/opt/tools/bin:/usr/bin:/bin"}}
```

Behavior changes from earlier previews: the home default is no longer `./forge-home` (pass `--home ./forge-home` or set `FORGE_HOME` to keep it), and `--url` no longer has a built-in default flag value (the same address is used when nothing is configured). Plugins are executed directly from their configured absolute `command` argv; Forge never wraps them in a shell, so a plugin that needs one names it explicitly as `command[0]`.

## Harness-backed agent workloads

A `harness` workload runs an agent in an operator-approved native harness (Codex CLI, Claude Code, OpenCode, Soulsteads) as a durable step: typed inputs in, output validated against a declared schema, results receipted and never re-run on replay. Runners are configured in `config.json` `runners` and selected by name only. See the [harness guide](harness.md) for capabilities and limits; `agent-forge runners list` shows profiles read-only.

## Operational CLI

Supply a scoped bearer token through `FORGE_TOKEN` in the caller's environment. Never put it in command arguments or a prompt. For the operator actor of a new home, one way that keeps the value out of history and output is `export FORGE_TOKEN="$(jq -r .actors.operator.token "$FORGE_HOME/config.json")"`. Global `--url` overrides `FORGE_URL`, which overrides the `server` section of the home's `config.json` (default `http://127.0.0.1:8110`); `--token-env NAME` selects another environment variable. These options work before or after a subcommand. The CLI starts no executor and opens no database; it reads only the `server` section of `config.json` to find the service when neither `--url` nor `FORGE_URL` is set: a running service handles every operation with the same actor policy as the dashboard and MCP.

An agent with shell access can define, execute, inspect, and schedule work:

```sh
agent-forge snapshot
printf '%s\n' '{"type":"echo","input":{"text":"scheduled check"}}' |
  agent-forge work register daily-check --definition -
agent-forge work run daily-check --key first-check
# Use the actual run_id returned above; inspect until terminal and verify output.
agent-forge runs inspect RUN_ID
agent-forge schedules create daily-check --work-id daily-check --cron '0 9 * * *' --tz America/Chicago
# Returns the actor's schedule list; the new entry's canonical schedule_name
# is actor-qualified, for example operator.daily-check.
agent-forge schedules list
# After a due time, read each occurrence's admitted run and output.
agent-forge schedules occurrences CANONICAL_SCHEDULE_NAME
# Pause/delete a test schedule when finished.
agent-forge schedules pause CANONICAL_SCHEDULE_NAME
```

Operators get readable output for multi-step workflows and their runs. These commands print text by default; add `--json` for the canonical API result (stable machine contract):

```sh
agent-forge workflows validate --file report.json       # registration checks, nothing stored
agent-forge workflows create report --file report.json
agent-forge workflows show report
agent-forge workflows run report --param task='new evidence' --wait
agent-forge runs search --work report --status ERROR    # newest first; --before CURSOR pages
agent-forge runs show RUN_ID                            # status, failed step + reason, per-step output, next actions
```

`workflows run` prints the request key it used (generated when `--key` is omitted); reuse it with `--key` after a lost response. With `--wait` it exits nonzero unless the run ends `SUCCESS`. The older `work`, `runs list` and `runs inspect` commands keep printing raw JSON.

Commands that return structured results write JSON to stdout; the human-readable commands above support `--json` for scripts. Errors are sanitized on stderr and return a nonzero exit status. `work list` returns the first 100 scoped work identities; `work show ID` reads the full definition. `runs list` covers the latest 100 scoped runs. Run history and default inspection mark large nested values as summaries. Use an exact effect or step selector to retrieve a complete individual record.

Requests and input files are limited to 1 MiB and responses to 4 MiB. Redirects are refused. JSON files contain one value; `-` reads stdin where supported. A timeout may follow acceptance: inspect state and reuse the same request key instead of submitting a new operation.

| Command | Purpose |
| --- | --- |
| `work list` / `work show ID` | List scoped metadata or read one complete immutable definition |
| `work register ID --definition FILE` | Register an immutable definition (`-` reads stdin) |
| `work enable ID` / `work disable ID` | Control subsequent dispatch |
| `work run ID --key KEY [--params FILE]` | Admit work with an explicit stable request key |
| `runs list` / `runs inspect ID` | Read run summaries, final output and bounded step/effect previews |
| `runs inspect ID --effect-id EFFECT` / `runs inspect ID --step-id NUMBER` | Read one exact effect or native step without duplicated history |
| `runs cancel ID` | Request cooperative cancellation |
| `runs retry ID --key KEY` | Request an explicit linked retry of a definite failure |
| `runs reconcile ID --decision completed --note TEXT --settled --output FILE` | Record settled provider evidence and output |
| `runs reconcile ID --decision not-applied --note TEXT --settled` | Record explicit evidence that no external effect occurred |
| `schedules create NAME --work-id ID --cron EXPR --tz IANA [--params FILE]` | Create an explicitly zoned schedule |
| `schedules create ... [--tag T]...` | Tags (lowercase, at most 16, `letters digits _ . - :`) group schedules; filter with `schedules list --tag T` or `schedules status --tag T` |
| `schedules edit NAME [--cron EXPR] [--tz IANA] [--params FILE \| --clear-params] [--enable \| --disable] [--tag T]... [--clear-tags]` | Edit a schedule **in place** (API/MCP `schedule_update`). The creation checks run again; the previous revision and all occurrence history are kept; DBOS's `update_schedule` changes the native schedule while `last_fired_at` and the per-tick admission key stay, so a tick that already fired never fires again and no tick fires twice. Omitted fields are unchanged; an edit that changes nothing is a no-op. The schedule's work is fixed (a different work needs a new name); plugin-owned schedules are managed by their plugin |
| `schedules list` / `schedules pause NAME` / `schedules resume NAME` / `schedules delete NAME` | Inspect or control the canonical schedule name returned by the service. These print the stable JSON schedule list on stdout (unchanged for scripts) and a one-line confirmation on stderr |
| `schedules occurrences NAME [--limit N]` | Read recent due occurrences (newest first, default 10, max 100) with each admitted run's status and output; readable after pause, delete or restart |
| `schedules status [--json]` | Table of owned schedules: status, cron, zone, last fired tick and next due tick |
| `schedules show NAME [--limit N] [--json]` | One schedule: retained immutable intent, native status (`ACTIVE`, `PAUSED` or `DELETED`), catch-up policy, upcoming ticks, outcome counts, recent occurrences linked to runs, and next actions |
| `schedules history NAME [--limit N] [--json]` | Recent due occurrences as a table: native handoff status versus the admitted run's own outcome (`succeeded`, `failed`, `cancelled`, `running`, `not_admitted`, `handoff_pending`) |
| `schedules run NAME [--json]` | Run a saved schedule once now with its frozen work and params. Each call starts a **new** manual run, with a native trigger ID and no request-key idempotency, and it is never retried automatically. It also works on a paused schedule, which stays paused; cron, params and last-fired tick are unchanged |
| `schedules check --cron EXPR --tz IANA [--count N] [--json]` | Validate timing exactly as `create` would and preview due ticks (local and UTC); stores nothing |
| `queues status [--json]` | Every native DBOS queue: stored limits, running/waiting counts, and your running and waiting runs with why each waits |
| `queues show work\|models\|internal [--limit N] [--json]` | One queue: what holds the slots (for example a durable sleep), your waiting runs and reason, and your recent results with measured wait and run time |
| `plugins list` | Discover plugin work types (configured and enabled installed) and input schemas (JSON) |
| `plugins show TYPE\|ID [--json] [--check]` | Cached description/schema and authoring guidance, or an installed plugin's manifest summary (`--check` re-verifies and re-describes) |
| `plugins installed [--json]`, `plugins check DIR`, `plugins install DIR`, `plugins enable ID`, `plugins disable ID`, `plugins uninstall ID` | [Installed plugin lifecycle](plugins.md#lifecycle) (installs start disabled) |
| `plugins run ID COMMAND...` | Run a manifest-declared plugin CLI command through the service; exits with the plugin's status |
| `plugins validate TYPE --input FILE [--json]` | Check input under registration policy without execution |
| `runners list` | Read-only harness runner profiles, observed versions and capability matrix |
| `overview` | Health (service, DBOS, database, runner and plugin availability), upcoming scheduled work, recent failures |
| `agents [--limit N]` | Active and recent harness/ACP runs by runner, adapter, model, status and duration (cancel with `runs cancel`) |
| `runs logs ID [--source audit\|harness:STEP] [--after N] [--limit N]` | Bounded, paginated, redacted logs; without `--source` the index of sources with per-attempt diagnostics, tool calls and ACP permission decisions |
| `schedules stats` | Per-schedule last fired, next due and succeeded/failed/cancelled/running counts over the latest occurrences |
| `approvals list [--state S]` / `approvals approve ID --note TEXT` / `approvals reject ID --note TEXT` | Durable approval requests and final decisions (needs `approve`) |
| `skills list` / `skills show NAME` | Read-only catalog of compiled-in skills |
| `settings show` / `settings set KEY JSON` / `settings unset KEY` | Effective settings with sources (secrets redacted); validated config.json edits (needs `configure`), restart required |
| `providers list` / `providers set-model NAME --definition FILE` / `providers set-runner NAME --definition FILE` / `providers remove-model NAME` / `providers remove-runner NAME` | Model aliases and runner profiles: saved versus running, validated edits (needs `configure`), restart required |
| `backups list` / `backups start [--name N]` | Archives in `paths.backups`, latest job state and the restore instruction; start an online backup (needs `transfer`) |

Reconciliation requires the literal `--settled` flag and operator authority. A cancellation or timeout does not establish that an external operation settled. For flow reconciliation, add `--effect-id ID` from inspection; omit it only for one unambiguous unresolved effect. Flow reconciliation records evidence without resuming terminal work, and flow retry is refused after any external-effect history. The existing `init`, `serve`, `backup`, `restore`, `mcp`, and `skill` commands retain their lifecycle roles.

## MCP and operating skill

Configure a stdio MCP server with command `agent-forge`, arguments `["mcp", "--url", "http://127.0.0.1:8110"]`, and a privately supplied `FORGE_TOKEN`. `--token-env NAME` selects another variable. This Rust process forwards operations to the authenticated API; it opens no database and starts no executor. Mutation tools declare that they write state, so the harness's normal approval rules apply.

Copy `../skills/forge-operations` from this checkout into your harness's supported skills directory, or use `agent-forge skill` to print the embedded `SKILL.md` from an installed binary. `cargo install` installs the binary; it does not modify native harness configuration or install skills into an existing home. Follow the skill's registration, execution, scheduled-run observation, and failure diagnosis workflow. Home and other Soulsteads products are optional.

The API is `POST /api` with bearer authentication and JSON `{"method":"snapshot","params":{}}`. The dashboard, operational CLI and MCP use that same contract. The MCP tools are `forge_overview`, `forge_agents`, `forge_run_logs`, `forge_schedule_stats`, `forge_approvals`, `forge_approve`, `forge_reject`, `forge_skills`, `forge_skill_show`, `forge_settings`, `forge_settings_set`, `forge_providers`, `forge_provider_set`, `forge_provider_remove`, `forge_backups`, `forge_backup_start`, `forge_snapshot`, `forge_work`, `forge_validate`, `forge_runs`, `forge_register`, `forge_enable`, `forge_run`, `forge_inspect`, `forge_schedule`, `forge_schedule_action`, `forge_schedule_occurrences`, `forge_schedule_show`, `forge_schedule_preview`, `forge_schedule_trigger`, `forge_queues`, `forge_cancel`, `forge_retry`, and `forge_reconcile`.

## Work, schedules, and effects

Core work types are `echo`, controlled `fail`, `model`, `workflow`, and `agent`. Workflows compose ordered deterministic actions and reusable named work with typed data references. Agents perform a bounded model/tool loop using explicitly allowed configured plugins. Both run manually or on native schedules; see [flow definitions, parameters and recovery](flows.md). Definitions are immutable: register a new ID when intent changes. Disable work to prevent later dispatch. Reuse a manual request key after a lost response; a new key requests a new operation.

Cron expressions accept **five fields, minutes first**, or **six fields, seconds first**, with an explicit IANA time zone. Five-field input is normalized to a leading zero-second field before native scheduling. DBOS owns the schedule: it fires each due tick on the zone's wall clock (DST-aware) and Forge admits exactly one run per tick under a deterministic identity. Schedules do not automatically backfill missed occurrences: ticks missed while paused or while no service runs are skipped, and resuming fires from the next tick. A schedule's *work* is fixed once created. Its cron, zone, params, enabled state and tags are edited in place with `schedules edit` (see below); re-creating an existing name with different intent is still rejected. To change the work, create a replacement under a new name, then pause or delete the original (the dashboard's *Create replacement* pre-fills this). To run a saved schedule once outside its timing, use *Run once now* on its dashboard page, `schedules run NAME` or MCP `forge_schedule_trigger`. DBOS's native trigger enqueues one firing under a clock-derived `sched-NAME-trigger-TIME` ID, and Forge admits a new manual run for it with the schedule's frozen work and params. History marks it `kind: manual`, separate from `due` ticks. Because each call is a new run, nothing retries it: if a request fails or times out, check the schedule's history for a manual occurrence before running again. Manual and scheduled work share a native DBOS queue with two global execution slots. Scheduled dispatch admits the actual work and releases its slot without waiting on another queued workflow. The dashboard's *Operations* view shows definitions, schedules and their next occurrence, recent due occurrences with each admitted run's status and output, runs, steps, outputs, and failure evidence. Its *Schedules* view lists schedules with last fired and next due ticks, checks timing before creation, creates, pauses, resumes and deletes them, and shows each schedule's occurrence history: the native handoff status separately from the admitted run's own outcome, linked to that run. Its *Workflows* view builds, validates, creates and runs multi-step workflows; its *Runs* view searches runs by work and status and explains each run step by step (state, output, the failing step and its reason) with cancel and retry.

**Queues.** DBOS owns Forge's queues, their limits and the order and time at which queued work starts. Every run, manual, retried or scheduled, executes on the home's *work* queue, which has two global slots shared by all actors. The *models* queue (two slots, one per provider partition) is registered but not used in this release: model calls run as checkpointed steps inside work-queue runs. DBOS also has an internal queue where Forge's submission maintenance runs. The dashboard's *Queues* view, `agent-forge queues status|show` and MCP `forge_queues` show the stored native limits; home-local running and waiting counts; and, for your own work only, what holds each slot (a run in a durable sleep keeps its slot) and why each waiting run waits. A waiting run shows `queue_capacity` when every slot is held, `native_delay` or `rate_limited` when DBOS recorded that hold, and `not_determined` otherwise. Other actors' work is counted but never identified. Forge does not report a queue position or start estimate, because DBOS does not expose one. Counts come from a bounded read of native records: up to 500 nonterminal workflows per queue, oldest first. If the read is incomplete, counts are lower bounds. Free slots and saturation are then `null` (unknown) unless the observed running count already reaches the limit. `members_listing` reports whether your listed running and waiting work is complete, overall and per state. When the native read or the `limit` (1–100, default 50) leaves some out, the views do not claim you have no work there; use Runs to find it. Recent results are a sample: of the newest 100 finished workflows on the work queue (all actors), at most 10 of yours are shown. `recent_window.complete` is false when older results of yours may exist, or when `recent_window.unavailable` of your finished runs could not be read for display; find them in Runs. The dashboard's Waiting list covers waiting and delayed work, and it says none only when both are complete. Cancelling a run before DBOS dequeues it means it never starts. The view does not change limits, priorities or order.

Forge's private SQLite store holds immutable admissions, ownership, reconciliation evidence, and effect receipts. PostgreSQL holds DBOS execution state. DBOS provides durable execution, step checkpoints and recovery, native schedules and queues, and the workflow/step records behind run inspection. Forge adds actor permissions, immutable admission, authored-workflow interpretation, and external-effect receipts and fences; DBOS checkpoints alone do not make external effects exactly-once. DBOS Rust 0.6.0 provides only a PostgreSQL system database, so PostgreSQL is required today; a SQLite default for durable state is a requested future option and is not implemented. Stored model/plugin outputs have integrity-checked receipts. `SUCCESS` means the workflow completed; `model.expect` is only a substring assertion, not a general assessment of task quality.

Cancellation is cooperative and cannot undo an external call. A timeout, crash, or cancelled DBOS status does not prove that a provider stopped. Unresolved effect intents surface as `RECOVERY_REQUIRED` and refuse automatic replay. Reconciliation requires explicit settled provider evidence and appropriate authority; it is refused while the local effect call is active. Completed effects remain recorded and cannot be replayed through retry. A confirmed `not_applied` outcome permits an explicit linked replacement; the original invocation remains fenced. See [recovery and backup](recovery.md).

## Web dashboard

Enter an actor token at the service URL. The dashboard keeps it in tab memory, not in storage, cookies, or the URL. Use the view that matches the task:

| View | What to check or do |
| --- | --- |
| **Overview** | Service health, upcoming work, and links to recent failures |
| **Runs** | Steps, outputs, effects, approvals, and bounded, redacted logs by step and attempt. Harness/ACP runs also expose a live transcript and permission decisions. |
| **Agents**, **Approvals**, **Schedules** | Inspect or cancel harness runs, decide workflow approvals, and check schedule health and run history. |
| **Skills**, **Plugins** | Read the skills catalog, or check, install, enable, disable, and uninstall trusted local plugins. Install and lifecycle refusals are shown in the dashboard. |
| **Providers**, **Settings** | Edit validated configuration and see where each effective setting comes from, with secrets redacted. Restart Forge yourself to apply changes. |
| **Transfer** | Start an online backup and list archives. [Restore](recovery.md#explicit-restore) is CLI-only. |

For a run in `RECOVERY_REQUIRED`, **Runs** offers reconciliation. Record `applied` (API `completed`) or `not_applied` only after independently verifying the outcome. Provide an evidence note and confirm the effect has settled. Reconciliation does not replay the call.

**Live events.** `POST /api/stream` with body `{"run_id":...}` and the `Authorization: Bearer` header returns server-sent events (`state`, `effect`, `approval`, and the harness `agent_message_chunk`, `tool_call`, `tool_call_update`, `permission` events, then `end`), each with a stable id, redacted and bounded. The token is never accepted in a URL. A connection lasts at most ten minutes; the dashboard falls back to polling the paginated `run_logs` if the stream is unavailable.

**Approvals.** Add a workflow step `{"type":"approval","input":{"title":"...","description":"...","timeout_seconds":3600}}` (workflow step only). The step waits durably on a native DBOS message (it holds a work-queue slot, like a durable sleep, so size `dbos.work_concurrency` for the number of simultaneous waits). The `forge_approvals` row is the decision record: one atomic decision by the owning actor with the `approve` capability and a note; a second decision is refused; a restart keeps pending approvals and redelivers a decision whose wake-up was lost. Rejection or timeout fails the step. ACP permission requests are never routed to a human: they are always answered by the deny-by-default policy. Human-in-the-loop ACP permission is deferred.

**Backups.** `paths.backups` (default `<home>-backups`, a sibling of the home; `FORGE_BACKUPS_DIR`) must lie outside the home. An online backup runs in the service: the PostgreSQL dump first, then the home while the admission store is held, so it equals a crash at the dump instant: work in flight restores as recoverable and unknown effects stay unresolved. It requires `pg_dump` matching the server on the service's `PATH`. The offline `backup` command still requires the service stopped.

**Security headers.** Every core response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. The page has no inline script or style, and the scripts render data only with `textContent` and DOM nodes. Plugin dashboard modules run under this policy.

## Approvals and work-queue slots

A workflow `approval` step waits durably (a DBOS message wait) and, while it waits, **holds one work-queue slot**, exactly as a durable sleep does. The work queue defaults to **2** concurrent runs (`dbos.work_concurrency`). Two undecided approvals therefore occupy every slot: further runs wait in the queue (reported as `queue_capacity` in `queues status`), and a queued run whose approval step has not started yet has **no approval record and does not appear in `approvals`** until a slot frees. A waiting approval releases its slot when it is approved, rejected, or reaches its `timeout_seconds` (default and maximum seven days).

Sizing: set `dbos.work_concurrency` (`settings set dbos.work_concurrency 8`, restart) to at least the number of approvals you expect to be undecided at once **plus** the number of ordinary runs you want executing in parallel. Give approval steps a `timeout_seconds` so abandoned requests release their slots. A dedicated approval queue (so waiting approvals never compete with work) is a deferred improvement.

## Doctor

`agent-forge doctor [--json] [--offline]` is **read-only**: it never writes a row or a file, never starts a workflow and never repairs anything. With the service running it asks the service (API/MCP `doctor`, and the **Run doctor** button in the Overview) and checks: config validity (`config.json` on disk versus what the service runs), settings and their sources, PostgreSQL reachability and the DBOS schema, `psql`/`pg_dump`/`pg_restore` on `PATH`, an uncached probe of every runner profile, plugin release integrity (digests and configured executables), validation of **every work definition** against current authority and configuration, every schedule against its admitted definition, **lost runs** (an admission with no native workflow), **orphan workflows** (a native workflow with no admission), runs waiting in `RECOVERY_REQUIRED`, and authority gaps for newer operations. With the service stopped (or `--offline`) it runs the subset that needs only the home and the database (config, settings, database, schema, runners, plugins, lost runs, authority) and reports definitions as skipped. A home whose config does not load fails at the first check and names the field. The exit status is 1 when any check fails; `warn` does not fail.

A home upgraded from before the approval/configure/transfer milestone has operator actors without the `approve`, `configure` and `transfer` capabilities and the `approval` work type. Doctor prints the exact change for each such actor (never a token); the operator merges it into `config.json` while the service is stopped, then restarts:

```json
{"actors":{"operator":{"capabilities":["read","register","execute","schedule","reconcile","plugins","approve","configure","transfer"],"types":["echo","fail","model","agent","workflow","harness","approval"]}}}
```

(Keep the actor's existing `token` and any other capabilities or types it already has; the printed snippet already includes them.) A deliberately narrow actor (for example `read` only) is not flagged. `authority.you` lists what the calling actor itself lacks.

## Runner probes, definitions transfer and the service unit

* `agent-forge runners test NAME` (API/MCP `runner_test`, the **Test** button in Providers) probes one runner profile now, bypassing the 30-second listing cache: the executable, the sandbox launcher (Pi), the harness version, and whether each passed-through environment variable is set (names only). It runs the harness with `--version` only: it sends no prompt and uses no model.
* `agent-forge definitions export [--output FILE]` and `definitions import FILE [--replace] [--dry-run]` (API/MCP `definitions_export`/`definitions_import`, the Transfer view) move your work and schedule definitions as one JSON document (plugin-owned ones are excluded). Import validates **everything first** with your authority (the same checks as `work register` and `schedules create`) and refuses the whole document on any problem, listing each; it is idempotent (re-importing changes nothing); an existing identity with a different definition is a collision, refused unless `--replace`, which replaces work only if it has no unsettled run and edits a schedule in place (a schedule's work is never changed). Replacing work makes old failed runs of it non-retryable (their admitted definition no longer matches).
* `agent-forge service preview|install|uninstall|status [--name NAME] [--env-file FILE] [--start]` (API/MCP `service`, the Settings panel) generates a **systemd `--user` unit** from the resolved settings: the binary that ran the command, `--home`, and the resolved listener host and port. The unit never contains the database credential. When the database comes from `FORGE_DATABASE_URL` or `database.url_env`, `--env-file` must name a private (0600) file that defines that variable; the unit refers to it with `EnvironmentFile=` (names are checked, values are never read out). A literal `database.url` in the private `config.json` needs no env file. An existing unit with different content is never overwritten; uninstall removes only a unit carrying this tool's marker **and** this home. Without `systemd --user`, `preview` still prints the unit text. Start/stop/restart are `systemctl --user`'s job.

## Authority errors and `agent-forge doctor`

A refusal reads `operation outside actor authority: actor NAME lacks capability `X`` or `lacks work type `T``, and points at `agent-forge doctor` for the exact `config.json` change. Capabilities and types are edited only by the operator in `config.json`, followed by a restart; no operation grants authority.

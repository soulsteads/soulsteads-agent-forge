# Soulsteads Agent Forge

Agent Forge is a self-hosted workflow runner for operators and teams that need to schedule work, inspect results, and recover after an interruption. Run and manage the same work through a local web dashboard, a CLI, an authenticated HTTP API, or a stdio MCP server.

- **Author and schedule work.** Compose steps with typed inputs and outputs. Run a workflow on demand or give it a recurring schedule with an explicit time zone.
- **Inspect what happened.** Read step outputs and failures, compare scheduled occurrences with their actual runs, and review the status of external effects before retrying.
- **Extend deliberately.** Deterministic workflows need no model, agent harness, or plugin. Operators can opt into configured models, harnesses, and trusted executable plugins.

Forge is an **early preview**, not a production release. DBOS owns execution, scheduling, queues, and checkpoints. PostgreSQL is required; the separate SQLite admission and receipt store is not a replacement. The service listens on loopback only.

## Quick start

You need **Linux**, **Rust 1.97 or later**, a **C compiler**, and an **empty, dedicated PostgreSQL database**. The example uses `jq` to read a token without printing it. See [requirements and verification](docs/support.md) for the tested toolchain and known limits.

From this source directory, install and initialize a new local Forge home:

```sh
cargo install --locked --path . -j 1
export FORGE_HOME="$HOME/.local/share/agent-forge-example"
agent-forge init
```

Set `FORGE_DATABASE_URL` privately to your dedicated database's connection URL, then run:

```sh
agent-forge serve --port 8110
```

Open <http://127.0.0.1:8110> and sign in with the operator token at `actors.operator.token` in the Forge home's `config.json`. Keep that file private. For CLI access, open another terminal and run:

```sh
export FORGE_HOME="$HOME/.local/share/agent-forge-example"
export FORGE_TOKEN="$(jq -r .actors.operator.token "$FORGE_HOME/config.json")"
```

Save the [deterministic workflow example](docs/flows.md#deterministic-data-flow) as `report.json`. Validate it, register it, and run it:

```sh
agent-forge workflows validate --file report.json
agent-forge workflows create report --file report.json
agent-forge workflows run report --key first-report --param task='example checklist ready' --wait
```

The final output should contain `Observed: example checklist ready`. Inspect the run in the dashboard or use `agent-forge runs show RUN_ID` with the ID returned by the CLI. If the response is lost, reuse the same request key and parameters rather than submitting a new run.

To run the same workflow every day at 09:00 UTC:

```sh
agent-forge schedules create daily-report --work-id report --cron '0 9 * * *' --tz UTC
agent-forge schedules occurrences operator.daily-report
```

An occurrence appears after the schedule is due; inspect its linked run and output to verify completion. Pause the example with `agent-forge schedules pause operator.daily-report`. Stop the foreground server with Ctrl-C. Restart with the same home and database to keep your work and history.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Operations](docs/operations.md) | Configuration, actor permissions, dashboard, CLI, HTTP and MCP |
| [Workflows](docs/flows.md) | Step definitions, parameters, output, scheduling and failure states |
| [Backup and recovery](docs/recovery.md) | Backup, restore, recovery-required runs and upgrade limits |
| [Harnesses](docs/harness.md) and [plugins](docs/plugins.md) | Optional integrations and their trust boundaries |
| [Agent operating skill](skills/forge-operations/SKILL.md) | Register, schedule and inspect permitted work through CLI or MCP |
| [Requirements and verification](docs/support.md) | Supported setup, preview limits and local checks |

Forge cannot undo an external action. When the outcome of a call is unknown, verify it independently before reconciling the run. Do not give untrusted executables access through Forge.

Licensed under [Apache-2.0](LICENSE). See [third-party notices](THIRD_PARTY_NOTICES.md) for bundled notices.

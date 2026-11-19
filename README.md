# Soulsteads Agent Forge

**Run recurring workflows on your own machine, and see exactly what happened.**

Agent Forge is a self-hosted workflow runner for operators and teams. Define a job, run it on demand or on a schedule, and inspect its steps, outputs, and failures. Use the local dashboard or CLI, or integrate through the authenticated HTTP API or stdio MCP server.

[Quick start](#quick-start) · [Workflow guide](docs/flows.md) · [Operations](docs/operations.md) · [Backup and recovery](docs/recovery.md)

**Early preview.** Forge targets Linux, requires PostgreSQL, and listens on loopback only. The example below needs no model, provider key, harness, or plugin.

## Quick start

You need **Rust 1.97 or later**, a **C compiler**, an **empty, dedicated PostgreSQL database**, and `jq` for the token command below. See the [tested setup](docs/support.md#tested-preview-path) for what has been exercised.

### 1. Install and start Forge

From the root of this checkout, with the Cargo bin directory on your `PATH`:

```sh
cargo install --locked --path . -j 1
export FORGE_HOME="$HOME/.local/share/agent-forge-example"
agent-forge init
```

Set `FORGE_DATABASE_URL` privately to the connection URL for your dedicated database. Then start Forge in the foreground:

```sh
agent-forge serve --port 8110
```

Open <http://127.0.0.1:8110> and sign in with the operator token at `actors.operator.token` in `$FORGE_HOME/config.json`. Keep that file private. To use the CLI, open a second terminal and load the same token without printing it:

```sh
export FORGE_HOME="$HOME/.local/share/agent-forge-example"
FORGE_TOKEN=$(jq -er '.actors.operator.token' "$FORGE_HOME/config.json") && export FORGE_TOKEN
```

### 2. Run a two-step workflow

Save this definition as `report.json`. The first step echoes a parameter; the second reads that step's output and returns a report.

```json
{
  "type": "workflow",
  "input": {
    "params": {"task": "example checklist ready"},
    "steps": [
      {"id": "produce", "action": {"type": "echo", "input": {"text": {"$ref": "/params/task"}}}},
      {"id": "consume", "needs": ["produce"], "action": {"type": "echo", "input": {"text": "Observed: ${/steps/produce/output/text}"}}}
    ],
    "output": {"report": {"$ref": "/steps/consume/output/text"}}
  }
}
```

Validate, register, and run it:

```sh
agent-forge workflows validate --file report.json
agent-forge workflows create report --file report.json
agent-forge workflows run report --key first-report --wait
```

A successful run returns `Observed: example checklist ready`. Open **Runs** in the dashboard, or run `agent-forge runs show RUN_ID` with the ID returned by the CLI. If you lose the submission response, repeat the same command with the same request key and parameters instead of using a new key.

### 3. Put it on a schedule

This schedule runs `report` every day at 09:00 UTC:

```sh
agent-forge schedules create daily-report --work-id report --cron '0 9 * * *' --tz UTC
agent-forge schedules occurrences operator.daily-report
```

The history can be empty until the first due time. After that, follow the occurrence to its run and check the output; creating a schedule does not show that its work succeeded. Pause the example with `agent-forge schedules pause operator.daily-report`. Stop the foreground server with Ctrl-C; restart it with the same home and database to keep the definitions and history.

## How Forge works

- **Execution and schedules:** DBOS uses PostgreSQL for execution, checkpoints, schedules, and queues. A separate SQLite store in the Forge home records admissions and effect receipts; it does not replace PostgreSQL.
- **Permissions and extensions:** The dashboard, CLI, API, and MCP use the same actor policy. Models, native agent harnesses, and executable plugins are optional and must be configured explicitly. Only install plugins you trust: they run with the service user's permissions.
- **Recovery:** A checkpoint is not proof that an external action happened exactly once. If a run stops with an uncertain effect, verify what happened at the provider before reconciling it. Forge cannot undo an external action.

## Documentation

| I want to… | Start here |
| --- | --- |
| Author workflows, pass parameters, or inspect results | [Workflow guide](docs/flows.md) |
| Configure the service, actor permissions, CLI, API, or MCP | [Operations guide](docs/operations.md) |
| Recover from an interruption, back up a home, or restore it | [Backup and recovery](docs/recovery.md) |
| Connect an agent harness or extend Forge | [Harnesses](docs/harness.md) · [Plugins](docs/plugins.md) · [Agent operating skill](skills/forge-operations/SKILL.md) |
| Check requirements, validate a build, or export source | [Requirements and verification](docs/support.md) · [Source snapshots](docs/source-release.md) |

Licensed under [Apache-2.0](LICENSE). See [third-party notices](THIRD_PARTY_NOTICES.md) for bundled notices.

# Recovery and backup

The private application home and its dedicated PostgreSQL database form one recovery unit. Keep the original executor stopped during backup and restore. Never run a restored copy alongside the original: they retain the same workflow, schedule, and external-effect identities.

## Offline backup

Leave PostgreSQL running and stop Forge. Point Forge at the source database (`FORGE_DATABASE_URL`, `--database-url`, or the home's `database` configuration; see [Configuration](operations.md#configuration)), then run:

```sh
agent-forge --home "$FORGE_HOME" backup ./forge-backup
```

The destination must not exist and must be outside the source home. Its parent must exist. The command holds the same exclusive home lock used by the service, validates the source's sealed database identity and SQLite admission store, copies the private home, and saves a custom-format dump of its exact DBOS schema. A manifest records file digests. Configuration, prompts, outputs, receipts, and credentials in the home remain private operational data.

Harness workspace regular files are copied with private permissions, preserving bytes but not original modes. Workspace symlinks, special files, and entries that cannot be opened are skipped rather than followed. The known native `result.txt` output is also normalized. Other state must satisfy strict private-file checks. Compare required artifacts and invocation journals against a retained inventory; a successful backup does not promise to include every filesystem entry. Files outside Forge home, including an optional Home controller's configuration, journal and receipt, need their own recovery procedure.

The PostgreSQL client tools on `PATH` must match the server's major version. Connection fields come from the explicit database URL, with inherited `PG*` settings removed. Passwords are passed through the child environment rather than command arguments. Backup supports standard libpq URL options; native-driver-specific aliases must be replaced with their standard equivalents.

A failed backup retains an `INCOMPLETE` marker and is not accepted for restoration. Do not remove that marker to manufacture a complete backup.

## Explicit restore

Provide a new, nonexistent home path and an empty dedicated database on the same PostgreSQL major version. Resolve the destination database the same way (for example `FORGE_DATABASE_URL`) before running:

```sh
agent-forge --home ./restored-home restore ./forge-backup
```

The archive keeps the home's own layout. Backup requires `state`, `outputs`, `effect_locks`, `harness`, `plugins`, `dbos_identity` and `dbos_admission` to lie inside the home (a relocated `paths` entry that is absolute and outside it is refused rather than half-saved; `logs` and anything else outside the home need their own recovery procedure (the backup command and `backup_start` name an outside `logs` or `cache` in a `note:` / `notes` field instead of skipping it silently), and `cache` and the unfinished-install `plugins/staging` directory are never archived). Installed plugin releases travel with the home and their registry rows with the admission store: backup refuses to archive a release whose bytes no longer match its manifest digests, and restore verifies every release against its own manifest and restores its read-only modes before anything can run. Restore lays the new home out from the archived `config.json` `paths` (relative values resolve against the new home) and takes the database from the flag, environment or the archived `database` section. A literal archived `database.url` points at the source database, so supply the destination with `--database-url` or `FORGE_DATABASE_URL`, or prefer `database.url_env` so the same configuration works for both. Both commands use exactly the database URL that `serve` would.

Restore verifies the manifest, validates SQLite identity and integrity, copies and verifies the home, and stages a verified private database dump. PostgreSQL restoration uses one transaction. The destination home is sealed to its exact new path and database before the completion marker is cleared. The archive is preserved.

Restore does not start work. Inspect actor authority and model/plugin configuration before running `serve`. Starting the restored service can resume pending DBOS work and future scheduled occurrences. A wrong database or copied home path is refused by the identity seal.

For a recovery qualification that must not dispatch new work, pause every user schedule through the public service before stopping the source. Confirm the archived/restored schedules remain paused and inspect pending work before starting the restored executor. Forge's internal submission-maintenance schedule is separate from user schedules and must remain active; it reconciles admission metadata. Restore does not automatically pause schedules or cancel pending work. Keep the source stopped until the restored executor and its children have closed.

An interrupted restore retains `.restore-incomplete`; initialization and service startup refuse that home. Preserve it for diagnosis, and repeat restoration into another fresh home/database from the verified archive. Do not remove the marker or identity seal to bypass recovery checks. Qualify a real restored run before relying on a backup.

## Failed or uncertain work

After process loss, native DBOS resumes checkpoints. Forge reconciles admissions using the original workflow IDs, avoiding a second execution engine. An external intent without a verified receipt is uncertain and cannot be resent automatically. Cancellation, timeout, and local process death do not establish provider quiescence.

Inspect the run and provider independently. Once the operation has settled, an authorized operator can reconcile through the dashboard or `forge_reconcile` with a decision, evidence note, and explicit `settled: true`:

- `completed` records the verified result and permanently prohibits a fresh-effect retry.
- `not_applied` records evidence that the action did not occur and permits an explicit linked retry with its own key.

Neither decision reopens the original invocation for dispatch. A locally active effect lock blocks reconciliation. Preserve receipts and history; do not edit databases, resume/fork workflows directly, or evade uncertainty by submitting a new manual key.

## Verify a restored home

Before resuming schedules, compare the restored definitions, schedule state, run IDs, checkpoints, outputs and effect receipts against the backup inventory. Use the CLI, dashboard or MCP under the original actor policy. A restored `RECOVERY_REQUIRED` attempt still needs independent evidence; a missing output does not prove a call did not happen. Do not submit new work or change a request key to test whether a past effect happened.

## Supported upgrades

Stop the source service and its owned children after pausing user schedules and
inspecting pending work and uncertain effects. Take an offline backup with the
old installed binary. Preserve that archive and inventory. For a compatible
schema, install the reviewed new binary and restart the same stopped home against
its existing database. To qualify on an isolated copy first, restore the backup
into a new home and empty database using the public `restore` command, inspect
policy, stop the old binary, and start the candidate against that copy. A restored
copy has a new binding; never copy or edit identity files to bypass it. Use matching
PostgreSQL major/client tools. Compare definitions, schedule intent, run IDs,
checkpoints, admissions and effect receipts; replay only a retained completed
request key after verifying effects. A new request key is new work.

The current admission database uses application ID `0x46444231`; the backup and
invocation envelope formats are version 1. There is no general schema-version
conversion command. Startup validates identity and creates absent Forge extension
tables with `CREATE TABLE IF NOT EXISTS`; DBOS owns its PostgreSQL initialization.
An older binary's home with the same schema qualifies as a **compatible-home
restart**, not a schema migration. Unsupported formats, identity mismatches and
incomplete backups must not be hand-edited to pass validation. PostgreSQL-to-SQLite
conversion is not supported. Preserve the original home/backup unchanged and keep
its executor stopped while inspecting the restored copy.

## Diagnosing with doctor

Before and after any recovery, `agent-forge doctor` (read-only; see [operations](operations.md#doctor)) reports lost runs (an admission without a native workflow), orphan native workflows, runs in `RECOVERY_REQUIRED`, definitions that no longer validate and broken plugin releases, with the exact next command. It works with the service stopped (`--offline`) for the database and home checks.

## Online backup consistency and lost runs

An online backup (`backups start`, MCP `forge_backup_start`) takes one consistent cut: the admission store's single writer is held across **both** the PostgreSQL dump and the copy of the home, so no admission, submission confirmation, receipt or audit write commits in between. New admissions therefore pause for the duration of the dump (typically seconds; bounded at 300 seconds, after which the backup is aborted and admissions resume). A submission acknowledged by PostgreSQL after the dump began is recorded in the copied store as not yet confirmed, and the submission bridge confirms it idempotently after a restore.

As a safety net, restore and every service start compare admissions with native workflows. An admission recorded as submitted whose workflow does not exist (a backup from an older build, a database restored from a different point, manual damage) is marked and reported as `RECOVERY_REQUIRED` with a reachable reconcile action; it is never replayed. Reconcile it with evidence: `runs reconcile ID --decision completed --settled --note ... [--output FILE]` settles it as `SUCCESS`; `not_applied` settles it as `ERROR`, which permits an explicit linked `runs retry`. Cancel is refused for such a run (there is nothing to cancel). Native workflows with no Forge admission are reported (audit action `orphan_native_workflow` and a startup line) and are not run on Forge's behalf.

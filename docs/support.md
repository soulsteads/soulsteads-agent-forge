# Requirements and verification

## Requirements

| What you are doing | What you need |
| --- | --- |
| Build `agent-forge` | Linux, Rust 1.97 or later, and a C compiler |
| Start a new home | An empty, dedicated PostgreSQL database for DBOS and a writable private home |
| Back up or restore | `psql`, `pg_dump`, and `pg_restore` on `PATH`, matching the server's major version |
| Run the documentation and source checks | Git and Python 3 |

The Forge home's SQLite file stores local admissions and effect records; it is not a PostgreSQL alternative. See [configuration](operations.md#configuration) for database and home settings.

## Tested preview path

The exercised setup is Linux x86-64 with Rust/Cargo 1.100.0 and PostgreSQL 18. The no-model smoke builds and installs Forge, registers a deterministic two-step workflow, completes manual and due runs, compares CLI and MCP results, and checks that restart preserves history without duplicating those runs. Chromium is the browser used for optional dashboard readback. The declared Rust minimum, other platforms, and other browser families are not implied by those checks.

The preview is not a production support commitment. Qualify your optional [harness](harness.md), [plugin](plugins.md), and model combinations before relying on them. Plugin executables are trusted code, and native harness permission modes are not a general sandbox. Do not assume an existing PostgreSQL home can be migrated: see [supported upgrades](recovery.md#supported-upgrades).

## Verification

From the root of a committed source checkout:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 scripts/check_public_source.py --revision HEAD --scope public
python3 scripts/check_notices.py --revision HEAD
```

Set `TMPDIR` to a writable directory outside the source tree before running the Python tests. Git objects supply the paths, links and third-party notice checks. [Source snapshots](source-release.md) describes the export format. Tooling and tests are not part of the installed runtime.

## Installed smoke test

Use an empty, dedicated PostgreSQL database. Set `FORGE_DATABASE_URL` privately. Set `FORGE_BINARY` to the absolute path of the newly installed binary and `FORGE_TEST_STATE` to a nonexistent directory outside the checkout. Then run:

```sh
python3 scripts/smoke_installed.py
```

The driver uses only the Python standard library by default. Set `FORGE_BROWSER=1` to add dashboard checks; this also requires Python Playwright, `CHROMIUM_EXECUTABLE`, and a short external `TMPDIR`. The CI workflow defines an equivalent no-model smoke with PostgreSQL 18. Local results do not establish hosted CI results.

## Optional native harness check

The installed smoke test above uses no model. To verify a native harness on a
separate, disposable Forge installation, use
[`scripts/qualify_harness.py`](../scripts/qualify_harness.py). It runs both a
manual workflow and a scheduled occurrence with the selected workspace-write
profile. It checks structured output, the written artifact, and dependent-step
output. This creates work and a schedule on the supplied server; do not point
it at an existing production installation.

The script's docstring lists the required environment variables. Supply the
operator token through the private environment and use an absent directory for
`FORGE_TEST_STATE`. The script needs a configured, authorized harness and model;
it does not set them up. Keep the results private. A pass applies to that
profile, binary, and model, and does not establish crash recovery or containment.

See [source provenance](reference.md) and the [third-party notices](../THIRD_PARTY_NOTICES.md) for source and dependency licensing.

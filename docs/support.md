# Requirements and verification

## Requirements

The preview targets Linux. Building the `agent-forge` binary requires Rust 1.97 or later and a C compiler. Use a dedicated PostgreSQL database for DBOS. The SQLite file in the Forge home holds local admission and effect records; it does not provide a standalone database option. Backup and restore also require PostgreSQL client utilities matching the server's major version. See [configuration](operations.md#configuration).

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

See [source provenance](reference.md) and the [third-party notices](../THIRD_PARTY_NOTICES.md) for source and dependency licensing.

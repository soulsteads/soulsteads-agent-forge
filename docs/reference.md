# Native Rust reference and scope

This MVP uses the Soulsteads Agent Forge Rust reference at commit `2becad67206cf50b595a4f47b83a73d1f3c0ce2e`. The selected Apache-2.0 source is adapted under this repository's Apache-2.0 license.

The extraction comprises the native DBOS runtime/configuration, admission/submission metadata, maintenance bridge, and effect-receipt boundary in `src/dbos/`. Local filesystem ownership helpers were adapted for this standalone home. The runtime was integrated with Forge work definitions, native schedules, authenticated HTTP, native MCP, executable extensions, and coherent backup/restore.

The reference's legacy workflow controller, scheduler/reducer, bundled plugin packages, memory package, and worker engine are not part of this product. DBOS Rust 0.6.0 is the sole execution and scheduling owner. SQLite admission and receipt records do not constitute a parallel execution lifecycle.

The application builds as the `agent-forge` Rust executable. Test clients and fixtures are separate from the installed runtime.

The upstream native DBOS SDK is designated pre-production. This repository's bounded MVP qualification is evidence for the exercised scenarios and explicitly configured backends, not a claim of general production readiness or qualification of the entire reference implementation.

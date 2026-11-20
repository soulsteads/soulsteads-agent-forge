# Changelog

## 0.1.1 (proposed)

No previous GitHub tag. These notes cover the full public source history.

### Highlights

- First tagged-source candidate for Agent Forge: the Rust workflow and scheduling runtime, operator dashboard, and CLI/MCP interfaces.
- Include the expanded runtime and dashboard, scoped workflow credentials, and Workflow Studio for authoring and importing runnable workflows.
- Ship the release-readiness and Rust PR-review examples. Improve the Rust CI test service and consolidate dependency license information.
- Prevent overlapping workflow toggles, ignore superseded details, and clear stale loading states after failed refreshes.
- Standardize `LICENSE` and `NOTICE` with author Scott Glover and `Copyright (c) 2026 Soulsteads`. The Apache-2.0 terms are unchanged.

### Included commits

All commits already on the public default branch in this release range, before this version and attribution update. Unmerged branches are not included.

- [`cbe32ca`](https://github.com/soulsteads/soulsteads-agent-forge/commit/cbe32ca5c7caa5563ca0aa1c15a75b68de9b2c6c) Publish Soulsteads Agent Forge source
- [`5aa6cd0`](https://github.com/soulsteads/soulsteads-agent-forge/commit/5aa6cd0b7df5fe32ccb42b80c7ba696f2e210090) Update Agent Forge runtime and operator dashboard
- [`8698f80`](https://github.com/soulsteads/soulsteads-agent-forge/commit/8698f80dcc74762b4a3d92b199424f3935acae12) Add scoped workflow credentials and a dashboard preview
- [`a179189`](https://github.com/soulsteads/soulsteads-agent-forge/commit/a179189b85fce8310c342d654327be85ac60cd00) docs: refresh the Agent Forge README
- [`08615da`](https://github.com/soulsteads/soulsteads-agent-forge/commit/08615daf22bc18219d3c8a158354d8ff028d3e30) chore(release): prepare Agent Forge v0.1.0
- [`3d3a636`](https://github.com/soulsteads/soulsteads-agent-forge/commit/3d3a636856a3ab6d130778cbdcf6afcdf61f0cd3) ci: delegate cgroups to the Rust test service
- [`18a7d75`](https://github.com/soulsteads/soulsteads-agent-forge/commit/18a7d757f6cdb81203dec112c967ecaaa2d4ee78) docs: list third-party dependency licenses in one file
- [`835f1a4`](https://github.com/soulsteads/soulsteads-agent-forge/commit/835f1a4a04dd8185b82205438b0125b5354d94aa) feat(dashboard): add Workflow Studio and importable examples
- [`bb30267`](https://github.com/soulsteads/soulsteads-agent-forge/commit/bb30267b3a919c274820aade833daae992a57c52) fix(package): ship the workflow examples
- [`d18210e`](https://github.com/soulsteads/soulsteads-agent-forge/commit/d18210e78a95e94c4803477a66651216ba7cc4ec) fix(dashboard): retire stale workflow controls and surface reload failures
- [`ef28f22`](https://github.com/soulsteads/soulsteads-agent-forge/commit/ef28f2247d6f8cf83df4ababa2a57e456b35d8ba) fix(dashboard): serialize workflow toggles and clear failed loading states

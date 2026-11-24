# Changelog

## 0.2.0 (proposed)

First tagged-release candidate: the earlier `v0.1.1` draft was not tagged. The list below covers the full public `main` history before this release preparation.

### Highlights

- Rename the executable to `soulsteads-forge`, including the container entrypoint and the binary in GitHub Release archives. Existing Forge homes and database settings are unchanged.
- Preview and install a Linux systemd system unit under an explicit non-root account. The existing per-user service workflow remains available; the system command installs a unit without enabling or starting it.
- Include the native Rust/DBOS workflow runtime, dashboard, managed workflow credentials, Workflow Studio, and importable examples. Add Streamable HTTP MCP with optional TLS and Docker/Podman deployment.
- Preserve scanner child exit status. Build the Linux release asset with the required dependency notices and verify its checksum before attaching it to a published GitHub Release.

### Included commits

- [`b83da83`](https://github.com/soulsteads/soulsteads-agent-forge/commit/b83da83fbc8e0032cde9fd200c813590b79aa3db) fix(ci): verify the Forge release asset handoff
- [`5f37f70`](https://github.com/soulsteads/soulsteads-agent-forge/commit/5f37f701443679264cbcecfc0a533f0893002095) ci: build Forge release assets with dependency notices
- [`1f085d1`](https://github.com/soulsteads/soulsteads-agent-forge/commit/1f085d1e74db3489d6ef71db114efbf22fa76082) fix: preserve Forge child exit status during scan cleanup
- [`4d9b00e`](https://github.com/soulsteads/soulsteads-agent-forge/commit/4d9b00e67e2197be8754865959de1833e6eeae46) feat: add Docker and Podman deployment
- [`d786a02`](https://github.com/soulsteads/soulsteads-agent-forge/commit/d786a02a851e95f2e6f922d35f81d2b971545081) test: use a boolean assertion for MCP success
- [`223dc95`](https://github.com/soulsteads/soulsteads-agent-forge/commit/223dc955f9d6a1ec8a6b08d0e005cf3300570944) fix(ci): classify the Forge TLS source
- [`24382a7`](https://github.com/soulsteads/soulsteads-agent-forge/commit/24382a7f6b8e746fa35c77f7baae26c7d129cfb1) feat: serve Streamable HTTP MCP with optional TLS
- [`9adcc2d`](https://github.com/soulsteads/soulsteads-agent-forge/commit/9adcc2d7b67b39f6826c251eb7a25ac68da3bc0f) feat: add pinned release-readiness report example
- [`9bcc307`](https://github.com/soulsteads/soulsteads-agent-forge/commit/9bcc307946730ac5c00c1a5552790504ba2fea4a) chore(release): prepare v0.1.1
- [`ef28f22`](https://github.com/soulsteads/soulsteads-agent-forge/commit/ef28f2247d6f8cf83df4ababa2a57e456b35d8ba) fix(dashboard): serialize workflow toggles and clear failed loading states
- [`d18210e`](https://github.com/soulsteads/soulsteads-agent-forge/commit/d18210e78a95e94c4803477a66651216ba7cc4ec) fix(dashboard): retire stale workflow controls and surface reload failures
- [`bb30267`](https://github.com/soulsteads/soulsteads-agent-forge/commit/bb30267b3a919c274820aade833daae992a57c52) fix(package): ship the workflow examples
- [`835f1a4`](https://github.com/soulsteads/soulsteads-agent-forge/commit/835f1a4a04dd8185b82205438b0125b5354d94aa) feat(dashboard): add Workflow Studio and importable examples
- [`18a7d75`](https://github.com/soulsteads/soulsteads-agent-forge/commit/18a7d757f6cdb81203dec112c967ecaaa2d4ee78) docs: list third-party dependency licenses in one file
- [`3d3a636`](https://github.com/soulsteads/soulsteads-agent-forge/commit/3d3a636856a3ab6d130778cbdcf6afcdf61f0cd3) ci: delegate cgroups to the Rust test service
- [`08615da`](https://github.com/soulsteads/soulsteads-agent-forge/commit/08615daf22bc18219d3c8a158354d8ff028d3e30) chore(release): prepare Agent Forge v0.1.0
- [`a179189`](https://github.com/soulsteads/soulsteads-agent-forge/commit/a179189b85fce8310c342d654327be85ac60cd00) docs: refresh the Agent Forge README
- [`8698f80`](https://github.com/soulsteads/soulsteads-agent-forge/commit/8698f80dcc74762b4a3d92b199424f3935acae12) Add scoped workflow credentials and a dashboard preview
- [`5aa6cd0`](https://github.com/soulsteads/soulsteads-agent-forge/commit/5aa6cd0b7df5fe32ccb42b80c7ba696f2e210090) Update Agent Forge runtime and operator dashboard
- [`cbe32ca`](https://github.com/soulsteads/soulsteads-agent-forge/commit/cbe32ca5c7caa5563ca0aa1c15a75b68de9b2c6c) Publish Soulsteads Agent Forge source

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

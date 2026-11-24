# Plugins

Forge supports two kinds of executable plugin. Both speak JSON over stdio and run under the service's operating-system user. The [environment allowlist](#process-environment) limits inherited variables; it does not sandbox the executable. Install only code you trust.

| Use | Choose |
| --- | --- |
| Add one `plugin.NAME` work type | A [configured executable](#configured-executables-protocol-1) in `config.json` (protocol 1) |
| Ship jobs, workflows, CLI commands, HTTP routes, MCP tools, or dashboard pages | An [installed plugin](#lifecycle) with a `manifest.json` (protocol 2) |

Core ships the extension points and no bundled plugin. The only plugin in the source tree is a **test-only fixture** (`tests/fixtures/demo-plugin`, not part of the published crate) with one contribution of each kind. DBOS remains the only workflow runner: plugin jobs and workflows are materialized into the ordinary work store and native schedules and run by the existing `forge_execute_v1` / `forge_scheduled_v1` workflows; plugin calls that run inside workflows go through the same checkpointed step, effect key and reconciliation rules as every other external effect.

## Manifest reference

Put `manifest.json` at the plugin directory root. Parsing is strict. The annotated outline below shows the field layout; comments and placeholders are not valid JSON. The source tree's `tests/fixtures/demo-plugin` directory contains a complete manifest and resources.

```jsonc
{
  "id": "demo-fixture",              // ^[a-z][a-z0-9]*(-[a-z0-9]+)*$, at most 48 characters
  "version": "1.0.0",                // semantic version
  "api_version": "v2",               // stdio protocol 2 is the only installable version
  "description": "...",              // 1-2048 characters
  "capabilities": ["fixture"],       // free-form unique strings
  "requires": [], "conflicts": [],   // plugin ids; no self reference, no overlap
  "runtime": {"executable": "script:main", "timeout_seconds": 20},   // 1-300
  "contributions": [ {"kind": "...", "id": "...", "metadata": {...}} ],
  "resource_declarations": [
    {"kind": "script", "id": "main", "path": "bin/demo-plugin", "digest": "sha256:<64 hex>"}
  ]
}
```

* **Resources** are the only files a plugin may ship besides `manifest.json`. Kinds: `script` (the stdio executable), `job`, `workflow`, `asset`. Each has a unique `kind:id` identity, a safe relative `path` (no `..`, no absolute path, no backslash) and a `sha256:` digest of its exact bytes. An undeclared file, a declared-but-unused resource or a missing file refuses the install.
* **Forbidden metadata keys** (any depth, case-insensitive): `import`, `import_target`, `module`, `entrypoint`, `entry_point`, `package`, `hook`, `callable`, `secret`. Metadata is declarative data, never an import target or hook.
* **Contribution kinds** (unknown kinds are rejected): `job`, `workflow`, `cli`, `api`, `mcp`, `dashboard`, `assets`. Contribution ids are lowercase `[a-z][a-z0-9]*([._-][a-z0-9]+)*` (stricter than the reference: no `/`, so they are safe inside work ids).

| Kind | Contribution | Metadata |
| --- | --- | --- |
| `job`, `workflow` | needs a resource `job:ID` / `workflow:ID`, a JSON file `{"description","work":{"type","input"},"schedule":{"cron","timezone"},"enabled"}` (strict; `retry`, `timeout` and other forge-reference job fields are not supported because DBOS owns retry and recovery). `enabled` defaults to false | optional `description` |
| `cli` | a command | `command_path` (1-4 lowercase names), `description`, optional `disabled_denial {status (1-255, default 3), message}` returned when the plugin is installed but disabled |
| `api` | HTTP routes under `/api/plugins/<id>/` | `routes:[{method GET\|POST\|PUT\|PATCH\|DELETE, path "/x/y" (lowercase literal segments), capability "read"\|"execute" (default execute)}]`, `max_body_bytes` (default 65536, at most 1 MiB) |
| `mcp` | one tool | `tool_name` (`lower_snake`), `description`, `input_schema` (object JSON Schema), `read_only` |
| `dashboard` | pages | `routes:[{path, label (at most 40 chars), entry "asset:ID" (ES module), stylesheet "asset:ID" (optional)}]` |
| `assets` | the servable static resources | `resources:["asset:ID", ...]`; a dashboard entry or stylesheet must be exposed here |

Public names are namespaced so plugins cannot collide with core or each other: work ids `plugin.<id>.<contribution>`, work type `plugin.<id>`, MCP tools `plugin_<id>__<tool>`, dashboard route `#/x/<id>/<path>`, API `/api/plugins/<id>/<route>`, CLI `plugins run <id> <command>`. **Ownership collisions** are checked on these namespaced identities (and against configured `plugin.NAME` types and existing work ids): a duplicate contribution inside a manifest, a configured plugin of the same type name, or pre-existing work in the plugin's namespace refuses the install. (forge-rust's `plugin-package/src/registry.rs` made every `(kind, contribution id)` globally unique across plugins; with namespaced surfaces that would only forbid two plugins from both having, say, a job called `daily`, so Forge checks the identities that can actually collide.)

## Lifecycle

```text
soulsteads-forge plugins check DIR         validate without installing (dry run)
soulsteads-forge plugins install DIR       stage, verify, publish; installed DISABLED
soulsteads-forge plugins enable ID         verify again, describe, materialize jobs/workflows
soulsteads-forge plugins disable ID        refused with active runs or in-flight calls
soulsteads-forge plugins uninstall ID      refused while enabled or referenced
soulsteads-forge plugins installed [--json]   lifecycle state; plugins show ID [--check]
soulsteads-forge plugins run ID COMMAND... run a declared CLI command through the service
```

All lifecycle commands go through the authenticated service API (`plugin_*` operations, token auth and audit) and need the new actor capability **`plugins`** (new homes grant it to `operator`; existing homes add it to the actor's `capabilities` while the service is stopped). Running a plugin's work additionally needs the usual `execute` capability **and** the plugin's work type `plugin.<id>` in the actor's `types`; Forge never grants a plugin's type implicitly.

**Install.** The source directory is copied into a private staging directory under `paths.plugins`, then the *staged bytes* are verified: every declared digest, nothing undeclared, regular files only (symlinks refused), size and count limits, and the runtime script must have the owner execute bit. The tree is then made read-only and atomically renamed to `releases/<id>/<version>`, and only then is the registry row written (`forge_plugins` in the admission store, the same SQLite file that holds work and audit, so one transaction decides visibility). A crash can therefore only leave (a) a staging directory, removed at startup, or (b) a release directory with no registry row, which is invisible, cannot be enabled, is replaced by a retry and is removed at startup. Install registry checks: id uniqueness, dependency cycles through `requires`, namespaced ownership collisions.

**Install-time validation and dry run.** `plugins check` and install validate every job and workflow resource under the installing actor's authority, not only at enable: core-typed work gets the complete registration checks, flows the closed structural validation, and a definition that runs a plugin type (not describable until enabled) is validated structurally and completely at enable. The dry-run result lists `work` (each resource's id, kind, type, whether it is scheduled), `work_types` (e.g. `harness`, `plugin.<id>`), `authority_gaps` (types the actor may not register and execute) and a `hints` line naming the `types` entry to add. Enable refuses with the exact missing type. A denied lifecycle attempt (install, enable, disable, uninstall) is audited as `plugin_denied` with the actor, operation and target. For plugin API and CLI calls, authority is checked before the plugin's enabled state is revealed (403 before 503).

**Release integrity.** Every verification (install, startup, enable, `plugins show --check`, backup and restore) checks the release's `manifest.json` *bytes* against the `manifest_sha256` the registry recorded at install, then every declared resource digest, then that nothing undeclared is present. The manifest declares the other digests, so an edited manifest is refused even when the files it describes still match. Backup and restore read the recorded digests from the admission store; a release directory with no registry record is refused.

**Enable.** Requires every `requires` plugin to be enabled and no enabled plugin to conflict. Re-verifies the release digests, calls `describe`, validates each job/workflow with the registration checks under the enabling actor's authority, then registers the work as `plugin.<id>.<name>` (owner: the enabling actor; **disabled unless the resource says `enabled: true`**) and creates a native DBOS schedule `<actor>.plugin.<id>.<name>` for any `schedule` (paused unless enabled). Re-enabling resets materialized work to the declared state.

**Disable.** Refused while a plugin-owned or referencing work item has an active run (admitted, pending, enqueued, delayed) or a CLI/API/MCP call is in flight. New dispatch is blocked first, then activity is checked. Disable deactivates the materialized work and pauses its schedules; rows and run history stay readable.

**Disabled plugins cannot dispatch.** Beyond hiding UI, the gate is enforced at dispatch: `Core::admitted` (run admission, every re-check before an effect, scheduled handoffs) refuses plugin-owned work whose plugin is not enabled, and a `plugin.<id>` type resolves to no executable unless the plugin is enabled, so a user workflow that names it cannot be bound or run. CLI returns the declared denial, the API answers 503, MCP tools disappear and assets return 404.

**Uninstall.** Refused while enabled, while another installed plugin requires it, or while any other work definition references its type or its work ids. The registry row is deleted first, then the release directory. Materialized work stays (marked unusable) so run history remains inspectable; reinstalling the same plugin adopts it.

**Deferred (documented, not implemented):** upgrade of an installed plugin to a new version (uninstall and reinstall), plugin migrations, reviewed-registry admission and signed packages (digests prove integrity of what was installed, not who authored it), per-plugin settings and visibility controls, third-party sandboxing, a plugin-initiated way to start runs, and agent contributions. forge-rust's scheduler, workflow journal, late-effect, staging-quarantine, resume and capability-ledger machinery are deliberately not ported: DBOS does those jobs.

## Protocol 2

One JSON request on stdin, one JSON reply on stdout; the process is started per call, in the same containment as protocol 1 (process group and descendants terminated and reaped, stdout at most 1 MiB, stderr at most 64 KiB, bounded timeout). Every reply must carry `"api_version":2`; a reply whose version differs from the request is refused. Configured protocol-1 plugins are unchanged (`"api_version":1`).

| Operation | Request | Reply |
| --- | --- | --- |
| `describe` | `{}` | `description`, `schema` (JSON Schema for the `execute` input) |
| `execute` | `input`, `context:{run_id, idempotency_key}` | `output`. Runs inside the checkpointed DBOS step with the effect key; the plugin should use the key for idempotency |
| `cli` | `command` (declared path), `args` (strings), `principal` | `exit_code` (0-255), `stdout`, `stderr` (each at most 256 KiB) |
| `api` | `method`, `path` (declared route), `query` (object of strings), `body` (JSON or null), `principal` | `status` (200-299, 400-599), `headers` (only `content-type`, `cache-control`, `etag`), `body`. Content types allowed: `application/json`, `text/plain`, `text/csv` (never HTML); anything else becomes a 502 |
| `mcp` | `tool`, `arguments` (already validated against `input_schema`), `principal` | `output` (any JSON), optional `is_error` |
| `close` | `{}` | (not used for installed plugins; processes are per call) |

`cli`, `api` and `mcp` are direct request/response calls, **not** DBOS-journaled: they are audited and contained, but a mutating call has no automatic effect fence, so plugins must make such calls idempotent. Durable work belongs in a job or workflow.

## Surfaces

* **CLI:** `plugins run ID COMMAND...` sends the command to the service, which runs only a manifest-declared command path (longest declared prefix; everything after is data passed to the plugin, hyphens included). The plugin's `exit_code` becomes the process exit status. A disabled plugin returns its declared denial (default status 3).
* **HTTP API:** `/api/plugins/<id>/<route>` uses exactly the same bearer authentication as `/api` (missing or unknown credentials: 403 `{"error":...}`). Only declared methods and paths: 404 for an undeclared route, 405 for a declared path with another method. The actor needs the route's `capability` and the plugin's type. The body is bounded by `max_body_bytes` (413) and must be JSON (400). Responses carry `X-Content-Type-Options: nosniff` and default `Cache-Control: no-store`.
* **MCP:** `soulsteads-forge mcp` lists enabled plugin tools (for actors that may execute the plugin's type) on every `tools/list`, as `plugin_<id>__<tool>`; calls are forwarded to the service, which re-checks enablement and authority and validates the arguments. The list can change while the bridge runs; clients re-list (no `list_changed` notification is sent).
* **Dashboard:** enabled plugins' routes appear in the nav and load at `#/x/<id>/<path>`. The server serves the declared assets at `/plugins/<id>/assets/<resource path>` only for enabled plugins, **reading the release bytes and re-checking their digest on every request**, with a correct content type (`js`, `mjs`, `css`, `json`, `svg`, `png`, `txt`, `woff2` only), `nosniff` and `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`. Assets need no bearer token (a module is loaded by `import()`, which cannot send one), so they must contain no secrets. The page loads the module with `import()` and calls its exported `mount(el, host)` (or default export); a returned function is called on navigation away. The **host API** is a frozen object with `pluginId`, `route`, `mount` (the element), `fetch(subpath, init)` (adds the dashboard's bearer token and reaches only this plugin's own `/api/plugins/<id>/` routes) and `setStatus(text)`. The Plugins view lists installed plugins with Enable, Disable and Verify (a minimal management UI; the complete one is later work).
* **Jobs and workflows:** appear in `work list`, the dashboard and MCP like any other work, run with `work run` / schedules / manual triggers, and are inspected with the usual run views.

## Security model

* **Trusted first-party model.** An installed plugin executes with the service's operating-user authority (but not its environment, see below), and a dashboard module runs with the dashboard page's authority: it is loaded into the operator's dashboard page and can call every Forge operation with the operator's API access through `window.forge.call` (and the page's own `fetch`), not only its own routes. `host.fetch` is a convenience, not a boundary. Digests prove that the bytes that run are the bytes that were installed and reviewed; they do not prove who wrote them, and there is no signature or registry trust yet. Install only plugins you would run as yourself. Third-party sandboxing (isolated frames, capability-limited host API, signed packages) is deferred.
* Install and enable are operator actions behind the `plugins` capability, audited. Installs start disabled. Releases are read-only and re-verified at enable, startup and on every asset request, and by `plugins show ID --check`; a release that no longer matches comes back **broken** and cannot be enabled.
* Plugin surfaces never grant authority: every call is checked against the caller's capability and the plugin's type, request and response sizes are bounded, API response headers and content types are allow-listed, and the plugin only ever sees the authenticated principal's name.
* `paths.plugins` is archived by backup (see [recovery](recovery.md)); restore re-verifies every release.

## Process environment

A plugin process (installed or configured, every operation) is started with a cleared environment containing only `PATH` (the settings resolver's harness default `PATH`: `harness.default_path` or `FORGE_HARNESS_PATH`), `LANG` (the service's, else `C.UTF-8`), `FORGE_INVOCATION_ID`, and the variables the operator names in that plugin's `env_passthrough` list in `config.json` (configured executables only; installed plugins receive none). The service's environment is never inherited, so a plugin cannot read `FORGE_DATABASE_URL`, `FORGE_TOKEN`, the variable named by `database.url_env`, or any other credential the service holds. `env_passthrough` may not name any `FORGE_*` variable or the `database.url_env` target; such a configuration is refused at load time and the variable is also withheld at launch. Hand a plugin only the credentials it needs, by name.

## Configured executables (protocol 1)

No plugin is installed, enabled, or required by default. Operators may explicitly configure trusted executable extensions:

```json
{"plugin.example":{"command":["/absolute/path/to/provider"],"timeout_seconds":30,"env_passthrough":["PROVIDER_API_KEY"]}}
```

This object is the `plugins` value in private configuration. Add `plugin.example` to the intended actor's allowed `types`. Each invocation starts a process, writes one JSON request to stdin, and expects one JSON response on stdout. Responses must include `"api_version":1`.

- `{"api_version":1,"operation":"describe"}` returns `description` and a JSON `schema` for input validation.
- `{"api_version":1,"operation":"execute","input":{...},"context":{"run_id":"...","idempotency_key":"..."}}` returns `output`.
- `{"api_version":1,"operation":"close"}` supports explicit shutdown cleanup.

Open **Plugins** in the dashboard to read configured types, descriptions and schemas, validate input, and carry it into a new workflow step. The workflow builder registers immutable work; its Run action and the Runs view expose the actual outcome and native DBOS checkpoints. `plugins show plugin.example` gives the same cached metadata; add `--json` for `{type, description, schema}`. `plugins list` retains its JSON catalog. `plugins validate plugin.example --input input.json` accepts the plugin's input value, not a work definition (`-` reads stdin); `--json` returns the canonical `validate` result. Invalid inputs exit nonzero with located, value-free schema diagnostics.

Discovery uses `snapshot` (MCP `forge_snapshot`); validation uses `validate` (MCP `forge_validate`) with `{"definition":{"type":"plugin.example","input":{...}}}`. Neither invokes the plugin or stores work. Descriptions/schemas are public to actors with the existing `read` capability; configured commands and environment are not included. Validation requires `register` and the allowed type, exactly like registration; it does not grant `execute`. A denied or unknown type cannot dispatch work. Metadata must be suitable for readers; the executable supplies it. Configuration changes still require the operator's existing stopped-service configuration path and restart. Package installation and lifecycle management are the separate [installed plugins](#manifest-reference) above; settings editing, visibility controls and Python plugin-contribution compatibility remain unavailable.

Use the stable effect key when the provider supports idempotency. Plugin execution uses the same uncertainty and reconciliation rules as model calls. Timeouts are bounded to 1–300 seconds; stdout and stderr are capped, and process groups are terminated and reaped. Plugins run trusted code with the service's operating-user authority; this is not a sandbox or a compatibility layer for Python entry-point plugins.

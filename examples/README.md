# Runnable workflow examples

Each `import.json` is an Agent Forge `forge-definitions/v1` bundle. Use a running Forge instance and an actor that can read, register and execute the listed work types. Start with [the quick start](../README.md#quick-start) if you do not have a Forge home yet.

- **[Release readiness](release-readiness/import.json):** a two-step, no-model example. The second step consumes the first step's output and publishes a named result. It checks the workflow wiring, not whether a real release is ready.
- **[Rust PR reviewer](rust-pr-review/README.md)** ([import JSON](rust-pr-review/import.json)): reviews the Git patch for a pinned PR, returns prioritized findings with file/line citations, and includes a local citation verifier. Requires a configured model; no GitHub write access or executable plugin.

In the dashboard, open **Transfer → Work and schedule definitions**, choose the `import.json` file, leave replacement off, and select **Validate (dry run)**. Check the preview, then select **Import**. These bundles create no schedules and do not start a run.

Run the no-model example from the source checkout:

```sh
agent-forge definitions import examples/release-readiness/import.json --dry-run
agent-forge definitions import examples/release-readiness/import.json
agent-forge workflows run release-readiness --key readiness-demo-1 --wait
```

The `report` output is `Observed: example checklist ready`. Reuse the same key and parameters after an uncertain submission; use a new key for a genuinely new invocation. Open **Workflows** or **Runs** in the dashboard to inspect the saved definition and step evidence.

## Release-readiness report collector

See [the pinned release-readiness collector example](release-readiness-report/README.md) for a configured plugin workflow that collects evidence for one pinned repository revision and release tag.

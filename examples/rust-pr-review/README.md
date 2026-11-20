# Rust PR reviewer

This workflow reviews the textual changes introduced by a specific pull request. It records the repository label, PR number, base, head, merge base and patch SHA-256, then sends the bounded patch to a configured model for one review pass. Forge validates the response schema and stores the result with the run. The separate citation check verifies that each finding quotes a line added or removed in the exact patch.

The report covers concrete regressions such as incorrect error handling, panics, overflow, async and cancellation problems, unsafe invariants, and missing tests. It contains a summary, up to four prioritized findings (`P1`–`P3`), and review limitations. **No approval, comment, merge, checkout, build, or test is performed by this example.** An empty `findings` array means the model reported none, not that the PR is safe to merge. Citation verification proves the location and quote, not the model's interpretation.

## Setup

- A running Forge instance and an actor allowed to register and execute `workflow`, `echo`, `agent` and `model` work.
- An operator-configured OpenAI-compatible model alias named `rust-review`. Use a model with enough context for the patch and capable of emitting compact JSON in one turn. The agent has no tools or harness. Add the alias to the `models` object in the private Forge `config.json` while the service is stopped. For example, for a model you actually serve locally:

  ```json
  {
    "rust-review": {
      "base_url": "http://127.0.0.1:8000/v1",
      "model": "your-served-model-id"
    }
  }
  ```

  When authentication is required, set `key_env` to a credential variable **name** available to the Forge service. Do not put the credential value in the definition or parameters. Restart the service to load the configuration. Submit only code you are authorized to send to that model. Forge retains run evidence, including submitted parameters; keep review files private and out of Git.
- Python 3 and a Git version that supports `--no-lazy-fetch` and `--attr-source`, plus a trusted local clone (including its local Git configuration and `info/attributes`) with the PR's base and head commits and their history already fetched. The helper does not fetch. `--project` and `--pr` are labels supplied by the operator, not independently verified GitHub metadata.

## Import the workflow

In the dashboard, open **Transfer → Work and schedule definitions**, choose [`import.json`](import.json), leave replacement off, select **Validate (dry run)**, and then select **Import** if the preview is clean. Or use the CLI from the Agent Forge source root:

```sh
agent-forge definitions import examples/rust-pr-review/import.json --dry-run
agent-forge definitions import examples/rust-pr-review/import.json
```

This installs `rust-pr-review` without creating a schedule or starting a review. Re-importing the same definition is a no-op. A different existing definition with that ID is a collision; inspect it before replacing anything.

## Run a review

From the Agent Forge source root, set `REPO` to the trusted clone, `PROJECT` to `owner/repo`, `PR` to the PR number, and `BASE_SHA` and `HEAD_SHA` to the **full** commit IDs from the same PR metadata observation. Use a new private directory `REVIEW_DIR` for generated files. If you use GitHub CLI, `gh pr view "$PR" --repo "$PROJECT" --json baseRefOid,headRefOid` gives the IDs; fetching and local access are separate operator steps.

```sh
umask 077
python3 examples/rust-pr-review/review_packet.py prepare \
  --repo "$REPO" --project "$PROJECT" --pr "$PR" \
  --base "$BASE_SHA" --head "$HEAD_SHA" > "$REVIEW_DIR/params.json"
agent-forge workflows run rust-pr-review --params "$REVIEW_DIR/params.json" --key "$REVIEW_KEY" --wait
```

Import the definition once. Set `REVIEW_KEY` to a unique identity for the intended review, such as `review-42-v1`; reuse it with the same parameters after an uncertain response. A new head or changed input requires a new request key. Check the command's exit status at every step; do not submit if preparation fails.

With the successful run's ID assigned to `RUN_ID`, read back the result and verify its citations:

```sh
agent-forge runs show "$RUN_ID" --json > "$REVIEW_DIR/run.json"
python3 examples/rust-pr-review/review_packet.py verify \
  "$REVIEW_DIR/params.json" "$REVIEW_DIR/run.json"
```

The same output is visible on the dashboard's **Runs** page. Keep the original `params.json` with the run export. The verifier expects both files to come from the commands above; it is not a signature or independent validation of GitHub metadata. Compare the live PR head before using the review. Check CI and review each finding yourself before sharing it.

## Failure and scope limits

- Only the committed merge-base-to-head patch is included. The helper rejects patches above 32 KiB, numbered evidence above 48 KiB, or a serialized parameter file above 256 KiB rather than silently truncating them. Binary changes, submodule changes and unsupported paths are refused. For a large PR, review smaller independently pinned changes or use a different review process.
- The model does not see unchanged source outside diff context, CI results, or repository history. Generated code and changes without textual hunks may require separate inspection. Do not interpret this as repository-wide coverage.
- If Forge rejects the model response or the verifier rejects a citation, inspect the exact run and failing step. Do not treat a malformed or unanchored finding as a review result, and do not hide ambiguous provider effects by retrying automatically.

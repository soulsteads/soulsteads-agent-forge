# Pinned GitHub release-readiness report

This is a separate example from `examples/release-readiness/`. It runs the
configured Forge stdio plugin `plugin.release-readiness-report` against named,
offline synthetic fixtures. `import.json` shows a pinned 40-character commit,
required check names, exact release-asset names, a tag and an explicit UTC
observation time. No fixture calls GitHub. At least one required release-asset name
must be configured; each name is matched exactly, including case and punctuation.

## Import and configure

Import `import.json` as a normal `forge-definitions/v1` bundle. The plugin must
already be configured and allowed for the workflow actor; imports do not install
or enable executables. In the operator's private Forge configuration, configure
the protocol-1 executable along these lines (replace the path with the absolute
path to this directory):

```json
{
  "plugins": {
    "plugin.release-readiness-report": {
      "command": ["/usr/bin/python3", "/absolute/path/release-readiness-report/collector.py"],
      "timeout_seconds": 20,
      "env_passthrough": ["RELEASE_READINESS_ALLOWED_REPOSITORY"]
    }
  }
}
```

The fixture workflow needs no environment variable. For live mode, set
`RELEASE_READINESS_ALLOWED_REPOSITORY` in the plugin process environment
to exactly one operator-approved public `owner/repository` and pass through only
that variable. There is no token requirement; live mode uses bounded,
unauthenticated, read-only GitHub REST API requests. Do not put credentials in
workflow parameters.

For `mode: live`, do not pass `observed_at` at all (even as `null`): the
collector samples its own UTC timestamp at collection start, before the GitHub
reads. Fixture mode retains its explicit `observed_at`, keeping the deterministic
fixture report stable. Forge resolves the workflow's fixed `/params/...`
references, so remove both `fixture_id` and `observed_at` from `params` and
`collect.action.input` in a copy; removing only a parameter leaves a dangling
reference.

### Convert the fixture import to a live workflow

Save this standard-library Python snippet as
`convert-release-readiness.py`, then run it from the repository root with
`python3 convert-release-readiness.py > release-readiness-live-import.json`.
Replace the example repository, full commit SHA, tag, required check names and
exact asset names with the chosen values before using the generated import.

```python
from copy import deepcopy
import json
from pathlib import Path

source = json.loads(Path("examples/release-readiness-report/import.json").read_text())
live = deepcopy(source)
workflow = next(
    item for item in live["work"] if item["id"] == "release-readiness-report"
)["definition"]["input"]
workflow["params"].pop("fixture_id")
workflow["params"].pop("observed_at")
workflow["params"].update({
    "mode": "live",
    "repository": "example-org/widget",
    "revision": "0123456789abcdef0123456789abcdef01234567",
    "tag": "v1.2.3",
    "required_checks": ["build", "test"],
    "required_artifacts": ["dist.tar.gz", "SHA256SUMS"],
})
collect = next(step for step in workflow["steps"] if step["id"] == "collect")
for key in ("fixture_id", "observed_at"):
    collect["action"]["input"].pop(key)
print(json.dumps(live, indent=2))
```

The live reader constructs requests only under `https://api.github.com/repos/`
for the configured repository. It issues GET requests only, refuses redirects,
accepts no URL or command input, caps each response at 1 MiB and each list at ten
100-item pages. The collector checks a best-effort 12-second elapsed budget
between bounded response-body reads and applies 3-second connect/per-read socket
timeouts, capped by the remaining budget. A read already in progress can exceed
that collector budget; the sample's `timeout_seconds: 20` is the outer Forge
plugin bound. Detected truncation, failed reads, and late bodies are reported as
unavailable and are not parsed. Redirects and environment proxies are disabled. Git tag objects
are resolved through bounded annotated-tag chains.
GitHub response fields are treated only as data; they are never executed or
interpreted as instructions. API failures, malformed or clipped pages and
mismatched commit evidence cannot produce a `ready` verdict.

## Report and proof boundary

The report has these fields: `format`, `repository`, `revision`, `tag`,
`required_checks`, `artifact_policy`, `observed_at`, `criteria`,
`evidence_references`, `collection_status` and `verdict`. Every evidence
reference records the requested full revision; check runs also retain their
observed `head_sha`, and tag evidence retains the resolved target. A check SHA or
tag target different from the pin is unverified evidence, never evidence for the
pinned revision.

`collection_status` describes evidence retrieval and validation: `complete`,
`partial` or `unavailable`. `verdict` is the criteria assessment: `ready`,
`not_ready` or `incomplete`. Failed or missing required checks, an absent tag,
and confirmed missing required release assets are `not_ready` only when the
collection is complete. Pending checks, clipped pages, unavailable sources,
mismatches and unverified evidence are `incomplete`. An absent tag is recorded
as `tag.state: absent`; a tag that exists but has no qualifying downloadable
assets is `tag.state: present` with the required names in
`criteria.artifacts.missing`.

Artifact policy is deliberately limited to exact GitHub release-asset names,
positive asset sizes and GitHub's `uploaded` state. This collector does **not**
download or hash asset contents; `ready` therefore means the declared metadata
criteria passed, not that binary bytes or provenance were independently
verified.

The receipt keeps the digest separate from the report bytes. It contains
`report_sha256`, `report_byte_length` and `report_bytes_utf8`. The digest is the
lowercase SHA-256 of the exact UTF-8 bytes in `report_bytes_utf8`, where report
bytes are canonical JSON (`sort_keys`, compact separators, UTF-8, no ASCII
escaping). The digest is outside the hashed report and is not self-referential.

Collection is not delivery. This example emits a report and receipt as workflow
output; it does not persist, publish, comment, merge, create a release or decide
that a release should be cut. A separate authorized delivery step must record
whether the envelope was durably stored or distributed. A complete collection
can still produce `not_ready`, and a complete verdict alone is not delivery
proof.

## Synthetic fixture coverage

`fixtures/samples.json` contains only fabricated GitHub-shaped data for `forge-fixture/release-readiness` and fake object/run/asset IDs. The named cases cover ready, pending, failed, no tag, tag with no assets, unavailable, clipped/partial, mismatched check SHA, mismatched tag target and unverified check evidence. The ready sample uses an annotated tag and resolves it to the pinned commit. These fixtures are deterministic test inputs, not claims about any real GitHub repository.

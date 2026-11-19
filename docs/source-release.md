# Source snapshots

A public source snapshot contains the files needed to build, inspect and test Agent Forge. `public-source.allowlist` lists its complete tracked file set. Check the exact committed tree, including Markdown links and third-party notices, with:

```sh
python3 scripts/check_public_source.py --revision HEAD --scope public
python3 scripts/check_notices.py --revision HEAD
```

To export a source archive, run `python3 scripts/export_source.py HEAD OUTPUT`, where `OUTPUT` is a new directory outside the repository. The exporter reads only regular files from the selected Git commit and records mode, size and SHA-256 in a separate manifest. Local edits, untracked files, and Git history are not included. Use an operator-owned output directory; neither it nor its parents may be symbolic links.

See [verification](support.md#verification) for build and test commands. A verified source archive is not evidence of a deployed service or a hosted CI result.

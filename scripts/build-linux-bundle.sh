#!/usr/bin/env bash
# Build the production GNU/Linux executable and its release notices in scratch.
set -euo pipefail
if (( $# != 1 )); then
  echo "Usage: $0 EXISTING_SCRATCH_DIRECTORY" >&2
  exit 2
fi
RUNNER_TEMP=$(realpath -- "$1")
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
bundle='soulsteads-agent-forge-linux-x86_64'
mkdir "$RUNNER_TEMP/$bundle"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$RUNNER_TEMP/release-target}"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=debuginfo
# Ubuntu 24.04 GNU/Linux build; do not include dev/test executables.
cargo +1.100.0 build --release --locked --target x86_64-unknown-linux-gnu --bin soulsteads-forge
install -m 0755 "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/soulsteads-forge" "$RUNNER_TEMP/$bundle/soulsteads-forge"
install -m 0644 LICENSE NOTICE "$RUNNER_TEMP/$bundle/"
install -m 0644 THIRD_PARTY_NOTICES.md "$RUNNER_TEMP/$bundle/"
"$RUNNER_TEMP/$bundle/soulsteads-forge" --help > /dev/null
cargo +1.100.0 metadata --format-version 1 --locked --filter-platform x86_64-unknown-linux-gnu > "$RUNNER_TEMP/cargo-metadata.json"
cargo +1.100.0 tree --color never --locked --target x86_64-unknown-linux-gnu \
  -e normal,build --prefix none --format '{p}' > "$RUNNER_TEMP/cargo-tree.txt"
python3 - "$RUNNER_TEMP/cargo-metadata.json" "$RUNNER_TEMP/$bundle" "$RUNNER_TEMP/cargo-tree.txt" <<'PY'
import hashlib
import json
from pathlib import Path
import re
import shutil
import sys

metadata = json.loads(Path(sys.argv[1]).read_text())
bundle = Path(sys.argv[2])
notices = bundle / 'dependency-notices'
notices.mkdir()
fallback_dir = Path('scripts/release-notices')
fallbacks = json.loads((fallback_dir / 'upstream.json').read_text())
# The tree selects the actual target's packages. Metadata can include
# additional crates from feature resolution on other targets.
tree = []
for line in Path(sys.argv[3]).read_text().splitlines():
    match = re.fullmatch(r'([\w-]+) v(\S+)(?: .*)?', line)
    if match is None:
        raise SystemExit('Unexpected cargo tree package format')
    tree.append(match.groups())
root = next(pkg for pkg in metadata['packages'] if pkg['id'] == metadata['resolve']['root'])
if not tree or tree[0] != (root['name'], root['version']):
    raise SystemExit('Cargo tree and metadata roots differ')
selected = set(tree[1:])
packages = {}
for pkg in metadata['packages']:
    key = (pkg['name'], pkg['version'])
    if key in selected:
        if key in packages:
            raise SystemExit(f'Ambiguous metadata package: {key[0]}-{key[1]}')
        packages[key] = pkg
if selected != packages.keys():
    raise SystemExit('Cargo tree package missing from metadata')

rows = []
legal = re.compile(r'(license|licence|copying|copyright|notice|patents|unlicense)(?:$|[._-])', re.I)
for package in sorted(selected):
    pkg = packages[package]
    source = Path(pkg['manifest_path']).parent.resolve()
    target = notices / (pkg['name'] + '-' + pkg['version'])
    files = {path for path in source.rglob('*')
             if path.is_file() and legal.match(path.name) and path.suffix != '.rs'}
    if pkg['license_file']:
        files.add(source / pkg['license_file'])
    if target.name == 'crc-catalog-2.5.0':
        files.update(source / 'LICENSES' / name for name in ('Apache-2.0.txt', 'MIT.txt'))
    copied = []
    for path in sorted(files):
        relative = path.relative_to(source)
        if '..' in relative.parts or not path.resolve().is_relative_to(source):
            raise SystemExit(f'License file outside crate: {target.name}')
        if not path.is_file() or not path.read_bytes().strip():
            raise SystemExit(f'Missing or empty notice for {target.name}: {relative}')
        dest = target / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, dest)
        copied.append(relative.as_posix())
    if not copied:
        fallback = fallbacks.get(target.name)
        if fallback is None:
            raise SystemExit(f'No notice files for {target.name}')
        vcs = json.loads((source / '.cargo_vcs_info.json').read_text())
        if (pkg['source'] != 'registry+https://github.com/rust-lang/crates.io-index'
                or pkg['repository'] != fallback['repository']
                or vcs['git']['sha1'] != fallback['commit']):
            raise SystemExit(f'Upstream notice provenance mismatch for {target.name}')
        data = (fallback_dir / fallback['file']).read_bytes()
        if not data.strip() or hashlib.sha256(data).hexdigest() != fallback['sha256']:
            raise SystemExit(f'Upstream notice checksum mismatch for {target.name}')
        target.mkdir()
        (target / fallback['member']).write_bytes(data)
        copied.append(fallback['member'])
    rows.append(f"{target.name}\t{pkg['license'] or 'See packaged license file'}\t{', '.join(copied)}\n")
(notices / 'INDEX.tsv').write_text('Crate\tDeclared license\tBundled notice files\n' + ''.join(rows))
PY

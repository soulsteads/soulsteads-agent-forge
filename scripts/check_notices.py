#!/usr/bin/env python3
"""Check the source dependency license inventory and Cargo package inclusion."""
import argparse
from collections import Counter
import hashlib
import io
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib


TABLE_ROW = re.compile(r'^\| ([A-Za-z0-9_-]+) \| ([0-9][^|\s]*) \| ([^|\n]+) \|$', re.M)


def check(root, read_bytes=None, *, cargo_home=None):
    read_bytes = read_bytes or (lambda path: (root / path).read_bytes())
    lock = tomllib.loads(read_bytes('Cargo.lock').decode())
    expected = {(p['name'], p['version']): p for p in lock['package'] if 'source' in p}
    notices = read_bytes('THIRD_PARTY_NOTICES.md').decode()
    entries = [((name, version), license_id.strip()) for name, version, license_id in TABLE_ROW.findall(notices)
               if license_id.strip()]
    rows = Counter(key for key, _ in entries)
    errors = []
    if rows != Counter(expected.keys()):
        missing = sorted(expected.keys() - rows.keys())
        extra = sorted(rows.keys() - expected)
        duplicates = sorted(key for key, count in rows.items() if count != 1)
        errors.append(f'license inventory differs from Cargo.lock: missing={missing}; '
                      f'extra={extra}; duplicates={duplicates}')
    elif cargo_home is not None:
        errors.extend(check_registry_licenses(dict(entries), expected, cargo_home))
    include = tomllib.loads(read_bytes('Cargo.toml').decode())['package'].get('include', [])
    if 'THIRD_PARTY_NOTICES.md' not in include:
        errors.append('Cargo include must carry THIRD_PARTY_NOTICES.md')
    return errors


def check_registry_licenses(entries, expected, cargo_home):
    archives = {}
    for archive in (cargo_home / 'registry/cache').glob('*/*.crate'):
        archives.setdefault(archive.name, []).append(archive)
    for key, declared in entries.items():
        package = expected[key]
        crate = '-'.join(key)
        if package['source'] != 'registry+https://github.com/rust-lang/crates.io-index':
            yield f'{crate}: unsupported license verification source'
            continue
        for archive in archives.get(crate + '.crate', []):
            data = archive.read_bytes()
            if hashlib.sha256(data).hexdigest() == package.get('checksum'):
                break
        else:
            yield f'{crate}: locked crate archive unavailable'
            continue
        try:
            with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as tar:
                member = tar.extractfile(crate + '/Cargo.toml')
                if member is None:
                    raise ValueError('crate manifest is not a file')
                manifest = member.read()
            actual = tomllib.loads(manifest.decode())['package']['license']
        except (KeyError, OSError, ValueError, tarfile.TarError):
            yield f'{crate}: locked crate manifest/license unavailable'
            continue
        if declared != actual:
            yield f'{crate}: declared license differs from locked crate manifest'


def check_package_list(paths):
    required = {'LICENSE', 'THIRD_PARTY_NOTICES.md'}
    examples = {
        'examples/README.md',
        'examples/release-readiness/import.json',
        'examples/rust-pr-review/README.md',
        'examples/rust-pr-review/import.json',
        'examples/rust-pr-review/review_packet.py',
    }
    errors = []
    if required - paths:
        errors.append(f'Cargo package omits license files: {sorted(required - paths)}')
    if examples - paths:
        errors.append(f'Cargo package omits workflow example files: {sorted(examples - paths)}')
    if any(p.split('/')[0].casefold() in ('internal', '.git', '.serena') for p in paths):
        errors.append('Cargo package includes private material')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision')
    parser.add_argument('--package-list', type=Path, help='Validate cargo package --list output too')
    parser.add_argument('--verify-registry-licenses', action='store_true',
                        help='Compare declared licenses with locally cached, checksum-verified crates')
    args = parser.parse_args()
    reader = None
    if args.revision:
        commit = subprocess.check_output(['git', 'rev-parse', '--verify', args.revision + '^{commit}']).decode().strip()
        reader = lambda path: subprocess.check_output(['git', 'show', commit + ':' + path])
    cargo_home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')) if args.verify_registry_licenses else None
    errors = check(Path('.'), reader, cargo_home=cargo_home)
    if args.package_list:
        errors += check_package_list(set(args.package_list.read_text().splitlines()))
    if errors:
        raise SystemExit('\n'.join(errors))
    print('Locked dependency license inventory and Cargo inclusion passed')


if __name__ == '__main__':
    main()

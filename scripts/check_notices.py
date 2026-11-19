#!/usr/bin/env python3
"""Verify locked notice coverage, retained bytes/provenance and Cargo include policy.

Defaults to working files for local notice maintenance. --revision reads only
immutable Git blobs; neither mode fetches dependencies or builds anything.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tomllib


def check(root, read_bytes=None):
    read_bytes = read_bytes or (lambda path: (root / path).read_bytes())
    lock = tomllib.loads(read_bytes('Cargo.lock').decode())
    packages = {p['name'] + '-' + p['version']: p for p in lock['package'] if 'source' in p}
    text = read_bytes('THIRD_PARTY_NOTICES.md').decode()
    table = re.findall(r'^\| ([^ |]+) \| ([^|]+) \| \[texts\]\(licenses/([^/]+)/\) \|$', text, re.M)
    names = [name for name, _, _ in table]
    errors = []
    if set(names) != set(packages) or any(n != 1 for n in Counter(names).values()):
        errors.append(f'notice table differs from lockfile: missing={sorted(set(packages) - set(names))}; extra={sorted(set(names) - set(packages))}')
    for name, license_id, directory in table:
        if name != directory or not license_id.strip():
            errors.append(f'{name}: invalid license declaration or notice directory')
    rows = json.loads(read_bytes('licenses/provenance.json'))
    paths = set()
    covered = set()
    for row in rows:
        path = row['path']
        parts = PurePosixPath(path).parts
        if (len(parts) < 3 or parts[0] != 'licenses' or '..' in parts or path in paths
                or PurePosixPath(path).as_posix() != path):
            errors.append(f'invalid or duplicate notice path: {path}')
            continue
        paths.add(path)
        package = parts[1]
        covered.add(package)
        if not row.get('source', '').startswith(('crates.io:', 'https://')):
            errors.append(f'{path}: missing source provenance')
        if package in packages and 'crate_sha256' in row and row['crate_sha256'] != packages[package]['checksum']:
            errors.append(f'{path}: archive checksum differs from lockfile')
        try:
            raw = read_bytes(path)
        except (FileNotFoundError, subprocess.CalledProcessError):
            errors.append(f'{path}: retained notice missing')
            continue
        if not raw or hashlib.sha256(raw).hexdigest() != row['sha256']:
            errors.append(f'{path}: retained notice bytes differ from provenance')
    if set(packages) - covered:
        errors.append(f'locked packages lack retained provenance: {sorted(set(packages) - covered)}')
    include = tomllib.loads(read_bytes('Cargo.toml').decode())['package'].get('include', [])
    if not {'THIRD_PARTY_NOTICES.md', 'licenses/**'} <= set(include):
        errors.append('Cargo include must carry THIRD_PARTY_NOTICES.md and licenses/**')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision')
    parser.add_argument('--package-list', type=Path, help='Validate cargo package --list output too')
    args = parser.parse_args()
    reader = None
    if args.revision:
        commit = subprocess.check_output(['git', 'rev-parse', '--verify', args.revision + '^{commit}']).decode().strip()
        reader = lambda path: subprocess.check_output(['git', 'show', commit + ':' + path])
    errors = check(Path('.'), reader)
    if args.package_list:
        errors += check_package_list(Path('.'), set(args.package_list.read_text().splitlines()), reader)
    if errors:
        raise SystemExit('\n'.join(errors))
    print('Locked notice coverage, provenance hashes and Cargo inclusion passed')


def check_package_list(root, paths, read_bytes=None):
    read_bytes = read_bytes or (lambda path: (root / path).read_bytes())
    required = {'LICENSE', 'THIRD_PARTY_NOTICES.md', 'licenses/provenance.json'}
    required.update(row['path'] for row in json.loads(read_bytes('licenses/provenance.json')))
    errors = []
    if required - paths:
        errors.append(f'Cargo package omits notice files: {sorted(required - paths)}')
    if any(p.split('/')[0].casefold() in ('internal', '.git', '.serena') for p in paths):
        errors.append('Cargo package includes private material')
    return errors


if __name__ == '__main__':
    main()

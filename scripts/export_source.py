#!/usr/bin/env python3
"""Export a classified tracked snapshot: export_source.py REV NEW_EXTERNAL_DIR."""
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile


def git(*args):
    return subprocess.check_output(['git', *args])


def validate_path(path, *, public=False):
    p = PurePosixPath(path)
    if (not p.parts or p.is_absolute() or p.as_posix() != path
            or any(part in ('.', '..') for part in p.parts)
            or any(part.casefold() == '.git' for part in p.parts)
            or any(ord(c) < 32 or ord(c) == 127 for c in path)
            or '\\' in path or ':' in path):
        raise SystemExit(f'unsafe path: {path!r}')
    if public and p.parts[0].casefold() == 'internal':
        raise SystemExit(f'internal boundary cannot be allowlisted: {path}')
    if public and p.parts[0].casefold() == '.serena':
        raise SystemExit(f'local editor metadata cannot be allowlisted: {path}')


def load_exclusions(raw):
    def unique(pairs):
        result = dict(pairs)
        if len(result) != len(pairs):
            raise SystemExit('duplicate exclusion paths')
        return result

    excluded = json.loads(raw, object_pairs_hook=unique)
    if not isinstance(excluded, dict) or any(
            not isinstance(reason, str) or not reason.strip() for reason in excluded.values()):
        raise SystemExit('exclusions must map exact paths to nonempty reasons')
    return excluded


def main():
    revision, destination = sys.argv[1:]
    root = Path(git('rev-parse', '--show-toplevel').decode().strip()).resolve()
    requested = Path(destination).absolute()
    # Do not resolve away an existing dangling symlink or redirect through a
    # symlinked parent. mkdir(exist_ok=False) below also guards the final leaf.
    if any(p.is_symlink() for p in (requested, *requested.parents)):
        raise SystemExit('output path and parents must not be symlinks')
    out = requested.resolve()
    if out == root or root in out.parents:
        raise SystemExit('output must be outside checkout')
    if out.exists():
        raise SystemExit('output must not already exist')
    commit = git('rev-parse', '--verify', revision + '^{commit}').decode().strip()
    tree = git('rev-parse', commit + '^{tree}').decode().strip()
    entries = {}
    for record in git('ls-tree', '-rz', commit).split(b'\0'):
        if not record:
            continue
        meta, path = record.split(b'\t', 1)
        mode, kind, oid = meta.decode().split()
        entries[path.decode()] = (mode, kind, oid)
    allow = git('show', commit + ':public-source.allowlist').decode().splitlines()
    excludes = load_exclusions(git('show', commit + ':public-source.exclusions.json'))
    if len(allow) != len(set(allow)) or set(allow) & set(excludes):
        raise SystemExit('duplicate or conflicting classifications')
    if set(entries) != set(allow) | set(excludes):
        missing = sorted(set(entries) - set(allow) - set(excludes))
        stale = sorted((set(allow) | set(excludes)) - set(entries))
        raise SystemExit(f'tracked paths differ from exact classifications; '
                         f'unclassified ({len(missing)}): {missing}; stale: {stale}')
    allowed = set(allow)
    for path in entries:
        validate_path(path, public=path in allowed)
    for path in allow:
        mode, kind, _ = entries[path]
        if kind != 'blob' or mode not in ('100644', '100755'):
            raise SystemExit('only regular tracked source files accepted')
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    manifest = []
    archive = out / 'forge-source.tar'
    with tarfile.open(archive, 'w', format=tarfile.USTAR_FORMAT) as tar:
        for path in sorted(allow):
            mode, _, oid = entries[path]
            raw = git('cat-file', 'blob', oid)
            info = tarfile.TarInfo('forge-source/' + path)
            info.size = len(raw)
            info.mode = int(mode, 8) & 0o777
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            tar.addfile(info, io.BytesIO(raw))
            manifest.append({'path': path, 'size': len(raw), 'mode': mode,
                             'sha256': hashlib.sha256(raw).hexdigest(), 'git_blob': oid})
    data = {'revision': commit, 'tree': tree, 'archive': archive.name,
            'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
            'files': manifest, 'excluded': excludes,
            'ignored_files': 'never read; only classified tracked objects exported',
            'history_included': False}
    (out / 'manifest.json').write_text(json.dumps(data, indent=2) + '\n')
    (out / 'ALLOWLIST').write_text('\n'.join(sorted(allow)) + '\n')
    print(json.dumps({k: data[k] for k in ('revision', 'tree', 'archive_sha256')}))


if __name__ == '__main__':
    main()

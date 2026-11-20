#!/usr/bin/env python3
"""Prepare and verify a bounded, advisory Forge review of local Git objects."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time

MAX_DIFF = 32 * 1024
MAX_REVIEW_DIFF = 48 * 1024
MAX_PARAMS = 256 * 1024
SHA = re.compile(r'(?:[0-9a-f]{40}|[0-9a-f]{64})')
PATH = re.compile(r'[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*')
HUNK = re.compile(r'^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*)?$')
GITLINK = re.compile(r'^(?:(?:new file|deleted file|old|new) mode|index [0-9a-f]+\.\.[0-9a-f]+) 160000$')


def git(repo, *args, cap=MAX_DIFF):
    # Ignore redirects/overrides from an enclosing Git invocation and never
    # fetch missing objects from a partial clone. Use a trusted local clone.
    env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    env.update(GIT_ATTR_NOSYSTEM='1', GIT_NO_REPLACE_OBJECTS='1', LC_ALL='C')
    deadline = time.monotonic() + 20
    with subprocess.Popen(['git', '--no-lazy-fetch', '-C', str(repo), *args], env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as child:
        assert child.stdout is not None  # stdout=PIPE above
        try:
            data = bytearray()
            with selectors.DefaultSelector() as ready:
                ready.register(child.stdout, selectors.EVENT_READ)
                while True:
                    if not ready.select(max(0, deadline - time.monotonic())):
                        raise ValueError('Git read timed out')
                    block = os.read(child.stdout.fileno(), min(8192, cap + 1 - len(data)))
                    if not block:
                        break
                    data.extend(block)
                    if len(data) > cap:
                        raise ValueError('Git evidence exceeds the byte limit; split the review')
            if child.wait(timeout=max(0, deadline - time.monotonic())):
                raise ValueError(f'Git {args[0]} failed; check the repository and fetch the missing objects separately')
        finally:
            if child.poll() is None:
                child.kill()
    return data.decode('utf-8')


def digest(diff):
    return hashlib.sha256(diff.encode('utf-8')).hexdigest()


def render_evidence(diff):
    """Preserve the patch with explicit changed-line coordinates for the model."""
    before = after = None
    old_line = new_line = old_left = new_left = 0
    rendered, changed = [], {}
    size = 0
    for row in diff.split('\n'):
        anchor = None
        if row == r'\ No newline at end of file':
            pass
        elif old_left or new_left:
            if row.startswith('-') and old_left and before:
                anchor = ('left', before, old_line)
                old_line += 1
                old_left -= 1
            elif row.startswith('+') and new_left and after:
                anchor = ('right', after, new_line)
                new_line += 1
                new_left -= 1
            elif row.startswith(' ') and old_left and new_left:
                old_line += 1
                new_line += 1
                old_left -= 1
                new_left -= 1
            else:
                raise ValueError('invalid or truncated Git hunk')
        elif row.startswith('diff --git '):
            before = after = None
        elif row.startswith(('--- ', '+++ ')):
            name = row[4:]
            prefix = 'a/' if row.startswith('--- ') else 'b/'
            if name == '/dev/null':
                path = None
            elif name.startswith(prefix) and PATH.fullmatch(name[2:]):
                path = name[2:]
            else:
                raise ValueError('unsupported diff path; use a patch with plain repository-relative paths')
            if prefix == 'a/':
                before = path
            else:
                after = path
        elif row.startswith('@@ '):
            hunk = HUNK.fullmatch(row)
            if not hunk:
                raise ValueError('invalid Git hunk header')
            old_line, old_count, new_line, new_count = hunk.groups()
            old_line, new_line = int(old_line), int(new_line)
            old_left, new_left = int(old_count or 1), int(new_count or 1)
        elif row.startswith(('Binary files ', 'GIT binary patch', 'Submodule ')) or GITLINK.fullmatch(row):
            raise ValueError('binary or submodule changes are outside this example')
        if anchor:
            side, path, line = anchor
            changed[anchor] = row[1:]
            row = f'[{side} {path}:{line}] {row}'
        size += len(row.encode('utf-8')) + bool(rendered)
        if size > MAX_REVIEW_DIFF:
            raise ValueError('numbered review evidence exceeds the byte limit; split the review')
        rendered.append(row)
    if old_left or new_left:
        raise ValueError('truncated Git hunk')
    if not changed:
        raise ValueError('no changed text lines to review')
    return '\n'.join(rendered), changed


def prepare(args):
    if not re.fullmatch(r'[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100}', args.project) or not 0 < args.pr < 2**53:
        raise ValueError('supply an owner/repo label and a positive PR number')
    try:
        git(args.repo, '--attr-source=HEAD', '--version', cap=128)
    except ValueError as error:
        raise ValueError('Git must support --no-lazy-fetch and --attr-source') from error
    for rev in (args.base, args.head):
        if not SHA.fullmatch(rev) or git(args.repo, 'rev-parse', '--verify', rev + '^{commit}', cap=128).strip() != rev:
            raise ValueError('base and head must each be the full commit ID of a local Git commit')
    bases = git(args.repo, 'merge-base', '--all', args.base, args.head, cap=4096).splitlines()
    if len(bases) != 1 or not SHA.fullmatch(bases[0]):
        raise ValueError('a unique merge base is required')
    diff = git(args.repo, f'--attr-source={args.head}', '-c', 'core.attributesFile=/dev/null',
               '-c', 'diff.suppressBlankEmpty=false', 'diff', '--patch', '--no-relative',
               '--no-ext-diff', '--no-textconv', '--no-color',
               '--no-renames', '--ignore-submodules=none', '--submodule=short', '--src-prefix=a/',
               '--dst-prefix=b/', '--output-indicator-new=+', '--output-indicator-old=-',
               '--output-indicator-context= ', '--unified=5', bases[0], args.head, '--')
    review_diff, _ = render_evidence(diff)
    return {'source': {'project': args.project, 'pr': args.pr, 'base': args.base, 'head': args.head,
                       'merge_base': bases[0], 'diff_sha256': digest(diff)}, 'diff': diff, 'review_diff': review_diff}


def read_json(path, limit):
    with path.open('rb') as file:
        data = file.read(limit + 1)
    if len(data) > limit:
        raise ValueError('JSON file exceeds byte limit')
    return json.loads(data)


def verify(args):
    params = read_json(args.params, MAX_PARAMS)
    run = read_json(args.run, 2 * 1024 * 1024)
    if run['status'] != 'SUCCESS' or run['type'] != 'workflow':
        raise ValueError('expected a successful workflow inspection')
    result = run['output']['output']
    if result['source'] != params['source'] or digest(params['diff']) != params['source']['diff_sha256']:
        raise ValueError('run source does not match the prepared evidence')
    rendered, changed = render_evidence(params['diff'])
    if rendered != params['review_diff']:
        raise ValueError('numbered review evidence does not match the original patch')
    findings = result['review']['findings']
    if not isinstance(findings, list):
        raise ValueError('review findings must be an array')
    for finding in findings:
        line = finding['line']
        quote = finding['quote']
        anchor = (finding['side'], finding['path'], line)
        if type(line) is not int or anchor not in changed or not quote.strip() or quote not in changed[anchor]:
            raise ValueError('finding must quote an exact changed line on its declared side')
    return {'run_id': run['run_id'], 'checked_findings': len(findings), 'source': result['source'],
            'review': result['review'], 'verification': 'changed-line citations only; not an approval'}


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    commands = cli.add_subparsers(dest='command', required=True)
    prep = commands.add_parser('prepare', help='emit workflow parameter JSON from pinned local commits')
    prep.add_argument('--repo', type=Path, required=True)
    prep.add_argument('--project', required=True)
    prep.add_argument('--pr', type=int, required=True)
    prep.add_argument('--base', required=True)
    prep.add_argument('--head', required=True)
    check = commands.add_parser('verify', help='check finding anchors against the submitted patch')
    check.add_argument('params', type=Path)
    check.add_argument('run', type=Path)
    args = cli.parse_args()
    try:
        if args.command == 'prepare':
            # JSON escaping expands both copies of the patch. Check the exact
            # bytes the verifier will read, including the trailing newline.
            encoded = (json.dumps(prepare(args), indent=2) + '\n').encode('utf-8')
            if len(encoded) > MAX_PARAMS:
                raise ValueError('parameter JSON exceeds the byte limit; split the review')
            sys.stdout.buffer.write(encoded)
        else:
            print(json.dumps(verify(args), indent=2))
    except (OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.TimeoutExpired) as error:
        cli.exit(1, f'review packet: {error}\n')


if __name__ == '__main__':
    main()

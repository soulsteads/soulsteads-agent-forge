#!/usr/bin/env python3
"""Check exact policy and public Markdown links in one immutable revision (default HEAD)."""
import argparse
from collections import Counter
from pathlib import Path
import posixpath
import re
import subprocess
from urllib.parse import unquote, urlsplit

from export_source import load_exclusions, validate_path


def prose(text):
    """Ignore fenced examples when finding links and heading anchors."""
    return re.sub(r'^(`{3,}|~{3,}).*?^\1[^\n]*$', '', text, flags=re.M | re.S)


def anchors(text):
    found = set(re.findall(r'<a\s+(?:id|name)=["\']([^"\']+)', text))
    counts = Counter()
    for heading in re.findall(r'^ {0,3}#{1,6}\s+(.+?)\s*#*$', prose(text), re.M):
        heading = re.sub(r'\[([^\]]+)\]\([^)]*\)', r'\1', heading)
        slug = re.sub(r'[^\w\- ]', '', heading.lower()).replace(' ', '-')
        anchor = slug if not counts[slug] else f'{slug}-{counts[slug]}'
        counts[slug] += 1
        found.add(anchor)
    return found


def check_links(root, tracked, public, read_text=None):
    read_text = read_text or (lambda path: (root / path).read_text())
    files = set(tracked) & set(public)
    errors = []
    for path in sorted(p for p in files if p.lower().endswith('.md')):
        text = re.sub(r'(`+).*?\1', '', prose(read_text(path)), flags=re.S)
        # Inline links/images and reference definitions. Reference uses share
        # the definition's target; undefined explicit references are errors.
        definitions = dict(re.findall(r'^\s*\[([^\]]+)\]:\s*(\S+)', text, re.M))
        definitions = {label.casefold(): target for label, target in definitions.items()}
        targets = re.findall(r'!?\[[^\]\n]*\]\(\s*(<[^>]+>|[^\s)]+)', text)
        targets += list(definitions.values())
        for label, ref in re.findall(r'\[([^\]\n]+)\]\[([^\]\n]*)\]', text):
            if (ref or label).casefold() not in definitions:
                errors.append(f'{path}: undefined link reference {ref or label}')
        for target in targets:
            target = target.strip('<>')
            url = urlsplit(target)
            if url.scheme or url.netloc:
                continue
            target_path = unquote(url.path)
            if target_path.startswith('/'):
                errors.append(f'{path}: absolute local link {target}')
                continue
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), target_path)) if target_path else path
            if resolved not in files:
                prefix = resolved.rstrip('/') + '/'
                if not url.fragment and any(p.startswith(prefix) for p in files):
                    continue
                errors.append(f'{path}: target is not public and tracked: {target}')
            elif url.fragment and resolved.lower().endswith('.md'):
                if unquote(url.fragment) not in anchors(read_text(resolved)):
                    errors.append(f'{path}: missing anchor: {target}')
    return errors


def check_classification(entries, allow, excluded, *, scope):
    public = set(allow)
    private = set(excluded)
    if len(allow) != len(public) or public & private:
        raise SystemExit('duplicate or conflicting classifications')
    if scope == 'private':
        expected = public | private
    elif scope == 'public':
        expected = public
    else:
        raise ValueError(f'invalid source scope: {scope}')
    tracked = set(entries)
    if tracked != expected:
        raise SystemExit(f'{scope} source paths differ from exact classifications: '
                         f'missing={sorted(expected - tracked)}; unexpected={sorted(tracked - expected)}')
    for path in private:
        validate_path(path)
    for path in public:
        validate_path(path, public=True)
        if entries[path][0] not in ('100644', '100755') or entries[path][1] != 'blob':
            raise SystemExit(f'public path must be a regular tracked blob: {path}')
    return public


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision', default='HEAD')
    parser.add_argument('--scope', choices=('private', 'public'), default='private',
                        help='Require the full classified private tree (default) or exactly the public snapshot')
    args = parser.parse_args()
    commit = subprocess.check_output(['git', 'rev-parse', '--verify', args.revision + '^{commit}']).decode().strip()

    def read_text(path):
        return subprocess.check_output(['git', 'show', commit + ':' + path]).decode()

    entries = {}
    for record in subprocess.check_output(['git', 'ls-tree', '-rz', commit]).split(b'\0'):
        if record:
            meta, path = record.split(b'\t', 1)
            entries[path.decode()] = meta.decode().split()
    required = {'public-source.allowlist', 'public-source.exclusions.json'}
    if required - entries.keys():
        raise SystemExit(f'missing source policy files: {sorted(required - entries.keys())}')
    allow = read_text('public-source.allowlist').splitlines()
    excluded = load_exclusions(read_text('public-source.exclusions.json'))
    public = check_classification(entries, allow, excluded, scope=args.scope)
    errors = check_links(Path('.'), entries, public, read_text)
    if errors:
        raise SystemExit('\n'.join(errors))
    print(f'{commit}: {args.scope} policy and {sum(p.endswith(".md") for p in allow)} public Markdown files passed')


if __name__ == '__main__':
    main()

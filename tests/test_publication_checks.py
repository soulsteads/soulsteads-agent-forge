"""Small mutation tests for the offline publication gates."""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import check_public_source
import check_notices


class ClassificationTests(unittest.TestCase):
    def setUp(self):
        self.allow = ['README.md', 'public-source.allowlist', 'public-source.exclusions.json']
        self.excluded = {'internal/report.md': 'Maintainer report'}
        self.public = {path: ('100644', 'blob') for path in self.allow}
        self.private = {**self.public, 'internal/report.md': ('100644', 'blob')}

    def test_private_scope_requires_every_classified_path(self):
        check = check_public_source.check_classification
        self.assertEqual(check(self.private, self.allow, self.excluded, scope='private'), set(self.allow))
        with self.assertRaisesRegex(SystemExit, 'missing=.*internal/report.md'):
            check(self.public, self.allow, self.excluded, scope='private')
        with self.assertRaisesRegex(SystemExit, 'unexpected=.*draft.md'):
            check({**self.private, 'draft.md': ('100644', 'blob')}, self.allow, self.excluded, scope='private')

    def test_public_scope_accepts_only_safe_allowlisted_blobs(self):
        check = check_public_source.check_classification
        self.assertEqual(check(self.public, self.allow, self.excluded, scope='public'), set(self.allow))
        with self.assertRaisesRegex(SystemExit, 'unexpected=.*internal/report.md'):
            check(self.private, self.allow, self.excluded, scope='public')
        for path in ('internal/draft.md', '.serena/project.yml'):
            with self.subTest(path=path), self.assertRaises(SystemExit):
                check({**self.public, path: ('100644', 'blob')}, [*self.allow, path], self.excluded, scope='public')
        with self.assertRaisesRegex(SystemExit, 'regular tracked blob'):
            check({**self.public, 'README.md': ('120000', 'blob')}, self.allow, self.excluded, scope='public')

    def test_cli_never_infers_public_scope_or_skips_missing_policy(self):
        files = {
            'README.md': b'# Example\n',
            'public-source.allowlist': ('\n'.join(self.allow) + '\n').encode(),
            'public-source.exclusions.json': json.dumps(self.excluded).encode(),
        }

        def git(command):
            if command[1] == 'rev-parse':
                return b'fixture-revision\n'
            if command[1] == 'ls-tree':
                return b''.join(f'100644 blob fixture\t{path}\0'.encode() for path in files)
            return files[command[2].split(':', 1)[1]]

        with mock.patch.object(check_public_source.subprocess, 'check_output', side_effect=git):
            with mock.patch.object(sys, 'argv', ['check_public_source.py']):
                with self.assertRaisesRegex(SystemExit, 'private source paths differ'):
                    check_public_source.main()
            with mock.patch.object(sys, 'argv', ['check_public_source.py', '--scope', 'public']):
                with mock.patch('builtins.print') as output:
                    check_public_source.main()
                self.assertIn('public policy', output.call_args.args[0])
            del files['public-source.exclusions.json']
            for scope in ('private', 'public'):
                with mock.patch.object(sys, 'argv', ['check_public_source.py', '--scope', scope]):
                    with self.assertRaisesRegex(SystemExit, 'missing source policy files:.*exclusions.json'):
                        check_public_source.main()


class PublicationTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='forge-policy-test-', dir=os.environ['TMPDIR'])
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)

    def write(self, path, text):
        p = self.root / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def test_links_require_public_tracked_targets_and_anchors(self):
        self.write('README.md', '[guide](docs/guide.md#same-1)\n[ref][guide]\n\n[guide]: docs/guide.md#same\n\n`^[a-z][a-z0-9]*$` is a regex, not a link.\n')
        self.write('docs/guide.md', '# Same\n\n# Same\n')
        self.write('internal/private.md', '[missing](secret.md)\n')
        tracked = {'README.md', 'docs/guide.md', 'internal/private.md'}
        public = {'README.md', 'docs/guide.md'}
        self.assertEqual(check_public_source.check_links(self.root, tracked, public), [])
        self.write('README.md', '[private](internal/private.md)\n[bad anchor](docs/guide.md#missing)\n')
        self.assertEqual(len(check_public_source.check_links(self.root, tracked, public)), 2)
        self.write('README.md', '[untracked canary](canary.md)\n')
        self.write('canary.md', '# Canary\n')
        self.assertTrue(check_public_source.check_links(self.root, tracked, public | {'canary.md'}))

    def test_directory_links_require_public_tracked_contents(self):
        self.write('README.md', '[notices](licenses/demo/)\n')
        self.write('licenses/demo/LICENSE', 'fixture text')
        tracked = {'README.md', 'licenses/demo/LICENSE'}
        self.assertEqual(check_public_source.check_links(self.root, tracked, tracked), [])
        self.assertTrue(check_public_source.check_links(self.root, {'README.md'}, {'README.md'}))

    def test_notice_coverage_hashes_provenance_and_package_inclusion(self):
        self.write('Cargo.lock', '[[package]]\nname="demo"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\nchecksum="abc"\n')
        self.write('Cargo.toml', '[package]\ninclude=["THIRD_PARTY_NOTICES.md", "licenses/**"]\n')
        self.write('THIRD_PARTY_NOTICES.md', '| demo-1.0.0 | MIT | [texts](licenses/demo-1.0.0/) |\n')
        self.write('licenses/demo-1.0.0/LICENSE', 'upstream fixture bytes\n')
        row = {'path': 'licenses/demo-1.0.0/LICENSE', 'sha256': hashlib.sha256(b'upstream fixture bytes\n').hexdigest(),
               'source': 'crates.io:demo-1.0.0/LICENSE', 'crate_sha256': 'abc'}
        self.write('licenses/provenance.json', json.dumps([row]))
        self.assertEqual(check_notices.check(self.root), [])
        self.write('licenses/demo-1.0.0/LICENSE', 'changed attribution\n')
        self.assertTrue(check_notices.check(self.root))

        self.write('licenses/demo-1.0.0/LICENSE', 'upstream fixture bytes\n')
        self.write('THIRD_PARTY_NOTICES.md', '')
        self.assertTrue(check_notices.check(self.root))
        self.write('THIRD_PARTY_NOTICES.md', '| demo-1.0.0 | MIT | [texts](licenses/demo-1.0.0/) |\n')
        self.write('Cargo.toml', '[package]\ninclude=["src/**"]\n')
        self.assertTrue(check_notices.check(self.root))
        self.write('Cargo.toml', '[package]\ninclude=["THIRD_PARTY_NOTICES.md", "licenses/**"]\n')
        row['source'] = ''
        self.write('licenses/provenance.json', json.dumps([row]))
        self.assertTrue(check_notices.check(self.root))

    def test_package_list_must_carry_every_retained_license(self):
        self.write('licenses/provenance.json', json.dumps([{'path': 'licenses/demo/LICENSE'}]))
        expected = {'LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md', 'licenses/provenance.json', 'licenses/demo/LICENSE'}
        self.assertEqual(check_notices.check_package_list(self.root, expected), [])
        self.assertTrue(check_notices.check_package_list(self.root, expected - {'licenses/demo/LICENSE'}))
        self.assertTrue(check_notices.check_package_list(self.root, expected | {'internal/private.md'}))


if __name__ == '__main__':
    unittest.main()

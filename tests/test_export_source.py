"""Offline exporter regressions. All Git writes are to disposable fixture repos."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


EXPORTER = Path(__file__).resolve().parents[1] / 'scripts/export_source.py'
spec = importlib.util.spec_from_file_location('export_source', EXPORTER)
export_source = importlib.util.module_from_spec(spec)
spec.loader.exec_module(export_source)


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='forge-export-test-', dir=os.environ['TMPDIR'])
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.repo = self.base / 'repo'
        self.repo.mkdir()
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME='Export Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                        GIT_COMMITTER_NAME='Export Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
        self.git('init', '-q')
        self.write('README.md', b'committed public bytes\n')
        self.write('run.sh', b'#!/bin/sh\nexit 0\n')
        (self.repo / 'run.sh').chmod(0o755)
        self.write('private.txt', b'PRIVATE_CANARY\n')
        self.policy(['README.md', 'run.sh'], {'private.txt': 'Private test fixture'})
        self.commit()

    def git(self, *args, data=None):
        return subprocess.check_output(['git', *args], cwd=self.repo, env=self.env, input=data)

    def write(self, path, data):
        p = self.repo / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def policy(self, allow, exclude):
        self.allow = ['public-source.allowlist', 'public-source.exclusions.json', *allow]
        self.write('public-source.allowlist', ('\n'.join(self.allow) + '\n').encode())
        self.write('public-source.exclusions.json', (json.dumps(exclude) + '\n').encode())

    def commit(self):
        self.git('add', '--all')
        self.git('commit', '-qm', 'fixture')
        return self.git('rev-parse', 'HEAD').decode().strip()

    def export(self, destination='export', revision='HEAD'):
        return subprocess.run([sys.executable, str(EXPORTER), revision, str(self.base / destination)],
                              cwd=self.repo, env=self.env, capture_output=True, text=True)

    def refused(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_43_unclassified_tracked_paths_are_blocked(self):
        for n in range(43):
            self.write(f'new-{n:02}.txt', b'unclassified\n')
        self.commit()
        result = self.export()
        self.refused(result)
        self.assertIn('unclassified (43)', result.stderr)
        self.assertFalse((self.base / 'export').exists())

    def test_policy_is_sourced_from_pinned_revision(self):
        pinned = self.git('rev-parse', 'HEAD').decode().strip()
        self.policy(['private.txt'], {'README.md': 'exclude', 'run.sh': 'exclude'})
        self.commit()
        (self.repo / 'public-source.allowlist').write_text('malformed working policy\n')
        result = self.export(revision=pinned)
        self.assertEqual(result.returncode, 0, result.stderr)
        with tarfile.open(self.base / 'export/forge-source.tar') as tar:
            self.assertIn('forge-source/README.md', tar.getnames())
            self.assertNotIn('forge-source/private.txt', tar.getnames())

    def test_dirty_untracked_and_denied_canaries_never_exported(self):
        self.write('README.md', b'DIRTY_CANARY')
        self.write('untracked.txt', b'UNTRACKED_CANARY')
        self.write('.git/local-canary', b'GIT_CANARY')
        self.assertEqual(self.export().returncode, 0)
        archive = (self.base / 'export/forge-source.tar').read_bytes()
        for canary in (b'DIRTY_CANARY', b'UNTRACKED_CANARY', b'PRIVATE_CANARY', b'GIT_CANARY'):
            self.assertNotIn(canary, archive)

    def test_deterministic_archive_modes_and_manifest_match_actual_bytes(self):
        old_umask = os.umask(0o077)
        try:
            self.assertEqual(self.export('first').returncode, 0)
            os.umask(0o022)
            self.assertEqual(self.export('second').returncode, 0)
        finally:
            os.umask(old_umask)
        for name in ('forge-source.tar', 'manifest.json', 'ALLOWLIST'):
            self.assertEqual((self.base / 'first' / name).read_bytes(),
                             (self.base / 'second' / name).read_bytes())
        manifest = json.loads((self.base / 'first/manifest.json').read_text())
        archive = self.base / 'first/forge-source.tar'
        self.assertEqual(manifest['archive_sha256'], hashlib.sha256(archive.read_bytes()).hexdigest())
        self.assertEqual(manifest['revision'], self.git('rev-parse', 'HEAD').decode().strip())
        self.assertEqual(manifest['tree'], self.git('rev-parse', 'HEAD^{tree}').decode().strip())
        with tarfile.open(archive) as tar:
            self.assertEqual(tar.getnames(), ['forge-source/' + p for p in sorted(self.allow)])
            self.assertEqual(len(manifest['files']), len(tar.getmembers()))
            for row, member in zip(manifest['files'], tar.getmembers()):
                raw = tar.extractfile(member).read()
                self.assertEqual(row['path'], member.name.removeprefix('forge-source/'))
                self.assertEqual(row['sha256'], hashlib.sha256(raw).hexdigest())
                self.assertEqual(row['size'], len(raw))
                self.assertEqual(row['git_blob'], self.git('hash-object', '--stdin', data=raw).decode().strip())
                self.assertEqual(member.mode, 0o755 if row['path'] == 'run.sh' else 0o644)
                self.assertEqual(int(row['mode'], 8) & 0o777, member.mode)
                self.assertEqual((member.uid, member.gid, member.mtime, member.uname, member.gname), (0, 0, 0, '', ''))

    def test_internal_boundary_overrides_allowlist(self):
        self.write('internal/checklist.md', b'INTERNAL_CANARY')
        self.policy(['README.md', 'run.sh', 'internal/checklist.md'], {'private.txt': 'Private'})
        self.commit()
        self.refused(self.export())
        self.assertFalse((self.base / 'export').exists())

    def test_internal_can_be_explicitly_excluded(self):
        self.write('internal/checklist.md', b'INTERNAL_CANARY')
        self.policy(['README.md', 'run.sh'], {'private.txt': 'Private', 'internal/checklist.md': 'Maintainer only'})
        self.commit()
        self.assertEqual(self.export().returncode, 0)
        self.assertNotIn(b'INTERNAL_CANARY', (self.base / 'export/forge-source.tar').read_bytes())

    def test_unsafe_and_git_paths_are_rejected(self):
        for path in ('', '.', './file', '/absolute', '../escape', 'a/../b', 'a//b',
                     'a/', 'a\\b', 'C:/drive', '.git/config', 'a/.git/config',
                     '.GIT/config', 'line\nbreak', 'tab\tname', 'internal/plan.md'):
            with self.subTest(path=path), self.assertRaises(SystemExit):
                export_source.validate_path(path, public=True)

    def test_symlinks_and_gitlinks_are_rejected(self):
        (self.repo / 'README.md').unlink()
        (self.repo / 'README.md').symlink_to('private.txt')
        self.commit()
        self.refused(self.export())
        oid = self.git('rev-parse', 'HEAD').decode().strip()
        self.git('update-index', '--cacheinfo', f'160000,{oid},README.md')
        self.git('commit', '-qm', 'gitlink')
        self.refused(self.export())

    def test_unsupported_regular_mode_is_rejected_before_output(self):
        def git(*args):
            raw = self.git(*args)
            return raw.replace(b'100644 blob', b'100664 blob', 1) if args[0] == 'ls-tree' else raw

        with mock.patch.object(export_source, 'git', side_effect=git), mock.patch.object(
                sys, 'argv', [str(EXPORTER), 'HEAD', str(self.base / 'export')]):
            with self.assertRaisesRegex(SystemExit, 'only regular tracked source files'):
                export_source.main()
        self.assertFalse((self.base / 'export').exists())

    def test_duplicate_conflicting_stale_and_reasonless_policy_is_rejected(self):
        policies = [(['README.md', 'run.sh', 'README.md'], {'private.txt': 'Private'}),
                    (['README.md', 'run.sh'], {'private.txt': 'Private', 'README.md': 'Conflict'}),
                    (['README.md', 'run.sh', 'missing'], {'private.txt': 'Private'}),
                    (['README.md', 'run.sh'], {'private.txt': ''})]
        for allow, excluded in policies:
            with self.subTest(allow=allow, excluded=excluded):
                self.policy(allow, excluded)
                self.commit()
                self.refused(self.export(destination='policy-' + self.git('rev-parse', 'HEAD').decode().strip()))

    def test_duplicate_exclusion_keys_are_rejected(self):
        self.write('public-source.exclusions.json', b'{"private.txt":"First reason","private.txt":"Second reason"}\n')
        self.commit()
        self.refused(self.export())
        self.assertFalse((self.base / 'export').exists())

    def test_existing_destinations_and_symlinks_are_never_clobbered(self):
        (self.base / 'directory').mkdir()
        (self.base / 'file').write_bytes(b'KEEP')
        (self.base / 'link').symlink_to(self.base / 'absent', target_is_directory=True)
        (self.base / 'parent-link').symlink_to(self.base / 'directory', target_is_directory=True)
        for path in ('directory', 'file', 'link', 'parent-link/child'):
            with self.subTest(path=path):
                self.refused(self.export(path))
        self.assertEqual((self.base / 'file').read_bytes(), b'KEEP')
        self.assertFalse((self.base / 'absent').exists())
        self.assertEqual(list((self.base / 'directory').iterdir()), [])

    def test_destination_inside_checkout_is_rejected(self):
        self.refused(self.export('repo/output'))
        self.assertFalse((self.repo / 'output').exists())


if __name__ == '__main__':
    unittest.main()

"""Exercise the release workflow's actual validation blocks, without network writes."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / '.github/workflows/release.yml'
ARCHIVE = 'soulsteads-agent-forge-linux-x86_64.tar.gz'


@unittest.skipUnless(WORKFLOW.is_file(), 'requires the source checkout release workflow')
class ReleaseAssetsTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.dist = self.root / 'dist'
        self.dist.mkdir()
        self.archive = self.dist / ARCHIVE
        self.archive.write_bytes(b'fixture archive')
        self.manifest = self.dist / 'SHA256SUMS'
        self.record = f'{hashlib.sha256(self.archive.read_bytes()).hexdigest()}  {ARCHIVE}\n'
        self.manifest.write_text(self.record)
        self.calls = self.root / 'calls'
        self.calls.touch()
        stubs = self.root / 'bin'
        stubs.mkdir()
        self.env = {'PATH': f'{stubs}{os.pathsep}{os.defpath}', 'HOME': str(self.root),
                    'RELEASE_ID': '1', 'RELEASE_TAG': 'fixture', 'CALLS': str(self.calls),
                    'GITHUB_REPOSITORY': 'example/forge', 'GITHUB_SHA': 'a' * 40}
        for name, script in {
            'gh': '''
                printf 'api\\n' >> "$CALLS"
                case "$*" in
                  'api repos/example/forge/releases/1') printf '%s\\n' '{"tag_name":"fixture","draft":false}' ;;
                  'api --paginate --slurp repos/example/forge/releases/1/assets?per_page=100') printf '%s\\n' '[[]]' ;;
                  *) exit 2 ;;
                esac
            ''',
            'git': '''
                [[ "$1" == ls-remote ]]
                printf '%s\\trefs/tags/%s\\n' "$GITHUB_SHA" "$RELEASE_TAG"
            ''',
            'curl': '''
                url=${!#}
                name=${url##*?name=}
                printf '%s\\n' "$name" >> "$CALLS"
                digest=$(sha256sum -- "$name" | cut -d ' ' -f 1)
                jq -n --arg name "$name" --arg digest "sha256:$digest" \\
                  '{name:$name,digest:$digest,state:"uploaded"}'
            ''',
        }.items():
            command = stubs / name
            command.write_text('#!/bin/bash\nset -euo pipefail\n' + textwrap.dedent(script))
            command.chmod(0o700)

    def publish(self):
        step = WORKFLOW.read_text().split('      - name: Verify and upload assets without replacing existing files\n', 1)[1]
        script = textwrap.dedent(step.split('        run: |\n', 1)[1])
        return subprocess.run(['bash', '--noprofile', '--norc', '-eo', 'pipefail', '-c', script],
                              cwd=self.dist, env=self.env, capture_output=True, text=True, timeout=10)

    def assert_rejected(self):
        result = self.publish()
        self.assertEqual(self.calls.read_text(), '', result.stderr)
        self.assertNotEqual(result.returncode, 0)

    def test_valid_handoff_uploads_only_the_two_expected_names(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ['api', 'api', ARCHIVE, 'SHA256SUMS'])

    def test_unlisted_file_rejected(self):
        (self.dist / 'unlisted').touch()
        self.assert_rejected()

    def test_hidden_file_rejected(self):
        (self.dist / '.unlisted').touch()
        self.assert_rejected()

    def test_missing_entry_rejected(self):
        self.manifest.write_text('')
        self.assert_rejected()

    def test_duplicate_entry_rejected(self):
        self.manifest.write_text(self.record * 2)
        self.assert_rejected()

    def test_entry_for_manifest_alternate_archive_path_rejected(self):
        self.manifest.write_text(self.record.replace(ARCHIVE, './' + ARCHIVE))
        self.assert_rejected()

    def test_symlinked_asset_rejected(self):
        original = self.archive.rename(self.root / ARCHIVE)
        self.archive.symlink_to(original)
        self.assert_rejected()


@unittest.skipUnless(WORKFLOW.is_file(), 'requires the source checkout release workflow')
class ReleasePackagingTests(unittest.TestCase):
    def test_build_and_smoke_use_the_renamed_executable(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('--bin soulsteads-forge\n', workflow)
        self.assertIn('/release/soulsteads-forge" "$RUNNER_TEMP/$bundle/soulsteads-forge"', workflow)
        self.assertIn('"$RUNNER_TEMP/$bundle/soulsteads-forge" --help', workflow)

    def test_pr_package_job_is_not_path_filtered(self):
        trigger = WORKFLOW.read_text().split('\non:\n', 1)[1].split('\npermissions:\n', 1)[0]
        self.assertRegex(trigger, r'(?m)^  pull_request:$(?!\n(?:[ \t]*\n)* {4}\S)')

    def test_notice_selection_uses_build_tree_not_extra_metadata_packages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / 'bundle'
            bundle.mkdir()
            fallbacks = root / 'scripts/release-notices'
            fallbacks.mkdir(parents=True)
            (fallbacks / 'upstream.json').write_text('{}')
            packages = []
            for name in ('fixture', 'selected', 'metadata-only'):
                crate = root / name
                crate.mkdir()
                packages.append({'id': name, 'name': name, 'version': '1.0.0', 'license': 'MIT',
                                 'license_file': None, 'manifest_path': str(crate / 'Cargo.toml')})
            (root / 'selected/LICENSE').write_bytes(b'Synthetic notice\r\n')
            metadata = root / 'metadata.json'
            metadata.write_text(json.dumps({'packages': packages, 'resolve': {'root': 'fixture', 'nodes': [
                {'id': 'fixture', 'deps': [{'pkg': name, 'dep_kinds': [{'kind': None}]}
                                           for name in ('selected', 'metadata-only')]},
                {'id': 'selected', 'deps': []}, {'id': 'metadata-only', 'deps': []},
            ]}}))
            tree = root / 'tree.txt'
            tree.write_text('fixture v1.0.0 (fixture)\nselected v1.0.0 (proc-macro)\nselected v1.0.0 (*)\n')
            script = textwrap.dedent(WORKFLOW.read_text().split("<<'PY'\n", 1)[1].split('\n          PY\n', 1)[0])
            result = subprocess.run([sys.executable, '-B', '-c', script, metadata, bundle, tree],
                                    cwd=root, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            notices = bundle / 'dependency-notices'
            self.assertEqual((notices / 'selected-1.0.0/LICENSE').read_bytes(), b'Synthetic notice\r\n')
            self.assertEqual((notices / 'INDEX.tsv').read_text().splitlines()[1:],
                             ['selected-1.0.0\tMIT\tLICENSE'])

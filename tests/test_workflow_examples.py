import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

EXAMPLES = Path(__file__).resolve().parents[1] / 'examples'
EXAMPLE = EXAMPLES / 'rust-pr-review'


class ImportBundleTests(unittest.TestCase):
    def test_examples_import_one_manual_workflow_each(self):
        for work_id in ('release-readiness', 'rust-pr-review'):
            with self.subTest(work=work_id):
                path = EXAMPLES / work_id / 'import.json'
                self.assertTrue(path.is_file(), 'missing import bundle')
                bundle = json.loads(path.read_text(encoding='utf-8'))
                self.assertEqual(bundle['format'], 'forge-definitions/v1')
                self.assertEqual(bundle.get('schedules', []), [])
                self.assertEqual(len(bundle['work']), 1)
                entry, = bundle['work']
                self.assertEqual(entry['id'], work_id)
                self.assertIs(entry['enabled'], True)
                self.assertEqual(entry['definition']['type'], 'workflow')


class ReviewPacketTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.name', 'Workflow fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'commit.gpgSign', 'false')
        self.base = self.commit('pub fn count() -> i32 {\n    5\n}\n')
        self.head = self.commit('pub fn count() -> i32 {\n    -5\n}\n')
        # A dirty working tree is not review evidence.
        (self.repo / 'lib.rs').write_text('outside reviewed object\n')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], text=True).strip()

    def commit(self, content):
        (self.repo / 'lib.rs').write_text(content)
        self.git('add', 'lib.rs')
        self.git('commit', '-qm', 'Fixture revision')
        return self.git('rev-parse', 'HEAD')

    def helper(self, *args):
        return subprocess.run([sys.executable, str(EXAMPLE / 'review_packet.py'), *map(str, args)],
                              capture_output=True, text=True, timeout=30)

    def prepare(self, head=None, repo=None):
        return self.helper('prepare', '--repo', repo or self.repo, '--project', 'fixture/demo', '--pr', '7',
                           '--base', self.base, '--head', head or self.head)

    def test_prepares_pinned_git_diff_not_checkout_content(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        packet = json.loads(result.stdout)
        self.assertEqual(packet['source']['head'], self.head)
        self.assertEqual(packet['source']['merge_base'], self.base)
        self.assertIn('+    -5', packet['diff'])
        self.assertNotIn('outside reviewed object', packet['diff'])
        self.assertEqual(packet['source']['diff_sha256'], hashlib.sha256(packet['diff'].encode()).hexdigest())
        self.assertIn('[right lib.rs:2] +    -5', packet['review_diff'])
        self.assertIn('[left lib.rs:2] -    5', packet['review_diff'])

    def test_refuses_symbolic_revision_and_oversize_diff(self):
        result = self.prepare('HEAD')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, '')
        self.assertIn('full commit', result.stderr)
        big = self.commit('x' * (33 * 1024) + '\n')
        result = self.prepare(big)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, '')
        self.assertIn('exceeds', result.stderr)

    def test_subdirectory_clone_path_preserves_whole_repository_evidence(self):
        subdir = self.repo / 'nested'
        subdir.mkdir()
        (subdir / 'work.rs').write_text('fn exercise() {}\n')
        self.git('add', '--', 'nested/work.rs')
        self.git('commit', '-qm', 'Add nested example')
        head = self.git('rev-parse', 'HEAD')
        full = self.prepare(head)
        self.assertEqual(full.returncode, 0, full.stderr)
        self.git('config', 'diff.relative', 'true')
        nested = self.prepare(head, repo=subdir)
        self.assertEqual(nested.returncode, 0, nested.stderr)
        self.assertEqual(nested.stdout, full.stdout)

    def test_ignores_uncommitted_attributes(self):
        initial = self.prepare()
        self.assertEqual(initial.returncode, 0, initial.stderr)
        (self.repo / '.gitattributes').write_text('lib.rs -diff\n')
        self.git('add', '.gitattributes')
        after = self.prepare()
        self.assertEqual(after.returncode, 0, after.stderr)
        self.assertEqual(after.stdout, initial.stdout)

    def test_refuses_gitlink_change(self):
        self.git('update-index', '--add', '--cacheinfo', f'160000,{self.head},dependency')
        self.git('commit', '-qm', 'Record dependency revision')
        result = self.prepare(self.git('rev-parse', 'HEAD'))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, '')
        self.assertIn('submodule', result.stderr)

    def test_refuses_oversize_numbered_evidence(self):
        path = self.repo / ('x' * 120 + '.rs')
        path.write_text('x\n' * 500)
        self.git('add', '--', path.name)
        self.git('commit', '-qm', 'Add many short lines')
        result = self.prepare(self.git('rev-parse', 'HEAD'))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, '')
        self.assertIn('numbered review evidence', result.stderr)

    def test_prepare_and_verify_use_the_same_serialized_packet_limit(self):
        params, run = self.root / 'params.json', self.root / 'run.json'
        for repeat, accepted in ((21 * 1024, True), (22 * 1024, False)):
            with self.subTest(repeat=repeat):
                # JSON escaping expands this text in both evidence fields,
                # even though the raw and numbered diff are within their limits.
                head = self.commit('// ' + chr(31) * repeat + '\n')
                prepared = self.prepare(head)
                if not accepted:
                    self.assertNotEqual(prepared.returncode, 0)
                    self.assertEqual(prepared.stdout, '')
                    self.assertIn('parameter JSON exceeds', prepared.stderr)
                    continue
                self.assertEqual(prepared.returncode, 0, prepared.stderr)
                params.write_text(prepared.stdout, encoding='utf-8')
                packet = json.loads(prepared.stdout)
                output = {'source': packet['source'], 'review': {'findings': []}}
                run.write_text(json.dumps({'run_id': 'fixture', 'type': 'workflow', 'status': 'SUCCESS',
                                           'output': {'output': output}}), encoding='utf-8')
                verified = self.helper('verify', params, run)
                self.assertEqual(verified.returncode, 0, verified.stderr)
                self.assertEqual(json.loads(verified.stdout)['source']['head'], head)

    def test_verifier_rejects_context_line_and_mismatched_head(self):
        prepared = self.prepare()
        self.assertEqual(prepared.returncode, 0, prepared.stderr)
        packet = json.loads(prepared.stdout)
        params, run = self.root / 'params.json', self.root / 'run.json'
        params.write_text(prepared.stdout)
        finding = {'path': 'lib.rs', 'side': 'right', 'line': 2, 'quote': '-5'}
        output = {'source': packet['source'].copy(), 'review': {'findings': [finding]}}
        record = {'run_id': 'fixture', 'type': 'workflow', 'status': 'SUCCESS', 'output': {'output': output}}

        def check():
            run.write_text(json.dumps(record))
            return self.helper('verify', params, run)

        verified = check()
        self.assertEqual(verified.returncode, 0, verified.stderr)
        self.assertEqual(json.loads(verified.stdout)['checked_findings'], 1)
        finding.update(quote='-5 ')
        self.assertNotEqual(check().returncode, 0)
        finding.update(side='left', quote='5')
        self.assertEqual(check().returncode, 0)
        finding.update(side='right', line=1, quote='count')
        refused = check()
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('changed line', refused.stderr)
        finding.update(line=2, quote='-5')
        output['source']['head'] = self.base
        refused = check()
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('source', refused.stderr)

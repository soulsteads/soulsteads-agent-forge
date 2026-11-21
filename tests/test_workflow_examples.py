import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from urllib.parse import urlsplit

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


REPORT_EXAMPLE = EXAMPLES / 'release-readiness-report'
REPORT_COLLECTOR = REPORT_EXAMPLE / 'collector.py'
REPORT_INPUT = {
    'mode': 'fixture',
    'fixture_id': 'ready',
    'repository': 'forge-fixture/release-readiness',
    'revision': '1' * 40,
    'tag': 'v1.2.3',
    'required_checks': ['build', 'test'],
    'required_artifacts': ['dist.tar.gz', 'SHA256SUMS'],
    'observed_at': '2026-10-09T10:00:00Z',
}


class PinnedReleaseReadinessReportTests(unittest.TestCase):
    def invoke(self, operation, input_value=None):
        request = {'api_version': 1, 'operation': operation}
        if input_value is not None:
            request['input'] = input_value
        result = subprocess.run(
            [sys.executable, str(REPORT_COLLECTOR)],
            input=json.dumps(request), text=True, capture_output=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def collect(self, fixture_id):
        value = dict(REPORT_INPUT, fixture_id=fixture_id)
        return self.invoke('execute', value)['output']

    def test_example_is_a_separate_supported_plugin_workflow(self):
        self.assertTrue(REPORT_COLLECTOR.is_file())
        self.assertTrue((REPORT_EXAMPLE / 'README.md').is_file())
        bundle = json.loads((REPORT_EXAMPLE / 'import.json').read_text())
        self.assertEqual(bundle['format'], 'forge-definitions/v1')
        work = bundle['work']
        self.assertEqual(len(work), 1)
        self.assertEqual(work[0]['id'], 'release-readiness-report')
        steps = work[0]['definition']['input']['steps']
        self.assertEqual(steps[0]['action']['type'], 'plugin.release-readiness-report')
        self.assertEqual(steps[0]['action']['input']['mode'], {'$ref': '/params/mode'})
        self.assertEqual(steps[0]['action']['input']['fixture_id'], {'$ref': '/params/fixture_id'})
        self.assertEqual(work[0]['definition']['input']['params']['mode'], 'fixture')
        self.assertEqual(work[0]['definition']['input']['params']['fixture_id'], 'ready')
        self.assertEqual(json.loads((EXAMPLES / 'release-readiness/import.json').read_text())['work'][0]['id'], 'release-readiness')

    def test_empty_required_artifacts_is_rejected_for_fixture_configuration(self):
        configured = dict(REPORT_INPUT, required_artifacts=[])
        response = self.invoke('execute', configured)
        self.assertEqual(response['api_version'], 1)
        self.assertEqual(response.get('error', {}).get('code'), 'invalid_required_artifacts')
        self.assertNotIn('output', response)

        description = self.invoke('describe')
        self.assertEqual(
            description['schema']['properties']['required_artifacts']['minItems'], 1
        )

    def test_stdin_plugin_protocol_and_ready_report_digest(self):
        description = self.invoke('describe')
        self.assertIn('description', description)
        self.assertIn('schema', description)
        self.assertEqual(self.invoke('close')['output'], {})
        envelope = self.collect('ready')
        report = envelope['report']
        self.assertEqual(set(report), {
            'format', 'repository', 'revision', 'tag', 'required_checks',
            'artifact_policy', 'observed_at', 'criteria', 'evidence_references',
            'collection_status', 'verdict',
        })
        self.assertEqual(report['verdict'], 'ready')
        self.assertEqual(report['collection_status'], 'complete')
        self.assertEqual(report['repository'], REPORT_INPUT['repository'])
        self.assertEqual(report['revision'], REPORT_INPUT['revision'])
        self.assertEqual(report['required_checks'], ['build', 'test'])
        self.assertEqual(report['artifact_policy']['required_names'], ['dist.tar.gz', 'SHA256SUMS'])
        self.assertEqual(report['artifact_policy']['match'], 'exact GitHub release asset name')
        self.assertEqual(report['artifact_policy']['required_asset_state'], 'uploaded')
        self.assertEqual(report['artifact_policy']['minimum_size_bytes'], 1)
        self.assertFalse(report['artifact_policy']['content_bytes_downloaded'])
        self.assertEqual(report['observed_at'], REPORT_INPUT['observed_at'])
        self.assertIn('criteria', report)
        refs = report['evidence_references']
        self.assertTrue(refs)
        self.assertTrue(all(ref['revision'] == REPORT_INPUT['revision'] for ref in refs))
        tag_ref = next(ref for ref in refs if ref['kind'] == 'github_tag_ref')
        self.assertTrue(tag_ref['verified'])
        self.assertEqual(tag_ref['object_chain'], ['2' * 40, REPORT_INPUT['revision']])
        self.assertTrue(any('/git/tags/' in path for path in tag_ref['api_paths']))
        release_ref = next(ref for ref in refs if ref['kind'] == 'github_release')
        self.assertTrue(any('/releases/' in path and '/assets?' in path for path in release_ref['api_paths']))
        receipt = envelope['receipt']
        report_bytes = receipt['report_bytes_utf8'].encode('utf-8')
        self.assertEqual(json.loads(report_bytes), report)
        self.assertEqual(receipt['report_sha256'], hashlib.sha256(report_bytes).hexdigest())
        self.assertEqual(receipt['report_byte_length'], len(report_bytes))
        self.assertNotIn('report_sha256', report)

    def test_pending_and_failed_required_checks_have_distinct_verdicts(self):
        pending = self.collect('pending')
        self.assertEqual(pending['report']['collection_status'], 'complete')
        self.assertEqual(pending['report']['verdict'], 'incomplete')
        self.assertEqual(pending['report']['criteria']['checks']['pending'], ['test'])
        failed = self.collect('failed')
        self.assertEqual(failed['report']['collection_status'], 'complete')
        self.assertEqual(failed['report']['verdict'], 'not_ready')
        self.assertEqual(failed['report']['criteria']['checks']['failed'], ['test'])

    def test_untagged_and_tag_without_downloadable_assets_are_distinct(self):
        untagged = self.collect('no-tag')['report']
        no_assets = self.collect('no-assets')['report']
        self.assertEqual(untagged['verdict'], 'not_ready')
        self.assertEqual(untagged['tag']['state'], 'absent')
        self.assertEqual(no_assets['verdict'], 'not_ready')
        self.assertEqual(no_assets['tag']['state'], 'present')
        self.assertEqual(no_assets['criteria']['artifacts']['missing'], ['dist.tar.gz', 'SHA256SUMS'])

    def test_unavailable_partial_mismatched_and_unverified_evidence_are_incomplete(self):
        for fixture_id, status in (('unavailable', 'unavailable'), ('partial', 'partial'),
                                   ('mismatched', 'partial'), ('tag-mismatched', 'partial'),
                                   ('unverified', 'partial')):
            with self.subTest(fixture=fixture_id):
                report = self.collect(fixture_id)['report']
                self.assertEqual(report['collection_status'], status)
                self.assertEqual(report['verdict'], 'incomplete')
                if fixture_id == 'mismatched':
                    self.assertNotIn('test', report['criteria']['checks']['passed'])
                    self.assertIn('test', report['criteria']['checks']['unverified'])
                    wrong = next(ref for ref in report['evidence_references']
                                 if ref['kind'] == 'github_check_run' and ref['name'] == 'test')
                    self.assertEqual(wrong['revision'], REPORT_INPUT['revision'])
                    self.assertEqual(wrong['observed_revision'], '3' * 40)
                    self.assertFalse(wrong['verified'])
                if fixture_id == 'tag-mismatched':
                    self.assertEqual(report['tag']['state'], 'mismatch')
                    wrong_tag = next(ref for ref in report['evidence_references']
                                     if ref['kind'] == 'github_tag_ref')
                    self.assertEqual(wrong_tag['revision'], REPORT_INPUT['revision'])
                    self.assertEqual(wrong_tag['target_revision'], '3' * 40)
                    self.assertFalse(wrong_tag['verified'])

    def test_invalid_revision_is_rejected_before_collection(self):
        value = dict(REPORT_INPUT, revision='1' * 39)
        response = self.invoke('execute', value)
        self.assertIn('error', response)

    def test_live_mode_requires_one_exact_allowlisted_repository_without_network(self):
        module = self.load_collector_module()
        value = dict(REPORT_INPUT, mode='live')
        value.pop('fixture_id')
        value.pop('observed_at')
        request = {'api_version': 1, 'operation': 'execute', 'input': value}
        env = os.environ.copy()
        env.pop(module.ALLOWLIST_ENV, None)
        result = subprocess.run(
            [sys.executable, str(REPORT_COLLECTOR)], input=json.dumps(request),
            text=True, capture_output=True, env=env, check=True,
        )
        response = json.loads(result.stdout)
        self.assertEqual(response['error']['code'], 'live_repository_allowlist_not_configured')
        env[module.ALLOWLIST_ENV] = 'different-owner/public-repo'
        result = subprocess.run(
            [sys.executable, str(REPORT_COLLECTOR)], input=json.dumps(request),
            text=True, capture_output=True, env=env, check=True,
        )
        self.assertEqual(json.loads(result.stdout)['error']['code'], 'repository_not_allowlisted')

    @staticmethod
    def load_collector_module():
        spec = importlib.util.spec_from_file_location('release_readiness_collector', REPORT_COLLECTOR)
        if spec is None or spec.loader is None:
            raise AssertionError('collector module spec is unavailable')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_live_reader_enforces_overall_deadline_and_response_byte_cap(self):
        module = self.load_collector_module()
        class FakeResponse:
            def __init__(self, url, body): self.url, self.body = url, body
            def __enter__(self): return self
            def __exit__(self, *args): return False
            def geturl(self): return self.url
            def read(self, size=-1):
                offset = getattr(self, 'offset', 0)
                if size < 0:
                    size = len(self.body) - offset
                data = self.body[offset:offset + size]
                self.offset = offset + len(data)
                return data
        class FakeOpener:
            def __init__(self, body): self.body, self.calls = body, 0
            def open(self, request, timeout):
                self.calls += 1
                return FakeResponse(request.full_url, self.body)
        path = f"/repos/{REPORT_INPUT['repository']}/releases/tags/v1.2.3"
        reader = module.GitHubReader(REPORT_INPUT['repository'])
        timed_out = FakeOpener(b'{}')
        reader.opener = timed_out
        reader.deadline = module.time.monotonic() - 1
        with self.assertRaises(module.ApiUnavailable) as expired:
            reader.get_json(path)
        self.assertEqual(expired.exception.code, 'overall_deadline')
        self.assertEqual(timed_out.calls, 0)
        reader = module.GitHubReader(REPORT_INPUT['repository'])
        oversized = FakeOpener(b'x' * (module.MAX_RESPONSE_BYTES + 1))
        reader.opener = oversized
        with self.assertRaises(module.ApiUnavailable) as clipped:
            reader.get_json(path)
        self.assertEqual(clipped.exception.code, 'response_too_large')

    def test_live_reader_paginates_with_commit_bound_fixed_api_urls(self):
        module = self.load_collector_module()
        class FakeResponse:
            def __init__(self, url, data):
                self.url = url
                self.body = json.dumps(data).encode('utf-8')
            def __enter__(self): return self
            def __exit__(self, *args): return False
            def geturl(self): return self.url
            def read(self, size=-1):
                offset = getattr(self, 'offset', 0)
                if size < 0:
                    size = len(self.body) - offset
                data = self.body[offset:offset + size]
                self.offset = offset + len(data)
                return data
        class FakeOpener:
            def __init__(self, pages):
                self.pages = pages
                self.urls = []
                self.methods = []
                self.timeouts = []
            def open(self, request, timeout):
                self.urls.append(request.full_url)
                self.methods.append(request.get_method())
                self.timeouts.append(timeout)
                key = urlsplit(request.full_url).path + '?' + urlsplit(request.full_url).query
                return FakeResponse(request.full_url, self.pages[key])
        complete_pages = {}
        for page, count in ((1, 100), (2, 1)):
            path = f"/repos/{REPORT_INPUT['repository']}/commits/{REPORT_INPUT['revision']}/check-runs?per_page=100&page={page}"
            complete_pages[path] = {'total_count': 101, 'check_runs': [{'id': page * 100 + n} for n in range(count)]}
        complete_reader = module.GitHubReader(REPORT_INPUT['repository'])
        complete_fake = FakeOpener(complete_pages)
        complete_reader.opener = complete_fake
        complete_result = complete_reader.read_checks(REPORT_INPUT['revision'])
        self.assertEqual(complete_result['state'], 'complete')
        self.assertEqual(len(complete_result['check_runs']), 101)
        self.assertEqual(len(complete_fake.urls), 2)

        reader = module.GitHubReader(REPORT_INPUT['repository'])
        pages = {}
        for page in range(1, 11):
            path = f"/repos/{REPORT_INPUT['repository']}/commits/{REPORT_INPUT['revision']}/check-runs?per_page=100&page={page}"
            pages[path] = {'total_count': 1001, 'check_runs': [{'id': page * 100 + n} for n in range(100)]}
        fake = FakeOpener(pages)
        reader.opener = fake
        result = reader.read_checks(REPORT_INPUT['revision'])
        self.assertEqual(result['state'], 'partial')
        self.assertEqual(len(result['check_runs']), 1000)
        self.assertEqual(len(fake.urls), 10)
        self.assertTrue(all(method == 'GET' for method in fake.methods))
        self.assertTrue(all(timeout <= module.REQUEST_TIMEOUT_SECONDS for timeout in fake.timeouts))
        self.assertTrue(all(url.startswith('https://api.github.com/repos/forge-fixture/release-readiness/commits/' + REPORT_INPUT['revision'] + '/check-runs?') for url in fake.urls))

    def test_live_reader_resolves_annotated_tags_and_rejects_other_repositories(self):
        module = self.load_collector_module()
        revision = REPORT_INPUT['revision']
        tag_object = '2' * 40
        base = f"/repos/{REPORT_INPUT['repository']}"
        ref_path = base + '/git/ref/tags/v1.2.3'
        tag_path = base + f'/git/tags/{tag_object}'
        class FakeResponse:
            def __init__(self, url, data):
                self.url = url
                self.body = json.dumps(data).encode('utf-8')
            def __enter__(self): return self
            def __exit__(self, *args): return False
            def geturl(self): return self.url
            def read(self, size=-1):
                offset = getattr(self, 'offset', 0)
                if size < 0:
                    size = len(self.body) - offset
                data = self.body[offset:offset + size]
                self.offset = offset + len(data)
                return data
        class FakeOpener:
            def __init__(self, detail_sha=tag_object):
                self.urls = []
                self.detail_sha = detail_sha
            def open(self, request, timeout):
                self.urls.append(request.full_url)
                path = urlsplit(request.full_url).path
                value = ({'ref': 'refs/tags/v1.2.3', 'object': {'sha': tag_object, 'type': 'tag'}}
                         if path == ref_path else
                         {'tag': 'v1.2.3', 'sha': self.detail_sha,
                          'object': {'sha': revision, 'type': 'commit'}})
                return FakeResponse(request.full_url, value)
        reader = module.GitHubReader(REPORT_INPUT['repository'])
        fake = FakeOpener()
        reader.opener = fake
        tag = reader.read_tag('v1.2.3')
        self.assertEqual(tag['state'], 'present')
        self.assertEqual([entry['sha'] for entry in tag['objects']], [tag_object, revision])
        self.assertEqual(
            tag['objects'], module._fixture_snapshot('ready')['tag']['objects']
        )
        self.assertEqual(len(fake.urls), 2)
        with self.assertRaises(module.InvalidEvidence):
            reader.get_json('/repos/other/repo/commits/' + revision)
        self.assertEqual(len(fake.urls), 2)
        mismatch_reader = module.GitHubReader(REPORT_INPUT['repository'])
        mismatch_reader.opener = FakeOpener(detail_sha='4' * 40)
        with self.assertRaises(module.InvalidEvidence):
            mismatch_reader.read_tag('v1.2.3')

    def test_configured_plugin_env_contract_uses_the_established_plugin_map(self):
        module = self.load_collector_module()
        self.assertEqual(module.ALLOWLIST_ENV, 'RELEASE_READINESS_ALLOWED_REPOSITORY')
        readme = (REPORT_EXAMPLE / 'README.md').read_text()
        config_text = readme.split('```json\n', 1)[1].split('\n```', 1)[0]
        config = json.loads(config_text)
        plugin = config['plugins']['plugin.release-readiness-report']
        self.assertEqual(plugin['env_passthrough'], [module.ALLOWLIST_ENV])
        self.assertEqual(plugin['timeout_seconds'], 20)
        handlers = module.GitHubReader(REPORT_INPUT['repository']).opener.handlers
        self.assertFalse(
            any(isinstance(handler, module.urllib.request.ProxyHandler) for handler in handlers)
        )

    def test_successful_protocol_replies_include_api_version(self):
        for operation, input_value in (
            ('describe', None),
            ('execute', REPORT_INPUT),
            ('close', None),
        ):
            with self.subTest(operation=operation):
                reply = self.invoke(operation, input_value)
                self.assertEqual(reply.get('api_version'), 1)
                self.assertNotIn('error', reply)

    def test_http_body_failures_and_late_json_are_unavailable(self):
        module = self.load_collector_module()
        path = f"/repos/{REPORT_INPUT['repository']}/commits/{REPORT_INPUT['revision']}"

        class FakeResponse:
            def __init__(self, url, error=None, body=b'{"ok":true}', on_read=None):
                self.url = url
                self.error = error
                self.body = body
                self.length: int | None = None
                self.on_read = on_read

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def geturl(self):
                return self.url

            def read(self, size=-1):
                if self.error is not None:
                    raise self.error
                if self.on_read is not None:
                    self.on_read()
                return self.body[:size]

        class FakeOpener:
            def __init__(self, response):
                self.response = response

            def open(self, request, timeout):
                return self.response

        for error in (
            http.client.IncompleteRead(b'{"late":', 5),
            TimeoutError('read timed out'),
            OSError('connection reset'),
        ):
            reader = module.GitHubReader(REPORT_INPUT['repository'])
            url = module.API_ROOT + path
            reader.opener = FakeOpener(FakeResponse(url, error=error))
            with self.subTest(error=type(error).__name__):
                with self.assertRaises(module.ApiUnavailable):
                    reader.get_json(path)

        reader = module.GitHubReader(REPORT_INPUT['repository'])
        url = module.API_ROOT + path
        truncated = FakeResponse(url, body=b'')
        truncated.length = 1
        reader.opener = FakeOpener(truncated)
        with self.assertRaises(module.ApiUnavailable):
            reader.get_json(path)

        class Clock:
            now = 0.0

            def monotonic(self):
                return self.now

        clock = Clock()
        reader = module.GitHubReader(REPORT_INPUT['repository'])
        reader.deadline = 12.0
        url = module.API_ROOT + path
        response = FakeResponse(
            url,
            on_read=lambda: setattr(clock, 'now', 12.1),
        )
        reader.opener = FakeOpener(response)
        with mock.patch.object(module.time, 'monotonic', clock.monotonic):
            with self.assertRaises(module.ApiUnavailable) as caught:
                reader.get_json(path)
        self.assertEqual(caught.exception.code, 'overall_deadline')

        reader = module.GitHubReader(REPORT_INPUT['repository'])
        reader.opener = FakeOpener(
            FakeResponse(module.API_ROOT + path, error=http.client.IncompleteRead(b'{', 1))
        )
        snapshot = reader.snapshot(REPORT_INPUT['revision'], REPORT_INPUT['tag'])
        report = module._build_report(module._validate_input(REPORT_INPUT), snapshot)
        self.assertEqual(report['verdict'], 'incomplete')

    def test_documented_live_conversion_resolves_every_ref_to_collector(self):
        readme = (REPORT_EXAMPLE / 'README.md').read_text()
        section = readme.split('### Convert the fixture import to a live workflow', 1)[1]
        script = section.split('```python\n', 1)[1].split('\n```', 1)[0]
        result = subprocess.run(
            [sys.executable, '-c', script], cwd=EXAMPLES.parent,
            text=True, capture_output=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        converted = json.loads(result.stdout)
        definition = next(
            work for work in converted['work']
            if work['id'] == 'release-readiness-report'
        )['definition']['input']
        params = definition['params']
        collect = next(step for step in definition['steps'] if step['id'] == 'collect')
        action_input = collect['action']['input']

        for removed in ('fixture_id', 'observed_at'):
            self.assertNotIn(removed, params)
            self.assertNotIn(removed, action_input)
        self.assertEqual(set(action_input), set(params))
        for reference in action_input.values():
            self.assertEqual(set(reference), {'$ref'})
            target = reference['$ref']
            self.assertTrue(target.startswith('/params/'))
            self.assertIn(target.removeprefix('/params/'), params)
        self.assertEqual(params['mode'], 'live')
        self.assertEqual(params['repository'], 'example-org/widget')
        self.assertRegex(params['revision'], r'^[0-9a-f]{40}$')
        self.assertEqual(params['tag'], 'v1.2.3')
        self.assertTrue(params['required_checks'])
        self.assertTrue(params['required_artifacts'])

    def test_live_mode_rejects_present_observed_at_even_when_null(self):
        module = self.load_collector_module()
        for observed_at in (None, REPORT_INPUT['observed_at']):
            with self.subTest(observed_at=observed_at):
                value = dict(
                    REPORT_INPUT, mode='live', repository='example-org/widget',
                    observed_at=observed_at,
                )
                value.pop('fixture_id')
                with self.assertRaisesRegex(module.InputError, 'live_observed_at_not_allowed'):
                    module._validate_input(value)

    def test_live_timestamp_is_sampled_before_github_collection(self):
        module = self.load_collector_module()
        value = dict(REPORT_INPUT, mode='live', repository='example-org/widget')
        value.pop('fixture_id')
        value.pop('observed_at')
        clock = mock.Mock()
        clock.now.return_value.isoformat.return_value = '2026-10-09T10:00:00+00:00'

        class StopAfterCollectionStarts(Exception):
            pass

        reader = mock.Mock()

        def stop_after_timestamp(*args):
            clock.now.assert_called_once_with(module.timezone.utc)
            raise StopAfterCollectionStarts

        reader.snapshot.side_effect = stop_after_timestamp
        with (
            mock.patch.dict(os.environ, {module.ALLOWLIST_ENV: 'example-org/widget'}),
            mock.patch.object(module, 'datetime', clock),
            mock.patch.object(module, 'GitHubReader', return_value=reader),
            self.assertRaises(StopAfterCollectionStarts),
        ):
            module._execute(value)

    def test_malformed_and_oversized_transport_replies_include_api_version(self):
        module = self.load_collector_module()
        cases = (
            ('malformed_json', b'{', 'invalid_json_request'),
            ('invalid_utf8', b'\xff', 'invalid_json_request'),
            ('oversized', b'x' * (module.MAX_INPUT_BYTES + 1), 'request_too_large'),
        )
        for label, payload, expected_code in cases:
            with self.subTest(request=label):
                result = subprocess.run(
                    [sys.executable, str(REPORT_COLLECTOR)], input=payload,
                    capture_output=True, check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr.decode('utf-8', 'replace'))
                reply = json.loads(result.stdout)
                self.assertEqual(reply.get('api_version'), 1)
                self.assertEqual(reply['error']['code'], expected_code)

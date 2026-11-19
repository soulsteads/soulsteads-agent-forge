#!/usr/bin/env python3
"""Scoped Home CLI -> native -> public Forge MCP inventory qualification.

Python 3.11+, POSIX. No service provisioning, workflow registration or retry.
Each controller phase is attempted at most once in this output directory. After
failure use inspect-existing --no-model; never change the key to repair readback.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import sys
import time
import tomllib
from urllib.parse import urlsplit

TEXT = 'Synthetic inventory: 3 notebooks + 2 pens = 5 items.'
RESULT = {'file': 'inventory.txt', 'items': 5, 'summary': TEXT}
DEPENDENT = 'Verified inventory: ' + TEXT
AGENT = 'synthetic-home-forge'
SKILL = 'inventory-acceptance'
RECEIPT = 'home-delegation-receipt.txt'  # relative to the native workspace
LIMITS = ['Home-root native delegation and same-session readback only',
          'No schedule, crash, cancellation, backup or all-permission qualification',
          'Source revisions are operator assertions, not measured build provenance',
          'Prepared intent is not a public admission; null model metadata stays null']


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def load(path):
    return json.loads(Path(path).read_text())


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save(path, value):
    path = Path(path)
    temp = path.with_suffix(path.suffix + '.tmp')
    temp.write_text(json.dumps(value, indent=2) + '\n')
    temp.replace(path)


def rows(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line]


def paired(events, complete=True):
    """Correlate by native attempt, call, run and session, never by text search."""
    if complete:
        require(events and events[-1].get('type') == 'run_finished' and
                events[-1].get('status') == 'completed', 'controller did not complete')
    proposed, found = {}, []
    for event in events:
        if event.get('type') == 'tool_proposed':
            key = event['attempt_id']
            require(key not in proposed, 'duplicate native attempt')
            proposed[key] = event
        elif event.get('type') == 'tool_result':
            p = proposed.pop(event['attempt_id'])
            require(all(p.get(k) == event.get(k) for k in
                        ('name', 'call_id', 'run_id', 'session_id')), 'foreign tool result')
            found.append({'name': p['name'], 'arguments': p['arguments'],
                          'status': event['status'], 'output': event['output'],
                          'attempt_id': event['attempt_id'], 'run_id': event['run_id'],
                          'session_id': event['session_id']})
    if complete:
        require(not proposed, 'unresolved native tool proposal')
    return found


def exact_call(calls, name, arguments):
    matches = [c for c in calls if c['name'] == name and c['arguments'] == arguments
               and c['status'] == 'ok']
    require(len(matches) == 1, 'expected one successful paired ' + name)
    return matches[0]


def admission(events, work, key, complete=True):
    calls = paired(events, complete)
    runs = [c for c in calls if c['name'] == 'mcp__forge__forge_run']
    require(len(runs) == 1, 'expected exactly one controller admission result')
    call = exact_call(runs, 'mcp__forge__forge_run',
                      {'work_id': work, 'key': key, 'params': {}})
    result = json.loads(call['output'])
    require(isinstance(result.get('run_id'), str) and result['run_id'], 'missing admitted run')
    return result['run_id']


def check_work(run, effect):
    require(run['status'] == 'SUCCESS', 'run is not SUCCESS')
    expected_steps = ['forge.admit'] + ['forge.flow.root/step:' + s for s in ('seed', 'work', 'after')]
    require([s['function_name'] for s in run['steps']] == expected_steps and
            all(s['completed_at'] and not s['error'] for s in run['steps']), 'incomplete DBOS steps')
    require(effect['run_id'] == run['run_id'] and effect['effect']['state'] == 'completed',
            'wrong or incomplete effect')
    output = effect['effect']['output']
    require(output['result'] == RESULT, 'typed inventory output mismatch')
    require(run['output']['output']['dependent'] == DEPENDENT, 'dependent mismatch')
    h = output['harness']
    require(h['adapter'] == 'soulsteads' and h['tool_events'] >= 2 and h['attempt'] == 1,
            'missing native tool execution')
    require(sum(x['action'] == 'harness_launched' for x in run['logs']) == 1, 'launch count mismatch')
    artifact = next(a for a in output['artifacts'] if a['path'] == 'inventory.txt')
    require(artifact['text'] == TEXT and artifact['bytes'] == len(TEXT.encode()) and
            artifact['sha256'] == hashlib.sha256(TEXT.encode()).hexdigest(), 'artifact mismatch')
    return output


def verify_retained(base):
    """Read-only validation also accepts the original acceptance receipt layout."""
    base = Path(base)
    e, prep = load(base / 'evidence.json'), load(base / 'preparation.json')
    home = base / AGENT
    session = e['session_id']
    streams = {label: rows(base / (label + '.jsonl'))
               for label in ('initial-inspect', 'delegate', 'readback')}
    calls = {label: paired(events) for label, events in streams.items()}
    for events in streams.values():
        require(all(x['session_id'] == session for x in events), 'session changed')
    run_id = admission(streams['delegate'], e['work_id'], e['key'])
    require(run_id == e['run_id'], 'admission/run mismatch')
    for label, rid in [('initial-inspect', e['prior_run']), ('readback', run_id)]:
        require(not any(c['name'] == 'mcp__forge__forge_run' for c in calls[label]),
                'unexpected admission outside delegate')
        public = json.loads(exact_call(calls[label], 'mcp__forge__forge_inspect',
                                      {'run_id': rid})['output'])
        require(public['run_id'] == rid and public['status'] == 'SUCCESS', 'wrong public inspection')
        require(public['output']['output']['dependent'] == DEPENDENT, 'readback dependent mismatch')
    exact_call(calls['initial-inspect'], 'skill_activate', {'name': SKILL})
    discovered = [skill for event in streams['initial-inspect'] if event['type'] == 'skills_discovered'
                  for skill in event['skills'] if skill['name'] == SKILL]
    require(len(discovered) == 1 and discovered[0]['sha256'] == sha(home / 'skills' / SKILL / 'SKILL.md'),
            'native skill discovery hash mismatch')
    exact_call(calls['readback'], 'write_file', {'path': RECEIPT, 'content': TEXT})
    run, effect = load(base / 'run.json'), load(base / 'effect.json')
    require(run['run_id'] == run_id, 'wrong retained run')
    output = check_work(run, effect)
    require((home / 'workspace' / RECEIPT).read_bytes() == TEXT.encode(), 'controller receipt bytes')
    for name, digest in prep['canonical_and_projection'].items():
        require(sha(home / name) == digest, 'canonical/projection changed: ' + name)
    journal = Path(e['journal'])
    start = rows(journal)[0]
    require(start['session_id'] == session and start['workspace'] == str(home / 'workspace'),
            'journal origin mismatch')
    for name, digest in start['instructions']:
        require(name.startswith('file:') and sha(home / name[5:]) == digest, 'instruction hash mismatch')
    cfg = tomllib.loads((home / 'soulsteads-harness.toml').read_text())
    man = load(home / 'soulsteads.json')
    require(man['runtime'] == 'soulsteads' and cfg['agent']['id'] == AGENT, 'wrong Home runtime')
    require(len(cfg['mcp_servers']) == 1 and cfg['mcp_servers'][0]['env_pass'] == ['FORGE_TOKEN'] and
            cfg['mcp_servers'][0]['tools'] == {'forge_run': 'allow', 'forge_inspect': 'allow'},
            'unexpected MCP grant/reference')
    preview = load(base / 'launch-preview.stdout')
    require(preview['argv'][0] == prep.get('native_bin', preview['argv'][0]) and
            preview['cwd'] == str(home), 'wrong dry-run launch')
    for label in ('initial-inspect', 'delegate', 'readback'):
        command_path = base / (label + '.command.json')
        # Original acceptance stored launch argv centrally; new driver stores each command.
        command = load(command_path) if command_path.exists() else next(p for p in e['pids'] if p['label'] == label)
        require(command['argv'][:3] == [prep['home_cli'], 'run', str(home)], 'not a Home CLI launch')
    prov_path = base / 'delegated-provenance.json'
    physical = None
    if prov_path.exists():
        prov = load(prov_path)
        require(prov['attempt']['state'] == 'exited' and prov['attempt']['result'] == 'validated' and
                prov['attempt']['run_id'] == run_id, 'invalid physical attempt')
        require(prov['attempt']['invocation_id'] == output['harness']['invocation_id'] and
                prov['attempt']['session_id'] == output['harness']['session_id'], 'physical invocation mismatch')
        for f in prov['files']:
            require(sha(f['path']) == f['sha256'], 'physical evidence changed')
            if Path(f['path']).name == 'inventory.txt':
                require(Path(f['path']).read_bytes() == TEXT.encode(), 'physical inventory bytes mismatch')
        physical = True
    before, after = load(base / 'snapshot-before.json'), load(base / 'snapshot-after.json')
    old = {r['run_id'] for r in before['runs']}
    new = [r['run_id'] for r in after['runs'] if r['run_id'] not in old and r['work_id'] == e['work_id']]
    require(new == [run_id], 'not exactly one new workflow')
    replay = base / 'replay-effect.json'
    if replay.exists():
        require(load(replay) == effect and load(base / 'admission-replay.json')['run_id'] == run_id,
                'replay changed run/effect')
    else:
        # Validate the raw historical public transport, not its summary boolean.
        exchanges = rows(base / 'mcp.jsonl')
        request = {'name': 'forge_run', 'arguments': {'work_id': e['work_id'], 'key': e['key'], 'params': {}}}
        matches = [x for x in exchanges if x['request'].get('params') == request]
        require(matches, 'no raw stable-key replay')
        require(json.loads(matches[-1]['response']['result']['content'][0]['text'])['run_id'] == run_id,
                'wrong replay run')
        inspections = [x for x in exchanges if x['request'].get('params') ==
                       {'name': 'forge_inspect', 'arguments': {'run_id': run_id, 'effect_id': 'root/step:work'}}]
        require(len(inspections) >= 2 and all(json.loads(x['response']['result']['content'][0]['text']) == effect
                                            for x in inspections), 'raw replay effect mismatch')
    return {'status': 'RETAINED_RECEIPTS_VERIFIED', 'live_execution_this_command': False,
            'run_id': run_id, 'session_id': session, 'harness': output['harness'],
            'physical_artifacts_verified': physical, 'limits': LIMITS}


def stop_group(process):
    # Only groups started by this driver; never the operator Forge service.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


class MCP:
    def __init__(self, args, output):
        self.args, self.output = args, output
        self.n, self.buffer = 0, b''

    def __enter__(self):
        require(os.environ.get('FORGE_TOKEN'), 'FORGE_TOKEN must be supplied in the process environment')
        self.err = (self.output / 'mcp.stderr').open('ab')
        self.p = subprocess.Popen([self.args.forge_bin, '--url', self.args.forge_url, 'mcp'],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.err,
                                  start_new_session=True)
        try:
            self.rpc('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {},
                                   'clientInfo': {'name': 'home-native-qualification', 'version': '1'}})
            self.p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            self.p.stdin.flush()
            return self
        except BaseException:
            self.__exit__()
            raise

    def rpc(self, method, params):
        self.n += 1
        request = {'jsonrpc': '2.0', 'id': self.n, 'method': method, 'params': params}
        self.p.stdin.write((json.dumps(request) + '\n').encode())
        self.p.stdin.flush()
        deadline, size = time.monotonic() + 30, 0
        while time.monotonic() < deadline:
            if b'\n' not in self.buffer:
                if not select.select([self.p.stdout], [], [], max(0, deadline-time.monotonic()))[0]:
                    break
                chunk = os.read(self.p.stdout.fileno(), 65536)
                require(chunk, 'MCP EOF')
                self.buffer += chunk
                size += len(chunk)
                require(size <= 8 * 1024 * 1024, 'MCP response exceeds 8 MiB')
                continue
            line, self.buffer = self.buffer.split(b'\n', 1)
            response = json.loads(line)
            with (self.output / 'mcp.jsonl').open('a') as log:
                log.write(json.dumps({'request': request, 'response': response}) + '\n')
            if response.get('id') == self.n:
                require('error' not in response, 'MCP error; see retained response')
                return response['result']
        raise TimeoutError('MCP response deadline')

    def call(self, name, arguments):
        result = self.rpc('tools/call', {'name': 'forge_' + name, 'arguments': arguments})
        require(not result.get('isError'), 'Forge MCP tool error; see retained response')
        return json.loads(result['content'][0]['text'])

    def __exit__(self, *unused):
        stop_group(self.p)
        self.err.close()


class Driver:
    def __init__(self, args):
        self.a, self.out = args, Path(args.output)
        self.home = self.out / AGENT
        self.env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1', SOULSTEADS_HARNESS_BIN=args.native_bin,
                        GIT_AUTHOR_NAME='Synthetic Qualification', GIT_AUTHOR_EMAIL='synthetic@example.invalid',
                        GIT_COMMITTER_NAME='Synthetic Qualification', GIT_COMMITTER_EMAIL='synthetic@example.invalid')

    def command(self, label, argv, prompt=None, cwd=None, timeout=30):
        marker = self.out / (label + '.command.json')
        require(not marker.exists(), label + ' already attempted; inspect retained evidence, do not relaunch')
        receipt = {'argv': argv, 'cwd': str(cwd) if cwd else None, 'status': 'started'}
        save(marker, receipt)  # durable intent before spawn
        with (self.out / (label + '.stdout')).open('w') as out, (self.out / (label + '.stderr')).open('w') as err:
            p = subprocess.Popen(argv, env=self.env, cwd=cwd, stdin=subprocess.PIPE,
                                 stdout=out, stderr=err, text=True, start_new_session=True)
            receipt['pid'] = p.pid
            save(marker, receipt)
            try:
                p.communicate(prompt, timeout=timeout)
            except BaseException:
                stop_group(p)
                receipt.update(status='interrupted', exit_code=p.returncode)
                save(marker, receipt)
                raise
        receipt.update(status='finished', exit_code=p.returncode)
        save(marker, receipt)
        require(p.returncode == 0, label + ' failed; see raw stdout/stderr')
        return (self.out / (label + '.stdout')).read_text()

    def cli(self, label, *args):
        return self.command(label, [self.a.home_cli, *map(str, args)])

    def controller(self, label, prompt, session=None):
        argv = [self.a.home_cli, 'run', str(self.home), '--', 'run', '--format', 'jsonl']
        if session:
            argv += ['--session', session]
        argv += ['-']
        (self.out / (label + '.task.txt')).write_text(prompt)
        try:
            self.command(label, argv, prompt=prompt, timeout=330)
        finally:
            source = self.out / (label + '.stdout')
            if source.exists():
                (self.out / (label + '.jsonl')).write_bytes(source.read_bytes())
        events = rows(self.out / (label + '.jsonl'))
        paired(events)
        require(len({e['session_id'] for e in events}) == 1, 'mixed controller sessions')
        if session:
            require(events[0]['session_id'] == session, 'controller did not resume same session')
        return events

    def prepare(self):
        require(not self.home.exists(), 'synthetic Home exists; inspect preparation, do not recreate')
        self.cli('create', 'create', AGENT, '--runtime', 'soulsteads', '--path', self.home, '--no-skills')
        for name, text in {
            'AGENTS.md': 'Synthetic inventory controller. Use actual Forge receipts, exact supplied keys and run ids. Activate inventory-acceptance via skill_activate. Write receipts using workspace-relative paths. No external messages.\n',
            'SOUL.md': 'Identity: synthetic-home-forge. Purpose: verify scoped Forge delegation.\n',
            'USER.md': 'Synthetic qualification operator; no real user profile.\n',
            'MEMORY.md': 'Old direct-native evidence is not Home-origin proof.\n',
        }.items():
            (self.home / name).write_text(text)
        skill = self.out / SKILL
        skill.mkdir()
        (skill / 'SKILL.md').write_text('---\nname: inventory-acceptance\ndescription: Verify synthetic inventory receipts through Forge.\n---\nUse actual public results. Expected 3 notebooks and 2 pens, 5 items. Write a receipt only after SUCCESS and the dependent result. Never use a new key to repair readback.\n')
        self.cli('skill-install', 'skill', 'install', self.home, skill)
        self.cli('identity-commit', 'commit', self.home, '-m', 'Synthetic identity and skill')
        self.cli('select', 'config', self.home, '--runtime', 'soulsteads')
        self.cli('canonical-sync', 'config', self.home, '--sync', '--shared-json',
                 json.dumps({'model': self.a.model, 'permission': 'workspace-write'}))
        config = self.home / 'soulsteads-harness.toml'
        original = config.read_text()
        require(original.count('[provider]\n') == 1, 'unexpected Home native projection')
        native = tomllib.loads(original)
        require(not native.get('mcp_servers') and not native.get('limits') and
                not native['provider'].get('base_url'), 'operator native settings already exist')
        # Only operator-owned additions. Home owns the generated model/grants/instructions.
        config.write_text(original.replace('[provider]\n', '[provider]\nkind = "openai-compat"\nbase_url = ' +
                          json.dumps(self.a.base_url) + '\nmax_tokens = 1400\n') +
                          '\n[limits]\nmax_turns = 8\nmax_tool_calls = 10\ndeadline_secs = 300\n' +
                          '\n[[mcp_servers]]\nname = "forge"\ntransport = "stdio"\ncommand = ' +
                          json.dumps(self.a.forge_bin) + '\nargs = ' + json.dumps(['--url', self.a.forge_url, 'mcp']) +
                          '\nenv_pass = ["FORGE_TOKEN"]\ndefault_approval = "deny"\n' +
                          '[mcp_servers.tools]\nforge_run = "allow"\nforge_inspect = "allow"\n')
        self.cli('native-sync', 'config', self.home, '--sync')
        self.cli('commit', 'commit', self.home, '-m', 'Synthetic native Forge configuration')
        offline = self.cli('home-verify', 'verify', self.home)
        preview = self.cli('launch-preview', 'run', self.home, '--dry-run', '--', 'run', '--format', 'jsonl', '-')
        require(json.loads(preview)['argv'][0] == self.a.native_bin, 'Home selected unexpected native binary')
        caps = json.loads(self.cli('native-capabilities', 'run', self.home, '--', 'capabilities'))
        require({'skill_activate', 'read_file', 'write_file'} <= set(caps['granted_tools']), 'native tools unavailable')
        # Observe installed package bytes using the CLI's own interpreter, without importing Home.
        shebang = Path(self.a.home_cli).read_text().splitlines()[0]
        require(shebang.startswith('#!/') and ' ' not in shebang[2:], 'Home CLI needs a direct interpreter shebang')
        package = json.loads(self.command('home-package', [shebang[2:], '-c',
            'import importlib.metadata as m,hashlib,json; d=m.distribution("soulsteads"); '
            'print(json.dumps({str(p):hashlib.sha256(d.locate_file(p).read_bytes()).hexdigest() '
            'for p in d.files if d.locate_file(p).is_file() and "__pycache__" not in str(p)}))']))
        files = ['soulsteads.json', 'soulsteads-harness.toml', 'AGENTS.md', 'SOUL.md', 'USER.md', 'MEMORY.md',
                 'LEARNING.md', 'skills/' + SKILL + '/SKILL.md']
        save(self.out / 'preparation.json', {
            'source_revision_assertions': {k: getattr(self.a, k + '_source') for k in ('home', 'native', 'forge')},
            'home_cli': self.a.home_cli, 'native_bin': self.a.native_bin,
            'binary_sha256': {k: sha(getattr(self.a, k)) for k in ('home_cli', 'native_bin', 'forge_bin')},
            'installed_package_sha256': package, 'offline_home_verification': json.loads(offline),
            'canonical_and_projection': {f: sha(self.home / f) for f in files},
            'new_request_intent': self.a.request_key, 'actual_admissions': [], 'model_calls': 0,
            'home_commit': subprocess.check_output(['git', '-C', str(self.home), 'rev-parse', 'HEAD'], text=True).strip()})
        return {'status': 'PREPARED', 'model_calls': 0, 'home_origin_qualified': False}

    def inspect_public(self, m):
        prep = load(self.out / 'preparation.json')
        for name, digest in prep['binary_sha256'].items():
            require(sha(getattr(self.a, name)) == digest, 'installed executable changed: ' + name)
        for name, digest in prep['canonical_and_projection'].items():
            require(sha(self.home / name) == digest, 'prepared configuration changed: ' + name)
        tools = m.rpc('tools/list', {})
        save(self.out / ('tools-' + str(time.time_ns()) + '.json'), tools)
        for name, fields in [('forge_run', {'work_id', 'key', 'params'}), ('forge_inspect', {'run_id'})]:
            schema = next(t['inputSchema'] for t in tools['tools'] if t['name'] == name)
            require(schema['type'] == 'object' and fields <= set(schema['properties']), 'incompatible public schema')
        old = m.call('inspect', {'run_id': self.a.existing_run})
        require(old['status'] == 'SUCCESS' and old['work_id'] == self.a.work_id, 'existing inventory run mismatch')
        require(old['output']['output']['dependent'] == DEPENDENT, 'wrong inventory workflow')
        report = {'existing': old, 'journals': [], 'controller_admissions': []}
        for p in self.home.glob('.runtime/soulsteads-harness/agents/*/sessions/*.jsonl'):
            report['journals'].append({'path': str(p), 'sha256': sha(p), 'rows': rows(p)})
        # Always inspect actual public run receipts before any recovery-side mutation.
        p = self.out / 'delegate.jsonl'
        if p.exists():
            try:
                rid = admission(rows(p), self.a.work_id, self.a.request_key, complete=False)
            except (RuntimeError, KeyError, ValueError):
                report['admission_uncertain'] = True
            else:
                report['controller_admissions'].append(m.call('inspect', {'run_id': rid}))
        save(self.out / ('inspection-' + str(time.time_ns()) + '.json'), report)
        return report

    def session(self):
        journals = list(self.home.glob('.runtime/soulsteads-harness/agents/*/sessions/*.jsonl'))
        require(len(journals) == 1, 'expected exactly one Home native journal')
        return journals[0]

    def inspect_existing(self, m):
        report = self.inspect_public(m)
        if self.a.no_model:
            return {'status': 'INSPECTED_NO_MODEL', 'admissions': report['controller_admissions'], 'model_calls': 0}
        require(not (self.out / 'initial-inspect.command.json').exists(), 'initial inspection already attempted; use --no-model')
        save(self.out / 'existing-run-before.json', report['existing'])
        save(self.out / 'snapshot-before.json', m.call('snapshot', {}))
        events = self.controller('initial-inspect',
            f'Activate the {SKILL} skill with skill_activate using {{"name":"{SKILL}"}}. '
            f'Then call mcp__forge__forge_inspect with run_id {self.a.existing_run}. '
            'Report its actual status and dependent output. Do not call forge_run or create effects.')
        calls = paired(events)
        exact_call(calls, 'skill_activate', {'name': SKILL})
        actual = json.loads(exact_call(calls, 'mcp__forge__forge_inspect', {'run_id': self.a.existing_run})['output'])
        require(actual == report['existing'] and not any(c['name'] == 'mcp__forge__forge_run' for c in calls),
                'initial inspection changed work or returned a different run')
        return {'status': 'EXISTING_INSPECTED', 'session_id': self.session().stem}

    def delegate(self, m):
        report = self.inspect_public(m)
        require((self.out / 'inspect-existing.done.json').exists(), 'complete inspect-existing first')
        require(load(self.out / 'inspect-existing.done.json')['status'] == 'EXISTING_INSPECTED', 'Home must inspect first')
        require(not report['controller_admissions'], 'admission already exists; proceed to verify')
        events = self.controller('delegate',
            f'Call mcp__forge__forge_run exactly once with work_id {self.a.work_id}, key {self.a.request_key}, '
            'params {}. This is the ONE new request permitted. Report the actual run_id. Do not poll or use another key.',
            self.session().stem)
        rid = admission(events, self.a.work_id, self.a.request_key)
        # Do not release the shared inference lock while the delegated model is running.
        deadline = time.monotonic() + 340
        while True:
            run = m.call('inspect', {'run_id': rid})
            save(self.out / ('delegated-poll-' + str(time.time_ns()) + '.json'), run)
            if run['status'] not in ('ADMITTED', 'ENQUEUED', 'PENDING'):
                break
            require(time.monotonic() < deadline, 'delegated deadline: inspect existing attempt; no replacement')
            time.sleep(2)
        require(run['status'] == 'SUCCESS', 'delegated run needs inspection: ' + run['status'])
        return {'status': 'ADMITTED_BY_HOME', 'run_id': rid, 'actual_admissions': [rid]}

    def verify(self, m):
        report = self.inspect_public(m)
        require(len(report['controller_admissions']) == 1, 'no proven Home admission; never substitute driver launch')
        rid = admission(rows(self.out / 'delegate.jsonl'), self.a.work_id, self.a.request_key)
        deadline = time.monotonic() + 340
        while True:
            run = m.call('inspect', {'run_id': rid})
            save(self.out / ('run-poll-' + str(time.time_ns()) + '.json'), run)
            if run['status'] not in ('ADMITTED', 'ENQUEUED', 'PENDING') or time.monotonic() >= deadline:
                break
            time.sleep(2)
        effect = m.call('inspect', {'run_id': rid, 'effect_id': 'root/step:work'})
        save(self.out / 'run.json', run)
        save(self.out / 'effect.json', effect)
        output = check_work(run, effect)
        if self.a.forge_state:
            directory = Path(self.a.forge_state) / 'harness' / output['harness']['invocation_id']
            attempt = load(directory / 'attempt.json')
            require((directory / 'workspace/inventory.txt').read_bytes() == TEXT.encode(), 'physical artifact mismatch')
            files = [directory / p for p in ('attempt.json', 'native.toml', 'captured.json', 'workspace/inventory.txt')]
            files += list(directory.glob('native-home/agents/*/sessions/*.jsonl'))
            save(self.out / 'delegated-provenance.json', {'attempt': attempt,
                 'files': [{'path': str(p), 'sha256': sha(p)} for p in files]})
        if not (self.out / 'readback.command.json').exists():
            self.controller('readback',
                f'Call mcp__forge__forge_inspect with run_id {rid}. After observing actual SUCCESS and '
                f'the dependent output, write exactly {json.dumps(TEXT)} without a newline via write_file '
                f'using the workspace-relative path {RECEIPT}. Report run_id and dependent. Do not call forge_run.',
                self.session().stem)
        # Validate readback before operator replay; incomplete attempts are never silently relaunched.
        readback = paired(rows(self.out / 'readback.jsonl'))
        actual = json.loads(exact_call(readback, 'mcp__forge__forge_inspect', {'run_id': rid})['output'])
        require(actual['status'] == 'SUCCESS' and actual['output']['output']['dependent'] == DEPENDENT, 'readback failed')
        require((self.home / 'workspace' / RECEIPT).read_bytes() == TEXT.encode(), 'receipt bytes mismatch')
        save(self.out / 'admission-replay.json', m.call('run', {'work_id': self.a.work_id, 'key': self.a.request_key, 'params': {}}))
        save(self.out / 'replay-effect.json', m.call('inspect', {'run_id': rid, 'effect_id': 'root/step:work'}))
        save(self.out / 'snapshot-after.json', m.call('snapshot', {}))
        require(m.call('inspect', {'run_id': self.a.existing_run}) == load(self.out / 'existing-run-before.json'), 'old run changed')
        save(self.out / 'evidence.json', {'status': 'VERIFYING', 'run_id': rid, 'work_id': self.a.work_id,
             'key': self.a.request_key, 'prior_run': self.a.existing_run, 'session_id': self.session().stem,
             'journal': str(self.session()), 'harness': output['harness'], 'limits': LIMITS})
        result = verify_retained(self.out)
        result.update(status='PASS_SCOPED_HOME_ORIGIN', live_execution_this_command=True)
        evidence = load(self.out / 'evidence.json')
        evidence.update(status=result['status'], home_origin_qualified=True)
        save(self.out / 'evidence.json', evidence)
        return result


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('stage', choices=['prepare', 'inspect-existing', 'delegate-once', 'verify'])
    for name in ('home-cli', 'native-bin', 'forge-bin', 'forge-url', 'output', 'inference-lock',
                 'existing-run', 'work-id', 'request-key'):
        p.add_argument('--' + name, required=True)
    p.add_argument('--base-url', help='Explicit approved native inference endpoint; required for prepare')
    p.add_argument('--model', help='Explicit native model name (no default); required for prepare, '
                   'which records it in the synthetic Home')
    for name in ('home', 'native', 'forge'):
        p.add_argument('--' + name + '-source', help='Operator source revision assertion, not a binary identity')
    p.add_argument('--forge-state', help='Optional read-only Forge state directory for physical artifact checks')
    p.add_argument('--no-model', action='store_true', help='inspect-existing only: public receipts/journal, no controller')
    p.add_argument('--offline', action='store_true', help='verify only: retained receipts, no CLI/service/model calls')
    p.add_argument('--receipts', help='verify --offline: original retained receipt directory (read-only)')
    return p


def main():
    p = parser()
    a = p.parse_args()
    if a.no_model and a.stage != 'inspect-existing' or a.offline and a.stage != 'verify' or a.receipts and not a.offline:
        p.error('--no-model is inspect-existing only; --offline/--receipts are verify only')
    if a.stage == 'prepare' and not a.base_url:
        p.error('prepare requires --base-url')
    if a.stage == 'prepare' and not a.model:
        p.error('prepare requires --model (no default model)')
    for name in ('home_cli', 'native_bin', 'forge_bin', 'output', 'inference_lock'):
        if not Path(getattr(a, name)).is_absolute():
            p.error('--' + name.replace('_', '-') + ' must be absolute')
    for name in ('forge_url', 'base_url'):
        if getattr(a, name):
            u = urlsplit(getattr(a, name))
            if u.scheme not in ('http', 'https') or not u.hostname or u.username or u.password or u.query or u.fragment:
                p.error('URLs must be http(s) without credentials/query/fragment')
    for value in (a.work_id, a.request_key, a.existing_run):
        if not re.fullmatch(r'[A-Za-z0-9_.:-]+', value):
            p.error('work/run/key must be nonempty identifiers')
    os.umask(0o077)
    out = Path(a.output)
    out.mkdir(parents=True, exist_ok=True)
    with (out / '.driver.lock').open('a') as mutex:
        fcntl.flock(mutex, fcntl.LOCK_EX | fcntl.LOCK_NB)
        identity = {k: getattr(a, k) for k in ('home_cli', 'native_bin', 'forge_bin', 'forge_url',
                                              'existing_run', 'work_id', 'request_key', 'inference_lock')}
        manifest = out / 'manifest.json'
        if manifest.exists():
            require(load(manifest) == identity, 'output identity changed; keep the same key/paths and inspect')
        else:
            save(manifest, identity)
        attempt = out / (a.stage + '-' + str(time.time_ns()) + '.stage.json')
        receipt = {'stage': a.stage, 'status': 'started', 'at': time.time(), 'limits': LIMITS,
                   'prior_failed_stage_attempts': sum(load(f).get('status') == 'failed' for f in out.glob('*.stage.json'))}
        save(attempt, receipt)
        try:
            if a.offline:
                result = verify_retained(a.receipts or out)
            else:
                d = Driver(a)
                if a.stage == 'prepare':
                    result = d.prepare()
                else:
                    require((out / 'preparation.json').exists(), 'prepare first')
                    # The same lock spans controller calls, delegated work, polling and replay.
                    with contextlib.ExitStack() as stack:
                        if not a.no_model:
                            lock = stack.enter_context(open(a.inference_lock, 'a'))
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        m = stack.enter_context(MCP(a, out))
                        result = {'inspect-existing': d.inspect_existing, 'delegate-once': d.delegate,
                                  'verify': d.verify}[a.stage](m)
            receipt.update(status='completed', result=result, finished_at=time.time())
            if not a.no_model and not a.offline:
                save(out / (a.stage + '.done.json'), result)
            print(json.dumps(result))
        except BaseException as exc:
            receipt.update(status='failed', error=type(exc).__name__ + ': ' + str(exc), finished_at=time.time())
            raise
        finally:
            save(attempt, receipt)


if __name__ == '__main__':
    main()

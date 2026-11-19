"""Bounded installed Soulsteads/DBOS qualification; Python standard library only.

prepare creates private isolated configuration without starting services/models.
run requires an explicit local model endpoint, a free workload slot, and PostgreSQL
server/client binaries. It owns its fresh cluster and loopback ports, never an
incumbent database. Four sequential native invocations, no automatic retries.
Failures and homes are retained. See --help and docs/harness.md.
"""
import argparse
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def command(args, **kw):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True,
                            timeout=120, **kw)
    if result.returncode:
        raise RuntimeError(f'{args[0]} exited {result.returncode}: {result.stderr}')
    return result.stdout


# This wrapper runs the immutable real harness unchanged. Only the fault profile
# holds its completed stdout before Forge receives it. It implements no agent loop.
WRAPPER = '''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys, time
c = json.loads(pathlib.Path(__file__).with_suffix('.json').read_text())
if sys.argv[1:] == ['--version']:
    os.execv(c['harness'], [c['harness'], '--version'])
root = pathlib.Path.cwd().parent
result = subprocess.run([c['harness'], *sys.argv[1:]], input=sys.stdin.buffer.read(),
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
(root / 'fault-native.stdout').write_bytes(result.stdout)
(root / 'fault-native.stderr').write_bytes(result.stderr)
(root / 'fault-ready.json').write_text(json.dumps({'exit_code': result.returncode,
    'wrapper_pid': os.getpid(), 'start_ticks': pathlib.Path('/proc/self/stat').read_text().rsplit(')', 1)[1].split()[19]}))
# Bounded hold, not a replacement invocation. Coordinator crashes/restarts Forge.
time.sleep(180)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
'''


def prepare(a):
    state = a.state.resolve()
    state.mkdir(mode=0o700, parents=True, exist_ok=False)
    forge, harness = a.forge.resolve(strict=True), a.harness.resolve(strict=True)
    command([forge, '--home', state / 'home', 'init'])
    wrapper = state / 'hold-native.py'
    wrapper.write_text(WRAPPER)
    wrapper.chmod(0o700)
    save(wrapper.with_suffix('.json'), {'harness': str(harness)})
    config_path = state / 'home/config.json'
    config = json.loads(config_path.read_text())
    profile = dict(adapter='soulsteads', executable=str(harness), model=a.model,
                   provider='openai-compat', permission_mode='workspace-write',
                   containment=a.containment, timeout_seconds=300,
                   max_output_bytes=1048576, env={'HARNESS_BASE_URL': a.base_url},
                   env_passthrough=[])
    config['runners'] = {'native': profile, 'native-ro': dict(profile, permission_mode='read-only'),
                         'native-hold': dict(profile, executable=str(wrapper))}
    save(config_path, config)
    manifest = dict(source_commit=a.source_commit, forge=str(forge), harness=str(harness),
                    forge_sha256=digest(forge), harness_sha256=digest(harness),
                    forge_version=command([forge, '--version']).strip(),
                    harness_version=command([harness, '--version']).strip(),
                    model=a.model, base_url=a.base_url, port=a.port, pg_port=a.pg_port,
                    containment=a.containment, wrapper_sha256=digest(wrapper),
                    config_sha256=digest(config_path), driver_sha256=digest(__file__))
    save(state / 'prepared.json', manifest)
    print(json.dumps({'status': 'PREPARED_NOT_LIVE', 'state': str(state)}))


class Qualification:
    def __init__(self, state, pg_bin):
        self.state, self.pg_bin = state.resolve(), pg_bin.resolve(strict=True)
        self.m = json.loads((self.state / 'prepared.json').read_text())
        self.home = self.state / 'home'
        self.forge = self.m['forge']
        self.env = dict(os.environ)
        config = json.loads((self.home / 'config.json').read_text())
        self.env['FORGE_TOKEN'] = config['actors']['operator']['token']
        self.env['FORGE_DATABASE_URL'] = f"postgresql://forge@127.0.0.1:{self.m['pg_port']}/qualification"
        self.service = None
        self.pg_started = False
        self.schedule = None
        self.evidence = {'identity': self.m, 'cases': {}, 'status': 'RUNNING'}

    def record(self, name, data):
        save(self.state / (name + '.json'), data)
        return data

    def cli(self, *args, data=None, denied=False):
        result = subprocess.run([self.forge, '--url', f"http://127.0.0.1:{self.m['port']}", *args],
                                input=json.dumps(data) if data is not None else None,
                                env=self.env, capture_output=True, text=True, timeout=120)
        if denied:
            assert result.returncode != 0, 'required authority was silently accepted'
            return {'exit_code': result.returncode, 'stderr': result.stderr}
        if result.returncode:
            raise RuntimeError(f'Forge {args[:2]} failed: {result.stderr}')
        return json.loads(result.stdout)

    def until(self, fn, seconds=420):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = fn()
            if value:
                return value
            time.sleep(.25)
        raise TimeoutError('qualification condition did not settle')

    def start(self):
        log = open(self.state / 'forge.log', 'ab')
        self.service = subprocess.Popen([self.forge, '--home', str(self.home), 'serve',
                                         '--port', str(self.m['port'])], env=self.env,
                                        stdout=log, stderr=log)
        log.close()
        self.record('owned-forge', {'pid': self.service.pid, 'port': self.m['port']})
        def ready():
            if self.service.poll() is not None:
                raise RuntimeError('Forge startup failed; see forge.log')
            try:
                return self.cli('snapshot')
            except RuntimeError:
                return None
        self.until(ready, 60)

    def stop(self, crash=False):
        if self.service and self.service.poll() is None:
            self.service.send_signal(signal.SIGKILL if crash else signal.SIGTERM)
            try:
                self.service.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.service.kill()
                self.service.wait(timeout=5)

    def detail(self, run):
        return self.cli('runs', 'inspect', run)

    def settled(self, run):
        return self.until(lambda: (d if d['status'] not in ('ADMITTED', 'ENQUEUED', 'PENDING')
                                   else None) if (d := self.detail(run)) else None)

    def effect(self, run, detail):
        rows = [e for e in detail['effects'] if e['action']['type'] == 'harness']
        assert len(rows) == 1, rows
        return self.cli('runs', 'inspect', run, '--effect-id', rows[0]['effect_id'])['effect']

    def attempts(self):
        return sorted((self.home / 'state/harness').glob('*/attempt.json'))

    def attempt(self, run):
        rows = [p for p in self.attempts() if json.loads(p.read_text())['run_id'] == run]
        assert len(rows) == 1, rows
        return rows[0]

    def launched_once(self, detail):
        assert sum(l['action'] == 'harness_launched' for l in detail['logs']) == 1

    def definition(self, profile='native'):
        seed = 'NATIVE-QUALIFICATION'
        return {'type': 'workflow', 'input': {'steps': [
            {'id': 'seed', 'action': {'type': 'echo', 'input': {'text': seed}}},
            {'id': 'native', 'needs': ['seed'], 'action': {'type': 'harness', 'input': {
                'profile': profile, 'permissions': 'workspace-write',
                'require': ['structured_output', 'tool_use', 'session_correlation'],
                'prompt': 'Use read_file to read inputs.json. Use write_file to write evidence.txt '
                          'with exactly the seed value, no newline. Return only {"file":"evidence.txt",'
                          '"seed":"' + seed + '"}.',
                'inputs': {'seed': {'$ref': '/steps/seed/output/text'}},
                'output_schema': {'type': 'object', 'properties': {
                    'file': {'const': 'evidence.txt'}, 'seed': {'const': seed}},
                    'required': ['file', 'seed'], 'additionalProperties': False}}}},
            {'id': 'dependent', 'needs': ['native'], 'action': {'type': 'echo', 'input': {
                'text': 'Consumed ${/steps/native/output/result/seed}'}}}],
            'output': {'$ref': '/steps/dependent/output/text'}}}

    def verify(self, run, label):
        d = self.record(label + '-run', self.settled(run))
        assert d['status'] == 'SUCCESS', d['status']
        assert d['output']['output'] == 'Consumed NATIVE-QUALIFICATION', d['output']
        e = self.record(label + '-effect', self.effect(run, d))
        assert e['state'] == 'completed'
        output = e['output']
        assert output['result'] == {'file': 'evidence.txt', 'seed': 'NATIVE-QUALIFICATION'}
        h = output['harness']
        assert h['adapter'] == 'soulsteads' and h['tool_events'] >= 2
        assert h['session_id'] and h['native_run_id'] and h['config_sha256']
        artifact = next(a for a in output['artifacts'] if a['path'] == 'evidence.txt')
        assert artifact['text'] == 'NATIVE-QUALIFICATION'
        path = self.attempt(run)
        assert artifact['sha256'] == digest(path.parent / 'workspace/evidence.txt')
        self.launched_once(d)
        return {'run_id': run, 'effect': e, 'attempt_sha256': digest(path)}

    def run(self):
        for key in ('forge', 'harness'):
            assert digest(self.m[key]) == self.m[key + '_sha256'], key + ' artifact drift'
        assert digest(__file__) == self.m['driver_sha256'], 'driver changed after preparation'
        assert digest(self.home / 'config.json') == self.m['config_sha256'], 'profile drift'
        assert digest(self.state / 'hold-native.py') == self.m['wrapper_sha256'], 'wrapper drift'
        # O_EXCL prevents accidental model retries or reuse of an earlier cluster.
        with (self.state / 'started.json').open('x') as f:
            json.dump({'started': datetime.datetime.now(datetime.timezone.utc).isoformat()}, f)
        try:
            for port in (self.m['port'], self.m['pg_port']):
                with socket.socket() as s:
                    s.bind(('127.0.0.1', port))
            pgdata = self.state / 'postgres'
            command([self.pg_bin / 'initdb', '-D', pgdata, '-A', 'trust', '-U', 'forge',
                     '--no-locale', '--encoding=UTF8'])
            command([self.pg_bin / 'pg_ctl', '-D', pgdata, '-l', self.state / 'postgres.log',
                     '-o', f"-h 127.0.0.1 -p {self.m['pg_port']} -c unix_socket_directories=''",
                     '-w', 'start'])
            self.pg_started = True
            command([self.pg_bin / 'createdb', '-h', '127.0.0.1', '-p', self.m['pg_port'],
                     '-U', 'forge', 'qualification'])
            self.start()
            self.record('runners', self.cli('runners', 'list'))
            cases = self.evidence['cases']
            before = self.attempts()
            # Use resolved leaf inputs for registration-time refusal. Workflow
            # references defer leaf validation until their values are resolved.
            denied = self.definition('native-ro')['input']['steps'][1]['action']
            denied['input']['inputs'] = {'seed': 'NATIVE-QUALIFICATION'}
            cases['permission_ceiling'] = self.cli('work', 'register', 'denied-ceiling',
                                                    '--definition', '-', data=denied, denied=True)
            unsupported = self.definition()['input']['steps'][1]['action']
            unsupported['input']['inputs'] = {'seed': 'NATIVE-QUALIFICATION'}
            unsupported['input']['require'].append('mcp_tools')
            cases['required_capability'] = self.cli('work', 'register', 'denied-capability',
                                                    '--definition', '-', data=unsupported, denied=True)
            assert self.attempts() == before
            self.cli('work', 'register', 'native-chain', '--definition', '-', data=self.definition())
            manual = self.cli('work', 'run', 'native-chain', '--key', 'manual')['run_id']
            cases['manual'] = self.verify(manual, 'manual')
            schedules = self.cli('schedules', 'create', 'native-due', '--work-id', 'native-chain',
                                 '--cron', '* * * * *', '--tz', 'UTC')
            schedule = next(s for s in schedules if s['schedule_name'].endswith('.native-due'))
            self.schedule = schedule['schedule_name']
            self.record('scheduled-due', schedule)
            def fired():
                return next((r for r in self.cli('runs', 'list')
                             if r['work_id'] == 'native-chain' and r['run_id'] != manual), None)
            due_run = self.until(fired, 180)
            self.cli('schedules', 'pause', self.schedule)
            created = due_run['created'] / (1000 if due_run['created'] > 1e11 else 1)
            due = datetime.datetime.fromisoformat(schedule['next_run']).timestamp()
            assert created >= due - 2
            cases['actual_due'] = self.verify(due_run['run_id'], 'due')
            assert cases['manual']['effect']['output']['harness']['session_id'] != cases['actual_due']['effect']['output']['harness']['session_id']
            # Completed-receipt recovery: exact effect and attempt bytes, same key.
            self.stop(crash=True)
            self.start()
            assert self.cli('work', 'run', 'native-chain', '--key', 'manual')['run_id'] == manual
            for label, run in [('manual', manual), ('actual_due', due_run['run_id'])]:
                replay = self.verify(run, label + '-replay')
                assert replay == cases[label], 'completed receipt or attempt changed'
            cases['completed_replay'] = 'exact receipts/attempts; same keys; one launch each'
            # A genuine native path rejection is required, not a textual refusal.
            negative = copy.deepcopy(self.definition()['input']['steps'][1]['action'])
            negative['input']['inputs'] = {}
            negative['input']['prompt'] = ('Qualification of the native path guard: call write_file once with '
                'path ../denied-marker and content denied. This marker is inside the isolated fixture home '
                'but outside your granted workspace; the tool must refuse. Do not substitute another path. '
                'After the refusal return {"file":"evidence.txt","seed":"NATIVE-QUALIFICATION"}.')
            self.cli('work', 'register', 'native-denial', '--definition', '-', data=negative)
            run = self.cli('work', 'run', 'native-denial', '--key', 'denial')['run_id']
            d = self.record('denial-run', self.settled(run))
            e = self.record('denial-effect', self.effect(run, d))
            path = self.attempt(run)
            records = [json.loads(line) for p in (path.parent / 'native-home').rglob('*.jsonl')
                       for line in p.read_text().splitlines()]
            self.record('denial-native-journal', records)
            assert any(r.get('kind') == 'tool_result' and r.get('status') == 'denied' for r in records), 'no actual denied native tool result (inconclusive; do not retry automatically)'
            assert not (path.parent / 'denied-marker').exists()
            assert d['status'] == 'RECOVERY_REQUIRED' and e['state'] == 'uncertain'
            self.launched_once(d)
            cases['native_denial'] = {'run_id': run, 'effect_id': e['effect_id'], 'state': e['state']}
            # Real native completion with a withheld receipt, then Forge process loss.
            self.cli('work', 'register', 'native-fault', '--definition', '-', data=self.definition('native-hold'))
            run = self.cli('work', 'run', 'native-fault', '--key', 'fault')['run_id']
            def fault_ready():
                return next((p.parent / 'fault-ready.json' for p in self.attempts()
                             if json.loads(p.read_text())['run_id'] == run
                             and (p.parent / 'fault-ready.json').exists()), None)
            ready = self.until(fault_ready)
            native = json.loads(ready.read_text())
            self.record('fault-boundary', native)
            events = [json.loads(line) for line in (ready.parent / 'fault-native.stdout').read_text().splitlines()]
            assert native['exit_code'] == 0 and events[-1]['type'] == 'run_finished'
            assert events[-1]['status'] == 'completed'
            assert any(e['type'] == 'tool_result' and e['name'] == 'write_file' and e['status'] == 'ok' for e in events)
            assert (ready.parent / 'workspace/evidence.txt').read_text() == 'NATIVE-QUALIFICATION'
            before = self.record('fault-before', self.detail(run))
            assert self.effect(run, before)['state'] == 'uncertain'
            self.stop(crash=True)
            self.start()
            d = self.record('fault-after', self.settled(run))
            e = self.record('fault-effect', self.effect(run, d))
            assert d['status'] == 'RECOVERY_REQUIRED' and e['state'] == 'uncertain'
            assert self.cli('work', 'run', 'native-fault', '--key', 'fault')['run_id'] == run
            self.launched_once(self.detail(run))
            assert len(self.attempts()) == 4, 'replacement or unexpected native invocation'
            attempt = json.loads(self.attempt(run).read_text())
            assert attempt['state'] in ('orphan_terminated', 'process_lost'), attempt
            pid = Path('/proc') / str(native['wrapper_pid']) / 'stat'
            assert not pid.exists() or pid.read_text().rsplit(')', 1)[1].split()[19] != native['start_ticks'], 'fault wrapper survived recovery'
            cases['interruption_no_replacement'] = {'run_id': run, 'attempt': attempt, 'effect': e}
            self.evidence['status'] = 'PASS'
        except BaseException as error:
            self.evidence.update(status='FAIL', error=str(error))
            raise
        finally:
            # Preserve original failure even if cleanup also fails.
            errors = []
            if self.schedule and self.service and self.service.poll() is None:
                try:
                    self.cli('schedules', 'pause', self.schedule)
                except Exception as error:
                    errors.append(str(error))
            self.stop()
            if self.pg_started:
                try:
                    command([self.pg_bin / 'pg_ctl', '-D', self.state / 'postgres', '-m', 'fast', '-w', 'stop'])
                except Exception as error:
                    errors.append(str(error))
            self.evidence['cleanup_errors'] = errors
            if errors:
                self.evidence['status'] = 'FAIL'
            self.record('evidence', self.evidence)
        assert self.evidence['status'] == 'PASS', self.evidence
        print(json.dumps({'status': 'PASS', 'evidence': str(self.state / 'evidence.json')}))


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='command', required=True)
    prepare_parser = sub.add_parser('prepare', help='no services or model calls')
    prepare_parser.add_argument('--state', type=Path, required=True)
    prepare_parser.add_argument('--forge', type=Path, required=True)
    prepare_parser.add_argument('--harness', type=Path, required=True)
    prepare_parser.add_argument('--source-commit', required=True)
    prepare_parser.add_argument('--model', required=True)
    prepare_parser.add_argument('--base-url', required=True)
    prepare_parser.add_argument('--port', type=int, default=19161)
    prepare_parser.add_argument('--pg-port', type=int, default=19162)
    prepare_parser.add_argument('--containment', choices=['auto', 'cgroup', 'scan'], default='auto')
    run_parser = sub.add_parser('run', help='four real native sessions; run only with a released workload slot')
    run_parser.add_argument('--state', type=Path, required=True)
    run_parser.add_argument('--pg-bin', type=Path, required=True)
    a = p.parse_args()
    if a.command == 'prepare':
        prepare(a)
    else:
        Qualification(a.state, a.pg_bin).run()


if __name__ == '__main__':
    main()

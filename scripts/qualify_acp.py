"""Live qualification driver for ACP / Pi / one-shot harness profiles; standard library only.

Owns a fresh Forge home, PostgreSQL cluster and loopback ports under --state. Every
path, port and endpoint comes from arguments or the profiles file; nothing is
discovered from or written to an incumbent home. Real harnesses and models only.

  chain:<profile>   deterministic seed -> harness (read + write a file) -> dependent step
  deny:<profile>    asks the agent to write outside its workspace; records the Forge decision
  kill:<profile>    SIGKILLs Forge while the agent turn is running, restarts, and checks
                    RECOVERY_REQUIRED/uncertain, no replacement launch, orphan termination

No case retries a model call automatically; failures are recorded as they happened.
"""
import argparse
import hashlib
import json
import os
import signal
import socket
import subprocess
import time
from pathlib import Path

SEED = 'ACP-QUALIFICATION'


def save(path, value):
    path.write_text(json.dumps(value, indent=2, default=str) + '\n')


def run(args, **kw):
    r = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=180, **kw)
    if r.returncode:
        raise RuntimeError(f'{args[0]} exited {r.returncode}: {r.stderr[-500:]}')
    return r.stdout


class Driver:
    def __init__(self, a):
        self.a = a
        self.state = a.state.resolve()
        self.ev = a.evidence.resolve()
        self.ev.mkdir(parents=True, exist_ok=True)
        self.home = self.state / 'home'
        self.forge = str(a.forge.resolve(strict=True))
        self.pg_bin = a.pg_bin.resolve(strict=True)
        self.service = None
        self.env = dict(os.environ)
        self.env['FORGE_DATABASE_URL'] = f'postgresql://forge@127.0.0.1:{a.pg_port}/{a.database}'
        self.results = {}

    def cli(self, *args, data=None):
        r = subprocess.run([self.forge, '--url', f'http://127.0.0.1:{self.a.port}', *args],
                           input=json.dumps(data) if data is not None else None,
                           env=self.env, capture_output=True, text=True, timeout=180)
        if r.returncode:
            raise RuntimeError(f'forge {args[:2]} failed: {r.stderr[-400:]}')
        return json.loads(r.stdout)

    def until(self, fn, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if (v := fn()):
                return v
            time.sleep(.3)
        raise TimeoutError('condition did not settle')

    def prepare(self):
        self.state.mkdir(mode=0o700, parents=True, exist_ok=True)
        if not self.home.exists():
            run([self.forge, '--home', self.home, 'init'])
        cfg_path = self.home / 'config.json'
        cfg = json.loads(cfg_path.read_text())
        cfg['runners'] = json.loads(self.a.profiles.read_text())
        save(cfg_path, cfg)
        self.env['FORGE_TOKEN'] = cfg['actors']['operator']['token']
        for port in (self.a.port, self.a.pg_port):
            with socket.socket() as s:
                s.bind(('127.0.0.1', port))
        data = self.state / 'postgres'
        if not data.exists():
            run([self.pg_bin / 'initdb', '-D', data, '-A', 'trust', '-U', 'forge', '--no-locale', '--encoding=UTF8'])
        run([self.pg_bin / 'pg_ctl', '-D', data, '-l', self.state / 'postgres.log', '-o',
             f"-h 127.0.0.1 -p {self.a.pg_port} -c unix_socket_directories=''", '-w', 'start'])
        run([self.pg_bin / 'createdb', '-h', '127.0.0.1', '-p', self.a.pg_port, '-U', 'forge', self.a.database])

    def start(self):
        log = open(self.state / 'forge.log', 'ab')
        self.service = subprocess.Popen([self.forge, '--home', str(self.home), 'serve', '--port', str(self.a.port)],
                                        env=self.env, stdout=log, stderr=log)
        log.close()
        def ready():
            if self.service.poll() is not None:
                raise RuntimeError('Forge exited during startup; see forge.log')
            try:
                return self.cli('snapshot')
            except RuntimeError:
                return None
        self.until(ready, 90)

    def stop(self, kill=False):
        if self.service and self.service.poll() is None:
            self.service.send_signal(signal.SIGKILL if kill else signal.SIGTERM)
            try:
                self.service.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.service.kill()
                self.service.wait(timeout=5)

    def definition(self, profile, prompt, perms='workspace-write', require=('structured_output',)):
        return {'type': 'workflow', 'input': {'steps': [
            {'id': 'seed', 'action': {'type': 'echo', 'input': {'text': SEED}}},
            {'id': 'agent', 'needs': ['seed'], 'action': {'type': 'harness', 'input': {
                'profile': profile, 'permissions': perms, 'require': list(require),
                'prompt': prompt, 'inputs': {'seed': {'$ref': '/steps/seed/output/text'}},
                'output_schema': {'type': 'object', 'properties': {
                    'file': {'const': 'evidence.txt'}, 'seed': {'const': SEED}},
                    'required': ['file', 'seed'], 'additionalProperties': False}}}},
            {'id': 'dependent', 'needs': ['agent'], 'action': {'type': 'echo', 'input': {
                'text': 'Consumed ${/steps/agent/output/result/seed}'}}}],
            'output': {'$ref': '/steps/dependent/output/text'}}}

    def settled(self, run_id, seconds=600):
        def done():
            d = self.cli('runs', 'inspect', run_id)
            return d if d['status'] not in ('ADMITTED', 'ENQUEUED', 'PENDING') else None
        return self.until(done, seconds)

    def attempt_for(self, run_id):
        for p in sorted((self.home / 'state/harness').glob('*/attempt.json')):
            if json.loads(p.read_text())['run_id'] == run_id:
                return p
        return None

    def effect(self, run_id, detail):
        rows = [e for e in detail['effects'] if e['action']['type'] == 'harness']
        assert len(rows) == 1, rows
        return self.cli('runs', 'inspect', run_id, '--effect-id', rows[0]['effect_id'])['effect']

    def copy_evidence(self, label, run_id, detail, effect):
        save(self.ev / f'{label}-run.json', detail)
        save(self.ev / f'{label}-effect.json', effect)
        p = self.attempt_for(run_id)
        if p:
            save(self.ev / f'{label}-attempt.json', json.loads(p.read_text()))
            log = p.parent / 'updates.jsonl'
            if log.exists():
                (self.ev / f'{label}-updates.jsonl').write_text(log.read_text())
            ws = p.parent / 'workspace'
            if (ws / 'evidence.txt').exists():
                (self.ev / f'{label}-evidence.txt').write_text((ws / 'evidence.txt').read_text())

    def prompt_chain(self):
        return ('Read the file inputs.json in your current working directory to get the seed value. '
                'Then create a file named evidence.txt in your current working directory whose content is exactly '
                'the seed value with no trailing newline. When done, reply with only this JSON and nothing else: '
                '{"file":"evidence.txt","seed":"<the seed value>"}. Use only file read and file write tools; do not run shell commands.')

    def case_chain(self, profile):
        name = f'chain-{profile}'
        self.cli('work', 'register', name, '--definition', '-',
                 data=self.definition(profile, self.prompt_chain(), require=('structured_output', 'tool_use')))
        run_id = self.cli('work', 'run', name, '--key', name)['run_id']
        d = self.settled(run_id)
        e = self.effect(run_id, d)
        self.copy_evidence(name, run_id, d, e)
        ok = d['status'] == 'SUCCESS' and e['state'] == 'completed'
        out = e.get('output') or {}
        harness = out.get('harness', {})
        launches = sum(l['action'] == 'harness_launched' for l in d['logs'])
        res = {'status': d['status'], 'effect_state': e['state'], 'launches': launches,
               'transport': harness.get('transport'), 'adapter': harness.get('adapter'),
               'version': harness.get('version'), 'tool_events': harness.get('tool_events'),
               'session_id': harness.get('session_id'), 'served_model': harness.get('served_model'),
               'result': out.get('result'), 'dependent': (d.get('output') or {}).get('output'),
               'permission_decisions': harness.get('permission_decisions'),
               'passed': bool(ok and out.get('result') == {'file': 'evidence.txt', 'seed': SEED} and launches == 1)}
        self.results[name] = res
        return res

    def case_deny(self, profile):
        name = f'deny-{profile}'
        prompt = ('Step 1: create a file at the path ../outside-marker.txt (one directory ABOVE your working directory) '
                  'containing the word denied. If that is refused or fails, do not try any other location. '
                  'Step 2: create evidence.txt in your current working directory containing exactly the seed value from '
                  'inputs.json with no trailing newline. Finally reply with only {"file":"evidence.txt","seed":"<the seed value>"}.')
        self.cli('work', 'register', name, '--definition', '-', data=self.definition(profile, prompt))
        run_id = self.cli('work', 'run', name, '--key', name)['run_id']
        d = self.settled(run_id)
        e = self.effect(run_id, d)
        self.copy_evidence(name, run_id, d, e)
        p = self.attempt_for(run_id)
        attempt = json.loads(p.read_text()) if p else {}
        decisions = attempt.get('permission_decisions') or []
        marker = (p.parent / 'outside-marker.txt') if p else None
        res = {'status': d['status'], 'effect_state': e['state'], 'decisions': decisions,
               'outside_marker_exists': bool(marker and marker.exists()),
               'passed': bool(any(x['decision'] == 'denied' for x in decisions) and not (marker and marker.exists()))}
        self.results[name] = res
        return res

    def case_kill(self, profile):
        name = f'kill-{profile}'
        prompt = ('Create a file named evidence.txt in your current working directory containing exactly the seed value '
                  'from inputs.json (read inputs.json first). Then, before replying, read inputs.json five more times, '
                  'one call at a time. Use only file read and file write tools. Finally reply with only {"file":"evidence.txt","seed":"<the seed value>"}.')
        self.cli('work', 'register', name, '--definition', '-', data=self.definition(profile, prompt))
        run_id = self.cli('work', 'run', name, '--key', name)['run_id']
        def running():
            p = self.attempt_for(run_id)
            if not p:
                return None
            a = json.loads(p.read_text())
            return a if a.get('state') == 'running' and a.get('pid') else None
        before = self.until(running, 120)
        # Let the real agent get into its turn (streamed updates observed) before the loss.
        log = Path(self.attempt_for(run_id)).parent / 'updates.jsonl'
        self.until(lambda: log.exists() and log.stat().st_size > 0, 120)
        save(self.ev / f'{name}-before-kill-attempt.json', before)
        self.stop(kill=True)
        self.start()
        d = self.settled(run_id, 180)
        e = self.effect(run_id, d)
        self.copy_evidence(name, run_id, d, e)
        again = self.cli('work', 'run', name, '--key', name)['run_id']
        d2 = self.cli('runs', 'inspect', run_id)
        launches = sum(l['action'] == 'harness_launched' for l in d2['logs'])
        a = json.loads(self.attempt_for(run_id).read_text())
        pid = before['pid']
        alive = Path(f'/proc/{pid}/stat').exists() and ') Z ' not in Path(f'/proc/{pid}/stat').read_text()
        res = {'status': d['status'], 'effect_state': e['state'], 'attempt_state': a['state'], 'launches': launches,
               'same_run_on_duplicate_key': again == run_id, 'agent_process_alive_after_recovery': alive,
               'passed': bool(d['status'] == 'RECOVERY_REQUIRED' and e['state'] == 'uncertain' and launches == 1
                              and a['state'] in ('orphan_terminated', 'process_lost') and again == run_id and not alive)}
        self.results[name] = res
        return res

    def stop_postgres(self):
        """Stop the cluster this driver owns, whatever happened before (including a failed
        prepare); fall back to the postmaster pid when pg_ctl cannot stop it."""
        data = self.state / 'postgres'
        if not (data / 'postmaster.pid').exists():
            return
        try:
            run([self.pg_bin / 'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'])
        except Exception as error:
            self.results['pg_stop_error'] = str(error)
            try:
                os.kill(int((data / 'postmaster.pid').read_text().splitlines()[0]), signal.SIGKILL)
            except (OSError, ValueError, IndexError):
                pass

    def main(self):
        # SIGTERM/SIGINT unwind through `finally` like any other exit.
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, lambda *_, s=sig: (_ for _ in ()).throw(SystemExit(128 + s)))
        try:
            self.prepare()
            self.start()
            for spec in self.a.cases:
                kind, profile = spec.split(':', 1)
                try:
                    getattr(self, f'case_{kind}')(profile)
                except Exception as error:  # recorded, never retried
                    self.results[f'{kind}-{profile}'] = {'passed': False, 'error': str(error)[:500]}
                save(self.ev / 'results.json', self.results)
        finally:
            self.stop()
            self.stop_postgres()
            save(self.ev / 'results.json', self.results)
        print(json.dumps({k: v.get('passed') if isinstance(v, dict) else v for k, v in self.results.items()}))


if __name__ == '__main__':
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--evidence', type=Path, required=True)
    p.add_argument('--forge', type=Path, required=True)
    p.add_argument('--pg-bin', type=Path, required=True)
    p.add_argument('--profiles', type=Path, required=True, help='JSON object: profile name -> runner profile')
    p.add_argument('--port', type=int, required=True)
    p.add_argument('--pg-port', type=int, required=True)
    p.add_argument('--database', default='forge_acp')
    p.add_argument('cases', nargs='+', help='kind:profile, kind in chain|deny|kill')
    Driver(p.parse_args()).main()

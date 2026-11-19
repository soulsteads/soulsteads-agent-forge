"""Live upgrade check: a home written by an OLD build keeps working under a NEW build.

Creates a fresh Forge home and PostgreSQL cluster under --state, drives the old binary
(create work with deliberately unsorted keys, run a flow with params, run plain plugin
work, create a schedule, leave one flow effect in flight and SIGKILL the service), then
starts the new binary on the SAME home and database and verifies:

  * idempotent re-registration of the same definitions (any key order) is accepted;
  * re-running the same keys returns the same run ids and executes nothing again
    (duplicate-key dedup still works, params digests are unchanged);
  * the in-flight flow effect is refused as unresolved (RECOVERY_REQUIRED), never with
    "immutable flow effect binding changed", and is never relaunched;
  * new work still runs and an old schedule is still readable.

Standard library only. Real binaries, a real PostgreSQL, a real plugin process.
"""
import argparse
import json
import os
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

PLUGIN = r'''#!/usr/bin/env python3
import json, os, sys, time
r = json.load(sys.stdin)
op = r.get("operation")
if op == "describe":
    print(json.dumps({"api_version": 1, "description": "upgrade fixture", "schema": {
        "type": "object", "properties": {"mode": {"type": "string"}, "tag": {"type": "string"}}, "required": ["tag"],
        "additionalProperties": False}}))
elif op == "execute":
    with open(os.environ["UPGRADE_RECEIPTS"], "a") as f:
        f.write(json.dumps({"tag": r["input"]["tag"], "run": r["context"]["run_id"]}) + "\n")
    if r["input"].get("mode") == "hold":
        time.sleep(60)
    print(json.dumps({"api_version": 1, "output": {"tag": r["input"]["tag"]}}))
else:
    print(json.dumps({"api_version": 1}))
'''


def run(args, **kw):
    r = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=180, **kw)
    if r.returncode:
        raise RuntimeError(f'{args[0]} exited {r.returncode}: {r.stderr[-400:]}')
    return r.stdout


class Check:
    def __init__(self, a):
        self.a = a
        self.state = a.state.resolve()
        self.state.mkdir(parents=True, exist_ok=True)
        self.ev = a.evidence.resolve()
        self.ev.mkdir(parents=True, exist_ok=True)
        self.home = self.state / 'home'
        self.receipts = self.state / 'receipts.jsonl'
        self.url = f'http://127.0.0.1:{a.port}'
        self.proc = None
        self.results = []
        self.env = dict(os.environ, UPGRADE_RECEIPTS=str(self.receipts),
                        FORGE_DATABASE_URL=f'postgresql://forge@127.0.0.1:{a.pg_port}/forge_upgrade')

    # -- infrastructure -------------------------------------------------------------
    def postgres(self):
        data = self.state / 'postgres'
        pg = self.a.pg_bin.resolve(strict=True)
        for port in (self.a.port, self.a.pg_port):
            with socket.socket() as s:
                s.bind(('127.0.0.1', port))
        run([pg / 'initdb', '-D', data, '-A', 'trust', '-U', 'forge', '--no-locale', '--encoding=UTF8'])
        run([pg / 'pg_ctl', '-D', data, '-l', self.state / 'postgres.log', '-o',
             f"-h 127.0.0.1 -p {self.a.pg_port} -c unix_socket_directories=''", '-w', 'start'])
        run([pg / 'createdb', '-h', '127.0.0.1', '-p', self.a.pg_port, '-U', 'forge', 'forge_upgrade'])

    def stop_postgres(self):
        data = self.state / 'postgres'
        if (data / 'postmaster.pid').exists():
            try:
                run([self.a.pg_bin.resolve() / 'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'])
            except Exception:
                os.kill(int((data / 'postmaster.pid').read_text().splitlines()[0]), signal.SIGKILL)

    def start(self, binary, label):
        log = open(self.state / f'server-{label}.log', 'ab')
        self.proc = subprocess.Popen([binary, '--home', str(self.home), 'serve', '--port', str(self.a.port)],
                                     env=self.env, stdout=log, stderr=log)
        log.close()
        for _ in range(240):
            if self.proc.poll() is not None:
                raise RuntimeError(f'{label} server exited during startup; see server-{label}.log')
            try:
                urllib.request.urlopen(self.url, timeout=1).read()
                return
            except OSError:
                time.sleep(.25)
        raise RuntimeError('server startup timeout')

    def stop(self, kill=False):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGKILL if kill else signal.SIGTERM)
            self.proc.wait(timeout=30)

    def call(self, method, ok=True, **params):
        req = urllib.request.Request(self.url + '/api', data=json.dumps({'method': method, 'params': params}).encode(),
                                     headers={'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                body = json.load(r)
        except urllib.error.HTTPError as error:
            if ok:
                raise AssertionError((method, error.code, error.read()[:300]))
            return error.code
        assert ok, (method, 'unexpected success')
        return body['result']

    def wait(self, run_id, accept):
        for _ in range(240):
            row = self.call('inspect', run_id=run_id)
            if row['status'] not in ('PENDING', 'ENQUEUED', 'ADMITTED', 'DELAYED'):
                assert row['status'] in accept, (row['status'], accept)
                return row
            time.sleep(.25)
        raise AssertionError('run timeout')

    def lines(self):
        return [json.loads(x) for x in self.receipts.read_text().splitlines()] if self.receipts.exists() else []

    def check(self, name, passed, detail=''):
        self.results.append({'check': name, 'passed': bool(passed), 'detail': str(detail)[:400]})
        print(('PASS ' if passed else 'FAIL ') + name, detail if not passed else '')

    # -- the scenario -----------------------------------------------------------------
    def main(self):
        try:
            self.postgres()
            plugin = self.state / 'plugin.py'
            plugin.write_text(PLUGIN)
            plugin.chmod(0o700)
            old = str(self.a.old.resolve(strict=True))
            new = str(self.a.new.resolve(strict=True))
            subprocess.run([old, '--home', str(self.home), 'init'], check=True, capture_output=True)
            cfg_path = self.home / 'config.json'
            cfg = json.loads(cfg_path.read_text())
            cfg['plugins'] = {'plugin.up': {'command': [str(plugin)], 'timeout_seconds': 90}}
            cfg['actors']['operator']['types'].append('plugin.up')
            cfg_path.write_text(json.dumps(cfg))
            self.token = cfg['actors']['operator']['token']

            # ---- OLD build: records everything with unsorted keys
            self.start(old, 'old')
            flow = {'type': 'workflow', 'input': {
                'steps': [{'id': 'b', 'action': {'type': 'echo', 'input': {'text': {'$ref': '/params/zeta'}}}},
                          {'id': 'a', 'needs': ['b'], 'action': {'type': 'plugin.up', 'input': {'tag': 'flow', 'mode': 'ok'}}}],
                'params': {'zeta': 'z', 'alpha': 'a'}, 'output': {'r': {'$ref': '/steps/a/output/tag'}}}}
            hold = {'type': 'workflow', 'input': {
                'steps': [{'id': 'h', 'action': {'type': 'plugin.up', 'input': {'tag': 'hold', 'mode': 'hold'}}}]}}
            self.call('register', work_id='flow', definition=flow)
            self.call('register', work_id='flow-hold', definition=hold)
            self.call('register', work_id='plugin-ok', definition={'type': 'plugin.up', 'input': {'tag': 'direct', 'mode': 'ok'}})
            params = {'zeta': 'z1', 'alpha': {'y': 2, 'x': 1}}
            flow_run = self.call('run', work_id='flow', key='k-flow', params=params)['run_id']
            plain_run = self.call('run', work_id='plugin-ok', key='k-plugin')['run_id']
            self.wait(flow_run, ('SUCCESS',))
            self.wait(plain_run, ('SUCCESS',))
            self.call('schedule', name='old-sched', work_id='flow', cron='0 3 * * *', tz='UTC', params=params)
            before = len(self.lines())
            hold_run = self.call('run', work_id='flow-hold', key='k-hold')['run_id']
            for _ in range(120):
                if any(x['tag'] == 'hold' for x in self.lines()):
                    break
                time.sleep(.25)
            self.check('old build: flow, plain plugin work, schedule and an in-flight effect recorded',
                       len(self.lines()) == before + 1, self.lines())
            self.stop(kill=True)
            subprocess.run(['pkill', '-9', '-f', str(plugin)], check=False)
            total = len(self.lines())

            # ---- NEW build on the SAME home and database
            self.start(new, 'new')
            def attempt(name, action):
                """Record one check; a refusal is a FAIL result, never a crash of the driver."""
                try:
                    outcome = action()
                    self.check(name, True if outcome is None else outcome[0], '' if outcome is None else outcome[1])
                except Exception as error:
                    self.check(name, False, error)

            # Same insertion order as the first registration, which is NOT sorted: a build whose
            # JSON encoding follows insertion order (serde_json preserve_order) would see a
            # different definition text here than the sorted text the old build stored.
            attempt('re-registering an old (unsorted-key) definition is idempotent',
                    lambda: self.call('register', work_id='flow', definition=flow) and None)
            ids = {}

            def rerun():
                ids['flow'] = self.call('run', work_id='flow', key='k-flow', params=params)['run_id']
                ids['plain'] = self.call('run', work_id='plugin-ok', key='k-plugin')['run_id']
                return ids['flow'] == flow_run and ids['plain'] == plain_run, ids
            attempt('same key and params return the SAME run ids', rerun)
            time.sleep(1)
            attempt('duplicate keys executed nothing again', lambda: (len(self.lines()) == total, self.lines()))

            def hold_state():
                held = self.wait(hold_run, ('RECOVERY_REQUIRED', 'ERROR'))
                self.held = held
                return held['status'] == 'RECOVERY_REQUIRED' and len(self.lines()) == total, held['status']
            attempt('in-flight effect is unresolved (RECOVERY_REQUIRED), not relaunched', hold_state)
            logtext = (self.state / 'server-new.log').read_text() + json.dumps(getattr(self, 'held', {}))
            self.check('no "binding changed" refusal anywhere', 'binding changed' not in logtext)

            def fresh():
                run_id = self.call('run', work_id='flow', key='k-new', params=params)['run_id']
                self.wait(run_id, ('SUCCESS',))
                return len(self.lines()) == total + 1, len(self.lines())
            attempt('new key runs to SUCCESS on the new build', fresh)
            attempt('schedule created by the old build is still readable',
                    lambda: (any(s['schedule_name'].endswith('old-sched') for s in self.call('snapshot')['schedules']), ''))
            try:
                (self.ev / 'hold-inspect.json').write_text(json.dumps(self.call('inspect', run_id=hold_run), indent=2, default=str))
            except Exception:
                pass
        finally:
            self.stop()
            self.stop_postgres()
            (self.ev / 'upgrade-results.json').write_text(json.dumps(self.results, indent=2))
        failed = [r for r in self.results if not r['passed']]
        print(json.dumps({'passed': len(self.results) - len(failed), 'failed': len(failed)}))
        raise SystemExit(1 if failed or not self.results else 0)


if __name__ == '__main__':
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--old', type=Path, required=True, help='binary of the previous build')
    p.add_argument('--new', type=Path, required=True, help='binary of the candidate build')
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--evidence', type=Path, required=True)
    p.add_argument('--pg-bin', type=Path, required=True)
    p.add_argument('--port', type=int, required=True)
    p.add_argument('--pg-port', type=int, required=True)
    Check(p.parse_args()).main()

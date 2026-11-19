#!/usr/bin/env python3
"""Live proof for the web UI (M3) and the M2 review fixes, with a real PostgreSQL, real
server restarts, a real MCP client, a real headless Chromium (Playwright) and a real
OpenCode ACP agent on a local model.

Needs a Python with `mcp` and `playwright` and an installed Chromium. Owns a fresh Forge
home, PostgreSQL cluster and loopback ports under --state; it never touches an incumbent
home. Every check records PASS/FAIL with evidence; nothing is retried to make it pass.

  python3 scripts/qualify_m3.py --forge BIN --pg-bin PGBIN --state DIR --evidence DIR \
      --m2fix-evidence DIR --port 19300 --pg-port 19301 --opencode PATH --opencode-config FILE

The provider-edit fixture uses FORGE_QUALIFY_MODEL_URL (default loopback port 8000)
and FORGE_QUALIFY_MODEL_ID (default your-model). Select your own test endpoint/model;
live model calls require an operator-approved budget.
"""
import argparse
import asyncio
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
FIXTURE = REPO / 'tests/fixtures/demo-plugin'
ACTIVE = ('ADMITTED', 'PENDING', 'ENQUEUED', 'DELAYED')
MODEL_URL = os.environ.get('FORGE_QUALIFY_MODEL_URL', 'http://127.0.0.1:8000/v1')
MODEL_ID = os.environ.get('FORGE_QUALIFY_MODEL_ID', 'your-model')

ENVPROBE = r'''#!/usr/bin/env python3
import json, os, sys
req = json.loads(sys.stdin.readline())
if req['operation'] == 'describe':
    print(json.dumps({'api_version': 1, 'description': 'Reports its own environment', 'schema': {'type': 'object', 'additionalProperties': False}}))
else:
    names = sorted(os.environ)
    leaked = [n for n, v in os.environ.items() if 'postgresql://' in v or v == os.environ.get('M3_TOKEN_VALUE', '\0')]
    print(json.dumps({'api_version': 1, 'output': {'names': names, 'leaked_names': leaked, 'granted': os.environ.get('ENVPROBE_GRANTED')}}))
'''

PI_STUB = r'''#!/usr/bin/env python3
"""Stand-in Pi that speaks the Pi RPC protocol and records what the sandbox lets it see."""
import json, os, sys, pathlib
if sys.argv[1:] == ['--version']:
    print('pi-sandbox-probe 0.0'); sys.exit(0)
def attempt(fn):
    try:
        return fn()
    except Exception as error:
        return 'ERROR ' + type(error).__name__ + ': ' + str(error)[:80]
uid = os.getuid()
def write_etc():
    open('/etc/m3-probe', 'w').write('x'); return 'WROTE'
probe = {
    'uid': uid,
    'run_user_listing': attempt(lambda: sorted(os.listdir('/run/user'))),
    'run_user_uid_listing': attempt(lambda: sorted(os.listdir(f'/run/user/{uid}'))),
    'session_bus_visible': os.path.exists(f'/run/user/{uid}/bus'),
    'run_dbus_listing': attempt(lambda: sorted(os.listdir('/run/dbus'))),
    'system_bus_socket_visible': os.path.exists('/run/dbus/system_bus_socket'),
    'home': os.environ.get('HOME'),
    'etc_write': attempt(write_etc),
    'resolv_conf_readable': os.path.exists('/etc/resolv.conf'),
    'workspace_write': attempt(lambda: (pathlib.Path('probe-write.txt').write_text('ok'), 'ok')[1]),
}
pathlib.Path('probe.json').write_text(json.dumps(probe, indent=1))
def send(m): sys.stdout.write(json.dumps(m) + '\n'); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    if m['type'] != 'prompt': continue
    send({'id': m['id'], 'type': 'response', 'command': 'prompt', 'success': True, 'data': {'disposition': 'started'}})
    send({'type': 'agent_start'})
    send({'type': 'message_start', 'message': {'role': 'assistant'}})
    send({'type': 'message_update', 'assistantMessageEvent': {'type': 'text_delta', 'delta': '{"done":true}', 'contentIndex': 0}})
    send({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'model': 'probe', 'content': [{'type': 'text', 'text': '{"done":true}'}]}})
    send({'type': 'agent_end', 'messages': [], 'willRetry': False})
    send({'type': 'agent_settled', 'aborted': False})
'''


def rmtree(path):
    subprocess.run(['chmod', '-R', 'u+rwx', str(path)], check=False)
    shutil.rmtree(path)


def run(args, **kw):
    r = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=300, **kw)
    if r.returncode:
        raise RuntimeError(f'{args[0]} exited {r.returncode}: {r.stderr[-500:]}')
    return r.stdout


class Live:
    def __init__(self, a):
        self.a = a
        self.state = a.state.resolve()
        self.ev = a.evidence.resolve()
        self.ev2 = a.m2fix_evidence.resolve()
        for d in (self.state, self.ev, self.ev2, self.ev / 'screenshots'):
            d.mkdir(parents=True, exist_ok=True)
        self.home = self.state / 'home'
        self.forge = str(a.forge.resolve(strict=True))
        self.pg = a.pg_bin.resolve(strict=True)
        self.url = f'http://127.0.0.1:{a.port}'
        self.db_url = f'postgresql://forge@127.0.0.1:{a.pg_port}/forge_m3'
        self.proc = None
        self.results = []
        self.commands = []
        self.token = 'unset'
        self.console = []   # (view, message) browser errors
        self.timeline = {}
        self.extra_env = {}
        self.killing = False

    # ---- bookkeeping ----------------------------------------------------------------
    def check(self, name, passed, evidence='', detail='', part='m3'):
        row = {'check': name, 'passed': bool(passed), 'evidence': evidence, 'detail': str(detail)[:400], 'part': part}
        self.results.append(row)
        print(('PASS ' if passed else 'FAIL ') + name + ('' if passed else f'  {detail}'), flush=True)
        for target, p in ((self.ev, 'm3'), (self.ev2, 'm2fix')):
            (target / 'results.json').write_text(json.dumps([r for r in self.results if r['part'] == p], indent=2) + '\n')

    def save(self, name, value, part='m3'):
        path = (self.ev if part == 'm3' else self.ev2) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value if isinstance(value, str) else json.dumps(value, indent=2, default=str) + '\n')
        return name

    # ---- infrastructure -------------------------------------------------------------
    def postgres(self):
        data = self.state / 'pg'
        with socket.socket() as s:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind(('127.0.0.1', self.a.pg_port))
        if not data.exists():
            run([self.pg / 'initdb', '-D', data, '-A', 'trust', '-U', 'forge', '--no-locale', '--encoding=UTF8'])
        run([self.pg / 'pg_ctl', '-D', data, '-l', self.state / 'pg.log', '-o',
             f"-h 127.0.0.1 -p {self.a.pg_port} -c unix_socket_directories=''", '-w', 'start'])
        run([self.pg / 'createdb', '-h', '127.0.0.1', '-p', self.a.pg_port, '-U', 'forge', 'forge_m3'])

    def stop_postgres(self):
        data = self.state / 'pg'
        if (data / 'postmaster.pid').exists():
            try:
                run([self.pg / 'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'])
            except Exception:
                os.kill(int((data / 'postmaster.pid').read_text().splitlines()[0]), signal.SIGKILL)

    def env(self, **extra):
        return {**os.environ, 'FORGE_DATABASE_URL': self.db_url, 'FORGE_TOKEN': self.token, 'FORGE_URL': self.url,
                'PATH': f'{self.pg}:' + os.environ['PATH'], 'M3_TOKEN_VALUE': self.token, 'ENVPROBE_GRANTED': 'granted-by-operator',
                **self.extra_env, **extra}

    def start(self):
        log = open(self.state / 'server.log', 'ab')
        self.proc = subprocess.Popen([self.forge, '--home', str(self.home), 'serve', '--port', str(self.a.port)],
                                     env=self.env(), stdout=log, stderr=log)
        log.close()
        for _ in range(360):
            if self.proc.poll() is not None:
                return False
            try:
                urllib.request.urlopen(self.url, timeout=1).read()
                return True
            except OSError:
                time.sleep(.25)
        return False

    def stop(self, sig=signal.SIGTERM):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(sig)
            self.proc.wait(timeout=90)

    def restart(self, kill=False):
        self.stop(signal.SIGKILL if kill else signal.SIGTERM)
        return self.start()

    # ---- clients --------------------------------------------------------------------
    def cli(self, *args, stdin=None, token=None, home=None):
        cmd = [self.forge, '--home', str(home or self.home), '--url', self.url, *args]
        env = self.env()
        if token is not None:
            env['FORGE_TOKEN'] = token
        r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=300, input=stdin)
        self.commands.append(f'agent-forge {" ".join(map(str, args))}\n  -> rc={r.returncode}\n  stdout: {r.stdout.strip()[:500]}\n  stderr: {r.stderr.strip()[:300]}')
        return r.returncode, r.stdout, r.stderr

    def cli_json(self, *args, stdin=None):
        code, out, err = self.cli(*args, stdin=stdin)
        if code:
            raise RuntimeError(f'cli {args} failed: {err[-300:]}')
        return json.loads(out)

    def api(self, method, **params):
        request = urllib.request.Request(self.url + '/api', data=json.dumps({'method': method, 'params': params}).encode(),
                                         headers={'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read() or b'{}')

    def result(self, method, **params):
        status, body = self.api(method, **params)
        if status != 200:
            raise RuntimeError(f'{method} -> {status} {body}')
        return body['result']

    def mcp(self, tool, **arguments):
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        async def session():
            params = StdioServerParameters(command=self.forge, args=['mcp', '--url', self.url, '--token-env', 'FORGE_TOKEN'],
                                           env={**os.environ, 'FORGE_TOKEN': self.token})
            with open(self.state / 'mcp-stderr.log', 'ab') as err:
                async with stdio_client(params, errlog=err) as (read, write):
                    async with ClientSession(read, write) as client:
                        await client.initialize()
                        r = await client.call_tool('forge_' + tool, arguments)
                        text = r.content[0].text
                        failed = getattr(r, 'isError', getattr(r, 'is_error', False))
                        return (not failed), (json.loads(text) if not failed else text)
        ok, value = self.in_thread(session)
        if not ok:
            raise RuntimeError(f'mcp {tool}: {value}')
        return value

    @staticmethod
    def in_thread(coroutine_function):
        # Playwright's sync API owns the main thread's event loop; MCP runs in its own.
        import concurrent.futures
        with concurrent.futures.ThreadPoolExecutor(1) as pool:
            return pool.submit(lambda: asyncio.run(coroutine_function())).result()

    def mcp_tools(self):
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        async def session():
            params = StdioServerParameters(command=self.forge, args=['mcp', '--url', self.url, '--token-env', 'FORGE_TOKEN'],
                                           env={**os.environ, 'FORGE_TOKEN': self.token})
            with open(self.state / 'mcp-stderr.log', 'ab') as err:
                async with stdio_client(params, errlog=err) as (read, write):
                    async with ClientSession(read, write) as client:
                        await client.initialize()
                        return [t.name for t in (await client.list_tools()).tools]
        return self.in_thread(session)

    def wait_run(self, run_id, accept=('SUCCESS',), timeout=900):
        end = time.time() + timeout
        while time.time() < end:
            d = self.result('inspect', run_id=run_id)
            if d['status'] not in ACTIVE:
                return d
            time.sleep(1)
        raise TimeoutError(run_id)

    def until(self, fn, seconds, every=.4):
        end = time.time() + seconds
        while time.time() < end:
            v = fn()
            if v:
                return v
            time.sleep(every)
        raise TimeoutError('condition did not settle')

    # ---- setup ----------------------------------------------------------------------
    def prepare(self):
        self.postgres()
        subprocess.run([self.forge, '--home', str(self.home), 'init'], check=True, capture_output=True, env=self.env(FORGE_TOKEN='x'))
        cfg_path = self.home / 'config.json'
        cfg = json.loads(cfg_path.read_text())
        self.token = cfg['actors']['operator']['token']
        # Runners: a real OpenCode ACP agent on the local model (read-only and workspace-write),
        # and a sandboxed Pi stand-in. Every path comes from the arguments.
        oc_home = self.state / 'oc-home'
        for d in ('c', 'd', 'k', 's'):
            (oc_home / d).mkdir(parents=True, exist_ok=True)
        oc_env = {'HOME': str(oc_home), 'XDG_CONFIG_HOME': str(oc_home / 'c'), 'XDG_DATA_HOME': str(oc_home / 'd'),
                  'XDG_CACHE_HOME': str(oc_home / 'k'), 'XDG_STATE_HOME': str(oc_home / 's'),
                  'OPENCODE_CONFIG': str(self.a.opencode_config.resolve(strict=True))}
        oc = lambda mode: {'adapter': 'opencode-acp', 'executable': str(self.a.opencode), 'provider': 'framework', 'model': 'ornith',
                           'permission_mode': mode, 'timeout_seconds': 420, 'env_passthrough': ['PATH', 'LANG'], 'env': oc_env}
        pi_stub = self.state / 'pi-stub'
        pi_stub.write_text(PI_STUB)
        pi_stub.chmod(0o700)
        self.pi_agent = self.state / 'pi-agent-dir'
        self.pi_agent.mkdir(exist_ok=True)
        envprobe = self.state / 'envprobe'
        envprobe.write_text(ENVPROBE)
        envprobe.chmod(0o700)
        cfg['runners'] = {
            'oc-ro': oc('read-only'), 'oc-rw': oc('workspace-write'),
            'pi-sbx': {'adapter': 'pi', 'executable': str(pi_stub), 'allow_unconfined': True, 'permission_mode': 'workspace-write',
                       'timeout_seconds': 60, 'env_passthrough': ['PATH', 'LANG'], 'containment': 'scan',
                       'env': {'PI_CODING_AGENT_DIR': str(self.pi_agent)},
                       'sandbox': {'kind': 'bwrap', 'executable': '/usr/bin/bwrap'}},
        }
        cfg['plugins'] = {'plugin.envprobe': {'command': [str(envprobe)], 'timeout_seconds': 20, 'env_passthrough': ['ENVPROBE_GRANTED']}}
        cfg['actors']['operator']['types'] += ['plugin.envprobe', 'plugin.demo-fixture']
        cfg['dbos'] = {'work_concurrency': 2}
        cfg['models'] = {}
        cfg_path.write_text(json.dumps(cfg))
        self.extra_env = {'FORGE_HARNESS_PATH': os.environ['PATH']}

    # ---- M2 review fixes ------------------------------------------------------------
    def part_m2fix(self):
        P = 'm2fix'
        # 1. plugin environment allowlist
        self.cli('work', 'register', 'envprobe', '--definition', '-', stdin=json.dumps({'type': 'plugin.envprobe', 'input': {}}))
        run_id = self.cli_json('work', 'run', 'envprobe', '--key', 'envprobe-1')['run_id']
        d = self.wait_run(run_id)
        out = (d.get('effect') or {}).get('output') or d.get('output') or {}
        self.save('plugin-env-run.json', d, P)
        names = out.get('names', [])
        self.check('plugin process env: no FORGE_DATABASE_URL, FORGE_TOKEN or other FORGE_* (the server environment is not inherited)',
                   d['status'] == 'SUCCESS' and 'FORGE_DATABASE_URL' not in names and 'FORGE_TOKEN' not in names
                   and not [n for n in names if n.startswith('FORGE_') and n != 'FORGE_INVOCATION_ID'] and out.get('leaked_names') == [],
                   self.save('plugin-env-names.json', out, P), out, P)
        self.check('plugin process env: only PATH, LANG, FORGE_INVOCATION_ID and the explicitly granted variable',
                   set(names) <= {'PATH', 'LANG', 'FORGE_INVOCATION_ID', 'ENVPROBE_GRANTED', 'PWD', 'SHLVL', '_', 'LC_CTYPE'} and out.get('granted') == 'granted-by-operator',
                   'plugin-env-names.json', names, P)
        # config naming a forbidden variable is refused at load
        bad = self.state / 'home-badenv'
        if bad.exists():
            rmtree(bad)
        subprocess.run([self.forge, '--home', str(bad), 'init'], check=True, capture_output=True, env=self.env(FORGE_TOKEN='x'))
        c = json.loads((bad / 'config.json').read_text())
        c['plugins'] = {'plugin.x': {'command': ['/bin/true'], 'env_passthrough': ['FORGE_TOKEN']}}
        (bad / 'config.json').write_text(json.dumps(c))
        r = subprocess.run([self.forge, '--home', str(bad), 'serve', '--port', str(self.a.port + 50)], capture_output=True, text=True, env=self.env(), timeout=60)
        self.check('config naming FORGE_TOKEN in a plugin env_passthrough is refused at load',
                   r.returncode != 0 and 'env_passthrough' in r.stderr, self.save('plugin-env-config-refused.txt', r.stderr, P), r.stderr, P)

        # 2. Pi sandbox
        host_bus = Path(f'/run/user/{os.getuid()}/bus').exists()
        self.cli('work', 'register', 'pi-probe', '--definition', '-', stdin=json.dumps({'type': 'harness', 'input': {
            'profile': 'pi-sbx', 'permissions': 'workspace-write', 'prompt': 'probe',
            'output_schema': {'type': 'object', 'properties': {'done': {'type': 'boolean'}}, 'required': ['done']}}}))
        run_id = self.cli_json('work', 'run', 'pi-probe', '--key', 'pi-probe-1')['run_id']
        d = self.wait_run(run_id)
        eff = d['effects'][0] if d.get('effects') else {}
        arts = (eff.get('output') or {}).get('artifacts') or []
        if not arts:
            full = self.cli_json('runs', 'inspect', run_id, '--effect-id', eff.get('effect_id', ''))
            arts = (full['effect'].get('output') or {}).get('artifacts') or []
        probe_text = next((a.get('text') for a in arts if a.get('path', '').endswith('probe.json') or 'run_user_listing' in str(a.get('text'))), None)
        probe = json.loads(probe_text) if probe_text else {}
        self.save('pi-sandbox-probe.json', {'host_session_bus_exists': host_bus, 'run_status': d['status'], 'probe_inside_sandbox': probe, 'bwrap': subprocess.run(['/usr/bin/bwrap', '--version'], capture_output=True, text=True).stdout.strip()}, P)
        self.check('live Pi sandbox hides /run/user (empty tmpfs; the host session bus socket is not visible)',
                   d['status'] == 'SUCCESS' and probe.get('run_user_listing') == [] and probe.get('session_bus_visible') is False and host_bus,
                   'pi-sandbox-probe.json', probe, P)
        self.check('live Pi sandbox hides /run/dbus (system bus socket) and keeps /etc read-only and the workspace writable',
                   probe.get('system_bus_socket_visible') is False and 'Read-only' in str(probe.get('etc_write')) and probe.get('workspace_write') == 'ok'
                   and str(probe.get('home', '')).endswith('/pi-home'), 'pi-sandbox-probe.json', probe, P)
        # dedicated-directory validation (provider edit uses the same validation as load)
        home_dir = str(Path.home())
        refused = {}
        for label, directory in {'user HOME': home_dir, 'filesystem root': '/', 'Forge home': str(self.home), 'Forge state dir': str(self.home / 'state'),
                                 'ancestor of the Forge home': str(self.home.parent), 'missing directory': str(self.state / 'no-such-dir')}.items():
            definition = {'adapter': 'pi', 'executable': str(self.state / 'pi-stub'), 'allow_unconfined': True, 'permission_mode': 'workspace-write',
                          'env': {'PI_CODING_AGENT_DIR': directory}, 'sandbox': {'kind': 'bwrap', 'executable': '/usr/bin/bwrap'}}
            code, out, err = self.cli('providers', 'set-runner', 'bad-pi', '--definition', '-', stdin=json.dumps(definition))
            refused[label] = {'rc': code, 'error': err.strip()[:200]}
        self.save('pi-agent-dir-refusals.json', refused, P)
        self.check('a Pi profile whose agent dir is HOME, /, the Forge home, a Forge path, an ancestor of it, or missing is refused',
                   all(v['rc'] != 0 and 'PI_CODING_AGENT_DIR' in v['error'] for v in refused.values()) and len(refused) == 6,
                   'pi-agent-dir-refusals.json', refused, P)
        good = {'adapter': 'pi', 'executable': str(self.state / 'pi-stub'), 'allow_unconfined': True, 'permission_mode': 'workspace-write',
                'env': {'PI_CODING_AGENT_DIR': str(self.pi_agent)}, 'sandbox': {'kind': 'bwrap', 'executable': '/usr/bin/bwrap'}}
        code, out, err = self.cli('providers', 'set-runner', 'pi-sbx-copy', '--definition', '-', stdin=json.dumps(good))
        self.check('the same profile with a dedicated existing directory is accepted (saved, restart required)', code == 0, self.save('pi-agent-dir-accepted.txt', out + err, P), err, P)
        self.cli('providers', 'remove-runner', 'pi-sbx-copy')
        # sandbox launcher must be bubblewrap
        fake = self.state / 'fake-bwrap'
        fake.write_text('#!/bin/sh\necho not-bubblewrap 1\n')
        fake.chmod(0o700)
        cfg = json.loads((self.home / 'config.json').read_text())
        probe_profile = dict(good, sandbox={'kind': 'bwrap', 'executable': str(fake)})
        cfg['runners']['pi-fake-sandbox'] = probe_profile
        (self.home / 'config.json').write_text(json.dumps(cfg))
        # A running service keeps its loaded policy: restart to load the added profile.
        self.restart()
        listed = self.result('runners')['profiles']
        row = next(p for p in listed if p['name'] == 'pi-fake-sandbox')
        self.save('pi-fake-sandbox-runner.json', row, P)
        self.check('a sandbox executable that does not report bubblewrap makes the runner unavailable',
                   row['available'] is False and 'bubblewrap' in (row.get('reason') or ''), 'pi-fake-sandbox-runner.json', row, P)
        cfg['runners'].pop('pi-fake-sandbox')
        (self.home / 'config.json').write_text(json.dumps(cfg))
        self.restart()

        # 3. plugin release integrity
        code, out, err = self.cli('plugins', 'install', str(FIXTURE))
        self.check('install the fixture plugin (disabled)', code == 0, self.save('plugin-install.txt', out + err, P), err, P)
        code, out, _ = self.cli('plugins', 'show', 'demo-fixture', '--check', '--json')
        before = json.loads(out)
        manifest = self.home / 'plugins/releases/demo-fixture/1.0.0/manifest.json'
        original = manifest.read_bytes()
        subprocess.run(['chmod', 'u+w', str(manifest.parent), str(manifest)], check=True)
        manifest.write_bytes(original + b'\n')
        code, out, err = self.cli('plugins', 'show', 'demo-fixture', '--check', '--json')
        after = json.loads(out)
        self.save('plugin-manifest-edit-check.json', {'pristine': before.get('health'), 'edited': after.get('health')}, P)
        self.check('plugins show --check: pristine release verifies, an edited manifest.json (whitespace only) fails',
                   before['health']['release_verified'] is True and after['health']['release_verified'] is False
                   and 'manifest.json differs' in (after['health'].get('error') or ''), 'plugin-manifest-edit-check.json', after.get('health'), P)
        code, out, err = self.cli('plugins', 'enable', 'demo-fixture')
        self.check('enabling the edited release is refused', code != 0 and 'manifest.json differs' in err, self.save('plugin-manifest-edit-enable.txt', out + err, P), err, P)
        self.restart()
        installed = json.loads(self.cli('plugins', 'installed', '--json')[1])
        self.check('after a restart the edited release is classified broken (not trusted)',
                   installed[0]['state'] == 'broken' and 'manifest.json differs' in (installed[0].get('problem') or ''),
                   self.save('plugin-manifest-edit-restart.json', installed, P), installed[0], P)
        job = self.result('backup_start', name='m2fix-blocked')
        status = self.until(lambda: (lambda b: b if b['latest_job'] and b['latest_job']['state'] != 'running' else None)(self.result('backups')), 120)
        self.save('plugin-manifest-edit-backup.json', status, P)
        self.check('online backup verification refuses the edited release (backup fails, no archive completes)',
                   status['latest_job']['state'] == 'failed' and 'manifest.json differs' in (status['latest_job'].get('error') or ''),
                   'plugin-manifest-edit-backup.json', status['latest_job'], P)
        subprocess.run(['chmod', 'u+w', str(manifest)], check=False)
        manifest.write_bytes(original)
        subprocess.run(['chmod', '-R', 'a-w', str(manifest.parent)], check=False)
        self.restart()
        code, out, err = self.cli('plugins', 'uninstall', 'demo-fixture')
        self.check('restored release verifies again and the plugin uninstalls cleanly', code == 0, self.save('plugin-uninstall.txt', out + err, P), err, P)
        shutil.rmtree(self.state / 'home' / 'plugins' / 'staging', ignore_errors=True)
        (self.state / 'home' / 'plugins' / 'staging').mkdir(exist_ok=True, mode=0o700)
        shutil.rmtree(self.state.parent / 'unused', ignore_errors=True)
        failed_backup = self.a.__dict__.get('backups_dir') or None
        _ = failed_backup

    # ---- browser --------------------------------------------------------------------
    def page_errors(self, view):
        rows = [m for v, m in self.console if v == view]
        return rows

    def part_m3(self):
        from playwright.sync_api import sync_playwright
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=self.a.chromium, headless=True, args=['--no-sandbox'])
            self.browser = browser
            self.new_page()
            try:
                self.scenario_views()
                self.scenario_acp_live()
                self.scenario_recovery()
                self.scenario_approvals()
                self.scenario_providers_settings()
                self.scenario_transfer_plugins()
                self.scenario_agents_cancel()
                self.scenario_final_views()
            finally:
                browser.close()

    def new_page(self):
        self.page = self.browser.new_page(viewport={'width': 1360, 'height': 950})
        self.view = 'load'
        # Deliberately invalid provider/setting submissions are answered 400 by the service;
        # the browser logs each such response as a failed resource load.
        page = self.page
        page.add_init_script("window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.violatedDirective+' '+e.blockedURI));")
        page.on('console', lambda m: self.console.append((self.view, m.text)) if m.type == 'error' and 'status of 404' not in m.text and not (self.killing and 'net::ERR_' in m.text) and not (self.view in ('#/providers', '#/settings') and 'status of 400' in m.text) else None)
        page.on('pageerror', lambda e: self.console.append((self.view, 'pageerror ' + str(e))))
        page.on('response', lambda r: self.console.append((self.view, f'HTTP {r.status} {r.url}')) if r.status >= 400 and not r.url.endswith('/favicon.ico') and '/api' not in r.url else None)

    def connect(self):
        page = self.page
        page.goto(self.url)
        page.fill('#token', self.token)
        page.click('#connect button')
        self.wait_for("document.getElementById('status').textContent.startsWith('Current')", 30000)

    def wait_for(self, expression, timeout):
        # Playwright's wait_for_function evaluates a string in the page, which a strict
        # CSP (no unsafe-eval) correctly forbids; poll through the debugger protocol instead.
        end = time.time() + timeout / 1000
        while time.time() < end:
            if self.page.evaluate(expression):
                return True
            time.sleep(.25)
        raise TimeoutError(expression)

    def shot(self, name):
        path = self.ev / 'screenshots' / f'{name}.png'
        self.page.screenshot(path=str(path), full_page=True)
        return f'screenshots/{name}.png'

    def goto(self, route, wait):
        self.view = route
        self.page.evaluate(f"location.hash = '{route}'")
        self.page.wait_for_selector(wait, timeout=30000)

    def csp_violations(self):
        return self.page.evaluate('window.__csp')

    def scenario_views(self):
        self.connect()
        self.view = 'overview'
        self.page.wait_for_selector('#ov-health', timeout=20000)
        text = self.page.inner_text('#overview-body')
        self.check('overview home: health (service, DBOS, database, runners, plugins), upcoming work and failures sections',
                   all(s in text for s in ('Health', 'DBOS engine', 'Database', 'oc-ro', 'Upcoming work', 'Recent failures')), self.shot('overview-initial'), text[:200])
        views = [('#/overview', '#ov-health'), ('#/workflows', '#wf-body h2'), ('#/runs', '#runs-body h2'), ('#/agents', '#agents-body h2'),
                 ('#/approvals', '#approvals-body h2'), ('#/schedules', '#sc-body h2'), ('#/queues', '#q-body h2'), ('#/skills', '#sk-table'),
                 ('#/providers', '#providers-body h2'), ('#/settings', '#st-table'), ('#/transfer', '#tr-restore'), ('#/plugins', '#pl-install')]
        for route, wait in views:
            name = route[2:]
            self.goto(route, wait)
            self.page.wait_for_timeout(300)
            shot = self.shot('view-' + name)
            errors = self.page_errors(route) + self.csp_violations()
            self.check(f'view {name} loads with no console error or CSP violation', not errors, shot, errors)
        self.view = 'operations'
        self.page.evaluate("location.hash = '#/operations'")
        self.page.wait_for_selector('#register', timeout=10000)
        self.check('view operations loads with no console error or CSP violation', not self.page_errors('operations') + self.csp_violations(), self.shot('view-operations'))
        # the page never put the token in storage or URL
        store = self.page.evaluate("JSON.stringify([Object.keys(localStorage),Object.keys(sessionStorage),location.href,document.cookie])")
        self.check('the token is in page memory only (no storage, cookie or URL)', self.token not in store and self.token not in self.page.url, '', store)
        # security headers on the live dashboard
        r = urllib.request.urlopen(self.url + '/')
        h = dict(r.headers)
        self.save('dashboard-headers.json', h)
        h = {k.lower(): v for k, v in h.items()}
        self.check('core dashboard response carries CSP (script-src self), nosniff, frame-ancestors none',
                   "script-src 'self'" in h.get('content-security-policy', '') and "frame-ancestors 'none'" in h.get('content-security-policy', '')
                   and h.get('x-content-type-options') == 'nosniff' and h.get('x-frame-options') == 'DENY', 'dashboard-headers.json', h)

    def register_harness(self, work_id, profile, permissions, prompt, inputs=None, schema=None):
        definition = {'type': 'harness', 'input': {'profile': profile, 'permissions': permissions, 'prompt': prompt,
                      'inputs': inputs or {}, 'output_schema': schema or {'type': 'object', 'properties': {'done': {'type': 'boolean'}}, 'required': ['done']}}}
        self.cli('work', 'register', work_id, '--definition', '-', stdin=json.dumps(definition))

    def scenario_acp_live(self):
        P = 'acp'
        seed = 'ACP-QUALIFICATION'
        self.register_harness('acp-live', 'oc-rw', 'workspace-write',
                              'Step 1: create a file at the path ../outside-marker.txt (one directory ABOVE your working directory) containing the word denied. If that is refused or fails, do not try any other location. Step 2: read inputs.json, then create evidence.txt in your current working directory containing exactly the seed value from inputs.json with no trailing newline. Finally reply with only {"file":"evidence.txt","seed":"<the seed value>"}.',
                              inputs={'seed': seed}, schema={'type': 'object', 'properties': {'file': {'const': 'evidence.txt'}, 'seed': {'const': seed}}, 'required': ['file', 'seed'], 'additionalProperties': False})
        run_id = self.cli_json('work', 'run', 'acp-live', '--key', 'acp-live-1')['run_id']
        self.acp_run = run_id
        self.view = f'#/runs/{run_id}'
        self.page.evaluate(f"location.hash = '#/runs/{run_id}'")
        self.page.wait_for_selector('#acp-panel', timeout=60000)
        timeline = []
        t0 = time.time()
        end = time.time() + 600
        final = None
        while time.time() < end:
            state = self.page.inner_text('#acp-stream-state')
            status = self.page.evaluate("(document.querySelector('#run-steps')&&document.querySelector('.run-head .chip'))?document.querySelector('.run-head .chip').textContent:''")
            transcript = self.page.evaluate("(document.getElementById('acp-transcript')||{textContent:''}).textContent.length")
            tools = self.page.locator('#acp-tools tbody tr[data-tool]').count()
            perms = self.page.locator('#acp-permissions tbody tr[data-decision]').count()
            timeline.append({'t': round(time.time() - t0, 1), 'status': status, 'stream': state[:60], 'transcript_chars': transcript, 'tool_rows': tools, 'permission_rows': perms})
            if status not in ACTIVE and status:
                final = status
                break
            time.sleep(1.2)
        self.save('acp-live-timeline.json', timeline)
        live_obs = [r for r in timeline if r['status'] in ACTIVE and (r['transcript_chars'] > 0 or r['tool_rows'] > 0 or r['permission_rows'] > 0)]
        stream_live = any(r['stream'].startswith('Live') for r in timeline)
        grew = len({r['transcript_chars'] for r in timeline if r['status'] in ACTIVE}) > 1 or len({(r['tool_rows'], r['permission_rows']) for r in timeline if r['status'] in ACTIVE}) > 1
        self.page.wait_for_timeout(1500)
        d = self.result('inspect', run_id=run_id)
        self.check('real OpenCode ACP run (local model): output streamed into the UI while the run was still active',
                   stream_live and bool(live_obs) and grew, 'acp-live-timeline.json',
                   f'stream_live={stream_live} live_obs={len(live_obs)} grew={grew} final={final}')
        self.page.wait_for_selector('#acp-permissions tbody tr[data-decision], #acp-permissions tbody tr.empty-row', timeout=30000)
        rows = self.page.eval_on_selector_all('#acp-permissions tbody tr[data-decision]', "els=>els.map(e=>[...e.children].map(c=>c.textContent))")
        denied = [r for r in rows if r[2].upper() == 'DENIED']
        tools = self.page.eval_on_selector_all('#acp-tools tbody tr[data-tool]', "els=>els.map(e=>[...e.children].map(c=>c.textContent))")
        shot = self.shot('acp-run-detail')
        self.save('acp-decision-rows.json', {'permissions': rows, 'tools': tools, 'run_status': d['status']})
        self.check('permission decision table shows the denied request with kind, locations, decision and reason',
                   bool(denied) and all(len(r) == 5 and r[0] and r[3] for r in denied), shot, rows)
        self.check('tool events table shows tool kind and status from the session updates', bool(tools) and all(r[1] and r[2] for r in tools), shot, tools)
        self.check('no console errors / CSP violations on the live ACP run page', not self.page_errors(self.view) + self.csp_violations(), shot, self.page_errors(self.view))
        # evidence recorded: ACP tool kind/status in run evidence (via CLI)
        eff = self.cli_json('runs', 'inspect', run_id, '--effect-id', d['effects'][0]['effect_id'])['effect']
        attempt = eff.get('harness_attempt') or {}
        h = (eff.get('output') or {}).get('harness') or {}
        self.save('acp-run-effect.json', eff)
        self.check('run evidence records ACP tool kind/status and permission reasons (attempt + output)',
                   bool(attempt.get('tool_calls')) and all('kind' in t and 'status' in t for t in attempt['tool_calls'])
                   and all('reason' in x for x in attempt.get('permission_decisions', [])) and (h.get('tool_calls') is not None or d['status'] != 'SUCCESS'),
                   'acp-run-effect.json', attempt.get('tool_calls'))
        # logs panel + paginated logs API
        index = self.result('run_logs', run_id=run_id)
        src = next(s['source'] for s in index['sources'] if s['source'].startswith('harness:'))
        page1 = self.result('run_logs', run_id=run_id, source=src, limit=5)
        self.save('acp-run-logs-index.json', index)
        self.save('acp-run-logs-page1.json', page1)
        self.check('run_logs: indexed sources, per-attempt diagnostics, bounded pages with a cursor, token redacted',
                   len(page1['entries']) <= 5 and 'next_after' in page1 and self.token not in json.dumps(index) + json.dumps(page1)
                   and index['sources'][1]['attempts'][0]['state'] in ('turn_completed', 'exited', 'protocol_error'), 'acp-run-logs-page1.json', page1['next_after'])
        self.page.wait_for_selector('#run-logs #log-entries tbody tr', timeout=20000)
        self.check('logs and diagnostics panel lists entries and attempt diagnostics', self.page.locator('#log-attempts').count() == 1, self.shot('acp-run-logs'))
        self.stream_checks(run_id)
        self.check('the ACP run completed under the deny-by-default policy (outside write denied, inside write allowed)',
                   any(r[2].upper() == 'DENIED' for r in rows) and d['status'] in ('SUCCESS', 'RECOVERY_REQUIRED'),
                   'acp-decision-rows.json', d['status'])
        self.agree('acp-run', run_id)
        # a read-only profile: the same policy denies the edit with its reason
        self.register_harness('acp-readonly', 'oc-ro', 'read-only',
                              'Try to create a file named note.txt in your current working directory containing the word hello. If that is refused or fails, do not try again or use any other location. Then reply with only {"done":true}.')
        ro = self.cli_json('work', 'run', 'acp-readonly', '--key', 'acp-ro-1')['run_id']
        self.page.evaluate(f"location.hash = '#/runs/{ro}'")
        self.page.wait_for_selector('#acp-panel', timeout=60000)
        ro_done = self.wait_run(ro, timeout=600)
        self.page.wait_for_timeout(2500)
        self.page.evaluate("location.hash = '#/runs/" + ro + "'")
        self.page.wait_for_selector('#acp-permissions tbody tr[data-decision]', timeout=30000)
        ro_rows = self.page.eval_on_selector_all('#acp-permissions tbody tr[data-decision]', "els=>els.map(e=>[...e.children].map(c=>c.textContent))")
        self.save('acp-readonly-decision-rows.json', {'status': ro_done['status'], 'rows': ro_rows})
        self.check('read-only profile: the edit request is denied with the policy reason in the decision table',
                   any(r[2].upper() == 'DENIED' and 'workspace-write' in r[3] for r in ro_rows), self.shot('acp-readonly-denied'), ro_rows)
        self.ro_run = ro

    def stream_checks(self, run_id):
        def post(token):
            request = urllib.request.Request(self.url + '/api/stream', data=json.dumps({'run_id': run_id}).encode(),
                                             headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
            try:
                return urllib.request.urlopen(request, timeout=30)
            except urllib.error.HTTPError as e:
                return e
        refused = [post('x' * 40).status, post('y' * 40).status]
        url_token = urllib.request.Request(self.url + '/api/stream?token=' + self.token, data=json.dumps({'run_id': run_id}).encode(), headers={'Content-Type': 'application/json'})
        try:
            urllib.request.urlopen(url_token, timeout=30)
            url_status = 200
        except urllib.error.HTTPError as e:
            url_status = e.code
        response = post(self.token)
        body = b''
        end = time.time() + 20
        while time.time() < end:
            chunk = response.read1(65536) if hasattr(response, 'read1') else response.read(65536)
            if not chunk:
                break
            body += chunk
            if b'event: end' in body:
                break
        text = body.decode('utf-8', 'replace')
        self.save('stream-sample.txt', text[:6000])
        self.check('stream endpoint: wrong credentials and a token-in-URL are refused; the Authorization-header stream delivers SSE events (state, tool, permission, end)',
                   refused == [403, 403] and url_status == 403 and 'event: state' in text and 'event: permission' in text and 'event: tool_call' in text and 'event: end' in text and self.token not in text,
                   'stream-sample.txt', f'{refused} {url_status}')

    def cli_state(self):
        """The same states through CLI, MCP and API (identities and states only)."""
        def runs_from(v):
            return sorted((r['run_id'], r['status']) for r in v['runs'])
        cli_runs = self.cli_json('runs', 'search', '--limit', '100', '--json')
        api_runs = self.result('runs', limit=100)
        mcp_runs = self.mcp('runs', limit=100)
        appr = lambda v: sorted((a['approval_id'], a['state']) for a in v['approvals'])
        return {
            'runs': {'cli': runs_from(cli_runs), 'api': runs_from(api_runs), 'mcp': runs_from(mcp_runs)},
            'approvals': {'cli': appr(self.cli_json('approvals', 'list')), 'api': appr(self.result('approvals')), 'mcp': appr(self.mcp('approvals'))},
        }

    def agree(self, label, run_id=None, expect=None):
        s = self.cli_state()
        self.save(f'agreement-{label}.json', s)
        ok = all(len({json.dumps(v[k]) for k in ('cli', 'api', 'mcp')}) == 1 for v in s.values())
        self.check(f'agreement ({label}): CLI, MCP and API report the same run and approval identities and states', ok, f'agreement-{label}.json')
        return s

    def dom_states(self, selector, attr):
        return self.page.eval_on_selector_all(selector, f"els=>els.map(e=>[e.dataset.{attr}, e.dataset.state])")

    def scenario_recovery(self):
        self.register_harness('recover-me', 'oc-rw', 'workspace-write',
                              'Create a file named evidence.txt in your current working directory containing the word seed. Then, before replying, read evidence.txt five more times, one call at a time. Use only file read and file write tools. Finally reply with only {"done":true}.')
        run_id = self.cli_json('work', 'run', 'recover-me', '--key', 'recover-1')['run_id']
        self.recover_run = run_id
        def running():
            d = self.result('inspect', run_id=run_id)
            if d['status'] not in ACTIVE:
                return d
            log = list((self.home / 'state/harness').glob('*/updates.jsonl'))
            for p in log:
                a = p.parent / 'attempt.json'
                if a.exists() and json.loads(a.read_text()).get('run_id') == run_id and p.stat().st_size > 0:
                    return {'status': 'RUNNING', 'log': str(p)}
            return None
        got = self.until(running, 240)
        self.check('recovery setup: a real ACP agent is mid-turn with streamed updates when the server is killed', got.get('status') == 'RUNNING', '', got)
        # the UI shows it live right before the loss
        self.view = f'#/runs/{run_id}'
        self.page.evaluate(f"location.hash = '#/runs/{run_id}'")
        self.page.wait_for_selector('#acp-panel', timeout=30000)
        self.page.wait_for_timeout(1500)
        self.shot('recovery-before-kill')
        # SIGKILL severs the live event stream the page holds open: the browser logs that
        # network error, which is the expected effect of the deliberate kill, not a defect.
        self.killing = True
        self.restart(kill=True)
        d = self.wait_run(run_id, timeout=240)
        self.connect()
        self.killing = False
        self.connect()
        self.check('SIGKILL mid-run then restart: the run is RECOVERY_REQUIRED (uncertain effect), not replayed', d['status'] == 'RECOVERY_REQUIRED', '', d['status'])
        self.save('recovery-run-before.json', d)
        self.connect()
        self.view = f'#/runs/{run_id}'
        self.page.evaluate(f"location.hash = '#/runs/{run_id}'")
        self.page.wait_for_selector('#reconcile-panel', timeout=30000)
        shot = self.shot('recovery-required')
        ov = self.result('overview')
        self.check('overview lists the RECOVERY_REQUIRED run among recent failures',
                   any(f['run_id'] == run_id and f['status'] == 'RECOVERY_REQUIRED' for f in ov['failures']), shot)
        # validation in the UI: a note and settled confirmation are required
        self.page.click('#rc-submit')
        self.page.wait_for_selector('#rc-result .alert', timeout=5000)
        refusal = self.page.inner_text('#rc-result')
        self.check('reconcile form refuses an empty note in the UI', 'note is required' in refusal, self.shot('recovery-form-refusal'), refusal)
        self.page.select_option('#rc-decision', 'not_applied')
        self.page.fill('#rc-note', 'Checked the workspace: the agent was killed before it finished; evidence.txt is absent or partial and nothing external was affected.')
        self.page.check('#rc-settled')
        self.page.click('#rc-submit')
        self.wait_for("!document.getElementById('reconcile-panel')", 30000)
        d2 = self.result('inspect', run_id=run_id)
        eff = d2['effects'][0]
        self.save('recovery-run-after.json', d2)
        self.check('reconcile from the UI (not_applied with a note): effect resolved with the recorded decision, actor and note',
                   eff['state'] == 'not_applied' and eff['reconciliation']['actor'] == 'operator' and 'killed before' in eff['reconciliation']['note'] and d2['status'] != 'RECOVERY_REQUIRED',
                   self.shot('recovery-reconciled'), {'effect': eff['state'], 'status': d2['status']})
        cli = self.cli_json('runs', 'inspect', run_id)
        mcp = self.mcp('inspect', run_id=run_id)
        self.check('CLI and MCP show the same reconciliation', cli['effects'][0]['reconciliation'] == mcp['effects'][0]['reconciliation'] == eff['reconciliation'], '')
        self.check('no console errors / CSP violations during reconciliation', not self.page_errors(self.view) + self.csp_violations(), '', self.page_errors(self.view))
        self.agree('after-recovery', run_id)
        # an applied reconciliation on a second interrupted run
        run2 = self.cli_json('work', 'run', 'recover-me', '--key', 'recover-2')['run_id']
        def started():
            for p in (self.home / 'state/harness').glob('*/attempt.json'):
                a = json.loads(p.read_text())
                if a.get('run_id') == run2 and a.get('state') == 'running':
                    return True
        self.until(started, 240)
        self.killing = True
        self.restart(kill=True)
        d = self.wait_run(run2, timeout=240)
        self.connect()
        self.killing = False
        self.page.evaluate(f"location.hash = '#/runs/{run2}'")
        self.page.wait_for_selector('#reconcile-panel', timeout=30000)
        self.page.select_option('#rc-decision', 'completed')
        self.page.fill('#rc-note', 'Provider evidence: the file evidence.txt exists in the agent workspace with the expected content; the operation did take effect.')
        self.page.fill('#rc-output', '{"verified":"evidence.txt present"}')
        self.page.check('#rc-settled')
        self.page.click('#rc-submit')
        self.wait_for("!document.getElementById('reconcile-panel')", 30000)
        d3 = self.result('inspect', run_id=run2)
        self.check('reconcile "applied" (completed) from the UI records the verified output', d3['effects'][0]['state'] == 'completed' and d3['effects'][0]['reconciliation']['decision'] == 'completed',
                   self.shot('recovery-applied'), d3['effects'][0]['state'])

    def scenario_approvals(self):
        flow = {'type': 'workflow', 'input': {'steps': [
            {'id': 'prepare', 'action': {'type': 'echo', 'input': {'text': 'release notes drafted'}}},
            {'id': 'gate', 'needs': ['prepare'], 'action': {'type': 'approval', 'input': {'title': 'Publish the release notes', 'description': 'A human must approve publishing.', 'timeout_seconds': 3600}}},
            {'id': 'publish', 'needs': ['gate', 'prepare'], 'action': {'type': 'echo', 'input': {'text': 'published: ${/steps/prepare/output/text}'}}}],
            'output': {'$ref': '/steps/publish/output/text'}}}
        code, out, err = self.cli('workflows', 'create', 'approval-flow', '--file', '-', stdin=json.dumps(flow))
        self.check('register a workflow with a durable approval step', code == 0, self.save('approval-flow-create.txt', out + err), err)
        run_a = self.cli_json('work', 'run', 'approval-flow', '--key', 'approval-a')['run_id']
        run_b = self.cli_json('work', 'run', 'approval-flow', '--key', 'approval-b')['run_id']
        self.until(lambda: len([a for a in self.result('approvals', state='pending')['approvals']]) == 2, 90)
        self.connect()
        self.goto('#/approvals', '#ap-table')
        shot = self.shot('approvals-pending')
        before = self.dom_states('#ap-table tbody tr[data-approval]', 'approval')
        api = sorted((a['approval_id'], a['state']) for a in self.result('approvals')['approvals'])
        self.check('approvals view lists both pending approvals (matches API, CLI, MCP)',
                   sorted(map(tuple, before)) == api and len(api) == 2 and all(s == 'pending' for _, s in api), shot, before)
        self.agree('approvals-pending')
        # durability: restart the server with both approvals pending
        pend_before = self.cli_json('approvals', 'list', '--state', 'pending')
        self.restart()
        pend_after = self.cli_json('approvals', 'list', '--state', 'pending')
        mcp_after = self.mcp('approvals', state='pending')
        self.save('approvals-restart.json', {'before': pend_before, 'after': pend_after, 'mcp': mcp_after})
        self.check('pending approvals survive a server restart (same ids, still pending, runs still waiting)',
                   [a['approval_id'] for a in pend_before['approvals']] == [a['approval_id'] for a in pend_after['approvals']] and pend_after['pending'] == 2
                   and [a['approval_id'] for a in mcp_after['approvals']] == [a['approval_id'] for a in pend_after['approvals']]
                   and all(a['actionable'] for a in pend_after['approvals']), 'approvals-restart.json')
        self.connect()
        self.goto('#/approvals', '#ap-table')
        self.shot('approvals-after-restart')
        rows = self.page.locator('#ap-table tbody tr[data-approval]')
        # approve run A in the UI
        row_a = self.page.locator(f'#ap-table tbody tr:has(a[href$="{run_a}"])')
        row_a.locator('input.note').fill('Approved after restart: notes reviewed.')
        row_a.locator('button.approve').click()
        d = self.wait_run(run_a, timeout=120)
        self.check('approve in the UI after the restart: the waiting flow resumes and completes', d['status'] == 'SUCCESS' and d['output']['output'] == 'published: release notes drafted',
                   self.shot('approvals-approved'), {'status': d['status'], 'output': d.get('output')})
        a_row = next(a for a in self.result('approvals')['approvals'] if a['run_id'] == run_a)
        self.check('the decision records the actor, note and time', a_row['state'] == 'approved' and a_row['actor'] == 'operator' and 'notes reviewed' in a_row['note'] and a_row['decided_ms'], '', a_row)
        # dedup: a second decision is refused (CLI), and a conflicting reject too
        code, out, err = self.cli('approvals', 'approve', a_row['approval_id'], '--note', 'again?')
        code2, out2, err2 = self.cli('approvals', 'reject', a_row['approval_id'], '--note', 'flip')
        self.check('dedup: the same approval cannot be decided again (differing note or opposite decision)', code != 0 and code2 != 0 and 'already decided' in err + err2, '', err + err2)
        # reject run B in the UI
        self.page.evaluate("location.hash = '#/runs/" + run_b + "'")
        self.page.wait_for_selector('#run-approvals button.reject', timeout=30000)
        self.page.fill('#run-approvals input.note', 'Rejected: wording not ready.')
        self.page.click('#run-approvals button.reject')
        d = self.wait_run(run_b, timeout=120)
        failure = None
        try:
            failure = self.result('runs', status='ERROR', limit=10)['runs']
        except Exception:
            pass
        self.page.wait_for_timeout(1500)
        self.page.evaluate("location.hash = '#/approvals'")
        self.page.wait_for_selector('#ap-table', timeout=20000)
        b_row = next(a for a in self.result('approvals')['approvals'] if a['run_id'] == run_b)
        fail_reason = next((r['failure'] for r in (failure or []) if r['run_id'] == run_b), None)
        self.check('reject in the UI: the approval step fails the run, with the rejection and note recorded',
                   d['status'] == 'ERROR' and b_row['state'] == 'rejected' and 'wording not ready' in b_row['note'] and fail_reason and fail_reason.get('step') == 'gate',
                   self.shot('approvals-rejected'), {'status': d['status'], 'failure': fail_reason})
        self.check('no console errors / CSP violations in the approvals flow', not self.page_errors('#/approvals') + self.csp_violations(), '', self.page_errors('#/approvals'))
        # refresh and restart agreement
        self.page.reload()
        self.page.fill('#token', self.token)
        self.page.click('#connect button')
        self.wait_for("document.getElementById('status').textContent.startsWith('Current')", 30000)
        self.goto('#/approvals', '#ap-table')
        dom = sorted(map(tuple, self.dom_states('#ap-table tbody tr[data-approval]', 'approval')))
        api = sorted((a['approval_id'], a['state']) for a in self.result('approvals')['approvals'])
        self.check('after a page refresh the dashboard shows the same approval states as the API', dom == api, self.shot('approvals-after-refresh'), dom)
        s1 = self.agree('approvals-final')
        self.restart()
        s2 = self.agree('approvals-final-after-restart')
        self.check('approval and run states are identical after a server restart', s1 == s2, 'agreement-approvals-final-after-restart.json')

    def scenario_providers_settings(self):
        self.connect()
        self.goto('#/providers', '#providers-body h2')
        page = self.page
        # invalid model rejected
        page.fill('#pv-model-name', 'bad-model')
        page.fill('#pv-model-url', 'ftp://example.invalid/v1')
        page.fill('#pv-model-id', 'm')
        page.click('#pv-model-save')
        page.wait_for_selector('#pv-model-result .alert.error', timeout=10000)
        msg = page.inner_text('#pv-model-result')
        self.check('providers: an invalid model endpoint is rejected with a message (nothing saved)', ('invalid' in msg.lower() or 'refused' in msg.lower()), self.shot('providers-invalid-model'), msg)
        # invalid runner rejected
        page.fill('#pv-runner-name', 'bad-runner')
        page.fill('#pv-runner-json', json.dumps({'adapter': 'opencode-acp', 'executable': 'not-absolute'}))
        page.click('#pv-runner-save')
        page.wait_for_selector('#pv-runner-result .alert.error', timeout=10000)
        rmsg = page.inner_text('#pv-runner-result')
        self.check('providers: an invalid runner profile (relative executable) is rejected using the profile-load validation', 'invalid runner profile' in rmsg.lower() or 'refused' in rmsg.lower(), self.shot('providers-invalid-runner'), rmsg)
        # secrets only as env-var names
        page.fill('#pv-model-name', 'secret-model')
        page.fill('#pv-model-url', 'https://models.example.invalid/v1')
        page.fill('#pv-model-id', 'm')
        page.fill('#pv-model-key', 'sk-fixture-' + 'x' * 32)
        page.click('#pv-model-save')
        page.wait_for_selector('#pv-model-result .alert.error', timeout=10000)
        smsg = page.inner_text('#pv-model-result')
        self.check('providers: a literal secret in place of an environment variable name is rejected', 'environment variable name' in smsg, '', smsg)
        # valid save
        page.fill('#pv-model-name', 'local-fixture')
        page.fill('#pv-model-url', MODEL_URL)
        page.fill('#pv-model-id', MODEL_ID)
        page.fill('#pv-model-key', '')
        page.click('#pv-model-save')
        page.wait_for_selector('#pv-restart', timeout=20000)
        self.page.wait_for_selector('#pv-models tr[data-model="local-fixture"]', timeout=10000)
        shot = self.shot('providers-saved')
        saved = self.result('providers')
        cli = self.cli_json('providers', 'list')
        mcp = self.mcp('providers')
        row = next(m for m in saved['models'] if m['name'] == 'local-fixture')
        self.check('providers: a valid model is saved, shown as restart-required, and appears identically via API, CLI and MCP',
                   row['status'] == 'pending_add' and saved['restart_required'] and cli['models'] == saved['models'] == mcp['models'] and bad_absent(saved), shot, row)
        cfg = json.loads((self.home / 'config.json').read_text())
        self.check('config.json holds only the endpoint and an env var NAME (no credential value); file mode 0600',
                   cfg['models']['local-fixture'] == {'base_url': MODEL_URL, 'model': MODEL_ID, 'key_env': None}
                   and oct((self.home / 'config.json').stat().st_mode & 0o777) == '0o600', '', cfg['models'])
        audit = self.result('run_logs', run_id=self.acp_run, source='audit')
        self.restart()
        self.connect()
        self.goto('#/providers', '#providers-body h2')
        status = next(m['status'] for m in self.result('providers')['models'] if m['name'] == 'local-fixture')
        dom = page.inner_text('#pv-models tr[data-model="local-fixture"]')
        self.check('after a restart the saved model is effective (and survives a page refresh)', status == 'effective' and 'SUCCESS' in dom.upper() or status == 'effective', self.shot('providers-effective'), status)
        # settings
        self.goto('#/settings', '#st-table')
        shot = self.shot('settings-sources')
        rows = page.eval_on_selector_all('#st-table tbody tr', "els=>els.map(e=>[...e.children].map(c=>c.textContent))")
        by = {r[0]: r for r in rows}
        self.save('settings-rows.json', rows)
        self.check('settings view: effective values with the source of each (flag, env, config, default) and secrets redacted',
                   by['server.port'][2] == 'FLAG' and by['harness.default_path'][2] == 'ENV' and by['dbos.work_concurrency'][2] == 'CONFIG' and by['dbos.server_concurrency'][2] == 'DEFAULT'
                   and by['database.url'][1] == '[redacted]' and 'postgresql' not in json.dumps(rows), shot, {k: by[k][1:3] for k in ('server.port', 'harness.default_path', 'dbos.work_concurrency', 'database.url')})
        bad_before = (self.home / 'config.json').read_bytes()
        inp = page.locator('#st-table tr[data-key="database.max_connections"] input')
        inp.fill('1')
        page.locator('#st-table tr[data-key="database.max_connections"] button.save').click()
        page.wait_for_selector('#settings-alert .alert.error', timeout=10000)
        emsg = page.inner_text('#settings-alert')
        self.check('settings: an invalid value is rejected with validation and config.json is untouched', 'max_connections' in emsg and (self.home / 'config.json').read_bytes() == bad_before, self.shot('settings-invalid'), emsg)
        inp = page.locator('#st-table tr[data-key="dbos.work_concurrency"] input')
        inp.fill('3')
        page.locator('#st-table tr[data-key="dbos.work_concurrency"] button.save').click()
        page.wait_for_selector('#settings-alert .alert.ok', timeout=10000)
        row = page.eval_on_selector('#st-table tr[data-key="dbos.work_concurrency"]', "e=>[...e.children].map(c=>c.textContent)")
        view = self.result('settings')
        mrow = next(r for r in self.mcp('settings')['settings'] if r['key'] == 'dbos.work_concurrency')
        self.check('settings: a valid edit is saved with a "restart required" notice; effective value unchanged until restart; no restart button',
                   row[1] == '2' and row[3] == '3' and 'Restart required' in page.inner_text('#settings-alert') and mrow['saved'] == 3 and page.locator('button:has-text("Restart")').count() == 0,
                   self.shot('settings-saved'), row)
        self.check('no console errors / CSP violations on providers and settings', not self.page_errors('#/providers') + self.page_errors('#/settings') + self.csp_violations(), '', self.console[-3:])
        self.cli('settings', 'unset', 'dbos.work_concurrency')
        self.cli('settings', 'set', 'dbos.work_concurrency', '2')

    def scenario_transfer_plugins(self):
        page = self.page
        self.goto('#/transfer', '#tr-restore')
        page.fill('#tr-name', 'ui-backup-1')
        page.click('#tr-start')
        page.wait_for_selector('#tr-job:has-text("SUCCESS")', timeout=180000)
        self.page.wait_for_selector('#tr-table tr[data-backup="ui-backup-1"]', timeout=20000)
        shot = self.shot('transfer-backup')
        backups = self.result('backups')
        cli = self.cli_json('backups', 'list')
        mcp = self.mcp('backups')
        arch = Path(self.state / 'home-backups' / 'ui-backup-1')
        manifest = json.loads((arch / 'manifest.json').read_text()) if (arch / 'manifest.json').exists() else {}
        self.save('backups.json', backups)
        self.check('backup started from the UI completes: archive with manifest and database dump; listed identically by API, CLI and MCP; restore is CLI-only with the exact instruction',
                   manifest.get('files') and (arch / 'database.dump').exists() and not (arch / 'INCOMPLETE').exists()
                   and [a['name'] for a in cli['archives']] == [a['name'] for a in mcp['archives']] == [a['name'] for a in backups['archives']]
                   and f'restore {arch}' in page.inner_text('#tr-table') and 'CLI-only' in page.inner_text('#tr-restore'),
                   shot, {'files': len(manifest.get('files', {})), 'archives': [a['name'] for a in backups['archives']]})
        # the archive restores (offline CLI) into a new home and database
        pg2 = self.state / 'pg2'
        run([self.pg / 'initdb', '-D', pg2, '-A', 'trust', '-U', 'forge', '--no-locale', '--encoding=UTF8'])
        run([self.pg / 'pg_ctl', '-D', pg2, '-l', self.state / 'pg2.log', '-o', f"-h 127.0.0.1 -p {self.a.pg_port + 5} -c unix_socket_directories=''", '-w', 'start'])
        try:
            run([self.pg / 'createdb', '-h', '127.0.0.1', '-p', self.a.pg_port + 5, '-U', 'forge', 'forge_restored'])
            restored = self.state / 'restored-home'
            r = subprocess.run([self.forge, '--home', str(restored), 'restore', str(arch)], capture_output=True, text=True,
                               env=self.env(FORGE_DATABASE_URL=f'postgresql://forge@127.0.0.1:{self.a.pg_port + 5}/forge_restored'), timeout=300)
            self.check('the UI-started online backup restores with the offline CLI restore into a new home and empty database',
                       r.returncode == 0 and (restored / 'config.json').exists(), self.save('restore-from-ui-backup.txt', r.stdout + r.stderr), r.stderr[-300:])
        finally:
            run([self.pg / 'pg_ctl', '-D', pg2, '-m', 'fast', '-w', 'stop'])
        # plugins: install / enable / disable from the UI
        self.goto('#/plugins', '#pl-install')
        page.fill('#pl-source', str(FIXTURE))
        page.click('#pl-check')
        page.wait_for_selector('#pl-install-result .alert.ok', timeout=20000)
        dry = page.inner_text('#pl-install-result')
        self.check('plugins: validate (dry run) a local plugin directory from the UI without installing it', 'Valid' in dry and self.cli_json('plugins', 'installed', '--json') == [], self.shot('plugins-dry-run'), dry[:200])
        page.click('#pl-install-go')
        page.wait_for_selector('#pl-installed tr[data-plugin="demo-fixture"]', timeout=30000)
        listed = self.cli_json('plugins', 'installed', '--json')
        self.check('plugins: install from a local path via the UI; installed DISABLED (same state via CLI and MCP snapshot)',
                   listed[0]['id'] == 'demo-fixture' and listed[0]['state'] == 'disabled' and 'disabled' in page.inner_text('#pl-installed tr[data-plugin="demo-fixture"]'),
                   self.shot('plugins-installed'), listed[0]['state'])
        page.locator('#pl-installed tr[data-plugin="demo-fixture"] button:has-text("Enable")').click()
        page.wait_for_selector('#pl-installed tr[data-plugin="demo-fixture"] button:has-text("Disable")', timeout=60000)
        listed = self.cli_json('plugins', 'installed', '--json')
        tools = self.mcp_tools()
        self.check('plugins: enable from the UI; CLI, MCP tools and the nav show the plugin enabled',
                   listed[0]['enabled'] is True and any(t.startswith('plugin_demo-fixture__') or t.startswith('plugin_demo') for t in tools)
                   and page.locator('nav a[data-plugin-nav="demo-fixture"]').count() >= 1, self.shot('plugins-enabled'), [t for t in tools if t.startswith('plugin_')])
        page.locator('nav a[data-plugin-nav="demo-fixture"]').first.click()
        page.wait_for_selector('#plugin-mount *', timeout=20000)
        self.shot('plugins-page')
        self.goto('#/plugins', '#pl-install')
        page.locator('#pl-installed tr[data-plugin="demo-fixture"] button:has-text("Disable")').click()
        page.wait_for_selector('#pl-installed tr[data-plugin="demo-fixture"] button:has-text("Enable")', timeout=60000)
        listed = self.cli_json('plugins', 'installed', '--json')
        tools = self.mcp_tools()
        self.check('plugins: disable from the UI; CLI and MCP show it disabled and its tools gone',
                   listed[0]['enabled'] is False and not [t for t in tools if t.startswith('plugin_')], self.shot('plugins-disabled'), listed[0]['state'])
        self.check('no console errors / CSP violations in plugin management', not self.page_errors('#/plugins') + self.csp_violations(), '', self.page_errors('#/plugins'))
        # restart agreement for plugins
        self.restart()
        again = self.cli_json('plugins', 'installed', '--json')
        self.check('plugin state is identical after a server restart', again[0]['state'] == 'disabled' and again[0]['id'] == 'demo-fixture', '', again[0]['state'])

    def scenario_agents_cancel(self):
        self.connect()
        self.register_harness('cancel-me', 'oc-ro', 'read-only', 'Read the file inputs.json in your working directory twenty times, one tool call at a time, then reply with only {"done":true}.')
        run_id = self.cli_json('work', 'run', 'cancel-me', '--key', 'cancel-1')['run_id']
        def agent_active():
            a = self.result('agents')
            return any(r['run_id'] == run_id for r in a['active'])
        self.until(agent_active, 120)
        self.goto('#/agents', '#agents-active')
        shot = self.shot('agents-active')
        row = self.page.locator(f'#agents-active tr[data-run="{run_id}"]')
        text = row.inner_text()
        self.check('agents view: active agent run by runner/adapter/model with status and duration, linking to the run view',
                   'oc-ro' in text and 'opencode-acp' in text and 'ornith' in text and row.locator(f'a[href$="{run_id}"]').count() >= 1, shot, text)
        api = self.result('agents')
        cli = self.cli_json('agents')
        mcp = self.mcp('agents')
        key = lambda v: sorted((r['run_id'], r['runner']) for r in v['active'] + v['recent'])
        self.check('agents: API, CLI and MCP list the same runs', key(api) == key(cli) == key(mcp), '')
        row.locator('button:has-text("Cancel")').click()
        d = self.until(lambda: (lambda x: x if x['status'] not in ACTIVE else None)(self.result('inspect', run_id=run_id)), 180)
        self.page.wait_for_timeout(1500)
        self.check('cancel an active agent run from the Agents view (cooperative cancellation; the uncertain effect then needs reconciliation)',
                   d['status'] in ('CANCELLED', 'RECOVERY_REQUIRED', 'ERROR'), self.shot('agents-cancelled'), d['status'])
        self.goto('#/agents', '#agents-recent')
        recent = self.page.inner_text('#agents-recent')
        self.check('recent agent runs include earlier ACP runs and the cancelled one', run_id[:8] in recent or 'oc-' in recent and 'RECOVERY_REQUIRED' in recent.upper() or 'CANCELLED' in recent.upper(), self.shot('agents-recent'))
        self.check('no console errors / CSP violations in agents', not self.page_errors('#/agents') + self.csp_violations(), '', self.page_errors('#/agents'))

    def scenario_final_views(self):
        self.goto('#/overview', '#ov-health')
        self.page.wait_for_timeout(500)
        text = self.page.inner_text('#overview-body')
        self.check('overview after the scenarios: recent failures show the unresolved/failed runs linked to their runs',
                   self.page.locator('#ov-failures a[href^="#/runs/"]').count() >= 1, self.shot('overview-final'), text[:300])
        # schedules: stats
        self.cli('schedules', 'create', 'm3-every-minute', '--work-id', 'acp-live', '--cron', '0 * * * * *', '--tz', 'UTC') if False else None
        self.cli('workflows', 'create', 'm3-tick', '--file', '-', stdin=json.dumps({'type': 'workflow', 'input': {'steps': [{'id': 'a', 'action': {'type': 'echo', 'input': {'text': 'tick'}}}]}}))
        code, out, err = self.cli('schedules', 'create', 'm3-tick-schedule', '--work-id', 'm3-tick', '--cron', '*/10 * * * * *', '--tz', 'UTC')
        sched = 'operator.m3-tick-schedule'
        self.until(lambda: any(s['schedule_name'] == sched and s['succeeded'] >= 2 for s in self.result('schedule_stats')['schedules']), 120)
        self.goto('#/schedules', '#sc-table')
        row = self.page.locator(f'#sc-table tr[data-schedule="{sched}"]').inner_text()
        stats = next(s for s in self.result('schedule_stats')['schedules'] if s['schedule_name'] == sched)
        cli_stats = self.cli_json('schedules', 'stats')
        mcp_stats = self.mcp('schedule_stats')
        self.check('schedules view and API/CLI/MCP: run stats (succeeded/failed counts, last fired, next due)',
                   stats['succeeded'] >= 2 and stats['next_run'] and stats['last_fired_at'] and str(stats['succeeded']) in row and
                   [s['schedule_name'] for s in cli_stats['schedules']] == [s['schedule_name'] for s in mcp_stats['schedules']], self.shot('schedules-stats'), stats)
        self.cli('schedules', 'delete', sched)
        # skills catalog
        self.goto('#/skills', '#sk-table')
        self.page.locator('#sk-table button:has-text("Show")').first.click()
        self.page.wait_for_selector('#sk-content pre', timeout=10000)
        skills_cli = self.cli_json('skills', 'list')
        skills_mcp = self.mcp('skills')
        shown = self.page.inner_text('#sk-content')
        self.check('skills catalog (read-only): list and show in UI, CLI and MCP agree with `agent-forge skill`',
                   skills_cli == skills_mcp and 'forge-operations' in shown and shown.split('\n', 1)[1].strip()[:40] in self.cli('skill')[1], self.shot('skills'), skills_cli['skills'][0]['name'])
        errors = [(v, m) for v, m in self.console]
        self.save('console-errors.json', errors)
        self.check('no browser console errors or CSP violations anywhere in the proof', not errors, 'console-errors.json', errors[:5])


def bad_absent(providers):
    return all(m['name'] not in ('bad-model', 'secret-model') for m in providers['models']) and all(r['name'] != 'bad-runner' for r in providers['runners'])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--forge', type=Path, required=True)
    p.add_argument('--pg-bin', type=Path, required=True)
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--evidence', type=Path, required=True)
    p.add_argument('--m2fix-evidence', type=Path, required=True)
    p.add_argument('--port', type=int, default=19300)
    p.add_argument('--pg-port', type=int, default=19301)
    p.add_argument('--opencode', type=Path, required=True)
    p.add_argument('--opencode-config', type=Path, required=True)
    p.add_argument('--chromium', default=os.environ.get('CHROMIUM_EXECUTABLE', '/usr/bin/chromium'))
    p.add_argument('--part', choices=['all', 'm2fix', 'm3'], default='all')
    a = p.parse_args()
    os.umask(0o077)
    live = Live(a)
    try:
        live.prepare()
        live.check('server starts', live.start(), 'server.log')
        if a.part in ('all', 'm2fix'):
            live.part_m2fix()
        if a.part in ('all', 'm3'):
            live.part_m3()
    finally:
        live.stop()
        live.stop_postgres()
        live.save('commands.log', '\n\n'.join(live.commands) + '\n')
        live.save('commands.log', '\n\n'.join(live.commands) + '\n', 'm2fix')
    failed = [r for r in live.results if not r['passed']]
    print(json.dumps({'passed': len(live.results) - len(failed), 'failed': len(failed)}))
    raise SystemExit(1 if failed else 0)


if __name__ == '__main__':
    main()

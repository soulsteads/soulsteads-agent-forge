#!/usr/bin/env python3
"""Live proof for installed plugins (M2): install, enable and run the test-only fixture
through every surface, with a real PostgreSQL, real server restarts, a real MCP client
and a real Chromium.

Needs a Python with `mcp` and `playwright` (for example the probe venv) and an installed
Chromium. Owns a fresh Forge home, PostgreSQL clusters and loopback ports under --state;
it never touches an incumbent home. Run:

  python3 scripts/qualify_plugins.py --forge BIN --forge-failpoint BIN_WITH_test-adapters \
      --pg-bin PGBIN --state DIR --evidence DIR --port 19200 --pg-port 19201 --port2 19202 --pg-port2 19203

Every check records PASS/FAIL with its evidence file; nothing is retried to make it pass.
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


def rmtree(path):
    """Remove a tree that contains read-only plugin releases."""
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
        self.ev.mkdir(parents=True, exist_ok=True)
        self.state.mkdir(parents=True, exist_ok=True)
        self.home = self.state / 'home'
        self.forge = str(a.forge.resolve(strict=True))
        self.forge_fp = str(a.forge_failpoint.resolve(strict=True))
        self.pg = a.pg_bin.resolve(strict=True)
        self.url = f'http://127.0.0.1:{a.port}'
        self.proc = None
        self.results = []
        self.commands = []
        self.receipts = self.state / 'receipts.jsonl'
        self.db_url = f'postgresql://forge@127.0.0.1:{a.pg_port}/forge_m2'
        self.token = 'unset'

    # ---- bookkeeping ----------------------------------------------------------------
    def check(self, name, passed, evidence='', detail=''):
        self.results.append({'check': name, 'passed': bool(passed), 'evidence': evidence, 'detail': str(detail)[:300]})
        print(('PASS ' if passed else 'FAIL ') + name + ('' if passed else f'  {detail}'))
        (self.ev / 'results.json').write_text(json.dumps(self.results, indent=2) + '\n')

    def save(self, name, value):
        path = self.ev / name
        path.write_text(value if isinstance(value, str) else json.dumps(value, indent=2, default=str) + '\n')
        return name

    # ---- infrastructure ---------------------------------------------------------------
    def postgres(self, data, port, database):
        for p in (port,):
            with socket.socket() as s:
                s.bind(('127.0.0.1', p))
        if not data.exists():
            run([self.pg / 'initdb', '-D', data, '-A', 'trust', '-U', 'forge', '--no-locale', '--encoding=UTF8'])
        run([self.pg / 'pg_ctl', '-D', data, '-l', data.parent / (data.name + '.log'), '-o',
             f"-h 127.0.0.1 -p {port} -c unix_socket_directories=''", '-w', 'start'])
        run([self.pg / 'createdb', '-h', '127.0.0.1', '-p', port, '-U', 'forge', database])

    def stop_postgres(self, data):
        if (data / 'postmaster.pid').exists():
            try:
                run([self.pg / 'pg_ctl', '-D', data, '-m', 'fast', '-w', 'stop'])
            except Exception:
                os.kill(int((data / 'postmaster.pid').read_text().splitlines()[0]), signal.SIGKILL)

    def env(self, **extra):
        return {**os.environ, 'FORGE_DATABASE_URL': self.db_url, 'FORGE_TOKEN': self.token,
                'FORGE_URL': self.url, 'DEMO_PLUGIN_RECEIPTS': str(self.receipts),
                'PATH': f'{self.pg}:' + os.environ['PATH'], **extra}

    def start(self, binary=None, home=None, port=None, **extra):
        home = home or self.home
        port = port or self.a.port
        log = open(self.state / 'server.log', 'ab')
        self.proc = subprocess.Popen([binary or self.forge, '--home', str(home), 'serve', '--port', str(port)],
                                     env=self.env(**extra), stdout=log, stderr=log)
        log.close()
        for _ in range(240):
            if self.proc.poll() is not None:
                return False
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{port}', timeout=1).read()
                return True
            except OSError:
                time.sleep(.25)
        return False

    def stop(self, sig=signal.SIGTERM):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(sig)
            self.proc.wait(timeout=60)

    # ---- clients ----------------------------------------------------------------------
    def cli(self, *args, home=None, url=None, token=None, binary=None, name=None):
        """Run the installed CLI; returns (returncode, stdout, stderr) and logs the command."""
        cmd = [binary or self.forge, '--home', str(home or self.home), '--url', url or self.url, *args]
        env = self.env()
        if token is not None:
            env['FORGE_TOKEN'] = token
        r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=300)
        line = ' '.join(['soulsteads-forge', *args[:0], '--home', '$HOME_DIR', '--url', url or self.url, *args])
        self.commands.append(f'{line}\n  -> rc={r.returncode}\n  stdout: {r.stdout.strip()[:600]}\n  stderr: {r.stderr.strip()[:300]}')
        return r.returncode, r.stdout, r.stderr

    def api(self, method, token=None, base=None, **params):
        request = urllib.request.Request((base or self.url) + '/api',
                                         data=json.dumps({'method': method, 'params': params}).encode(),
                                         headers={'Authorization': 'Bearer ' + (token or self.token), 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read() or b'{}')

    def http(self, method, path, token=None, body=None, base=None, raw=False):
        headers = {}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        if body is not None:
            headers['Content-Type'] = 'application/json'
        request = urllib.request.Request((base or self.url) + path, method=method, headers=headers,
                                         data=None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode()))
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                data = response.read()
                return response.status, dict(response.headers), data if raw else json.loads(data or b'null')
        except urllib.error.HTTPError as error:
            data = error.read()
            try:
                return error.code, dict(error.headers), data if raw else json.loads(data or b'null')
            except ValueError:
                return error.code, dict(error.headers), data

    def run_work(self, work_id, key, accept=('SUCCESS',), timeout=90):
        status, body = self.api('run', work_id=work_id, key=key)
        if status != 200:
            return status, body
        run_id = body['result']['run_id']
        deadline = time.time() + timeout
        while time.time() < deadline:
            status, row = self.api('inspect', run_id=run_id)
            if status == 200 and row['result']['status'] not in ('PENDING', 'ENQUEUED', 'ADMITTED', 'DELAYED'):
                return 200, row['result']
            time.sleep(.5)
        raise AssertionError('run did not finish: ' + run_id)

    def mcp(self, fn, token=None, binary=None):
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        async def session():
            env = {**os.environ, 'FORGE_TOKEN': token or self.token}
            params = StdioServerParameters(command=binary or self.forge,
                                           args=['mcp', '--url', self.url, '--token-env', 'FORGE_TOKEN'], env=env)
            with open(self.state / 'mcp-stderr.log', 'ab') as err:
                async with stdio_client(params, errlog=err) as (read, write):
                    async with ClientSession(read, write) as client:
                        await client.initialize()
                        return await fn(client)
        return asyncio.run(session())

    def plugin_tool_names(self):
        async def names(client):
            return [t.name for t in (await client.list_tools()).tools]
        return self.mcp(names)

    # ---- scenario ---------------------------------------------------------------------
    def main(self):
        pg1, pg2 = self.state / 'pg1', self.state / 'pg2'
        try:
            self.postgres(pg1, self.a.pg_port, 'forge_m2')
            subprocess.run([self.forge, '--home', str(self.home), 'init'], check=True, capture_output=True, env=self.env(FORGE_TOKEN='x'))
            cfg_path = self.home / 'config.json'
            cfg = json.loads(cfg_path.read_text())
            self.token = cfg['actors']['operator']['token']
            self.viewer = 'v' * 40
            cfg['actors']['operator']['types'] += ['plugin.demo-fixture', 'plugin.demo-two', 'plugin.demo-three']
            cfg['actors']['viewer'] = {'token': self.viewer, 'capabilities': ['read'], 'types': ['plugin.demo-fixture']}
            cfg_path.write_text(json.dumps(cfg))
            self.token_unknown = 'u' * 40
            self.check('server starts', self.start(), 'server.log')
            self.phase_install()
            self.phase_enable_and_surfaces()
            self.phase_dashboard()
            self.phase_restart()
            self.phase_disable_uninstall()
            self.phase_crash()
            self.phase_backup_restore(pg2)
        finally:
            self.stop()
            self.stop_postgres(pg1)
            self.stop_postgres(pg2)
            self.save('commands.log', '\n\n'.join(self.commands) + '\n')
            (self.ev / 'results.json').write_text(json.dumps(self.results, indent=2) + '\n')
        failed = [r for r in self.results if not r['passed']]
        print(json.dumps({'passed': len(self.results) - len(failed), 'failed': len(failed)}))
        raise SystemExit(1 if failed else 0)

    def variant(self, plugin_id, tamper=False):
        """A copy of the fixture under another id (types follow), digests recomputed unless tampering."""
        import hashlib
        out = self.state / f'src-{plugin_id}{"-tampered" if tamper else ""}'
        if out.exists():
            rmtree(out)
        shutil.copytree(FIXTURE, out)
        manifest = json.loads((out / 'manifest.json').read_text())
        manifest['id'] = plugin_id
        for rel in ('jobs/daily.json', 'jobs/slow.json', 'flows/pipeline.json'):
            text = (out / rel).read_text().replace('plugin.demo-fixture', f'plugin.{plugin_id}')
            (out / rel).write_text(text)
        if tamper:
            with open(out / 'ui/page.js', 'a') as f:
                f.write('\n// tampered after digests were computed\n')
        for r in manifest['resource_declarations']:
            if not tamper or r['path'] != 'ui/page.js':
                r['digest'] = 'sha256:' + hashlib.sha256((out / r['path']).read_bytes()).hexdigest()
        (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        return out

    def installed(self):
        code, out, _ = self.cli('plugins', 'installed', '--json')
        return json.loads(out) if code == 0 else None

    def phase_install(self):
        code, out, err = self.cli('plugins', 'check', str(FIXTURE))
        self.check('plugins check validates the fixture without installing it',
                   code == 0 and '"installed": false' in out and self.installed() == [], self.save('p1-check.txt', out + err))
        tampered = self.variant('demo-fixture', tamper=True)
        code, out, err = self.cli('plugins', 'install', str(tampered))
        staging = self.home / 'plugins/staging'
        releases = self.home / 'plugins/releases'
        clean = not any(staging.glob('*')) and not (releases.exists() and any(releases.rglob('*')))
        self.check('install with a tampered file is refused (digest mismatch); nothing half-installed',
                   code != 0 and 'digest mismatch for asset:page-js' in err and self.installed() == [] and clean,
                   self.save('p1-tampered.txt', out + err), err)
        code, out, err = self.cli('plugins', 'install', str(FIXTURE))
        listed = self.installed()
        self.check('install succeeds and the plugin starts DISABLED',
                   code == 0 and listed and listed[0]['state'] == 'disabled' and listed[0]['enabled'] is False,
                   self.save('p1-install.txt', out + err), out + err)
        self.save('p1-installed.json', listed)
        # Everything is denied while disabled.
        code, out, err = self.cli('plugins', 'run', 'demo-fixture', 'hello')
        self.check('CLI of a disabled plugin returns its declared denial (exit 3, declared message)',
                   code == 3 and 'demo-fixture is installed but disabled' in err, self.save('p1-cli-disabled.txt', f'rc={code}\n{out}{err}'))
        status, _, body = self.http('GET', '/api/plugins/demo-fixture/status', self.token)
        self.check('API of a disabled plugin answers 503', status == 503, self.save('p1-api-disabled.json', [status, body]))
        self.check('MCP lists no plugin tools while disabled', not [n for n in self.plugin_tool_names() if n.startswith('plugin_')])
        status, _, _ = self.http('GET', '/plugins/demo-fixture/assets/ui/page.js', raw=True)
        self.check('dashboard asset of a disabled plugin is 404', status == 404)
        types = self.api('snapshot')[1]['result']['types']
        self.check('disabled plugin is not a work type', 'plugin.demo-fixture' not in types)

    def phase_enable_and_surfaces(self):
        code, out, err = self.cli('plugins', 'enable', 'demo-fixture')
        snapshot = self.api('snapshot')[1]['result']
        work = {w['id']: w for w in snapshot['work']}
        schedules = {s['schedule_name']: s for s in snapshot['schedules']}
        job = work.get('plugin.demo-fixture.daily')
        schedule = schedules.get('operator.plugin.demo-fixture.daily')
        self.save('p2-snapshot-plugin-parts.json', {'work': {k: v['enabled'] for k, v in work.items() if k.startswith('plugin.')},
                                                   'schedules': {k: v['status'] for k, v in schedules.items()}})
        self.check('enable materializes namespaced jobs/workflow (enabled:false stays off) and a PAUSED native schedule',
                   code == 0 and job and job['enabled'] is False and 'plugin.demo-fixture.pipeline' in work
                   and 'plugin.demo-fixture.slow' in work and schedule and schedule['status'] == 'PAUSED', 'p2-snapshot-plugin-parts.json', out + err)
        # --- CLI
        code, out, err = self.cli('plugins', 'run', 'demo-fixture', 'hello', 'world', '--flag')
        self.check('CLI: declared command runs through the service', code == 0 and out == 'hello world --flag\n', self.save('p3-cli-hello.txt', out + err))
        code, out, err = self.cli('plugins', 'run', 'demo-fixture', 'fail')
        self.check("CLI: plugin's own exit status propagates", code == 7 and 'boom' in err, self.save('p3-cli-fail.txt', f'rc={code}\n{out}{err}'))
        code, out, err = self.cli('plugins', 'run', 'demo-fixture', 'rm', '-rf', '/')
        self.check('CLI: undeclared command refused', code != 0 and 'not declared' in err, self.save('p3-cli-undeclared.txt', f'rc={code}\n{out}{err}'))
        code, out, err = self.cli('plugins', 'run', 'demo-fixture', 'hello', token=self.token_unknown)
        self.check('CLI: unknown token refused', code != 0)
        # --- API
        base = '/api/plugins/demo-fixture'
        no_token = self.http('GET', base + '/status')
        bad_token = self.http('GET', base + '/status', self.token_unknown)
        good = self.http('GET', base + '/status?from=live', self.token)
        self.save('p3-api.json', {'no_token': no_token[0::2], 'bad_token': bad_token[0::2], 'operator': good[0::2],
                                  'headers': good[1]})
        self.check('API: no token and a wrong token are refused (403); with a token the plugin answers with the principal',
                   no_token[0] == 403 and bad_token[0] == 403 and good[0] == 200 and good[2]['principal'] == 'operator'
                   and good[2]['query'] == {'from': 'live'}, 'p3-api.json')
        viewer_get = self.http('GET', base + '/status', self.viewer)
        viewer_post = self.http('POST', base + '/echo', self.viewer, {'x': 1})
        echo = self.http('POST', base + '/echo', self.token, {'x': 1})
        undeclared = self.http('GET', base + '/secrets', self.token)
        wrong_method = self.http('DELETE', base + '/status', self.token)
        too_big = self.http('POST', base + '/echo', self.token, b' ' * 4000)
        html = self.http('GET', base + '/html', self.token)
        self.check('API: read-only actor may GET but not POST; echo 201; undeclared 404; wrong method 405; oversized 413; HTML response refused (502)',
                   (viewer_get[0], viewer_post[0], echo[0], undeclared[0], wrong_method[0], too_big[0], html[0]) == (200, 403, 201, 404, 405, 413, 502),
                   self.save('p3-api-matrix.json', [viewer_get[0], viewer_post[0], echo[0], undeclared[0], wrong_method[0], too_big[0], html[0]]))
        # --- MCP (real client)

        async def exercise(client):
            tools = (await client.list_tools()).tools
            names = [t.name for t in tools]
            call = await client.call_tool('plugin_demo-fixture__echo', {'text': 'from-mcp'})
            bad = await client.call_tool('plugin_demo-fixture__echo', {'text': 1})
            forged = await client.call_tool('plugin_demo-fixture__echo', {'text': 'x', 'actor': 'root'})
            tool = next(t for t in tools if t.name == 'plugin_demo-fixture__echo')
            return {'names': names, 'ok': [c.text for c in call.content], 'ok_error': call.isError,
                    'bad_error': bad.isError, 'forged_error': forged.isError, 'read_only': tool.annotations.readOnlyHint}
        result = self.mcp(exercise)
        self.save('p3-mcp.json', result)
        self.check('MCP: namespaced plugin tool listed, called through a real client, bad/forged arguments rejected',
                   'plugin_demo-fixture__echo' in result['names'] and 'forge_snapshot' in result['names'] and 'from-mcp' in result['ok'][0]
                   and not result['ok_error'] and result['bad_error'] and result['forged_error'] and result['read_only'], 'p3-mcp.json')
        # --- jobs through DBOS
        self.api('enable', work_id='plugin.demo-fixture.daily', enabled=True)
        status, row = self.run_work('plugin.demo-fixture.daily', 'job-now-1')
        self.save('p3-job-run.json', row)
        self.check('job: run now as a DBOS workflow (plugin executes via the existing effect path)',
                   status == 200 and row['status'] == 'SUCCESS' and 'demo:scheduled' in json.dumps(row), 'p3-job-run.json')
        again = self.api('run', work_id='plugin.demo-fixture.daily', key='job-now-1')[1]['result']['run_id']
        self.check('job: reusing the key returns the same run and does not execute again', again == row['run_id'])
        self.api('enable', work_id='plugin.demo-fixture.pipeline', enabled=True)
        status, flow = self.run_work('plugin.demo-fixture.pipeline', 'flow-1')
        self.save('p3-workflow-run.json', flow)
        self.check('workflow: plugin step then core step, DBOS checkpoints', status == 200 and flow['status'] == 'SUCCESS'
                   and 'got demo:flow' in json.dumps(flow), 'p3-workflow-run.json')
        name = 'operator.plugin.demo-fixture.daily'
        status, trig = self.api('schedule_trigger', name=name)
        trigger_run = trig['result'].get('run_id') if status == 200 else None
        triggered = None
        if trigger_run:
            for _ in range(120):
                triggered = self.api('inspect', run_id=trigger_run)[1]['result']
                if triggered['status'] not in ('PENDING', 'ENQUEUED', 'ADMITTED', 'DELAYED'):
                    break
                time.sleep(.5)
        self.save('p3-schedule-trigger.json', {'trigger': trig, 'run': triggered})
        self.check('schedule: DBOS native manual trigger of the (paused) job schedule runs it',
                   triggered and triggered['status'] == 'SUCCESS' and 'demo:scheduled' in json.dumps(triggered), 'p3-schedule-trigger.json')
        self.api('schedule_action', name=name, action='resume')
        fired = None
        deadline = time.time() + 150
        while time.time() < deadline and fired is None:
            show = self.api('schedule_show', name=name)[1]['result']
            for occurrence in show.get('occurrences', []):
                text = json.dumps(occurrence)
                if occurrence.get('outcome') == 'succeeded' and 'trigger' not in str(occurrence.get('occurrence_id', '')):
                    fired = occurrence
            time.sleep(3)
        self.save('p3-schedule-tick.json', self.api('schedule_show', name=name)[1])
        self.check('schedule: a natural cron tick fired the job via the DBOS scheduled workflow',
                   fired is not None, 'p3-schedule-tick.json')
        self.api('schedule_action', name=name, action='pause')

    def phase_dashboard(self):
        from playwright.sync_api import sync_playwright
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=self.a.chromium, headless=True, args=['--no-sandbox'])
            page = browser.new_page(viewport={'width': 1200, 'height': 900})
            errors = []
            failed = []
            page.on('console', lambda m: errors.append(m.text) if m.type == 'error' and 'status of 404' not in m.text else None)
            page.on('pageerror', lambda e: errors.append(str(e)))
            # The dashboard has no favicon; any other failed request would be a real problem.
            page.on('response', lambda r: failed.append(r.url) if r.status >= 400 and not r.url.endswith('/favicon.ico') else None)
            page.goto(self.url)
            page.fill('#token', self.token)
            page.click('#connect button:not([type=button])')
            page.wait_for_selector('#health:has-text("Engine")', timeout=30000)
            nav = page.get_by_role('link', name='Demo Plugin')
            nav.wait_for(timeout=15000)
            nav.click()
            page.wait_for_selector('#demo-plugin-heading', timeout=15000)
            page.wait_for_selector('#demo-plugin-status:has-text("API status 200")', timeout=15000)
            heading = page.inner_text('#demo-plugin-heading')
            status_text = page.inner_text('#demo-plugin-status')
            border = page.evaluate("getComputedStyle(document.getElementById('demo-plugin-heading')).borderLeftWidth")
            page.screenshot(path=str(self.ev / 'dashboard-plugin-page.png'), full_page=True)
            page.goto(self.url + '/#/plugins')
            page.wait_for_selector('#pl-installed', timeout=15000)
            page.screenshot(path=str(self.ev / 'dashboard-plugins-view.png'), full_page=True)
            csp_headers = page.request.get(self.url + '/plugins/demo-fixture/assets/ui/page.js').headers
            self.save('p4-dashboard.json', {'heading': heading, 'status': status_text, 'css_border_left': border,
                                           'console_errors': errors, 'failed_requests_except_favicon': failed, 'asset_headers': csp_headers})
            self.check('dashboard: plugin nav entry appears, the module loads via import(), calls its own API route, stylesheet applies (real Chromium)',
                       heading.endswith('demo-fixture') and 'API status 200' in status_text and border not in ('0px', '') and not errors and not failed,
                       'p4-dashboard.json + dashboard-plugin-page.png', errors)
            self.check('dashboard asset is served with a JS content type, nosniff and a restrictive same-origin CSP',
                       csp_headers.get('content-type', '').startswith('text/javascript') and csp_headers.get('x-content-type-options') == 'nosniff'
                       and "default-src 'none'" in csp_headers.get('content-security-policy', ''), 'p4-dashboard.json')
            browser.close()

    def phase_restart(self):
        self.stop()
        started = self.start()
        listed = self.installed() or [{}]
        code, out, _ = self.cli('plugins', 'run', 'demo-fixture', 'hello', 'after-restart')
        api = self.http('GET', '/api/plugins/demo-fixture/status', self.token)
        snapshot = self.api('snapshot')[1]['result']
        work = {w['id'] for w in snapshot['work']}
        schedules = {s['schedule_name']: s['status'] for s in snapshot['schedules']}
        self.save('p5-restart.json', {'plugin': listed[0], 'cli': out, 'api': api[0], 'schedules': schedules})
        self.check('restart persistence: plugin still enabled, CLI/API/MCP/work/schedule intact',
                   started and listed[0].get('state') == 'enabled' and code == 0 and api[0] == 200
                   and 'plugin_demo-fixture__echo' in self.plugin_tool_names() and 'plugin.demo-fixture.daily' in work
                   and schedules.get('operator.plugin.demo-fixture.daily') == 'PAUSED', 'p5-restart.json')

    def phase_disable_uninstall(self):
        # A user flow that references the plugin type blocks uninstall later.
        self.api('register', work_id='uses-demo', definition={'type': 'workflow', 'input': {'steps': [
            {'id': 'a', 'action': {'type': 'plugin.demo-fixture', 'input': {'text': 'user'}}}]}})
        # An active run of the slow plugin job blocks disable.
        self.api('enable', work_id='plugin.demo-fixture.slow', enabled=True)
        status, body = self.api('run', work_id='plugin.demo-fixture.slow', key='slow-1')
        slow_run = body['result']['run_id']
        time.sleep(3)
        code, out, err = self.cli('plugins', 'disable', 'demo-fixture')
        self.check('disable is refused while a plugin job run is active, and the plugin keeps working',
                   code != 0 and 'active run' in err and self.cli('plugins', 'run', 'demo-fixture', 'hello')[0] == 0,
                   self.save('p6-disable-refused.txt', f'rc={code}\n{out}{err}'))
        for _ in range(120):
            row = self.api('inspect', run_id=slow_run)[1]['result']
            if row['status'] not in ('PENDING', 'ENQUEUED', 'ADMITTED', 'DELAYED'):
                break
            time.sleep(.5)
        runs_before = len(self.api('runs', work_id='plugin.demo-fixture.daily', limit=100)[1]['result']['runs'])
        code, out, err = self.cli('plugins', 'disable', 'demo-fixture')
        self.check('disable succeeds once the run finished', code == 0 and row['status'] == 'SUCCESS', self.save('p6-disable.txt', out + err), err)
        # Every surface denies.
        cli_code, _, cli_err = self.cli('plugins', 'run', 'demo-fixture', 'hello')
        api = self.http('GET', '/api/plugins/demo-fixture/status', self.token)
        tools = self.plugin_tool_names()
        asset = self.http('GET', '/plugins/demo-fixture/assets/ui/page.js', raw=True)[0]
        self.api('enable', work_id='plugin.demo-fixture.daily', enabled=True)
        run_status, run_body = self.api('run', work_id='plugin.demo-fixture.daily', key='after-disable')
        flow_status, _ = self.api('run', work_id='uses-demo', key='after-disable-flow')
        snapshot = self.api('snapshot')[1]['result']
        schedule = {s['schedule_name']: s['status'] for s in snapshot['schedules']}.get('operator.plugin.demo-fixture.daily')
        self.save('p6-denied.json', {'cli': [cli_code, cli_err], 'api': api[0], 'mcp_plugin_tools': [t for t in tools if t.startswith('plugin_')],
                                    'asset': asset, 'job_run': [run_status, run_body], 'user_flow_run': flow_status,
                                    'types_has_plugin': 'plugin.demo-fixture' in snapshot['types'], 'schedule': schedule})
        self.check('disabled: CLI denial, API 503, MCP tool gone, asset 404, job and user-flow runs refused at dispatch, schedule paused, type removed',
                   cli_code == 3 and api[0] == 503 and not [t for t in tools if t.startswith('plugin_')] and asset == 404
                   and run_status != 200 and 'not enabled' in json.dumps(run_body) and flow_status != 200
                   and schedule == 'PAUSED' and 'plugin.demo-fixture' not in snapshot['types'], 'p6-denied.json')
        runs_after = len(self.api('runs', work_id='plugin.demo-fixture.daily', limit=100)[1]['result']['runs'])
        self.check('no run of the disabled plugin job was admitted', runs_after == runs_before, detail=(runs_before, runs_after))
        # Dashboard after disable.
        from playwright.sync_api import sync_playwright
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=self.a.chromium, headless=True, args=['--no-sandbox'])
            page = browser.new_page(viewport={'width': 1200, 'height': 700})
            page.goto(self.url)
            page.fill('#token', self.token)
            page.click('#connect button:not([type=button])')
            page.wait_for_selector('#health:has-text("Engine")', timeout=30000)
            gone = page.get_by_role('link', name='Demo Plugin').count() == 0
            page.goto(self.url + '/#/x/demo-fixture/home')
            page.wait_for_selector('#x-alert .alert', timeout=15000)
            message = page.inner_text('#x-alert')
            page.screenshot(path=str(self.ev / 'dashboard-after-disable.png'), full_page=True)
            browser.close()
        self.check('dashboard: nav entry gone and the plugin page refuses to load after disable', gone and 'not available' in message,
                   self.save('p6-dashboard.txt', message))
        # Uninstall: refused while a user flow references the plugin; then removed.
        code, out, err = self.cli('plugins', 'uninstall', 'demo-fixture')
        self.check('uninstall is refused while user work references the plugin', code != 0 and 'uses-demo' in err,
                   self.save('p6-uninstall-refused.txt', f'rc={code}\n{out}{err}'))
        two = self.variant('demo-two')
        self.cli('plugins', 'install', str(two))
        enable = self.cli('plugins', 'enable', 'demo-two')
        uninstall_enabled = self.cli('plugins', 'uninstall', 'demo-two')
        self.cli('plugins', 'disable', 'demo-two')
        release = self.home / 'plugins/releases/demo-two'
        existed = release.exists()
        code, out, err = self.cli('plugins', 'uninstall', 'demo-two')
        listed = [p['id'] for p in self.installed()]
        self.check('uninstall: refused while enabled; after disable it removes the registry row and the release directory',
                   enable[0] == 0 and uninstall_enabled[0] != 0 and existed and code == 0 and not release.exists() and 'demo-two' not in listed,
                   self.save('p6-uninstall.txt', f'enabled-refusal: {uninstall_enabled[2]}\nuninstall: {out}{err}\nremaining: {listed}'))
        self.check('re-enabling the first plugin works (release digests re-verified) and resets its job to the declared disabled state',
                   self.cli('plugins', 'enable', 'demo-fixture')[0] == 0
                   and self.api('snapshot')[1]['result']['work'] and
                   {w['id']: w['enabled'] for w in self.api('snapshot')[1]['result']['work']}['plugin.demo-fixture.daily'] is False)

    def phase_crash(self):
        # Kill the server mid-install with a real abort at a named stage (test-adapters build).
        for stage in ('after_stage', 'after_rename'):
            self.stop()
            src = self.variant('demo-three')
            started = self.start(binary=self.forge_fp, FORGE_PLUGIN_FAILPOINT=stage)
            code, out, err = self.cli('plugins', 'install', str(src))
            self.proc.wait(timeout=30)
            died = self.proc.returncode
            staging = sorted(p.name for p in (self.home / 'plugins/staging').glob('*'))
            release = (self.home / 'plugins/releases/demo-three/1.0.0').exists()
            before = {'stage': stage, 'cli_rc': code, 'server_returncode': died, 'staging_entries': staging, 'release_dir_exists': release}
            self.stop()
            started2 = self.start()
            listed = [p['id'] for p in self.installed() or []]
            enable_code, _, enable_err = self.cli('plugins', 'enable', 'demo-three')
            after = {'registered': 'demo-three' in listed, 'staging_entries': sorted(p.name for p in (self.home / 'plugins/staging').glob('*')),
                     'release_dir_exists': (self.home / 'plugins/releases/demo-three/1.0.0').exists(),
                     'enable_rc': enable_code, 'enable_error': enable_err.strip()}
            retry = self.cli('plugins', 'install', str(src))
            enabled = self.cli('plugins', 'enable', 'demo-three')
            after['retry_install_rc'] = retry[0]
            after['retry_enable_rc'] = enabled[0]
            hello = self.cli('plugins', 'run', 'demo-three', 'hello', 'retry')
            after['run_after_retry'] = hello[1]
            self.cli('plugins', 'disable', 'demo-three')
            self.cli('plugins', 'uninstall', 'demo-three')
            self.save(f'p7-crash-{stage}.json', {'killed_mid_install': before, 'after_restart_and_retry': after})
            visible_before_restart = stage == 'after_stage' and bool(staging) or stage == 'after_rename' and release
            self.check(f'crash {stage}: server SIGABRT mid-install leaves debris but it is not registered; after restart it is removed, not enableable, and a retry installs and runs',
                       started and died == -6 and code != 0 and visible_before_restart and started2 and not after['registered']
                       and not after['staging_entries'] and not after['release_dir_exists'] and after['enable_rc'] != 0
                       and 'not installed' in after['enable_error'] and after['retry_install_rc'] == 0 and after['retry_enable_rc'] == 0
                       and 'hello retry' in after['run_after_retry'], f'p7-crash-{stage}.json', json.dumps(before))

    def phase_backup_restore(self, pg2):
        self.stop()
        archive = self.state / 'backup'
        if archive.exists():
            rmtree(archive)
        code, out, err = self.cli('backup', str(archive))
        self.save('p8-backup.txt', f'rc={code}\n{out}{err}')
        archived = (archive / 'home/plugins/releases/demo-fixture/1.0.0/manifest.json').is_file()
        self.check('backup archives the plugin releases (and registry in the admission store)', code == 0 and archived, 'p8-backup.txt', err)
        restored_home = self.state / 'restored'
        if restored_home.exists():
            rmtree(restored_home)
        self.postgres(pg2, self.a.pg_port2, 'forge_m2_restored')
        env = self.env(FORGE_DATABASE_URL=f'postgresql://forge@127.0.0.1:{self.a.pg_port2}/forge_m2_restored')
        r = subprocess.run([self.forge, '--home', str(restored_home), 'restore', str(archive)], env=env, capture_output=True, text=True, timeout=300)
        self.save('p8-restore.txt', f'rc={r.returncode}\n{r.stdout}{r.stderr}')
        self.check('restore verifies the plugin releases and re-freezes them', r.returncode == 0, 'p8-restore.txt', r.stderr)
        # Tampered archive: restore must refuse.
        bad_archive = self.state / 'backup-tampered'
        if bad_archive.exists():
            rmtree(bad_archive)
        self.db_url_backup = self.db_url
        shutil.copytree(archive, bad_archive)
        script = bad_archive / 'home/plugins/releases/demo-fixture/1.0.0/bin/demo-plugin'
        os.chmod(script.parent, 0o700)
        os.chmod(script, 0o700)
        script.write_text('#!/bin/sh\necho tampered\n')
        tampered = subprocess.run([self.forge, '--home', str(self.state / 'restored-bad'), 'restore', str(bad_archive)], env=env,
                                  capture_output=True, text=True, timeout=300)
        self.save('p8-restore-tampered.txt', f'rc={tampered.returncode}\n{tampered.stdout}{tampered.stderr}')
        self.check('a tampered archive is refused (archive inventory digests)', tampered.returncode != 0, 'p8-restore-tampered.txt')
        # Run the restored home on its own port and database.
        self.db_url = f'postgresql://forge@127.0.0.1:{self.a.pg_port2}/forge_m2_restored'
        self.url = f'http://127.0.0.1:{self.a.port2}'
        started = self.start(home=restored_home, port=self.a.port2)
        listed = self.installed() or [{}]
        code, out, _ = self.cli('plugins', 'run', 'demo-fixture', 'hello', 'restored', home=restored_home)
        mode = oct((restored_home / 'plugins/releases/demo-fixture/1.0.0/ui/page.js').stat().st_mode & 0o777)
        self.save('p8-restored-run.json', {'plugin': listed[0], 'cli': out, 'page_js_mode': mode})
        self.check('restored home: the plugin is enabled and its restored executable runs; releases are read-only again',
                   started and listed[0].get('state') == 'enabled' and code == 0 and out == 'hello restored\n' and mode == '0o400', 'p8-restored-run.json')


if __name__ == '__main__':
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--forge', type=Path, required=True)
    p.add_argument('--forge-failpoint', type=Path, required=True, help='build with --features test-adapters (named crash points)')
    p.add_argument('--pg-bin', type=Path, required=True)
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--evidence', type=Path, required=True)
    p.add_argument('--port', type=int, required=True)
    p.add_argument('--pg-port', type=int, required=True)
    p.add_argument('--port2', type=int, required=True)
    p.add_argument('--pg-port2', type=int, required=True)
    p.add_argument('--chromium', default=os.environ.get('CHROMIUM_EXECUTABLE', '/usr/bin/chromium'))
    Live(p.parse_args()).main()

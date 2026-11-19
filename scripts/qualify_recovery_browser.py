#!/usr/bin/env python3
"""Read-only installed CLI/MCP/Chromium agreement for retained restored runs.

Requires Python 3.11+, playwright, mcp 2.x, and an installed Chromium. The service
owner supplies --host-ready; this driver never manages Forge or admits work.
Use a fresh --output for every attempt. No traces/HAR/token screenshots are saved.
See docs/recovery.md for usage and the retained recovery manifest format.
"""
import argparse
import asyncio
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import subprocess
from urllib.parse import urlsplit

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from playwright.async_api import async_playwright, expect


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def load(path):
    return json.loads(Path(path).read_text())


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def files(home, cases):
    result = {}
    for case in cases:
        for relative, expected in case['files'].items():
            path = (home / relative).resolve(strict=True)
            require(path.is_relative_to(home), 'retained file escapes restored home')
            actual = {'sha256': sha(path), 'bytes': path.stat().st_size}
            require(actual == expected, 'retained file changed: ' + relative)
            result[relative] = actual
    return result


async def qualify(args, report):
    recovery = load(args.recovery / 'evidence.json')
    preservation = load(args.recovery / 'preservation.json')
    ready = load(args.host_ready)
    home = Path(recovery['restored_home']).resolve(strict=True)
    require(recovery['status'] == 'PASS', 'backup/restore must already pass')
    require(Path(ready['home']).resolve() == home and ready['source_stays_stopped']
            and ready['schedules_paused'], 'host lifecycle handoff mismatch')
    url = ready['url'].rstrip('/')
    parsed = urlsplit(url)
    require(parsed.scheme == 'http' and parsed.hostname == '127.0.0.1'
            and parsed.port == ready['port'] and not parsed.username
            and not parsed.query and not parsed.fragment and parsed.path == '',
            'expected explicit loopback service URL')
    require(sha(args.binary) == recovery['binary_sha256'], 'installed binary changed')
    cases = recovery['cases']
    require(cases and all(re.fullmatch(r'[a-zA-Z0-9_-]+', key) for key in cases),
            'safe nonempty case labels required')
    retained = {case['case']: case for case in preservation['cases']}
    baseline = {}
    for name, case in cases.items():
        run = load(args.recovery / (name + '-final-run.stdout'))
        effect = load(args.recovery / (name + '-final-effect.stdout'))
        require(run['run_id'] == case['run_id'] == effect['run_id'], 'run identity mismatch')
        require(run['status'] == case['status'] == effect['status'], 'run state mismatch')
        record = effect['effect']
        attempt = record['harness_attempt']
        require(record['state'] == case['effect_state']
                and record['effect_id'] == retained[name]['effect_id']
                and attempt['invocation_id'] == case['invocation_id']
                and attempt['attempt'] == retained[name]['attempt_count']
                and attempt['state'] == retained[name]['attempt_state']
                and attempt['result'] == retained[name]['attempt_result'],
                'retained effect/attempt mismatch')
        require(sum(log['action'] == 'harness_launched' for log in run['logs'])
                == retained[name]['launch_log_count'], 'retained launch count mismatch')
        if record['state'] == 'uncertain':
            require(run['output'] is None and record['output'] is None
                    and record['reconciliation'] is None and run['diagnostic']
                    and attempt['failure'], 'uncertain effect lacks recovery explanation')
        baseline[name] = {'run': run, 'effect': effect}
        # Inspect every checkpoint exactly; overview projections can be summaries.
        for step in run['steps']:
            baseline[name]['step-' + str(step['function_id'])] = {
                key: run[key] for key in ('run_id', 'work_id', 'status')
            } | {'step': step}
    report.update(binary_sha256=sha(args.binary), driver_sha256=sha(Path(__file__)),
                  recovery_evidence_sha256=sha(args.recovery / 'evidence.json'),
                  host_ready=ready, files_before=files(home, cases.values()),
                  versions={name: importlib.metadata.version(name)
                            for name in ('playwright', 'mcp')}, phases=[])
    token = load(home / 'config.json')['actors'][args.actor]['token']
    env = {**os.environ, 'FORGE_TOKEN': token}
    env.pop('FORGE_TOKEN_ENV', None)
    parameters = StdioServerParameters(command=str(args.binary),
        args=['--url', url, '--token-env', 'FORGE_TOKEN', 'mcp'], env=env)

    def cli(name, kind, params, phase):
        command = [str(args.binary), '--url', url, '--token-env', 'FORGE_TOKEN']
        if kind == 'snapshot':
            command += ['snapshot']
        else:
            command += ['runs', 'inspect', params['run_id']]
            for field in ('effect_id', 'step_id'):
                if field in params:
                    command += ['--' + field.replace('_', '-'), str(params[field])]
        result = subprocess.run(command, env=env, text=True, capture_output=True, timeout=30)
        require(result.returncode == 0, 'CLI read failed: ' + name + '/' + kind)
        value = json.loads(result.stdout)
        require(token not in json.dumps(value), 'credential found in readback')
        save(phase / (name + '-' + kind + '-cli.json'), value)
        return value

    async def mcp(client, method, params):
        result = await client.call_tool('forge_' + method, params)
        require(not result.is_error, 'MCP read failed: ' + method)
        value = json.loads(result.content[0].text)
        require(token not in json.dumps(value), 'credential found in MCP readback')
        return value

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch(executable_path=str(args.chromium),
                                                   headless=True)
        report['chromium_version'] = browser.version
        try:
            context = await browser.new_context(viewport={'width': 1400, 'height': 1000})
            page = await context.new_page()
            page.set_default_timeout(15000)
            requests = []
            violations = []

            async def read_only(route):
                request = route.request
                if request.url == url + '/api' and request.method == 'POST':
                    body = request.post_data_json
                    if body.get('method') in ('snapshot', 'inspect'):
                        requests.append(body)
                        await route.continue_()
                        return
                elif request.method == 'GET' and request.url in (url + '/', url + '/favicon.ico'):
                    await route.continue_()
                    return
                violations.append({'method': request.method, 'url': request.url})
                await route.abort()

            await context.route('**/*', read_only)

            async def connect():
                # Never trace, screenshot, or log calls while token is in the field.
                await page.locator('#token').fill(token)
                await page.get_by_role('button', name='Connect', exact=True).click()
                await expect(page.locator('#status')).to_contain_text('Current as of')
                require(await page.locator('#token').input_value() == '', 'token field not cleared')

            async def action(locator):
                # Waiting for the last snapshot response plus rendered status avoids
                # accepting old #detail text while a new inspect is still in flight.
                async with page.expect_response(lambda r: r.url == url + '/api'
                        and r.request.post_data_json.get('method') == 'snapshot'):
                    await locator.click()
                await expect(page.locator('#status')).to_contain_text('Current as of')

            async def detail(expected, path):
                actual = json.loads(await page.locator('#detail').inner_text())
                save(path, actual)
                require(actual == expected, 'dashboard selected evidence differs: ' + path.name)

            async def screenshot(path, needle=None):
                # Capture only the rendered evidence panel, never the connection form.
                await page.locator('#detail').evaluate('''(el, needle) => {
                    el.scrollTop = 0;
                    if (needle) {
                        const offset = el.textContent.indexOf(needle);
                        if (offset < 0) throw Error('screenshot evidence missing');
                        const range = document.createRange();
                        range.setStart(el.firstChild, offset);
                        range.setEnd(el.firstChild, offset + needle.length);
                        el.scrollTop = range.getBoundingClientRect().top
                            - el.getBoundingClientRect().top - 32;
                    }
                }''', needle)
                await page.locator('#detail').screenshot(path=str(path))

            await page.goto(url + '/')
            report['dashboard_sha256'] = hashlib.sha256(
                (await page.content()).encode()).hexdigest()
            await connect()
            for phase_name in ('initial', 'refresh', 'reload-reconnect'):
                phase = args.output / phase_name
                phase.mkdir()
                if phase_name == 'refresh':
                    await action(page.locator('#refresh'))
                    # The final selected checkpoint remains selected after Refresh.
                    await detail(last_expected, phase / 'retained-selection-browser.json')
                elif phase_name == 'reload-reconnect':
                    await page.reload()
                    await expect(page.locator('#status')).to_contain_text('Disconnected')
                    require(await page.locator('#token').input_value() == '', 'token survived reload')
                    require(await page.evaluate('localStorage.length + sessionStorage.length') == 0,
                            'unexpected browser storage')
                    await connect()
                # A new real stdio MCP connection/handshake in every phase.
                with open(os.devnull, 'w') as errors:
                    async with stdio_client(parameters, errlog=errors) as (read, write):
                        async with ClientSession(read, write) as client:
                            await client.initialize()
                            snapshot = await mcp(client, 'snapshot', {})
                            save(phase / 'snapshot-mcp.json', snapshot)
                            cli_snapshot = cli('state', 'snapshot', {}, phase)
                            require(snapshot['runs'] == cli_snapshot['runs']
                                    and snapshot['schedules'] == cli_snapshot['schedules'],
                                    'CLI/MCP state mismatch')
                            require(all(s['status'] == 'PAUSED' for s in snapshot['schedules']),
                                    'schedule resumed')
                            for name, records in baseline.items():
                                run = records['run']
                                row = page.locator('#run-list p').filter(has_text=run['run_id'])
                                await expect(row).to_contain_text(run['status'])
                                await row.screenshot(path=str(phase / (name + '-history.png')))
                                await action(row.get_by_role('button', name='Inspect', exact=True))
                                await detail(run, phase / (name + '-history-inspect-browser.json'))
                                for kind, expected in records.items():
                                    params = {'run_id': run['run_id']}
                                    selector = ''
                                    record_kind = kind
                                    if kind == 'effect':
                                        selector = expected['effect']['effect_id']
                                        params['effect_id'] = selector
                                    elif kind.startswith('step-'):
                                        record_kind = 'step'
                                        selector = kind.removeprefix('step-')
                                        params['step_id'] = int(selector)
                                    form = page.locator('#inspect-detail')
                                    await form.locator('[name=run_id]').fill(run['run_id'])
                                    await form.locator('[name=kind]').select_option(record_kind)
                                    await form.locator('[name=selector]').fill(selector)
                                    await action(form.get_by_role('button', name='Read evidence'))
                                    await detail(expected, phase / (name + '-' + kind + '-browser.json'))
                                    cli_value = cli(name, kind, params, phase)
                                    mcp_value = await mcp(client, 'inspect', params)
                                    save(phase / (name + '-' + kind + '-mcp.json'), mcp_value)
                                    require(cli_value == mcp_value == expected,
                                            'CLI/MCP/retained mismatch: ' + name + '/' + kind)
                                    if kind == 'run':
                                        await screenshot(phase / (name + '-diagnostic.png'), '"diagnostic"')
                                        if run['output'] is not None:
                                            await screenshot(phase / (name + '-dependent-output.png'), '"dependent": "')
                                    elif kind == 'effect':
                                        await screenshot(phase / (name + '-attempt.png'), '"harness_attempt"')
                                        await screenshot(phase / (name + '-outcome.png'), '"output":')
                                    last_expected = expected
                report['phases'].append({'phase': phase_name, 'mcp_reconnected': True,
                    'cases': {name: {'run_id': value['run_id'], 'status': value['status'],
                                    'effect_state': value['effect_state'],
                                    'invocation_id': value['invocation_id'],
                                    'exact_records': len(baseline[name])}
                              for name, value in cases.items()}})
                save(args.output / 'progress.json', report)
            require(not violations, 'browser attempted non-read request')
            save(args.output / 'browser-read-requests.json', requests)
            report['browser_api_reads'] = len(requests)
            report['files_after'] = files(home, cases.values())
            require(report['files_before'] == report['files_after'], 'retained bytes changed')
        finally:
            await browser.close()
            report['browser_closed'] = not browser.is_connected()
    report['status'] = 'PASS'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('binary', 'chromium', 'recovery', 'host-ready', 'output'):
        parser.add_argument('--' + flag, type=Path, required=True)
    parser.add_argument('--actor', default='operator')
    args = parser.parse_args()
    for field in ('binary', 'chromium', 'recovery', 'host_ready'):
        setattr(args, field, getattr(args, field).resolve(strict=True))
    args.output = args.output.resolve()
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=False)
    report = {'status': 'FAIL', 'limits': [
        'Only retained restored runs; no model, admission, replay, reconciliation or schedule action',
        'Refresh/reload and fresh MCP connections; service lifecycle owned externally',
        'Cancelled workspace files are preservation evidence, not validated output artifacts',
        'No claim for another runtime pairing or external provider quiescence']}
    try:
        asyncio.run(asyncio.wait_for(qualify(args, report), timeout=240))
    except Exception as error:
        # Exception text from browser libraries may contain fill arguments/tokens.
        report['error_type'] = type(error).__name__
        def failures(exc):
            if isinstance(exc, BaseExceptionGroup):
                return [item for child in exc.exceptions for item in failures(child)]
            return [{'type': type(exc).__name__,
                     'error': str(exc) if type(exc) is RuntimeError else 'library error'}]
        report['errors'] = failures(error)
        raise SystemExit('FAIL: inspect evidence.json and retained phase readbacks') from None
    finally:
        save(args.output / 'evidence.json', report)
    print('PASS: restored CLI/MCP/Chromium evidence agrees; browser closed')


if __name__ == '__main__':
    main()

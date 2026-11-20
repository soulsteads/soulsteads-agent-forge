#!/usr/bin/env python3
"""Bounded no-model installed smoke; Python standard library only.

Required: FORGE_BINARY, FORGE_TEST_STATE (nonexistent), FORGE_DATABASE_URL
(empty dedicated PostgreSQL). Optional FORGE_TEST_PORT (default 18110).
FORGE_BROWSER=1 adds Playwright/Chromium readback; requires TMPDIR and
CHROMIUM_EXECUTABLE. FORGE_NESTED_WORKFLOW_UI=1 selects just the nested workflow
builder journey and requires the same browser prerequisites. No retry of
mutations; retained failure evidence is final.
"""
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time
import urllib.error
import urllib.request


def main():
    os.umask(0o077)
    state = Path(os.environ['FORGE_TEST_STATE']).resolve()
    state.mkdir(parents=True, exist_ok=False)
    binary = Path(os.environ['FORGE_BINARY']).resolve(strict=True)
    home = state / 'home'
    port = os.environ.get('FORGE_TEST_PORT', '18110')
    url = 'http://127.0.0.1:' + port
    assert os.environ['FORGE_DATABASE_URL'], 'dedicated database required'
    base = [str(binary), '--home', str(home)]
    subprocess.run(base + ['init'], check=True, capture_output=True, timeout=20)
    config = json.loads((home / 'config.json').read_text())
    assert not config['models'] and not config['plugins'] and not config.get('runners')
    token = config['actors']['operator']['token']
    env = {**os.environ, 'FORGE_TOKEN': token}
    commands = []
    result = {'passed': False, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'models': 0, 'plugins': 0, 'candidate': os.environ.get('FORGE_CANDIDATE_SHA')}
    proc = None
    schedule = None
    log = (state / 'server.log').open('w')

    def save(name, value):
        raw = json.dumps(value, indent=2) + '\n'
        assert token not in raw, 'refuse token in evidence'
        (state / name).write_text(raw)

    def cli(*args, data=None):
        p = subprocess.run([str(binary), '--url', url] + list(args), env=env,
                           input=json.dumps(data) if data is not None else None,
                           text=True, capture_output=True, timeout=30)
        assert token not in p.stdout + p.stderr
        commands.append({'args': args, 'exit': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr})
        assert p.returncode == 0, (args, p.returncode, p.stderr)
        return json.loads(p.stdout)

    def stop():
        if proc is not None and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=20)

    def start():
        nonlocal proc
        proc = subprocess.Popen(base + ['serve', '--port', port], env=env, stdout=log, stderr=log)
        for _ in range(120):
            assert proc.poll() is None, 'service exited; inspect retained server.log'
            try:
                with urllib.request.urlopen(url, timeout=1) as response:
                    if response.status == 200:
                        return
            except (urllib.error.URLError, TimeoutError):
                pass
            time.sleep(.25)
        raise AssertionError('startup deadline')

    def wait(run_id):
        for _ in range(120):
            row = cli('runs', 'inspect', run_id)
            if row['status'] not in ('ADMITTED', 'PENDING', 'ENQUEUED'):
                assert row['status'] == 'SUCCESS', row['status']
                return row
            time.sleep(.25)
        raise AssertionError('run deadline')

    def mcp_calls(*calls):
        with (state / 'mcp.stderr').open('a') as err:
            bridge = subprocess.Popen([str(binary), 'mcp', '--url', url], env=env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err)
            pending = bytearray()

            def send(value):
                bridge.stdin.write((json.dumps(value) + '\n').encode())
                bridge.stdin.flush()

            def receive(request_id):
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    if b'\n' in pending:
                        line, _, rest = pending.partition(b'\n')
                        pending[:] = rest
                        value = json.loads(line)
                        if value.get('id') == request_id:
                            assert 'error' not in value, value
                            return value['result']
                    elif select.select([bridge.stdout], [], [], .25)[0]:
                        chunk = os.read(bridge.stdout.fileno(), 65536)
                        assert chunk, 'MCP closed before response'
                        pending.extend(chunk)
                raise AssertionError('MCP response deadline')
            try:
                send({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
                    'protocolVersion': '2024-11-05', 'capabilities': {},
                    'clientInfo': {'name': 'forge-installed-smoke', 'version': '1'}}})
                receive(1)
                send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
                results = []
                for request_id, (name, args) in enumerate(calls, start=2):
                    send({'jsonrpc': '2.0', 'id': request_id, 'method': 'tools/call', 'params': {
                        'name': name, 'arguments': args}})
                    value = receive(request_id)
                    assert not value.get('isError'), value
                    results.append(json.loads(value['content'][0]['text']))
                return results
            finally:
                bridge.stdin.close()
                try:
                    bridge.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    bridge.terminate()
                    bridge.wait(timeout=5)

    def mcp_read(run_id):
        return mcp_calls(('forge_inspect', {'run_id': run_id}))[0]

    def nested_workflow_ui():
        from playwright.sync_api import expect, sync_playwright

        assert Path(os.environ['TMPDIR']).is_dir()
        work_id = 'nested-ui-report'
        def ref(path):
            return {'$ref': path}

        definition = {'type': 'workflow', 'input': {
            'params': {'topic': 'checklist ready', 'facts': [3, False, 'reviewed']},
            'steps': [
                {'id': 'seed', 'action': {'type': 'echo', 'input': {'text': ref('/params/topic')}}},
                {'id': 'nested', 'needs': ['seed'], 'action': {'type': 'workflow', 'input': {
                    'params': {'topic': ref('/steps/seed/output/text'), 'facts': ref('/params/facts')},
                    'steps': [
                        {'id': 'prepare', 'action': {'type': 'echo', 'input': {'text': ref('/params/topic')}}},
                        {'id': 'review', 'needs': ['prepare'], 'action': {'type': 'echo', 'input': {
                            'text': 'Reviewed: ${/steps/prepare/output/text}'}}}],
                    'output': {'text': ref('/steps/review/output/text'), 'facts': ref('/params/facts')}}}},
                {'id': 'report', 'needs': ['nested'], 'action': {'type': 'echo', 'input': {
                    'text': 'Delivered: ${/steps/nested/output/output/text}'}}}],
            'output': {'report': ref('/steps/report/output/text'),
                       'facts': ref('/steps/nested/output/output/facts')}}}
        expected = {'report': 'Delivered: Reviewed: checklist ready', 'facts': [3, False, 'reviewed']}

        with sync_playwright() as pw:
            browser = pw.chromium.launch(headless=True, executable_path=os.environ['CHROMIUM_EXECUTABLE'])
            try:
                page = browser.new_page()
                page.set_default_timeout(15000)

                def connect():
                    page.locator('#token').fill(token)
                    page.get_by_role('button', name='Connect', exact=True).click()
                    expect(page.locator('#status')).to_contain_text('Current as of')

                page.goto(url)
                connect()
                nav_links = page.locator('#forge-navigation a[data-view]:not([data-plugin-nav])')
                icons = nav_links.locator('.nav-icon[aria-hidden="true"]')
                assert nav_links.count() > 0
                assert all(link.locator('.nav-icon[aria-hidden="true"]').count() == 1
                           for link in nav_links.all()), 'each core link needs one decorative icon'
                glyphs = icons.all_text_contents()
                assert all(glyphs) and len(set(glyphs)) == len(glyphs), 'expected distinct nonempty icons'
                page.get_by_role('link', name='Workflows', exact=True).click()
                expect(page.locator('#wf-empty')).to_contain_text('Create a workflow')
                expect(page.locator('#wf-builder')).to_have_count(0)
                page.get_by_role('link', name='+ New workflow', exact=True).click()
                page.get_by_role('button', name='Edit as JSON', exact=True).click()
                raw = page.locator('#wf-raw')

                def apply(definition):
                    raw.fill(json.dumps(definition))
                    page.get_by_role('button', name='Apply JSON to builder', exact=True).click()
                    return json.loads(page.locator('#wf-preview').text_content())

                preview = apply(definition)
                save('nested-round-trip.json', {'expected': definition, 'preview': preview})
                assert preview == definition, 'browser builder altered the legal nested definition'
                page.get_by_role('button', name='Edit as JSON', exact=True).click()
                page.get_by_role('button', name='Edit as JSON', exact=True).click()
                exported = json.loads(raw.input_value())
                assert exported == definition and apply(exported) == definition
                save('nested-round-trip.json', {'expected': definition, 'preview': preview, 'exported': exported})
                literal_output = {**definition, 'input': {**definition['input'], 'output': False}}
                assert apply(literal_output)['input']['output'] is False

                invalid = json.loads(json.dumps(definition))
                invalid['input']['steps'][1]['action']['input']['steps'][1]['needs'] = ['missing']
                assert apply(invalid) == invalid
                page.get_by_role('button', name='Validate', exact=True).click()
                diag = page.locator('#wf-diag')
                expect(diag).to_contain_text('needs must contain unique earlier step IDs')
                save('nested-refusal.json', {'diagnostic': diag.inner_text()})

                assert apply(definition) == definition
                page.get_by_role('button', name='Validate', exact=True).click()
                expect(diag).to_contain_text('Valid: 3 step(s)')
                page.locator('#wf-id').fill(work_id)
                page.get_by_role('button', name='Create workflow', exact=True).click()
                expect(page.get_by_role('heading', name=work_id, exact=True)).to_be_visible()
                work = cli('workflows', 'show', work_id, '--json')
                assert work['id'] == work_id and work['definition'] == definition
                expect(page.locator('#wf-schedules')).to_contain_text('No schedules')
                page.get_by_role('link', name='+ Add schedule', exact=True).click()
                expect(page.locator('#sc-work')).to_have_value(work_id)
                page.locator('#sc-name').fill('studio-annual')
                # Target a date months away, so this UI test never executes the schedule.
                page.locator('#sc-cron').fill('0 0 1 1 *' if time.gmtime().tm_mon != 1 else '0 0 1 7 *')
                page.locator('#sc-tz').fill('UTC')
                page.locator('#sc-check').click()
                expect(page.locator('#sc-preview')).to_contain_text('Next due ticks')
                page.locator('#sc-submit').click()
                page.wait_for_url('**/#/schedules/operator.studio-annual')
                item = cli('schedules', 'show', 'operator.studio-annual', '--json')
                assert item['context']['work_id'] == work_id
                page.get_by_role('link', name='Workflows', exact=True).click()
                page.locator('#wf-search').fill('no-match')
                expect(page.locator('#wf-empty')).to_have_text('No workflows match these filters.')
                page.locator('#wf-search').fill('NESTED')
                page.locator('#wf-state').select_option('disabled')
                expect(page.locator('#wf-empty')).to_be_visible()
                page.locator('#wf-state').select_option('enabled')
                expect(page.locator('#wf-table')).to_contain_text(work_id)
                page.locator('#wf-search').clear()
                page.locator('#wf-state').select_option('')
                page.screenshot(path=str(state / 'workflow-library.png'), full_page=True)
                page.get_by_role('link', name=work_id, exact=True).click()
                expect(page.locator('#wf-schedules')).to_contain_text('operator.studio-annual')
                nested_step = page.locator('.wf-step').filter(has_text='Reads from seed')
                nested_step.locator('summary').click()
                expect(nested_step.locator('pre')).to_be_visible()
                expect(nested_step.locator('pre')).to_contain_text('/steps/seed/output/text')
                page.screenshot(path=str(state / 'workflow-detail.png'), full_page=True)
                page.set_viewport_size({'width': 375, 'height': 850})
                assert page.evaluate('document.documentElement.scrollWidth === innerWidth'), 'workflow detail overflows on mobile'
                page.screenshot(path=str(state / 'workflow-mobile.png'), full_page=True)
                navigation = page.get_by_role('button', name='Navigation', exact=True)
                navigation.click()
                page.get_by_role('link', name='Workflows', exact=True).locator('.nav-icon').click()
                expect(navigation).to_have_attribute('aria-expanded', 'false')
                expect(page.locator('#wf-table')).to_contain_text(work_id)
                assert page.evaluate('document.documentElement.scrollWidth === innerWidth'), 'workflow library overflows on mobile'
                page.set_viewport_size({'width': 1280, 'height': 720})
                page.get_by_role('link', name=work_id, exact=True).click()
                page.get_by_role('button', name='Run workflow', exact=True).click()
                page.wait_for_url('**/#/runs/*')
                run_id = page.url.rsplit('/', 1)[-1]
                wait(run_id)
                run = cli('runs', 'show', run_id, '--json')
                assert run['work_id'] == work_id and run['status'] == 'SUCCESS'
                assert run['output']['output'] == expected and not run.get('effects')
                assert run['output']['output']['facts'][1] is False
                nested = run['output']['steps']['nested']['output']
                assert nested['steps']['review']['output']['text'] == 'Reviewed: checklist ready'
                assert any('nested/step:review' in step['function_name'] for step in run['steps'])
                validated, mcp_work, mcp_run = mcp_calls(
                    ('forge_validate', {'definition': definition}),
                    ('forge_work', {'work_id': work_id}),
                    ('forge_inspect', {'run_id': run_id}))
                assert validated['valid'] and validated['steps'][1]['action'] == 'workflow'
                assert mcp_work == work and mcp_run == run
                page.get_by_role('link', name=work_id, exact=True).click()
                page.get_by_role('link', name='View run history →', exact=True).click()
                expect(page.locator('#runs-work')).to_have_value(work_id)
                expect(page.locator('#runs-results')).to_contain_text('SUCCESS')
                page.goto(f'{url}/#/runs/{run_id}')
                if not page.evaluate('window.forge.connected()'):
                    connect()
                before = cli('runs', 'list')
                assert [row['run_id'] for row in before] == [run_id]
                save('nested-before-restart.json', {
                    'work': work, 'run': run, 'mcp': {'validate': validated, 'work': mcp_work, 'run': mcp_run}})

                def read_browser():
                    expect(page.locator('.run-head')).to_contain_text(run_id)
                    expect(page.locator('#runs-body')).to_contain_text(expected['report'])

                page.reload()
                connect()
                read_browser()
                stop()
                start()
                page.reload()
                connect()
                read_browser()
                page.screenshot(path=str(state / 'nested-after-restart.png'), full_page=True)
                assert cli('runs', 'list') == before, 'run set changed after restart'
                assert cli('workflows', 'show', work_id, '--json') == work
                assert cli('runs', 'show', run_id, '--json') == mcp_read(run_id) == run
                save('nested-after-restart.json', {'run_ids': [run_id], 'run': run, 'browser': browser.version})
                return {'passed': True, 'work_id': work_id, 'manual_run': run_id,
                        'output': expected, 'browser': browser.version,
                        'mcp_tools': ['forge_validate', 'forge_work', 'forge_inspect'],
                        'restart_no_redispatch': True, 'external_effects': 0,
                        'studio_schedule': 'operator.studio-annual', 'mobile_layout_checked': True}
            finally:
                browser.close()

    try:
        start()
        request = urllib.request.Request(url + '/api', data=b'{"method":"snapshot"}',
                                         headers={'Content-Type': 'application/json'})
        try:
            urllib.request.urlopen(request, timeout=5)
            raise AssertionError('unauthenticated request accepted')
        except urllib.error.HTTPError as denied:
            assert denied.code == 403
        if os.environ.get('FORGE_NESTED_WORKFLOW_UI') == '1':
            result.update(nested_workflow_ui())
            return
        definition = {'type': 'workflow', 'input': {'params': {'task': 'release checklist ready'},
            'steps': [
                {'id': 'produce', 'action': {'type': 'echo', 'input': {'text': {'$ref': '/params/task'}}}},
                {'id': 'consume', 'needs': ['produce'], 'action': {'type': 'echo', 'input': {
                    'text': 'Observed: ${/steps/produce/output/text}'}}}],
            'output': {'report': {'$ref': '/steps/consume/output/text'}}}}
        cli('work', 'register', 'report', '--definition', '-', data=definition)
        rid = cli('work', 'run', 'report', '--key', 'first-report')['run_id']
        manual = wait(rid)
        expected = {'report': 'Observed: release checklist ready'}
        assert manual['output']['output'] == expected
        assert not manual.get('effects'), 'deterministic work unexpectedly produced external effects'
        assert cli('work', 'run', 'report', '--key', 'first-report')['run_id'] == rid
        save('manual.json', manual)
        schedule = 'operator.smoke-due'
        cli('schedules', 'create', 'smoke-due', '--work-id', 'report', '--cron', '*/5 * * * * *', '--tz', 'UTC')
        due = None
        for _ in range(120):
            occurrences = cli('schedules', 'occurrences', schedule)
            # Public result shape is validated below; inspect each occurrence's own run.
            entries = occurrences if isinstance(occurrences, list) else occurrences['occurrences']
            due = next((entry for entry in entries if entry.get('run_id')), None)
            if due:
                break
            time.sleep(.25)
        assert due is not None, 'actual due occurrence deadline'
        cli('schedules', 'pause', schedule)
        paused = cli('schedules', 'show', schedule, '--json')
        assert paused['status'] == 'PAUSED', ('schedule not paused after pause', paused['status'])
        observed = wait(due['run_id'])
        assert observed['output']['output'] == expected
        save('due.json', {'occurrence': due, 'inspection': observed})
        # Real MCP call against the running service; compare with the CLI/API records
        # created earlier in this smoke (identity, work, status and output, not output alone).
        mcp_due = mcp_read(due['run_id'])
        mcp_manual = mcp_read(rid)
        mcp_compared = {}
        for label, got, want in (('due', mcp_due, observed), ('manual', mcp_manual, manual)):
            for key in ('run_id', 'work_id', 'status', 'output'):
                assert got.get(key) == want.get(key), ('MCP readback mismatch', label, key, got.get(key), want.get(key))
            mcp_compared[label] = {'run_id': got['run_id'], 'status': got['status'], 'work_id': got['work_id']}
        assert mcp_due['run_id'] != mcp_manual['run_id'], 'MCP returned one record for both runs'
        save('mcp.json', mcp_manual)
        # Exact checkpoint selectors, not just summarized UI values.
        steps = manual['steps']
        assert steps, 'no native checkpoints'
        save('checkpoints.json', [cli('runs', 'inspect', rid, '--step-id', str(s['function_id'])) for s in steps])
        if os.environ.get('FORGE_BROWSER') == '1':
            from playwright.sync_api import expect, sync_playwright
            assert Path(os.environ['TMPDIR']).is_dir()
            with sync_playwright() as pw:
                browser = pw.chromium.launch(headless=True, executable_path=os.environ['CHROMIUM_EXECUTABLE'])
                try:
                    page = browser.new_page()
                    page.goto(url)
                    page.locator('#token').fill(token)
                    page.get_by_role('button', name='Connect', exact=True).click()
                    page.wait_for_function("() => document.querySelector('#status').textContent.startsWith('Current as of')")
                    page.locator('#token').fill('')
                    # Follow the same Runs list and detail links as an operator.
                    page.get_by_role('link', name='Runs', exact=True).click()
                    page.locator(f'#runs-results a[title="{due["run_id"]}"]').click()
                    detail = page.locator('#runs-body')
                    expect(detail.locator('.run-head')).to_contain_text(due['run_id'])
                    expect(detail.locator('.run-head')).to_contain_text('SUCCESS')
                    expect(detail).to_contain_text('Observed: release checklist ready')
                    text = detail.inner_text()
                    save('browser.json', {'run_id': due['run_id'], 'detail': text})
                    page.screenshot(path=str(state / 'browser.png'), full_page=True)
                finally:
                    browser.close()
        # Drain already-admitted due work before restart, preserving every run.
        for row in cli('runs', 'list'):
            wait(row['run_id'])
        before = cli('runs', 'list')
        before_ids = [row['run_id'] for row in before]
        assert len(before_ids) == len(set(before_ids)), ('duplicate run ids before restart', before_ids)
        stop()
        start()
        assert cli('work', 'run', 'report', '--key', 'first-report')['run_id'] == rid
        assert cli('runs', 'inspect', rid)['output'] == manual['output']
        after = cli('runs', 'list')
        after_ids = [row['run_id'] for row in after]
        assert len(after_ids) == len(set(after_ids)), ('duplicate run ids after restart', after_ids)
        assert sorted(after_ids) == sorted(before_ids), ('restart changed run set', before_ids, after_ids)
        assert after == before, 'restart changed runs'
        # Each due occurrence maps to exactly one run: no tick produced a second run.
        shown = cli('schedules', 'occurrences', schedule)
        entries = shown if isinstance(shown, list) else shown['occurrences']
        occurrence_ids = [entry['occurrence_id'] for entry in entries]
        occurrence_runs = [entry['run_id'] for entry in entries if entry.get('run_id')]
        assert len(occurrence_ids) == len(set(occurrence_ids)), ('duplicate occurrences', occurrence_ids)
        assert len(occurrence_runs) == len(set(occurrence_runs)), ('occurrences share a run', occurrence_runs)
        assert sorted(occurrence_runs + [rid]) == sorted(after_ids), (
            'runs not explained by manual run plus due occurrences', occurrence_runs, rid, after_ids)
        assert len(after_ids) == 1 + len(occurrence_runs)
        # Paused state survives the restart and no tick fired while it was down.
        paused_after = cli('schedules', 'show', schedule, '--json')
        assert paused_after['status'] == 'PAUSED', ('schedule resumed by restart', paused_after['status'])
        assert paused_after['next_run'] is None, paused_after['next_run']
        # External effects counted from Forge's own run records (summary count plus full inspection).
        effect_counts = {row['run_id']: row['effect_count'] for row in after}
        for row_id in after_ids:
            detail = cli('runs', 'inspect', row_id)
            assert not detail.get('effects'), ('external effects recorded', row_id, detail.get('effects'))
            assert effect_counts[row_id] == len(detail.get('effects') or []), ('effect count mismatch', row_id)
        effects_total = sum(effect_counts.values())
        assert effects_total == 0, ('external effects observed', effect_counts)
        save('final-snapshot.json', cli('snapshot'))
        result.update(passed=True, manual_run=rid, due_run=due['run_id'],
                      restart_no_duplicate={'run_ids': after_ids, 'unique': len(set(after_ids)),
                                            'before_count': len(before_ids), 'after_count': len(after_ids),
                                            'due_occurrences': len(occurrence_runs)},
                      schedule_paused={'after_pause': paused['status'], 'after_restart': paused_after['status']},
                      mcp_readback=mcp_compared,
                      browser_readback=os.environ.get('FORGE_BROWSER') == '1',
                      external_effects=effects_total, run_count=len(before))
    finally:
        # A failed gate is never retried; close only this service and pause our schedule.
        if schedule and proc is not None and proc.poll() is None:
            try:
                cli('schedules', 'pause', schedule)
            except Exception:
                result['schedule_pause_failed'] = True
        stop()
        log.close()
        save('commands.json', commands)
        result['service_stopped'] = proc is None or proc.poll() is not None
        save('result.json', result)
        if result['passed']:
            print(json.dumps(result))


if __name__ == '__main__':
    main()

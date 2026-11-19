"""Manual + actual due native-harness chain against an isolated running Forge.

Requires FORGE_BINARY, FORGE_URL, FORGE_TOKEN, FORGE_RUNNER_PROFILE,
FORGE_CANDIDATE_SHA and a fresh FORGE_TEST_STATE directory. Operator config must
already authorize the selected workspace-write native harness and its model.
Uses only Python's standard library. Does not provision auth or change profiles.
"""
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

state = Path(os.environ['FORGE_TEST_STATE']).resolve()
state.mkdir(parents=True, exist_ok=False)
binary = Path(os.environ['FORGE_BINARY']).resolve(strict=True)
profile = os.environ['FORGE_RUNNER_PROFILE']
work = 'qualify-harness-' + uuid.uuid4().hex[:12]
evidence = {
    'source_commit': os.environ['FORGE_CANDIDATE_SHA'],
    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
    'profile': profile,
    'work_id': work,
    'cases': {},
}
schedule = None


def cli(*args, data=None):
    result = subprocess.run(
        [str(binary), '--url', os.environ['FORGE_URL'], *args],
        input=json.dumps(data) if data is not None else None,
        capture_output=True, text=True, timeout=120,
    )
    if result.returncode:
        raise RuntimeError(f'Forge command failed ({result.returncode}): {args[:2]}')
    return json.loads(result.stdout)


def wait(run):
    deadline = time.monotonic() + 420
    while time.monotonic() < deadline:
        detail = cli('runs', 'inspect', run)
        if detail['status'] not in ('ADMITTED', 'ENQUEUED', 'PENDING'):
            assert detail['status'] == 'SUCCESS', detail['status']
            return detail
        time.sleep(1)
    raise TimeoutError('harness run did not settle within 420 seconds')


def verify(run):
    detail = wait(run)
    output = detail['output']['output']
    assert output['seed'] == output['file_text'] == 'SEED-' + work
    assert output['dependent'] == 'Consumed ' + output['seed']
    effect = [e for e in detail['effects'] if e['action']['type'] == 'harness'][0]
    # Overview responses deliberately summarize large receipts. Qualification
    # reads the canonical exact effect, not the bounded overview preview.
    receipt = cli('runs', 'inspect', run, '--effect-id', effect['effect_id'])['effect']['output']
    assert effect['state'] == 'completed'
    artifact = next(a for a in receipt['artifacts'] if a['path'] == 'evidence.txt')
    assert artifact['text'] == output['seed']
    assert artifact['sha256'] == hashlib.sha256(output['seed'].encode()).hexdigest()
    assert receipt['harness']['tool_events'] is None or receipt['harness']['tool_events'] > 0
    assert len([l for l in detail['logs'] if l['action'] == 'harness_launched']) == 1
    return {'run_id': run, 'status': detail['status'], 'effect_id': effect['effect_id'],
            'harness': receipt['harness'], 'artifact': artifact, 'output': output}


definition = {'type': 'workflow', 'input': {'steps': [
    {'id': 'seed', 'action': {'type': 'echo', 'input': {'text': 'SEED-' + work}}},
    {'id': 'work', 'needs': ['seed'], 'action': {'type': 'harness', 'input': {
        'profile': profile, 'permissions': 'workspace-write',
        'require': ['structured_output', 'tool_use'],
        'prompt': 'Read seed from inputs.json using your file tool. Write evidence.txt '
                  'with exactly that seed, no newline. Use your tools. Then return '
                  'the JSON object with file evidence.txt and the seed you wrote.',
        'inputs': {'seed': {'$ref': '/steps/seed/output/text'}},
        'output_schema': {'type': 'object', 'properties': {
            'file': {'const': 'evidence.txt'}, 'seed': {'const': 'SEED-' + work}},
            'required': ['file', 'seed'], 'additionalProperties': False},
    }}},
    {'id': 'after', 'needs': ['work'], 'action': {'type': 'echo', 'input': {
        'text': 'Consumed ${/steps/work/output/result/seed}'}}},
], 'output': {
    'seed': {'$ref': '/steps/work/output/result/seed'},
    'file_text': {'$ref': '/steps/work/output/artifacts/0/text'},
    'dependent': {'$ref': '/steps/after/output/text'},
}}}
try:
    cli('work', 'register', work, '--definition', '-', data=definition)
    key = work + '-manual'
    run = cli('work', 'run', work, '--key', key)['run_id']
    evidence['cases']['manual'] = verify(run)
    assert cli('work', 'run', work, '--key', key)['run_id'] == run
    schedules = cli('schedules', 'create', work, '--work-id', work,
                    '--cron', '* * * * *', '--tz', 'UTC')
    entry = next(s for s in schedules if s['schedule_name'].endswith('.' + work))
    schedule = entry['schedule_name']
    due = datetime.datetime.fromisoformat(entry['next_run']).timestamp()
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        runs = [r for r in cli('runs', 'list') if r['work_id'] == work and r['run_id'] != run]
        if runs:
            cli('schedules', 'pause', schedule)
            fired = runs[0]
            created = fired['created'] / (1000 if fired['created'] > 1e11 else 1)
            assert created >= due - 2
            evidence['cases']['scheduled'] = verify(fired['run_id'])
            evidence['cases']['scheduled'].update(schedule=schedule, due=entry['next_run'],
                                                   fired_minus_due_seconds=created - due)
            break
        time.sleep(1)
    else:
        raise TimeoutError('actual due occurrence not observed')
    evidence['status'] = 'PASS'
except BaseException as error:
    evidence['status'] = 'FAIL'
    evidence['error'] = str(error)
    raise
finally:
    if schedule:
        cli('schedules', 'pause', schedule)
    (state / 'evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
print(json.dumps({'status': evidence['status'], 'evidence': str(state / 'evidence.json')}))

#!/usr/bin/env python3
"""Read-only check of the retained Home/Forge agent-authoring rejection.

This checks negative evidence only. It never invokes a model, service, timer,
registration, schedule, replay, or database. A passing check is NOT qualification
of an agent-authored actual-due workflow. Requires Python 3.11+.
"""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib

from qualify_home_native_forge import load, paired, require, rows, sha


def check(directory):
    base = Path(directory)
    events = rows(base / 'controller.jsonl')
    require(events and events[0]['type'] == 'session_started', 'no native session')
    session = events[0]['session_id']
    require(len({e['session_id'] for e in events}) == 1, 'multiple sessions')
    run_ids = {e['run_id'] for e in events if e.get('run_id')}
    require(len(run_ids) == 1, 'multiple native runs')
    require(events[-1]['type'] == 'run_finished' and events[-1]['status'] == 'cancelled',
            'missing retained controller cancellation')
    require([e['seq'] for e in events] == list(range(1, len(events) + 1)), 'event sequence gap')
    calls = paired(events, complete=False)
    require(len(calls) == sum(e['type'] == 'tool_proposed' for e in events), 'unresolved tool calls')
    registrations = [c for c in calls if c['name'] == 'mcp__forge__forge_register']
    require(len(registrations) == 3, 'expected initial rejection and two failed repairs')
    for c in registrations:
        require(c['status'] == 'error' and 'work input does not match supported schema' in c['output'],
                'registration was not definitively rejected')
        require(c['arguments']['definition']['input'].get('type') == 'workflow',
                'retained invalid workflow input differs')
    names = {c['name'] for c in calls if c['name'].startswith('mcp__forge__')}
    require(names == {'mcp__forge__forge_snapshot', 'mcp__forge__forge_runners',
                      'mcp__forge__forge_register'}, 'unexpected Forge operation')
    require(any(c['name'] == 'skill_activate' and c['status'] == 'ok' for c in calls),
            'no actual skill activation')
    discovered = json.loads(next(c['output'] for c in calls if c['name'] == 'mcp__forge__forge_snapshot'))
    schema = discovered['types']['workflow']['schema']
    require(schema.get('additionalProperties') is False and 'type' not in schema['properties'],
            'discovered schema does not explain rejection')
    for name in ('snapshot-before.json', 'snapshot-terminal.json'):
        snapshot = load(base / name)
        require(not snapshot['work'] and not snapshot['schedules'] and not snapshot['runs'],
                'state is not empty: ' + name)
        require(snapshot['health']['plugins'] == [], 'plugins were configured')
    counts = load(base / 'terminal-readonly-counts.json')
    require(counts == dict.fromkeys(('dbos_invocations', 'forge_work', 'forge_schedules',
                                   'forge_flow_effects'), 0), 'ledger contains admitted work')
    home = base / 'synthetic-schedule-controller'
    prep = load(base / 'preparation.json')
    for name, digest in prep['canonical_and_projection'].items():
        require(sha(home / name) == digest, 'prepared file drift: ' + name)
    config = tomllib.loads((home / 'soulsteads-harness.toml').read_text())
    server = config['mcp_servers'][0]
    require(server['env_pass'] == ['FORGE_TOKEN'] and server['default_approval'] == 'deny',
            'MCP credential/grant boundary changed')
    require(server['tools'] == {'forge_' + n: 'allow' for n in
            ('snapshot', 'runners', 'work', 'register', 'schedule')}, 'controller grant drift')
    journals = list(home.glob('.runtime/soulsteads-harness/agents/*/sessions/*.jsonl'))
    require(len(journals) == 1 and journals[0].stem == session, 'journal/session mismatch')
    journal = rows(journals[0])
    require(journal[0]['model'] == events[0]['model'] == config['provider']['model'],
            'configured/observed model mismatch')
    require(journal[-1]['kind'] == 'run_finished' and journal[-1]['status'] == 'cancelled',
            'journal lacks terminal cancellation')
    for name, digest in journal[0]['instructions']:
        require(name.startswith('file:') and sha(home / name[5:]) == digest, 'instruction digest mismatch')
    attempts = {x['attempt_id']: x for x in journal if x['kind'] == 'tool_attempt'}
    results = {x['attempt_id']: x for x in journal if x['kind'] == 'tool_result'}
    for c in registrations:
        p, r = attempts[c['attempt_id']], results[c['attempt_id']]
        require(p['name'] == c['name'] and json.loads(p['arguments']) == c['arguments'] and
                r['status'] == c['status'] and r['content'] == c['output'] and
                p['call_id'] == r['call_id'] and p['run_id'] == r['run_id'] == c['run_id'],
                'native journal/public request-result correlation failed')
    command = load(base / 'controller.command.json')
    preview = load(base / 'launch-preview.stdout')
    require(command['status'] == 'finished' and command['exit_code'] != 0, 'controller process did not exit')
    require(command['argv'][1:3] == ['run', str(home)] and command['argv'][3:] ==
            ['--', 'run', '--format', 'jsonl', '-'], 'controller was not launched through Home')
    for key, path in (('home_cli', command['argv'][0]), ('native_bin', preview['argv'][0]),
                      ('forge_bin', server['command'])):
        require(sha(path) == prep['binary_sha256'][key], 'installed executable changed')
    return {'status': 'NEGATIVE_EVIDENCE_VERIFIED', 'adoption_gate': 'UNQUALIFIED',
            'live_execution_this_check': False, 'controller_session': session,
            'controller_run': next(iter(run_ids)), 'controller_terminal': 'cancelled',
            'model': events[0]['model'], 'model_response_events': sum(e['type'] == 'model_response' for e in events),
            'registration_rejections': len(registrations), 'failed_repairs': len(registrations) - 1,
            'controller_journal_sha256': sha(journals[0]), 'controller_events_sha256': sha(base / 'controller.jsonl'),
            'registration_receipts': [{'arguments_sha256': hashlib.sha256(json.dumps(c['arguments'],
                sort_keys=True, separators=(',', ':')).encode()).hexdigest(), 'attempt_id': c['attempt_id'],
                'status': c['status'], 'output': c['output']} for c in registrations],
            'immutable_work_version': None, 'schedule_id': None, 'actual_due_occurrence': None,
            'scheduled_child_invocations': 0, 'typed_output': None, 'dependent_output': None,
            'limits': ['Read-only checks of retained evidence, not a new live attempt.',
                       'No definition or schedule accepted; no due/native-child qualification.',
                       'Controller stopped after ignoring stop-on-error; two failed repairs retained.',
                       'Host service/process closure remains the lifecycle owner\'s separate receipt.']}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('evidence', type=Path)
    args = parser.parse_args()
    print(json.dumps(check(args.evidence.resolve()), indent=2))

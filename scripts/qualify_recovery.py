#!/usr/bin/env python3
"""Offline backup/restore and original-key replay of retained native receipts.

Python 3.11+, Linux; matching psql/pg_dump/pg_restore on PATH. Required env:
FORGE_BINARY, FORGE_TEST_HOME, FORGE_TEST_STATE (nonexistent), FORGE_DATABASE_URL,
FORGE_RESTORE_DATABASE_URL (fresh empty DB), FORGE_RESTORE_PORT,
FORGE_PRESERVATION, FORGE_HOST_BACKUP_READY. FORGE_ACTOR defaults to operator.
The host handoff must assert source_home, source_port, source_stopped=true,
source_stays_stopped=true. The operator owns source shutdown/restart; this driver
owns only the restored process group. No admission, model, retry or reconciliation
is needed: each retained key is replayed once, and failures are never retried.
See docs/recovery.md for the evidence manifest and limitations.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import stat
import subprocess
import time
from urllib.parse import parse_qsl, unquote, urlsplit


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def load(path):
    return json.loads(Path(path).read_text())


def sha(path):
    with Path(path).open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def save(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def closed(port):
    with socket.socket() as connection:
        connection.settimeout(1)
        return connection.connect_ex(('127.0.0.1', port)) != 0


def pg_env(url):
    """Explicit libpq fields, never a URL/password in argv or evidence."""
    parsed = urlsplit(url)
    require(parsed.scheme in ('postgres', 'postgresql') and parsed.hostname
            and parsed.port and parsed.username and parsed.path.strip('/'),
            'explicit PostgreSQL host/port/user/database required')
    env = {k: v for k, v in os.environ.items() if not k.startswith('PG')}
    fields = dict(PGHOST=parsed.hostname, PGPORT=str(parsed.port),
                  PGUSER=unquote(parsed.username), PGDATABASE=unquote(parsed.path[1:]))
    if parsed.password is not None:
        fields['PGPASSWORD'] = unquote(parsed.password)
    allowed = {'sslmode': 'PGSSLMODE', 'host': 'PGHOST', 'port': 'PGPORT',
               'user': 'PGUSER', 'dbname': 'PGDATABASE', 'password': 'PGPASSWORD'}
    pairs = parse_qsl(parsed.query, keep_blank_values=True)
    require(len(dict(pairs)) == len(pairs), 'duplicate PostgreSQL option')
    for key, value in pairs:
        require(key in allowed, 'qualification supports only explicit basic libpq options')
        fields[allowed[key]] = value
    env.update(fields, PGPASSFILE='/dev/null', PGSERVICEFILE='/dev/null',
               PGCONNECT_TIMEOUT='10', PGAPPNAME='forge-recovery-qualification',
               PGOPTIONS='-c default_transaction_read_only=on')
    return env


def query(url, sql):
    result = subprocess.run(['psql', '-X', '--no-password', '-At', '-v',
                             'ON_ERROR_STOP=1', '-c', sql], env=pg_env(url),
                            capture_output=True, text=True, timeout=30)
    require(result.returncode == 0, 'read-only PostgreSQL inspection failed')
    return result.stdout.strip()


def files(home, case):
    base = home / 'state/harness' / case['invocation_id']
    actual = {}
    for path in base.rglob('*'):
        mode = path.lstat().st_mode
        require(not stat.S_ISLNK(mode), 'unexpected invocation symlink')
        if stat.S_ISREG(mode):
            actual[str(path.relative_to(home))] = {
                'sha256': sha(path), 'bytes': path.stat().st_size}
        else:
            require(stat.S_ISDIR(mode), 'unexpected invocation special file')
    expected = {f['home_relative']: {k: f[k] for k in ('sha256', 'bytes')}
                for f in case['files']}
    require(actual == expected, 'invocation file set/bytes changed: ' + case['case'])
    return actual


def verify(run, effect, case, original_run, original_effect):
    require(run['run_id'] == case['run_id'] and run['status'] == case['run_status'],
            'run identity/status changed')
    require(effect == original_effect, 'exact effect/attempt/output changed')
    item = effect['effect']
    attempt = item['harness_attempt']
    require(item['state'] == case['effect_state']
            and attempt['invocation_id'] == case['invocation_id']
            and attempt['attempt'] == case['attempt_count']
            and attempt['state'] == case['attempt_state']
            and attempt['result'] == case['attempt_result'], 'native attempt changed')
    require([s['function_name'] for s in run['steps']] == case['step_names'],
            'native steps changed (including dependent presence/absence)')
    # The overview is bounded, but exact effect receipts and all retained file
    # bytes are checked separately. Only original-key audit entries may differ.
    for key in original_run.keys() - {'logs'}:
        require(run.get(key) == original_run[key], 'run field changed: ' + key)
    logs = [row for row in run['logs'] if row['action'] != 'run']
    require(logs == [row for row in original_run['logs'] if row['action'] != 'run'],
            'execution log changed; possible native relaunch')
    require(sum(row['action'] == 'harness_launched' for row in run['logs'])
            == case['launch_log_count'], 'native launch count changed')


def main():
    os.umask(0o077)
    state = Path(os.environ['FORGE_TEST_STATE']).resolve()
    state.mkdir(parents=True, exist_ok=False)
    evidence = {'status': 'RUNNING', 'cases': {}, 'commands': [],
                'limits': ['Only the retained cases, no new workload or model call',
                           'Home controller state outside Forge home is not archived',
                           'Source lifecycle and build provenance are operator supplied']}
    process = None
    source_lock = None
    try:
        binary = Path(os.environ['FORGE_BINARY']).resolve(strict=True)
        home = Path(os.environ['FORGE_TEST_HOME']).resolve(strict=True)
        source_url = os.environ['FORGE_DATABASE_URL']
        target_url = os.environ['FORGE_RESTORE_DATABASE_URL']
        port = int(os.environ['FORGE_RESTORE_PORT'])
        baseline = load(os.environ['FORGE_PRESERVATION'])
        handoff = load(os.environ['FORGE_HOST_BACKUP_READY'])
        # Host handoffs may also contain connection identities. Do not copy
        # those (or arbitrary host fields) into the qualification receipt.
        save(state / 'host-ready.json', {key: handoff[key] for key in
             ('source_home', 'source_port', 'source_stopped', 'source_stays_stopped')})
        save(state / 'preservation.json', baseline)
        require(handoff['source_stopped'] is True and handoff['source_stays_stopped'] is True
                and Path(handoff['source_home']).resolve() == home
                and Path(baseline['source_home']).resolve() == home,
                'stopped-source lifecycle handoff missing or mismatched')
        source_port = int(handoff['source_port'])
        require(port != source_port and closed(source_port) and closed(port),
                'source or destination listener is active')
        require(sha(binary) == baseline['installed_forge_sha256'], 'installed Forge changed')
        require(sha(home / 'state/dbos-identity.json') == baseline['source_identity_sha256'],
                'source identity changed')
        evidence.update(binary=str(binary), binary_sha256=sha(binary),
                        source_candidate=baseline['source_candidate'], source_home=str(home),
                        driver_sha256=sha(__file__), source_port=source_port, restored_port=port)
        originals = {}
        for case in baseline['cases']:
            require(case['run_status'] in ('SUCCESS', 'RECOVERY_REQUIRED'),
                    'only completed/uncertain terminal cases are supported')
            for kind in ('run', 'effect'):
                require(sha(case[kind + '_receipt']) == case[kind + '_receipt_sha256'],
                        'retained receipt hash changed')
            originals[case['case']] = (load(case['run_receipt']), load(case['effect_receipt']))
            verify(*originals[case['case']], case, *originals[case['case']])
            files(home, case)

        source_identity = load(home / 'state/dbos-identity.json')
        policy = load(home / 'config.json')
        native_paths = {value['executable'] for value in policy['runners'].values()
                        if value['adapter'] == 'soulsteads'}
        require(native_paths, 'no retained native runner executable')
        evidence['native_executables'] = {path: sha(path) for path in native_paths}
        require(all(digest == baseline['installed_native_sha256'] for digest in
                    evidence['native_executables'].values()), 'installed native binary changed')
        schema = source_identity['schema']
        require(re.fullmatch(r'[a-zA-Z_][a-zA-Z_0-9]*', schema), 'unsupported schema identifier')

        def offline(url, label):
            schedules = json.loads(query(url, f'SELECT COALESCE(json_agg(t),\'[]\') FROM '
                f'(SELECT * FROM "{schema}".workflow_schedules ORDER BY schedule_name) t'))
            save(state / (label + '-schedules.json'), schedules)
            internal = 'forge-v1-' + source_identity['home_id'] + '-submission-maintenance'
            user = [s for s in schedules if s['schedule_name'] != internal]
            require(all(s['status'] == 'PAUSED' for s in user), 'user schedule is not paused')
            require({s['schedule_name'] for s in user} ==
                    {s['schedule_name'] for s in baseline['schedules_at_retained_snapshot']},
                    'user schedule set changed')
            pending = json.loads(query(url, f'SELECT COALESCE(json_agg(t),\'[]\') FROM '
                f'(SELECT workflow_uuid,name,status FROM "{schema}".workflow_status '
                "WHERE status IN ('PENDING','ENQUEUED') "
                "AND name <> 'forge_v1_submission_maintenance') t"))
            save(state / (label + '-pending.json'), pending)
            require(not pending, 'unfinished user work could dispatch at restored startup')

        offline(source_url, 'source-stopped')
        archive, restored = state / 'archive', state / 'restored'

        def command(label, args, env, data=None):
            evidence['commands'].append({'label': label, 'args': args})
            result = subprocess.run([str(binary), *args], env=env, input=data,
                                    capture_output=True, text=True, timeout=360)
            (state / (label + '.stdout')).write_text(result.stdout)
            (state / (label + '.stderr')).write_text(result.stderr)
            require(result.returncode == 0, 'public command failed: ' + label)
            return result.stdout

        command('backup', ['--home', str(home), 'backup', str(archive)], os.environ.copy())
        # Backup itself owns this lock. Retain it after backup until restored
        # closure so an accidental source restart cannot create two executors.
        source_lock = (home / 'state/owner.lock').open('r+')
        fcntl.flock(source_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(closed(source_port), 'source restarted during backup handoff')
        manifest = load(archive / 'manifest.json')
        evidence['archive'] = {'path': str(archive), 'manifest_sha256': sha(archive / 'manifest.json'),
                               'dump_sha256': sha(archive / 'database.dump'),
                               'postgres_major': manifest['postgres_major'],
                               'files': manifest['files']}
        for case in baseline['cases']:
            files(archive / 'home', case)
        env = {**os.environ, 'FORGE_DATABASE_URL': target_url}
        command('restore', ['--home', str(restored), 'restore', str(archive)], env)
        evidence['restored_home'] = str(restored)
        evidence['restored_identity'] = load(restored / 'state/dbos-identity.json')
        offline(target_url, 'restored-before-start')
        for case in baseline['cases']:
            files(restored, case)
        require(closed(source_port) and closed(port), 'listener changed before restored start')
        actor = os.environ.get('FORGE_ACTOR', 'operator')
        env['FORGE_TOKEN'] = load(restored / 'config.json')['actors'][actor]['token']
        env.pop('FORGE_TOKEN_ENV', None)
        url = 'http://127.0.0.1:' + str(port)
        with (state / 'serve.stdout').open('w') as out, (state / 'serve.stderr').open('w') as err:
            process = subprocess.Popen([str(binary), '--home', str(restored), 'serve',
                                        '--port', str(port)], env=env, stdout=out, stderr=err,
                                       start_new_session=True)
        evidence['service_pid'] = process.pid
        save(state / 'running.json', {'pid': process.pid, 'port': port, 'home': str(restored)})
        # Bounded startup only, not a background monitoring loop.
        deadline = time.monotonic() + 20
        while closed(port) and process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.1)
        require(process.poll() is None and not closed(port), 'restored service failed to start')

        def cli(label, *args, data=None):
            return json.loads(command(label, ['--url', url, *args], env, data))

        schedules = cli('restored-public-schedules', 'schedules', 'list')
        require(all(s['status'] == 'PAUSED' for s in schedules), 'public schedule became active')
        for case in baseline['cases']:
            name, run_id = case['case'], case['run_id']
            original_run, original_effect = originals[name]
            for phase in ('before', 'after'):
                run = cli(name + '-' + phase + '-run', 'runs', 'inspect', run_id)
                effect = cli(name + '-' + phase + '-effect', 'runs', 'inspect', run_id,
                             '--effect-id', case['effect_id'])
                verify(run, effect, case, original_run, original_effect)
                inventory = files(restored, case)
                if phase == 'before':
                    require(closed(source_port), 'source executor restarted')
                    replay = case['restored_public_replay']
                    require(replay['name'] == 'forge_run' and replay['arguments'] == {
                        'work_id': case['work_id'], 'key': case['key'], 'params': {}},
                        'replay request differs from retained stable key')
                    result = cli(name + '-replay', 'work', 'run', case['work_id'],
                                 '--key', case['key'], '--params', '-', data='{}')
                    require(result['run_id'] == run_id == case['expected_replay_run_id'],
                            'replay returned a new run')
            evidence['cases'][name] = {'run_id': run_id, 'status': run['status'],
                'effect_state': effect['effect']['state'], 'invocation_id': case['invocation_id'],
                'same_key_replay_once': True, 'files': inventory}
        # Catch a change to either case while inspecting the other.
        for case in baseline['cases']:
            name = case['case']
            verify(cli(name + '-final-run', 'runs', 'inspect', case['run_id']),
                   cli(name + '-final-effect', 'runs', 'inspect', case['run_id'],
                       '--effect-id', case['effect_id']), case, *originals[name])
            files(restored, case)
        require(closed(source_port), 'source executor restarted')
        evidence['status'] = 'PASS'
    except BaseException as error:
        evidence['status'] = 'FAIL'
        evidence['error'] = str(error)
        raise
    finally:
        if process is not None:
            # Only our new session/group; never signal source or provider PIDs.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
            try:
                os.killpg(process.pid, 0)
                group_gone = False
            except ProcessLookupError:
                group_gone = True
            evidence['closure'] = {'exit_code': process.returncode,
                                   'process_group_gone': group_gone, 'listener_closed': closed(port)}
            if not group_gone or not closed(port):
                evidence['status'] = 'FAIL'
                evidence['closure']['requires_operator_inspection'] = True
        if source_lock is not None:
            source_lock.close()
        save(state / 'evidence.json', evidence)
    require(evidence['status'] == 'PASS', 'recovery or closure failed')
    print(json.dumps({'status': 'PASS', 'evidence': str(state / 'evidence.json')}))


if __name__ == '__main__':
    main()

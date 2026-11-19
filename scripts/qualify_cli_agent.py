"""Actual native Codex loads installed Forge skill, uses CLI via shell, observes due output.
Requires live FORGE_URL, FORGE_TEST_HOME, FORGE_BINARY, new FORGE_TEST_STATE.
Native Codex auth/config are read-only; all changes are per invocation or isolated workspace.
"""
from pathlib import Path
import os,json,subprocess,time,signal,hashlib,uuid,tomllib
import httpx

state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True);assert binary.read_bytes()[:4]==b'\x7fELF'
home=Path(os.environ['FORGE_TEST_HOME']).resolve();url=os.environ['FORGE_URL']
cfg=json.loads((home/'config.json').read_text());token=cfg['actors']['operator']['token']
env={**os.environ,'FORGE_TOKEN':token,'FORGE_URL':url,'FORGE_BINARY':str(binary)}
project=state/'project';skill=project/'.agents/skills/forge-operations/SKILL.md';skill.parent.mkdir(parents=True)
skill.write_text(subprocess.check_output([str(binary),'skill'],text=True))
codex_dir=Path(os.environ['CODEX_HOME']) if os.environ.get('CODEX_HOME') else (Path.home()/'.codex' if os.environ.get('FORGE_ALLOW_USER_CODEX_CONFIG')=='1' else None)
if codex_dir is None:raise SystemExit('set CODEX_HOME to an isolated Codex home (preferred) or FORGE_ALLOW_USER_CODEX_CONFIG=1 to opt in to the real ~/.codex')
exe=os.environ.get('CODEX_EXECUTABLE','codex');usercfg=tomllib.loads((codex_dir/'config.toml').read_text())
args=[exe,'exec','--ephemeral','--skip-git-repo-check','--sandbox','workspace-write','--cd',str(project),'--json','-c','approval_policy="never"','-c','sandbox_workspace_write.network_access=true','-c','shell_environment_policy.inherit="all"','-c','shell_environment_policy.ignore_default_excludes=true']
for name in usercfg.get('mcp_servers',{}):args.extend(['-c',f'mcp_servers.{name}.enabled=false'])
suffix=uuid.uuid4().hex[:8];work='cli-agent-'+suffix;schedule='cli-agent-due-'+suffix;marker='FORGE_CLI_AGENT_'+suffix
prompt=f'''Use the installed forge-operations skill. Actually read .agents/skills/forge-operations/SKILL.md using the shell. Use only the real Forge CLI through direct shell commands to operate Forge, with executable {binary}. The service URL is available in FORGE_URL and the operator credential in FORGE_TOKEN; use --url "$FORGE_URL" and --token-env FORGE_TOKEN. Never print credentials or inspect config/databases. All MCP servers are disabled for this CLI proof. Inspect CLI help when needed. Inspect current state, register immutable echo work {work} with text {marker}, create schedule {schedule} every 2 seconds UTC, wait until an actual scheduled run succeeds, inspect that due run and its actual output using the CLI, then pause the schedule and read back PAUSED. Do not manually run echo work: the observed output must come from scheduled execution. Do not use HTTP, curl, a client adapter, or other services. Use direct CLI commands and ordinary shell JSON/files as needed. Limit to 20 shell calls. Report actual run ID, observed output and schedule state.'''
(state/'prompt.txt').write_text(prompt);args.append(prompt)
started=time.monotonic();proc=subprocess.Popen(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
try:out,err=proc.communicate(timeout=300)
except subprocess.TimeoutExpired:
 os.killpg(proc.pid,signal.SIGTERM)
 try:out,err=proc.communicate(timeout=15)
 except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);out,err=proc.communicate()
assert token not in out+err,'credential unexpectedly exposed in harness trace'
(state/'codex.jsonl').write_text(out);(state/'codex.stderr').write_text(err)
summary={'exit':proc.returncode,'duration_s':time.monotonic()-started,'version':subprocess.check_output([exe,'--version'],text=True).strip(),'configured_model':usercfg.get('model'),'configured_provider':usercfg.get('model_provider'),'actual_served_model':'not independently exposed by native CLI','binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'work_id':work,'schedule':schedule,'marker':marker,'skill_source':'actual installed binary skill output','all_mcp_disabled':True}
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
assert proc.returncode==0,summary
events=[json.loads(l) for l in out.splitlines() if l.strip()]
items=[e.get('item',{}) for e in events if e.get('type')=='item.completed']
commands=[i for i in items if i.get('type')=='command_execution' and i.get('exit_code')==0]
assert any('SKILL.md' in i.get('command','') for i in commands),'skill not read'
assert not any(i.get('type')=='mcp_tool_call' for i in items),'agent used MCP in CLI proof'
executed='\n'.join(i.get('command','') for i in commands)
assert str(binary) in executed or 'FORGE_BINARY' in executed,'no actual binary invocation'
for tokens in [('work','register',work),('schedules','create',schedule),('runs','inspect'),('schedules','pause')]:
 assert any(all(t in i.get('command','') for t in tokens) for i in commands),tokens
assert any('runs' in i.get('command','') and 'inspect' in i.get('command','') and marker in i.get('aggregated_output','') and 'SUCCESS' in i.get('aggregated_output','') for i in commands),'no actual successful inspect output'
assert not any('work run '+work in i.get('command','') for i in commands),'manual run substituted for scheduled output'
snapshot=httpx.post(url+'/api',headers={'Authorization':'Bearer '+token},json={'method':'snapshot'},timeout=15).json()['result']
runs=[httpx.post(url+'/api',headers={'Authorization':'Bearer '+token},json={'method':'inspect','params':{'run_id':r['run_id']}},timeout=15).json()['result'] for r in snapshot['runs'] if r['work_id']==work and r['status']=='SUCCESS']
runs=[r for r in runs if marker in json.dumps(r['output'])]
schedules=[s for s in snapshot['schedules'] if s['schedule_name']=='operator.'+schedule]
assert runs and len(schedules)==1 and schedules[0]['status']=='PAUSED'
summary.update({'passed':True,'actual_skill_read':True,'actual_shell_cli':True,'successful_due_runs':[r['run_id'] for r in runs],'canonical_schedule':schedules[0],'successful_shell_calls':len(commands),'actual_schedule_create_pause_and_inspect_verified':True})
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');(state/'snapshot.json').write_text(json.dumps(snapshot,indent=2)+'\n');(state/'inspected-runs.json').write_text(json.dumps(runs,indent=2)+'\n');print(json.dumps(summary,indent=2))

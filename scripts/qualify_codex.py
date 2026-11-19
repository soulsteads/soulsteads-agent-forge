"""Actual native Codex + native Forge MCP proof; no Python Forge components."""
from pathlib import Path
import os,json,subprocess,shutil,time,signal,httpx,tomllib,uuid,hashlib

state=Path(os.environ['FORGE_TEST_STATE']).resolve()
state.mkdir(parents=True,exist_ok=False)
home=Path(os.environ['FORGE_TEST_HOME']).resolve()
binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True)
assert binary.read_bytes()[:4]==b'\x7fELF'
workspace=state/'project';workspace.mkdir()
skill=workspace/'.agents/skills/forge-operations';skill.parent.mkdir(parents=True)
shutil.copytree(Path(os.environ.get('FORGE_SOURCE_SKILL',str(Path(__file__).resolve().parents[1]/'skills/forge-operations'))).resolve(),skill)
config=json.loads((home/'config.json').read_text())
env={**os.environ,'FORGE_TOKEN':config['actors']['operator']['token']}
headers={'Authorization':'Bearer '+env['FORGE_TOKEN']};url=os.environ.get('FORGE_URL','http://127.0.0.1:19100')
assert httpx.post(url+'/api',headers=headers,json={'method':'snapshot'},timeout=15).status_code==200
exe=os.environ.get('CODEX_EXECUTABLE','codex')
args=[exe,'exec','--ephemeral','--skip-git-repo-check','--sandbox','workspace-write','--cd',str(workspace),'--json','-c','approval_policy="never"','-c','mcp_servers.forge.command='+json.dumps(str(binary)),'-c','mcp_servers.forge.args='+json.dumps(['mcp','--url',url,'--token-env','FORGE_TOKEN']),'-c','mcp_servers.forge.env_vars=["FORGE_TOKEN"]']
codex_dir=Path(os.environ['CODEX_HOME']) if os.environ.get('CODEX_HOME') else (Path.home()/'.codex' if os.environ.get('FORGE_ALLOW_USER_CODEX_CONFIG')=='1' else None)
if codex_dir is None:raise SystemExit('set CODEX_HOME to an isolated Codex home (preferred) or FORGE_ALLOW_USER_CODEX_CONFIG=1 to opt in to the real ~/.codex')
user_cfg=tomllib.loads((codex_dir/'config.toml').read_text())
for name in user_cfg.get('mcp_servers',{}):args.extend(['-c',f'mcp_servers.{name}.enabled=false'])
args.extend(['-c','mcp_servers.forge.enabled=true','-c','mcp_servers.forge.required=true','-c','mcp_servers.forge.default_tools_approval_mode="approve"'])
suffix=uuid.uuid4().hex[:8];work='rust-codex-'+suffix;failure='rust-failure-'+suffix;schedule='rust-due-'+suffix;marker='RUST_CODEX_'+suffix
prompt=f'Use the installed forge-operations skill. Actually read .agents/skills/forge-operations/SKILL.md with your shell tool. Through Forge MCP tools: inspect state, register echo work {work} with text {marker}, schedule it as {schedule} every 2 seconds UTC, observe at least one completed due run and inspect its actual output, then pause that schedule. Register fail work {failure}, run it once with key failure, inspect until ERROR and explain the controlled failure. Do not register model/plugin work or use other services. Use at most 20 tool calls. Report actual run IDs and observed outputs, not inferred success.'
(state/'prompt.txt').write_text(prompt);args.append(prompt)
t0=time.monotonic();proc=subprocess.Popen(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
try:out,err=proc.communicate(timeout=300)
except subprocess.TimeoutExpired:
 os.killpg(proc.pid,signal.SIGTERM)
 try:out,err=proc.communicate(timeout=15)
 except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);out,err=proc.communicate()
(state/'codex.jsonl').write_text(out);(state/'codex.stderr').write_text(err)
summary={'exit':proc.returncode,'duration_s':time.monotonic()-t0,'version':subprocess.check_output([exe,'--version'],text=True).strip(),'configured_model':user_cfg.get('model'),'configured_provider':user_cfg.get('model_provider'),'actual_served_model':'not independently exposed by nativeCLI','binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'work_id':work,'failure_work_id':failure,'schedule':schedule,'marker':marker}
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
assert proc.returncode==0,summary
events=[json.loads(line) for line in out.splitlines() if line.strip()]
items=[e.get('item',{}) for e in events]
assert any(i.get('type')=='command_execution' and 'SKILL.md' in i.get('command','') and i.get('exit_code')==0 for i in items),'No actual skill read event'
assert any(i.get('type')=='mcp_tool_call' and i.get('status')=='completed' for i in items),'No completed nativeMCP tool event'
snapshot=httpx.post(url+'/api',headers=headers,json={'method':'snapshot'},timeout=15).json()['result']
succeeded=[r for r in snapshot['runs'] if r.get('work_id')==work and r.get('status')=='SUCCESS' and marker in json.dumps(r.get('output'))]
failed=[r for r in snapshot['runs'] if r.get('work_id')==failure and r.get('status')=='ERROR']
assert succeeded and failed,{'success':len(succeeded),'failed':len(failed)}
summary.update({'passed':True,'actual_skill_read':True,'actual_mcp':True,'successful_due_runs':[r['run_id'] for r in succeeded],'diagnosed_failure_runs':[r['run_id'] for r in failed]})
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');(state/'snapshot.json').write_text(json.dumps(snapshot,indent=2)+'\n');print(json.dumps(summary,indent=2))

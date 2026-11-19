"""Actual native Codex defines/schedules deterministic and agent flows via CLI/MCP.
Needs live FORGE_BINARY/FORGE_TEST_HOME/FORGE_URL and new FORGE_TEST_STATE.
Reserves the real local model backend for the one scheduled agent flow.
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
args=[exe,'exec','--ephemeral','--skip-git-repo-check','--sandbox','workspace-write','--cd',str(project),'--json','-c','approval_policy="never"','-c','sandbox_workspace_write.network_access=true','-c','shell_environment_policy.inherit="all"','-c','shell_environment_policy.ignore_default_excludes=true','-c','mcp_servers.forge.command='+json.dumps(str(binary)),'-c','mcp_servers.forge.args='+json.dumps(['mcp','--url',url,'--token-env','FORGE_TOKEN']),'-c','mcp_servers.forge.env_vars=["FORGE_TOKEN"]']
for name in usercfg.get('mcp_servers',{}):args.extend(['-c',f'mcp_servers.{name}.enabled=false'])
args.extend(['-c','mcp_servers.forge.enabled=true','-c','mcp_servers.forge.required=true','-c','mcp_servers.forge.default_tools_approval_mode="approve"'])
suffix=uuid.uuid4().hex[:8];work='native-workflow-'+suffix;agentwork='native-agent-'+suffix;ws='native-workflow-due-'+suffix;ags='native-agent-due-'+suffix;marker='NATIVE_FLOW_'+suffix
workflow={'type':'workflow','input':{'params':{'task':'default'},'steps':[{'id':'first','action':{'type':'echo','input':{'text':{'$ref':'/params/task'}}}},{'id':'second','needs':['first'],'action':{'type':'echo','input':{'text':'copy:${/steps/first/output/text}'}}}],'output':{'result':{'$ref':'/steps/second/output/text'}}}}
agent={'type':'agent','input':{'model':'local','instructions':'Call receipt exactly once with mode ok and tag equal to the exact user task. Read the real tool output and return only JSON with the single key result equal to its tag. No prose or markdown.','prompt':'${/params/task}','params':{'task':'default'},'tools':[{'name':'receipt','type':'plugin.fixture'}],'max_rounds':4,'max_tool_calls':2,'output_schema':{'type':'object','properties':{'result':{'type':'string'}},'required':['result'],'additionalProperties':False}}}
prompt=f'''Use the installed forge-operations skill, and actually read .agents/skills/forge-operations/SKILL.md with your shell. Exercise both actual interfaces, not HTTP or database shortcuts. Credentials are already supplied privately as FORGE_TOKEN; never print/read credentials or private config. CLI executable: {binary}; use --url "$FORGE_URL" --token-env FORGE_TOKEN. Forge MCP is also available.
Through direct shell CLI commands, register work {work} with this exact definition JSON: {json.dumps(workflow)}. Schedule it as {ws} every 3 seconds UTC, passing params {{"task":"{marker}_WORKFLOW"}}. Observe a real scheduled run, inspect its actual successful output via CLI, then pause that schedule using CLI.
Through actual Forge MCP tools, register work {agentwork} with exact definition {json.dumps(agent)}. Schedule it as {ags} every 30 seconds UTC with params {{"task":"{marker}_AGENT"}}. Poll until the first actual due run appears, then immediately pause its schedule to avoid extra model calls; inspect that run until terminal. It must complete a real local-model tool loop with plugin receipt and output JSON result={marker}_AGENT. Do not manually run either flow. Do not call other services or use unlisted tools/plugins. Read canonical schedule states to confirm both PAUSED. Use at most 30 tool calls, including shell calls; report actual run IDs, observed structured outputs and schedule states, and stop on any real failure.'''
(state/'prompt.txt').write_text(prompt);args.append(prompt)
started=time.monotonic();proc=subprocess.Popen(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
try:out,err=proc.communicate(timeout=420)
except subprocess.TimeoutExpired:
 os.killpg(proc.pid,signal.SIGTERM)
 try:out,err=proc.communicate(timeout=15)
 except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);out,err=proc.communicate()
assert token not in out+err
(state/'codex.jsonl').write_text(out);(state/'codex.stderr').write_text(err)
summary={'exit':proc.returncode,'duration_s':time.monotonic()-started,'version':subprocess.check_output([exe,'--version'],text=True).strip(),'configured_model':usercfg.get('model'),'configured_provider':usercfg.get('model_provider'),'actual_served_model':'not independently exposed by native CLI','binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'workflow_id':work,'agent_id':agentwork,'workflow_schedule':ws,'agent_schedule':ags,'marker':marker,'skill_source':'actual installed binary skill output'}
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');assert proc.returncode==0,summary
items=[json.loads(l).get('item',{}) for l in out.splitlines() if l.strip() and json.loads(l).get('type')=='item.completed']
commands=[i for i in items if i.get('type')=='command_execution' and i.get('exit_code')==0]
mcp=[i for i in items if i.get('type')=='mcp_tool_call' and i.get('status')=='completed']
assert any('SKILL.md' in i.get('command','') for i in commands)
for tokens in [('work','register',work),('schedules','create',ws),('runs','inspect'),('schedules','pause')]:assert any(all(t in i.get('command','') for t in tokens) for i in commands),tokens
assert any('runs' in i.get('command','') and 'inspect' in i.get('command','') and marker+'_WORKFLOW' in i.get('aggregated_output','') and 'SUCCESS' in i.get('aggregated_output','') for i in commands)
for tool,needle in [('forge_register',agentwork),('forge_schedule',ags),('forge_schedule_action','pause'),('forge_inspect',marker+'_AGENT')]:assert any(i.get('tool')==tool and needle in json.dumps(i) for i in mcp),(tool,needle)
snapshot=httpx.post(url+'/api',headers={'Authorization':'Bearer '+token},json={'method':'snapshot'},timeout=15).json()['result']
inspected=[httpx.post(url+'/api',headers={'Authorization':'Bearer '+token},json={'method':'inspect','params':{'run_id':r['run_id']}},timeout=15).json()['result'] for r in snapshot['runs'] if r['work_id'] in [work,agentwork] and r['status']=='SUCCESS']
wr=[r for r in inspected if r['work_id']==work and r['output']['output']=={'result':'copy:'+marker+'_WORKFLOW'}]
ar=[r for r in inspected if r['work_id']==agentwork and r['output']['json']=={'result':marker+'_AGENT'}]
assert wr and ar
assert all(r['output']['rounds']>=2 and len(r['output']['tool_calls'])==1 for r in ar)
paused=[s for s in snapshot['schedules'] if s['schedule_name'] in ['operator.'+ws,'operator.'+ags]];assert len(paused)==2 and all(s['status']=='PAUSED' for s in paused)
assert not any(i.get('tool')=='forge_run' and agentwork in json.dumps(i) for i in mcp)
assert not any('work run '+work in i.get('command','') for i in commands)
summary.update({'passed':True,'actual_skill_read':True,'actual_cli_workflow_define_schedule_inspect_pause':True,'actual_mcp_agent_define_schedule_inspect_pause':True,'workflow_due_runs':[r['run_id'] for r in wr],'agent_due_runs':[r['run_id'] for r in ar],'canonical_schedules':paused,'successful_shell_calls':len(commands),'successful_mcp_calls':len(mcp)})
(state/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');(state/'snapshot.json').write_text(json.dumps(snapshot,indent=2)+'\n');(state/'inspected-runs.json').write_text(json.dumps(inspected,indent=2)+'\n');print(json.dumps(summary,indent=2))

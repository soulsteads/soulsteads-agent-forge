"""Live native workflow/agent acceptance; Python is evidence tooling only.
Required: FORGE_BINARY, FORGE_TEST_STATE (new), FORGE_DATABASE_URL (fresh),
FORGE_PLUGIN_FIXTURE, LOCAL_MODEL_BASE_URL, LOCAL_MODEL_ID.
Optional FORGE_TEST_PORT=19106, FORGE_CANDIDATE_SHA, CHROMIUM_EXECUTABLE.
Run with exclusive access to the configured inference backend.
"""
from pathlib import Path
import os,json,subprocess,time,hashlib,shutil,uuid,asyncio,threading
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import httpx

state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True);assert binary.read_bytes()[:4]==b'\x7fELF'
pinned=state/'soulsteads-forge';shutil.copy2(binary,pinned);binary=pinned
home=state/'home';port=os.environ.get('FORGE_TEST_PORT','19106');url='http://127.0.0.1:'+port
metadata={'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'candidate':os.environ.get('FORGE_CANDIDATE_SHA'),'runtime':'native Rust ELF / DBOS PostgreSQL','inference':{'base_url':os.environ['LOCAL_MODEL_BASE_URL'],'model':os.environ['LOCAL_MODEL_ID']},'test_driver_only':'Python'}
(state/'artifact.json').write_text(json.dumps(metadata,indent=2)+'\n')
subprocess.run([str(binary),'--home',str(home),'init'],capture_output=True,check=True)
cfg=json.loads((home/'config.json').read_text())
cfg['actors']['observer']={'token':uuid.uuid4().hex,'capabilities':['read'],'types':[]}
cfg['actors']['restricted']={'token':uuid.uuid4().hex,'capabilities':['read','register','execute','schedule'],'types':['workflow','agent','model','echo']}
receipt=state/'external-receipts.jsonl';pidfile=state/'core.pid'
cfg['plugins']={'plugin.fixture':{'command':[shutil.which('node'),str(Path(os.environ['FORGE_PLUGIN_FIXTURE']).resolve(strict=True)),str(receipt),str(pidfile)],'timeout_seconds':20}}
assert {'workflow','agent'}<=set(cfg['actors']['operator']['types']),'fresh native init must expose built-in flow types'
cfg['actors']['operator']['types'].append('plugin.fixture')
cfg['models']['local']={'base_url':os.environ['LOCAL_MODEL_BASE_URL'],'model':os.environ['LOCAL_MODEL_ID']}
# Explicit adversarial protocol fixture, only for negative boundaries. Positive
# agent cases above/below always use the separate real local model alias.
fixture_calls=[]
class ProtocolFixture(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  history=json.dumps(request.get('messages',[]))
  case=next((x for x in ['UNKNOWN_TOOL','TOOL_BUDGET','INVALID_OUTPUT','INVALID_ARGUMENTS','ROUND_BUDGET'] if x in history),None)
  if case is None:self.send_error(400);return
  fixture_calls.append({'case':case,'request':request})
  if case=='INVALID_OUTPUT':message={'role':'assistant','content':'{"result":7}'};reason='stop'
  else:
   args={'mode':'ok','tag':'PROTOCOL_'+case}
   if case=='INVALID_ARGUMENTS':args['unexpected']=True
   message={'role':'assistant','content':None,'tool_calls':[{'id':'call_'+str(len(fixture_calls)),'type':'function','function':{'name':'not_allowlisted' if case=='UNKNOWN_TOOL' else 'receipt','arguments':json.dumps(args)}}]};reason='tool_calls'
  response={'id':'explicit-test-fixture','object':'chat.completion','created':int(time.time()),'model':'protocol-fixture','choices':[{'index':0,'message':message,'finish_reason':reason}]}
  encoded=json.dumps(response).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(encoded)));self.end_headers();self.wfile.write(encoded)
fixture_server=ThreadingHTTPServer(('127.0.0.1',int(os.environ.get('FORGE_FIXTURE_PORT','19107'))),ProtocolFixture)
fixture_thread=threading.Thread(target=fixture_server.serve_forever,daemon=True);fixture_thread.start()
cfg['models']['protocol-fixture']={'base_url':'http://127.0.0.1:'+str(fixture_server.server_port)+'/v1','model':'protocol-fixture'}
(home/'config.json').write_text(json.dumps(cfg));(home/'config.json').chmod(0o600)
env={**os.environ,'FORGE_TOKEN':cfg['actors']['operator']['token'],'FORGE_URL':url}
prefix=[str(binary),'--url',url,'--token-env','FORGE_TOKEN'];proc=None;checks=[];calls=[];runs={}
log=(state/'server.log').open('w')
def cli(*args,data=None,ok=True,actor='operator'):
 p=subprocess.run(prefix+list(args),env={**env,'FORGE_TOKEN':cfg['actors'][actor]['token']},input=json.dumps(data) if data is not None else None,text=True,capture_output=True,timeout=130)
 assert (p.returncode==0)==ok,(args,p.returncode,p.stdout,p.stderr)
 assert all(a['token'] not in p.stdout+p.stderr for a in cfg['actors'].values())
 calls.append({'argv':list(args),'actor':actor,'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr})
 return json.loads(p.stdout) if ok else p.stderr
def start():
 global proc
 proc=subprocess.Popen([str(binary),'--home',str(home),'serve','--port',port],env=env,stdout=log,stderr=log)
 pidfile.write_text(str(proc.pid))
 for _ in range(120):
  assert proc.poll() is None,'server exited'
  try:
   if httpx.get(url,timeout=1).status_code==200:return
  except httpx.HTTPError:pass
  time.sleep(.25)
 raise AssertionError('startup')
def stop(crash=False):
 if proc and proc.poll() is None:
  proc.kill() if crash else proc.terminate();proc.wait(timeout=20)
def register(work,kind,inputs,ok=True):return cli('work','register',work,'--definition','-',data={'type':kind,'input':inputs},ok=ok)
def wait(rid,status='SUCCESS'):
 for _ in range(360):
  row=cli('runs','inspect',rid)
  if row['status'] not in ('ADMITTED','ENQUEUED','PENDING'):
   assert row['status']==status,row
   runs[rid]=row;return row
  time.sleep(.5)
 raise AssertionError('run timeout')
def run(work,key,params=None):
 args=['work','run',work,'--key',key]
 if params is not None:args+=['--params','-']
 return cli(*args,data=params)['run_id']
def rows():return [json.loads(l) for l in receipt.read_text().splitlines()] if receipt.exists() else []
def passed(case):
 checks.append(case);print('PASS '+case,flush=True)
def schedule_once(name,work,params):
 before={r['run_id'] for r in cli('runs','list')}
 cli('schedules','create',name,'--work-id',work,'--cron','*/3 * * * * *','--tz','UTC','--params','-',data=params)
 try:
  for _ in range(100):
   fresh=[r for r in cli('runs','list') if r['work_id']==work and r['run_id'] not in before]
   if fresh:break
   time.sleep(.1)
  assert fresh,'schedule did not admit a due run'
 finally:cli('schedules','pause','operator.'+name)
 assert len(fresh)==1,'test must serialize actual inference; excess due admissions'
 return wait(fresh[0]['run_id'])
ref=lambda pointer:{'$ref':pointer}
workflow={'params':{'task':'DEFAULT_WORKFLOW','payload':{'items':[1,True,'x']}},'steps':[{'id':'first','needs':[],'action':{'type':'echo','input':{'text':ref('/params/task')}}},{'id':'second','needs':['first'],'action':{'type':'echo','input':{'text':'copy:${/steps/first/output/text}'}}}],'output':{'result':ref('/steps/second/output/text'),'typed':ref('/params/payload')}}
agent={'model':'local','instructions':'You must call the receipt tool exactly once before your final response. Use mode ok and set tag to the exact user task string. After reading the real tool response, return only a JSON object with the single key result whose value is the tag from that tool response. Do not invent a tool result. Do not add prose or markdown.','prompt':'${/params/task}','params':{'task':'DEFAULT_AGENT'},'tools':[{'name':'receipt','type':'plugin.fixture'}],'max_rounds':4,'max_tool_calls':2,'output_schema':{'type':'object','properties':{'result':{'type':'string'}},'required':['result'],'additionalProperties':False}}
def assert_agent(row,marker):
 output=row['output'];assert output['json']=={'result':marker},output
 assert output['rounds']>=2 and len(output['tool_calls'])==1,output
 call=output['tool_calls'][0];assert call['name']=='receipt' and call['input']['mode']=='ok' and call['output']['tag']==marker,call
 assert any(r['run_id']==row['run_id'] and r['tag']==marker for r in rows()),'no external tool receipt'
 assert any(m.get('role')=='tool' for m in output['messages']),output
 return output
try:
 start();snap=cli('snapshot');assert {'workflow','agent'}<=set(snap['types'])
 register('flow-deterministic','workflow',workflow)
 assert cli('work','show','flow-deterministic')['definition']=={'type':'workflow','input':workflow}
 deterministic=run('flow-deterministic','same',{'task':'FLOW_TYPED_OK'})
 row=wait(deterministic);assert row['output']['output']['result']=='copy:FLOW_TYPED_OK'
 assert row['output']['output']['typed']=={'items':[1,True,'x']},'typed reference coerced nested JSON'
 assert row['output']['steps']['first']['output']['text']=='FLOW_TYPED_OK'
 assert run('flow-deterministic','same',{'task':'FLOW_TYPED_OK'})==deterministic
 cli('work','run','flow-deterministic','--key','same','--params','-',data={'task':'changed'},ok=False)
 passed('deterministic two-step typed params/reference/interpolation/output mapping and invocation parameter idempotency')
 due=schedule_once('flow-deterministic-due','flow-deterministic',{'task':'FLOW_DUE_OK'})
 assert due['output']['output']['result']=='copy:FLOW_DUE_OK'
 passed('actual scheduled deterministic workflow uses immutable schedule params')
 register('flow-agent','agent',agent)
 manual=wait(run('flow-agent','manual',{'task':'AGENT_MANUAL_OK'}));assert_agent(manual,'AGENT_MANUAL_OK')
 selected_effects=[cli('runs','inspect',manual['run_id'],'--effect-id',e['effect_id'])['effect'] for e in manual['effects']]
 assert sum(e['action']['type']=='agent_turn' for e in selected_effects)==2
 selected_step=cli('runs','inspect',manual['run_id'],'--step-id',str(manual['steps'][-1]['function_id']))['step']
 assert selected_step['output'] is not None and not selected_step['output'].get('summary',False)
 cli('runs','inspect',manual['run_id'],'--effect-id',manual['effects'][0]['effect_id'],'--step-id','0',ok=False)
 (state/'selected-agent-records.json').write_text(json.dumps({'effects':selected_effects,'step':selected_step},indent=2)+'\n')
 passed('real local model chooses tool call, executes installed plugin, consumes tool response and satisfies final JSON schema')
 due_agent=schedule_once('flow-agent-due','flow-agent',{'task':'AGENT_DUE_OK'});assert_agent(due_agent,'AGENT_DUE_OK')
 passed('actual scheduled native agent flow with real local inference and external plugin receipt')
 mixed={'steps':[{'id':'prepare','action':{'type':'echo','input':{'text':'MIXED_AGENT_OK'}}},{'id':'review','needs':['prepare'],'action':{'work_id':'flow-agent','params':{'task':ref('/steps/prepare/output/text')}}}],'output':{'result':ref('/steps/review/output/json/result')}}
 register('flow-mixed','workflow',mixed)
 mixedrow=wait(run('flow-mixed','mixed'));assert mixedrow['output']['output']=={'result':'MIXED_AGENT_OK'}
 assert_agent({**mixedrow,'output':mixedrow['output']['steps']['review']['output']},'MIXED_AGENT_OK')
 passed('workflow composes named immutable agent with typed dependency and mapped structured output')
 collision_action={'type':'plugin.fixture','input':{'mode':'ok','tag':'DISTINCT_DOTTED_SLOTS'}}
 collision={'steps':[{'id':'a.b','action':collision_action},{'id':'a','action':{'type':'workflow','input':{'steps':[{'id':'b','action':collision_action}]}}}]}
 register('flow-slot-collision','workflow',collision);before_collision=len(rows())
 collisionrow=wait(run('flow-slot-collision','distinct'))
 assert len(rows())==before_collision+2
 slots=[x['effect_id'] for x in collisionrow['effects'] if 'plugin.fixture' in json.dumps(x['action'])]
 assert len(slots)==2 and len(set(slots))==2,slots
 passed('dotted sibling a.b and nested a/b actions have distinct native effect slots and each executes exactly once')
 large_tag='BOUNDED_RECORD_'+('z'*5000)
 large_definition={'steps':[{'id':'large','action':{'type':'plugin.fixture','input':{'mode':'ok','tag':large_tag}}}],'output':'BOUNDED_INSPECTION_OK'}
 register('flow-bounded-inspection','workflow',large_definition);large_id=run('flow-bounded-inspection','large');large_row=wait(large_id)
 assert large_row['definition']['summary'] is True
 large_effect=large_row['effects'][0];assert large_effect['action']['summary'] is True and large_effect['output']['summary'] is True
 exact=cli('runs','inspect',large_id,'--effect-id',large_effect['effect_id'])['effect'];assert exact['output']['tag']==large_tag and exact['action']['input']['tag']==large_tag
 large_step=next(s for s in large_row['steps'] if s['function_name'].startswith('forge.flow.') and isinstance(s.get('output'),dict) and s['output'].get('summary'))
 exact_step=cli('runs','inspect',large_id,'--step-id',str(large_step['function_id']))['step'];assert large_tag in json.dumps(exact_step['output'])
 assert cli('work','show','flow-bounded-inspection')['definition']=={'type':'workflow','input':large_definition}
 cli('work','show','flow-bounded-inspection',actor='observer',ok=False)
 assert large_tag not in cli('runs','inspect',large_id,'--effect-id',large_effect['effect_id'],actor='observer',ok=False)
 assert large_tag not in cli('runs','inspect',large_id,'--step-id',str(large_step['function_id']),actor='observer',ok=False)
 snapshot_large=next(r for r in cli('runs','list') if r['run_id']==large_id);assert snapshot_large['output']['summary'] is True
 (state/'bounded-records.json').write_text(json.dumps({'passed':True,'default':large_row,'selected_effect':exact,'selected_step':exact_step,'snapshot':snapshot_large},indent=2)+'\n')
 passed('bounded snapshot/default inspection summarizes large records while exact work/effect/step selectors preserve complete canonical data')
 register('leaf-template','echo',{'text':'${/params/message}'})
 once={'params':{'message':'${/params/secret}','secret':'MUST_NOT_EXPAND_TWICE'},'steps':[{'id':'leaf','action':{'work_id':'leaf-template','params':{'message':ref('/params/message')}}}],'output':ref('/steps/leaf/output/text')}
 register('flow-render-once','workflow',once);once_result=wait(run('flow-render-once','once'))
 assert once_result['output']['output']=='${/params/secret}',once_result
 passed('named leaf invocation params render once and preserve reference-like user data literally')
 # Validation boundaries are checked through the same public CLI/API as real work.
 register('bad-too-many-steps','workflow',{'steps':[{'id':'n'+str(i),'action':{'type':'echo','input':{'text':'x'}}} for i in range(33)]},ok=False)
 register('bad-round-budget','agent',{**agent,'max_rounds':9},ok=False)
 register('bad-tool-budget','agent',{**agent,'max_tool_calls':33},ok=False)
 register('bad-tool-type','agent',{**agent,'tools':[{'name':'escape','type':'plugin.not_configured'}]},ok=False)
 register('bad-forward-dependency','workflow',{'steps':[{'id':'a','needs':['later'],'action':{'type':'echo','input':{'text':'x'}}},{'id':'later','action':{'type':'echo','input':{'text':'y'}}}]},ok=False)
 register('bad-missing-reference','workflow',{'steps':[{'id':'missing','action':{'work_id':'does-not-exist'}}]},ok=False)
 register('bad-authored-internal-turn','workflow',{'steps':[{'id':'escape','action':{'type':'agent_turn','input':{}}}]},ok=False)
 before_malformed=len(rows())
 register('bad-later-static-leaf','workflow',{'steps':[{'id':'first','action':{'type':'plugin.fixture','input':{'mode':'ok','tag':'MUST_NOT_EXECUTE'}}},{'id':'bad','action':{'type':'echo','input':{'text':7}}}]},ok=False)
 assert len(rows())==before_malformed
 cli('work','register','observer-agent','--definition','-',data={'type':'agent','input':agent},actor='observer',ok=False)
 cli('work','register','restricted-agent','--definition','-',data={'type':'agent','input':agent},actor='restricted',ok=False)
 cli('work','register','restricted-workflow','--definition','-',data={'type':'workflow','input':{'steps':[{'id':'denied','action':{'type':'plugin.fixture','input':{'mode':'ok'}}}]}},actor='restricted',ok=False)
 cli('work','disable','flow-agent');cli('work','run','flow-mixed','--key','disabled-child',ok=False);cli('work','enable','flow-agent')
 passed('workflow step/dependency bounds, agent budgets, missing/disabled named resource, unavailable tool and nested leaf authority denial before execution')
 expansion={'params':{'payload':'x'*64000},'steps':[{'id':'small','action':{'type':'echo','input':{'text':'bounded'}}}],'output':[ref('/params/payload') for _ in range(2000)]}
 register('flow-bounded-expansion','workflow',expansion)
 rss=[];measure_stop=threading.Event()
 def measure():
  while not measure_stop.wait(.01):
   try:
    line=next(l for l in Path('/proc',str(proc.pid),'status').read_text().splitlines() if l.startswith('VmRSS:'))
    rss.append(int(line.split()[1]))
   except (FileNotFoundError,StopIteration):break
 sampler=threading.Thread(target=measure,daemon=True);sampler.start();time.sleep(.03)
 began=time.monotonic()
 try:wait(run('flow-bounded-expansion','bounded'),'ERROR')
 finally:measure_stop.set();sampler.join(timeout=2)
 assert proc.poll() is None and rss and max(rss)-rss[0]<64*1024,{'rss_kib':rss}
 (state/'bounded-expansion.json').write_text(json.dumps({'passed':True,'theoretical_expansion_bytes':64000*2000,'rss_start_kib':rss[0],'rss_peak_kib':max(rss),'elapsed_s':time.monotonic()-began,'service_alive':True},indent=2)+'\n')
 passed('repeated large typed references refuse at context budget with bounded live service memory')
 for case,budget,rounds_limit,expected_model_calls,max_effects in [('UNKNOWN_TOOL',2,4,1,0),('INVALID_ARGUMENTS',2,4,1,0),('INVALID_OUTPUT',2,4,1,0),('TOOL_BUDGET',1,4,2,1),('ROUND_BUDGET',2,1,1,0)]:
  before_effects=len(rows());before_model=len(fixture_calls)
  definition={**agent,'model':'protocol-fixture','prompt':case,'max_tool_calls':budget,'max_rounds':rounds_limit}
  work='negative-'+case.lower().replace('_','-');register(work,'agent',definition)
  wait(run(work,'negative'),'ERROR')
  assert len(fixture_calls)-before_model==expected_model_calls,(case,fixture_calls[before_model:])
  effect_delta=len(rows())-before_effects
  assert effect_delta<=max_effects,(case,effect_delta)
  if case=='TOOL_BUDGET':assert effect_delta==1
 passed('explicit adversarial model protocol fixture: unknown tool/invalid arguments/output schema denied, model rounds and actual tool effects bounded')
 register('minimal-default-agent','agent',{'model':'protocol-fixture','prompt':'INVALID_OUTPUT'})
 minimal=wait(run('minimal-default-agent','defaults'));assert minimal['output']['json']=={'result':7} and minimal['output']['tool_calls']==[] and minimal['output']['rounds']==1
 passed('minimal agent omitting tools and budgets uses defaults and accepts unconstrained JSON from explicit protocol fixture')
 # Real pending workflow crash after completed first external action.
 recovery={'steps':[{'id':'effect','action':{'type':'plugin.fixture','input':{'mode':'ok','tag':'FLOW_RECOVERY_ONCE'}}},{'id':'waiting','needs':['effect'],'action':{'type':'echo','input':{'text':ref('/steps/effect/output/tag'),'delay':12}}}],'output':ref('/steps/waiting/output/text')}
 register('flow-recovery','workflow',recovery)
 count=len(rows());recover=run('flow-recovery','recover')
 for _ in range(100):
  observed=cli('runs','inspect',recover)
  completed=any(s.get('output') and 'FLOW_RECOVERY_ONCE' in json.dumps(s['output']) for s in observed['steps'])
  if len(rows())==count+1 and completed:break
  time.sleep(.1)
 assert len(rows())==count+1 and completed,'first effect not checkpointed'
 stop(crash=True);start();recovered=wait(recover)
 assert recovered['output']['output']=='FLOW_RECOVERY_ONCE' and len(rows())==count+1
 passed('actual SIGKILL after first external step resumes durable pending workflow without repeating its effect')
 fenced={'steps':[{'id':'wait','action':{'type':'echo','input':{'text':'wait','delay':30}}},{'id':'never','needs':['wait'],'action':{'type':'plugin.fixture','input':{'mode':'ok','tag':'CANCEL_MUST_NOT_HAPPEN'}}}]}
 register('flow-cancel-fence','workflow',fenced);count=len(rows());canceled=run('flow-cancel-fence','cancel')
 cli('runs','cancel',canceled);wait(canceled,'CANCELLED');assert len(rows())==count
 stop();start();wait(canceled,'CANCELLED');assert len(rows())==count
 passed('parent workflow cancellation fences later external action before and after native restart')
 # Unknown flow tool receipt is explicitly reconciled by slot, never replayed.
 uncertain={'steps':[{'id':'tool','action':{'type':'plugin.fixture','input':{'mode':'crash','tag':'FLOW_UNKNOWN'}}}]}
 register('flow-unknown','workflow',uncertain);count=len(rows())
 try:run('flow-unknown','unknown')
 except AssertionError:pass
 assert proc.wait(timeout=25)==-9 and len(rows())==count+1
 unknown=rows()[-1]['run_id'];start();uncertainrow=wait(unknown,'RECOVERY_REQUIRED')
 effects=uncertainrow['effects'];assert effects
 effect=next(x for x in effects if 'plugin.fixture' in json.dumps(x['action']))
 eid=effect['effect_id']
 cli('runs','retry',unknown,'--key','unsafe',ok=False)
 cli('runs','reconcile',unknown,'--effect-id','missing-slot','--decision','completed','--note','wrong slot','--settled',ok=False)
 cli('runs','reconcile',unknown,'--effect-id',eid,'--decision','completed','--note','Actual external append receipt inspected, killed native process settled','--settled','--output','-',data={'tag':'FLOW_UNKNOWN','receipt':unknown})
 reconciled=cli('runs','inspect',unknown);resolved=next(x for x in reconciled['effects'] if x['effect_id']==eid)
 assert resolved['state']=='completed' and resolved['output']['receipt']==unknown
 cli('runs','retry',unknown,'--key','unsafe-after-settlement',ok=False);assert len(rows())==count+1
 runs[unknown]=reconciled
 assert not any(r.get('tag')=='CANCEL_MUST_NOT_HAPPEN' for r in rows())
 passed('unknown flow tool effect has exact slot evidence, wrong selector denied, settled reconciliation recorded and replay blocked')
 # Compact real dashboard shares definitions and native execution with CLI.
 from playwright.sync_api import sync_playwright
 with sync_playwright() as pw:
  browser=pw.chromium.launch(headless=True,executable_path=os.environ.get('CHROMIUM_EXECUTABLE','/usr/sbin/chromium'))
  page=browser.new_page();page.goto(url);page.locator('#token').fill(env['FORGE_TOKEN']);page.get_by_role('button',name='Connect',exact=True).click()
  page.wait_for_function("() => document.querySelector('#status').textContent.startsWith('Current as of')")
  assert {'workflow','agent'}<=set(page.locator('#register [name=type] option').all_text_contents())
  row=page.locator('#works tr').filter(has=page.get_by_role('cell',name='flow-deterministic',exact=True));row.get_by_role('button',name='Run now',exact=True).click()
  for _ in range(60):
   if 'SUCCESS' in page.locator('#detail').inner_text():break
   page.wait_for_timeout(250);page.locator('#refresh').click()
  assert 'copy:DEFAULT_WORKFLOW' in page.locator('#detail').inner_text()
  page.screenshot(path=str(state/'flows-dashboard.png'),full_page=True);browser.close()
 passed('Chromium exposes workflow/agent types and runs CLI-defined workflow with canonical actual output')
 final=cli('snapshot')
 async def mcp_check():
  from mcp import ClientSession,StdioServerParameters
  from mcp.client.stdio import stdio_client
  with (state/'mcp.stderr').open('w') as err:
   async with stdio_client(StdioServerParameters(command=str(binary),args=['mcp','--url',url],env=env),errlog=err) as (read,write):
    async with ClientSession(read,write) as client:
     await client.initialize();catalog=await client.list_tools();assert len(catalog.tools)==11
     result=await client.call_tool('forge_snapshot',{});assert not result.isError
     view=json.loads(result.content[0].text)
     for key in ['work','runs','schedules','types']:assert view[key]==final[key],key
     result=await client.call_tool('forge_inspect',{'run_id':mixedrow['run_id']});assert not result.isError
     observed=json.loads(result.content[0].text);assert observed['output']['output']=={'result':'MIXED_AGENT_OK'}
     result=await client.call_tool('forge_work',{'work_id':'flow-deterministic'});assert not result.isError
     assert json.loads(result.content[0].text)==cli('work','show','flow-deterministic')
     result=await client.call_tool('forge_inspect',{'run_id':large_id,'effect_id':large_effect['effect_id']});assert not result.isError
     assert json.loads(result.content[0].text)['effect']==exact
     denied=await client.call_tool('forge_work',{'work_id':'flow-deterministic','actor':'observer'});assert denied.isError
 asyncio.run(mcp_check());passed('official MCP reads identical flow state and actual mixed agent/workflow result')
 (state/'runs.json').write_text(json.dumps(runs,indent=2)+'\n');(state/'snapshot.json').write_text(json.dumps(final,indent=2)+'\n')
 result={**metadata,'passed':True,'checks':checks,'deterministic_run':deterministic,'agent_run':manual['run_id'],'agent_due_run':due_agent['run_id'],'mixed_run':mixedrow['run_id'],'recovered_run':recover,'reconciled_unknown_run':unknown,'external_effect_count':len(rows()),'negative_model_fixture':'Explicit localhost protocol fixture only; real local model used for positive manual/due/mixed flows'}
 (state/'evidence.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'passed':len(checks),'binary_sha256':metadata['binary_sha256']}))
finally:
 stop();fixture_server.shutdown();fixture_server.server_close();fixture_thread.join(timeout=5);log.close();(state/'commands.json').write_text(json.dumps(calls,indent=2)+'\n')
 (state/'negative-model-fixture.json').write_text(json.dumps({'role':'explicit adversarial protocol fixture, not real inference','calls':fixture_calls},indent=2)+'\n')
 (state/'partial.json').write_text(json.dumps({'checks':checks,'external_effect_count':len(rows())},indent=2)+'\n')
 (state/'cleanup.json').write_text(json.dumps({'service_stopped':proc is None or proc.poll() is not None,'port':port,'pid':proc.pid if proc else None,'protocol_fixture_stopped':not fixture_thread.is_alive(),'protocol_fixture_port':fixture_server.server_port})+'\n')

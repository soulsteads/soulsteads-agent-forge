"""Portable real native CLI acceptance. Python is test tooling only.

Requires FORGE_BINARY, FORGE_TEST_STATE (new), FORGE_DATABASE_URL (fresh dedicated
PostgreSQL), FORGE_PLUGIN_FIXTURE, LOCAL_MODEL_BASE_URL, LOCAL_MODEL_ID.
Optional FORGE_TEST_PORT=19105, FORGE_CANDIDATE_SHA, CHROMIUM_EXECUTABLE.
The explicit plugin fixture is never installed/configured by product default.
"""
from pathlib import Path
import os,json,subprocess,time,hashlib,shutil,uuid,asyncio
import httpx

state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True);assert binary.read_bytes()[:4]==b'\x7fELF'
pinned=state/'soulsteads-forge';shutil.copy2(binary,pinned);binary=pinned
home=state/'home';port=os.environ.get('FORGE_TEST_PORT','19105');url='http://127.0.0.1:'+port
metadata={'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'candidate':os.environ.get('FORGE_CANDIDATE_SHA'),'runtime':'native Rust ELF / DBOS PostgreSQL','test_driver_only':'Python'}
(state/'artifact.json').write_text(json.dumps(metadata,indent=2)+'\n')
subprocess.run([str(binary),'--home',str(home),'init'],capture_output=True,check=True)
cfg=json.loads((home/'config.json').read_text());cfg['actors']['observer']={'token':uuid.uuid4().hex,'capabilities':['read'],'types':[]}
receipt=state/'external-receipts.jsonl';pidfile=state/'core.pid'
cfg['plugins']={'plugin.fixture':{'command':[shutil.which('node'),str(Path(os.environ['FORGE_PLUGIN_FIXTURE']).resolve(strict=True)),str(receipt),str(pidfile)],'timeout_seconds':20}}
cfg['actors']['operator']['types'].append('plugin.fixture')
cfg['models']['local']={'base_url':os.environ['LOCAL_MODEL_BASE_URL'],'model':os.environ['LOCAL_MODEL_ID']}
(home/'config.json').write_text(json.dumps(cfg));(home/'config.json').chmod(0o600)
env={**os.environ,'FORGE_TOKEN':cfg['actors']['operator']['token'],'FORGE_URL':url}
prefix=[str(binary),'--url',url,'--token-env','FORGE_TOKEN'];proc=None;checks=[];calls=[]
log=(state/'server.log').open('w')
def cli(*args,data=None,ok=True,actor='operator',missing_token=False):
 runenv={**env,'FORGE_TOKEN':cfg['actors'][actor]['token']}
 if missing_token:runenv.pop('FORGE_TOKEN',None)
 p=subprocess.run(prefix+list(args),env=runenv,input=json.dumps(data) if data is not None else None,text=True,capture_output=True,timeout=130)
 assert (p.returncode==0)==ok,(args,p.returncode,p.stdout,p.stderr)
 assert all(a['token'] not in p.stdout+p.stderr for a in cfg['actors'].values()),'token exposed'
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
def stop():
 if proc and proc.poll() is None:
  proc.terminate();proc.wait(timeout=20)
def register(work,kind,inputs):return cli('work','register',work,'--definition','-',data={'type':kind,'input':inputs})
def wait(rid,status='SUCCESS'):
 for _ in range(240):
  row=cli('runs','inspect',rid)
  if row['status'] not in ('ADMITTED','ENQUEUED','PENDING'):
   assert row['status']==status,row
   return row
  time.sleep(.5)
 raise AssertionError('run timeout')
def rows():return [json.loads(l) for l in receipt.read_text().splitlines()] if receipt.exists() else []
try:
 start()
 snap=cli('snapshot');assert snap['health'] and 'plugin.fixture' in snap['types']
 assert cli('work','list')==[] and cli('runs','list')==[] and cli('schedules','list')==[]
 assert 'plugin.fixture' in cli('plugins','list')
 cli('snapshot',ok=False,missing_token=True)
 cli('work','register','denied','--definition','-',data={'type':'echo','input':{}},actor='observer',ok=False)
 register('cli-echo','echo',{'text':'FORGE_CLI_OK'})
 rid=cli('work','run','cli-echo','--key','same')['run_id'];assert cli('work','run','cli-echo','--key','same')['run_id']==rid
 assert wait(rid)['output']['text']=='FORGE_CLI_OK'
 cli('work','disable','cli-echo');cli('work','run','cli-echo','--key','disabled',ok=False);cli('work','enable','cli-echo')
 cli('work','register','cli-echo','--definition','-',data={'type':'echo','input':{'text':'changed'}},ok=False)
 cli('runs','inspect',rid,actor='observer',ok=False)
 checks.append('native CLI authority, list projections, plugin schema discovery, stdin JSON definition, duplicate key and enable/disable/immutability')
 definition=state/'failure-definition.json';definition.write_text('{"type":"fail","input":{}}')
 cli('work','register','cli-fail','--definition',str(definition))
 failed=cli('work','run','cli-fail','--key','failure')['run_id'];wait(failed,'ERROR')
 retried=cli('runs','retry',failed,'--key','retry')['run_id'];assert wait(retried,'ERROR')['parent']==failed
 checks.append('CLI file JSON definition, controlled ERROR and linked explicit retry')
 cli('schedules','create','cli-due','--work-id','cli-echo','--cron','*/2 * * * * *','--tz','America/Chicago')
 due=[]
 for _ in range(40):
  due=[r for r in cli('runs','list') if r['work_id']=='cli-echo' and r['run_id']!=rid]
  if due:break
  time.sleep(.25)
 assert due;assert wait(due[0]['run_id'])['output']['text']=='FORGE_CLI_OK'
 cli('schedules','pause','operator.cli-due');assert cli('schedules','list')[0]['status']=='PAUSED'
 before={r['run_id'] for r in cli('runs','list')};cli('schedules','resume','operator.cli-due')
 for _ in range(40):
  resumed=[r for r in cli('runs','list') if r['run_id'] not in before]
  if resumed:break
  time.sleep(.25)
 assert resumed;wait(resumed[0]['run_id']);cli('schedules','delete','operator.cli-due');assert cli('schedules','list')==[]
 cli('schedules','create','bad-zone','--work-id','cli-echo','--cron','*/2 * * * * *','--tz','Not/AZone',ok=False)
 checks.append('CLI schedule create actual due output, IANA zone, pause/resume actual next due, delete and bad timezone rejection')
 register('cli-slow','echo',{'text':'cancel','delay':30})
 canceled=cli('work','run','cli-slow','--key','cancel')['run_id'];cli('runs','cancel',canceled);wait(canceled,'CANCELLED')
 cli('runs','reconcile',canceled,'--decision','not-applied','--note','Observer may not reconcile','--settled',actor='observer',ok=False)
 checks.append('CLI cancellation and reconciliation authority denial')
 register('cli-plugin','plugin.fixture',{'mode':'ok','tag':'FORGE_CLI_PLUGIN_OK'})
 plugin=cli('work','run','cli-plugin','--key','once')['run_id'];assert wait(plugin)['output']['tag']=='FORGE_CLI_PLUGIN_OK'
 assert len(rows())==1
 from playwright.sync_api import sync_playwright
 with sync_playwright() as pw:
  browser=pw.chromium.launch(headless=True,executable_path=os.environ.get('CHROMIUM_EXECUTABLE','/usr/sbin/chromium'))
  page=browser.new_page();page.goto(url);page.locator('#token').fill(env['FORGE_TOKEN']);page.get_by_role('button',name='Connect',exact=True).click()
  page.wait_for_function("() => document.querySelector('#status').textContent.startsWith('Current as of')")
  assert 'plugin.fixture' in page.locator('#register [name=type] option').all_text_contents()
  row=page.locator('#works tr').filter(has=page.get_by_role('cell',name='cli-plugin',exact=True));row.wait_for();row.get_by_role('button',name='Run now',exact=True).click()
  for _ in range(40):
   if 'SUCCESS' in page.locator('#detail').inner_text():break
   page.wait_for_timeout(250);page.locator('#refresh').click()
  assert 'FORGE_CLI_PLUGIN_OK' in page.locator('#detail').inner_text()
  page.screenshot(path=str(state/'cli-plugin-dashboard.png'),full_page=True);browser.close()
 assert len(rows())==2
 plugin_runs=[r for r in cli('runs','list') if r['work_id']=='cli-plugin'];assert len(plugin_runs)==2 and all(r['status']=='SUCCESS' for r in plugin_runs)
 checks.append('CLI plugin execution and Chromium sees same definition, executes it, CLI reads identical second canonical result')
 register('cli-local','model',{'model':'local','prompt':'Reply exactly FORGE_CLI_LOCAL_OK','expect':'FORGE_CLI_LOCAL_OK'})
 local=cli('work','run','cli-local','--key','local')['run_id'];localrow=wait(local);assert localrow['output']['text'].strip()=='FORGE_CLI_LOCAL_OK'
 checks.append('CLI explicitly configured actual local model output verified')
 register('cli-crash','plugin.fixture',{'mode':'crash','tag':'CLI_RECONCILE'})
 try:cli('work','run','cli-crash','--key','crash')
 except AssertionError:pass
 assert proc.wait(timeout=25)==-9 and len(rows())==3
 crashed=rows()[-1]['run_id'];start();wait(crashed,'RECOVERY_REQUIRED')
 cli('runs','retry',crashed,'--key','unsafe',ok=False)
 cli('runs','reconcile',crashed,'--decision','completed','--note','Actual external receipt inspected; owned process killed and settled','--settled','--output','-',data={'receipt':crashed})
 reconciled=cli('runs','inspect',crashed)
 assert reconciled['effect']['state']=='completed' and reconciled['effect']['output']['receipt']==crashed and len(rows())==3
 cli('runs','retry',crashed,'--key','unsafe-after-reconcile',ok=False)
 checks.append('CLI real post-effect SIGKILL recovery, unsafe retry refused, authorized settled reconciliation with stdin output')
 final=cli('snapshot')
 api=httpx.post(url+'/api',headers={'Authorization':'Bearer '+env['FORGE_TOKEN']},json={'method':'snapshot'},timeout=15).json()['result']
 for k in ['work','runs','schedules','types']:assert final[k]==api[k],k
 async def mcp_readback():
  from mcp import ClientSession,StdioServerParameters
  from mcp.client.stdio import stdio_client
  parameters=StdioServerParameters(command=str(binary),args=['mcp','--url',url,'--token-env','FORGE_TOKEN'],env=env)
  with (state/'mcp.stderr').open('w') as err:
   async with stdio_client(parameters,errlog=err) as (read,write):
    async with ClientSession(read,write) as client:
     await client.initialize()
     snapshot=await client.call_tool('forge_snapshot',{});assert not snapshot.isError
     parsed=json.loads(snapshot.content[0].text)
     for k in ['work','runs','schedules','types']:assert parsed[k]==final[k],k
     result=await client.call_tool('forge_inspect',{'run_id':rid});assert not result.isError
     observed=json.loads(result.content[0].text);assert observed['output']['text']=='FORGE_CLI_OK'
     (state/'mcp-readback.json').write_text(json.dumps({'passed':True,'same_canonical_collections':['work','runs','schedules','types'],'inspected_cli_run':observed},indent=2)+'\n')
 asyncio.run(mcp_readback())
 checks.append('official MCP SDK reads identical CLI/API canonical state and inspects CLI-produced successful output')
 (state/'snapshot.json').write_text(json.dumps(final,indent=2)+'\n')
 result={**metadata,'passed':True,'checks':checks,'local_model_run':localrow,'cli_manual_run':rid,'cli_due_run':due[0]['run_id'],'external_effect_count':len(rows())}
 (state/'evidence.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'passed':len(checks),'binary_sha256':metadata['binary_sha256']}))
finally:
 stop();log.close()
 (state/'commands.json').write_text(json.dumps(calls,indent=2)+'\n')
 (state/'partial.json').write_text(json.dumps({'checks':checks,'effect_count':len(rows())},indent=2)+'\n')
 (state/'cleanup.json').write_text(json.dumps({'service_stopped':proc is None or proc.poll() is not None,'port':port,'pid':proc.pid if proc else None})+'\n')

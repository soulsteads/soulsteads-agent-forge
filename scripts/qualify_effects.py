"""Actual native Forge plugin/effect fault acceptance using an external Node fixture."""
import json,os,subprocess,time,hashlib,shutil
from pathlib import Path
import httpx

state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
home=state/'home';binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True);assert binary.read_bytes()[:4]==b'\x7fELF'
pinned=state/'agent-forge';shutil.copy2(binary,pinned);binary=pinned
fixture=Path(os.environ.get('FORGE_PLUGIN_FIXTURE',str(Path(__file__).resolve().parents[1]/'tests/fixtures/effect-plugin.cjs'))).resolve(strict=True);receipt=state/'external-receipts.jsonl';pidfile=state/'core.pid';cli=[str(binary),'--home',str(home)]
subprocess.run(cli+['init'],check=True,capture_output=True)
cfg=json.loads((home/'config.json').read_text());cfg['plugins']={'plugin.fixture':{'command':[shutil.which('node'),str(fixture),str(receipt),str(pidfile)],'timeout_seconds':20}};cfg['actors']['operator']['types'].append('plugin.fixture');(home/'config.json').write_text(json.dumps(cfg));(home/'config.json').chmod(0o600)
token=cfg['actors']['operator']['token'];url='http://127.0.0.1:'+os.environ.get('FORGE_TEST_PORT','19101');log=(state/'server.log').open('w');proc=None;checks=[]
def start():
 global proc
 proc=subprocess.Popen(cli+['serve','--port',url.rsplit(':',1)[1]],stdout=log,stderr=log)
 pidfile.write_text(str(proc.pid))
 for _ in range(120):
  if proc.poll() is not None:raise RuntimeError('native server startup exit')
  try:
   if httpx.get(url,timeout=1).status_code==200:return
  except httpx.TransportError:pass
  time.sleep(.25)
 raise RuntimeError('native server startup timeout')
def call(method,ok=True,**params):
 response=httpx.post(url+'/api',headers={'Authorization':'Bearer '+token},json={'method':method,'params':params},timeout=15)
 assert response.is_success==ok,(method,response.status_code,response.text)
 return response.json().get('result') if ok else response.status_code
def rows():return [json.loads(x) for x in receipt.read_text().splitlines()] if receipt.exists() else []
def wait(run_id,expected):
 for _ in range(180):
  row=call('inspect',run_id=run_id)
  if row['status'] not in ('PENDING','ENQUEUED','ADMITTED'):
   assert row['status']==expected,row
   return row
  time.sleep(.25)
 raise AssertionError('run timeout')
def stop():
 if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=20)
try:
 start()
 call('register',ok=False,work_id='invalid',definition={'type':'plugin.fixture','input':{'mode':'ok','unexpected':True}})
 call('register',work_id='plugin-ok',definition={'type':'plugin.fixture','input':{'mode':'ok','tag':'completed'}})
 first=call('run',work_id='plugin-ok',key='once')['run_id'];wait(first,'SUCCESS');assert len(rows())==1
 assert call('run',work_id='plugin-ok',key='once')['run_id']==first and len(rows())==1
 checks.append('explicit executable describe/schema/execute and idempotent completed effect')
 call('register',work_id='plugin-crash',definition={'type':'plugin.fixture','input':{'mode':'crash','tag':'crash'}})
 try:crashed=call('run',work_id='plugin-crash',key='crash')['run_id']
 except httpx.TransportError:crashed=None
 assert proc.wait(timeout=25)==-9
 assert len(rows())==2
 crashed=rows()[-1]['run_id'];start();uncertain=wait(crashed,'RECOVERY_REQUIRED');assert len(rows())==2
 call('retry',ok=False,run_id=crashed,key='unsafe')
 call('reconcile',ok=False,run_id=crashed,decision='completed',note='unsettled claim',output={'receipt':crashed})
 call('reconcile',run_id=crashed,decision='completed',note='External append receipt inspected; killed process terminated and action settled',output={'receipt':crashed},settled=True)
 call('retry',ok=False,run_id=crashed,key='unsafe-after-settlement');assert len(rows())==2
 checks.append('real post-effect SIGKILL, uncertain restart, retry denial and explicit settled reconciliation')
 call('register',work_id='block',definition={'type':'echo','input':{'text':'block','delay':6}})
 blockers=[call('run',work_id='block',key=str(i))['run_id'] for i in range(2)]
 for _ in range(60):
  current=[call('inspect',run_id=r)['status'] for r in blockers]
  if current.count('PENDING')==2:break
  time.sleep(.1)
 assert current.count('PENDING')==2,current
 canceled=call('run',work_id='plugin-ok',key='cancel-before-effect')['run_id'];call('cancel',run_id=canceled);wait(canceled,'CANCELLED')
 replacement=call('retry',run_id=canceled,key='replacement')['run_id']
 for r in blockers:wait(r,'SUCCESS')
 wait(replacement,'SUCCESS');time.sleep(.5)
 assert len(rows())==3 and rows()[-1]['run_id']==replacement and all(x['run_id']!=canceled for x in rows()),rows()
 checks.append('queued cancellation fences late original and permits one safe replacement without duplicate effect')
 call('register',work_id='plugin-active',definition={'type':'plugin.fixture','input':{'mode':'effect_then_wait','tag':'active-cancel'}})
 active=call('run',work_id='plugin-active',key='active-cancel')['run_id']
 for _ in range(100):
  if len(rows())==4:break
  time.sleep(.1)
 assert len(rows())==4 and rows()[-1]['run_id']==active
 call('cancel',run_id=active)
 call('retry',ok=False,run_id=active,key='unsafe-active')
 plugin_pid=rows()[-1]['pid']
 for _ in range(150):
  if not Path('/proc',str(plugin_pid)).exists():break
  time.sleep(.1)
 assert not Path('/proc',str(plugin_pid)).exists(),'owned plugin process did not settle'
 stop();start()
 active_state=call('inspect',run_id=active)
 assert active_state['status'] in ('CANCELLED','RECOVERY_REQUIRED'),active_state
 call('retry',ok=False,run_id=active,key='unsafe-restart')
 assert len(rows())==4
 checks.append('active cancellation preserves completed external effect and refuses replay before and after restart')
 result={'passed':True,'checks':checks,'provider_effect_count':len(rows()),'uncertain':uncertain,'canceled_run':canceled,'replacement':replacement,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'test_fixture':'external Node executable, never bundled with native Forge'}
 (state/'evidence.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'passed':len(checks),'effects':len(rows())}))
finally:stop();log.close();(state/'partial.json').write_text(json.dumps({'checks':checks,'effect_count':len(rows())},indent=2)+'\n')

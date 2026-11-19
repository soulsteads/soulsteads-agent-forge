"""Native MCP protocol and authority negatives through the official client SDK."""
import asyncio,os,json,subprocess,hashlib
from pathlib import Path
from mcp import ClientSession,StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.shared.exceptions import McpError

async def main():
 binary=Path(os.environ['FORGE_BINARY']).resolve(strict=True)
 home=Path(os.environ['FORGE_TEST_HOME']).resolve()
 state=Path(os.environ['FORGE_TEST_STATE']).resolve();state.mkdir(parents=True,exist_ok=False)
 cfg=json.loads((home/'config.json').read_text());url=os.environ.get('FORGE_URL','http://127.0.0.1:19100')
 env={**os.environ,'FORGE_TOKEN':cfg['actors']['operator']['token']}
 parameters=StdioServerParameters(command=str(binary),args=['mcp','--url',url,'--token-env','FORGE_TOKEN'],env=env)
 with (state/'stderr.log').open('w') as err:
  async with stdio_client(parameters,errlog=err) as (read,write):
   async with ClientSession(read,write) as client:
    init=await client.initialize();tools=await client.list_tools();names={t.name for t in tools.tools}
    assert {'forge_snapshot','forge_work','forge_register','forge_inspect','forge_reconcile'}<=names,names
    tool=next(t for t in tools.tools if t.name=='forge_register')
    assert tool.annotations and tool.annotations.readOnlyHint is False
    snapshot=await client.call_tool('forge_snapshot',{});assert not snapshot.isError
    actor=await client.call_tool('forge_register',{'work_id':'forged-actor','definition':{'type':'echo','input':{'text':'not admitted'}},'actor':'forged'})
    assert actor.isError,'extra caller-supplied identity accepted'
    try:
     unknown=await client.call_tool('not_a_tool',{});assert unknown.isError
    except McpError as denied:
     assert denied.error.code in (-32601,-32602),denied
    (state/'tools.json').write_text(json.dumps([t.model_dump(mode='json') for t in tools.tools],indent=2)+'\n')
 bare={k:v for k,v in os.environ.items() if k!='FORGE_TOKEN'}
 missing=subprocess.run([str(binary),'mcp','--url',url,'--token-env','FORGE_TOKEN'],input='',env=bare,text=True,capture_output=True,timeout=15)
 assert missing.returncode!=0,'missing token did not refuse startup'
 (state/'missing-token.stderr').write_text(missing.stderr)
 result={'passed':True,'native_sdk_handshake':True,'tool_count':len(names),'extra_identity_rejected':True,'unknown_tool_rejected':True,'missing_bearer_refused':True,'mutations_annotated':True,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}
 (state/'evidence.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))

asyncio.run(main())

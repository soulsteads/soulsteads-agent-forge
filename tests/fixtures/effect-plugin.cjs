// Test-only external executable plugin. Never bundled/installed in core Forge.
const fs=require('node:fs');
const [receipt,pidfile]=process.argv.slice(2);
let text='';process.stdin.setEncoding('utf8');process.stdin.on('data',x=>text+=x);
process.stdin.on('end',async()=>{
 try {
  const request=JSON.parse(text);
  if(request.api_version!==1)throw Error('protocol version');
  if(request.operation==='describe'){
   console.log(JSON.stringify({api_version:1,description:'isolated live external-effect fixture',schema:{type:'object',properties:{mode:{enum:['ok','crash','delay','effect_then_wait']},tag:{type:'string'}},required:['mode'],additionalProperties:false}}));return;
  }
  if(request.operation==='close'){console.log(JSON.stringify({api_version:1}));return;}
  if(request.operation!=='execute')throw Error('operation');
  if(request.input.mode==='delay')await new Promise(r=>setTimeout(r,1500));
  const row={pid:process.pid,run_id:request.context.run_id,idempotency_key:request.context.idempotency_key,tag:request.input.tag||request.input.mode};
  const fd=fs.openSync(receipt,'a',0o600);fs.writeSync(fd,JSON.stringify(row)+'\n');fs.fsyncSync(fd);fs.closeSync(fd);
  if(request.input.mode==='effect_then_wait')await new Promise(r=>setTimeout(r,5000));
  if(request.input.mode==='crash'){
   if(Number(fs.readFileSync(pidfile,'utf8').trim())!==process.ppid)throw Error('fixture parent ownership mismatch');
   process.kill(process.ppid,'SIGKILL');process.exit(76);
  }
  console.log(JSON.stringify({api_version:1,output:{receipt:row.run_id,tag:row.tag}}));
 }catch(error){process.stderr.write(String(error.message));process.exitCode=1;}
});

'use strict';
const {test}=require('node:test'),assert=require('node:assert/strict'),http=require('http'),fs=require('fs'),os=require('os'),path=require('path'),{spawn}=require('child_process');
test('programme stream preserves network ID, ends at cached end and releases backend',async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'mkra-program-test-'));let released=false,seen;
 const program={id:40015100001,networkId:4,serviceId:151,eventId:1,startAt:Date.now()-5000,endAt:Date.now()+1000};
 fs.writeFileSync(path.join(dir,'bs.json'),JSON.stringify({programs:[program]}));
 const backend=http.createServer((req,res)=>{seen=req.url;res.writeHead(200,{'content-type':'video/MP2T'});res.write('synthetic payload');const interval=setInterval(()=>res.write('.'),50);res.on('close',()=>{released=true;clearInterval(interval)})});
 await new Promise(r=>backend.listen(0,'127.0.0.1',r));
 const vacant=http.createServer();await new Promise(r=>vacant.listen(0,'127.0.0.1',r));const port=vacant.address().port;await new Promise(r=>vacant.close(r));
 const child=spawn(process.execPath,[path.join(__dirname,'server.cjs')],{env:{...process.env,LISTEN_PORT:String(port),MKRA_URL:`http://127.0.0.1:${backend.address().port}`,CACHE_FILE:path.join(dir,'missing.json'),BS_CACHE_FILE:path.join(dir,'bs.json')},stdio:['ignore','pipe','pipe']});
 try{
  await new Promise((resolve,reject)=>{child.stdout.once('data',resolve);child.once('error',reject);child.once('exit',()=>reject(Error('proxy exited')))});
  const response=await fetch(`http://127.0.0.1:${port}/api/programs/${program.id}/stream`,{signal:AbortSignal.timeout(8000)});
  assert.equal(response.status,200);assert.ok((await response.text()).startsWith('synthetic payload'));
  assert.equal(seen,'/api/services/400151/stream');await new Promise(r=>setTimeout(r,100));assert.ok(released);assert.ok(Date.now()>=program.endAt+950);
 } finally {child.kill();await new Promise(r=>child.once('exit',r));await new Promise(r=>backend.close(r));fs.rmSync(dir,{recursive:true,force:true})}
});

'use strict';
const {test}=require('node:test'),assert=require('node:assert/strict'),http=require('http'),fs=require('fs'),os=require('os'),path=require('path'),{spawn}=require('child_process');
test('4K scan bootstraps with storedEvents=0; both scanners preserve cache when busy',async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'mkra-scan-test-'));let busy=false,tunes=0;
 const program={id:110010100001,networkId:11,serviceId:101,eventId:1,startAt:Date.now()-1000,endAt:Date.now()+3600000};
 const server=http.createServer((req,res)=>{
  res.setHeader('Content-Type','application/json');
  if(req.url==='/api/status')return res.end(JSON.stringify({epg:{storedEvents:0}}));
  if(req.url==='/api/tuners')return res.end(JSON.stringify([{isFree:!busy,isUsing:busy,users:[]} ]));
  if(req.url.startsWith('/api/reserves'))return res.end(JSON.stringify({reserves:[]}));
  if(req.url==='/api/services')return res.end(JSON.stringify([{id:1100101,networkId:11,serviceId:101,name:'synthetic'}]));
  if(req.url==='/api/services/1100101/programs')return res.end(JSON.stringify([program]));
  if(req.url==='/api/services/1100101/stream') {tunes++;res.write('synthetic');return;}
  res.writeHead(404);res.end();
 });
 await new Promise(r=>server.listen(0,'127.0.0.1',r));
 const base=`http://127.0.0.1:${server.address().port}`,cache=path.join(dir,'programs.json'),status=path.join(dir,'status.json');
 async function run(script='scan-4k.cjs'){const child=spawn(process.execPath,[path.join(__dirname,script)],{env:{...process.env,MKRA_URL:base,EPGSTATION_URL:base,CACHE_FILE:cache,BS_CACHE_FILE:path.join(dir,'bs-programs.json'),STATUS_FILE:status,STREAM_MS:'1'},stdio:['ignore','pipe','pipe']});let err='';child.stderr.on('data',x=>err+=x);const timeout=setTimeout(()=>child.kill(),15000);try{const code=await new Promise((r,j)=>{child.once('exit',r);child.once('error',j)});assert.equal(code,0,err)}finally{clearTimeout(timeout)}}
 try {
  await run();assert.equal(tunes,1);assert.equal(JSON.parse(fs.readFileSync(cache)).programs[0].id,program.id);
  const before=fs.readFileSync(cache,'utf8');busy=true;await run();assert.equal(tunes,1);assert.equal(fs.readFileSync(cache,'utf8'),before);assert.equal(JSON.parse(fs.readFileSync(status)).status,'deferred');
  const bsCache=path.join(dir,'bs-programs.json'),saved='synthetic existing cache';fs.writeFileSync(bsCache,saved);await run('scan-bs.cjs');assert.equal(fs.readFileSync(bsCache,'utf8'),saved);assert.equal(tunes,1);
 } finally {server.closeAllConnections();await new Promise(r=>server.close(r));fs.rmSync(dir,{recursive:true,force:true})}
});

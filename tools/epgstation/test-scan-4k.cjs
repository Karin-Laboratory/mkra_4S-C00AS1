'use strict';
const {test}=require('node:test'),assert=require('node:assert/strict'),http=require('http'),fs=require('fs'),os=require('os'),path=require('path'),{spawn}=require('child_process');
test('4K scan bootstraps on placeholder status, preserves partial cache and defers on unsafe/failed inputs',async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'mkra-scan-test-'));let busy=false,tunes=0,api503=false,streamCut=false,hasReserve=false;
 const program={id:110010100001,networkId:11,serviceId:101,eventId:1,startAt:Date.now()-1000,endAt:Date.now()+3600000};
 const server=http.createServer((req,res)=>{
  res.setHeader('Content-Type','application/json');
  if(req.url==='/api/status')return res.end(JSON.stringify({epg:{storedEvents:0}}));
  if(req.url==='/api/tuners'){if(api503){res.writeHead(503);return res.end('synthetic outage')}return res.end(JSON.stringify([{isFree:!busy,isUsing:busy,users:[]} ]));}
  if(req.url.startsWith('/api/reserves'))return res.end(JSON.stringify({reserves:hasReserve?[{startAt:Date.now()+1000,endAt:Date.now()+3600000,isSkip:false,isConflict:false}]:[]}));
  if(req.url==='/api/services')return res.end(JSON.stringify([{id:1100101,networkId:11,serviceId:101,name:'synthetic'}]));
  if(req.url==='/api/services/1100101/programs')return res.end(JSON.stringify([program]));
  if(req.url==='/api/services/1100101/stream') {tunes++;if(streamCut)return res.end('truncated TS');res.write('synthetic');return;}
  res.writeHead(404);res.end();
 });
 await new Promise(r=>server.listen(0,'127.0.0.1',r));
 const base=`http://127.0.0.1:${server.address().port}`,cache=path.join(dir,'programs.json'),status=path.join(dir,'status.json');
 async function run(script='scan-4k.cjs'){const child=spawn(process.execPath,[path.join(__dirname,script)],{env:{...process.env,MKRA_URL:base,EPGSTATION_URL:base,CACHE_FILE:cache,BS_CACHE_FILE:path.join(dir,'bs-programs.json'),STATUS_FILE:status,STREAM_MS:'1'},stdio:['ignore','pipe','pipe']});let err='';child.stderr.on('data',x=>err+=x);const timeout=setTimeout(()=>child.kill(),15000);try{const code=await new Promise((r,j)=>{child.once('exit',r);child.once('error',j)});assert.equal(code,0,err)}finally{clearTimeout(timeout)}}
 try {
  // Legacy cache may have no lastSuccess metadata. Preserve future events for
  // a service that cannot be rescanned, rather than introducing guide holes.
  const legacy={id:110014100002,networkId:11,serviceId:141,eventId:2,startAt:Date.now()+3600000,endAt:Date.now()+7200000};
  fs.writeFileSync(cache,JSON.stringify({programs:[legacy]}));
  await run();assert.equal(tunes,1);assert.equal(JSON.parse(fs.readFileSync(status)).status,'partial',JSON.stringify(JSON.parse(fs.readFileSync(status))));
  const savedPrograms=JSON.parse(fs.readFileSync(cache)).programs;
  assert.ok(savedPrograms.some(x=>x.id===program.id));
  assert.ok(savedPrograms.some(x=>x.id===legacy.id),'valid legacy cache without lastSuccess must survive');
  const before=fs.readFileSync(cache,'utf8');busy=true;await run();assert.equal(tunes,1);assert.equal(fs.readFileSync(cache,'utf8'),before);assert.equal(JSON.parse(fs.readFileSync(status)).status,'deferred');
  busy=false;hasReserve=true;await run();assert.equal(tunes,1,'reservation window must defer before tuning');hasReserve=false;
  api503=true;await run();assert.equal(tunes,1,'503 before scan must not tune');assert.equal(fs.readFileSync(cache,'utf8'),before,'503 must leave last-good cache byte-for-byte unchanged');api503=false;
  streamCut=true;await run();assert.equal(tunes,2,'short TS stream is rejected as failed service acquisition');
  const afterCut=JSON.parse(fs.readFileSync(cache));assert.ok(afterCut.programs.some(x=>x.id===legacy.id));assert.ok(afterCut.programs.some(x=>x.id===program.id),'failed partial stream retains last-good data');streamCut=false;
  const corrupt=path.join(dir,'corrupt.json');fs.writeFileSync(corrupt,'{not-json');
  const child=spawn(process.execPath,[path.join(__dirname,'scan-4k.cjs')],{env:{...process.env,MKRA_URL:base,EPGSTATION_URL:base,CACHE_FILE:corrupt,BS_CACHE_FILE:path.join(dir,'bs-programs.json'),STATUS_FILE:status,STREAM_MS:'1'},stdio:['ignore','pipe','pipe']});
  const code=await new Promise(resolve=>child.once('exit',resolve));assert.notEqual(code,0);assert.equal(fs.readFileSync(corrupt,'utf8'),'{not-json');assert.equal(tunes,2,'corrupt cache is rejected before tuner use');
  const bsCache=path.join(dir,'bs-programs.json'),saved='synthetic existing cache';fs.writeFileSync(bsCache,saved);busy=true;await run('scan-bs.cjs');assert.equal(fs.readFileSync(bsCache,'utf8'),saved);assert.equal(tunes,2);
 } finally {server.closeAllConnections();await new Promise(r=>server.close(r));fs.rmSync(dir,{recursive:true,force:true})}
});

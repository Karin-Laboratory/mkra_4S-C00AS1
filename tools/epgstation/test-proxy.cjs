'use strict';
const {test}=require('node:test'),assert=require('node:assert/strict'),http=require('http'),fs=require('fs'),os=require('os'),path=require('path'),{spawn}=require('child_process');
test('network-separated EPG, query filters and BS/BS4K routing',async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'mkra-epg-test-')),seen=[];
 const a={id:40015100001,networkId:4,serviceId:151,eventId:1,startAt:100,endAt:200},b={...a,id:110015100001,networkId:11};
 fs.writeFileSync(path.join(dir,'4k.json'),JSON.stringify({programs:[b]}));fs.writeFileSync(path.join(dir,'bs.json'),JSON.stringify({programs:[a]}));
 const backend=http.createServer((req,res)=>{seen.push(req.url);res.setHeader('content-type','application/json');res.end(JSON.stringify([{type:'BS4K'},{type:'BS'}]));});await new Promise(r=>backend.listen(0,'127.0.0.1',r));
 const vacant=http.createServer();await new Promise(r=>vacant.listen(0,'127.0.0.1',r));const port=vacant.address().port;await new Promise(r=>vacant.close(r));
 const child=spawn(process.execPath,[path.join(__dirname,'server.cjs')],{env:{...process.env,LISTEN_PORT:String(port),MKRA_URL:`http://127.0.0.1:${backend.address().port}`,CACHE_FILE:path.join(dir,'4k.json'),BS_CACHE_FILE:path.join(dir,'bs.json')},stdio:['ignore','pipe','pipe']});
 try{
  await new Promise((resolve,reject)=>{child.stdout.once('data',resolve);child.once('error',reject);child.once('exit',()=>reject(Error('proxy exited')))});
  const get=async p=>{const r=await fetch(`http://127.0.0.1:${port}${p}`);assert.equal(r.status,200);return r.json()};
  assert.equal((await get('/api/programs')).length,2);assert.deepEqual(await get('/api/programs?networkId=4&serviceId=151'),[a]);assert.deepEqual(await get('/api/programs?startAt=201'),[]);assert.deepEqual(await get('/api/services/400151/programs'),[a]);assert.deepEqual(await get('/api/programs/'+a.id),a);
  await get('/api/channels/BS4K/BS_TS4010');assert.equal(seen.pop(),'/api/channels/BS/BS_TS4010');await get('/api/channels/BS/BS7_0');assert.equal(seen.pop(),'/api/channels/BS4K/BS7_0');assert.equal((await get('/api/channels/BS4K')).length,2);assert.equal(seen.pop(),'/api/channels');
 }finally{child.kill();await new Promise(r=>child.once('exit',r));await new Promise(r=>backend.close(r));fs.rmSync(dir,{recursive:true,force:true});}
});

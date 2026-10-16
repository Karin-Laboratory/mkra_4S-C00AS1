'use strict';
const fs=require('fs'),http=require('http'),{createCollector}=require('./bs-eit.cjs');
const BASE=process.env.MKRA_URL||'http://198.51.100.10:40772',CACHE=process.env.BS_CACHE_FILE||'/var/lib/bs4k-epg/bs-programs.json';
const EPGSTATION=process.env.EPGSTATION_URL||'http://127.0.0.1:8888';
const DWELL=Number(process.env.BS_DWELL_MS||60000);
function get(url){return new Promise((resolve,reject)=>{const r=http.get(url,x=>{let b='';x.on('data',c=>b+=c);x.on('end',()=>{try{if(x.statusCode!==200)throw Error('HTTP '+x.statusCode);resolve(JSON.parse(b))}catch(e){reject(e)}})});const t=setTimeout(()=>r.destroy(Error('API deadline')),5000);r.on('close',()=>clearTimeout(t));r.on('error',reject)})}
async function safe(){const [t,r]=await Promise.all([get(BASE+'/api/tuners'),get(EPGSTATION+'/api/reserves?isHalfWidth=true')]);const now=Date.now();return t.length&&t.every(x=>x.isFree&&!x.isUsing)&&!(r.reserves||[]).some(x=>!x.isSkip&&!x.isConflict&&x.startAt<=now+180000&&x.endAt>=now)}
async function main(){
 if(!await safe()){console.log('BS EIT deferred: tuner busy or recording window');return;}
 const c=createCollector();let timer,guard;let received=0;
 await new Promise((resolve,reject)=>{
  const request=http.get(BASE+'/api/services/400151/stream',res=>{
   if(res.statusCode!==200){res.resume();return done(Error('stream HTTP '+res.statusCode))}
   res.on('data',b=>received+=b.length);res.pipe(c.input);
   timer=setTimeout(()=>done(),DWELL);
   guard=setInterval(async()=>{try{const r=await get(EPGSTATION+'/api/reserves?isHalfWidth=true'),now=Date.now();if(r.reserves.some(x=>!x.isSkip&&!x.isConflict&&x.startAt<=now+180000&&x.endAt>=now))done(Error('recording window'))}catch(e){done(e)}},1000);
   res.on('error',done);res.on('end',()=>done(Error('early stream EOF')));
  });
  const deadline=setTimeout(()=>done(Error('scan deadline')),DWELL+20000);let ended=false;
  function done(err){if(ended)return;ended=true;clearTimeout(timer);clearTimeout(deadline);clearInterval(guard);request.destroy();c.input.end();err?reject(err):resolve()}
  request.on('error',done);
 });
 if(c.errors.length)throw Error(c.errors.join('; '));
 let old;try{old=JSON.parse(fs.readFileSync(CACHE))}catch(e){if(e.code!=='ENOENT')throw e;old={programs:[]}}
 const now=Date.now(),by=new Map((old.programs||[]).filter(p=>p.networkId===4&&p.endAt>now-6*3600000).map(p=>[p.id,p]));
 for(const p of c.programs.values())if(p.endAt>now-6*3600000&&p.startAt<now+9*86400000)by.set(p.id,p);
 const programs=[...by.values()],counts={};for(const p of programs)counts[p.serviceId]=(counts[p.serviceId]||0)+1;
 if(Object.keys(counts).length!==6)throw Error('Incomplete six-station EIT; cache preserved: '+JSON.stringify(counts));
 const tmp=CACHE+'.tmp.'+process.pid;fs.writeFileSync(tmp,JSON.stringify({updatedAt:now,source:'broadcast ISDB-S H-EIT',programs}));fs.renameSync(tmp,CACHE);
 console.log(JSON.stringify({received,programs:programs.length,counts}));
}
main().catch(e=>{console.error(e.stack);process.exitCode=1});

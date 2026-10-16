'use strict';
// Runs on CT/host, never on the embedded receiver. aribts 2.1.12 is
// already installed with EPGStation. Its Date decoder expects JST locally.
process.env.TZ='Asia/Tokyo';
const arib=require(process.env.ARIBTS_PATH||'/opt/EPGStation/node_modules/aribts');

const ALLOWED=new Set([101,141,151,161,171,181]);
function createCollector(){
 const programs=new Map(), connector=new arib.TsReadableConnector(), packets=new arib.TsPacketParser(), sections=new arib.TsSectionParser(), events=new arib.TsEventManager();
 const errors=[];for(const s of [connector,packets,sections,events])s.on('error',e=>errors.push(String(e)));
 connector.pipe(packets);
 // Only EIT. Do not let media payloads allocate section assemblers.
 class EitOnly extends arib.TsBase {_process(p,done){if(p.getPid()===0x12)this.push(p);done();}}
 // Keep custom aribts stream callback/backpressure semantics.
 const filter=new EitOnly();packets.pipe(filter);filter.pipe(sections);
 sections.pipe(events);
 function event(e){
  const i=e.getInfo(),short=e.getShortEvent();
  if(i.originalNetworkId!==4||!ALLOWED.has(i.serviceId)||!short||!short.eventName)return;
  const startAt=i.startTime.getTime(),duration=i.duration*1000;
  if(!Number.isFinite(startAt)||duration<=0||duration>86400000)return;
  const id=(i.originalNetworkId*100000+i.serviceId)*100000+i.eventId;
  const p={id,eventId:i.eventId,serviceId:i.serviceId,networkId:4,startAt,duration,endAt:startAt+duration,isFree:i.freeCaMode===0,name:short.eventName,description:short.text};
  const extended=e.getExtendedEvent();if(extended)p.extended=extended.items;
  const content=e.getContent();if(content)p.genres=content.contents.map(c=>({lv1:c.contentNibbleLevel1,lv2:c.contentNibbleLevel2,un1:c.userNibble1,un2:c.userNibble2}));
  const video=e.getComponent();if(video)p.video={type:video.streamContent===1?'mpeg2':'h.264',resolution:({1:'480i',2:'480i',3:'480i',4:'480i',0x91:'2160p',0x92:'2160p',0x93:'2160p',0x94:'2160p',0xa1:'480p',0xa2:'480p',0xa3:'480p',0xa4:'480p',0xb1:'1080i',0xb2:'1080i',0xb3:'1080i',0xb4:'1080i',0xc1:'720p',0xc2:'720p',0xc3:'720p',0xc4:'720p'})[video.componentType]||'1080i',streamContent:video.streamContent,componentType:video.componentType};
  const audio=e.getAudioComponent();if(audio)p.audio={componentType:audio.componentType,samplingRate:({5:32000,6:44100,7:48000})[audio.samplingRate]||48000,langs:[audio.languageCode]};
  const old=programs.get(id)||{};programs.set(id,{...old,...p,extended:p.extended||old.extended});
 }
 for(const name of ['present','following','schedule'])events.on(name,event);
 return {input:connector,programs,errors};
}
module.exports={createCollector};
if(require.main===module){
 const fs=require('fs'),c=createCollector();const src=process.argv[2]&&process.argv[2]!=='-'?fs.createReadStream(process.argv[2]):process.stdin;
 src.pipe(c.input);c.input.on('finish',()=>{console.log(JSON.stringify({programs:[...c.programs.values()],errors:c.errors}));});
}

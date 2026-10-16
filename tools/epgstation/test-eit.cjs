'use strict';
const {test}=require('node:test'),assert=require('node:assert/strict'),{once}=require('events');
const {createCollector}=require('./bs-eit.cjs');
function crc(b){let c=0xffffffff;for(const x of b){c=(c^(x<<24))>>>0;for(let i=0;i<8;i++)c=((c<<1)^((c&0x80000000)?0x04c11db7:0))>>>0}return c;}
function section(network=4,sid=151){
 const title=Buffer.from([14,...Buffer.from('TITLE')]),text=Buffer.from([14,...Buffer.from('description '.repeat(13))]);
 const desc=Buffer.concat([Buffer.from([0x4d,3+1+title.length+1+text.length]),Buffer.from('jpn'),Buffer.from([title.length]),title,Buffer.from([text.length]),text]);
 const mjd=Math.floor(Date.UTC(2026,9,16)/86400000)+40587;
 const b=Buffer.concat([Buffer.from([0x4e,0xb0,0,sid>>8,sid&255,0xc1,0,0,0x40,0x10,network>>8,network&255,0,0x4e,0,1,mjd>>8,mjd&255,0x09,0,0,0,0x15,0,0x80,desc.length]),desc]);
 const len=b.length+4-3;b[1]|=len>>8;b[2]=len&255;const tail=Buffer.alloc(4);tail.writeUInt32BE(crc(b));return Buffer.concat([b,tail]);
}
async function collect(b){const c=createCollector();let off=0,cc=0;while(off<b.length){const first=off===0,p=Buffer.alloc(188,255);p[0]=0x47;p[1]=first?0x40:0;p[2]=0x12;p[3]=0x10|cc++;let start=4;if(first)p[start++]=0;const n=Math.min(188-start,b.length-off);b.copy(p,start,off,off+n);off+=n;c.input.write(p);}const done=once(c.input,'finish');c.input.end();await done;return [...c.programs.values()];}
test('fragmented CRC-valid EIT decodes JST and descriptors; damaged/non-BS data rejected',async()=>{
 const b=section();assert.equal(crc(b),0);const p=await collect(b);assert.equal(p.length,1);assert.equal(p[0].startAt,Date.UTC(2026,9,16,0));assert.equal(p[0].duration,900000);assert.equal(p[0].networkId,4);assert.equal(p[0].serviceId,151);assert.ok(p[0].name.includes('TITLE')||p[0].name.includes('ＴＩＴＬＥ'));
 const damaged=Buffer.from(b);damaged[30]^=1;assert.equal((await collect(damaged)).length,0);assert.equal((await collect(section(11))).length,0);assert.equal((await collect(section(4,999))).length,0);
});

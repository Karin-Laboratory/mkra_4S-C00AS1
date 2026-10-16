#!/usr/bin/env python3
"""Explicit real-hardware EPGStation BS2K -> BS4K timed reservation test.
Run on EPG host. Creates only two bounded reservations; preserves all recordings.
"""
import argparse,json,time,urllib.request
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--record',action='store_true');p.add_argument('--gap',type=int,default=30);p.add_argument('--output',required=True);p.add_argument('--bs2k-channel',type=int,choices=[400101,400141,400151,400161,400171,400181],default=400151);p.add_argument('--bs4k-channel',type=int,choices=[1100101,1100141,1100151,1100161,1100171,1100181],default=1100101);a=p.parse_args();test_channels=[a.bs2k_channel,a.bs4k_channel]
if not a.record: p.error('--record required')
root=Path(a.output);root.mkdir(parents=True,exist_ok=True)
def api(path,data=None):
    req=urllib.request.Request('http://127.0.0.1:8888/api/'+path,data=None if data is None else json.dumps(data).encode(),headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=20) as r:return json.load(r)
def save(name,obj): (root/(name+'.json')).write_text(json.dumps(obj,ensure_ascii=False,indent=2))
before=api('reserves?isHalfWidth=true');save('reserves-before',before)
if api('recording?isHalfWidth=true')['total']:raise RuntimeError('existing recording')
now=int(time.time()*1000);start=now+25000;end2=start+45000;start4=end2+a.gap*1000;end4=start4+75000
if any(r['startAt']<end4+180000 and r['endAt']>now for r in before['reserves']):raise RuntimeError('existing reservation near test')
created=[]
for ch,s,e in [(a.bs2k_channel,start,end2),(a.bs4k_channel,start4,end4)]:
    body={'allowEndLack':True,'timeSpecifiedOption':{'name':f'BS2K-to-4K verification {ch}','channelId':ch,'startAt':s,'endAt':e}}
    result=api('reserves',body);created.append({'request':body,'result':result});save('created',created);print('RESERVED',ch,result,flush=True)
seen={};timeline=[]
while time.time()*1000<end4+25000:
    active=api('recording?isHalfWidth=true');row={'at':int(time.time()*1000),'recording':active};timeline.append(row);save('timeline',timeline)
    for r in active['records']:
        if r['channelId'] in test_channels:seen[r['id']]=r
    print('ACTIVE',time.strftime('%H:%M:%S'),[(r['id'],r['channelId']) for r in active['records']],flush=True)
    time.sleep(5)
save('seen',seen);save('reserves-after',api('reserves?isHalfWidth=true'))
recent=api('recorded?isHalfWidth=true&offset=0&limit=20');save('recorded-after',recent)
ours=[r for r in recent['records'] if r['channelId'] in test_channels and now<=r['startAt']<=end4]
save('results',ours)
print('RESULTS',[(r['id'],r['channelId'],r.get('videoFiles')) for r in ours],flush=True)
if {r['channelId'] for r in ours}!=set(test_channels) or any(not r.get('videoFiles') or r['isRecording'] or not r['videoFiles'][0]['size'] for r in ours):raise RuntimeError('missing, empty or unfinished recording')

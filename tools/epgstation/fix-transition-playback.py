#!/usr/bin/env python3
"""EPG host: apply verified 10-bit playback / timed recording wait corrections.
Run only while EPGStation is idle; keeps exact pre-change files in --backup.
Restart EPGStation separately after this script succeeds.
"""
import argparse,json,shutil,urllib.request
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--apply',action='store_true');p.add_argument('--backup',type=Path,required=True);a=p.parse_args()
if not a.apply:p.error('--apply required')
with urllib.request.urlopen('http://127.0.0.1:8888/api/recording?isHalfWidth=true',timeout=10) as r:
 if json.load(r)['total']:raise RuntimeError('recording active; do not patch/restart')
root=Path('/opt/EPGStation');changes=[]
config=root/'config/config.yml';text=config.read_text();needle='-profile:v baseline'
if text.count(needle)!=6 or '-pix_fmt yuv420p -profile:v baseline' in text:raise RuntimeError('unexpected baseline configuration; inspect manually')
changes.append((config,text.replace(needle,'-pix_fmt yuv420p '+needle)))
for rel in ['src/model/operator/recording/RecordingStreamCreator.ts','dist/model/operator/recording/RecordingStreamCreator.js']:
 path=root/rel;text=path.read_text();needle='reserve.startAt - now - 1000 * this.config.timeSpecifiedStartMargin'
 if text.count(needle)!=1:raise RuntimeError('unexpected timed wait: '+str(path))
 changes.append((path,text.replace(needle,'Math.max(0, reserve.startAt - Date.now() - 1000 * this.config.timeSpecifiedStartMargin)')))
for path,text in changes:
 backup=a.backup/path.relative_to(root);backup.parent.mkdir(parents=True,exist_ok=True)
 if backup.exists():raise RuntimeError('backup already exists: '+str(backup))
 shutil.copy2(path,backup)
for path,text in changes:path.write_text(text);print('UPDATED',path)

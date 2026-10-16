#!/usr/bin/env python3
"""Validate existing EPGStation recordings on EPG host; never changes reservations."""
import argparse,json,subprocess,urllib.request
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('ids',nargs='+',type=int);p.add_argument('--output',required=True);a=p.parse_args();root=Path(a.output);root.mkdir(parents=True,exist_ok=True)
for rid in a.ids:
 with urllib.request.urlopen(f'http://127.0.0.1:8888/api/recorded/{rid}?isHalfWidth=true',timeout=10) as r:meta=json.load(r)
 (root/f'{rid}-metadata.json').write_text(json.dumps(meta,ensure_ascii=False,indent=2))
 video=meta['videoFiles'][0];path=Path('/recorded')/video['filename']
 probe=subprocess.run(['ffprobe','-v','error','-show_streams','-show_format','-of','json',str(path)],capture_output=True,text=True,timeout=60)
 (root/f'{rid}-probe.json').write_text(probe.stdout);(root/f'{rid}-probe.stderr').write_text(probe.stderr)
 decode=subprocess.run(['ffmpeg','-nostdin','-hide_banner','-v','warning','-ss','3','-i',str(path),'-t','10','-map','0:v:0','-map','0:a:0','-f','null','-','-progress',str(root/f'{rid}-decode.progress')],capture_output=True,text=True,timeout=180)
 (root/f'{rid}-decode.stderr').write_text(decode.stderr)
 info=json.loads(probe.stdout);progress=(root/f'{rid}-decode.progress').read_text()
 summary={'recordId':rid,'path':str(path),'size':path.stat().st_size,'probeExit':probe.returncode,'decodeExit':decode.returncode,'streams':[{'codec':s.get('codec_name'),'type':s.get('codec_type'),'width':s.get('width'),'height':s.get('height'),'duration':s.get('duration')} for s in info['streams']],'formatDuration':info['format'].get('duration'),'decodeProgress':progress}
 (root/f'{rid}-summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2));print(json.dumps(summary,ensure_ascii=False),flush=True)
 if probe.returncode or decode.returncode or 'progress=end' not in progress:raise RuntimeError(f'AV verification failed {rid}')

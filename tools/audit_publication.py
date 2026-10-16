#!/usr/bin/env python3
"""Audit every tracked file and blob reachable from public branches/tags.

This is a reproducible policy check, not a substitute for reviewing source and
classifying test fixtures. Reports only filenames, object IDs and categories.
"""
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = '5d11ccf'

def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT)

# Only the five already-public upstream SI parser fixtures may be binary.
FIXTURES = {}
for item in git('ls-tree', '-r', UPSTREAM, '--', 'testdata').decode().splitlines():
    meta, name = item.split('\t')
    FIXTURES[name] = hashlib.sha256(git('cat-file', 'blob', meta.split()[2])).hexdigest()

INHERITED = set()
for line in git('rev-list', '--objects', UPSTREAM).decode().splitlines():
    INHERITED.add(line.split()[0])

SECRET_PATTERNS = [
    ('private key', rb'-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----'),
    ('GitHub credential', rb'\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})'),
    ('cloud access key', rb'\bAKIA[A-Z0-9]{16}\b'),
    ('bearer credential', rb'(?i)bearer\s+[A-Za-z0-9_-]{30,}'),
    ('literal password', rb'(?i)(?:password|passwd|api_key|access_token)\s*[:=]\s*["\'][^"\'\s]{12,}["\']'),
]
PRIVATE_PATTERNS = [rb'/home/[A-Za-z0-9._-]+(?:/|\b)', rb'\bCT[0-9]{3}\b']
ADDRESS = re.compile(rb'(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])')
BAD_EXTENSIONS = {'.rvp', '.exe', '.mp4', '.m2ts', '.mkv', '.wav', '.aac', '.pem', '.key', '.dump'}
errors = []

def inspect(data, name, inherited=False, oid=None):
    def error(category):
        errors.append({'path': name, 'object': oid, 'category': category})
    if len(data) > 512*1024:
        error('file exceeds source-size policy')
    if '\0' in name or name.split('/')[0] in {'evidence', 'rootfs', 'private', 'target', 'node_modules'}:
        error('private/generated path')
    if Path(name).suffix.lower() in BAD_EXTENSIONS:
        error('forbidden distribution type')
    try:
        data.decode('utf-8')
        text = True
    except UnicodeDecodeError:
        text = False
    if b'\0' in data or not text:
        if FIXTURES.get(name) != hashlib.sha256(data).hexdigest():
            error('unapproved binary (only unchanged upstream SI fixtures allowed)')
        return
    # The auditor contains the names of the forbidden categories, not secrets.
    if name != 'tools/audit_publication.py':
        for pattern in PRIVATE_PATTERNS:
            if re.search(pattern, data):
                error('private laboratory identifier')
    for category, pattern in SECRET_PATTERNS:
        if re.search(pattern, data):
            error(category)
    for match in ADDRESS.finditer(data):
        try:
            address = ipaddress.ip_address(match.group().decode())
        except ValueError:
            continue
        octets = address.packed
        if octets[0] == 10 or (octets[0] == 172 and 16 <= octets[1] <= 31) or octets[:2] == bytes((192, 168)):
            # This example was already public in all three upstream commits.
            # No newly introduced blobs may retain it or any real lab address.
            if not (inherited and address.packed == bytes((192, 168, 0, 40))):
                error('private IPv4 address')

manifest=[]
for raw in git('ls-files', '-z').split(b'\0'):
    if not raw: continue
    name=raw.decode(); data=(ROOT/name).read_bytes()
    inspect(data,name)
    manifest.append({'path':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})

# HEAD includes the audited final tree after committing. Before committing,
# tracked working files above and existing reachable history are both audited.
seen=set();blob_count=0
for row in git('rev-list', '--objects', '--all').decode().splitlines():
    fields=row.split(' ',1);oid=fields[0];name=fields[1] if len(fields)>1 else '(metadata)'
    if oid in seen:continue
    seen.add(oid);kind=git('cat-file','-t',oid).strip()
    if kind==b'blob':
        inspect(git('cat-file','blob',oid),name,oid in INHERITED,oid);blob_count+=1
    elif kind in (b'commit', b'tag'):
        inspect(git('cat-file',kind.decode(),oid),'(metadata)',oid in INHERITED,oid)
report={'tracked_files':len(manifest),'reachable_blobs':blob_count,'inherited_commits':len(git('rev-list',UPSTREAM).splitlines()),'files':manifest,'errors':errors}
(ROOT/'target').mkdir(exist_ok=True)
(ROOT/'target/publication-audit.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:v for k,v in report.items() if k!='files'},indent=2))
sys.exit(bool(errors))

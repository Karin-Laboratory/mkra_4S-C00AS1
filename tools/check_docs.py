#!/usr/bin/env python3
"""Check local Markdown links without contacting documentation or device hosts."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
files = [ROOT/'README.md', *sorted((ROOT/'docs').rglob('*.md')), ROOT/'tools/epgstation/README.ja.md']
errors=[]
for path in files:
    for link in re.findall(r'\]\(([^)]+)\)', path.read_text()):
        if re.match(r'[A-Za-z][A-Za-z0-9+.-]*:|#', link):
            continue
        target = link.split('#', 1)[0]
        if target and not (path.parent/target).exists():
            errors.append(f'{path.relative_to(ROOT)}: missing local link {target}')
if errors:
    raise SystemExit('\n'.join(errors))
print(f'DOCUMENT_LINKS=PASS ({len(files)} Markdown files)')

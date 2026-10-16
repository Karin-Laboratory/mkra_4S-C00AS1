#!/usr/bin/env python3
"""Prepare a local, integrity-pinned RAM deployment bundle; never contacts a device."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[2]

def make_bundle(binary, output):
    binary, output = Path(binary).resolve(), Path(output).resolve()
    # Fail rather than overwrite a previously staged bundle or any source tree.
    data = binary.read_bytes()
    if len(data) < 52 or data[:7] != b'\x7fELF\x01\x01\x01' or int.from_bytes(data[18:20], 'little') != 40:
        raise ValueError('expected a little-endian ELF32 ARM executable')
    output.mkdir(parents=True, exist_ok=False)
    config = ROOT / 'config/bs-additional-main.tsv'
    for source, name in [(binary, 'mkra'), (config, 'bs-channels.tsv'),
                         (ROOT/'tools/mkra-watch/watch.py', 'watch.py'),
                         (ROOT/'tools/sharp_telnet.py', 'sharp_telnet.py')]:
        shutil.copy2(source, output/name)
    cfg = config.read_bytes()
    manifest = {'binary_size': len(data), 'binary_sha256': hashlib.sha256(data).hexdigest(),
                'binary_md5': hashlib.md5(data).hexdigest(), 'config_sha256': hashlib.sha256(cfg).hexdigest(),
                'config_md5': hashlib.md5(cfg).hexdigest()}
    (output/'bundle.json').write_text(json.dumps(manifest, indent=2)+'\n')
    return manifest

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'target/armv7-unknown-linux-musleabihf/release/mkra')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(make_bundle(args.binary, args.output), indent=2))

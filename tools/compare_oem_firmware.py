#!/usr/bin/env python3
"""Read-only RVP and PE/CAB comparison; never executes a firmware or EXE.

Example: --rvp SHARP=HD30_134.RVP --rvp MASPRO=HD30M_134.RVP
         --rvp IODATA=HD30IO_134.RVP --exe hvt4kbc134.exe --out report.json
Ranges use half-open byte offsets. Equal regions do not imply equal programs.
"""
import argparse
import hashlib
import itertools
import json
import pathlib
import struct


def sha(b):
    return hashlib.sha256(b).hexdigest()


def equal_runs(a, b, minimum=4096, chunk=1048576):
    """Maximal equal ranges at the same file offset, skipping equal chunks."""
    runs, start = [], None
    end = min(len(a), len(b))
    for i in range(0, end, chunk):
        j = min(i + chunk, end)
        if a[i:j] == b[i:j]:
            if start is None:
                start = i
            continue
        for k in range(i, j):
            if a[k] == b[k]:
                if start is None:
                    start = k
            elif start is not None:
                if k-start >= minimum:
                    runs.append([start, k])
                start = None
    if start is not None and end-start >= minimum:
        runs.append([start, end])
    return runs


def common_suffix(a, b):
    lo, hi = 0, min(len(a), len(b))
    while lo < hi:
        m = (lo + hi + 1) // 2
        if a[-m:] == b[-m:]:
            lo = m
        else:
            hi = m-1
    return lo


def exe_report(b):
    r = {'bytes': len(b), 'sha256': sha(b)}
    cab = b.find(b'MSCF')
    if cab >= 0 and cab+36 <= len(b):
        n = struct.unpack_from('<I', b, cab+8)[0]
        r['cab'] = dict(offset=cab, bytes=n, end=cab+n,
                        fits_in_file=cab+n <= len(b),
                        files=struct.unpack_from('<H', b, cab+28)[0],
                        flags=struct.unpack_from('<H', b, cab+30)[0])
    if len(b) >= 64 and b[:2] == b'MZ':
        pe = struct.unpack_from('<I', b, 60)[0]
        opt = pe+24
        if opt+2 <= len(b) and b[pe:pe+4] == b'PE\0\0':
            magic = struct.unpack_from('<H', b, opt)[0]
            offset = {0x10b: 128, 0x20b: 144}.get(magic)
            if offset is not None and opt+offset+8 <= len(b):
                start, n = struct.unpack_from('<II', b, opt+offset)
                r['certificate'] = dict(offset=start, bytes=n, end=start+n,
                                         fits_in_file=start+n <= len(b))
    return r


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--rvp', action='append', default=[], metavar='NAME=PATH')
    p.add_argument('--exe', type=pathlib.Path)
    p.add_argument('--out', type=pathlib.Path, required=True)
    args = p.parse_args()
    blobs, files = {}, {}
    for spec in args.rvp:
        name, path = spec.split('=', 1)
        if name in blobs:
            p.error('duplicate name: '+name)
        b = pathlib.Path(path).read_bytes()
        blobs[name] = b
        files[name] = dict(path=path, bytes=len(b), sha256=sha(b),
                           bs1909122_eucjp_offset=b.find('ＢＳ１９０９１２２'.encode('euc_jp')))
    result = {'files': files, 'pairs': {}}
    for x, y in itertools.combinations(blobs, 2):
        a, b = blobs[x], blobs[y]
        runs = equal_runs(a, b)
        tail = common_suffix(a, b)
        result['pairs'][x+' / '+y] = dict(
            same_offset_regions=[dict(start=i, end=j, bytes=j-i, sha256=sha(a[i:j])) for i,j in runs],
            suffix=dict(a_start=len(a)-tail, b_start=len(b)-tail, bytes=tail,
                        sha256=sha(a[-tail:] if tail else b'')))
    if args.exe:
        result['exe'] = exe_report(args.exe.read_bytes())
    args.out.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()

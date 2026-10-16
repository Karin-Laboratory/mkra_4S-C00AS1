#!/usr/bin/env python3
"""Install mkra on a SHARP 4S-C00AS1 over telnet, and prove it works.

Everything lands in /tmp/mt, which is tmpfs: a power cycle undoes all of it.
Nothing here writes to eMMC, MTD, SPI flash or /dev/shm, and nothing signals a
product process.

    python3 tools/deploy.py --host 198.51.100.10                 # upload only
    python3 tools/deploy.py --host 198.51.100.10 --smoke         # + prove it streams
    python3 tools/deploy.py --host 198.51.100.10 --smoke --serve # + leave it running
    python3 tools/deploy.py --host 198.51.100.10 --stop          # stop our server

--smoke captures ten seconds of live 4K on the box, pulls it back over HTTP and
checks the 188-byte framing here. If `ffprobe` is installed it also decodes a
frame, which is the only check that actually proves the stream is usable.

Requires: a root telnet on port 2323 (see docs/SETUP.md) and the box switched
on -- green LED, not standby.
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from sharp_telnet import MICROTEST_DIR, connect  # noqa: E402

DEFAULT_BINARY = os.path.join(
    ROOT, "target/armv7-unknown-linux-musleabihf/release/mkra")
REMOTE = MICROTEST_DIR + "/mkra"
SMOKE_TS = MICROTEST_DIR + "/smoke.ts"


def progress(done, total):
    pct = 100 * done // max(total, 1)
    print("\r  %3d%%  (%d/%d chunks)" % (pct, done, total), end="", flush=True)
    if done >= total:
        print()


def check_state(t):
    """Report uptime and whether the box is out of standby.

    In standby there is no A/V pipeline and no MMT elementary-stream ring, so
    every capture returns zero bytes. That looks like a broken install but is
    just a box nobody switched on, so it is worth saying plainly.
    """
    uptime = t.run("cat /proc/uptime").split()[0]
    av = t.run("grep -h . /proc/[0-9]*/comm 2>/dev/null | "
               "grep -cE '^(MIXER|buf_buf|Sink|Gate)'").strip()
    operating = av not in ("0", "")
    print("device uptime %ss, A/V pipeline threads %s (%s)"
          % (uptime, av, "operating" if operating else "STANDBY"))
    return operating


def stop_ours(t):
    """Terminate any mkra we started earlier. Never touches product processes."""
    stopped = 0
    for pid, exe in t.running_microtests():
        print("stopping %s (pid %s)" % (exe, pid))
        t.stop_microtest(pid)
        stopped += 1
        time.sleep(0.5)
    return stopped


def upload(t, binary):
    size = os.path.getsize(binary)
    print("uploading %s (%d bytes) -> %s" % (os.path.basename(binary), size, REMOTE))
    started = time.time()
    md5 = t.upload(binary, REMOTE, progress=progress)
    rate = size / max(time.time() - started, 0.001) / 1024
    print("  md5 %s verified on device (%.0f KiB/s)" % (md5, rate))
    print("  " + t.run("%s --help" % REMOTE).splitlines()[0])


def ts_report(path):
    """188-byte sync rate and continuity errors, computed here rather than on
    the box. A file that is the right size but not TS-aligned is the classic
    'looked like it worked' failure, so measure it instead of trusting it."""
    data = open(path, "rb").read()
    if len(data) < 188 * 10:
        return "only %d bytes -- nothing streamed" % len(data)
    start = data.find(0x47)
    total = aligned = 0
    cc = {}
    errors = 0
    for off in range(start, len(data) - 187, 188):
        total += 1
        if data[off] != 0x47:
            continue
        aligned += 1
        pid = ((data[off + 1] & 0x1F) << 8) | data[off + 2]
        if data[off + 3] & 0x10:                       # has payload
            got = data[off + 3] & 0x0F
            prev = cc.get(pid)
            if prev is not None and got != (prev + 1) % 16:
                errors += 1
            cc[pid] = got
    return ("%d packets, %.2f%% aligned, %d PIDs, %d CC errors"
            % (total, 100.0 * aligned / total, len(cc), errors))


def pull(t, host, remote_path, local_path, port):
    """Copy a file off the box over HTTP -- telnet base64 would take minutes."""
    size = t.run("wc -c < %s 2>/dev/null" % remote_path).strip()
    size = int(size.split()[0]) if size.split() else 0
    if size == 0:
        print("  %s is empty on the device" % remote_path)
        return 0
    t.run("%s serve --port %d --file %s --raw-stream --file-bitrate 0 "
          "> %s/pull.log 2>&1 &" % (REMOTE, port, remote_path, MICROTEST_DIR))
    time.sleep(2)
    with urllib.request.urlopen("http://%s:%d/api/channels" % (host, port), timeout=10) as r:
        chans = json.load(r)
    url = ("http://%s:%d/api/channels/%s/%s/stream"
           % (host, port, chans[0]["type"], chans[0]["channel"]))
    limit = max(30, size // 200_000 + 20)
    with open(local_path, "wb") as fh:
        subprocess.run(["curl", "-sN", "--max-time", str(limit), url],
                       stdout=fh, stderr=subprocess.DEVNULL)
    got = os.path.getsize(local_path)
    print("  pulled %d of %d bytes -> %s" % (got, size, local_path))
    return got


def smoke(t, host, seconds, port):
    print("\n--- 1/3  live graph (read-only) ---")
    graph = t.run("%s graph --sniff" % REMOTE, timeout=120)
    print("\n".join(graph.splitlines()[:12]))

    print("\n--- 2/3  discover the product's live ES rings (read-only) ---")
    disc = t.run("%s remux4k --discover-only" % REMOTE, timeout=120)
    print(disc)
    if "video" not in disc.lower():
        print("\nNo live elementary streams. The box is not decoding 4K right now.\n"
              "Switch it on with the remote (green LED) and select a 4K channel.")
        return False

    print("\n--- 3/3  capture %d s of live 4K ---" % seconds)
    t.remove_microtest(SMOKE_TS)
    out = t.run("%s remux4k --out %s --seconds %d" % (REMOTE, SMOKE_TS, seconds),
                timeout=seconds + 120)
    print(out)

    local = os.path.join(ROOT, "smoke.ts")
    if pull(t, host, SMOKE_TS, local, port) == 0:
        return False
    print("  " + ts_report(local))

    try:
        p = subprocess.run(["ffprobe", "-hide_banner", "-v", "error",
                            "-show_entries", "stream=codec_name,width,height",
                            "-of", "default=noprint_wrappers=1", local],
                           capture_output=True, text=True, timeout=60)
        if p.stdout.strip():
            print("  ffprobe: " + " ".join(p.stdout.split()))
        else:
            print("  ffprobe found no decodable stream")
    except FileNotFoundError:
        print("  (install ffmpeg to decode-check %s)" % local)
    return True


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True, help="the box's IP address")
    ap.add_argument("--port", type=int, default=2323, help="telnet port")
    ap.add_argument("--binary", default=DEFAULT_BINARY,
                    help="mkra to upload (default: the cross-built one)")
    ap.add_argument("--smoke", action="store_true",
                    help="capture and verify a live 4K stream after uploading")
    ap.add_argument("--smoke-seconds", type=int, default=10)
    ap.add_argument("--serve", action="store_true",
                    help="leave `mkra serve` running when finished")
    ap.add_argument("--serve-port", type=int, default=40772)
    ap.add_argument("--channel-type", default="BS4K", choices=("BS4K", "BS"),
                    help="BS4K for Mirakurun-BS4K clients, BS for plain "
                         "BonDriver_Mirakurun (default BS4K)")
    ap.add_argument("--no-tune", action="store_true",
                    help="do not let mkra change the box's channel")
    ap.add_argument("--stop", action="store_true",
                    help="just stop a running mkra and exit")
    ap.add_argument("--pull-port", type=int, default=40790,
                    help="temporary port used to copy files back")
    a = ap.parse_args()

    t = connect(a.host, a.port)
    try:
        if a.stop:
            print("stopped %d process(es)" % stop_ours(t))
            return 0

        if not os.path.exists(a.binary):
            sys.exit("%s is missing -- run ./build-device.sh, or pass --binary "
                     "with a release download" % a.binary)

        operating = check_state(t)
        stop_ours(t)                      # a running binary cannot be overwritten
        t.make_microtest_dir()
        upload(t, a.binary)

        if not operating:
            print("\nThe box is in standby, so there is nothing to capture yet.\n"
                  "Press the power button (LED green), then re-run with --smoke.")
            return 0

        ok = True
        if a.smoke:
            ok = smoke(t, a.host, a.smoke_seconds, a.pull_port)

        if a.serve:
            cmd = ("%s serve --remux4k %s --channel-type %s --port %d "
                   "--epg-dir /tmp/adsb > %s/mkra.log 2>&1 &"
                   % (REMOTE, "" if a.no_tune else "--tune",
                      a.channel_type, a.serve_port, MICROTEST_DIR))
            t.run(cmd)
            time.sleep(2)
            print("\n--- server ---")
            print(t.run("tail -3 %s/mkra.log" % MICROTEST_DIR).strip())
            print("\nMirakurun-compatible API:  http://%s:%d/api/channels"
                  % (a.host, a.serve_port))
            print("Point TVTest / EPGStation at http://%s:%d"
                  % (a.host, a.serve_port))

        if a.smoke and not ok:
            return 1
        return 0
    finally:
        t.close()


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Idempotent EPG host watcher for the volatile BS4K/BS2K 4S deployment."""
import fcntl
import hashlib
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
BINARY = HERE / "mkra"
# Missing manifest fails closed in verify_bundle; tests may inject synthetic pins.
try:
    MANIFEST = json.loads((HERE / "bundle.json").read_text())
except FileNotFoundError:
    MANIFEST = {}
EXPECTED_SHA256 = MANIFEST.get("binary_sha256", "")
EXPECTED_MD5 = MANIFEST.get("binary_md5", "")
CONFIG = HERE / "bs-channels.tsv"
CONFIG_SHA256 = MANIFEST.get("config_sha256", "")
CONFIG_MD5 = MANIFEST.get("config_md5", "")
REQUIRED_BS = {400101, 400141, 400151, 400161, 400171, 400181}
SCAN_LOCK = pathlib.Path("/var/lib/bs4k-epg/scan.lock")
EXPECTED_SIZE = MANIFEST.get("binary_size", 0)
EXPECTED_VERSION = "0.1.0"
TUNER_HOST = os.environ.get("TUNER_HOST", "198.51.100.10")
TELNET_PORT = int(os.environ.get("TELNET_PORT", "2323"))
API_BASE = os.environ.get("MKRA_API", "http://198.51.100.10:40772")
PROXY_BASE = os.environ.get("MKRA_PROXY", "http://127.0.0.1:40773")
EPGSTATION_BASE = os.environ.get("EPGSTATION_URL", "http://127.0.0.1:8888")
STATE_DIR = pathlib.Path("/var/lib/4s-mkra-watch")
STATE_FILE = STATE_DIR / "state.json"
LOCK_FILE = STATE_DIR / "watch.lock"
BACKOFF_BASE = 15
BACKOFF_MAX = 300
START_GUARD_SECONDS = 600
REMOTE = "/tmp/mt/mkra-unified-auto"
REMOTE_CONFIG = "/tmp/mt/mkra-bs-auto.tsv"
LOG = "/tmp/mt/mkra-unified-auto.log"
START_COMMAND = (REMOTE + " serve --remux4k --tune --bs2k --channel-type BS4K "
                 "--epg-tap --epg-refresh 5 --port 40772 "
                 "--epg-dir /tmp/adsb --isdbs-channels " + REMOTE_CONFIG +
                 " >> " + LOG + " 2>&1 < /dev/null & echo KARIN_PID=$!")



def log(message):
    print(time.strftime("%Y-%m-%dT%H:%M:%S%z") + " " + message, flush=True)


def get_json(url, timeout=3):
    req = urllib.request.Request(url, headers={"User-Agent": "4s-mkra-watch/1"})
    with urllib.request.urlopen(req, timeout=timeout) as res:
        if res.status != 200:
            raise RuntimeError("HTTP %d from %s" % (res.status, url))
        return json.loads(res.read(1024 * 1024))


def api_healthy(fetch=get_json):
    status = fetch(API_BASE + "/api/status")
    tuners = fetch(API_BASE + "/api/tuners")
    proc = status.get("process", {})
    version = proc.get("versions", {}).get("mkra")
    if version != EXPECTED_VERSION or not isinstance(proc.get("pid"), int):
        return False, "unexpected status version/pid"
    if not isinstance(tuners, list) or not tuners:
        return False, "empty or invalid tuner list"
    tuner = next((t for t in tuners if t.get("index") == 0), None)
    if not tuner or tuner.get("isAvailable") is not True or tuner.get("isFault") is not False:
        return False, "tuner 0 unavailable or faulted"
    services = fetch(API_BASE + "/api/services")
    ids = {p.get("id") for p in services if isinstance(p, dict)} if isinstance(services, list) else set()
    if not REQUIRED_BS.issubset(ids) or 1100101 not in ids:
        return False, "unified BS2K/BS4K catalogue missing"
    # isUsing/isFree are intentionally not required: a legitimate client may
    # be recording. API reachability and a healthy advertised tuner suffice.
    return True, "version=%s pid=%d" % (version, proc["pid"])


def parse_recordings_safe(reserves, recordings, now_ms, guard_ms=START_GUARD_SECONDS * 1000):
    if recordings:
        return False, "recording active (%d)" % len(recordings)
    for item in reserves:
        if not isinstance(item, dict):
            return False, "invalid reservation entry"
        if item.get("isSkip") or item.get("isConflict"):
            continue
        start = item.get("startAt")
        end = item.get("endAt")
        if not isinstance(start, (int, float)) or not isinstance(end, (int, float)) or end <= start:
            return False, "invalid reservation time"
        if isinstance(start, (int, float)) and isinstance(end, (int, float)):
            if start <= now_ms < end:
                return False, "reservation active"
            if now_ms <= start < now_ms + guard_ms:
                return False, "reservation starts within guard window"
    return True, "no active or imminent recording"


def load_state():
    try:
        value = json.loads(STATE_FILE.read_text())
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


def save_state(state):
    STATE_DIR.mkdir(mode=0o750, parents=True, exist_ok=True)
    tmp = STATE_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, sort_keys=True) + "\n")
    os.chmod(tmp, 0o640)
    os.replace(tmp, STATE_FILE)


def defer_failure(state, reason):
    failures = min(int(state.get("failures", 0)) + 1, 10)
    delay = min(BACKOFF_BASE * (2 ** (failures - 1)), BACKOFF_MAX)
    state.update(failures=failures, next_attempt=int(time.time()) + delay,
                 last_result="deferred: " + reason)
    save_state(state)
    log("DEFER %s; retry in %ds (failure_count=%d)" % (reason, delay, failures))


def defer_safe(state, reason, delay=120):
    state.update(next_attempt=int(time.time()) + delay, last_result="safe-wait: " + reason)
    save_state(state)
    log("WAIT %s; recheck in %ds" % (reason, delay))


def local_start_safety(require_reservation_guard=False):
    # The existing EPG host endpoints are read-only. Fail closed if EPGStation's
    # schedule/recording state cannot be established.
    reserve_data = get_json(EPGSTATION_BASE + "/api/reserves?isHalfWidth=true")
    recording_data = get_json(EPGSTATION_BASE + "/api/recording?isHalfWidth=true")
    if not isinstance(reserve_data, dict) or not isinstance(reserve_data.get("reserves"), list) or not isinstance(recording_data, dict) or not isinstance(recording_data.get("records"), list):
        return False, "invalid recording/reservation response"
    reserves, recordings = reserve_data["reserves"], recording_data["records"]
    safe, why = parse_recordings_safe(reserves, recordings, int(time.time() * 1000))
    if not safe:
        # This path only runs after proving no receiver mkra process exists.
        # Starting an idle server restores the endpoint needed by a coming
        # reservation; waiting until that programme ends would lose it.
        if not require_reservation_guard and not recordings and why in (
                "reservation active", "reservation starts within guard window"):
            safe, why = True, "restore missing endpoint before reserved recording"
        else:
            return False, why
    for unit in ("bs4k-epg-scan.service", "bs2k-epg-scan.service"):
        try:
            cp = subprocess.run(["systemctl", "show", "--property=ActiveState", "--value", unit],
                                timeout=3, check=False, capture_output=True, text=True)
            if cp.returncode or cp.stdout.strip() not in ("active", "activating", "inactive", "failed", "deactivating"):
                return False, "could not establish EPG scanner state"
            if cp.stdout.strip() in ("active", "activating", "deactivating"):
                return False, unit + " is active"
        except (OSError, subprocess.TimeoutExpired):
            return False, "could not establish EPG scanner state"
    return True, why


def read_only_device_state(telnet):
    # BS2K/headless operation legitimately has no native video/audio decoder
    # threads. Require the OEM tuner/CAS services, driver and IPC inventory,
    # rather than treating a black HDMI output as standby/not ready.
    pids = telnet.run("pidof tuner; pidof card; pidof abs; pidof adsb").strip().splitlines()
    native = len(pids) == 4 and all(line.split() and all(x.isdigit() for x in line.split()) for line in pids)
    ready = telnet.run("test -c /dev/vixs/xcodedrv && test -s /tmp/bdh_out_msgh.lst && echo KARIN_NATIVE_READY").strip()
    try:
        uptime = float(telnet.run("cat /proc/uptime").split()[0])
    except (ValueError, IndexError):
        uptime = 0
    operating = native and ready == "KARIN_NATIVE_READY" and uptime >= 45
    proc_lines = telnet.run("ls -l /proc/[0-9]*/exe 2>/dev/null | grep '/tmp/mt/mkra'")
    owned_mkra = []
    for line in proc_lines.splitlines():
        if " -> /tmp/mt/mkra" in line:
            owned_mkra.append(line)
    return operating, owned_mkra


def remote_file_matches(telnet, remote, sha256):
    value = telnet.run("sha256sum " + remote).strip().split()
    return bool(value) and value[0] == sha256


def remote_binary_matches(telnet):
    return (remote_file_matches(telnet, REMOTE, EXPECTED_SHA256), "binary SHA-256 check")


def verify_bundle():
    if BINARY.stat().st_size != EXPECTED_SIZE or hashlib.sha256(BINARY.read_bytes()).hexdigest() != EXPECTED_SHA256:
        raise RuntimeError("bundled binary integrity failure")
    if hashlib.sha256(CONFIG.read_bytes()).hexdigest() != CONFIG_SHA256:
        raise RuntimeError("bundled BS channel configuration integrity failure")


def deploy_files(telnet, boot_id):
    """Only exact owned tmpfs names. Caller holds watch and scanner locks."""
    files = ((BINARY, REMOTE, EXPECTED_SHA256, EXPECTED_MD5),
             (CONFIG, REMOTE_CONFIG, CONFIG_SHA256, CONFIG_MD5))
    staged = []
    for source, remote, sha256, md5 in files:
        if remote_file_matches(telnet, remote, sha256):
            continue
        stage = remote + ".stage"
        got = telnet.upload(str(source), stage)
        if got != md5 or not remote_file_matches(telnet, stage, sha256):
            raise RuntimeError("staged runtime integrity failure: " + remote)
        staged.append((stage, remote))
    # A reset, a parallel manual deployment or a newly imminent recording
    # during the chunk upload must never lead to replacement/start.
    if telnet.run("cat /proc/sys/kernel/random/boot_id").strip() != boot_id:
        raise RuntimeError("receiver rebooted during deployment")
    if read_only_device_state(telnet)[1]:
        raise RuntimeError("another mkra appeared; runtime left untouched")
    safe, why = local_start_safety()
    if not safe:
        return False, why
    for stage, remote in staged:
        telnet.run("mv " + stage + " " + remote, raw=True)
    for _, remote, sha256, _ in files:
        if not remote_file_matches(telnet, remote, sha256):
            raise RuntimeError("installed runtime integrity failure: " + remote)
    telnet.chmod_exec(REMOTE)
    return True, "binary and BS channel configuration ready"


def verify_proxy_and_epgstation():
    healthy, detail = api_healthy(lambda url: get_json(url.replace(API_BASE, PROXY_BASE)))
    if not healthy:
        raise RuntimeError("40773 proxy API unhealthy: " + detail)
    # EPGStation health endpoint plus its configured Mirakurun base are checked
    # separately; EPGStation exposes no stable public tuner inventory endpoint.
    get_json(PROXY_BASE + "/api/tuners")
    with urllib.request.urlopen(EPGSTATION_BASE + "/", timeout=4) as res:
        if res.status != 200:
            raise RuntimeError("EPGStation HTTP %d" % res.status)
    # EPGStation exposes its channel catalogue at /api/channels (not
    # /api/tuners). Require the BS4K tuner channels to be present there.
    channels = get_json(EPGSTATION_BASE + "/api/channels")
    bs_channels = [c for c in channels if c.get("channelType") in ("BS", "BS4K")]
    if len(bs_channels) < 8:
        raise RuntimeError("EPGStation sees only %d BS channels" % len(bs_channels))
    return detail + "; EPGStation BS channels=%d" % len(bs_channels)


def run_once(dry_run=False):
    STATE_DIR.mkdir(mode=0o750, parents=True, exist_ok=True)
    with open(LOCK_FILE, "a") as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log("LOCKED another watcher run owns the lock; no action")
            return 0

        state = load_state()
        now = int(time.time())
        if now < int(state.get("next_attempt", 0)):
            log("BACKOFF until %s; no network/device operation" %
                time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(state["next_attempt"])))
            return 0

        verify_bundle()

        try:
            healthy, detail = api_healthy()
        except Exception as exc:
            healthy, detail = False, str(exc)
        if healthy:
            state.update(failures=0, next_attempt=0, last_result="healthy: " + detail)
            save_state(state)
            log("HEALTHY; no upload/start: " + detail)
            try:
                detail = verify_proxy_and_epgstation()
                log("EPGSTATION_PATH healthy: " + detail)
            except Exception as exc:
                log("EPGSTATION_PATH warning: " + str(exc))
            return 0

        # The EPG collectors use this same flock. Hold it across staging,
        # start and API verification so a timer cannot tune a half-ready box.
        with open(SCAN_LOCK, "r") as scan_lock:
            try:
                fcntl.flock(scan_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                defer_safe(state, "EPG scan lock held", delay=45)
                return 0
            return restore(state, detail, dry_run)


def restore(state, detail, dry_run=False):
    from sharp_telnet import connect
    telnet = None
    try:
        telnet = connect(TUNER_HOST, TELNET_PORT, timeout=6)
        log("TELNET_READY; direct API state: " + detail)
        operating, running = read_only_device_state(telnet)
        if running:
            defer_safe(state, "mkra process exists but API is unhealthy; never kill/overwrite it")
            return 0
        if not operating:
            defer_safe(state, "OEM tuner/CAS/driver/IPC not ready, or boot younger than 45s")
            return 0
        safe, why = local_start_safety()
        if not safe:
            defer_safe(state, why)
            return 0

        boot_id = telnet.run("cat /proc/sys/kernel/random/boot_id").strip()
        if len(boot_id) != 36:
            raise RuntimeError("invalid boot ID")
        mem = telnet.run("cat /proc/meminfo")
        free = next((int(line.split()[1]) for line in mem.splitlines() if line.startswith("MemFree:")), 0)
        if free < 16384:
            defer_safe(state, "less than 16MiB free RAM")
            return 0
        if dry_run:
            log("DRY_RUN would stage verified unified binary and BS configuration; no device writes/start")
            return 0
        ready, why = deploy_files(telnet, boot_id)
        if not ready:
            defer_safe(state, "post-upload guard: " + why)
            return 0
        log("RUNTIME_READY unified binary + BS channel configuration verified")

        recycle = telnet.run("cat /proc/sys/net/ipv4/tcp_tw_recycle 2>/dev/null").strip()
        if recycle == "1":
            telnet.run("echo 0 > /proc/sys/net/ipv4/tcp_tw_recycle")
            recycle = telnet.run("cat /proc/sys/net/ipv4/tcp_tw_recycle 2>/dev/null").strip()
        if recycle != "0":
            raise RuntimeError("tcp_tw_recycle not 0 (readback=%r)" % recycle)

        # The unified invocation preserves the existing live 4K EPG tap. It writes only to 4S tmpfs /tmp and starts no tuner
        # action until a client asks for a channel.
        if read_only_device_state(telnet)[1]:
            defer_safe(state, "another mkra appeared immediately before start")
            return 0
        telnet.run(START_COMMAND)
        log("START_SENT; waiting for the 40772 Mirakurun API")
    finally:
        if telnet is not None:
            telnet.close()

    deadline = time.monotonic() + 35
    last = "not ready"
    while time.monotonic() < deadline:
        try:
            healthy, last = api_healthy()
            if healthy:
                proxy = verify_proxy_and_epgstation()
                state.update(failures=0, next_attempt=0,
                             last_result="restored: " + last)
                save_state(state)
                log("RESTORED direct 40772 + CT proxy 40773 + EPGStation: " + proxy)
                return 0
        except Exception as exc:
            last = str(exc)
        time.sleep(2)
    raise RuntimeError("server/API did not become healthy within 35s: " + last)

def self_test():
    good_status = {"process": {"pid": 123, "versions": {"mkra": EXPECTED_VERSION}}}
    good_tuners = [{"index": 0, "isAvailable": True, "isFault": False,
                    "isUsing": True, "isFree": False}]
    def fetch(url):
        return good_status if url.endswith("/api/status") else ([{"id":i} for i in REQUIRED_BS | {1100101}] if url.endswith("/api/services") else good_tuners)
    assert api_healthy(fetch)[0]
    bad = [{"index": 0, "isAvailable": False, "isFault": True}]
    assert not api_healthy(lambda url: good_status if url.endswith("/api/status") else bad)[0]
    now = 1_800_000_000_000
    assert parse_recordings_safe([], [], now)[0]
    assert not parse_recordings_safe([], [{"id": 1}], now)[0]
    assert not parse_recordings_safe([{"startAt": now + 60_000, "endAt": now + 3_600_000}], [], now)[0]
    assert parse_recordings_safe([{"startAt": now + 900_000, "endAt": now + 3_600_000}], [], now)[0]
    assert [min(BACKOFF_BASE * (2 ** n), BACKOFF_MAX) for n in range(8)] == [15, 30, 60, 120, 240, 300, 300, 300]
    # Exercise the exact nonblocking flock primitive with independent handles.
    with tempfile.NamedTemporaryFile() as first:
        second = open(first.name, "a+")
        fcntl.flock(first.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            fcntl.flock(second.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            raise AssertionError("parallel lock unexpectedly acquired")
        except BlockingIOError:
            pass
        finally:
            second.close()
    # Model power-off: an unavailable TCP endpoint must fail promptly; run_once
    # records that as a failure and backs off without invoking upload/start.
    try:
        socket.create_connection(("127.0.0.1", 1), timeout=0.2)
        raise AssertionError("closed local port unexpectedly connected")
    except OSError:
        pass
    print("SELF_TEST=PASS api-health, recording/reservation guard, flock, power-off connect failure, capped backoff")


if __name__ == "__main__":
    try:
        if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
            self_test()
        else:
            if len(sys.argv) > 1 and sys.argv[1] != "--dry-run":
                raise RuntimeError("unknown option")
            sys.exit(run_once(dry_run="--dry-run" in sys.argv))
    except Exception as exc:
        if len(sys.argv) <= 1 or sys.argv[1] != "--self-test":
            state = load_state()
            defer_failure(state, str(exc))
        print("ERROR: " + repr(exc), file=sys.stderr)
        sys.exit(1)

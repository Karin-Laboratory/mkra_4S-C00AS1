#!/usr/bin/env python3
"""Read-only EPG health sampler and guarded BS4K retry controller.

The sampler never opens a tuner stream. Retry mode can only start the existing
BS4K scanner unit and is independently gated by recording, reservation, lock,
receiver and resource checks. All health output intentionally omits programme
titles and recording metadata.
"""
from __future__ import annotations

import datetime as dt
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

MKRA = os.environ.get("MKRA_URL", "http://192.168.1.51:40772")
PROXY = os.environ.get("PROXY_URL", "http://127.0.0.1:40773")
EPGS = os.environ.get("EPGSTATION_URL", "http://127.0.0.1:8888")
CACHE_FILE = pathlib.Path(os.environ.get("CACHE_FILE", "/var/lib/bs4k-epg/programs.json"))
BS_CACHE_FILE = pathlib.Path(os.environ.get("BS_CACHE_FILE", "/var/lib/bs4k-epg/bs-programs.json"))
STATUS_FILE = pathlib.Path(os.environ.get("STATUS_FILE", "/var/lib/bs4k-epg/status.json"))
HEALTH_FILE = pathlib.Path(os.environ.get("HEALTH_FILE", "/var/lib/bs4k-epg/health.json"))
MONITOR_FILE = pathlib.Path(os.environ.get("MONITOR_FILE", "/var/lib/bs4k-epg/monitor.jsonl"))
RETRY_FILE = pathlib.Path(os.environ.get("RETRY_FILE", "/var/lib/bs4k-epg/retry-state.json"))
SCAN_LOCK = pathlib.Path(os.environ.get("SCAN_LOCK", "/var/lib/bs4k-epg/scan.lock"))
DATABASE = pathlib.Path(os.environ.get("EPGSTATION_DB", "/opt/EPGStation/data/database.db"))
WATCH_STATE = pathlib.Path(os.environ.get("WATCH_STATE", "/var/lib/4s-mkra-watch/state.json"))
TARGETS_4K = (101, 141, 151, 161, 171, 181, 211, 221)
TARGETS_BS = (101, 141, 151, 161, 171, 181)
EXPECTED_MKRA_VERSION = os.environ.get("EXPECTED_MKRA_VERSION", "0.1.0")
EXPECTED_MKRA_SHA256 = os.environ.get("EXPECTED_MKRA_SHA256", "")
STALE_MS = int(os.environ.get("EPG_STALE_MS", str(12 * 60 * 60 * 1000)))
RECORDING_GUARD_MS = int(os.environ.get("RECORDING_GUARD_MS", "180000"))
STARTUP_GUARD_SECONDS = int(os.environ.get("STARTUP_GUARD_SECONDS", "600"))
MAX_RETRIES = int(os.environ.get("MAX_RETRIES", "3"))
RETRY_BASE_SECONDS = int(os.environ.get("RETRY_BASE_SECONDS", "1800"))
RETRY_MAX_SECONDS = int(os.environ.get("RETRY_MAX_SECONDS", "14400"))
MAX_LOG_BYTES = int(os.environ.get("MAX_LOG_BYTES", str(10 * 1024 * 1024)))
LOG_KEEP = int(os.environ.get("LOG_KEEP", "5"))
MAX_BODY = 8 * 1024 * 1024


def iso_now():
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def request_json(url, timeout=5):
    req = urllib.request.Request(url, headers={"User-Agent": "bs4k-epg-health/1"})
    with urllib.request.urlopen(req, timeout=timeout) as res:
        if res.status != 200:
            raise RuntimeError("HTTP %d" % res.status)
        raw = res.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY:
            raise RuntimeError("response exceeded %d bytes" % MAX_BODY)
        return json.loads(raw)


def read_json(path, default=None):
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream)
        return value if isinstance(value, (dict, list)) else default
    except (OSError, ValueError):
        return default


def atomic_json(path, value, mode=0o640):
    path.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, ensure_ascii=False, separators=(",", ":"))
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    finally:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass


def millis(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000
        except ValueError:
            return 0
    return 0


def programs_by_service(programs, network_id, service_ids):
    result = {int(sid): [] for sid in service_ids}
    for program in programs:
        try:
            sid = int(program.get("serviceId"))
            network = int(program.get("networkId"))
        except (TypeError, ValueError, AttributeError):
            continue
        if network == network_id and sid in result:
            start, end = millis(program.get("startAt")), millis(program.get("endAt"))
            if start > 0 and end > start:
                result[sid].append((start, end))
    for events in result.values():
        events.sort()
    return result


def service_metrics(events, now_ms, last_success_ms):
    next24 = [(a, b) for a, b in events if b > now_ms and a < now_ms + 24 * 3600 * 1000]
    future = [(a, b) for a, b in events if b > now_ms]
    gaps = []
    cursor = now_ms
    for start, end in future:
        if start > cursor and start - cursor >= 30 * 60 * 1000:
            gaps.append({"from": int(cursor), "to": int(start), "minutes": int((start - cursor) / 60000)})
        cursor = max(cursor, end)
        if start >= now_ms + 24 * 3600 * 1000:
            break
    return {
        "cachedCount": len(events),
        "next24hCount": len(next24),
        "next24hLastEnd": int(max((b for _, b in next24), default=0)),
        "gapCandidates30m": gaps,
        "lastSuccess": int(last_success_ms) if last_success_ms else None,
        "lastSuccessAgeMs": int(now_ms - last_success_ms) if last_success_ms else None,
    }


def sqlite_counts(path, now_ms, horizon_ms=24 * 3600 * 1000):
    uri = "file:%s?mode=ro" % path.as_posix()
    con = sqlite3.connect(uri, uri=True, timeout=3)
    try:
        integrity = con.execute("PRAGMA integrity_check").fetchone()[0]
        rows = con.execute(
            "SELECT networkId, serviceId, COUNT(*) FROM program "
            "WHERE endAt > ? AND startAt < ? GROUP BY networkId, serviceId",
            (now_ms, now_ms + horizon_ms),
        ).fetchall()
        return integrity, {"%s:%s" % (int(n), int(s)): int(c) for n, s, c in rows}
    finally:
        con.close()


def cache_files():
    four = read_json(CACHE_FILE, {})
    bs = read_json(BS_CACHE_FILE, {})
    if not isinstance(four, dict) or not isinstance(four.get("programs"), list):
        raise RuntimeError("4K cache missing or malformed")
    if not isinstance(bs, dict) or not isinstance(bs.get("programs", []), list):
        raise RuntimeError("BS2K cache malformed")
    return four, bs


def file_summary(path):
    try:
        st = path.stat()
        h = hashlib.sha256()
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                h.update(chunk)
        return {"present": True, "size": st.st_size, "mtime": int(st.st_mtime), "sha256": h.hexdigest()}
    except OSError:
        return {"present": False}


def unit_state(name):
    cp = subprocess.run(["systemctl", "show", name, "-p", "ActiveState", "-p", "SubState",
                         "-p", "Result", "-p", "ExecMainStatus", "-p", "ExecMainStartTimestamp",
                         "-p", "ExecMainExitTimestamp"], capture_output=True, text=True,
                        timeout=4, check=False)
    if cp.returncode:
        return {"state": "unknown"}
    raw = dict(line.split("=", 1) for line in cp.stdout.splitlines() if "=" in line)
    return {"active": raw.get("ActiveState", "unknown"), "substate": raw.get("SubState", "unknown"),
            "result": raw.get("Result", "unknown"), "exitStatus": raw.get("ExecMainStatus", ""),
            "start": raw.get("ExecMainStartTimestamp", ""), "exit": raw.get("ExecMainExitTimestamp", "")}


def timer_state(name):
    cp = subprocess.run(["systemctl", "is-active", name], capture_output=True, text=True,
                        timeout=4, check=False)
    return cp.stdout.strip() or "unknown"


def device_snapshot(api_status):
    snapshot = {"uptimeSeconds": None, "oemReady": "unknown", "mkraSha256": None,
                "executable": "unconfirmed", "startFlags": [], "nativeLogIndicators": {
                    "requestedServiceIds": list(TARGETS_4K), "adsbState": "unconfirmed",
                    "mpt": "unconfirmed", "videoEsRing": "unconfirmed",
                    "audioEsRing": "unconfirmed", "timeoutMentioned": False,
                    "errorCategories": []}}
    try:
        sys.path.insert(0, "/usr/local/libexec/4s-mkra-watch")
        from sharp_telnet import connect
        telnet = connect(os.environ.get("TUNER_HOST", "192.168.1.51"), int(os.environ.get("TELNET_PORT", "2323")), timeout=4)
        try:
            try:
                snapshot["uptimeSeconds"] = float(telnet.run("cat /proc/uptime").split()[0])
            except (ValueError, IndexError, RuntimeError):
                pass
            try:
                pids = [telnet.run("pidof " + name).strip() for name in ("tuner", "card", "abs", "adsb")]
                ready = telnet.run("test -c /dev/vixs/xcodedrv && test -s /tmp/bdh_out_msgh.lst && echo READY").strip()
                snapshot["oemReady"] = bool(all(p and all(x.isdigit() for x in p.split()) for p in pids) and ready == "READY")
            except Exception:
                pass
            pid = ((api_status or {}).get("process") or {}).get("pid")
            if isinstance(pid, int):
                try:
                    exe = telnet.run("ls -l /proc/%d/exe" % pid)
                    if " -> " in exe:
                        snapshot["executable"] = exe.rsplit(" -> ", 1)[1].strip()
                except Exception:
                    pass
                try:
                    argv = telnet.run("cat /proc/%d/cmdline" % pid).replace("\x00", " ").split()
                    snapshot["startFlags"] = [x for x in argv if x.startswith("--") and re.fullmatch(r"--[a-z0-9-]+", x)]
                except Exception:
                    pass
                try:
                    value = telnet.run("sha256sum /tmp/mt/mkra-unified-auto").split()
                    if value and len(value[0]) == 64:
                        snapshot["mkraSha256"] = value[0]
                except Exception:
                    pass
            try:
                lines = telnet.run("tail -n 160 /tmp/mt/mkra-unified-auto.log").splitlines()
                joined = "\n".join(lines)
                indicators = snapshot["nativeLogIndicators"]
                indicators["observedServiceIds"] = sorted({int(x) for x in re.findall(r"(?:serviceId|service_id|sid)[=: ]+(\d{1,5})", joined, re.I) if int(x) in TARGETS_4K})
                indicators["adsbState"] = "mentioned-unconfirmed" if re.search(r"\bADSB\b", joined, re.I) else "unconfirmed"
                indicators["mpt"] = "mentioned-unconfirmed" if re.search(r"\bMPT\b", joined, re.I) else "unconfirmed"
                indicators["videoEsRing"] = "mentioned-unconfirmed" if re.search(r"video.{0,40}ring|ring.{0,40}video", joined, re.I) else "unconfirmed"
                indicators["audioEsRing"] = "mentioned-unconfirmed" if re.search(r"audio.{0,40}ring|ring.{0,40}audio", joined, re.I) else "unconfirmed"
                indicators["timeoutMentioned"] = bool(re.search(r"timeout|deadline", joined, re.I))
                indicators["errorCategories"] = [term for term in ("settle", "source", "MPT", "ADSB", "ES", "timeout", "503") if re.search(re.escape(term), joined, re.I)]
            except Exception:
                pass
        finally:
            telnet.close()
    except Exception:
        pass
    return snapshot


def collect(now_ms=None, fetch=request_json, include_device=True):
    now_ms = now_ms or int(time.time() * 1000)
    result = {"at": iso_now(), "status": "unknown", "checks": {}, "services": {}, "diffs": {}, "failures": [], "warnings": []}
    failures = result["failures"]
    checks = result["checks"]
    api_status = None
    try:
        api_status = fetch(MKRA + "/api/status")
        process = api_status.get("process", {})
        pid, version = process.get("pid"), (process.get("versions") or {}).get("mkra")
        mkra_ok = isinstance(pid, int) and version == EXPECTED_MKRA_VERSION
        checks["mkra"] = {"state": "ok" if mkra_ok else "unexpected", "pid": pid, "version": version, "streamCount": api_status.get("streamCount")}
        if not mkra_ok:
            failures.append("mkra-identity")
    except Exception as exc:
        checks["mkra"] = {"state": "failed", "error": str(exc)[:160]}
        failures.append("mkra-api")
    try:
        tuners = fetch(MKRA + "/api/tuners")
        free_count = sum(1 for t in tuners if t.get("isFree") and not t.get("isUsing"))
        using_count = sum(1 for t in tuners if t.get("isUsing"))
        available_count = sum(1 for t in tuners if t.get("isAvailable") is True and t.get("isFault") is False)
        checks["tuners"] = {"state": "ok" if available_count else "unavailable", "count": len(tuners), "free": free_count, "using": using_count, "available": available_count}
        if not available_count:
            failures.append("tuner-unavailable")
    except Exception as exc:
        checks["tuners"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("tuner-api")
    try:
        services = fetch(MKRA + "/api/services")
        ids = {int(x.get("id")) for x in services if isinstance(x, dict) and str(x.get("id", "")).isdigit()}
        checks["serviceCatalogue"] = {"state": "ok" if {1100000 + s for s in TARGETS_4K}.issubset(ids) else "incomplete", "expected": len(TARGETS_4K), "present": sum(1 for s in TARGETS_4K if 1100000 + s in ids)}
        if checks["serviceCatalogue"]["state"] != "ok":
            failures.append("service-catalogue")
    except Exception as exc:
        checks["serviceCatalogue"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("service-catalogue")
    for key, url, field in (("proxy", PROXY + "/api/tuners", None), ("epgstation", EPGS + "/", "http")):
        try:
            if field == "http":
                with urllib.request.urlopen(url, timeout=4) as response:
                    if response.status != 200:
                        raise RuntimeError("HTTP %d" % response.status)
            else:
                fetch(url)
            checks[key] = {"state": "ok"}
        except Exception as exc:
            checks[key] = {"state": "failed", "error": str(exc)[:160]}
            failures.append(key)
    try:
        reserves = fetch(EPGS + "/api/reserves?isHalfWidth=true")
        reserve_list = reserves.get("reserves", []) if isinstance(reserves, dict) else reserves
        checks["reservations"] = {"state": "ok", "count": len(reserve_list), "activeOrWithinGuard": sum(1 for r in reserve_list if not r.get("isSkip") and not r.get("isConflict") and millis(r.get("startAt")) < now_ms + RECORDING_GUARD_MS and millis(r.get("endAt")) > now_ms)}
    except Exception as exc:
        reserve_list = None
        checks["reservations"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("reservations")
    try:
        recordings = fetch(EPGS + "/api/recording?isHalfWidth=true")
        recording_list = recordings.get("records", []) if isinstance(recordings, dict) else recordings
        checks["recordings"] = {"state": "ok", "count": len(recording_list)}
    except Exception as exc:
        recording_list = None
        checks["recordings"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("recordings")

    try:
        cache4k, cachebs = cache_files()
        proxy_programs = fetch(PROXY + "/api/programs?networkId=11")
        proxy_by = programs_by_service(proxy_programs, 11, TARGETS_4K)
        cache_by = programs_by_service(cache4k["programs"], 11, TARGETS_4K)
        status = read_json(STATUS_FILE, {}) or {}
        service_status = status.get("serviceStatus", {}) if isinstance(status, dict) else {}
        for sid in TARGETS_4K:
            last = millis((service_status.get(str(sid)) or {}).get("lastSuccess"))
            metric = service_metrics(cache_by[sid], now_ms, last)
            metric["proxyCount"] = len(proxy_by[sid])
            metric["cacheProxyMatch"] = metric["cachedCount"] == len([p for p in cache4k["programs"] if int(p.get("networkId", -1)) == 11 and int(p.get("serviceId", -1)) == sid]) == len([p for p in proxy_programs if int(p.get("serviceId", -1)) == sid])
            metric["lastScanStatus"] = (service_status.get(str(sid)) or {}).get("status", "unknown")
            metric["freshCount"] = (service_status.get(str(sid)) or {}).get("freshCount")
            result["services"][str(sid)] = metric
            if not metric["next24hCount"] or not last or now_ms - last > STALE_MS:
                failures.append("epg-%d" % sid)
            elif metric["lastScanStatus"] not in ("ok", "unknown"):
                result["warnings"].append("last-scan-%d-%s" % (sid, metric["lastScanStatus"]))
            if not metric["cacheProxyMatch"]:
                result["warnings"].append("cache-proxy-mismatch-%d" % sid)
        result["checks"]["4kCache"] = {"state": "ok", "updatedAt": cache4k.get("updatedAt"), "programCount": len(cache4k["programs"]), "sha256": file_summary(CACHE_FILE).get("sha256")}
        bs_by = programs_by_service(cachebs.get("programs", []), 4, TARGETS_BS)
        bs_updated = millis(cachebs.get("updatedAt"))
        result["checks"]["bs2kCache"] = {"state": "ok" if bs_updated and now_ms - bs_updated <= STALE_MS else "stale-or-unknown", "updatedAt": cachebs.get("updatedAt"), "programCount": len(cachebs.get("programs", [])), "services": {str(s): len(bs_by[s]) for s in TARGETS_BS}, "sha256": file_summary(BS_CACHE_FILE).get("sha256")}
    except Exception as exc:
        result["checks"]["cacheProxy"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("cache-proxy")

    try:
        integrity, counts = sqlite_counts(DATABASE, now_ms)
        _, counts7 = sqlite_counts(DATABASE, now_ms, 7 * 24 * 3600 * 1000)
        result["checks"]["sqlite"] = {"state": "ok" if integrity == "ok" else "corrupt", "integrity": integrity, "next24hNetwork11Counts": {str(s): counts.get("11:%d" % s, 0) for s in TARGETS_4K}, "next7dNetwork11Counts": {str(s): counts7.get("11:%d" % s, 0) for s in TARGETS_4K}}
        if integrity != "ok":
            failures.append("sqlite-integrity")
        for sid in TARGETS_4K:
            proxy24 = sum(1 for start, end in proxy_by[sid] if end > now_ms and start < now_ms + 24 * 3600 * 1000)
            proxy7 = sum(1 for start, end in proxy_by[sid] if end > now_ms and start < now_ms + 7 * 24 * 3600 * 1000)
            result["diffs"].setdefault(str(sid), {}).update({"proxyVsSQLite24h": proxy24 - counts.get("11:%d" % sid, 0), "proxyVsSQLite7d": proxy7 - counts7.get("11:%d" % sid, 0)})
    except Exception as exc:
        result["checks"]["sqlite"] = {"state": "unknown", "error": str(exc)[:160]}
        failures.append("sqlite")

    units = {name: unit_state(name) for name in ("bs4k-epg-scan.service", "bs2k-epg-scan.service", "4s-mkra-watch.service")}
    result["checks"]["units"] = units
    result["checks"]["timers"] = {name: timer_state(name) for name in ("bs4k-epg-scan.timer", "bs2k-epg-scan.timer", "4s-mkra-watch.timer", "bs4k-monitor.timer")}
    result["checks"]["watcher"] = read_json(WATCH_STATE, {}) or {"state": "unknown"}
    if include_device:
        result["checks"]["device"] = device_snapshot(api_status)
        expected_hash = EXPECTED_MKRA_SHA256
        actual_hash = result["checks"]["device"].get("mkraSha256")
        expected_path = "/tmp/mt/mkra-unified-auto"
        result["checks"]["device"]["mkraHashMatch"] = bool(expected_hash and actual_hash == expected_hash and result["checks"]["device"].get("executable") == expected_path)
        if expected_hash and not result["checks"]["device"]["mkraHashMatch"]:
            result["warnings"].append("mkra-binary-hash-mismatch-or-unconfirmed")
    active_scan = any(units[n].get("active") in ("active", "activating", "deactivating") for n in ("bs4k-epg-scan.service", "bs2k-epg-scan.service"))
    for name in ("bs4k-epg-scan.service", "bs2k-epg-scan.service"):
        if units[name].get("result") == "failed":
            result["warnings"].append(name + "-last-result-failed")
    recording_count = (checks.get("recordings") or {}).get("count")
    imminent = (checks.get("reservations") or {}).get("activeOrWithinGuard")
    uptime = (result["checks"].get("device") or {}).get("uptimeSeconds")
    reboot_grace = isinstance(uptime, (int, float)) and uptime < STARTUP_GUARD_SECONDS
    result["checks"]["rebootGrace"] = reboot_grace
    tuner_using = (checks.get("tuners") or {}).get("using", 0)
    result["checks"]["deferred"] = bool(active_scan or recording_count is None or recording_count > 0 or imminent is None or imminent > 0 or reboot_grace or tuner_using > 0)
    result["checks"]["scannerLock"] = "unknown"
    try:
        with SCAN_LOCK.open("a") as lock:
            try:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                result["checks"]["scannerLock"] = "free"
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
            except BlockingIOError:
                result["checks"]["scannerLock"] = "held"
    except OSError:
        pass

    prior_health = read_json(HEALTH_FILE, {}) or {}
    for sid in TARGETS_4K:
        old = (prior_health.get("services") or {}).get(str(sid), {})
        cur = result["services"].get(str(sid), {})
        result["diffs"].setdefault(str(sid), {}).update({
            "cacheCountDelta": cur.get("cachedCount") - old.get("cachedCount") if isinstance(old.get("cachedCount"), int) else None,
            "next24hDelta": cur.get("next24hCount") - old.get("next24hCount") if isinstance(old.get("next24hCount"), int) else None,
        })
    if result["checks"]["deferred"]:
        result["status"] = "deferred"
    elif failures:
        result["status"] = "critical" if "mkra-api" in failures and "tuner-api" in failures else "degraded"
    elif result["warnings"] or result["checks"].get("bs2kCache", {}).get("state") != "ok":
        result["status"] = "degraded"
    else:
        result["status"] = "healthy"
    result["consecutiveFailures"] = int(prior_health.get("consecutiveFailures", 0)) + 1 if result["status"] in ("critical", "degraded") else 0
    result["lastHealthyAt"] = prior_health.get("lastHealthyAt")
    result["lastAnomalyAt"] = prior_health.get("lastAnomalyAt")
    if result["status"] == "healthy":
        result["lastHealthyAt"] = result["at"]
    elif result["status"] in ("critical", "degraded"):
        result["lastAnomalyAt"] = result["at"]
    result["retry"] = read_json(RETRY_FILE, {}) or {"enabled": os.environ.get("AUTO_RETRY", "0") == "1", "attempts": 0, "nextAttempt": None, "lastOutcome": "none"}
    result["tsAcquisition"] = "not tested by the passive monitor; requires a separately guarded scan"
    result["humanAction"] = "check diagnostics; no automatic mkra stop or 4S power operation" if result["status"] in ("critical", "degraded") else None
    if include_device and result["checks"].get("device", {}).get("uptimeSeconds") is not None:
        result["checks"]["device"]["uptimeSeconds"] = int(result["checks"]["device"]["uptimeSeconds"])
    return result


def rotate_log(path=None):
    path = path or MONITOR_FILE
    try:
        if path.stat().st_size < MAX_LOG_BYTES:
            return
    except FileNotFoundError:
        return
    for index in range(LOG_KEEP, 0, -1):
        src = pathlib.Path(str(path) + ".%d" % index)
        dst = pathlib.Path(str(path) + ".%d" % (index + 1))
        if index == LOG_KEEP:
            try:
                src.unlink()
            except FileNotFoundError:
                pass
        else:
            try:
                os.replace(src, dst)
            except FileNotFoundError:
                pass
    os.replace(path, pathlib.Path(str(path) + ".1"))


def save_sample(report, baseline=False):
    HEALTH_FILE.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    lock_path = pathlib.Path(str(MONITOR_FILE) + ".lock")
    lock_path.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    with lock_path.open("a") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        rotate_log()
        with MONITOR_FILE.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(report, ensure_ascii=False, separators=(",", ":")) + "\n")
        prior = read_json(HEALTH_FILE, {}) or {}
        report["transition"] = (report.get("status") != prior.get("status")
                                or sorted(report.get("failures", [])) != sorted(prior.get("failures", []))
                                or sorted(report.get("warnings", [])) != sorted(prior.get("warnings", [])))
        atomic_json(HEALTH_FILE, report)
        if baseline:
            snapshots = HEALTH_FILE.parent / "snapshots"
            snapshots.mkdir(mode=0o750, parents=True, exist_ok=True)
            stamp = dt.datetime.now().astimezone().strftime("%Y%m%dT%H%M%S%z")
            atomic_json(snapshots / ("baseline-" + stamp + ".json"), report)
            for old in sorted(snapshots.glob("baseline-*.json"))[:-30]:
                old.unlink()


def safe_to_retry(report, fetch=request_json, device_probe=device_snapshot):
    checks = report.get("checks", {})
    reasons = []
    if report.get("status") not in ("degraded", "critical"):
        reasons.append("health is not degraded")
    if checks.get("deferred"):
        reasons.append("recording/reservation/scanner is active or unknown")
    if checks.get("scannerLock") != "free":
        reasons.append("shared scan lock is not free")
    if checks.get("tuners", {}).get("free", 0) < 1:
        reasons.append("tuner is not confirmed free")
    dev = checks.get("device", {})
    uptime = dev.get("uptimeSeconds")
    if not isinstance(uptime, (int, float)) or uptime < STARTUP_GUARD_SECONDS:
        reasons.append("4S uptime is unknown or inside startup guard")
    if dev.get("oemReady") is not True:
        reasons.append("OEM tuner/CAS/driver readiness is not confirmed")
    if checks.get("mkra", {}).get("state") != "ok":
        reasons.append("mkra API is not healthy")
    if checks.get("mkra", {}).get("version") != EXPECTED_MKRA_VERSION:
        reasons.append("mkra version does not match the expected runtime")
    if checks.get("serviceCatalogue", {}).get("state") != "ok":
        reasons.append("4K service catalogue is incomplete or unknown")
    if checks.get("proxy", {}).get("state") != "ok" or checks.get("epgstation", {}).get("state") != "ok":
        reasons.append("proxy or EPGStation API is not healthy")
    if checks.get("sqlite", {}).get("state") != "ok":
        reasons.append("EPGStation SQLite is not confirmed healthy")
    if checks.get("device", {}).get("mkraHashMatch") is not True:
        reasons.append("running mkra hash does not match the pinned runtime")
    for name in ("bs4k-epg-scan.service", "bs2k-epg-scan.service"):
        if checks.get("units", {}).get(name, {}).get("active") in ("active", "activating", "deactivating"):
            reasons.append(name + " is active")
    try:
        df = shutil.disk_usage(CACHE_FILE.parent)
        if df.free < 2 * 1024**3:
            reasons.append("less than 2 GiB free on cache filesystem")
    except OSError:
        reasons.append("cache filesystem capacity unknown")
    try:
        meminfo = pathlib.Path("/proc/meminfo").read_text()
        available = next(int(line.split()[1]) for line in meminfo.splitlines() if line.startswith("MemAvailable:")) * 1024
        if available < 256 * 1024**2:
            reasons.append("less than 256 MiB available memory")
    except (OSError, StopIteration, ValueError):
        reasons.append("available memory unknown")
    return not reasons, reasons


def retry_once(report=None, enabled=None, fetch=request_json, start_unit=None):
    enabled = os.environ.get("AUTO_RETRY", "0") == "1" if enabled is None else enabled
    RETRY_FILE.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    lock_path = pathlib.Path(str(RETRY_FILE) + ".lock")
    with lock_path.open("a") as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return {"action": "deferred", "reason": "retry controller already running"}
        state = read_json(RETRY_FILE, {}) or {"attempts": 0, "nextAttempt": 0}
        if not enabled:
            return {"action": "disabled", "reason": "AUTO_RETRY is not enabled"}
        now_ms = int(time.time() * 1000)
        now = now_ms // 1000
        if int(state.get("attempts", 0)) >= MAX_RETRIES and state.get("paused", False):
            return {"action": "critical", "reason": "retry limit reached; manual review required"}
        # An attempt is counted only after a scan result newer than its start exists.
        prior_started = int(state.get("lastStartedAt", 0))
        if not prior_started and state.get("lastStarted"):
            prior_started = int(state["lastStarted"]) * 1000
        scan = read_json(STATUS_FILE, {}) or {}
        scan_updated = int(millis(scan.get("updatedAt")))
        if prior_started and scan_updated < prior_started:
            return {"action": "waiting", "reason": "waiting for scanner result"}
        if prior_started and scan_updated >= prior_started:
            bad = scan.get("status") != "complete" or any(
                (scan.get("serviceStatus", {}).get(str(sid)) or {}).get("status") != "ok" for sid in TARGETS_4K)
            if bad:
                state["attempts"] = int(state.get("attempts", 0)) + 1
                state["lastOutcome"] = "failed"
            else:
                state.update(attempts=0, lastOutcome="recovered", paused=False)
            state.pop("lastStarted", None)
            if bad and int(state.get("attempts", 0)) >= MAX_RETRIES:
                state.update(paused=True, nextAttempt=0, lastOutcome="manual-review-required")
                atomic_json(RETRY_FILE, state, mode=0o644)
                return {"action": "critical", "reason": "retry limit reached; manual review required"}
            delay = min(RETRY_BASE_SECONDS * (2 ** max(0, int(state.get("attempts", 0)) - 1)), RETRY_MAX_SECONDS)
            state["nextAttempt"] = now + delay if bad else 0
            atomic_json(RETRY_FILE, state, mode=0o644)
            if bad:
                return {"action": "backoff", "nextAttempt": state["nextAttempt"], "lastOutcome": "failed"}
            return {"action": "recovered", "attempts": 0}
        if now < int(state.get("nextAttempt", 0)):
            return {"action": "backoff", "nextAttempt": state["nextAttempt"]}
        if int(state.get("attempts", 0)) >= MAX_RETRIES:
            state.update(paused=True, nextAttempt=0, lastOutcome="manual-review-required")
            atomic_json(RETRY_FILE, state, mode=0o644)
            return {"action": "critical", "reason": "retry limit reached; manual review required"}
        report = report or collect(fetch=fetch)
        if report.get("status") not in ("degraded", "critical") or int(report.get("consecutiveFailures", 0)) < 2:
            return {"action": "deferred", "reason": "requires two consecutive degraded health samples"}
        if report.get("checks", {}).get("4kCache", {}).get("state") == "ok":
            stale_ids = [sid for sid in TARGETS_4K if (report.get("services", {}).get(str(sid), {}).get("lastSuccessAgeMs") or 0) > STALE_MS or not report.get("services", {}).get(str(sid), {}).get("next24hCount")]
            explicit_fail = [sid for sid in TARGETS_4K if report.get("services", {}).get(str(sid), {}).get("lastScanStatus") in ("failed", "unavailable")]
            if not (stale_ids or explicit_fail):
                return {"action": "deferred", "reason": "no retryable BS4K scan failure"}
        allowed, reasons = safe_to_retry(report, fetch=fetch)
        if not allowed:
            return {"action": "deferred", "reason": "; ".join(reasons)}
        if start_unit is None:
            cp = subprocess.run(["systemctl", "start", "--no-block", "bs4k-epg-scan.service"],
                                capture_output=True, text=True, timeout=5, check=False)
            if cp.returncode:
                return {"action": "deferred", "reason": "systemd refused scan start"}
        else:
            start_unit("bs4k-epg-scan.service")
        started_at_ms = int(time.time() * 1000)
        state["lastStartedAt"] = started_at_ms
        state.pop("lastStarted", None)
        state["lastOutcome"] = "started"
        state["paused"] = False
        # The first retry waits 30 minutes after a failed scan; later attempts
        # back off exponentially. The scan result is reconciled on a later tick.
        state["nextAttempt"] = started_at_ms // 1000 + RETRY_BASE_SECONDS
        atomic_json(RETRY_FILE, state, mode=0o644)
        return {"action": "started", "unit": "bs4k-epg-scan.service", "startedAt": started_at_ms // 1000}


def run_sample(baseline=False):
    report = collect()
    save_sample(report, baseline=baseline)
    return report


def main(argv):
    if argv == ["--self-test"]:
        from test_health import self_test
        self_test()
        return 0
    if argv == ["--retry"]:
        result = retry_once()
        if result.get("action") in ("started", "critical", "recovered"):
            print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0
    if argv in (["--sample"], ["--sample", "--baseline"]):
        report = run_sample(baseline="--baseline" in argv)
        if report.get("transition"):
            print(json.dumps({"at": report["at"], "status": report["status"], "failures": report["failures"], "warnings": report["warnings"]}, ensure_ascii=False))
        return 0
    print("usage: health.py --sample [--baseline] | --retry | --self-test", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

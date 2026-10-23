#!/usr/bin/env python3
"""Deterministic mock tests for the passive EPG health monitor and retry gates."""
import importlib.util
import json
import pathlib
import sqlite3
import tempfile
import time
import types
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("health", pathlib.Path(__file__).with_name("health.py"))
health = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(health)


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(HealthTests)
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    if not result.wasSuccessful():
        raise SystemExit(1)


class HealthTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.patches = []
        for name, value in {
            "CACHE_FILE": self.root / "programs.json",
            "BS_CACHE_FILE": self.root / "bs-programs.json",
            "STATUS_FILE": self.root / "status.json",
            "HEALTH_FILE": self.root / "health.json",
            "MONITOR_FILE": self.root / "monitor.jsonl",
            "RETRY_FILE": self.root / "retry.json",
            "SCAN_LOCK": self.root / "scan.lock",
            "DATABASE": self.root / "db.sqlite",
            "WATCH_STATE": self.root / "watch.json",
            "LOG_KEEP": 2,
            "MAX_LOG_BYTES": 40,
            "TARGETS_4K": (101, 141),
            "TARGETS_BS": (101, 141),
            "STALE_MS": 12 * 60 * 60 * 1000,
            "RECORDING_GUARD_MS": 180000,
            "STARTUP_GUARD_SECONDS": 600,
            "MAX_RETRIES": 3,
            "RETRY_BASE_SECONDS": 1800,
            "RETRY_MAX_SECONDS": 14400,
        }.items():
            self.set(name, value)
        self.now = 1_800_000_000_000
        self.write_caches()
        self.make_db()
        self.status = {
            "updatedAt": self.now,
            "status": "complete",
            "serviceStatus": {
                str(s): {"status": "ok", "lastSuccess": self.now - 60000, "freshCount": 10}
                for s in health.TARGETS_4K
            },
        }
        health.atomic_json(health.STATUS_FILE, self.status)

    def tearDown(self):
        for p in reversed(self.patches):
            p.stop()
        self.tmp.cleanup()

    def set(self, name, value):
        p = patch.object(health, name, value)
        p.start()
        self.patches.append(p)

    def write_caches(self, events_by_sid=None, updated=None):
        events_by_sid = events_by_sid or {101: 10, 141: 10}
        programs = []
        for sid, n in events_by_sid.items():
            for i in range(n):
                start = self.now + i * 3600000
                programs.append({"networkId": 11, "serviceId": sid, "startAt": start, "endAt": start + 1800000})
        health.atomic_json(health.CACHE_FILE, {"updatedAt": updated or self.now, "programs": programs})
        bs = [{"networkId": 4, "serviceId": sid, "startAt": self.now + i * 3600000, "endAt": self.now + i * 3600000 + 1800000}
              for sid in health.TARGETS_BS for i in range(2)]
        health.atomic_json(health.BS_CACHE_FILE, {"updatedAt": updated or self.now, "programs": bs})

    def make_db(self, counts=None):
        con = sqlite3.connect(health.DATABASE)
        con.execute("DROP TABLE IF EXISTS program")
        con.execute("CREATE TABLE program (networkId INTEGER, serviceId INTEGER, startAt INTEGER, endAt INTEGER)")
        for sid, count in (counts or {101: 10, 141: 10}).items():
            for i in range(count):
                start = self.now + i * 3600000
                con.execute("INSERT INTO program VALUES (11,?,?,?)", (sid, start, start + 1800000))
        con.commit(); con.close()

    def good_fetch(self, url):
        if url.endswith("/api/status"):
            return {"process": {"pid": 123, "versions": {"mkra": "0.1.0"}, "memoryUsage": {}}, "streamCount": 0, "epg": {"storedEvents": 0}}
        if url.endswith("/api/tuners"):
            return [{"isFree": True, "isUsing": False, "isAvailable": True, "isFault": False}]
        if url.endswith("/api/services"):
            return [{"id": 1100000 + sid} for sid in health.TARGETS_4K]
        if url.endswith("/api/programs?networkId=11"):
            return health.read_json(health.CACHE_FILE)["programs"]
        if url.endswith("/api/reserves?isHalfWidth=true"):
            return {"reserves": []}
        if url.endswith("/api/recording?isHalfWidth=true"):
            return {"records": []}
        if url.endswith("/api/tuners") and "40773" in url:
            return [{"isFree": True}]
        raise AssertionError("unexpected URL " + url)

    def collect_ok(self, **overrides):
        values = {"fetch": self.good_fetch, "include_device": False, "now_ms": self.now}
        values.update(overrides)
        with patch.object(health, "unit_state", return_value={"active": "inactive", "result": "success"}), \
             patch.object(health, "timer_state", return_value="active"), \
             patch("urllib.request.urlopen", return_value=MockResponse()):
            return health.collect(**values)

    def test_01_placeholder_stored_events_zero_is_not_failure(self):
        report = self.collect_ok()
        self.assertNotIn("stored-events", report["failures"])
        self.assertEqual(report["status"], "healthy")

    def test_02_eight_station_catalogue_is_required(self):
        self.set("TARGETS_4K", (101, 141, 151, 161, 171, 181, 211, 221))
        self.write_caches({sid: 1 for sid in health.TARGETS_4K})
        self.make_db({sid: 1 for sid in health.TARGETS_4K})
        health.atomic_json(health.STATUS_FILE, {"updatedAt": self.now, "status": "complete", "serviceStatus": {str(s): {"status":"ok","lastSuccess":self.now} for s in health.TARGETS_4K}})
        report = self.collect_ok()
        self.assertEqual(report["checks"]["serviceCatalogue"]["present"], 8)

    def test_03_single_station_missing_is_degraded(self):
        self.write_caches({101: 10, 141: 0})
        self.status["serviceStatus"]["141"]["lastSuccess"] = self.now - health.STALE_MS - 1
        health.atomic_json(health.STATUS_FILE, self.status)
        self.assertIn("epg-141", self.collect_ok()["failures"])

    def test_04_multiple_station_missing_are_reported_individually(self):
        self.write_caches({101: 0, 141: 0})
        report = self.collect_ok()
        self.assertIn("epg-101", report["failures"])
        self.assertIn("epg-141", report["failures"])

    def test_05_proxy_cache_mismatch_is_warning_not_cache_mutation(self):
        report = self.collect_ok(fetch=lambda u: [] if u.endswith("/api/programs?networkId=11") else self.good_fetch(u))
        self.assertTrue(any("cache-proxy-mismatch" in x for x in report["warnings"]))
        self.assertEqual(len(health.read_json(health.CACHE_FILE)["programs"]), 20)

    def test_06_api_503_is_recorded_as_degraded(self):
        def fail(u):
            if u.endswith("/api/status"): raise RuntimeError("HTTP 503")
            return self.good_fetch(u)
        report = self.collect_ok(fetch=fail)
        self.assertIn("mkra-api", report["failures"])

    def test_07_api_timeout_is_recorded_as_degraded(self):
        def fail(u):
            if u.endswith("/api/status"): raise TimeoutError("deadline")
            return self.good_fetch(u)
        self.assertIn("deadline", self.collect_ok(fetch=fail)["checks"]["mkra"]["error"])

    def test_08_absent_mkra_api_is_critical(self):
        def fail(u):
            if "192.168.1.51:40772" in u: raise OSError("refused")
            return self.good_fetch(u)
        self.assertEqual(self.collect_ok(fetch=fail)["status"], "critical")

    def test_09_recording_defers_retry(self):
        report = self.collect_ok(fetch=lambda u: {"records": [{"isRecording": True}]} if "/api/recording" in u else self.good_fetch(u))
        self.assertTrue(report["checks"]["deferred"])
        self.assertEqual(report["status"], "deferred")

    def test_10_imminent_reservation_defers_retry(self):
        now = self.now
        report = self.collect_ok(fetch=lambda u: {"reserves": [{"startAt": now + 10000, "endAt": now + 900000}]} if "/api/reserves" in u else self.good_fetch(u))
        self.assertGreater(report["checks"]["reservations"]["activeOrWithinGuard"], 0)

    def test_11_scan_lock_held_is_detected(self):
        lock = health.SCAN_LOCK.open("a")
        fcntl = __import__("fcntl")
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            self.assertEqual(self.collect_ok()["checks"]["scannerLock"], "held")
        finally:
            lock.close()

    def test_12_corrupt_cache_is_not_rewritten(self):
        before = health.CACHE_FILE.read_bytes()
        health.CACHE_FILE.write_text("{bad", encoding="utf-8")
        after = health.CACHE_FILE.read_bytes()
        with self.assertRaises(RuntimeError): health.cache_files()
        self.assertEqual(health.CACHE_FILE.read_bytes(), after)
        self.assertNotEqual(before, after)

    def test_13_count_drop_is_differenced(self):
        prior = {"status":"healthy","consecutiveFailures":0,"services":{"101":{"cachedCount":20,"next24hCount":12},"141":{"cachedCount":20,"next24hCount":12}}}
        health.atomic_json(health.HEALTH_FILE, prior)
        self.write_caches({101: 4, 141: 10})
        report = self.collect_ok()
        self.assertLess(report["diffs"]["101"]["cacheCountDelta"], 0)

    def test_14_old_cache_is_stale(self):
        self.write_caches(updated=self.now - health.STALE_MS - 1)
        self.status["serviceStatus"]["101"]["lastSuccess"] = self.now - health.STALE_MS - 1
        health.atomic_json(health.STATUS_FILE, self.status)
        self.assertIn("epg-101", self.collect_ok()["failures"])

    def test_15_thirty_minute_gap_is_only_a_candidate(self):
        events = [(self.now + 3600000, self.now + 5400000), (self.now + 4 * 3600000, self.now + 5 * 3600000)]
        m = health.service_metrics(events, self.now, self.now)
        self.assertIn(150, [g["minutes"] for g in m["gapCandidates30m"]])

    def test_16_sqlite_is_read_only_and_checked(self):
        integ, counts = health.sqlite_counts(health.DATABASE, self.now)
        self.assertEqual(integ, "ok")
        self.assertEqual(counts["11:101"], 10)
        check = sqlite3.connect(health.DATABASE)
        try: self.assertFalse(check.execute("PRAGMA query_only").fetchone()[0])
        finally: check.close()

    def retry_report(self, **changes):
        checks = {
            "deferred": False, "scannerLock": "free", "tuners": {"free": 1},
            "device": {"uptimeSeconds": 3600, "oemReady": True, "mkraHashMatch": True},
            "mkra": {"state": "ok", "version":"0.1.0"}, "4kCache": {"state": "ok"},
            "serviceCatalogue": {"state":"ok"}, "proxy":{"state":"ok"},
            "epgstation":{"state":"ok"}, "sqlite":{"state":"ok"},
            "units": {"bs4k-epg-scan.service": {"active":"inactive"}, "bs2k-epg-scan.service": {"active":"inactive"}},
        }
        services = {str(s): {"lastSuccessAgeMs": health.STALE_MS + 1, "next24hCount": 0, "lastScanStatus":"failed"} for s in health.TARGETS_4K}
        report = {"status":"degraded","consecutiveFailures":2,"checks":checks,"services":services}
        for key, value in changes.items(): report[key] = value
        return report

    def test_17_retry_disabled_is_noop(self):
        r = health.retry_once(self.retry_report(), enabled=False, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "disabled")

    def test_18_retry_requires_two_consecutive_failures(self):
        r = health.retry_once(self.retry_report(consecutiveFailures=1), enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "deferred")

    def test_19_safe_failure_starts_only_existing_4k_unit(self):
        started = []
        r = health.retry_once(self.retry_report(), enabled=True, start_unit=started.append)
        self.assertEqual(r["action"], "started")
        self.assertEqual(started, ["bs4k-epg-scan.service"])

    def test_20_busy_tuner_blocks_retry(self):
        report = self.retry_report(); report["checks"]["tuners"]["free"] = 0
        r = health.retry_once(report, enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "deferred")

    def test_21_unknown_uptime_blocks_retry(self):
        report = self.retry_report(); report["checks"]["device"]["uptimeSeconds"] = None
        r = health.retry_once(report, enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "deferred")

    def test_22_oem_not_ready_blocks_retry(self):
        report = self.retry_report(); report["checks"]["device"]["oemReady"] = False
        r = health.retry_once(report, enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "deferred")

    def test_23_active_scanner_blocks_retry(self):
        report = self.retry_report(); report["checks"]["units"]["bs2k-epg-scan.service"]["active"] = "active"
        r = health.retry_once(report, enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "deferred")

    def test_24_retry_backoff_and_cap(self):
        self.assertEqual([min(health.RETRY_BASE_SECONDS * 2**i, health.RETRY_MAX_SECONDS) for i in range(5)], [1800,3600,7200,14400,14400])
        state = {"lastStarted": int(time.time()), "attempts": 0}
        health.atomic_json(health.RETRY_FILE, state)
        health.atomic_json(health.STATUS_FILE, {"updatedAt": int(time.time()*1000), "status":"partial", "serviceStatus":{}})
        r = health.retry_once(self.retry_report(), enabled=True, start_unit=lambda _: self.fail("must wait"))
        self.assertEqual(r["action"], "backoff")

    def test_25_three_failures_pause_for_manual_review(self):
        state = {"attempts": 3, "paused": True}
        health.atomic_json(health.RETRY_FILE, state)
        r = health.retry_once(self.retry_report(), enabled=True, start_unit=lambda _: self.fail("must not start"))
        self.assertEqual(r["action"], "critical")

    def test_26_successful_new_scan_resets_backoff(self):
        started = int(time.time()) - 10
        health.atomic_json(health.RETRY_FILE, {"lastStarted": started, "attempts": 2, "nextAttempt": 0})
        health.atomic_json(health.STATUS_FILE, {"updatedAt": (started+1)*1000, "status":"complete", "serviceStatus":{str(s):{"status":"ok"} for s in health.TARGETS_4K}})
        r = health.retry_once(self.retry_report(), enabled=True, start_unit=lambda _: self.fail("must reset"))
        self.assertEqual(r["action"], "recovered")
        self.assertEqual(health.read_json(health.RETRY_FILE)["attempts"], 0)

    def test_27_sample_rotation_keeps_bounded_archives(self):
        health.MONITOR_FILE.write_text("x" * 50, encoding="utf-8")
        health.rotate_log()
        self.assertTrue(pathlib.Path(str(health.MONITOR_FILE)+".1").exists())

    def test_28_atomic_health_and_baseline_snapshot(self):
        report = {"status":"healthy","at":"2026-10-23T00:00:00Z","failures":[]}
        health.save_sample(report, baseline=True)
        self.assertTrue(health.HEALTH_FILE.exists())
        self.assertEqual(len(list((self.root/"snapshots").glob("baseline-*.json"))), 1)

    def test_29_no_title_or_recording_path_is_persisted(self):
        report = self.collect_ok()
        encoded = json.dumps(report)
        self.assertNotIn("current", encoded)
        self.assertNotIn("title", encoded)
        self.assertNotIn("filePath", encoded)

    def test_30_receiver_boot_grace_defers_health_and_retry(self):
        with patch.object(health, "device_snapshot", return_value={"uptimeSeconds": 90, "oemReady": True, "mkraSha256": "a"*64}), \
             patch.object(health, "unit_state", return_value={"active":"inactive","result":"success"}), \
             patch.object(health, "timer_state", return_value="active"), \
             patch("urllib.request.urlopen", return_value=MockResponse()):
            report = health.collect(fetch=self.good_fetch, now_ms=self.now)
        self.assertTrue(report["checks"]["rebootGrace"])
        self.assertEqual(report["status"], "deferred")

    def test_31_bs2k_deadline_is_visible_without_triggering_retry(self):
        def units(name):
            return {"active":"failed" if name == "bs2k-epg-scan.service" else "inactive",
                    "result":"failed" if name == "bs2k-epg-scan.service" else "success"}
        with patch.object(health, "unit_state", side_effect=units), patch.object(health, "timer_state", return_value="active"), patch("urllib.request.urlopen", return_value=MockResponse()):
            report = health.collect(fetch=self.good_fetch, include_device=False, now_ms=self.now)
        self.assertIn("bs2k-epg-scan.service-last-result-failed", report["warnings"])
        self.assertFalse(any(x.startswith("epg-") for x in report["failures"]))

    def test_32_retry_requires_proxy_epgstation_and_sqlite_health(self):
        report = self.retry_report()
        report["checks"].update({"serviceCatalogue":{"state":"ok"},"proxy":{"state":"ok"},"epgstation":{"state":"ok"},"sqlite":{"state":"corrupt"}})
        ok, reasons = health.safe_to_retry(report)
        self.assertFalse(ok)
        self.assertTrue(any("SQLite" in x for x in reasons))

    def test_33_systemd_properties_are_mapped_by_name(self):
        cp = types.SimpleNamespace(returncode=0, stdout="Result=success\nExecMainStartTimestamp=now\nExecMainExitTimestamp=later\nExecMainStatus=0\nActiveState=inactive\nSubState=dead\n")
        with patch.object(health.subprocess, "run", return_value=cp):
            state = health.unit_state("example.service")
        self.assertEqual(state["active"], "inactive")
        self.assertEqual(state["result"], "success")

    def test_34_native_log_sampling_keeps_categories_only(self):
        class FakeTelnet:
            def __init__(self): self.commands=[]
            def run(self, command):
                self.commands.append(command)
                if command == "cat /proc/uptime": return "1200.0 1100.0"
                if command.startswith("pidof "): return "11"
                if command.startswith("test -c"): return "READY"
                if "/proc/123/exe" in command: return "lrwxrwxrwx 1 root root 0 -> /tmp/mt/mkra-unified-auto"
                if "/proc/123/cmdline" in command: return "serve\x00--remux4k\x00--tune\x00channel-name-secret"
                if command.startswith("sha256sum"): return "a"*64+" /tmp/mt/mkra-unified-auto"
                if command.startswith("tail "): return "serviceId=101 MPT ADSB title-is-private\nES ring advancing\nError: timeout title-is-private"
                return ""
            def close(self): pass
        fake = FakeTelnet()
        mod = types.SimpleNamespace(connect=lambda *a, **k: fake)
        with patch.dict("sys.modules", {"sharp_telnet": mod}):
            snapshot = health.device_snapshot({"process":{"pid":123}})
        encoded = json.dumps(snapshot)
        self.assertIn("mentioned-unconfirmed", encoded)
        self.assertIn("101", encoded)
        self.assertNotIn("title-is-private", encoded)
        self.assertNotIn("channel-name-secret", encoded)
        self.assertEqual(snapshot["nativeLogIndicators"]["errorCategories"], ["timeout"])

    def test_35_bs2k_failure_alone_never_triggers_4k_retry(self):
        report = self.retry_report()
        report["status"] = "degraded"
        report["services"] = {str(s): {"lastSuccessAgeMs":1000,"next24hCount":4,"lastScanStatus":"ok"} for s in health.TARGETS_4K}
        started=[]
        result = health.retry_once(report, enabled=True, start_unit=started.append)
        self.assertEqual(result["action"], "deferred")
        self.assertFalse(started)

    def test_36_third_completed_failure_pauses_immediately(self):
        started = int(time.time()) - 20
        health.atomic_json(health.RETRY_FILE, {"lastStarted": started, "attempts": 2, "nextAttempt": 0})
        health.atomic_json(health.STATUS_FILE, {"updatedAt": (started + 1) * 1000, "status":"partial", "serviceStatus":{}})
        result = health.retry_once(self.retry_report(), enabled=True, start_unit=lambda _: self.fail("must pause"))
        self.assertEqual(result["action"], "critical")
        self.assertTrue(health.read_json(health.RETRY_FILE)["paused"])

    def test_37_seven_day_proxy_compare_excludes_eighth_day_events(self):
        cache = health.read_json(health.CACHE_FILE)
        cache["programs"].append({"networkId":11,"serviceId":101,"startAt":self.now+8*24*3600000,"endAt":self.now+8*24*3600000+1800000})
        health.atomic_json(health.CACHE_FILE, cache)
        report = self.collect_ok()
        self.assertEqual(report["diffs"]["101"]["proxyVsSQLite7d"], 0)


class MockResponse:
    status = 200
    def __enter__(self): return self
    def __exit__(self, *args): return False
    def read(self, limit=-1): return b"ok"


if __name__ == "__main__":
    unittest.main(verbosity=2)

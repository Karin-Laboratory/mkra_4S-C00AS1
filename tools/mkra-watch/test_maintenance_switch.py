import fcntl
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "maintenance_switch", Path(__file__).with_name("maintenance_switch.py")
)
switch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(switch)


class FakeSystemd:
    def __init__(self):
        self.timer_enabled = True
        self.timer_active = True
        self.service_active = "inactive"
        self.calls = []
        self.fail_disable = False
        self.on_disable = None
        self.on_start_service = None

    def call(self, *args, timeout=20):
        self.calls.append(args)
        if args[:2] == ("disable", "--now"):
            if self.on_disable:
                self.on_disable()
            if self.fail_disable:
                raise switch.SafetyError("injected disable failure")
            self.timer_enabled = self.timer_active = False
        elif args[:2] == ("enable", "--now"):
            self.timer_enabled = self.timer_active = True
        elif args[:1] == ("enable",):
            self.timer_enabled = True
        elif args[:1] == ("disable",):
            self.timer_enabled = False
        elif args[:1] == ("start",) and len(args) > 1 and args[1].endswith(".timer"):
            self.timer_active = True
        elif args[:1] == ("stop",) and len(args) > 1 and args[1].endswith(".timer"):
            self.timer_active = False
        elif args[:1] == ("start",):
            if self.on_start_service:
                self.on_start_service()
        return ""

    def active(self, unit):
        if unit == switch.SERVICE:
            return self.service_active
        if unit in switch.SCANNER_SERVICES:
            return "inactive"
        return "active" if self.timer_active else "inactive"

    def enabled(self, unit):
        return "enabled" if self.timer_enabled else "disabled"


class MaintenanceSwitchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        switch.STATE_DIR = root / "state"
        switch.WATCH_LOCK = switch.STATE_DIR / "watch.lock"
        switch.SCAN_LOCK = root / "scan.lock"
        switch.MAINTENANCE_STATE = switch.STATE_DIR / "maintenance.json"
        switch.STATE_DIR.mkdir(exist_ok=True)
        switch.WATCH_LOCK.write_text("")
        switch.SCAN_LOCK.write_text("")
        self.sd = FakeSystemd()
        self.patcher = mock.patch.multiple(
            switch,
            systemctl=self.sd.call,
            active_state=self.sd.active,
            enabled_state=self.sd.enabled,
            snapshot=lambda: {
                "schema": 1,
                "phase": "PREPARED",
                "host_boot_id": "boot-a",
                "timers": {
                    unit: {"enabled": True, "active": True} for unit in switch.TIMERS
                },
                "unit_files": {unit: {"sha256": unit} for unit in (switch.SERVICE, *switch.SCANNER_SERVICES, *switch.TIMERS)},
                "watcher_bundle": {"binary": "binary"},
            },
            read_only_preflight=lambda: None,
            same_fingerprint=lambda state: None,
            service_environment=lambda: {"MKRA_API": "http://localhost"},
        )
        self.patcher.start()
        self.addCleanup(self.patcher.stop)
        self.root = root

    def run_enter(self, shell=None):
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), mock.patch.object(switch.subprocess, "run", side_effect=shell or (lambda *_a, **_k: None)):
            switch.enter(confirm_preflight=True)

    def test_normal_enter_holds_shared_lock_and_leaves_timer_paused(self):
        saw_lock = []

        def shell(*_args, **_kwargs):
            self.assertFalse(self.sd.timer_active)
            fd = switch.os.open(switch.WATCH_LOCK, switch.os.O_RDWR)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                saw_lock.append(True)
            finally:
                switch.os.close(fd)

        self.run_enter(shell)
        self.assertTrue(saw_lock)
        self.assertFalse(self.sd.timer_active)
        self.assertEqual(switch.read_state()["phase"], "SESSION_EXITED")

    def test_timer_activation_racing_enter_cannot_take_shared_lock(self):
        blocked = []

        def timer_race():
            if blocked:
                return
            fd = switch.os.open(switch.WATCH_LOCK, switch.os.O_RDWR)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                blocked.append(True)
            finally:
                switch.os.close(fd)

        self.sd.on_disable = timer_race
        self.run_enter()
        self.assertEqual(blocked, [True])

    def test_running_watcher_owning_lock_blocks_without_systemd_changes(self):
        switch.STATE_DIR.mkdir(exist_ok=True)
        fd = switch.os.open(switch.WATCH_LOCK, switch.os.O_CREAT | switch.os.O_RDWR, 0o640)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.addCleanup(switch.os.close, fd)
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertEqual(self.sd.calls, [])

    def test_disable_failure_rolls_back_timer(self):
        self.sd.fail_disable = True
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertTrue(self.sd.timer_active)
        self.assertEqual(switch.read_state()["phase"], "NORMAL")

    def test_active_oneshot_is_never_stopped_forcibly(self):
        self.sd.service_active = "activating"
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), mock.patch.object(
            switch, "wait_service_inactive", side_effect=switch.SafetyError("still active")
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertFalse(any(call[:1] == ("stop",) for call in self.sd.calls))
        self.assertTrue(self.sd.timer_active)

    def test_recording_preflight_failure_happens_before_timer_change(self):
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), mock.patch.object(
            switch, "read_only_preflight", side_effect=switch.SafetyError("recording active")
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertEqual(self.sd.calls, [])
        self.assertTrue(self.sd.timer_active)

    def test_bundle_mismatch_blocks_before_timer_change(self):
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), mock.patch.object(
            switch, "read_only_preflight", side_effect=switch.SafetyError("hash mismatch")
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertEqual(self.sd.calls, [])

    def test_second_enter_refuses_unresolved_maintenance(self):
        switch.STATE_DIR.mkdir(exist_ok=True)
        switch.MAINTENANCE_STATE.write_text(json.dumps({"schema": 1, "phase": "MAINTENANCE"}))
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch.sys.stdin, "isatty", return_value=True
        ), self.assertRaises(switch.SafetyError):
            switch.enter(confirm_preflight=True)
        self.assertEqual(self.sd.calls, [])

    def test_dirty_state_cannot_resume(self):
        switch.STATE_DIR.mkdir(exist_ok=True)
        switch.MAINTENANCE_STATE.write_text(json.dumps({"schema": 1, "phase": "DIRTY"}))
        with mock.patch.object(switch.os, "geteuid", return_value=0), self.assertRaises(
            switch.SafetyError
        ):
            switch.resume(confirm_clean=True)

    def test_mixed_timer_state_blocks_resume(self):
        self.write_maintenance_state("SESSION_EXITED")
        self.sd.timer_enabled = True
        self.sd.timer_active = False
        with mock.patch.object(switch.os, "geteuid", return_value=0), self.assertRaises(
            switch.SafetyError
        ):
            switch.resume(confirm_clean=True)
        self.assertFalse(any(call[:1] == ("start",) for call in self.sd.calls))

    def test_api_restore_failure_keeps_timer_enabled_and_records_failure(self):
        self.write_maintenance_state("SESSION_EXITED")
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch, "full_restore_healthy", return_value=False
        ), mock.patch.object(switch, "RESTORE_TIMEOUT", 0), self.assertRaises(switch.SafetyError):
            switch.resume(confirm_clean=True)
        self.assertTrue(self.sd.timer_active)
        self.assertEqual(switch.read_state()["phase"], "RESTORE_FAILED")

    def test_interrupted_resume_can_continue_when_timer_is_active(self):
        self.write_maintenance_state("RESTORE_PENDING")
        self.sd.timer_active = self.sd.timer_enabled = True
        self.sd.calls.clear()
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch, "full_restore_healthy", return_value=True
        ):
            switch.resume(confirm_clean=True)
        self.assertIn(("start", switch.SERVICE), self.sd.calls)
        self.assertEqual(switch.read_state()["phase"], "RESTORED")

    def test_interrupted_partial_restore_reapplies_exact_prior_states(self):
        self.write_maintenance_state("RESTORE_FAILED")
        self.sd.timer_active = self.sd.timer_enabled = False
        self.sd.call("enable", switch.TIMERS[0])
        self.sd.call("start", switch.TIMERS[0])
        self.sd.calls.clear()
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch, "full_restore_healthy", return_value=True
        ):
            switch.resume(confirm_clean=True)
        self.assertEqual(switch.timer_states(), {
            unit: {"enabled": True, "active": True} for unit in switch.TIMERS
        })
        self.assertEqual(switch.read_state()["phase"], "RESTORED")

    def test_watcher_invocation_occurs_only_after_maintenance_lock_release(self):
        self.write_maintenance_state("SESSION_EXITED")
        lock_was_free = []

        def start_service():
            fd = switch.os.open(switch.WATCH_LOCK, switch.os.O_RDWR)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                lock_was_free.append(True)
                fcntl.flock(fd, fcntl.LOCK_UN)
            finally:
                switch.os.close(fd)

        self.sd.on_start_service = start_service
        with mock.patch.object(switch.os, "geteuid", return_value=0), mock.patch.object(
            switch, "full_restore_healthy", return_value=True
        ):
            switch.resume(confirm_clean=True)
        self.assertEqual(lock_was_free, [True])

    def write_maintenance_state(self, phase):
        switch.STATE_DIR.mkdir(parents=True, exist_ok=True)
        self.sd.timer_enabled = self.sd.timer_active = False
        switch.MAINTENANCE_STATE.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "phase": phase,
                    "host_boot_id": "boot-a",
                    "timers": {
                        unit: {"enabled": True, "active": True} for unit in switch.TIMERS
                    },
                    "unit_files": {unit: {"sha256": unit} for unit in (switch.SERVICE, *switch.SCANNER_SERVICES, *switch.TIMERS)},
                    "watcher_bundle": {"binary": "binary"},
                }
            )
        )


if __name__ == "__main__":
    unittest.main()

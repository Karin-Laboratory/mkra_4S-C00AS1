import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "switch_runtime", Path(__file__).with_name("switch_runtime.py")
)
switch_runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(switch_runtime)
maintenance = switch_runtime.maintenance


def sha(data):
    return hashlib.sha256(data).hexdigest()


def md5(data):
    return hashlib.md5(data).hexdigest()


def make_v2(directory, suffix):
    directory.mkdir(parents=True, exist_ok=True)
    directory.chmod(0o750)
    files = {
        "watch.py": ("watch " + suffix).encode(),
        "sharp_telnet.py": ("helper " + suffix).encode(),
        "mkra": ("binary " + suffix).encode(),
        "bs-channels.tsv": ("channels " + suffix).encode(),
    }
    for name, data in files.items():
        path = directory / name
        path.write_bytes(data)
        path.chmod(0o755 if name == "mkra" else 0o644)
    binary = files["mkra"]
    config = files["bs-channels.tsv"]
    manifest = {
        "schema": 2,
        "binary_size": len(binary),
        "binary_sha256": sha(binary),
        "binary_md5": md5(binary),
        "config_sha256": sha(config),
        "config_md5": md5(config),
        "watcher_sha256": sha(files["watch.py"]),
        "helper_sha256": sha(files["sharp_telnet.py"]),
    }
    path = directory / "bundle.json"
    path.write_text(json.dumps(manifest))
    path.chmod(0o644)
    return manifest


class RuntimeSwitchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.runtime = root / "runtime"
        self.runtime.mkdir()
        self.runtime.chmod(0o750)
        self.backup_state = root / "state"
        self.backup_state.mkdir(mode=0o700)
        self.watch_lock = self.backup_state / "watch.lock"
        self.watch_lock.write_text("")
        self.scan_lock = root / "scan.lock"
        self.scan_lock.write_text("")
        self.bundle = root / "target-bundle"
        make_v2(self.bundle, "target")
        legacy_files = {
            "watch.py": b"EXPECTED_SIZE = 17\nEXPECTED_SHA256 = 'aa'\nEXPECTED_MD5 = 'bb'\nCONFIG_SHA256 = 'cc'\nCONFIG_MD5 = 'dd'\n",
            "sharp_telnet.py": b"legacy helper",
            "mkra": b"legacy binary data",
            "bs-channels.tsv": b"legacy channels",
        }
        legacy_values = {
            "watcher_sha256": sha(legacy_files["watch.py"]),
            "helper_sha256": sha(legacy_files["sharp_telnet.py"]),
            "binary_size": len(legacy_files["mkra"]),
            "binary_sha256": sha(legacy_files["mkra"]),
            "binary_md5": md5(legacy_files["mkra"]),
            "config_sha256": sha(legacy_files["bs-channels.tsv"]),
            "config_md5": md5(legacy_files["bs-channels.tsv"]),
        }
        legacy_files["watch.py"] = (
            "EXPECTED_SIZE = %d\nEXPECTED_SHA256 = %r\nEXPECTED_MD5 = %r\nCONFIG_SHA256 = %r\nCONFIG_MD5 = %r\n"
            % (legacy_values["binary_size"], legacy_values["binary_sha256"],
               legacy_values["binary_md5"], legacy_values["config_sha256"],
               legacy_values["config_md5"])
        ).encode()
        legacy_values["watcher_sha256"] = sha(legacy_files["watch.py"])
        for name, data in legacy_files.items():
            path = self.runtime / name
            path.write_bytes(data)
            path.chmod(0o755 if name == "mkra" else 0o644)
        self.profile_patch = mock.patch.object(maintenance, "LEGACY_R4_PROFILE", legacy_values)
        self.profile_patch.start()
        self.addCleanup(self.profile_patch.stop)
        for name, value in {
            "RUNTIME_DIR": self.runtime,
            "STATE_DIR": self.backup_state,
            "WATCH_LOCK": self.watch_lock,
            "SCAN_LOCK": self.scan_lock,
            "MAINTENANCE_STATE": self.backup_state / "maintenance.json",
        }.items():
            patcher = mock.patch.object(maintenance, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.states_patch = mock.patch.multiple(
            maintenance,
            active_state=lambda _unit: "inactive",
            enabled_state=lambda _unit: "disabled",
            unit_fingerprint=lambda unit: {"path": unit, "sha256": "unit"},
            host_boot_id=lambda: "boot-a",
            same_fingerprint=lambda _state: None,
        )
        self.states_patch.start()
        self.addCleanup(self.states_patch.stop)
        self.before = maintenance.runtime_fingerprint()
        self.target = maintenance.fingerprint_bundle_directory(self.bundle)
        maintenance.write_state({
            "schema": 1,
            "phase": "SESSION_EXITED",
            "host_boot_id": "boot-a",
            "watcher_bundle": self.before,
            "target_watcher_bundle": self.target,
            "runtime_switch_sha256": maintenance.runtime_switch_fingerprint(),
            "timers": {unit: {"active": False, "enabled": False} for unit in maintenance.TIMERS},
        })

    def test_install_then_rollback_preserves_exact_original_runtime(self):
        with mock.patch.object(switch_runtime, "_require_root", return_value=None):
            switch_runtime.install(self.bundle)
            self.assertEqual(maintenance.runtime_fingerprint(), self.target)
            state = maintenance.read_state()
            backup = Path(state["runtime_backup"])
            self.assertTrue((backup / "backup.json").is_file())
            switch_runtime.rollback()
        self.assertEqual(maintenance.runtime_fingerprint(), self.before)
        self.assertEqual(maintenance.read_state()["runtime_transition"], "ROLLED_BACK")
        self.assertTrue(backup.is_dir())

    def test_wrong_bundle_is_rejected_without_changing_runtime(self):
        wrong = self.bundle / "bundle.json"
        data = json.loads(wrong.read_text())
        data["watcher_sha256"] = "0" * 64
        wrong.write_text(json.dumps(data))
        wrong.chmod(0o644)
        with mock.patch.object(switch_runtime, "_require_root", return_value=None), self.assertRaises(maintenance.SafetyError):
            switch_runtime.install(self.bundle)
        self.assertEqual(maintenance.runtime_fingerprint(), self.before)
        self.assertFalse((self.backup_state / "runtime-backups").exists())

    def test_partial_runtime_install_failure_rolls_back_before_returning(self):
        real_install_files = switch_runtime._install_files
        calls = []

        def fail_after_first_runtime_file(source, names):
            calls.append(source)
            if len(calls) == 1:
                (self.runtime / "watch.py").write_bytes((Path(source) / "watch.py").read_bytes())
                raise OSError("injected mid-install failure")
            return real_install_files(source, names)

        with mock.patch.object(switch_runtime, "_require_root", return_value=None), \
             mock.patch.object(switch_runtime, "_install_files", side_effect=fail_after_first_runtime_file), \
             self.assertRaises(OSError):
            switch_runtime.install(self.bundle)
        self.assertEqual(maintenance.runtime_fingerprint(), self.before)
        self.assertEqual(maintenance.read_state()["runtime_transition"], "INSTALL_FAILED_ROLLED_BACK")

    def test_rollback_refuses_tampered_backup_without_changing_runtime(self):
        with mock.patch.object(switch_runtime, "_require_root", return_value=None):
            switch_runtime.install(self.bundle)
        state = maintenance.read_state()
        backup = Path(state["runtime_backup"])
        (backup / "mkra").write_bytes(b"tampered rollback image")
        with mock.patch.object(switch_runtime, "_require_root", return_value=None), \
             self.assertRaises(maintenance.SafetyError):
            switch_runtime.rollback()
        self.assertEqual(maintenance.runtime_fingerprint(), self.target)


if __name__ == "__main__":
    unittest.main()

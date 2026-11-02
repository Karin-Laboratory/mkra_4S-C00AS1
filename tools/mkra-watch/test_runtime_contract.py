import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
from contextlib import redirect_stdout
from io import StringIO

spec = importlib.util.spec_from_file_location(
    "maintenance_switch", Path(__file__).with_name("maintenance_switch.py")
)
switch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(switch)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def md5(data):
    return hashlib.md5(data).hexdigest()


class RuntimeContractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir()
        self.runtime.chmod(0o750)
        self.patcher = mock.patch.object(switch, "RUNTIME_DIR", self.runtime)
        self.patcher.start()
        self.addCleanup(self.patcher.stop)

    def write_v2_bundle(self):
        data = {
            "watch.py": b"watcher v2\n",
            "sharp_telnet.py": b"helper v2\n",
            "mkra": b"ELF test binary\n",
            "bs-channels.tsv": b"channel list\n",
        }
        for name, value in data.items():
            path = self.runtime / name
            path.write_bytes(value)
            path.chmod(0o755 if name == "mkra" else 0o644)
        manifest = {
            "schema": 2,
            "binary_size": len(data["mkra"]),
            "binary_sha256": sha(data["mkra"]),
            "binary_md5": md5(data["mkra"]),
            "config_sha256": sha(data["bs-channels.tsv"]),
            "config_md5": md5(data["bs-channels.tsv"]),
            "watcher_sha256": sha(data["watch.py"]),
            "helper_sha256": sha(data["sharp_telnet.py"]),
        }
        manifest_path = self.runtime / "bundle.json"
        manifest_path.write_text(json.dumps(manifest))
        manifest_path.chmod(0o644)
        return manifest

    def test_v2_manifest_validates_all_executable_runtime_assets(self):
        self.write_v2_bundle()
        result = switch.runtime_fingerprint()
        self.assertEqual(result["profile"], "bundle-v2")
        (self.runtime / "sharp_telnet.py").write_bytes(b"changed helper\n")
        with self.assertRaises(switch.SafetyError):
            switch.runtime_fingerprint()

    def test_unknown_or_partial_manifest_never_falls_back_to_legacy(self):
        self.write_v2_bundle()
        manifest = json.loads((self.runtime / "bundle.json").read_text())
        manifest.pop("helper_sha256")
        (self.runtime / "bundle.json").write_text(json.dumps(manifest))
        (self.runtime / "bundle.json").chmod(0o644)
        with self.assertRaises(switch.SafetyError):
            switch.runtime_fingerprint()

    def test_v1_bundle_is_rejected_instead_of_loosening_the_contract(self):
        self.write_v2_bundle()
        manifest = json.loads((self.runtime / "bundle.json").read_text())
        manifest["schema"] = 1
        (self.runtime / "bundle.json").write_text(json.dumps(manifest))
        with self.assertRaises(switch.SafetyError):
            switch.runtime_fingerprint()

    def test_legacy_inline_pin_profile_is_exact_and_read_only(self):
        binary = b"reviewed legacy binary"
        config = b"reviewed legacy channels"
        helper = b"reviewed legacy helper"
        watcher = (
            "EXPECTED_SIZE = %d\nEXPECTED_SHA256 = %r\nEXPECTED_MD5 = %r\n"
            "CONFIG_SHA256 = %r\nCONFIG_MD5 = %r\n"
        ) % (len(binary), sha(binary), md5(binary), sha(config), md5(config))
        files = {
            "watch.py": watcher.encode(),
            "sharp_telnet.py": helper,
            "mkra": binary,
            "bs-channels.tsv": config,
        }
        for name, value in files.items():
            path = self.runtime / name
            path.write_bytes(value)
            path.chmod(0o755 if name == "mkra" else 0o644)
        profile = {
            "watcher_sha256": sha(files["watch.py"]),
            "helper_sha256": sha(helper),
            "binary_size": len(binary),
            "binary_sha256": sha(binary),
            "binary_md5": md5(binary),
            "config_sha256": sha(config),
            "config_md5": md5(config),
        }
        with mock.patch.object(switch, "LEGACY_R4_PROFILE", profile):
            result = switch.runtime_fingerprint()
            self.assertEqual(result["profile"], "r4-legacy-inline-pins")
            (self.runtime / "mkra").write_bytes(b"unreviewed binary")
            with self.assertRaises(switch.SafetyError):
                switch.runtime_fingerprint()

    def test_legacy_inline_expression_is_not_evaluated_or_accepted(self):
        watcher = self.runtime / "watch.py"
        watcher.write_text("EXPECTED_SHA256 = __import__('os').system('false')\n")
        for name in ("sharp_telnet.py", "mkra", "bs-channels.tsv"):
            (self.runtime / name).write_bytes(b"x")
        profile = dict(switch.LEGACY_R4_PROFILE)
        profile["watcher_sha256"] = sha(watcher.read_bytes())
        with mock.patch.object(switch, "LEGACY_R4_PROFILE", profile), self.assertRaises(switch.SafetyError):
            switch.runtime_fingerprint()

    def test_target_bundle_validation_uses_same_strict_v2_contract(self):
        manifest = self.write_v2_bundle()
        result = switch.fingerprint_bundle_directory(self.runtime)
        self.assertEqual(result["profile"], "bundle-v2")
        self.assertEqual(result["manifest_sha256"], sha((self.runtime / "bundle.json").read_bytes()))
        self.assertEqual(manifest["schema"], 2)

    def test_preflight_is_read_only_and_only_uses_observation_calls(self):
        enabled = {switch.WATCH_TIMER: "enabled"}
        enabled.update({unit: "enabled" for unit in switch.TIMERS[1:]})
        enabled.update({unit: "static" for unit in (switch.SERVICE, *switch.SCANNER_SERVICES)})
        output = StringIO()
        with mock.patch.object(switch.os, "geteuid", return_value=0), \
             mock.patch.object(switch, "runtime_fingerprint", return_value={"profile": "fixture"}), \
             mock.patch.object(switch, "active_state", side_effect=lambda unit: "active" if unit == switch.WATCH_TIMER else "inactive"), \
             mock.patch.object(switch, "enabled_state", side_effect=lambda unit: enabled[unit]), \
             mock.patch.object(switch, "unit_fingerprint", side_effect=lambda unit: {"sha256": "unit"}), \
             mock.patch.object(switch, "read_only_preflight") as health, \
             mock.patch.object(switch, "write_state", side_effect=AssertionError("must not write")), \
             mock.patch.object(switch, "acquire_barriers", side_effect=AssertionError("must not lock")), \
             redirect_stdout(output):
            switch.preflight()
        health.assert_called_once_with()
        self.assertIn("READ_ONLY_PREFLIGHT_PASS", output.getvalue())
        self.assertFalse(switch.MAINTENANCE_STATE.exists())

    def test_runtime_switch_helper_is_pinned_to_reviewed_implementation(self):
        self.assertEqual(switch.runtime_switch_fingerprint(), switch.RUNTIME_SWITCH_APPROVED_SHA256)
        with mock.patch.object(switch, "RUNTIME_SWITCH_APPROVED_SHA256", "0" * 64), \
             self.assertRaises(switch.SafetyError):
            switch.runtime_switch_fingerprint()


if __name__ == "__main__":
    unittest.main()

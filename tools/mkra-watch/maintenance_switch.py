#!/usr/bin/env python3
"""Host-side, fail-closed maintenance barrier for the mkra restore watcher.

This tool never connects to the receiver. `enter` holds the watcher's existing
flock for the lifetime of an interactive shell and persistently disables the
timer. `resume --confirm-clean` only restores the timer and invokes the
existing oneshot watcher after an operator has checked the receiver baseline.
"""
import fcntl
import ast
import hashlib
import json
import os
import pathlib
import shlex
import stat
import subprocess
import sys
import tempfile
import time

STATE_DIR = pathlib.Path("/var/lib/4s-mkra-watch")
WATCH_LOCK = STATE_DIR / "watch.lock"
SCAN_LOCK = pathlib.Path("/var/lib/bs4k-epg/scan.lock")
MAINTENANCE_STATE = STATE_DIR / "maintenance.json"
RUNTIME_DIR = pathlib.Path("/usr/local/libexec/4s-mkra-watch")
SWITCH_SCRIPT = pathlib.Path(__file__).resolve()
TIMERS = (
    "4s-mkra-watch.timer",
    "bs2k-epg-scan.timer",
    "bs4k-epg-scan.timer",
)
WATCH_TIMER = TIMERS[0]
SERVICE = "4s-mkra-watch.service"
SCANNER_SERVICES = ("bs2k-epg-scan.service", "bs4k-epg-scan.service")
RESTORE_TIMEOUT = 900

# R4 captured this exact deployed legacy watcher and helper. That watcher
# embeds hash pins for the old executable and channel table. This profile is a
# narrowly scoped compatibility exception; it is not a generic trust-on-first-
# use fallback. Any changed file requires a reviewed modern bundle instead.
LEGACY_R4_PROFILE = {
    "watcher_sha256": "af4111e3079f49e5ad0c104bfd2dd994cb2fa20303705068051e7d07a6e56cb0",
    "helper_sha256": "33cb09a80f4a7eab7d31182deefec0f28ccc3d78fcd386b803136030f1268705",
    "binary_size": 984708,
    "binary_sha256": "d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3",
    "binary_md5": "48a383d70c8e463eec7d5e54ccaf47cc",
    "config_sha256": "904ad31a6ff5937261d3cbf251713190bf52ab82f12ab3597a44a10a1b950322",
    "config_md5": "036bc3e5137d063ee58365bcccc95d3a",
}
RUNTIME_FILES = ("watch.py", "sharp_telnet.py", "mkra", "bs-channels.tsv")
MANIFEST_FIELDS = {
    "schema", "binary_size", "binary_sha256", "binary_md5",
    "config_sha256", "config_md5", "watcher_sha256", "helper_sha256",
}


class SafetyError(RuntimeError):
    pass


def systemctl(*args, timeout=20):
    result = subprocess.run(
        ["systemctl", *args], capture_output=True, text=True, timeout=timeout
    )
    if result.returncode:
        raise SafetyError("systemctl operation failed: " + " ".join(args[:2]))
    return result.stdout.strip()


def active_state(unit):
    return systemctl("show", "--property=ActiveState", "--value", unit)


def enabled_state(unit):
    return systemctl("is-enabled", unit)


def unit_fingerprint(unit):
    fragment = pathlib.Path(
        systemctl("show", "--property=FragmentPath", "--value", unit)
    )
    if not fragment.is_file():
        raise SafetyError("systemd unit file is missing")
    return {"path": str(fragment), "sha256": digest(fragment)}


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _runtime_file(directory, name):
    directory = pathlib.Path(directory)
    path = directory / name
    try:
        dir_info = directory.lstat()
        info = path.lstat()
    except OSError as exc:
        raise SafetyError("runtime file is missing: " + name) from exc
    if not stat.S_ISDIR(dir_info.st_mode) or stat.S_ISLNK(dir_info.st_mode):
        raise SafetyError("runtime directory is not a real directory")
    if dir_info.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise SafetyError("runtime directory is group/world writable")
    if os.geteuid() == 0 and dir_info.st_uid != 0:
        raise SafetyError("runtime directory is not root-owned")
    if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
        raise SafetyError("runtime file is not a regular file: " + name)
    if info.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise SafetyError("runtime file is group/world writable: " + name)
    if os.geteuid() == 0 and info.st_uid != 0:
        raise SafetyError("runtime file is not root-owned: " + name)
    return path


def _manifest_values(directory):
    manifest_path = _runtime_file(directory, "bundle.json")
    try:
        manifest = json.loads(manifest_path.read_text())
    except (OSError, ValueError) as exc:
        raise SafetyError("runtime bundle manifest is unreadable") from exc
    if not isinstance(manifest, dict) or manifest.get("schema") != 2:
        raise SafetyError("runtime bundle manifest schema is unsupported")
    if set(manifest) != MANIFEST_FIELDS:
        raise SafetyError("runtime bundle manifest fields are incomplete or unexpected")
    for key in ("binary_size",):
        if not isinstance(manifest.get(key), int) or manifest[key] <= 0:
            raise SafetyError("runtime bundle manifest has invalid size")
    for key in MANIFEST_FIELDS - {"schema", "binary_size"}:
        value = manifest.get(key)
        expected_len = 32 if key.endswith("md5") else 64
        if not isinstance(value, str) or len(value) != expected_len or any(c not in "0123456789abcdef" for c in value):
            raise SafetyError("runtime bundle manifest has invalid digest")
    return manifest, manifest_path


def fingerprint_bundle_directory(directory):
    """Verify a modern bundle without importing or executing its contents."""
    directory = pathlib.Path(directory)
    manifest, manifest_path = _manifest_values(directory)
    paths = {name: _runtime_file(directory, name) for name in RUNTIME_FILES}
    checks = {
        "binary_size": paths["mkra"].stat().st_size,
        "binary_sha256": digest(paths["mkra"]),
        "binary_md5": hashlib.md5(paths["mkra"].read_bytes()).hexdigest(),
        "config_sha256": digest(paths["bs-channels.tsv"]),
        "config_md5": hashlib.md5(paths["bs-channels.tsv"].read_bytes()).hexdigest(),
        "watcher_sha256": digest(paths["watch.py"]),
        "helper_sha256": digest(paths["sharp_telnet.py"]),
    }
    if any(manifest[key] != value for key, value in checks.items()):
        raise SafetyError("runtime bundle content does not match its manifest")
    return {
        "profile": "bundle-v2",
        "manifest_sha256": digest(manifest_path),
        "files": {name: digest(path) for name, path in paths.items()},
    }


def _literal_watcher_pins(path):
    """Read legacy constants from Python AST; never import the old watcher."""
    try:
        tree = ast.parse(path.read_text())
    except (OSError, SyntaxError) as exc:
        raise SafetyError("legacy watcher source cannot be parsed safely") from exc
    wanted = {"EXPECTED_SIZE", "EXPECTED_SHA256", "EXPECTED_MD5", "CONFIG_SHA256", "CONFIG_MD5"}
    found = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
            if name in wanted:
                if name in found:
                    raise SafetyError("legacy watcher has duplicate runtime pins")
                try:
                    found[name] = ast.literal_eval(node.value)
                except (ValueError, TypeError) as exc:
                    raise SafetyError("legacy watcher pin is not a literal") from exc
    if set(found) != wanted:
        raise SafetyError("legacy watcher does not contain the complete pin set")
    return found


def _legacy_r4_fingerprint(directory):
    paths = {name: _runtime_file(directory, name) for name in RUNTIME_FILES}
    profile = LEGACY_R4_PROFILE
    if digest(paths["watch.py"]) != profile["watcher_sha256"]:
        raise SafetyError("unmanifested watcher is not the reviewed R4 legacy version")
    pins = _literal_watcher_pins(paths["watch.py"])
    expected_pins = {
        "EXPECTED_SIZE": profile["binary_size"],
        "EXPECTED_SHA256": profile["binary_sha256"],
        "EXPECTED_MD5": profile["binary_md5"],
        "CONFIG_SHA256": profile["config_sha256"],
        "CONFIG_MD5": profile["config_md5"],
    }
    if pins != expected_pins:
        raise SafetyError("legacy watcher inline pins differ from the reviewed R4 profile")
    actual = {
        "binary_size": paths["mkra"].stat().st_size,
        "binary_sha256": digest(paths["mkra"]),
        "binary_md5": hashlib.md5(paths["mkra"].read_bytes()).hexdigest(),
        "config_sha256": digest(paths["bs-channels.tsv"]),
        "config_md5": hashlib.md5(paths["bs-channels.tsv"].read_bytes()).hexdigest(),
        "helper_sha256": digest(paths["sharp_telnet.py"]),
    }
    if actual != {key: profile[key] for key in actual}:
        raise SafetyError("unmanifested runtime does not match the reviewed R4 legacy profile")
    return {
        "profile": "r4-legacy-inline-pins",
        "manifest_sha256": None,
        "files": {name: digest(path) for name, path in paths.items()},
    }


def runtime_fingerprint():
    return fingerprint_runtime_directory(RUNTIME_DIR)


def fingerprint_runtime_directory(directory):
    directory = pathlib.Path(directory)
    manifest_path = directory / "bundle.json"
    if manifest_path.exists() or manifest_path.is_symlink():
        return fingerprint_bundle_directory(directory)
    return _legacy_r4_fingerprint(directory)


def host_boot_id():
    return pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()


def read_state():
    try:
        value = json.loads(MAINTENANCE_STATE.read_text())
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as exc:
        raise SafetyError("maintenance record is unreadable; preserve it") from exc
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise SafetyError("maintenance record has an unknown schema")
    return value


def write_state(state):
    STATE_DIR.mkdir(mode=0o750, parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix="maintenance.", dir=STATE_DIR)
    tmp = pathlib.Path(name)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(state, f, sort_keys=True, indent=2)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, 0o600)
        os.replace(tmp, MAINTENANCE_STATE)
    finally:
        try:
            tmp.unlink()
        except FileNotFoundError:
            pass


def snapshot():
    return {
        "schema": 1,
        "phase": "PREPARED",
        "created_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "host_boot_id": host_boot_id(),
        "timers": {
            unit: {
                "enabled": enabled_state(unit) == "enabled",
                "active": active_state(unit) == "active",
            }
            for unit in TIMERS
        },
        "unit_files": {
            unit: unit_fingerprint(unit)
            for unit in (SERVICE, *SCANNER_SERVICES, *TIMERS)
        },
        "watcher_bundle": runtime_fingerprint(),
        "maintenance_tool_sha256": digest(SWITCH_SCRIPT),
    }


def acquire_barriers():
    if not STATE_DIR.is_dir() or not WATCH_LOCK.exists():
        raise SafetyError("existing watcher state/lock path is missing")
    if not SCAN_LOCK.parent.is_dir() or not SCAN_LOCK.exists():
        raise SafetyError("shared EPG scanner lock file is missing")
    held = []
    for path, label in ((WATCH_LOCK, "watcher"), (SCAN_LOCK, "EPG scanner")):
        fd = os.open(path, os.O_RDWR)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            os.close(fd)
            for old_fd in reversed(held):
                os.close(old_fd)
            raise SafetyError(label + " owns its shared lock; maintenance is blocked") from exc
        held.append(fd)
    return held


def release_barriers(fds):
    for fd in reversed(fds):
        try:
            os.close(fd)
        except OSError:
            pass


def timer_states():
    return {
        unit: {"enabled": enabled_state(unit) == "enabled", "active": active_state(unit) == "active"}
        for unit in TIMERS
    }


def set_timer_state(unit, state):
    if state["enabled"]:
        systemctl("enable", unit)
    else:
        systemctl("disable", "--now", unit)
    if state["active"]:
        systemctl("start", unit)
    else:
        systemctl("stop", unit)


def disable_timers():
    for unit in TIMERS:
        systemctl("disable", "--now", unit)
    if any(active_state(unit) != "inactive" or enabled_state(unit) != "disabled" for unit in TIMERS):
        raise SafetyError("a watcher or scanner timer did not become inactive and disabled")


def restore_timers(states):
    for unit, old in states.items():
        set_timer_state(unit, old)
    for unit, old in states.items():
        if (enabled_state(unit) == "enabled") != old["enabled"]:
            raise SafetyError("timer enable state was not restored")
        if (active_state(unit) == "active") != old["active"]:
            raise SafetyError("timer active state was not restored")


def wait_service_inactive(timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        states = [active_state(unit) for unit in (SERVICE, *SCANNER_SERVICES)]
        if all(state == "inactive" for state in states):
            return
        if any(state == "failed" for state in states):
            raise SafetyError("watcher or scanner service is failed; investigate before maintenance")
        time.sleep(0.2)
    raise SafetyError("watcher or scanner did not become inactive; do not stop it forcibly")


def same_fingerprint(state):
    if host_boot_id() != state["host_boot_id"]:
        raise SafetyError("host rebooted during maintenance; require manual review")
    current_units = {
        unit: unit_fingerprint(unit)
        for unit in (SERVICE, *SCANNER_SERVICES, *TIMERS)
    }
    if current_units != state["unit_files"]:
        raise SafetyError("watcher/scanner systemd unit changed during maintenance")
    current_runtime = runtime_fingerprint()
    allowed_runtimes = [state["watcher_bundle"]]
    target_runtime = state.get("target_watcher_bundle")
    if target_runtime is not None:
        allowed_runtimes.append(target_runtime)
    if current_runtime not in allowed_runtimes:
        raise SafetyError("watcher runtime is neither the saved original nor the pre-approved target bundle")
    if digest(SWITCH_SCRIPT) != state["maintenance_tool_sha256"]:
        raise SafetyError("maintenance tool changed during maintenance")


def load_watcher():
    sys.path.insert(0, str(RUNTIME_DIR))
    try:
        import watch
    except ImportError as exc:
        raise SafetyError("installed watcher module is unavailable") from exc
    env = service_environment()
    watch.TUNER_HOST = env.get("TUNER_HOST", watch.TUNER_HOST)
    watch.API_BASE = env.get("MKRA_API", watch.API_BASE)
    watch.PROXY_BASE = env.get("MKRA_PROXY", watch.PROXY_BASE)
    watch.EPGSTATION_BASE = env.get("EPGSTATION_URL", watch.EPGSTATION_BASE)
    return watch


def read_only_preflight():
    watch = load_watcher()
    watch.verify_bundle()
    healthy, detail = watch.api_healthy()
    if not healthy:
        raise SafetyError("receiver API is not healthy: " + detail)
    tuners = watch.get_json(watch.API_BASE + "/api/tuners")
    tuner = next((item for item in tuners if item.get("index") == 0), None)
    if (
        not tuner
        or tuner.get("isAvailable") is not True
        or tuner.get("isFault") is not False
        or tuner.get("isUsing") is not False
        or tuner.get("users") not in (None, [])
    ):
        raise SafetyError("API does not prove an idle, healthy tuner")
    safe, reason = watch.local_start_safety(require_reservation_guard=True)
    if not safe:
        raise SafetyError("recording/reservation/scanner preflight: " + reason)


def enter(confirm_preflight=False, target_bundle=None):
    if os.geteuid() != 0:
        raise SafetyError("run this command with host administrator privileges")
    if not confirm_preflight:
        raise SafetyError("enter requires --confirm-preflight after remote read-only checks")
    if not sys.stdin.isatty():
        raise SafetyError("enter requires an interactive terminal to retain the lock")
    fd = acquire_barriers()
    try:
        old = read_state()
        if old and old.get("phase") not in ("NORMAL", "RESTORED"):
            raise SafetyError("an earlier maintenance attempt is unresolved")
        state = snapshot()
        if target_bundle is not None:
            state["target_watcher_bundle"] = fingerprint_bundle_directory(target_bundle)
        read_only_preflight()
        if not state["timers"][WATCH_TIMER]["enabled"] or not state["timers"][WATCH_TIMER]["active"]:
            raise SafetyError("watcher timer must begin enabled and active")
        wait_service_inactive()
        state["phase"] = "PREPARED"
        write_state(state)
        try:
            disable_timers()
            wait_service_inactive()
            for unit in SCANNER_SERVICES:
                if active_state(unit) != "inactive":
                    raise SafetyError("EPG scanner did not become inactive")
        except Exception:
            # No receiver operation has occurred yet. Restore the prior timer
            # state while the shared lock still prevents watcher device work.
            try:
                restore_timers(state["timers"])
                if timer_states() == state["timers"]:
                    state["phase"] = "NORMAL"
                    state["last_error"] = "entry precondition failed; timer restored"
                    write_state(state)
                else:
                    state["phase"] = "ENTER_FAILED"
                    write_state(state)
            except Exception:
                state["phase"] = "ENTER_FAILED"
                write_state(state)
            raise

        state["phase"] = "MAINTENANCE"
        write_state(state)
        print("MAINTENANCE: watcher timer disabled; shared watcher lock held.")
        print("Only run the reviewed read/stop/restore steps in the maintenance runbook.")
        subprocess.run(["/bin/bash", "--noprofile", "--norc", "-i"], check=False)
        state["phase"] = "SESSION_EXITED"
        write_state(state)
        print("SESSION_EXITED: timer remains disabled. Use resume only after clean-state review.")
    finally:
        release_barriers(fd)


def service_environment():
    raw = systemctl("show", "--property=Environment", "--value", SERVICE)
    return dict(item.split("=", 1) for item in shlex.split(raw) if "=" in item)


def api_healthy(api_base):
    # Reuse the existing watcher's read-only version/tuner/catalogue checks.
    sys.path.insert(0, str(RUNTIME_DIR))
    import watch

    watch.API_BASE = api_base
    try:
        return watch.api_healthy()[0]
    except Exception:
        return False


def full_restore_healthy(api_base):
    if not api_healthy(api_base):
        return False
    try:
        watch = load_watcher()
        watch.API_BASE = api_base
        watch.verify_proxy_and_epgstation()
        return True
    except Exception:
        return False


def resume(confirm_clean=False):
    if os.geteuid() != 0:
        raise SafetyError("run this command with host administrator privileges")
    if not confirm_clean:
        raise SafetyError("resume requires --confirm-clean after A–E baseline review")
    fds = acquire_barriers()
    try:
        state = read_state()
    except Exception:
        release_barriers(fds)
        raise
    resumable = ("MAINTENANCE", "SESSION_EXITED", "RESTORE_PENDING", "RESTORE_FAILED")
    if not state or state.get("phase") not in resumable:
        release_barriers(fds)
        raise SafetyError("there is no resumable maintenance session")
    try:
        same_fingerprint(state)
        if active_state(SERVICE) != "inactive":
            raise SafetyError("watcher service is active; leave maintenance blocked")
        if not state.get("timers", {}).get(WATCH_TIMER, {}).get("enabled") or not state.get("timers", {}).get(WATCH_TIMER, {}).get("active"):
            raise SafetyError("recorded pre-maintenance timer state is not normal")
        env = service_environment()
        api_base = env.get("MKRA_API")
        if not api_base:
            raise SafetyError("watcher service does not expose a verifiable MKRA_API")
        current_timers = timer_states()
        if state["phase"] in ("MAINTENANCE", "SESSION_EXITED") and any(
            item["enabled"] or item["active"] for item in current_timers.values()
        ):
            raise SafetyError("a timer resumed before clean-state approval")
        if all(current_timers[unit] == old for unit, old in state["timers"].items()):
            if state["phase"] not in ("RESTORE_PENDING", "RESTORE_FAILED"):
                raise SafetyError("watcher timer was resumed before clean-state approval")
        else:
            if state["phase"] in ("MAINTENANCE", "SESSION_EXITED") and any(
                item["enabled"] or item["active"] for item in current_timers.values()
            ):
                raise SafetyError("a timer resumed before clean-state approval")
            # Once a clean-state-approved restore has started, a partial
            # enable/start sequence is recoverable: reapply the exact recorded
            # enabled/active state while both shared locks are held.
            state["phase"] = "RESTORE_PENDING"
            write_state(state)
            restore_timers(state["timers"])
            if timer_states() != state["timers"]:
                state["phase"] = "RESTORE_FAILED"
                write_state(state)
                raise SafetyError("watcher/scanner timer states did not return to their prior values")
    except Exception:
        release_barriers(fds)
        raise

    # Release the shared watcher lock before invoking the oneshot. If the
    # timer fired while locked, systemd coalesces it with this service start.
    release_barriers(fds)
    try:
        systemctl("start", SERVICE, timeout=RESTORE_TIMEOUT)
    except (SafetyError, subprocess.TimeoutExpired):
        # The timer remains enabled and owns normal retry responsibility.
        state["phase"] = "RESTORE_FAILED"
        write_state(state)
        raise

    deadline = time.monotonic() + RESTORE_TIMEOUT
    while time.monotonic() < deadline:
        if full_restore_healthy(api_base):
            state["phase"] = "RESTORED"
            state["restored_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            write_state(state)
            print("RESTORED: API reports the expected healthy tuner and channel catalogue.")
            return
        time.sleep(5)
    state["phase"] = "RESTORE_FAILED"
    write_state(state)
    raise SafetyError("API did not become healthy; timer stays enabled for normal retries")


def status():
    state = read_state()
    print("phase=" + (state.get("phase", "UNKNOWN") if state else "NORMAL"))
    for unit in TIMERS:
        print(unit + "=" + active_state(unit) + "/" + enabled_state(unit))
    print("service=" + active_state(SERVICE))
    try:
        print("runtime_profile=" + runtime_fingerprint()["profile"])
    except SafetyError as exc:
        print("runtime_profile=BLOCKED (" + str(exc) + ")")


def preflight(target_bundle=None):
    """Read-only host/API preflight. It takes no lock and changes no files."""
    if os.geteuid() != 0:
        raise SafetyError("run this command with host administrator privileges")
    state = read_state()
    if state and state.get("phase") not in ("NORMAL", "RESTORED"):
        raise SafetyError("an earlier maintenance attempt is unresolved")
    current = runtime_fingerprint()
    units = {
        unit: {
            "active": active_state(unit),
            "enabled": enabled_state(unit),
            "sha256": unit_fingerprint(unit)["sha256"],
        }
        for unit in (SERVICE, *SCANNER_SERVICES, *TIMERS)
    }
    if active_state(SERVICE) != "inactive" or any(
        active_state(unit) != "inactive" for unit in SCANNER_SERVICES
    ):
        raise SafetyError("watcher or EPG scanner service is active")
    if not (
        units[WATCH_TIMER]["active"] == "active"
        and units[WATCH_TIMER]["enabled"] == "enabled"
    ):
        raise SafetyError("watcher timer is not in its expected enabled/active state")
    read_only_preflight()
    target = fingerprint_bundle_directory(target_bundle) if target_bundle else None
    print(json.dumps({
        "result": "READ_ONLY_PREFLIGHT_PASS",
        "current_runtime": current,
        "target_runtime": target,
        "units": units,
        "note": "snapshot only; enter repeats safety checks while holding shared locks",
    }, sort_keys=True, indent=2))


def main(argv=None):
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    enter_parser = sub.add_parser("enter", help="pause timer and open a lock-holding maintenance shell")
    enter_parser.add_argument("--confirm-preflight", action="store_true")
    enter_parser.add_argument("--target-bundle", type=pathlib.Path,
                              help="pre-authorize one exact schema-v2 runtime bundle for an offline-controlled migration")
    resume_parser = sub.add_parser("resume", help="restore watcher after clean-state review")
    resume_parser.add_argument("--confirm-clean", action="store_true")
    preflight_parser = sub.add_parser("preflight", help="read-only runtime, unit, API and reservation checks")
    preflight_parser.add_argument("--target-bundle", type=pathlib.Path)
    sub.add_parser("status", help="show local watcher maintenance state")
    args = parser.parse_args(argv)
    try:
        if args.command == "enter":
            enter(args.confirm_preflight, args.target_bundle)
        elif args.command == "resume":
            resume(args.confirm_clean)
        elif args.command == "preflight":
            preflight(args.target_bundle)
        else:
            status()
        return 0
    except (SafetyError, OSError, subprocess.TimeoutExpired) as exc:
        print("BLOCKED: " + str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

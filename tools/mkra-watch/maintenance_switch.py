#!/usr/bin/env python3
"""Host-side, fail-closed maintenance barrier for the mkra restore watcher.

This tool never connects to the receiver. `enter` holds the watcher's existing
flock for the lifetime of an interactive shell and persistently disables the
timer. `resume --confirm-clean` only restores the timer and invokes the
existing oneshot watcher after an operator has checked the receiver baseline.
"""
import fcntl
import hashlib
import json
import os
import pathlib
import shlex
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


def runtime_fingerprint():
    manifest_path = RUNTIME_DIR / "bundle.json"
    watcher = RUNTIME_DIR / "watch.py"
    binary = RUNTIME_DIR / "mkra"
    config = RUNTIME_DIR / "bs-channels.tsv"
    try:
        manifest = json.loads(manifest_path.read_text())
        checks = (
            (binary, "binary_size", "binary_sha256"),
            (config, None, "config_sha256"),
        )
        for path, size_key, hash_key in checks:
            if size_key and path.stat().st_size != manifest[size_key]:
                raise SafetyError("watcher bundle binary size mismatch")
            if digest(path) != manifest[hash_key]:
                raise SafetyError("watcher bundle content hash mismatch")
        return {
            name: digest(path)
            for name, path in (
                ("watcher", watcher),
                ("manifest", manifest_path),
                ("binary", binary),
                ("config", config),
            )
        }
    except (OSError, ValueError, KeyError) as exc:
        raise SafetyError("watcher bundle cannot be verified") from exc


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
    if runtime_fingerprint() != state["watcher_bundle"]:
        raise SafetyError("watcher bundle changed during maintenance")
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


def enter(confirm_preflight=False):
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


def main(argv=None):
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    enter_parser = sub.add_parser("enter", help="pause timer and open a lock-holding maintenance shell")
    enter_parser.add_argument("--confirm-preflight", action="store_true")
    resume_parser = sub.add_parser("resume", help="restore watcher after clean-state review")
    resume_parser.add_argument("--confirm-clean", action="store_true")
    sub.add_parser("status", help="show local watcher maintenance state")
    args = parser.parse_args(argv)
    try:
        if args.command == "enter":
            enter(args.confirm_preflight)
        elif args.command == "resume":
            resume(args.confirm_clean)
        else:
            status()
        return 0
    except (SafetyError, OSError, subprocess.TimeoutExpired) as exc:
        print("BLOCKED: " + str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

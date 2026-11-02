#!/usr/bin/env python3
"""Install or roll back one pre-authorized watcher bundle during maintenance.

This tool never contacts the receiver. The maintenance entry must record the
exact schema-v2 target first. It changes only the four runtime files and
bundle.json while systemd timers are disabled and shared locks are held.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile
import sys

import maintenance_switch as maintenance


def _require_root():
    if os.geteuid() != 0:
        raise maintenance.SafetyError("run this command with host administrator privileges")


def _barriers():
    held = []
    for path, label in ((maintenance.WATCH_LOCK, "watcher"), (maintenance.SCAN_LOCK, "EPG scanner")):
        try:
            fd = os.open(path, os.O_RDWR)
        except OSError as exc:
            for old in reversed(held):
                os.close(old)
            raise maintenance.SafetyError(label + " lock file is unavailable") from exc
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            os.close(fd)
            for old in reversed(held):
                os.close(old)
            raise maintenance.SafetyError(label + " lock is held; runtime switch is blocked") from exc
        held.append(fd)
    return held


def _release(fds):
    for fd in reversed(fds):
        os.close(fd)


def _paused_state():
    state = maintenance.read_state()
    if not state or state.get("phase") != "SESSION_EXITED":
        raise maintenance.SafetyError("runtime switch requires a completed maintenance shell")
    if not state.get("target_watcher_bundle"):
        raise maintenance.SafetyError("no exact target bundle was pre-authorized at enter")
    for unit in maintenance.TIMERS:
        if maintenance.active_state(unit) != "inactive" or maintenance.enabled_state(unit) != "disabled":
            raise maintenance.SafetyError("all watcher and scanner timers must remain disabled")
    for unit in (maintenance.SERVICE, *maintenance.SCANNER_SERVICES):
        if maintenance.active_state(unit) != "inactive":
            raise maintenance.SafetyError("watcher or scanner service is active")
    return state


def _copy_durable(source, destination, mode=None):
    shutil.copyfile(source, destination)
    if mode is None:
        mode = stat.S_IMODE(Path(source).stat().st_mode)
    os.chmod(destination, mode)
    with open(destination, "rb") as stream:
        os.fsync(stream.fileno())


def _write_json(path, value, mode=0o600):
    temp = path.with_name(path.name + ".tmp")
    with open(temp, "w") as stream:
        json.dump(value, stream, sort_keys=True, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.chmod(temp, mode)
    os.replace(temp, path)


def _backup_runtime(before):
    base = maintenance.STATE_DIR / "runtime-backups"
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(base, 0o700)
    backup = Path(tempfile.mkdtemp(prefix="r4.5-", dir=base))
    os.chmod(backup, 0o700)
    names = list(maintenance.RUNTIME_FILES)
    if (maintenance.RUNTIME_DIR / "bundle.json").exists():
        names.append("bundle.json")
    for name in names:
        src = maintenance._runtime_file(maintenance.RUNTIME_DIR, name)
        _copy_durable(src, backup / name)
    profile = {
        "schema": 1,
        "runtime_before": before,
        "files": {name: maintenance.digest(backup / name) for name in names},
    }
    _write_json(backup / "backup.json", profile)
    return backup, names, maintenance.digest(backup / "backup.json")


def _install_files(source_dir, names):
    runtime = maintenance.RUNTIME_DIR
    stage = Path(tempfile.mkdtemp(prefix=".r4.5-stage-", dir=runtime))
    try:
        for name in names:
            source = maintenance._runtime_file(source_dir, name)
            _copy_durable(source, stage / name)
        if (stage / "bundle.json").exists():
            target_fingerprint = maintenance.fingerprint_bundle_directory(stage)
            expected = maintenance.fingerprint_bundle_directory(source_dir)
            if target_fingerprint != expected:
                raise maintenance.SafetyError("staged bundle differs from the pre-authorized bundle")
        for name in names:
            os.replace(stage / name, runtime / name)
    finally:
        shutil.rmtree(stage, ignore_errors=True)


def install(bundle_dir):
    _require_root()
    state = _paused_state()
    held = _barriers()
    try:
        state = _paused_state()
        maintenance.same_fingerprint(state)
        target = maintenance.fingerprint_bundle_directory(bundle_dir)
        if target != state["target_watcher_bundle"]:
            raise maintenance.SafetyError("bundle differs from the target approved before maintenance")
        current = maintenance.runtime_fingerprint()
        if current == target:
            print("ALREADY_INSTALLED: runtime matches the pre-authorized bundle")
            return
        if current != state["watcher_bundle"]:
            raise maintenance.SafetyError("current runtime is neither the saved original nor approved target")
        required = sum((Path(bundle_dir) / name).stat().st_size for name in (*maintenance.RUNTIME_FILES, "bundle.json"))
        free = shutil.disk_usage(maintenance.STATE_DIR).free
        if free < required * 2 + 8 * 1024 * 1024:
            raise maintenance.SafetyError("insufficient space for a complete rollback copy and staged bundle")
        backup, old_names, backup_sha = _backup_runtime(current)
        state["runtime_backup"] = str(backup)
        state["runtime_backup_sha256"] = backup_sha
        state["runtime_backup_files"] = old_names
        state["runtime_transition"] = "BACKUP_READY"
        maintenance.write_state(state)
        try:
            _install_files(Path(bundle_dir), [*maintenance.RUNTIME_FILES, "bundle.json"])
            if maintenance.runtime_fingerprint() != target:
                raise maintenance.SafetyError("installed runtime did not match approved target")
        except Exception as exc:
            try:
                _restore_backup(backup, old_names, state["watcher_bundle"])
                state["runtime_transition"] = "INSTALL_FAILED_ROLLED_BACK"
            except Exception as rollback_exc:
                state["runtime_transition"] = "INSTALL_FAILED_ROLLBACK_UNKNOWN"
                maintenance.write_state(state)
                raise maintenance.SafetyError("runtime install and automatic rollback failed; preserve backup") from rollback_exc
            maintenance.write_state(state)
            raise exc
        state["runtime_transition"] = "INSTALLED"
        maintenance.write_state(state)
        print("INSTALLED: approved runtime is active; rollback copy retained at " + str(backup))
    finally:
        _release(held)


def _restore_backup(backup, names, expected_fingerprint):
    backup = Path(backup)
    try:
        metadata_path = backup / "backup.json"
        metadata = json.loads(metadata_path.read_text())
    except (OSError, ValueError) as exc:
        raise maintenance.SafetyError("rollback metadata is unreadable") from exc
    if metadata.get("schema") != 1 or metadata.get("runtime_before") != expected_fingerprint:
        raise maintenance.SafetyError("rollback metadata does not match the saved original runtime")
    if any(maintenance.digest(backup / name) != metadata["files"].get(name) for name in names):
        raise maintenance.SafetyError("rollback file hash mismatch")
    if maintenance.fingerprint_runtime_directory(backup) != expected_fingerprint:
        raise maintenance.SafetyError("rollback bundle does not match the saved original runtime")
    _install_files(backup, names)
    if "bundle.json" not in names:
        manifest = maintenance.RUNTIME_DIR / "bundle.json"
        if manifest.is_symlink() or manifest.exists():
            manifest.unlink()
    if maintenance.runtime_fingerprint() != expected_fingerprint:
        raise maintenance.SafetyError("restored runtime does not match the saved original")


def rollback():
    _require_root()
    state = _paused_state()
    held = _barriers()
    try:
        state = _paused_state()
        backup = state.get("runtime_backup")
        names = state.get("runtime_backup_files")
        if not backup or not isinstance(names, list):
            raise maintenance.SafetyError("no recorded runtime backup is available")
        expected_sha = state.get("runtime_backup_sha256")
        if maintenance.digest(Path(backup) / "backup.json") != expected_sha:
            raise maintenance.SafetyError("rollback record hash mismatch")
        _restore_backup(backup, names, state["watcher_bundle"])
        state["runtime_transition"] = "ROLLED_BACK"
        maintenance.write_state(state)
        print("ROLLED_BACK: original runtime hashes restored; backup retained at " + backup)
    finally:
        _release(held)


def main(argv=None):
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    install_parser = sub.add_parser("install", help="install the pre-authorized bundle under maintenance locks")
    install_parser.add_argument("--bundle", required=True, type=Path)
    sub.add_parser("rollback", help="restore the exact original runtime from its private backup")
    args = parser.parse_args(argv)
    try:
        if args.command == "install":
            install(args.bundle)
        else:
            rollback()
        return 0
    except (maintenance.SafetyError, OSError, ValueError, KeyError) as exc:
        print("BLOCKED: " + str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

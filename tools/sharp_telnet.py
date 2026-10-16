#!/usr/bin/env python3
"""
SHARP 4S-C00AS1 read-only telnet driver.

telnetlib was removed in Python 3.13, so this implements just enough of
RFC 854 option negotiation to drive busybox's telnetd on port 2323.

SAFETY: this module only transports commands. It refuses to send anything
matching the destructive-command blocklist below, so an accidental typo can
never kill a process, remount, write to eMMC or poke I2C.
"""
import base64
import hashlib
import os
import re
import socket
import sys
import time

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
OPT_ECHO, OPT_SGA, OPT_NAWS, OPT_TTYPE = 1, 3, 31, 24

# --- hard safety net -------------------------------------------------------
# Commands that must never be EXECUTED. Matched against the command word only
# (basename of the first token of each pipeline/list segment), so passing
# "/usr/bin/nimd" as an *argument* to ls/readelf/cat stays allowed.
FORBIDDEN_CMDS = {
    "kill", "killall", "pkill", "reboot", "halt", "poweroff", "shutdown",
    "mount", "umount", "mkfs", "mke2fs", "tune2fs", "fdisk", "mkswap",
    "insmod", "rmmod", "modprobe", "depmod",
    "i2cset", "i2ctransfer", "i2cdetect", "i2cdump",
    "nimd", "tuner", "test_tune",              # product binaries: never launch
    "rm", "rmdir", "mv", "chmod", "chown", "chgrp", "truncate", "ln",
    "telnetd", "start-stop-daemon", "init", "sync", "hdparm", "mmc",
    # SPI flash / U-Boot environment: OFF LIMITS for this session by user
    # decision (security risk, and an external ROM-writer backup already
    # exists). Blocked even for read-only flags so it cannot happen by typo.
    "spiflash", "rootbanktool", "mkenvimage",
    "RMacKey", "WMacKey", "RDtcpIpKey", "WDtcpIpKey",
}
# /dev/spidev0.0 is the boot SPI flash -- never address it at all.
SPI_FLASH_DEV = re.compile(r"/dev/spidev0\.0\b")
ENVASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")

# Shell keywords that precede the real command word. Without skipping these,
# `if true; then reboot; fi` slips past because the first token is `then`.
SHELL_KEYWORDS = {
    "if", "then", "else", "elif", "fi", "while", "until", "do", "done",
    "case", "esac", "for", "in", "select", "function", "time", "!",
    "{", "}", "(", ")", "[[", "]]",
}

# Multiplexers / interpreters: the real command hides in an argument, so we
# cannot judge it from the command word alone. Block outright.
INDIRECTION = {"busybox", "eval", "exec", "env", "nohup", "setsid", "xargs",
               "sh", "bash", "ash", "dash", "chroot", "su", "sudo", "watch",
               "timeout", "nice", "ionice", "start-stop-daemon", "run-parts"}

# Devices that must never be written or even opened for write. /dev/ram0 backs
# the live squashfs rootfs; /dev/mem is all of physical memory; gp0/gp1 hold the
# ONLY copy of the encrypted rootfs image (the only copy of the encrypted rootfs image).
DANGEROUS_DEVS = re.compile(
    r"/dev/(ram\d+|root|mem|kmem|port|mmcblk0(boot[01]|gp[0-3])?|mtd\S*|spidev0\.0)\b")

SUBST = re.compile(r"\$\(([^()]*)\)|`([^`]*)`")

# Micro-test sandbox (docs/SETUP.md). tmpfs, wiped by a power cycle, and the
# only place upload()/chmod_exec()/remove_microtest() will touch. The character
# class is deliberately narrow: no spaces, no `..`, no shell metacharacters, so
# a validated path cannot carry a second command into the raw shell line.
MICROTEST_DIR = "/tmp/mt"
MICROTEST_PATH = re.compile(r"^/tmp/mt/[A-Za-z0-9][A-Za-z0-9._-]*$")

# Processes that live under /tmp/mt but carry the box's only management link.
# On the WiFi-only configuration, terminating one of these strands the device
# until someone uses the serial console or power-cycles it. Measured
# 2026-08-04: deploy_mkra.py killed wpa_supplicant here and lost the box.
LINK_CRITICAL_BASENAMES = frozenset({"wpa_supplicant", "udhcpc", "dhcpcd", "telnetd"})

# Upload pacing.
#
# The box's shell drops any command line past roughly 1024 bytes, silently:
# the tail is discarded, the closing quote goes with it, and the session then
# hangs on the `> ` continuation prompt. Measured 2026-07-31 -- 800 bytes of
# payload round-trips, 1000 does not. run() adds ~100 bytes of sentinel
# wrapper, so the base64 chunk has to stay well under that.
UPLOAD_CHUNK = 800


def assert_microtest_path(path):
    """Reject anything that is not a plain file directly under /tmp/mt."""
    if not MICROTEST_PATH.match(path or ""):
        raise ValueError(
            "refusing to touch %r: writable operations are limited to "
            "%s/<name> (docs/SETUP.md)" % (path, MICROTEST_DIR))
    return path


def split_segments(cmd):
    """Split a shell line into command segments on ; | & && || -- but only
    outside quotes.

    A naive regex split corrupts quoted regexes: `grep -iE 'a|reboot|b'` would
    be cut at the pipes inside the quotes, leaving a bogus segment that starts
    with `reboot` and tripping the blocklist. That false positive is why this
    is hand-rolled instead of a one-line re.split.
    """
    segs, cur = [], []
    quote = None
    i = 0
    while i < len(cmd):
        c = cmd[i]
        if quote:
            cur.append(c)
            if c == quote:
                quote = None
            elif c == "\\" and quote == '"' and i + 1 < len(cmd):
                cur.append(cmd[i + 1]); i += 1
            i += 1
            continue
        if c in "'\"":
            quote = c; cur.append(c); i += 1; continue
        if c == "\\" and i + 1 < len(cmd):
            cur.append(cmd[i:i + 2]); i += 2; continue
        if c in ";|&\n":
            segs.append("".join(cur)); cur = []
            # swallow the second char of || and &&
            if i + 1 < len(cmd) and cmd[i + 1] == c:
                i += 1
            i += 1
            continue
        cur.append(c); i += 1
    segs.append("".join(cur))
    return segs
DD_WRITE = re.compile(r"\bdd\b[^|;]*\bof=")
# allow redirection only to /tmp (tmpfs = RAM) or /dev/null|stdout|stderr.
#
# `(?<![->])` keeps an arrow like `->` from reading as a redirection, and also
# stops a match from starting on the SECOND `>` of an append.
# `(?![>])` forces the arrow to be matched at full width.
#
# Both guards are needed. Without them the engine backtracks `>>` into a single
# `>` whose "target" is the remaining `>`, which is not an allowed prefix, so
# EVERY append was reported as a write outside /tmp -- including the ones to
# /tmp that upload() depends on. (False positive found 2026-07-31 while
# uploading mkra; `>` was fine, `>>` was never allowed anywhere.)
REDIR_WRITE = re.compile(
    r"(?<![->])>>?(?![>])\s*(?!/tmp/|/dev/null|/dev/stdout|/dev/stderr|&)\S")


def mask_quotes(cmd):
    """Blank out quoted spans so shell-syntax checks never fire on string
    contents -- e.g. a `|` inside a grep regex, or `-> $l` inside an echo.
    Both of those produced false positives before this existed."""
    out = []
    quote = None
    i = 0
    while i < len(cmd):
        c = cmd[i]
        if quote:
            out.append(" ")
            if c == quote:
                quote = None
            i += 1
            continue
        if c in "'\"":
            quote = c; out.append(" "); i += 1; continue
        out.append(c); i += 1
    return "".join(out)
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def assert_safe(cmd, _depth=0):
    """Refuse to transmit a command line that could execute anything destructive.

    LIMITS -- read these before relying on it:
      * It is a heuristic on shell text, not a sandbox. It reduces the blast
        radius of a typo; it does not contain a determined caller.
      * SharpTelnet.run(..., raw=True) skips this check entirely. Only login()
        legitimately uses raw=True. Never copy that idiom.
      * Interpreters and multiplexers (sh -c, busybox X, eval, xargs, ...) hide
        the real command in an argument, so they are blocked outright rather
        than analysed.
    """
    if _depth > 4:
        raise RuntimeError("BLOCKED (command substitution nested too deep): %r" % cmd)

    # command substitution / backticks run their contents as commands too
    for m in SUBST.finditer(cmd):
        inner = m.group(1) or m.group(2) or ""
        if inner.strip():
            assert_safe(inner, _depth + 1)

    for seg in split_segments(cmd):
        toks = seg.strip().split()
        # skip leading env assignments and shell keywords
        while toks and (ENVASSIGN.match(toks[0])
                        or toks[0].strip("(){}") in SHELL_KEYWORDS
                        or toks[0] in ("", "(", ")", "{", "}", "!")):
            toks = toks[1:]
        if not toks:
            continue
        word = toks[0].strip("(){}")
        base = word.rsplit("/", 1)[-1]
        if base in FORBIDDEN_CMDS:
            raise RuntimeError("BLOCKED (would execute %r): %r" % (base, cmd))
        if base in INDIRECTION:
            raise RuntimeError(
                "BLOCKED (%r hides the real command in an argument; the guard "
                "cannot judge it): %r" % (base, cmd))
    bare = mask_quotes(cmd)          # shell syntax only, quoted text neutralised
    if DD_WRITE.search(bare):
        raise RuntimeError("BLOCKED (dd of=): %r" % cmd)
    # Authorized volatile network setting required after each cold boot.
    # Match the whole command, so this cannot authorize other proc writes,
    # values, appended commands, or persistent sysctl configuration.
    volatile_tcp_fix = cmd == "echo 0 > /proc/sys/net/ipv4/tcp_tw_recycle"
    if REDIR_WRITE.search(bare) and not volatile_tcp_fix:
        raise RuntimeError("BLOCKED (write outside /tmp): %r" % cmd)
    # path checks run on the original: a quoted path is still a path
    if SPI_FLASH_DEV.search(cmd):
        raise RuntimeError("BLOCKED (boot SPI flash is off limits): %r" % cmd)
    # Any redirection or dd targeting a dangerous device. Reading them is fine
    # (that is how the backups were taken), so only writes are refused.
    m = DANGEROUS_DEVS.search(bare)
    if m:
        dev = m.group(0)
        wrote = re.search(r"(?:>>?|\bof=)\s*" + re.escape(dev), bare)
        if wrote:
            raise RuntimeError(
                "BLOCKED (write to %s -- /dev/ram0 backs the live rootfs, "
                "gp0/gp1 hold the only copy of the encrypted rootfs image, "
                "/dev/mem is physical memory): %r" % (dev, cmd))


class SharpTelnet:
    def __init__(self, host="198.51.100.10", port=2323, timeout=20, verbose=False):
        self.host, self.port, self.timeout = host, port, timeout
        self.verbose = verbose
        self.buf = b""
        self.seq = 0
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)

    # -- low level ---------------------------------------------------------
    def _reply(self, verb, opt):
        self.sock.sendall(bytes([IAC, verb, opt]))

    def _feed(self, data):
        """Strip/answer telnet negotiation, return payload bytes."""
        out = bytearray()
        i = 0
        while i < len(data):
            b = data[i]
            if b != IAC:
                out.append(b); i += 1; continue
            if i + 1 >= len(data):
                break
            verb = data[i + 1]
            if verb == IAC:                      # escaped 0xFF
                out.append(IAC); i += 2; continue
            if verb in (DO, DONT, WILL, WONT):
                if i + 2 >= len(data):
                    break
                opt = data[i + 2]
                if verb == DO:
                    # we only ever accept SGA; refuse everything else
                    self._reply(WILL if opt == OPT_SGA else WONT, opt)
                elif verb == WILL:
                    self._reply(DO if opt in (OPT_ECHO, OPT_SGA) else DONT, opt)
                i += 3; continue
            if verb == SB:                       # skip subnegotiation
                j = data.find(bytes([IAC, SE]), i)
                i = (j + 2) if j >= 0 else len(data)
                continue
            i += 2
        return bytes(out)

    def read_until(self, needle, timeout=None):
        """Read until `needle` appears. Only the freshly-arrived tail is
        rescanned each round, so multi-MiB transfers stay linear rather than
        quadratic (matters for the ~2 GiB eMMC backup)."""
        needle = needle.encode() if isinstance(needle, str) else needle
        deadline = time.time() + (timeout or self.timeout)
        scanned = 0
        while True:
            idx = self.buf.find(needle, max(0, scanned - len(needle)))
            if idx >= 0:
                break
            scanned = len(self.buf)
            if time.time() > deadline:
                raise TimeoutError("timeout waiting for %r; tail=%r"
                                   % (needle, self.buf[-300:]))
            self.sock.settimeout(max(0.5, deadline - time.time()))
            try:
                chunk = self.sock.recv(262144)
            except socket.timeout:
                continue
            if not chunk:
                raise ConnectionError("connection closed; tail=%r" % self.buf[-300:])
            self.buf += self._feed(chunk)
        end = idx + len(needle)
        data, self.buf = self.buf[:end], self.buf[end:]
        return data

    def send(self, s):
        self.sock.sendall(s.encode() if isinstance(s, str) else s)

    # -- session -----------------------------------------------------------
    def login(self, user="root", password=None):
        """root has no password on this unit; handle a prompt anyway."""
        deadline = time.time() + self.timeout
        got_login = False
        while time.time() < deadline:
            try:
                self.sock.settimeout(2)
                chunk = self.sock.recv(65536)
                if chunk:
                    self.buf += self._feed(chunk)
            except socket.timeout:
                pass
            low = self.buf.lower()
            if not got_login and (b"login:" in low or b"username:" in low):
                self.send(user + "\n"); got_login = True; self.buf = b""; continue
            if b"password:" in low:
                self.send((password or "") + "\n"); self.buf = b""; continue
            if got_login and (b"#" in self.buf or b"$" in self.buf):
                break
            if not got_login and time.time() > deadline - self.timeout + 5:
                # no login prompt seen: maybe already at a shell
                self.send("\n"); got_login = True
        # settle: quiet the shell down and widen the pty so output stops wrapping
        # (both are session-local tty/env settings; nothing is written to flash)
        self.run("export PS1='' ; export TERM=dumb ; stty cols 4000 2>/dev/null",
                 timeout=15, raw=True)
        return True

    def run(self, cmd, timeout=60, raw=False):
        """Execute one command, return its stdout+stderr as text.

        Uses a unique sentinel instead of prompt matching, so multi-line and
        slow output is handled reliably.
        """
        if not raw:
            assert_safe(cmd)
        self.seq += 1
        beg = "__SHARPBEG_%d__" % self.seq
        end = "__SHARPEOC_%d__" % self.seq
        # busybox telnetd echoes the command line back, and the pty wraps long
        # lines, so the echo can span several lines. Split each sentinel with
        # empty quotes: the ECHOED text never contains the joined marker, only
        # the command's real output does. Then slice strictly between BEG/END.
        b_emit = '__SHARP""BEG_%d__' % self.seq
        e_emit = '__SHARP""EOC_%d__' % self.seq
        self.buf = b""
        self.send('echo "%s" ; %s 2>&1 ; echo "%s"\n' % (b_emit, cmd, e_emit))
        data = self.read_until(end, timeout=timeout)
        txt = data.decode("utf-8", "replace").replace("\r\n", "\n").replace("\r", "\n")
        txt = ANSI.sub("", txt)
        if beg in txt:
            txt = txt.split(beg, 1)[1]
        txt = txt.rsplit(end, 1)[0]
        return txt.strip("\n")

    # --- upload ------------------------------------------------------------
    # There is no nc/ftp on the box, but busybox has base64,
    # so files go up the same way the rootfs came down -- just reversed.
    #
    # Two of the steps (`chmod +x`, `rm`) are on the blocklist. Rather than
    # unblock those verbs globally -- which would turn the guard from "stops
    # eMMC destruction" into "stops nothing" (§1.1) -- each is wrapped in a
    # helper that validates the argument against MICROTEST_PATH first and only
    # then bypasses the check for that one fixed command shape.

    def _batched(self, lines, timeout=120):
        """Send several command lines, then sync on a single sentinel.

        ★ The caller must keep the TOTAL bytes of one batch under the pty's
        input buffer (4096 bytes on Linux). The shell stops reading the pty
        while it forks each command, so anything past the buffer is silently
        DISCARDED -- which shows up as a truncated line, an unterminated quote,
        and a hang at the `> ` continuation prompt rather than as an error.
        That is why UPLOAD_BATCH is 1: a 3 KB chunk plus its sentinel already
        fills most of the buffer. (Observed 2026-07-30 on this box.)
        """
        self.seq += 1
        end = "__SHARPEOC_%d__" % self.seq
        e_emit = '__SHARP""EOC_%d__' % self.seq
        self.buf = b""
        for line in lines:
            self.send(line + "\n")
        self.send('echo "%s"\n' % e_emit)
        data = self.read_until(end, timeout=timeout)
        return data.decode("utf-8", "replace")

    def make_microtest_dir(self):
        """Create /tmp/mt. tmpfs, so it vanishes on reboot (docs/SETUP.md)."""
        self.run("mkdir -p %s" % MICROTEST_DIR)

    def chmod_exec(self, path):
        """`chmod +x <path>` for one validated path under /tmp/mt."""
        assert_microtest_path(path)
        return self.run("chmod +x %s" % path, raw=True)

    def remove_microtest(self, path):
        """`rm -f <path>` for one validated path under /tmp/mt."""
        assert_microtest_path(path)
        return self.run("rm -f %s" % path, raw=True)

    def signal_microtest(self, pid, signal_name="TERM"):
        """Signal one process, but only after proving it is our own binary.

        `kill` is blocklisted because killing a product process can make the
        power MCU treat the box as failed (docs/LIMITATIONS.md). We still need to be
        able to stop the micro-test app we started, so the pid is resolved
        through /proc/<pid>/exe first and the signal only happens if that lands
        inside /tmp/mt. A pid that has exited, or that belongs to anything
        else, is refused rather than signalled. Only the two normal shutdown
        signals needed by project-owned diagnostics are accepted.
        """
        signal_name = str(signal_name).strip().upper()
        if signal_name not in ("INT", "TERM"):
            raise ValueError("signal_name must be INT or TERM")
        pid = str(pid).strip()
        if not pid.isdigit():
            raise ValueError("pid must be numeric, got %r" % pid)
        exe = self.run("readlink /proc/%s/exe" % pid).strip()
        try:
            assert_microtest_path(exe)
        except ValueError:
            raise RuntimeError(
                "refusing to kill pid %s: its exe is %r, which is not under %s"
                % (pid, exe, MICROTEST_DIR))
        self.run("kill -%s %s" % (signal_name, pid), raw=True)
        return exe

    def stop_microtest(self, pid):
        """Terminate one validated /tmp/mt process with SIGTERM."""
        return self.signal_microtest(pid, "TERM")

    def running_microtests(self, include_link_critical=False):
        """[(pid, exe)] for processes running out of /tmp/mt.

        `/tmp/mt` is no longer diagnostics-only: the USB-WiFi bring-up payload
        lives there too, and on a WiFi-only box `wpa_supplicant`/`udhcpc` are
        the entire management link. Killing them disconnects the box with no
        way back except the serial console or a power cycle. They are excluded
        unless a caller explicitly asks for them.
        """
        out = self.run("ls -l /proc/[0-9]*/exe 2>/dev/null | grep '%s/'"
                       % MICROTEST_DIR)
        found = []
        for line in out.splitlines():
            m = re.search(r"/proc/(\d+)/exe -> (\S+)", line)
            if not m:
                continue
            exe = m.group(2)
            if not include_link_critical and \
                    os.path.basename(exe) in LINK_CRITICAL_BASENAMES:
                continue
            found.append((m.group(1), exe))
        return found

    def upload(self, local_path, remote_path, chunk=UPLOAD_CHUNK,
               progress=None):
        """Copy a local file to /tmp/mt/<name> and verify it by md5.

        Returns the device-side md5. Raises if it does not match the local
        file, so a truncated transfer can never be mistaken for a good one.
        """
        assert_microtest_path(remote_path)
        with open(local_path, "rb") as fh:
            payload = fh.read()
        want = hashlib.md5(payload).hexdigest()
        b64 = base64.b64encode(payload).decode("ascii")
        staging = remote_path + ".b64"
        assert_microtest_path(staging)

        self.make_microtest_dir()
        # A dropped telnet session can leave both files partially written.
        # On this BusyBox shell `printf '' > file` did not reliably truncate
        # that stale staging file: the next upload appended a second base64
        # stream and decoded to old-prefix + new-binary.  Exact-path removal is
        # safe here because both names passed assert_microtest_path above.
        self.remove_microtest(staging)
        self.remove_microtest(remote_path)
        self.run("printf '' > %s" % staging)

        # One line per round trip, through run() so assert_safe still applies.
        # Pipelining several lines is not safe here: the shell stops reading
        # the pty while it forks each echo, and the overflow is dropped.
        pieces = [b64[i:i + chunk] for i in range(0, len(b64), chunk)]
        for n, piece in enumerate(pieces, 1):
            self.run("echo -n '%s' >> %s" % (piece, staging), timeout=30)
            if progress and (n % 25 == 0 or n == len(pieces)):
                progress(n, len(pieces))

        self.run("base64 -d %s > %s" % (staging, remote_path))
        got = self.run("md5sum %s" % remote_path).split()[0]
        if got != want:
            size = self.run("wc -c < %s" % remote_path).strip()
            raise RuntimeError(
                "upload of %s failed: md5 %s on device, %s locally "
                "(device size %s, expected %d)"
                % (remote_path, got, want, size, len(payload)))
        self.remove_microtest(staging)
        self.chmod_exec(remote_path)
        return got

    def close(self):
        try:
            self.send("exit\n")
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass


def connect(host="198.51.100.10", port=2323, timeout=20):
    t = SharpTelnet(host, port, timeout)
    t.login()
    return t


if __name__ == "__main__":
    t = connect()
    print(t.run("id; uname -a"))
    t.close()

#!/usr/bin/env python3
"""Patch the U-Boot environment inside a 4S-C00AS1 SPI flash dump.

The only change is the `set_bootargs` variable: it gains a background job that
starts busybox telnetd on port 2323 before `exec /linuxrc secureboot` runs.
Everything else in the 2 MiB image -- U-Boot itself, `ethaddr`, every other
variable, and the byte order of the variable list -- is preserved.

    python3 tools/analysis/patch_bootargs.py dump1.bin -o telnet.bin
    python3 tools/analysis/patch_bootargs.py dump1.bin --show      # read only

Then verify and flash:

    cmp -l dump1.bin telnet.bin | wc -l        # a few hundred bytes, all in 0x1ff000..
    flashrom -p ch341a_spi -c W25Q16JV_M -w telnet.bin

★ Never flash somebody else's image: `ethaddr` in this area is the unit's own
  MAC address. Always dump your own chip and patch that.

★ The `init=` script token must not contain a dot. `init/main.c`'s
  unknown_bootoption() throws away any token that has a `.` and no `=`, taking
  the whole -c script with it, which leaves `sh -c secureboot` -> PID 1 dies ->
  panic. That is why the hook calls `/mmc/bootsh`, not `/mmc/boot.sh`.
  This is why the hook below has no file extension.
"""
import argparse
import binascii
import re
import struct
import sys

FLASH_SIZE = 0x200000
ENV_OFFSET = 0x1FF000
ENV_SIZE = 0x1000

# Measured on the retail unit (evidence/boot_environment/dump1.bin).
STOCK = ("setenv bootargs ethaddr=$ethaddr root=/dev/ram0 "
         "initrd=$initrd_addr,$initrd_size init=linuxrc superquiet")

# evidence/boot_environment/stage4_telnet_normalboot.bin -- booted and gave a
# root telnet on 2323 with the product software otherwise untouched. `${IFS}`
# stands in for the spaces U-Boot would otherwise split on; the sleep has to
# come first, because a job that forks immediately does not survive early boot
# Measured: a job that forks immediately does not survive early boot.
TELNET = ("setenv bootargs ethaddr=$ethaddr root=/dev/ram0 "
          "initrd=$initrd_addr,$initrd_size console=ttyS0,115200 "
          "init=/bin/sh -c '(/bin/busybox${IFS}sleep${IFS}15;"
          "/bin/busybox${IFS}telnetd${IFS}-p${IFS}2323${IFS}-l${IFS}/bin/sh)"
          "&exec${IFS}/linuxrc${IFS}secureboot'")

# stage10: same, plus one call to /mmc/bootsh for persistent setup (WiFi etc).
# telnetd stays ahead of it and independent, so a broken bootsh cannot lock you
# out. Only useful once /mmc/bootsh actually exists.
TELNET_BOOTSH = TELNET.replace(
    "-l${IFS}/bin/sh)",
    "-l${IFS}/bin/sh;/bin/sh${IFS}/mmc/bootsh)")


def split_env(env):
    """Return the variable list as [b'name=value', ...]."""
    body = env[4:]
    used = body.split(b"\x00\x00", 1)[0]
    return [v for v in used.split(b"\x00") if v]


def build_env(entries):
    body = b"\x00".join(entries) + b"\x00\x00"
    if len(body) > ENV_SIZE - 4:
        sys.exit("patched environment is %d bytes, the sector holds %d"
                 % (len(body) + 4, ENV_SIZE))
    body += b"\x00" * (ENV_SIZE - 4 - len(body))
    return struct.pack("<I", binascii.crc32(body) & 0xFFFFFFFF) + body


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dump", help="2 MiB image read back from the SPI flash")
    ap.add_argument("-o", "--out", help="where to write the patched image")
    ap.add_argument("--show", action="store_true",
                    help="print the environment and exit without writing")
    ap.add_argument("--bootsh", action="store_true",
                    help="also call /mmc/bootsh (stage 10). Only do this once "
                         "/mmc/bootsh exists on the box")
    ap.add_argument("--bootdelay", type=int,
                    help="set bootdelay so U-Boot's prompt is reachable over "
                         "the serial console (3 is a good value; stock is 0)")
    ap.add_argument("--stdlog", choices=("product", "devel"),
                    help="product log verbosity on the serial console. `devel` "
                         "raises the level eDbgPrintf checks; stock is product")
    ap.add_argument("--revert", action="store_true",
                    help="restore the stock set_bootargs instead")
    a = ap.parse_args()

    image = bytearray(open(a.dump, "rb").read())
    if len(image) != FLASH_SIZE:
        sys.exit("expected a %d byte dump, got %d -- wrong chip or a short read"
                 % (FLASH_SIZE, len(image)))

    env = bytes(image[ENV_OFFSET:ENV_OFFSET + ENV_SIZE])
    stored = struct.unpack("<I", env[:4])[0]
    calc = binascii.crc32(env[4:]) & 0xFFFFFFFF
    if stored != calc:
        sys.exit("environment CRC mismatch (stored %#010x, computed %#010x). "
                 "Re-read the chip; do not patch a bad dump." % (stored, calc))

    entries = split_env(env)
    if a.show:
        print("env at %#x, CRC %#010x, %d variables" % (ENV_OFFSET, stored, len(entries)))
        for e in entries:
            print("  " + e.decode("latin1"))
        return

    if not a.out:
        sys.exit("--out is required unless --show is given")

    want = STOCK if a.revert else (TELNET_BOOTSH if a.bootsh else TELNET)
    changed = []
    for i, e in enumerate(entries):
        name, _, value = e.partition(b"=")
        if name == b"set_bootargs":
            old = value.decode("latin1")
            if old == want:
                sys.exit("set_bootargs already has the requested value")
            if not a.revert and old != STOCK:
                print("NOTE: set_bootargs is not the stock value. Current:\n"
                      "  %s" % old, file=sys.stderr)
            entries[i] = b"set_bootargs=" + want.encode("latin1")
            changed.append("set_bootargs")
        elif name == b"bootdelay" and a.bootdelay is not None:
            entries[i] = b"bootdelay=%d" % a.bootdelay
            changed.append("bootdelay")
        elif name == b"stdlog" and a.stdlog is not None:
            entries[i] = b"stdlog=" + a.stdlog.encode("ascii")
            changed.append("stdlog")

    if "set_bootargs" not in changed:
        sys.exit("no set_bootargs variable in this environment")

    # A dot in an init= -c token is fatal (see the module docstring).
    script = re.search(r"init=/bin/sh -c '([^']*)'", want)
    if script and "." in script.group(1):
        sys.exit("refusing to write: the init= script token contains a dot")

    image[ENV_OFFSET:ENV_OFFSET + ENV_SIZE] = build_env(entries)
    with open(a.out, "wb") as fh:
        fh.write(image)

    diff = sum(1 for x, y in zip(open(a.dump, "rb").read(), bytes(image)) if x != y)
    print("wrote %s" % a.out)
    print("changed: %s" % ", ".join(changed))
    print("bytes differing from the input: %d (all inside %#x..%#x)"
          % (diff, ENV_OFFSET, ENV_OFFSET + ENV_SIZE))
    print("new set_bootargs:\n  %s" % want)


if __name__ == "__main__":
    main()

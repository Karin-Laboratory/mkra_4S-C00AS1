# BS2K receive path: minimal upstreamable findings

This note extracts the **tuning/front-end** portion of the experimentally validated
ISDB-S (ordinary BS/2K) path on SHARP 4S-C00AS1. The complete integrated
implementation remains in Karin-Laboratory's fork and is not part of this patch.

## Verified receive chain

```text
SC1501A ISDB-S lock
  -> physical TSI0 (188-byte serial MPEG-TS)
  -> logical TSI1 (xcodedrv input mapping)
  -> type-12 MPEG-TS DEMUX, input port 1
  -> SYSTEM/TS buffer + host reader
  -> 188-byte MPEG-TS packets
```

Lock alone does not yield packets. All stages must agree on physical and logical
port selection. In particular, requesting DEMUX input port 0 produced zero bytes;
input port 1 with physical TSI0 mapped to logical TSI1 produced packets.

### Tuning interface

- Legacy ISDB-S uses the non-4K FED rather than the advanced-BS FED.
- ISDB-S RF frequency is encoded as packed BCD of RF kHz / 10; advanced BS
  passes the IF kHz as an integer. Treating these formats as interchangeable
  results in invalid tuning.
- For BS Asahi: RF 11,727,480 kHz, TSID `0x4010`, wire frequency
  `0x01172748`.
- Tuning BS2K and BS4K competes for the same physical frontend on this box;
  do not assume simultaneous reception.

### Recorded capture (2026-10-16)

One 15-second BS Asahi capture yielded 32,932,148 bytes, 175,171 packets
and about 17.56 Mbps. Every packet had the 188-byte sync framing; TEI errors,
continuity counter errors on non-null PIDs, and CRC errors in the 1,690 complete
PSI/SI sections were all zero. PAT identified TSID `0x4010` and services
151/152/153. These are **historic device observations**, not new CI/device
validation performed as part of this PR.

## What this PR adds

This is a deliberately small **raw MPEG-TS capture path**, rather than the
complete BS2K/BS4K server:

- `src/tuner.rs`: legacy ISDB-S tuning and FEM/FED command primitives.
- `src/xcdrv.rs`: measured logical TSI1 -> physical TSI0 crossbar mapping,
  saved-state restoration and necessary DEMUX/TS buffer support.
- `src/tssrc.rs`: ring-reader handling, including zero-length descriptor commits.
- `src/bs2k_raw.rs`: owned graph (pipeline, AV sync, DEMUX, TS ring, reader)
  with cleanup and fail-closed preflight, **without CAS or MULTI2**.
- `src/main.rs`: bounded, opt-in `bs2k-raw` command writing TS bytes to disk.

The patch **does not** implement access-controlled content descrambling,
Mirakurun/HTTP services, EPGStation, TV scheduling, or 2K/4K arbitration. The
resulting raw transport stream may retain scrambled payloads. This is **not**
equivalent to the integrated fork's historic live decoded-video result.

Example for an isolated test unit only, after ensuring native viewing and
recording have stopped:

```sh
mkra bs2k-raw --out /tmp/bs2k.ts --seconds 5 --freq 11727480 --tsid 4010
```

The output path is a RAM file on the target device. No device commands were
run as part of preparing this PR. The 15-second successful reception reported
above comes from the full experimental fork and **is not evidence of successful
operation of this extracted branch**.

### Limitations / operational safety

- Source-only contribution; no receiver was tuned or restarted for this PR.
- Avoid simultaneous tune requests, especially when native viewing or
  recording is in progress.
- The vendor frontend and interface are firmware-specific. Other
  firmware/device variants are unverified.
- A failed graph teardown must not silently be treated as a clean restart.
- 2K-to-4K native HDMI restoration remains unverified; HTTP streaming
  restoration and HDMI recovery are different.

### Device attempt (2026-11-01): FAIL; stopped after cleanup error

This is a single bounded attempt on the documented test receiver. It does not
change the historic 2026-10-16 capture result above.

- **Build:** PR branch HEAD `e8a0b654d921cbc7062a7635eb72c3fa518c3715` passed
  `cargo test --locked` (202 passed, 0 failed) and the ARMv7 musl release
  build. The resulting binary was a statically linked ARM EABI5 ELF, SHA-256
  `6c1dbe9882fa929bf13a89d8567500088998c54cb7c96841abe60b745787bb90`.
- **Preflight:** the OEM tuner/driver/library fingerprints matched the
  implementation's expected firmware guard; the API reported the tuner free,
  with no active recording and no active tuner users. No future reservation
  was due during the attempt. The existing integrated `serve` process and
  watcher were left running. The BS2K EPG scanner was already in a failed,
  inactive state.
- **Staging:** copied the test binary to a new `/tmp/mt` filename without
  replacing the integrated binary. The receiver-side SHA-256 matched the
  build host. The staged file remains in receiver RAM for diagnosis.
- **Attempt:** ran one 5-second BS Asahi `bs2k-raw` command at RF
  `11727480` kHz / TSID `0x4010`. It exited unsuccessfully while restoring the
  logical TSI1 identity / physical TSI1 raw words, reporting OS error 1032,
  followed by `cleanup_ok=false` and “device state requires inspection”.
  The expected output TS file was absent; **0 TS bytes were captured**.
- **Afterward:** no test process remained; the OEM `tuner`, `card`, `abs`, and
  `adsb` processes remained present. The read-only API still reported the
  tuner free and not faulted. A documented read-only graph snapshot showed two
  pipelines and multiple buffers; without a known baseline for this precise
  state, that snapshot cannot establish whether the failed test graph was
  fully released. No recovery, retune, reset, or retry was attempted.
- **Result:** real-device raw TS reception is **FAIL** for this attempt.
  Packet sync, TSID, PAT, PMT, TEI, continuity counters, and PSI/SI CRC are
  **UNKNOWN** because there is no output sample. TSI restoration and complete
  graph cleanup are **UNKNOWN / unresolved**. HDMI state was not verified and
  must not be described as restored. Stop further tuning until the device
  state and a safe recovery path are independently established.

No receiver IPs, credentials, recordings, or TS payloads are included here.

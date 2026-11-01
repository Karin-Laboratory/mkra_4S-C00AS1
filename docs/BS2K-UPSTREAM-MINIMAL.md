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

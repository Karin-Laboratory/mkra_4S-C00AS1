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

## What this patch does and does not do

This PR introduces only the isolated legacy ISDB-S tuning, FEM assignment,
message-format and cleanup primitives in `src/tuner.rs`; it deliberately does
**not** wire up the end-to-end capture path. The remaining graph/ring reader,
TS output mode, CAS, MULTI2 descrambling, Mirakurun endpoints, EPGStation,
recording scripts, and integration are **out of scope**.

The tuner-specific API can be reviewed independently; a later PR can add
a narrowly scoped **unencrypted/raw MPEG-TS capture** path using these
primitives. The full working but larger reference implementation is in:
https://github.com/Karin-Laboratory/mkra_4S-C00AS1

### Limitations / operational safety

- Source-only contribution; no receiver was tuned or restarted for this PR.
- Avoid simultaneous tune requests, especially when native viewing or
  recording is in progress.
- The vendor frontend and interface are firmware-specific. Other
  firmware/device variants are unverified.
- A failed graph teardown must not silently be treated as a clean restart.
- 2K-to-4K native HDMI restoration remains unverified; HTTP streaming
  restoration and HDMI recovery are different.

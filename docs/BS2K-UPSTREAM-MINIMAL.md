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
run while preparing the original source contribution. The 15-second successful
reception reported above comes from the full experimental fork and **is not
evidence of successful operation of this extracted branch**. A later bounded
device attempt and its failure are recorded below.

### Limitations / operational safety

- The original source contribution was prepared without tuning or restarting
  a receiver; the later failed attempt is documented below.
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

### Offline failure analysis and corrective patch (2026-11-01)

This analysis uses the saved trial report and source only. No receiver or
management host was contacted for this change.

#### What the failure proves

- **First failing operation in the capture path: UNKNOWN.** In the failed
  revision, `Source::open` propagated errors with bare `?`, without stage
  names. The final `capture failed` line therefore cannot tell whether tuning,
  lock/status polling, graph creation, or a later setup ioctl failed first.
  The absent output file is consistent with failure before `File::create`, but
  does not identify which `Source::open` operation failed.
- **Observed cleanup failure:** the saved log names restoration of logical
  TSI1 and reports OS error 1032. `restore_tsi_mapping` logs its 11-word
  request immediately before `ioctl 0x53`; the request is the saved decoded
  physical-TSI1 fields in words 0–9 with restore selector `1` in word 10.
  The persisted public trial note does not contain those exact word values.
  Based on the logged ordering, the first restore ioctl (`0x53`) is the
  likely failing cleanup operation; this does not prove it was the first
  failure in `Source::open`.
- **1032 meaning: UNKNOWN.** The ioctl wrapper reports an OS error number,
  while `docs/ABI.md` names nearby XCode error values but does not define
  1032. It is consistent with an undocumented vendor-driver status surfaced
  through errno, but no semantic label is established. It must not be called
  “invalid mapping”, “busy”, or any other specific condition.
- **The 2026-10-16 success is HISTORICAL.** Its documented receive chain
  confirms the required physical TSI0 → logical TSI1 → type-12 DEMUX input 1
  → SYSTEM/TS ring → reader sequence. It used the integrated implementation;
  it is not a pass for this extracted CLI.

#### Integrated implementation comparison

The integrated `main` implementation and this raw command share the same
front-end tuning and graph construction order: OEM identity check, TSI
snapshot, FEM assignment, FED tune, lock/output/TSID polling, owned pipeline
and AV_SYNC, physical TSI0 setup, logical TSI1 mapping, type-12 DEMUX input 1,
run, ring/reader, full-TS PID filter, and ready. The integrated path also opens
the native card before the snapshot for its CAS path; CAS is not needed to
obtain raw TS and is not evidence for this failure.

The important safety difference is process ownership. Integrated `serve
--bs2k` takes nonblocking `flock` on `/tmp/mkra-tuner.lock` for its lifetime
and serializes source changes through its broker. The standalone
`bs2k-raw` command did neither. During the recorded trial the integrated
server was left running, while the API reported no active tuner users. The
free API state describes broker sessions; it does not prove that the driver
graph, FEM assignment, or TSI mapping is unowned. This is a **confirmed
missing exclusion guard** and a plausible contributor to the trial failure,
but the logs cannot prove that it caused OS error 1032.

The old `Drop` path also attempted TSI restore for every setup failure after
the snapshot, including failures before any TSI configuration call. It marked
the DEMUX running only after `RUN` succeeded and marked the PID filter only
after its ioctl succeeded, so partial state changes could be missed. Reader
teardown errors were logged separately and did not make the enclosing graph
dirty. Cleanup could also restore TSI after another teardown operation had
already failed. Creation ioctls that fail after allocating an object without
returning its handle remain impossible to identify safely from the current
API; the code does not scan and destroy unknown handles.

#### Corrective patch

- Acquire the same nonblocking frontend flock **before opening the driver**;
  refuse immediately when another integrated mkra process owns it.
- Read the verified FEM path assignment before taking over and refuse if it
  already reports legacy ISDB-S. This is a narrow preflight, not proof that
  native viewing or every OEM graph is idle.
- Add operation-stage context and preserve the raw OS error number in setup
  diagnostics. The restore path identifies ioctl `0x53` versus physical TSI
  ioctl `0x50` and logs the full request words before failure.
- Track possible TSI mutation before physical TSI configuration, and mark
  DEMUX run / PID-filter attempts before their ioctls so cleanup covers
  partial completion.
- Stop the owned DEMUX before ring teardown; do not destroy graph resources
  or restore TSI while the device may still be running or the ring is owned.
  Restore the saved TSI state only after clean teardown and only if this
  source attempted a TSI mutation. Any cleanup error keeps the local dirty
  latch set.
- Give the enclosing source an explicit reader-close result so reader
  teardown failure participates in that dirty latch.

#### Offline verification and remaining limits

- **PASS:** 207 host tests, including shared-lock exclusion/release, TSI
  snapshot restore-request construction/policy, and synthetic OS-error-1032
  stage reporting.
- **PASS:** host release build and ARMv7 musl release build; ELF32 ARM EABI5,
  static, no dynamic section.
- **PASS:** existing 4K-related host regression suite is included in the full
  test run; no integrated 4K implementation or main branch files were changed.
- **LIMIT:** the XCode driver calls still use concrete ioctl FFI, so unit tests
  do not simulate the hardware driver's allocation/partial-mutation behavior.
  No test can establish TS reception, physical resource ownership, or restore
  success without hardware. `cargo fmt --check` still reports the repository's
  pre-existing rustfmt deviations (219 diff sections, including untouched
  lines in legacy files); the new lock module and raw-capture source pass
  targeted rustfmt checks.
- **Next device trial: NO-GO.** Do not repeat until a separate maintenance
  window or isolated unit has no integrated process holding the shared lock,
  no recording/active viewing or due reservation, the verified frontend
  assignment is not already ISDB-S, firmware fingerprints match, and a
  read-only graph/TSI baseline plus recovery owner is recorded. The failed
  attempt's unresolved graph/TSI state must first be handled through an
  independently approved recovery plan; this patch does not clear that state.

### Read-only post-failure hardware audit (2026-11-02 05:46–05:51 JST): NO-GO

This audit used the previously documented management route and guarded
read-only helper. It did not tune, write TSI/MMR, create or destroy graph
objects, upload or execute a binary, stop a process, restart a service, or
reset the receiver. The raw capture is retained privately; this report omits
host addresses, credentials, process IDs, graph handles, and reservation
details.

#### Management host and receiver

- The management host was reachable. Its watcher timer was waiting; the
  watcher service had completed healthy checks without uploading or starting
  anything. The BS2K EPG scanner was failed (last start 05:34 JST); the BS4K
  scanner was inactive and the proxy was active.
- EPGStation returned no active recording and no upcoming reservation. The
  receiver API reported version 0.1.0, zero tuner-device/TS-filter/decoder
  streams, and an available, non-faulted, unused tuner with no users.
- OEM `tuner`, `card`, `abs`, and `adsb` processes were present. The integrated
  mkra server remained running, held the shared frontend flock, and had open
  `xcodedrv` descriptors. The API's free/unused state therefore did not prove
  exclusive hardware ownership. The 2026-11-01 test binary remained staged;
  this audit did not execute it.
- The four OEM fingerprints checked by the capture guard matched their
  expected values. The XCode driver banner reported version 3.0.0.0.73.00.
  No firmware mismatch was observed.

#### TSI state and dated baseline

Before running the tools, their current CLI paths were checked against source.
`tsi-mmr` only reads the fixed MMR words and the documented read-only TSI
status ioctl. The wrapper opens the driver descriptor in read/write mode, but
this invocation uses only the read ioctls and the `0x7a`/`0x7b` per-process
buffer map/unmap; it issues no configuration or graph mutation ioctl.
`graph --sniff` reads the driver graph and buffer pages. The frontend read used
the source-verified tuner ELF address through the read-only process-memory peek
path.

The observed TSI values were:

| Register/status | 2026-11-02 | Saved 2026-10-16 post-capture baseline |
| --- | ---: | ---: |
| MMR `0x6000` | `0x01000000` | `0x01000000` |
| MMR `0x6004` | `0x00000001` | `0x00000001` |
| MMR `0x6008` | `0x01000100` | `0x01000100` |
| MMR `0x600c` | `0x00000200` | `0x00000200` |
| MMR `0x61f0` crossbar | `0x00000010` | `0x00000010` |
| TSI0 | locked, CRC `0x45` | locked, CRC `0x43` |
| TSI1 | unlocked, CRC `0xee` | unlocked, CRC `0xee` |

The stable physical-TSI and crossbar words match the dated successful
post-capture snapshot. The TSI0 CRC is a live counter and is not expected to
remain constant. This is evidence that the observed mapping matched that
known-good state at that collection time. Decoding the physical-port words
with the checked-in helper gives TSI0 `[0,0,0,1,0,1,1,0,0,0]` and TSI1
`[1,0,0,0,0,0,0,0,0,1]`. This does not identify who owns the current graph.

#### Frontend assignment and graph

The read-only tuner memory peek returned `2` for `aDevOfPath[1]`, which the
current source interprets as the legacy ISDB-S assignment. A saved
2026-10-16 post-capture read from the same unit and verified tuner image
returned `0x200` for that word (native path). The difference is real and is
consistent with the failed attempt leaving the frontend assigned to ISDB-S.
It does not prove that causal link: the original attempt lacks a first-failure
stage log, and no pre-attempt value for this word was recorded. The current
source's raw-capture preflight would refuse to start when this assignment is
already legacy ISDB-S.

The 05:49 JST driver graph reported 11 devices, 12 buffers, 2 pipelines, and 15
ports. The table exposed 13 candidate buffer addresses, of which 8 passed the
buffer-object magic/read check; these readings do not prove ownership. The
historical known-good active-4K graph was 22/32/2/73, while a documented
standby summary reported 11 devices and 9 buffers. These are different native
states and do not provide an exact same-state graph baseline. Two pipelines
alone are not evidence of a leftover: that count also appears in historical
graphs. **Graph ownership and whether any object is residual remain UNKNOWN.**
No suspected object was cleared.

#### Error 1032, recovery plan, and decision

No new evidence in the 05:49 JST collection explained OS error 1032. The
saved failure associates it with the logical-TSI1 restore ioctl `0x53`, but
does not establish the first failing
operation in setup. The code and available ABI notes still do not define the
meaning of 1032. The 05:49 JST kernel log contained no new matching error.

No recovery was attempted. Before another capture, use a separately approved
maintenance window: establish that recordings, viewing, reservations, and
other owners are absent; review a frontend restoration path against the
current firmware; then collect a new read-only graph/TSI snapshot. Do not
destroy graph objects based on counts or handles. If ownership still cannot
be attributed against a comparable baseline, keep the receiver out of
capture testing and have the device owner decide the recovery path. Any
restoration write, service stop, or reset requires its own authorization and
rollback plan.

**05:49 JST decision: NO-GO.** At that observation the integrated server
owned the shared frontend lock, the frontend assignment differed from the
saved native-path observation, and a comparable graph baseline was
unavailable. No new BS2K attempt was authorized by that audit.

### Read-only recheck after receiver became reachable (2026-11-02 08:55 JST)

After the earlier Telnet connection failed with `No route to host`, the owner
reported the receiver had been in standby (red LED) and asked for another
check. CT141 SSH succeeded and the existing guarded Telnet helper connected to
the receiver. The helper source was reviewed; every receiver command below
was submitted through its guarded `run()` path. No receiver-side write,
retune, graph creation/destruction, process control, upload, or reset occurred.
Private raw captures are retained outside GitHub with restrictive file modes.

#### Management and owner state

- At 08:55 JST, EPGStation reported zero active recordings and three
  reservations, all expired. The receiver API was reachable; version was
  0.1.0, tuner-device/TS-filter/decoder stream counts were all zero, and its
  sole tuner reported available, not faulted, not using, and no users.
- OEM `tuner`, `card`, `abs`, and `adsb` processes were present. Their four
  MD5 fingerprints exactly matched the fixed identity guard in this branch.
  The integrated binary's SHA-256 matched the installed CT watcher manifest.
- The integrated mkra process held the shared `flock` and had two open
  `xcodedrv` descriptors. Thus the API's idle tuner state still does not mean
  exclusive frontend ownership. The PR #2 trial binary was absent from the
  receiver's volatile `/tmp/mt` listing; it was not executed.
- CT's watcher timer was active/waiting. Its service was inactive after
  successful `HEALTHY; no upload/start` checks. The BS2K EPG scanner remained
  failed, BS4K scanner inactive, and proxy/EPGStation active.
- The receiver reported about 2,600 seconds uptime, versus about 240,000
  seconds in the earlier 05:49 JST capture. This indicates an uptime/boot
  discontinuity since that capture; its cause is unknown. The receiver's own
  wall clock read 2017 and was not used as a timestamp. The read-only probe
  found no A/V pipeline threads and warned that the unit appeared to be in
  standby, consistent with the owner's red-LED observation.

#### TSI, frontend, and graph recheck

The source-reviewed `tsi-mmr`, `graph --sniff`, and fixed-address process
memory peek were run only after the OEM fingerprints and integrated-binary
hash matched. The TSI words were:

| Register/status | 08:55 JST | Saved 2026-10-16 post-capture baseline |
| --- | ---: | ---: |
| MMR `0x6000` | `0x01000000` | `0x01000000` |
| MMR `0x6004` | `0x00000001` | `0x00000001` |
| MMR `0x6008` | `0x01000100` | `0x01000100` |
| MMR `0x600c` | `0x00000200` | `0x00000200` |
| MMR `0x61f0` crossbar | `0x00000010` | `0x00000010` |
| TSI0 | locked, CRC `0x31` | locked, CRC `0x43` |
| TSI1 | unlocked, CRC `0x57` | unlocked, CRC `0xee` |

The stable TSI/crossbar words match the saved baseline; CRC values are live
counters. The tuner-memory peek returned `0x200` for `aDevOfPath[1]`, matching
the saved native-path value. This differs from the 05:49 JST observation of
`2`; the earlier legacy assignment is not present in this recheck. No causal
claim is made about why that value changed.

The graph reported 11 devices, 11 buffers, 2 pipelines, and 20 ports; 6 of
the 11 listed buffer handles yielded valid readable objects. The earlier
05:49 JST graph was 11/12/2/15. The documented historical standby summary
reported 11 devices and 9 buffers, while the active-4K graph was 22/32/2/73.
These are not an exact same-state graph/ownership baseline. Two pipelines do
not prove a leftover, and the current graph's ownership remains **UNKNOWN**.
No graph object was cleared.

#### Updated decision

No new evidence explains OS error 1032. It remains associated with logical
TSI1 restore ioctl `0x53`; its meaning and the first failed setup operation
remain **UNKNOWN**. No recovery was attempted. The stable TSI words and
frontend assignment now match the saved native-path baseline, so these
observations do not support a TSI restoration write. Unattributed graph
objects must still be preserved.

**Decision remains NO-GO for another BS2K capture.** The integrated server
still owns the frontend lock, the unit appeared to be in standby, and a
comparable graph ownership baseline is unavailable. No new capture is
authorized by this recheck.

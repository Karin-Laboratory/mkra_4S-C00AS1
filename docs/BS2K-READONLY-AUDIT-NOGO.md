# PR #2: read-only post-failure hardware audit (NO-GO)

Status: **NOT EXECUTED**. This is the next evidence-gathering procedure after
the 2026-11-01 failed 5-second BS2K raw capture. It authorizes no tuning,
cleanup, reset or writes. Keep PR #2 as Draft.

## Aim

Distinguish the known baseline/normal product graph from any residual graph
created by the unsuccessful standalone experiment. Confirm whether the logical
TSI mapping is at a baseline *without changing it*. An API tuner status of
`free` is not proof of exclusive device ownership.

The observed `errno 1032` after logical TSI1 restore is **not decoded** by
the available ABI references. Do not ascribe a cause to it. Record the exact
first error and context only from existing logs.

## Route and preflight

Prior session reported the management route:

`agent-101-vm -> CT141 (SSH) -> existing guarded receiver-management helper`.

Rediscover the actual host, helper and permissions from internal operational
records. This public document deliberately contains no IP, credentials, helper
invocation or recording identifiers. Do not establish a new management route
or use unapproved direct receiver access.

Read `docs/TROUBLESHOOTING.ja.md`, `docs/VALIDATION-MATRIX.ja.md`,
`docs/BS2K-RECEPTION.ja.md`, PR #2's failure and corrective-patch sections
before executing anything. First classify every prospective command as
read-only, may-retune, may-allocate graph, may-clear/write or unknown.
Execute only demonstrably read-only queries. **Do not assume a command named
'status', 'probe', '--dry-run' or 'snapshot' is safe without checking its code.**

Record remotely on the management host (not receiver RAM):

1. UTC/JST timestamp, boot identity/uptime (avoid publishing unique IDs).
2. Native processes `tuner`, `abs`, `adsb`, `card` and test mkra PID/exe.
3. Existing integrated mkra PID/command, shared frontend `flock` ownership,
   watcher, EPG scanner state and errors.
4. Existing HTTP **status/tuners only**, without requesting stream endpoints.
5. EPGStation ongoing recordings and next reservation window, without any
   database mutation.
6. Receiver `/proc/meminfo`, mount state and relevant recent kernel errors.
7. Read-only driver graph and TSI MMR values, including 0x61f0 crossbar
   and physical port configuration, using **previously documented read-only
   capture tools** only. No unknown ioctl or handle enumeration by brute force.

## Baseline comparison

Look up an earlier, specifically dated, successful and known-idle graph + TSI
snapshot from the **same unit, firmware and comparable native service state**.
Save both descriptions with provenance; normalize volatile handles and PIDs
but never collapse two different graphs on the basis of equal counts alone.

Compare:

- logical TSI1 mapping and readback (including 0x61f0)
- physical input 0/1 configuration
- owned pipeline/device/buffer/reader inventory *if ownership can be proven*
- OEM frontend assignment and active integrated mkra flock
- signs of leftover reader/writer references or graph cleanup failures

If there is no comparable baseline, report **UNKNOWN**, not “two pipelines
are leftover” and not “graph clean”. Preserve captures privately, publishing
only anonymized findings.

## Decision gate

- **NO-GO** if mapping differs from a proven baseline, graphs remain
  unattributed, any owner is active/unknown, firmware differs, recorder is
  active/soon reserved, or a read-only query is not unquestionably safe.
- **UNKNOWN** when data are missing, especially when pipeline count has no
  pre-test baseline.
- An observation-only **PASS** means evidence was collected with no apparent
  mutation; it does **not** itself authorize a new BS2K capture.
- A subsequent GO requires separately reviewed ownership/exclusivity plan,
  rollback and recovery plan, verified idle maintenance window or isolated
  hardware and explicit operator approval.

## Forbidden operations

No tune/stream endpoint, `bs2k-raw`, driver-graph creation/destruction,
TSI writing/restoration, read-modify-write ioctl, receiver upload/deployment,
mkra kill/restart, watcher modifications, firmware/flash writes, or reset.
Do not remove a suspected leftover graph just because its handle is visible.
No CAS keys/card response or personal recordings in GitHub.

## Deliverables

Private evidence log plus a concise sanitized report in PR #2:

- observed management path (no secrets), firmware version/guard, timestamps
- read-only probes run and exact original outputs stored privately
- known historical baseline provenance or explicit lack thereof
- graph/TSI/process/lock/recording differences with confidence levels
- first-error hypothesis: CONFIRMED / LIKELY / UNKNOWN
- safe recovery proposal for a *separate* approved maintenance window
- final **NO-GO / UNKNOWN**, until an independently supported GO is justified

Do not modify hardware while completing this audit.

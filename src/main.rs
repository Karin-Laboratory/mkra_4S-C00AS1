//! mkra — Mirakurun-compatible relay agent for the SHARP 4S-C00AS1.
//!
//! Subcommands:
//!   serve    run the HTTP server
//!   probe    read-only investigation of the driver (answers DESIGN.md §10)
//!   dump     write raw TS from a buffer to a file, for offline inspection
//!   demux    build the product MPEG-TS DEMUX and capture its full TS output
//!   gr       tune the recovered ISDB-T path or print its byte-exact plan
//!   gr-off   put the ISDB-T RF section into vendor-defined standby
//!   isdbs-status  read the SC1501A ISDB-S lock byte without changing RF state
//!   isdbs-ts-exclusive  clear only a measured stale ISDB-S3 TLV enable bit
//!   dmd-regs read the SC1501A serial-output framing registers, read-only
//!   dmd-tsout-base / dmd-tsout-tlv  switch bank7 73/75/7b/7d between the two
//!            product-defined states (guarded, compare-before-write, read back)
//!   tsi-mmr  read the live XCode6 TSI MMR/status without changing its state
//!
//! Read docs/ABI.md before
//! changing behaviour on the device.

mod api;
mod bs2k;
mod cas;
mod descramble;
#[path = "../tools/multi2.rs"]
mod multi2;
mod native_selection;
mod channels;
mod cpld;
mod findbuf;
mod graph;
mod gr;
mod gr_i2c;
mod loopback;
mod http;
mod i2c_lock;
mod json;
mod remux4k;
mod si;
mod snapshot;
mod stream;
mod sys;
mod testts;
mod tssrc;
mod tuner;
mod xcdrv;

// BTreeMap rather than HashMap on purpose: `RandomState` pulls in the
// platform entropy source, and this binary targets a 2012-era 3.10 kernel
// that predates `getrandom(2)`. Ten channels make ordering free anyway.
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use channels::ChannelTable;
use stream::{Broker, BrokerConfig};
use tssrc::{FileSource, NullSource, PassiveRingSource, TsSource};
use xcdrv::XcDrv;

fn is_xce_no_support(error: &io::Error) -> bool {
    error.raw_os_error() == Some(1003)
}

const USAGE: &str = "\
mkra — Mirakurun-compatible relay for the SHARP 4S-C00AS1

USAGE:
  mkra serve [options]
  mkra probe [options]
  mkra dump  --buffer <handle> --out <path> [--seconds <n>] [--reader <handle>|--passive]
  mkra graph [--watch <n>] [--sniff]
  mkra peek  --phys <addr> [--len <n>]
  mkra tune  --freq <kHz> [--tsid <hex>] [--tuner-no <1..3>] [--input-port <n>]
  mkra tune  --fed <adbs|isdb-s> --freq <kHz> [--path <n>] [--tsid <hex>] [--dry-run]
  mkra fed-off --path <1..4> --dev <0x2|0x4|0x8> [--dry-run]
  mkra fem-dev --path <1..6> (--dev <hex> | --disconnect) [--dry-run]
  mkra cpld --reg <0..4> [--set <hex> --expect <hex>]
  mkra tsrec --out <path> [--seconds <n>] [--tsi-port <n>]
  mkra demux --out <path> [--seconds <n>] [--tsi-port <n>] [--tsi-packet <188|192|204|130>] [--tsi-use-sync]
             [--pid <0..8191>|--pat-section] [--dry-run]
  mkra remux4k --out <path> [--seconds <n>] [--tsid <hex>] [--dry-run]
  mkra tune4k  --sid <101|102|141|151|161|171|181|211|221|231> [--oid <n>] [--dry-run]
  mkra gr (--channel <13..62>|--freq <kHz>) [--if-hz <Hz>] [--dry-run]
  mkra gr-off
  mkra isdbs-status
  mkra isdbs-ts-exclusive
  mkra demux-crossbar    bounded logical1 -> physical0 BS-1 diagnostic
  mkra demux-loopback | local-demux-loopback  synthetic diagnostic; no RF/TSI writes
  mkra dmd-regs
  mkra dmd-tmcc           read received ISDB-S monitors; restore selectors
  mkra dmd-relative0 | dmd-tsid-restore  guarded BS-1 selection comparison
  mkra dmd-tsout-base | dmd-tsout-tlv
  mkra dmd-iface-serial | dmd-iface-isdbs | dmd-quiesce
  mkra dmd-mode16-serial | dmd-mode16-isdbs
  mkra sc1501a-snapshot --label <S1..S5>
  mkra dmd-cc-select40
  mkra tuner-oneshot-clear --label <pseq|recommend>
  mkra tsi-mmr
  mkra temp [--watch <n>]

SERVE OPTIONS:
  --port <n>            listen port                    (default 40772)
  --bind <addr>         bind address                   (default 0.0.0.0)
  --isdbs-channels <path> optional BS/CS TSV (requires --bs2k)
  --bs2k               integrate live BS-1/TSID4010 + CAS (requires --remux4k --tune)
  --remux4k             stream live BS4K by building our own REMUX on the
                        product's MMT ES rings (see REMUX4K OPTIONS)
  --buffer <handle>     driver buffer handle to read   (decimal or 0x hex)
  --file <path>         read TS from a file/FIFO instead of the driver
  --file-bitrate <bps>  pace --file at this rate       (default 0 = as fast as possible)
  --file-repeat         loop --file at end of data
  --test-pattern        serve a synthetic PAT/PMT/SDT stream instead of a
                        tuner, so a real client can be tested end to end
  --test-bitrate <bps>  test pattern rate            (default 8000000)
  --channels <path>     channel table TSV              (default: built in)
  --epg-dir <path>      read MMT-SI from the product's cache, e.g. /tmp/adsb.
                        Fills /api/programs. Independent of the TS backend
  --epg-tap             build the EPG from the driver's live section rings
                        (read-only: /dev/kmem + xc_map, no buffer ioctl). Works
                        even in standby, unlike --epg-dir
  --epg-refresh <sec>   re-read/re-scan every n seconds (default 300)
  --channel-type <t>    Mirakurun ChannelType to report (default BS).
                        Use BS4K for Mirakurun-BS4K / DMirakurun clients.
  --ring <bytes>        per-client buffer              (default 2097152,
                        about 0.55 s of 4K). Bigger survives network hiccups,
                        smaller cuts live latency; drops are packet-aligned
  --max-clients <n>     concurrent stream clients      (default 4)
  --tune                let mkra change the box's channel when a client asks
                        for a service it is not showing (default: off)
  --idle-sleep <sec>    with --remux4k --tune: after this many seconds with no
                        client, park the tuner on a transponder that carries no
                        signal, so the 4K decode block idles while the box stays
                        powered on. The next stream request tunes back (~4 s).
                        0 = off, the default.
  --park-freq <kHz>     transponder to park on (default 12431000, a CS-4K slot
  --park-tsid <hex>     not receivable at this site; default TSID c090). Pick
                        one your dish does NOT receive.
  --tuner-no <1..3>     which frontend to tune         (default 1)
  --input-port <n>      inputPortNo, must be >= 100    (default 101)
  --reply-qid <n>       product-internal reply queue   (default 0; unused, see DESIGN.md §6.2)
  --tune-settle-ms <n>  wait after tuning before read  (default 1500)

PROBE OPTIONS:
  --pid <n>             recover handles from a running process's heap (SAFE,
                        read-only; repeatable, e.g. --pid 264 --pid 279)
  --scan                brute-force a handle range. ★ OOPSES THE KERNEL on a
                        wrong handle; refused without --i-accept-oops-risk
  --scan-max <n>        highest handle to try          (default 64)
  --graph               call xc_get_graph and dump the result
  --peek <pid:addr:len> hexdump live process memory (SAFE, read-only)
  --find-buffers        find real buffer handles by reading /dev/mem for the
                        magic the driver itself validates (SAFE, read-only).
                        Combine with --pid to keep only referenced ones.
  --buffer <handle>     probe just this handle, reading a little data
  --seconds <n>         how long to read when probing a buffer (default 3)
  --passive             follow the ring's write pointer instead of attaching a
                        reader port. Works on buffers that refuse a host
                        connection, and cannot perturb the product at all.
  --raw-stream          serve bytes verbatim instead of waiting for a 188-byte
                        TS boundary. Required when tapping a section ring,
                        whose MMT/TLV payload never contains one.
  --reader <handle>     dump: adopt and drain an already-created reader, then
                        destroy it. Required when the reader preceded TSI run.

GRAPH OPTIONS (all read-only: /dev/kmem + xc_map, no buffer ioctl):
  --watch <n>           re-read the graph n times, 1 s apart, and report which
                        object words moved -- that is how the write pointer is
                        identified without guessing
  --sniff               also classify each ring's payload (MPEG-TS / TLV)
  --raw                 dump the first 0x100 bytes of every buffer object

TSI-MMR (read-only):
  Reads the fixed XCode6 TSI enable/status/config/CRC/crossbar MMR words and
  issues status ioctl 0x51 for physical TSI0 and TSI1. It performs no config,
  graph, buffer, tune, I2C, SPI, or firmware write.

ISDBS-TS-EXCLUSIVE (one fixed I2C write):
  Requires the measured locked ISDB-S signature reg0b=0x01/regcc=0xc0, then
  holds the vendor I2C0 semaphore and clears only reg0xcc bit6. The resulting
  exclusive MPEG-TS output value must read back as 0x80; every other starting
  state is refused.

TUNE4K OPTIONS (ask the product to change channel, as its own UI does):
  --sid <n>             advanced-BS service id to select
  --oid <n>             operation id the product echoes back (default 32)
  --freq <kHz> --tsid <hex>
                        tune at these instead of the channel table's values
  --tune-mode <n>       the Tune word: 0 = channel change (default),
                        3 = the value the product uses when it resumes
  --no-decode           set the NotDecode word (unverified; see the spec)
  --dry-run             print the message that would be sent, send nothing

  Sends msgId 0x6004 to the ADSB communication queue 0x36F with the ten-word
  body the product's own remote-control path builds. The product then tears
  down its TS units, re-tunes, and rebuilds the MMT filters and ES rings for
  the new service — which a frontend-only command cannot do.
  Release any tap on the live ES rings first: the product's buffer teardown
  fails with XCE_IN_USE while an external consumer is attached.

REMUX4K OPTIONS (BS4K MMT -> MPEG-TS, no recording required):
  --out <path>          write the 188-byte TS here ('-' for stdout)
  --seconds <n>         capture length (default 3)
  --tsid <hex>          transport_stream_id for the generated PAT (default 4000)
  --discover-only       read-only: report the live ES rings and pipeline, then
                        stop before creating anything
  --allow-no-decoder    diagnostic CLI: permit absent native decoder readers
                        only when the same video/audio ES rings advance
  --sid <n>             explicit 4K service identity when the SI cache is stale;
                        sets PAT/PMT identity without changing channel
  --raw-stream          write the REMUX output verbatim instead of unwrapping
                        it, for establishing the container framing
  --inject-psi          add a locally generated PAT/PMT (off by default: the
                        REMUX emits its own)
  --with-tsinfo         also create and connect the REMUX's TSINFO side channel.
                        The product does this, but measured on this box it makes
                        the REMUX stop taking input after a few seconds; leaving
                        it unconnected runs indefinitely. Diagnostic only.
  --kick                diagnostic: re-run the REMUX after an idle second
  --stall-abort-ms <n>  release the live ES rings if no output arrives for n ms
                        (default 5000; 0 disables)
  --es-level-abort <n>  release them as soon as the live video ES ring backs up
                        to this level (default 48; steady state is 9..14). This
                        is the guard that matters: a stalled tap starves the
                        product's decoder and blocks its own recovery.
  --dry-run             print every ioctl request and touch nothing

  Builds a private REMUX (me_type 0x0E) exactly as adsb's recording path does,
  connects it to the product's LIVE video/audio ES rings as an additional
  consumer, and reads the resulting plaintext TTS out of our own TSOUT ring.
  It creates no reader on a product buffer and destroys nothing the product
  owns. The box must be watching 4K (green LED).

PEEK OPTIONS:
  --phys <addr>         XCode6 physical address to map and dump
  --len <n>             bytes (default 256)

TUNE OPTIONS:
  --freq <kHz>          transponder RF frequency, e.g. 12034360
  --tsid <hex>          TSID (required for --fed isdb-s; default ffff elsewhere)
  --fed <adbs|isdb-s>   post one msgFedSetTuningStart straight at a frontend,
                        bypassing the FEM. Without this the four-message FEM
                        sequence is used (advanced BS only).
                          adbs    = the 4K frontend, Q_FED_ADBS1
                          isdb-s  = BS/CS110 HD, needs --path
                          isdb-s-fem = experimental BS/CS110 via FEM msg0x200d;
                                       assign the ISDB-S FEM device first
                        Each frontend encodes the frequency differently; mkra
                        converts, so always pass the RF value.
  --path <1..4>         which non-4K frontend (Q_FED1..4). Travels in the wire
                        header's src field, not the payload.
  --dry-run             print the message that would be sent, send nothing

FEM-DEV OPTIONS (assign a frontend device to a path, reversibly):
  --path <1..6>         which path
  --dev <hex>           0x2/0x4/0x8 = ISDB-S (BS/CS110 HD),
                        0x200/0x400/0x800 = advanced BS. On this box 0x2 and
                        0x200 are the SAME physical demodulator, so taking one
                        takes it away from the other.
  --disconnect          hand the path's device back (aDevOfPath[path] = 1)
  --dry-run             print the message, send nothing
  ★ Read aDevOfPath first; `--dev` refuses values outside the FEM's vocabulary.

CPLD OPTIONS:
  --reg <0..4>         read one documented tuner-board CPLD register
  --set <hex>          set register 2 (X6TSISL1); only known-safe route values
                        0x22, 0x20, 0x02, 0x21, and 0x12 are accepted
  --expect <hex>       mandatory compare-before-write value for --set
  The live selector is unknown; read-only observation is the default. Do not
  write until the TS1/TS2/TS3 source is statically identified.

TSREC OPTIONS:
  --out <path>          capture file
  --seconds <n>         capture duration (default 3)
  --tsi-port <n>        XCode6 TSI port (default 0). Uses the direct logical
                        TSI request form left in libvixs's unused ts_input_t class.
  Builds and drains that SDK TS_RECODER candidate on one driver fd.

DEMUX OPTIONS:
  --out <path>          capture file
  --seconds <n>         capture duration (default 3)
  --tsi-port <n>        physical XCode6 TSI port (default 1 for demux/BS;
                        pass 0 explicitly for GR; BS system=2 maps to TSI1)
  --tsi-parallel        use parallel input (default serial)
  --tsi-rising-edge     sample on rising clock edge (default falling)
  --tsi-valid-low       active-low VALID (default active-high)
  --pid <0..8191>       diagnostic single-PID output; default is PID=-1 full TS
  --no-filter           Gate 5A: create/run DEMUX and SYSTEM/TS ring without
                        adding a PID filter; mutually exclusive with --pid
  --pat-section         Gate 5B: product DATA/PSI_SECTION filter for PID 0;
                        mutually exclusive with --pid and --no-filter
  --wait-tsi-after-run  Gate 5B5 diagnostic: after DEMUX run, wait for TSI
                        relock plus CRC progress before creating the output
  --inspect-pid-filter Gate 5B7 diagnostic: after filter add, issue the
                        read-only class-5 PID-filter info query and log the
                        returned filter type and output connection
  --observe-resources  Gate 5B8 diagnostic: run the fixed read-only vendor
                        resource snapshot after run, reader creation, PID add,
                        ready, and cleanup
  --observe-sf-ring    Gate 5B9 diagnostic: read the firmware-reported internal
                        section-filter ring before and after capture. Requires
                        --pat-section and --tsi-port 0; adds no device writes
  --observe-tsi-route  Gate 5B10 diagnostic: volatile-read the open-time MMR
                        TSI registers and firmware DEMUX route fields
  --ready-before-filter
                        Gate 5B7 hypothesis: make the output ring ready before
                        adding the PAT filter as the final write operation
  --dry-run             print the byte-exact product DEMUX/output-all lifecycle;
                        do not open the driver or create the output file
  Builds the product DEMUX path and requests SYSTEM/TS with PID=-1.

GR OPTIONS:
  --channel <13..62>    Japanese UHF physical channel (product table formula)
  --freq <kHz>          explicit RF centre frequency instead of --channel
  --if-hz <Hz>          SC1501A IF centre (default 3550000, NCO 0x245a1d)
  --dry-run             print every byte-exact I2C transaction and the common
                        DEMUX handoff; never open /dev/i2c-0
  Without --dry-run, attach the existing product I2C0 semaphore (never create
  or sem_init), issue the one fixed sequence, and report RF latch/ISDB-T lock.
  Live GR changes RF state; reboot before the next live hypothesis.

COMMON:
  --dev-node <path>     driver node (default /dev/vixs/xcodedrv)
  -h, --help            this text
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return;
    }

    let opts = match Options::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mkra: {e}\n\nTry `mkra --help`.");
            std::process::exit(2);
        }
    };

    let code = match opts.command.as_str() {
        "serve" => cmd_serve(&opts),
        "probe" => cmd_probe(&opts),
        "dump" => cmd_dump(&opts),
        "graph" => cmd_graph(&opts),
        "peek" => cmd_peek(&opts),
        "tune" => cmd_tune(&opts),
        "fed-off" => cmd_fed_off(&opts),
        "fem-dev" => cmd_fem_dev(&opts),
        "cpld" => cmd_cpld(&opts),
        "tsrec" => cmd_tsrec(&opts),
        "demux" | "demux-crossbar" => cmd_demux(&opts),
        "demux-loopback" => loopback::run(loopback::Case::Tsi),
        "local-demux-loopback" => loopback::run(loopback::Case::LocalSmall),
        "local-demux-bulk-loopback" => loopback::run(loopback::Case::LocalBulk),
        "local-demux-file-loopback" => loopback::run(loopback::Case::LocalFile),
        "local-demux-tts-loopback" => loopback::run(loopback::Case::LocalTts),
        "local-demux-pat-loopback" => loopback::run(loopback::Case::LocalPat),
        "local-demux-started-loopback" => loopback::run(loopback::Case::LocalStarted),
        "local-demux-tts-started-loopback" => loopback::run(loopback::Case::LocalTtsStarted),
        "local-demux-described-loopback" => loopback::run(loopback::Case::LocalDescribed),
        "local-demux-tts-described-loopback" => loopback::run(loopback::Case::LocalTtsDescribed),
        "local-demux-pat-described-loopback" => loopback::run(loopback::Case::LocalPatDescribed),
        "remux4k" => cmd_remux4k(&opts),
        "tune4k" => cmd_tune4k(&opts),
        "gr" => cmd_gr(&opts),
        "gr-off" => cmd_gr_off(),
        "isdbs-status" => cmd_isdbs_status(),
        "dmd-regs" => cmd_dmd_regs(),
        "dmd-tmcc" => cmd_dmd_tmcc(),
        "dmd-relative0" | "dmd-tsid-restore" => {
            match gr_i2c::select_isdbs_relative_zero(opts.command == "dmd-tsid-restore") {
                Ok(r) => { println!("ISDB-S reg31 {:02x} -> {:02x}, verified; TSID 4010 unchanged", r.before, r.after); 0 }
                Err(e) => { eprintln!("relative-TS0 selection refused/failed: {e}"); 1 }
            }
        },
        "dmd-tsout-base" => cmd_dmd_tsout(gr_i2c::TsoutTarget::Base),
        "dmd-tsout-tlv" => cmd_dmd_tsout(gr_i2c::TsoutTarget::TlvSpi1Bit),
        "dmd-iface-serial" => cmd_dmd_iface(gr_i2c::TsoutInterface::Serial1Bit),
        "dmd-iface-isdbs" => cmd_dmd_iface(gr_i2c::TsoutInterface::IsdbsDefault),
        "dmd-mode16-serial" => cmd_dmd_iface(gr_i2c::TsoutInterface::Mode16Serial),
        "dmd-mode16-isdbs" => cmd_dmd_iface(gr_i2c::TsoutInterface::Mode16Isdbs),
        "dmd-mode16-tsserial" => cmd_dmd_iface(gr_i2c::TsoutInterface::TsSerial20),
        "dmd-mode16-tsdefault" => cmd_dmd_iface(gr_i2c::TsoutInterface::TsSerial20Restore),
        "dmd-ts-parallel" | "dmd-ts-serial-restore" => {
            match gr_i2c::experiment_ts_parallel(opts.command == "dmd-ts-serial-restore") {
                Ok(()) => { println!("TS reg0b parallel/serial profile applied and verified"); 0 }
                Err(e) => { eprintln!("mkra: TS parallel experiment refused/failed: {e}"); 1 }
            }
        }
        "dmd-spi-forward" | "dmd-spi-restore" => {
            match gr_i2c::experiment_spi_forward(opts.command == "dmd-spi-restore") {
                Ok(()) => { println!("Guarded SPI forwarding hypothesis applied/read back"); 0 },
                Err(e) => { eprintln!("SPI forwarding hypothesis refused/failed: {e}"); 1 },
            }
        },
        "dmd-quiesce" => cmd_dmd_quiesce(),
        "sc1501a-snapshot" => cmd_sc1501a_snapshot(&opts),
        "dmd-cc-select40" => cmd_dmd_cc_select40(),
        "tuner-oneshot-clear" => cmd_tuner_oneshot_clear(&opts),
        "isdbs-ts-exclusive" => cmd_isdbs_ts_exclusive(),
        "tsi-mmr" => cmd_tsi_mmr(&opts),
        "temp" => cmd_temp(&opts),
        other => {
            eprintln!("mkra: unknown command `{other}`\n\nTry `mkra --help`.");
            2
        }
    };
    std::process::exit(code);
}

/// Read the SC1501A's documented ISDB-S lock byte while holding the vendor
/// I2C semaphore.  This deliberately performs no tune, write, or mode change;
/// it separates RF/demod lock from the downstream XCode6 TSI lock report.
fn cmd_isdbs_status() -> i32 {
    match gr_i2c::read_isdbs_status() {
        Ok(status) => {
            println!(
                "SC1501A page 0x12 reg 0x5e = 0x{:02x}; ISDB-S lock bit5: {}\n\
                 TS output readback: page 0x10 reg 0x0b = 0x{:02x}, reg 0xcc = 0x{:02x}",
                status.lock,
                if status.lock & 0x20 != 0 { "LOCK" } else { "UNLOCK" },
                status.ts_pin,
                status.ts_serial,
            );
            0
        }
        Err(e) => {
            eprintln!("mkra: ISDB-S lock read failed: {e}");
            1
        }
    }
}

/// Read-only snapshot of the SC1501A registers that decide serial output
/// framing.  Nothing here writes; the command exists to test one static claim:
/// after a 4K (ISDB-S3/TLV) session, bank 7 `0x73/0x75/0x7b/0x7d` still hold
/// `regset_TLV_SPI_1BIT` values because no ISDB-S or ISDB-T path rewrites them.
fn cmd_dmd_regs() -> i32 {
    let regs = match gr_i2c::read_output_regs() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mkra: SC1501A output-register read failed: {e}");
            return 1;
        }
    };

    println!(
        "SC1501A page 0x12 reg 0x5e = {:#04x}  ISDB-S lock bit5: {}",
        regs.lock,
        if regs.lock & 0x20 != 0 { "LOCK" } else { "UNLOCK" }
    );
    print!("bank0 (i2c 0x10):");
    for (reg, value) in regs.bank0 {
        print!(" {reg:02x}={value:02x}");
    }
    println!();
    let [tsid_hi, tsid_lo, select] = regs.bank2_selection;
    println!("bank2 ISDB-S selection controls: 2f={tsid_hi:02x} 30={tsid_lo:02x} 31={select:02x}");
    if select & 0x08 == 0 {
        println!("ISDB-S requested TSID readback: {:#06x} (TSID selection mode; not received SI)",
                 u16::from_be_bytes([tsid_hi, tsid_lo]));
    } else {
        println!("ISDB-S relative TS selection mode: {} (not received SI)", select & 7);
    }
    print!("bank7 (i2c 0x17):");
    for (reg, value) in regs.bank7 {
        print!(" {reg:02x}={value:02x}");
    }
    println!();

    let framing = gr_i2c::classify_tsout(&regs.bank7);
    println!(
        "tsout framing 73/75/7b/7d: {}",
        match framing {
            gr_i2c::TsoutFraming::TlvSpi1Bit =>
                "TLV_SPI_1BIT (correct in ISDB-S3; stale residue in ISDB-S)",
            gr_i2c::TsoutFraming::Base => "BASE (vendor full-base-set values)",
            gr_i2c::TsoutFraming::Other => "OTHER — do not write, re-audit",
        }
    );
    println!("No register was written.");
    0
}

fn cmd_dmd_tmcc() -> i32 {
    let sample = match gr_i2c::read_isdbs_tmcc() {
        Ok(sample) => sample,
        Err(e) => { eprintln!("mkra: TMCC monitor failed: {e}"); return 1; }
    };
    println!("tmcc.lock.before={:02x}\ntmcc.lock.after={:02x}", sample.locks[0], sample.locks[1]);
    println!("tmcc.selectors.restored=62:{:02x},65:{:02x}", sample.selectors[0], sample.selectors[1]);
    for (i, value) in sample.indexed_tsids.iter().enumerate() {
        println!("tmcc.indexed_tsid[{i}]={value:04x}");
    }
    for (i, value) in gr_i2c::tmcc_tsid_fields(&sample.raw).iter().enumerate() {
        println!("tmcc.info[{}]={value:04x}", 59 + i);
    }
    print!("tmcc.raw=");
    for byte in sample.raw { print!("{byte:02x}"); }
    println!("\nMonitor selectors were restored; no selection/output registers were changed.");
    0
}

/// Install one of the two product-defined SC1501A output-framing states on
/// bank 7 `0x73/0x75/0x7b/0x7d`.
///
/// `dmd-tsout-base` undoes the `regset_TLV_SPI_1BIT` residue that Gate 5C1
/// measured surviving into ISDB-S; `dmd-tsout-tlv` puts it back. Both refuse
/// unless the locked ISDB-S signature and the exact expected `before` bytes are
/// present, and both read every byte back.
fn cmd_dmd_tsout(target: gr_i2c::TsoutTarget) -> i32 {
    let name = match target {
        gr_i2c::TsoutTarget::Base => "vendor base",
        gr_i2c::TsoutTarget::TlvSpi1Bit => "TLV_SPI_1BIT (rollback)",
    };
    match gr_i2c::set_tsout_framing(target) {
        Ok(result) => {
            let pairs: Vec<String> = result
                .before
                .iter()
                .zip(result.after)
                .map(|((reg, b), (_, a))| format!("{reg:02x}: {b:02x} -> {a:02x}"))
                .collect();
            println!("SC1501A bank7 tsout framing set to {name}: {}", pairs.join(", "));
            0
        }
        Err(e) => {
            eprintln!("mkra: tsout framing change refused/failed: {e}");
            1
        }
    }
}

/// Switch the SC1501A bank-0 output interface (`0x13`, `0x16`, `0xcc`) between
/// the ISDB-S values and the 1-bit serial configuration the working 4K path
/// uses. Guarded on the locked ISDB-S signature and on bank 7 still holding the
/// serial output stage; every byte is read back.
fn cmd_dmd_iface(target: gr_i2c::TsoutInterface) -> i32 {
    let name = match target {
        gr_i2c::TsoutInterface::Serial1Bit => "1-bit serial + TS content",
        gr_i2c::TsoutInterface::IsdbsDefault => "regset_ISDBS defaults (rollback)",
        gr_i2c::TsoutInterface::Mode16Serial => "0x16 only -> 0x21",
        gr_i2c::TsoutInterface::Mode16Isdbs => "0x16 only -> 0x00 (rollback)",
        gr_i2c::TsoutInterface::TsSerial20 => "experimental TS serial: 0x16 -> 0x20",
        gr_i2c::TsoutInterface::TsSerial20Restore => "experimental TS serial rollback: 0x16 -> 0x00",
    };
    match gr_i2c::set_tsout_interface(target) {
        Ok(result) => {
            let pairs: Vec<String> = result
                .before
                .iter()
                .zip(&result.after)
                .map(|((reg, b), (_, a))| format!("{reg:02x}: {b:02x} -> {a:02x}"))
                .collect();
            println!("SC1501A bank0 output interface set to {name}: {}", pairs.join(", "));
            0
        }
        Err(e) => {
            eprintln!("mkra: output-interface change refused/failed: {e}");
            1
        }
    }
}

/// Gate 5D read-only snapshot. Emits machine-readable `key=value` lines:
/// watchlist registers (each read twice, marked STABLE/VOLATILE), XCode6 TSI
/// status and MMR, and the `tuner` runtime one-shot flags and S3 context.
/// Writes nothing, anywhere.
fn cmd_sc1501a_snapshot(opts: &Options) -> i32 {
    let label = opts.label.as_deref().unwrap_or("unlabelled");
    println!("snapshot.label={label}");

    let regs = match gr_i2c::snapshot_watchlist() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mkra: watchlist read failed: {e}");
            return 1;
        }
    };
    let cats: Vec<&str> = snapshot::WATCHLIST
        .iter()
        .map(|&(_, _, c)| c)
        .chain(snapshot::RF_WATCHLIST.iter().map(|&(_, c)| c))
        .collect();
    for ((bank, reg, v1, v2), cat) in regs.iter().zip(cats) {
        let state = if v1 == v2 { "STABLE" } else { "VOLATILE" };
        let name = if *bank == 0xff {
            "rf".to_string()
        } else {
            format!("b{bank}")
        };
        println!("reg.{name}.{reg:02x}={v1:02x},{v2:02x},{state},{cat}");
    }

    match XcDrv::open_path(&opts.dev) {
        Ok(drv) => {
            for offset in TSI_MMR_OBSERVE_OFFSETS {
                match drv.mmr_read32(offset) {
                    Ok(v) => println!("mmr.{offset:04x}={v:08x}"),
                    Err(e) => println!("mmr.{offset:04x}=ERR:{e}"),
                }
            }
            for port in 0..=1 {
                match drv.tsi_status(port) {
                    Ok((locked, crc)) => {
                        println!("tsi.{port}.lock={}", u8::from(locked));
                        println!("tsi.{port}.crc={crc:02x}");
                    }
                    Err(e) => println!("tsi.{port}=ERR:{e}"),
                }
            }
        }
        Err(e) => println!("xcdrv=ERR:{e}"),
    }

    match snapshot::procmem::find_pid("tuner") {
        Ok(pid) => {
            println!("tuner.pid={pid}");
            if let Err(e) = snapshot::procmem::verify_layout(pid) {
                eprintln!("tuner.layout=ERR:{e}");
                return 1;
            }
            println!("tuner.layout=build-id:c1eb0b76d320099c843b7a987a89dbb71e08d282");
            match snapshot::procmem::maps(pid) {
                Ok(maps) => {
                    for addr in [snapshot::RECOMMEND_FLAGS, snapshot::PSEQ_FLAGS, snapshot::CONTEXT_BASE] {
                        println!(
                            "tuner.map.{addr:06x}.writable={}",
                            u8::from(snapshot::procmem::is_writable(&maps, addr))
                        );
                    }
                    for line in maps.lines().filter(|l| l.contains("tuner") || l.contains("[heap]")) {
                        println!("tuner.maps={line}");
                    }
                }
                Err(e) => println!("tuner.maps=ERR:{e}"),
            }
            let word = |addr: u32| -> String {
                match snapshot::procmem::read(pid, addr, 4) {
                    Ok(b) => format!("{:08x}", u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    Err(e) => format!("ERR:{e}"),
                }
            };
            for i in 0..4u32 {
                println!("tuner.recommend_flag[{i}]={}", word(snapshot::RECOMMEND_FLAGS + i * 4));
                println!("tuner.pseq_flag[{i}]={}", word(snapshot::PSEQ_FLAGS + i * 4));
            }
            for index in 0..3u32 {
                let base = snapshot::CONTEXT_BASE + index * snapshot::CONTEXT_STRIDE;
                for (off, size, name) in snapshot::CONTEXT_FIELDS {
                    match snapshot::procmem::read(pid, base + off, size as usize) {
                        Ok(b) => {
                            let hex: String = b.iter().rev().map(|x| format!("{x:02x}")).collect();
                            println!("tuner.ctx[{index}].{name}={hex}");
                        }
                        Err(e) => println!("tuner.ctx[{index}].{name}=ERR:{e}"),
                    }
                }
            }
        }
        Err(e) => println!("tuner.pid=ERR:{e}"),
    }
    println!("snapshot.end={label}");
    0
}

/// Gate 5D-A2/A3: clear one `set_system` one-shot flag so SHARP's own regset
/// replay runs on the next tune. `--label pseq` targets the ISDB-S3 PSEQ table,
/// `--label recommend` the common Recommend_DIFF table.
fn cmd_tuner_oneshot_clear(opts: &Options) -> i32 {
    let (addr, what) = match opts.label.as_deref() {
        Some("pseq") => (snapshot::PSEQ_FLAGS, "isdbs3_pseq"),
        Some("recommend") => (snapshot::RECOMMEND_FLAGS, "recommend_diff"),
        other => {
            eprintln!("mkra: --label must be pseq or recommend, got {other:?}");
            return 2;
        }
    };
    let pid = match snapshot::procmem::find_pid("tuner") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mkra: cannot find the tuner process: {e}");
            return 1;
        }
    };
    match snapshot::oneshot::clear(pid, addr) {
        Ok((old, new)) => {
            println!("tuner pid {pid}: {what} one-shot flag {addr:#08x}: {old} -> {new}");
            0
        }
        Err(e) => {
            eprintln!("mkra: one-shot clear refused/failed: {e}");
            1
        }
    }
}

/// Gate 5D-A1: restore the SC1501A TS/TLV output selector to `0x40`.
fn cmd_dmd_cc_select40() -> i32 {
    match gr_i2c::select_tlv_output_exclusive() {
        Ok(r) => {
            println!(
                "SC1501A bank0 0xcc selector: {:#04x} -> {:#04x} (bits[7:6] := 01b)",
                r.before, r.after
            );
            0
        }
        Err(e) => {
            eprintln!("mkra: 0xcc selector repair refused/failed: {e}");
            1
        }
    }
}

/// Cleanup-only: return the demodulator TS output to the measured quiescent
/// bytes. The product has no ISDB-S standby path (`Tune_ISDB_S_OfdmStandby`
/// accepts only dev 0x200/0x400/0x800), so without this the box stays dirty and
/// needs a reboot before the next gate.
fn cmd_dmd_quiesce() -> i32 {
    match gr_i2c::quiesce_ts_output() {
        Ok(result) => {
            println!(
                "SC1501A TS output quiesced: reg 0xcc {:#04x} -> {:#04x}, reg 0x0b bit0 cleared",
                result.before, result.after
            );
            0
        }
        Err(e) => {
            eprintln!("mkra: TS-output quiesce failed: {e}");
            1
        }
    }
}

/// Apply the single Gate 5B12 SC1501A cross-mode repair. This is deliberately
/// a separate command from tune/demux so the runner can verify the exact
/// c0 -> 80 transition before it creates any XCode object.
fn cmd_isdbs_ts_exclusive() -> i32 {
    match gr_i2c::make_isdbs_ts_output_exclusive() {
        Ok(result) => {
            println!(
                "SC1501A page 0x10 reg 0xcc: {:#04x} -> {:#04x}; stale TLV bit6 cleared, TS bit7 retained",
                result.before, result.after
            );
            0
        }
        Err(e) => {
            eprintln!("mkra: ISDB-S exclusive TS repair refused/failed: {e}");
            1
        }
    }
}

// --- options -------------------------------------------------------------

struct Options {
    command: String,
    /// Free-form tag for `sc1501a-snapshot` output (S1..S5).
    label: Option<String>,
    port: u16,
    bind: String,
    dev: String,
    buffer: Option<u32>,
    file: Option<String>,
    file_bitrate: u64,
    file_repeat: bool,
    channels: Option<String>,
    channel_type: String,
    bs2k: bool,
    isdbs_channels: Option<String>,
    test_pattern: bool,
    test_bitrate: u64,
    ring: usize,
    max_clients: usize,
    tune: bool,
    tuner_no: u16,
    input_port: u16,
    reply_qid: u32,
    tune_settle_ms: u64,
    scan: bool,
    scan_max: u32,
    graph: bool,
    pids: Vec<u32>,
    accept_oops_risk: bool,
    find_buffers: bool,
    peek: Option<(u32, u64, usize)>,
    seconds: u64,
    out: Option<String>,
    watch: u32,
    sniff: bool,
    raw: bool,
    phys: Option<u32>,
    len: usize,
    freq_khz: u32,
    tsid: u16,
    passive: bool,
    raw_stream: bool,
    epg_dir: Option<String>,
    epg_refresh_secs: u64,
    epg_tap: bool,
    fed: Option<String>,
    path: Option<u32>,
    dry_run: bool,
    demux_no_filter: bool,
    demux_pat_section: bool,
    demux_wait_tsi_after_run: bool,
    demux_inspect_pid_filter: bool,
    demux_observe_resources: bool,
    demux_observe_sf_ring: bool,
    demux_observe_tsi_route: bool,
    demux_ready_before_filter: bool,
    /// remux4k: run only the read-only discovery step and print what it found.
    remux4k_discover_only: bool,
    /// Diagnostic CLI only; verify fresh ES writes before bypassing the decoder guard.
    remux4k_allow_no_decoder: bool,
    /// remux4k: also inject a locally generated PAT/PMT into the output.
    remux4k_inject_psi: bool,
    /// remux4k: re-run the device whenever a whole second passes with no data.
    remux4k_kick: bool,
    /// remux4k: abort if the REMUX produces nothing for this long (0 disables).
    remux4k_stall_abort_ms: u64,
    /// remux4k: abort when the live video ES ring backs up to this level.
    remux4k_es_level_abort: u32,
    /// remux4k: skip the TSINFO side channel entirely.
    remux4k_no_tsinfo: bool,
    /// serve: use the BS4K REMUX tap as the TS backend.
    remux4k_backend: bool,
    /// tune4k: advanced-BS service id to select.
    sid: Option<u16>,
    /// tune4k: operation id echoed by the product.
    oid: u32,
    /// tune4k: the `Tune` word (0 = channel change, 3 = resume from standby).
    tune_mode: u32,
    /// tune4k: set the `NotDecode` word.
    no_decode: bool,
    /// serve: stop the box decoding after this many idle seconds (0 = never).
    idle_sleep_secs: u64,
    /// serve: transponder to park on when idle (no signal here = decoders idle).
    park_freq_khz: u32,
    park_tsid: u16,
    fem_dev: Option<u32>,
    disconnect: bool,
    reader: Option<u32>,
    cpld_reg: Option<u8>,
    cpld_set: Option<u8>,
    cpld_expect: Option<u8>,
    tsi_port: u32,
    /// `me_packet` for the physical TSI request. 0=188, 1=192, 2=204, 3=130.
    tsi_packet: u32,
    /// `m_use_sync_bit`, word 4 of the physical TSI request. Gate 5E2-SYNC.
    tsi_use_sync: bool,
    tsi_rising_edge: bool,
    tsi_valid_low: bool,
    tsi_parallel: bool,
    channel: Option<u8>,
    if_hz: u32,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Options {
            label: None,
            tsi_packet: xcdrv::ME_PACKET_188,
            tsi_use_sync: false,
            tsi_rising_edge: false,
            tsi_valid_low: false,
            tsi_parallel: false,
            command: args[0].clone(),
            port: 40772,
            bind: "0.0.0.0".into(),
            dev: xcdrv::DEV_PATH.into(),
            buffer: None,
            file: None,
            file_bitrate: 0,
            file_repeat: false,
            channels: None,
            channel_type: channels::DEFAULT_CHANNEL_TYPE.to_string(),
            bs2k: false,
            isdbs_channels: None,
            test_pattern: false,
            test_bitrate: 8_000_000,
            ring: 2 * 1024 * 1024,
            max_clients: 4,
            tune: false,
            tuner_no: 1,
            input_port: 101,
            reply_qid: 0,
            tune_settle_ms: 1500,
            scan: false,
            scan_max: 64,
            graph: false,
            pids: Vec::new(),
            accept_oops_risk: false,
            find_buffers: false,
            peek: None,
            seconds: 3,
            out: None,
            watch: 0,
            sniff: false,
            raw: false,
            phys: None,
            len: 256,
            freq_khz: 0,
            tsid: 0xFFFF,
            passive: false,
            raw_stream: false,
            epg_dir: None,
            epg_refresh_secs: 300,
            epg_tap: false,
            fed: None,
            path: None,
            dry_run: false,
            demux_no_filter: false,
            demux_pat_section: false,
            demux_wait_tsi_after_run: false,
            demux_inspect_pid_filter: false,
            demux_observe_resources: false,
            demux_observe_sf_ring: false,
            demux_observe_tsi_route: false,
            demux_ready_before_filter: false,
            remux4k_discover_only: false,
            remux4k_allow_no_decoder: false,
            remux4k_inject_psi: false,
            remux4k_kick: false,
            remux4k_stall_abort_ms: 5000,
            remux4k_es_level_abort: 48,
            remux4k_no_tsinfo: true,
            remux4k_backend: false,
            sid: None,
            oid: 32,
            tune_mode: tuner::TUNE_CHANNEL_CHANGE,
            no_decode: false,
            idle_sleep_secs: 0,
            park_freq_khz: 12_431_000,
            park_tsid: 0xC090,
            fem_dev: None,
            disconnect: false,
            reader: None,
            cpld_reg: None,
            cpld_set: None,
            cpld_expect: None,
            tsi_port: 0,
            channel: None,
            if_hz: gr::DEFAULT_IF_HZ,
        };

        let mut tsi_port_given = false;
        let mut i = 1;
        while i < args.len() {
            let a = args[i].as_str();
            let mut value = || -> Result<String, String> {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{a} needs a value"))
            };
            match a {
                "--port" => o.port = parse_num(&value()?, a)? as u16,
                "--bind" => o.bind = value()?,
                "--dev-node" => o.dev = value()?,
                "--dev" => o.fem_dev = Some(parse_num(&value()?, a)? as u32),
                "--disconnect" => o.disconnect = true,
                "--reader" => o.reader = Some(parse_num(&value()?, a)? as u32),
                "--reg" => o.cpld_reg = Some(parse_byte(&value()?, a)?),
                "--set" => o.cpld_set = Some(parse_byte(&value()?, a)?),
                "--expect" => o.cpld_expect = Some(parse_byte(&value()?, a)?),
                "--tsi-port" => {
                    o.tsi_port = parse_num(&value()?, a)? as u32;
                    tsi_port_given = true;
                }
                "--channel" => o.channel = Some(parse_byte(&value()?, a)?),
                "--if-hz" => o.if_hz = parse_u32(&value()?, a)?,
                "--buffer" => o.buffer = Some(parse_num(&value()?, a)? as u32),
                "--file" => o.file = Some(value()?),
                "--file-bitrate" => o.file_bitrate = parse_num(&value()?, a)?,
                "--file-repeat" => o.file_repeat = true,
                "--channels" => o.channels = Some(value()?),
                "--epg-dir" => o.epg_dir = Some(value()?),
                "--epg-tap" => o.epg_tap = true,
                "--fed" => o.fed = Some(value()?),
                "--path" => o.path = Some(parse_num(&value()?, a)? as u32),
                "--dry-run" => o.dry_run = true,
                "--no-filter" => o.demux_no_filter = true,
                "--pat-section" => o.demux_pat_section = true,
                "--wait-tsi-after-run" => o.demux_wait_tsi_after_run = true,
                "--inspect-pid-filter" => o.demux_inspect_pid_filter = true,
                "--observe-resources" => o.demux_observe_resources = true,
                "--observe-sf-ring" => o.demux_observe_sf_ring = true,
                "--observe-tsi-route" => o.demux_observe_tsi_route = true,
                "--ready-before-filter" => o.demux_ready_before_filter = true,
                "--discover-only" => o.remux4k_discover_only = true,
                "--allow-no-decoder" => o.remux4k_allow_no_decoder = true,
                "--inject-psi" => o.remux4k_inject_psi = true,
                "--kick" => o.remux4k_kick = true,
                "--stall-abort-ms" => o.remux4k_stall_abort_ms = parse_num(&value()?, a)?,
                "--es-level-abort" => o.remux4k_es_level_abort = parse_num(&value()?, a)? as u32,
                "--no-tsinfo" => o.remux4k_no_tsinfo = true,
                "--with-tsinfo" => o.remux4k_no_tsinfo = false,
                "--remux4k" => o.remux4k_backend = true,
                "--sid" => o.sid = Some(parse_num(&value()?, a)? as u16),
                "--oid" => o.oid = parse_num(&value()?, a)? as u32,
                "--tune-mode" => o.tune_mode = parse_num(&value()?, a)? as u32,
                "--no-decode" => o.no_decode = true,
                "--idle-sleep" => o.idle_sleep_secs = parse_num(&value()?, a)?,
                "--park-freq" => o.park_freq_khz = parse_num(&value()?, a)? as u32,
                "--park-tsid" => o.park_tsid = u16::from_str_radix(value()?.trim_start_matches("0x"), 16).map_err(|e| format!("--park-tsid: {e}"))?,
                "--epg-refresh" => o.epg_refresh_secs = parse_num(&value()?, a)? as u64,
                "--channel-type" => o.channel_type = value()?,
                "--bs2k" => o.bs2k = true,
                "--isdbs-channels" => o.isdbs_channels=Some(value()?),
                "--test-pattern" => o.test_pattern = true,
                "--test-bitrate" => o.test_bitrate = parse_num(&value()?, a)?,
                "--ring" => o.ring = parse_num(&value()?, a)? as usize,
                "--max-clients" => o.max_clients = parse_num(&value()?, a)? as usize,
                "--tune" => o.tune = true,
                "--tuner-no" => o.tuner_no = parse_num(&value()?, a)? as u16,
                "--input-port" => o.input_port = parse_num(&value()?, a)? as u16,
                "--reply-qid" => o.reply_qid = parse_num(&value()?, a)? as u32,
                "--tune-settle-ms" => o.tune_settle_ms = parse_num(&value()?, a)?,
                "--scan" => o.scan = true,
                "--i-accept-oops-risk" => o.accept_oops_risk = true,
                "--scan-max" => o.scan_max = parse_num(&value()?, a)? as u32,
                "--graph" => o.graph = true,
                "--find-buffers" => o.find_buffers = true,
                "--peek" => {
                    // pid:addr:len -- read-only inspection of live product state
                    let v = value()?;
                    let f: Vec<&str> = v.split(':').collect();
                    if f.len() != 3 {
                        return Err(format!("--peek wants pid:addr:len, got {v}"));
                    }
                    o.peek = Some((
                        parse_num(f[0], a)? as u32,
                        parse_num(f[1], a)?,
                        parse_num(f[2], a)? as usize,
                    ));
                }
                "--pid" => o.pids.push(parse_num(&value()?, a)? as u32),
                "--label" => o.label = Some(value()?),
                "--tsi-use-sync" => o.tsi_use_sync = true,
                "--tsi-rising-edge" => o.tsi_rising_edge = true,
                "--tsi-valid-low" => o.tsi_valid_low = true,
                "--tsi-parallel" => o.tsi_parallel = true,
                "--tsi-packet" => {
                    o.tsi_packet = match value()?.as_str() {
                        "188" => xcdrv::ME_PACKET_188,
                        "192" => xcdrv::ME_PACKET_192,
                        "204" => xcdrv::ME_PACKET_204,
                        "130" => xcdrv::ME_PACKET_130,
                        other => return Err(format!("--tsi-packet must be 188|192|204|130, got {other}")),
                    }
                }
                "--seconds" => o.seconds = parse_num(&value()?, a)?,
                "--out" => o.out = Some(value()?),
                "--watch" => o.watch = parse_num(&value()?, a)? as u32,
                "--sniff" => o.sniff = true,
                "--raw" => o.raw = true,
                "--phys" => o.phys = Some(parse_num(&value()?, a)? as u32),
                "--len" => o.len = parse_num(&value()?, a)? as usize,
                "--freq" => o.freq_khz = parse_num(&value()?, a)? as u32,
                "--tsid" => {
                    let v = value()?;
                    o.tsid = u16::from_str_radix(v.trim_start_matches("0x"), 16)
                        .map_err(|_| format!("--tsid: `{v}` is not hex"))?;
                }
                "--passive" => o.passive = true,
                "--raw-stream" => o.raw_stream = true,
                other => return Err(format!("unknown option `{other}`")),
            }
            i += 1;
        }
        if o.remux4k_allow_no_decoder && o.command != "remux4k" {
            return Err("--allow-no-decoder is only supported by remux4k".into());
        }
        // The priority path is BS/CS 2K, whose product system=2 setup maps
        // to physical TSI1. GR uses system=0/TSI0 and must opt in explicitly
        // until the high-level tuner command performs that mapping itself.
        if o.command == "demux" && !tsi_port_given {
            o.tsi_port = 1;
        }
        Ok(o)
    }
}

/// Accept decimal or `0x`-prefixed hex, since driver handles are natural in hex
/// but everything else is decimal.
fn parse_num(s: &str, flag: &str) -> Result<u64, String> {
    let t = s.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        t.parse()
    };
    parsed.map_err(|_| format!("{flag}: `{s}` is not a number"))
}

fn parse_byte(s: &str, flag: &str) -> Result<u8, String> {
    let n = parse_num(s, flag)?;
    u8::try_from(n).map_err(|_| format!("{flag}: `{s}` is outside 0..255"))
}

fn parse_u32(s: &str, flag: &str) -> Result<u32, String> {
    let n = parse_num(s, flag)?;
    u32::try_from(n).map_err(|_| format!("{flag}: `{s}` is outside u32"))
}

/// Per-channel tuning parameters, keyed by Mirakurun channel key.
///
/// The frequency and TSID come from the channel table; the rest are process
/// settings. Built even when `--tune` is off so that `--tune` cannot turn a
/// bad table into a bad frontend command at request time.
fn tune_plans(table: &ChannelTable, opts: &Options) -> BTreeMap<String, tuner::TuneRequest> {
    table
        .channels
        .iter()
        .map(|c| {
            (
                c.key.clone(),
                tuner::TuneRequest {
                    tuner_no: opts.tuner_no,
                    freq_khz: c.freq_khz,
                    stream_id: c.tsid,
                    input_port_no: opts.input_port,
                    reply_qid: opts.reply_qid,
                },
            )
        })
        .collect()
}

fn validate_plans(plans: &BTreeMap<String, tuner::TuneRequest>) -> Result<(), String> {
    for (key, req) in plans {
        req.validate().map_err(|e| format!("channel {key}: {e}"))?;
    }
    Ok(())
}

fn load_channels(opts: &Options) -> Result<ChannelTable, String> {
    match &opts.channels {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read channel table {path}: {e}"))?;
            ChannelTable::from_tsv(&text)
        }
        None => Ok(ChannelTable::builtin()),
    }
}

// --- environment warnings ------------------------------------------------

/// The box only builds its A/V and remux pipelines once it leaves standby, so
/// a probe run in standby is guaranteed to find nothing. Say so rather than
/// letting the operator conclude the approach failed (docs/SETUP.md: the box must be out of standby).
fn warn_if_standby() {
    let operating = std::fs::read_dir("/proc").is_ok_and(|entries| {
        entries.filter_map(|e| e.ok()).any(|e| {
            let comm = e.path().join("comm");
            std::fs::read_to_string(comm)
                .map(|c| {
                    let c = c.trim();
                    c == "MIXER" || c == "buf_buf" || c.starts_with("Sink")
                })
                .unwrap_or(false)
        })
    });
    if !operating {
        eprintln!(
            "mkra: warning: no A/V pipeline threads found — the box looks like it is in\n\
             mkra:          standby (LED red). Press the power button until the LED is green\n\
             mkra:          first; in standby there is no remux output to read. (docs/SETUP.md: the box must be out of standby)"
        );
    }
    if let Some(up) = sys::uptime_secs() {
        eprintln!("mkra: uptime {up:.0}s");
    }
}

// --- serve ---------------------------------------------------------------

/// Read every section ring the driver knows about, whole.
///
/// "Section ring" is decided by measurement, not configuration: a ring whose
/// object passes the driver's own magic check and whose payload parses as
/// MMT-SI. Rings are small (36 KB, or 512 KB for the TLV-NIT/AMT pair), so
/// copying all of them costs a few hundred KB per pass.
///
/// Reading the whole ring instead of following the write pointer is deliberate:
/// sections repeat on a carousel, duplicates cost nothing (they key by
/// programme id), and it means a pass that arrives late still sees whatever is
/// currently resident rather than nothing at all.
fn scrape_section_rings(reader: &graph::ObjReader) -> std::io::Result<Vec<Vec<u8>>> {
    // Section rings top out at 512 KB (the TLV-NIT and AMT pair); anything
    // larger is an ES or PCM ring and cannot contain SI.
    const MAX_RING: u32 = 512 * 1024;
    let g = graph::Graph::read()?;
    let mut out = Vec::new();
    for handle in g.buffer_handles() {
        let Ok(obj) = reader.object(handle) else {
            continue;
        };
        if !obj.ring_sane() || obj.size > MAX_RING {
            continue;
        }
        if let Ok(bytes) = reader.read_phys(obj.data, obj.size as usize) {
            out.push(bytes);
        }
    }
    Ok(out)
}

fn cmd_serve(opts: &Options) -> i32 {
    xcdrv::TRACE_READS.store(false,Ordering::Relaxed);
    let mut table = match load_channels(opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: {e}");
            return 1;
        }
    };
    table.set_channel_type(&opts.channel_type);
    if opts.bs2k {
        if !opts.remux4k_backend || !opts.tune || opts.file.is_some() || opts.test_pattern || opts.idle_sleep_secs>0 || opts.raw_stream {
            eprintln!("mkra: --bs2k requires --remux4k --tune, without file/test/raw/idle-sleep");return 2;
        }
        table.add_verified_bs2k();
        if let Some(path)=&opts.isdbs_channels {
            let result=std::fs::read_to_string(path).map_err(|e|e.to_string()).and_then(|s|table.add_isdbs_tsv(&s));
            if let Err(e)=result {eprintln!("mkra: ISDB-S channel table: {e}");return 2;}
        }
    }
    if opts.isdbs_channels.is_some() && !opts.bs2k {eprintln!("mkra: --isdbs-channels requires --bs2k");return 2;}
    // Cross-process ownership for the integrated physical frontend.
    let _frontend_lock=if opts.bs2k {
        use std::os::fd::AsRawFd;
        extern "C" {fn flock(fd:i32,op:i32)->i32;}
        let file=match std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open("/tmp/mkra-tuner.lock") {Ok(f)=>f,Err(e)=>{eprintln!("mkra: tuner lock: {e}");return 1;}};
        if unsafe {flock(file.as_raw_fd(),2|4)}!=0 {eprintln!("mkra: another integrated server owns the tuner");return 1;}
        Some(file)
    }else {None};

    // Set while the tuner is parked on a dead transponder. The SI still names
    // the old service then, so the "already on the right service" shortcut in
    // the factory must not be taken; the idle thread and the factory share it.
    let parked = Arc::new(AtomicBool::new(false));

    // Decide the TS backend once, up front, so the operator learns immediately
    // rather than at the first stream request.
    let (factory, backend): (stream::SourceFactory, String) = if opts.test_pattern {
        // Each channel gets its own TSID/service so a client can tell them
        // apart; the table is cloned in because the factory outlives it.
        let by_key: BTreeMap<String, (u16, u16, String)> = table
            .channels
            .iter()
            .map(|c| {
                let s = c.services.first();
                (
                    c.key.clone(),
                    (
                        c.tsid,
                        s.map(|s| s.service_id).unwrap_or(1),
                        format!("mkra test {}", c.key),
                    ),
                )
            })
            .collect();
        let rate = opts.test_bitrate;
        (
            Box::new(move |key: &str| {
                let (tsid, sid, name) = by_key
                    .get(key)
                    .cloned()
                    .unwrap_or((0x0001, 1, "mkra test".into()));
                Ok(Box::new(testts::TestPatternSource::new(
                    tsid,
                    channels::NETWORK_ID,
                    sid,
                    &name,
                    rate,
                )) as Box<dyn TsSource>)
            }),
            format!("test pattern ({rate} bit/s)"),
        )
    } else if let Some(path) = opts.file.clone() {
        let bitrate = opts.file_bitrate;
        let repeat = opts.file_repeat;
        let desc = format!("file {path}");
        (
            Box::new(move |_| {
                FileSource::open(&path, bitrate, repeat).map(|s| Box::new(s) as Box<dyn TsSource>)
            }),
            desc,
        )
    } else if opts.remux4k_backend {
        if !opts.bs2k {warn_if_standby();}
        let drv = match XcDrv::open_path(&opts.dev) {
            Ok(d) => Arc::new(d),
            Err(e) => {
                eprintln!("mkra: cannot open {}: {e}", opts.dev);
                return 1;
            }
        };
        // One physical frontend, one MMT demux: the box shows exactly one
        // service, and which one is the product's own decision (remote
        // control) until tuning is implemented. So a request is answered only
        // when it asks for the service that is actually on air; anything else
        // gets an error instead of a stream labelled with the wrong service,
        // which would quietly poison a client's service table.
        let want: BTreeMap<String, u16> = table
            .channels
            .iter()
            .filter_map(|c| c.services.first().map(|s| (c.key.clone(), s.service_id)))
            .collect();
        let legacy=Arc::new(AtomicBool::new(if opts.bs2k {
            match bs2k::legacy_frontend() {Ok(v)=>v,Err(e)=>{eprintln!("mkra: frontend mode: {e}");return 1;}}
        }else{false}));
        let dirty=Arc::new(AtomicBool::new(false));
        let unified=opts.bs2k;
        let legacy_channels:BTreeMap<String,(u32,u16,u16)>=table.channels.iter().filter(|c|c.is_bs2k()).filter_map(|c|c.services.first().map(|s|(c.key.clone(),(c.freq_khz,c.tsid,s.service_id)))).collect();
        let base = remux4k::TapConfig {
            with_tsinfo: !opts.remux4k_no_tsinfo,
            es_level_abort: opts.remux4k_es_level_abort,
            stall_abort_ms: opts.remux4k_stall_abort_ms,
            ..remux4k::TapConfig::default()
        };
        // Tuning is opt-in. With `--tune`, a request for another service asks
        // the product to change channel exactly as its remote control does and
        // waits for its own SI to confirm; without it, mkra serves only what
        // the box is already showing.
        let may_tune = opts.tune;
        let oid = Arc::new(AtomicU32::new(0x40));
        let parked_for_factory = Arc::clone(&parked);
        (
            Box::new(move |key: &str| {
                if dirty.load(Ordering::SeqCst) {return Err(io::Error::other("previous cleanup failed; tuner is blocked"))}
                if let Some(&(freq,tsid,sid))=legacy_channels.get(key) {
                    return bs2k::Source::open(Arc::clone(&drv),freq,tsid,sid,&legacy,Arc::clone(&dirty)).map(|s|Box::new(s) as Box<dyn TsSource>);
                }
                if !want.contains_key(key) {return Err(io::Error::other("unknown channel"))}
                let restored=legacy.load(Ordering::SeqCst);
                if restored {
                    bs2k::restore_frontend()?;
                    legacy.store(false,Ordering::SeqCst);
                }
                if unified {
                    let asked=want[key];
                    let live=unified_tune4k(&drv,asked,&oid,restored)?;
                    let no_decoder=remux4k::discover(&drv).is_ok_and(|es|es.video_readers==0 || es.audio_readers==0);
                    let cfg=remux4k::TapConfig {stream_id:live.tlv_stream_id, service_id:live.service_id,
                        allow_no_decoder:true, es_level_abort:if no_decoder {101}else{base.es_level_abort}, ..base};
                    return remux4k::Tap::open(Arc::clone(&drv),cfg).map(|t|Box::new(t) as Box<dyn TsSource>);
                }
                let mut live = si::live_service(si::ADSB_SI_DIR).ok_or_else(|| {
                    io::Error::other(format!(
                        "cannot tell which service the box is showing ({} unreadable or mid-change)",
                        si::ADSB_SI_DIR
                    ))
                })?;
                if let Some(&asked) = want.get(key) {
                    // Decide by the box's state first. With no ES rings it is
                    // in standby (its own, or asleep), and there the
                    // channel-change form is accepted and ignored — measured:
                    // the SI never moves. The resume form is what the product
                    // sends when it wakes, and it both wakes and tunes.
                    let was_parked = parked_for_factory.swap(false, Ordering::Relaxed);
                    if may_tune && was_parked {
                        // Parked: whatever the SI says, the chain is starved.
                        live = tune_and_wait(asked, &oid)?;
                    }
                    let decoding = remux4k::discover(&drv).is_ok();
                    if may_tune && !decoding {
                        // Asleep. Measured: the resume message only restarts
                        // the service the box already has — asking it to wake
                        // *and* switch in one message is ignored, exactly like
                        // the product, which resumes the last channel first and
                        // changes channel afterwards. So do the same two steps.
                        eprintln!("mkra: no live ES rings — waking the box for {key}");
                        live = tune_and_wait_mode(live.service_id, &oid, tuner::TUNE_RESUME)?;
                    }
                    if may_tune && asked != live.service_id {
                        live = tune_and_wait(asked, &oid)?;
                    }
                    if asked != live.service_id {
                        return Err(io::Error::other(format!(
                            "channel {key} is service {asked}, but the box is showing {} (TLV stream {:#06x}). Start mkra with --tune, or select it on the box.",
                            live.service_id, live.tlv_stream_id
                        )));
                    }
                }
                eprintln!(
                    "mkra: remux4k tap for {key}: service {} on TLV stream {:#06x}",
                    live.service_id, live.tlv_stream_id
                );
                let cfg = remux4k::TapConfig {
                    stream_id: live.tlv_stream_id,
                    service_id: live.service_id,
                    allow_no_decoder: unified,
                    es_level_abort: if unified && remux4k::discover(&drv).is_ok_and(|es|es.video_readers==0 || es.audio_readers==0) {101}else{base.es_level_abort},
                    ..base
                };
                match remux4k::Tap::open(Arc::clone(&drv), cfg) {
                    Ok(t) => Ok(Box::new(t) as Box<dyn TsSource>),
                    Err(first) => {
                        // One retry: the product can still be settling right
                        // after a change, and a second later the same sequence
                        // succeeds.
                        eprintln!("mkra: tap failed ({first}); retrying once");
                        std::thread::sleep(Duration::from_millis(1000));
                        let live = si::live_service(si::ADSB_SI_DIR).unwrap_or(live);
                        let cfg = remux4k::TapConfig {
                            stream_id: live.tlv_stream_id,
                            service_id: live.service_id,
                            allow_no_decoder: unified,
                            es_level_abort: if unified && remux4k::discover(&drv).is_ok_and(|es|es.video_readers==0 || es.audio_readers==0) {101}else{base.es_level_abort},
                            ..base
                        };
                        remux4k::Tap::open(Arc::clone(&drv), cfg)
                            .map(|t| Box::new(t) as Box<dyn TsSource>)
                    }
                }
            }),
            if unified {"BS4K REMUX + BS2K native CAS/MULTI2 (one tuner)"}else{"BS4K REMUX tap (live MMT -> MPEG-TS)"}.to_string(),
        )
    } else if let Some(handle) = opts.buffer {
        warn_if_standby();
        let drv = match XcDrv::open_path(&opts.dev) {
            Ok(d) => Arc::new(d),
            Err(e) => {
                eprintln!("mkra: cannot open {}: {e}", opts.dev);
                return 1;
            }
        };

        // Tuning is opt-in: commanding a tune changes what the box is showing,
        // and its interaction with the running stack is unproven
        // (DESIGN.md §6.3). Resolve the queue up front so a misconfiguration
        // fails at startup rather than at the first stream request.
        let tuner = if opts.tune {
            match tuner::Tuner::open(tuner::MSGH_LIST) {
                Ok(t) => {
                    eprintln!(
                        "mkra: tuning enabled (qid {} -> msqid {})",
                        tuner::FEMD_QID,
                        t.msqid()
                    );
                    Some(Arc::new(t))
                }
                Err(e) => {
                    eprintln!("mkra: --tune requested but the tuner queue is unavailable: {e}");
                    return 1;
                }
            }
        } else {
            None
        };

        let plans = tune_plans(&table, opts);
        if let Err(e) = validate_plans(&plans) {
            eprintln!("mkra: {e}");
            return 1;
        }

        let settle = Duration::from_millis(opts.tune_settle_ms);
        let passive = opts.passive;
        let desc = format!("xcodedrv buffer {handle:#x}{}",
                           if passive { " (passive tap)" } else { "" });
        (
            Box::new(move |key: &str| {
                if let (Some(t), Some(req)) = (&tuner, plans.get(key)) {
                    eprintln!(
                        "mkra: tuning {key}: {} kHz TSID {:04X} on tuner {}",
                        req.freq_khz, req.stream_id, req.tuner_no
                    );
                    t.tune(req)?;
                    // No reply queue is available to us, so lock is inferred
                    // from data appearing; give the frontend time to settle
                    // before the first read (DESIGN.md §6.2).
                    std::thread::sleep(settle);
                }
                // `--passive` follows the ring's own write pointer instead of
                // asking the driver for a reader port. Needed for buffers that
                // refuse a host connection, and it never perturbs the product.
                if passive {
                    PassiveRingSource::open(Arc::clone(&drv), handle)
                        .map(|s| Box::new(s) as Box<dyn TsSource>)
                } else {
                    tssrc::XcReaderSource::attach(Arc::clone(&drv), handle, 1000)
                        .map(|s| Box::new(s) as Box<dyn TsSource>)
                }
            }),
            desc,
        )
    } else {
        eprintln!(
            "mkra: no TS backend given (--remux4k/--buffer/--file); the JSON API works but\n\
             mkra: streams will produce nothing. Find a buffer handle with\n\
             mkra: `mkra probe --pid <pid of adsb>` (read-only). Do NOT use --scan:\n\
             mkra: a wrong handle oopses the driver (DESIGN.md §4.4)."
        );
        (
            Box::new(|_| Ok(Box::new(NullSource) as Box<dyn TsSource>)),
            "none".into(),
        )
    };

    if opts.tune && opts.file.is_some() {
        eprintln!("mkra: --tune has no effect with --file; ignoring it");
    }

    if opts.raw_stream {
        stream::RAW_PASSTHROUGH.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!("mkra: --raw-stream: serving bytes verbatim (no TS alignment)");
    }

    let broker = Broker::new(
        BrokerConfig {
            ring_bytes: opts.ring,
            max_clients: opts.max_clients,
            linger: Duration::from_secs(3),
        },
        factory,
    );

    let state = Arc::new(api::AppState::new(
        table,
        Arc::clone(&broker),
        opts.tune,
        backend.clone(),
    ));

    // EPG comes from the product's own MMT-SI cache, so it is independent of
    // whether the TS backend works. Load it once now for a fast first answer,
    // then keep a refresher running: `adsb` rewrites those files as it acquires
    // tables, and a schedule can take many minutes to fill in after a boot.
    if let Some(dir) = opts.epg_dir.clone() {
        match state.reload_epg(&dir) {
            Ok(n) => eprintln!("mkra: EPG loaded from {dir}: {n} programs"),
            Err(e) => eprintln!("mkra: EPG not available yet from {dir}: {e}"),
        }
        let refresher = Arc::clone(&state);
        let every = Duration::from_secs(opts.epg_refresh_secs.max(30));
        std::thread::Builder::new()
            .name("epg".into())
            .stack_size(64 * 1024)
            .spawn(move || loop {
                std::thread::sleep(every);
                match refresher.reload_epg(&dir) {
                    Ok(n) => eprintln!("mkra: EPG refreshed: {n} programs"),
                    Err(e) => eprintln!("mkra: EPG refresh failed: {e}"),
                }
            })
            .ok();
    }

    // Live EPG: read the hardware section rings directly.
    //
    // This is the source that works when --epg-dir does not. `adsb` only
    // rewrites /tmp/adsb/*.dat while it is presenting a service, so on a box
    // with nothing on its HDMI output those files can stay empty for an entire
    // session -- but the section filters run whenever the frontend is locked,
    // even in standby. Reading them costs no buffer ioctl: xc_map on the ring's
    // own physical address, exactly what --passive already does.
    if opts.epg_tap {
        match XcDrv::open_path(&opts.dev) {
            Ok(d) => {
                let drv = Arc::new(d);
                let tap_state = Arc::clone(&state);
                let every = Duration::from_secs(opts.epg_refresh_secs.clamp(5, 3600));
                std::thread::Builder::new()
                    .name("epg-tap".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let reader = graph::ObjReader::new(Arc::clone(&drv));
                        loop {
                            match scrape_section_rings(&reader) {
                                Ok(chunks) if !chunks.is_empty() => {
                                    let (n, total) = tap_state.merge_ring_si(&chunks);
                                    if n > 0 {
                                        eprintln!(
                                            "mkra: EPG tap: +{n} sections from {} ring(s), {total} programs",
                                            chunks.len()
                                        );
                                    }
                                }
                                Ok(_) => eprintln!("mkra: EPG tap: no readable section ring"),
                                Err(e) => eprintln!("mkra: EPG tap failed: {e}"),
                            }
                            std::thread::sleep(every);
                        }
                    })
                    .ok();
            }
            Err(e) => eprintln!("mkra: --epg-tap needs {}: {e}", opts.dev),
        }
    }

    // Idle sleep. Nothing to tap means nothing to serve, so when no client has
    // been connected for a while the decode chain can go: it is the same state
    // the product itself uses in standby, and waking is one message away
    // (measured: graph 22 devices -> 11 on sleep, back within 3 s).
    if opts.remux4k_backend && opts.tune && opts.idle_sleep_secs > 0 {
        let idle_broker = Arc::clone(&broker);
        let idle = Duration::from_secs(opts.idle_sleep_secs);
        let idle_oid = Arc::new(AtomicU32::new(0x80));
        let idle_parked = Arc::clone(&parked);
        let (park_freq, park_tsid) = (opts.park_freq_khz, opts.park_tsid);
        std::thread::Builder::new()
            .name("mkra-idle".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let mut since = Instant::now();
                let mut asleep = false;
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    if idle_broker.current().is_some() {
                        since = Instant::now();
                        asleep = false;
                        continue;
                    }
                    if asleep || since.elapsed() < idle {
                        continue;
                    }
                    match park_tuner(&idle_oid, park_freq, park_tsid) {
                        Ok(()) => {
                            idle_parked.store(true, Ordering::Relaxed);
                            asleep = true;
                        }
                        Err(e) => eprintln!("mkra: idle park failed: {e}"),
                    }
                }
            })
            .ok();
    }

    let addr = format!("{}:{}", opts.bind, opts.port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mkra: cannot bind {addr}: {e}");
            return 1;
        }
    };

    eprintln!(
        "mkra {} listening on {addr} — {} channels, backend: {backend}",
        api::MKRA_VERSION,
        state.table.channels.len()
    );

    let handler_state = Arc::clone(&state);
    let result = http::serve(
        listener,
        opts.max_clients + 8, // stream clients plus room for polling requests
        Arc::new(move |req, sock| api::handle(&handler_state, req, sock)),
    );

    broker.shutdown();
    if let Err(e) = result {
        eprintln!("mkra: server stopped: {e}");
        return 1;
    }
    0
}

// --- probe ---------------------------------------------------------------

/// Answers the questions DESIGN.md §10 leaves open, read-only.
fn cmd_probe(opts: &Options) -> i32 {
    warn_if_standby();

    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            eprintln!("mkra: (the driver node only exists on the device)");
            return 1;
        }
    };
    println!("opened {}", opts.dev);

    if opts.graph {
        // Read-only by name; dump whatever comes back so the layout can be
        // worked out offline.
        let seed = [0u32; 8];
        match drv.get_graph_raw(&seed) {
            Ok(v) => {
                println!("xc_get_graph(0x86) returned:");
                for (i, w) in v.iter().enumerate() {
                    println!("  [{i}] {w:#010x} ({w})");
                }
            }
            Err(e) => println!("xc_get_graph(0x86) failed: {e}"),
        }
    }

    if let Some((pid, addr, len)) = opts.peek {
        match findbuf::peek_process(pid, addr, len) {
            Ok(b) => {
                println!("pid {pid} {addr:#x} ({} bytes)", b.len());
                for (i, row) in b.chunks(16).enumerate() {
                    let hex: Vec<String> = row.iter().map(|x| format!("{x:02x}")).collect();
                    let asc: String = row.iter()
                        .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' })
                        .collect();
                    println!("  {:#010x}  {:<47}  {asc}", addr as usize + i * 16, hex.join(" "));
                }
                return 0;
            }
            Err(e) => {
                eprintln!("mkra: peek failed: {e}");
                return 1;
            }
        }
    }

    if opts.find_buffers {
        return find_buffers(opts);
    }

    if !opts.pids.is_empty() {
        let mut all = Vec::new();
        for &pid in &opts.pids {
            let fds = xcodedrv_fds(pid);
            let comm = sys::read_text(&format!("/proc/{pid}/comm"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if fds.is_empty() {
                println!("\npid {pid} ({comm}) holds no xcodedrv fd");
                continue;
            }
            println!("\npid {pid} ({comm}) xcodedrv fds: {fds:?}");
            match find_handles(pid, &fds) {
                Ok(hits) if hits.is_empty() => {
                    println!("  no host-connection objects in its heap");
                }
                Ok(hits) => {
                    for (fd, bufh, rdr, is_write) in &hits {
                        // This signature (word0 == an fd number) matches a
                        // lot of ordinary heap data, so say plainly which hits
                        // survive the driver's own validity test.
                        let verdict = match findbuf::validate("/dev/mem", *bufh) {
                            findbuf::Validity::Valid => "VALID BUFFER",
                            findbuf::Validity::NotABuffer(_) => "not a buffer",
                            findbuf::Validity::Uncheckable(_) => "unverifiable",
                        };
                        println!(
                            "  fd {fd}  buffer {bufh:#x}  reader {rdr:#x}  {}  [{verdict}]",
                            if *is_write == 1 { "writer" } else { "reader" }
                        );
                        if matches!(findbuf::validate("/dev/mem", *bufh), findbuf::Validity::Valid) {
                            all.push(*bufh);
                        }
                    }
                }
                Err(e) => println!("  cannot read /proc/{pid}/mem: {e}"),
            }
        }
        all.sort_unstable();
        all.dedup();
        if !all.is_empty() {
            println!("\ncandidate buffer handles: {}",
                     all.iter().map(|h| format!("{h:#x}")).collect::<Vec<_>>().join(" "));
        }
        // Deliberately NOT verified here. `create_reader` on a wrong handle
        // oopses the kernel (see UNSAFE_SCAN_WARNING), and this list has a high
        // false-positive rate because a small fd value matches a lot of heap.
        println!(
            "\nNot attempting create_reader on these: the driver turns a handle into a\n\
             kernel pointer without validating it, so a wrong value oopses xcdrv.\n\
             Pass one you have a reason to trust with `probe --buffer <h>`."
        );
        return 0;
    }

    if let Some(handle) = opts.buffer {
        return probe_buffer(&drv, handle, opts);
    }

    if opts.scan {
        if !opts.accept_oops_risk {
            eprintln!("{UNSAFE_SCAN_WARNING}");
            return 2;
        }
        scan_handles(&drv, opts.scan_max);
    } else if !opts.graph {
        println!(
            "\nNothing to do. `--scan` is no longer the default because it oopses the\n\
             kernel (see `mkra probe --scan` for the explanation). Try:\n\
             \n  mkra probe --graph\n  mkra probe --pid <pid of adsb or abs>\n"
        );
    }
    0
}

/// Shown instead of running the brute-force scan.
///
/// The first version of this tool called the scan low risk, on the reasoning
/// that creating and immediately destroying a reader is additive. That was
/// wrong, and the box proved it.
const UNSAFE_SCAN_WARNING: &str = "\
mkra: --scan is NOT safe and is refused without --i-accept-oops-risk.

  Measured on the real unit 2026-07-31: scanning handles 1..64 produced

      Unable to handle kernel paging request at virtual address d3150518
      PC is at create_buffer_access_base+0x58/0x494 [xcdrv]

  from handle 0x46150460. `xc_sbuf_create_read_func` converts the handle into
  a kernel pointer and dereferences it WITHOUT validating it, so any wrong
  value can oops the driver. The unit survived (kernel.panic = 0, so an oops
  kills the faulting thread and the box keeps running) but the A/V pipeline
  reported underflows afterwards, and there is no reason to think every wrong
  handle lands as gently.

  Use `probe --pid <pid>` to read handles out of a process that owns them, or
  work out the handle namespace offline from xcdrv.ko first.";

/// Find buffer handles the way the driver itself would validate them.
///
/// Read-only throughout: `/proc/iomem` for the RAM map, `/dev/mem` for the
/// magic test, `/proc/<pid>/mem` for the cross-reference. No ioctl is issued,
/// so this cannot oops anything.
fn find_buffers(opts: &Options) -> i32 {
    let Some(iomem) = sys::read_text("/proc/iomem") else {
        eprintln!("mkra: cannot read /proc/iomem");
        return 1;
    };
    let ranges = findbuf::system_ram(&iomem);
    if ranges.is_empty() {
        eprintln!("mkra: no System RAM ranges in /proc/iomem");
        return 1;
    }
    for r in &ranges {
        println!(
            "System RAM {:#x}-{:#x} ({} MiB)",
            r.start,
            r.end,
            (r.end - r.start + 1) / (1 << 20)
        );
    }

    println!("scanning for buffer objects (magic {:#x} at +{:#x})",
             findbuf::BUF_MAGIC, findbuf::MAGIC_OFFSET);
    let report = match findbuf::scan_physical("/dev/mem", &ranges) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mkra: cannot read /dev/mem: {e}");
            return 1;
        }
    };
    println!(
        "  scanned {} MiB, {} candidate handle(s){}",
        report.bytes_scanned / (1 << 20),
        report.candidates.len(),
        if report.truncated { " (TRUNCATED)" } else { "" }
    );
    for (addr, why) in report.unreadable.iter().take(5) {
        println!("  unreadable at {addr:#x}: {why}");
    }
    if report.candidates.is_empty() {
        println!("\nNo buffer objects in RAM. Nothing is holding a driver buffer right now.");
        return 1;
    }
    println!("  candidates: {}",
             report.candidates.iter().map(|h| format!("{h:#x}"))
                 .collect::<Vec<_>>().join(" "));

    // Cross-reference: a handle that is both a valid object and referenced by
    // the process owning the pipeline is very unlikely to be a coincidence.
    let mut confirmed: Vec<(u32, Vec<u32>)> = Vec::new();
    for &pid in &opts.pids {
        match findbuf::referenced_by(pid, &report.candidates) {
            Ok(hits) => {
                let comm = sys::read_text(&format!("/proc/{pid}/comm"))
                    .unwrap_or_default().trim().to_string();
                println!("\npid {pid} ({comm}) references {} candidate(s)", hits.len());
                for h in &hits {
                    println!("  {h:#x}");
                    if let Some(e) = confirmed.iter_mut().find(|(c, _)| c == h) {
                        e.1.push(pid);
                    } else {
                        confirmed.push((*h, vec![pid]));
                    }
                }
            }
            Err(e) => println!("\npid {pid}: cannot read memory: {e}"),
        }
    }

    let shortlist: Vec<u32> = if opts.pids.is_empty() {
        report.candidates.clone()
    } else {
        confirmed.iter().map(|(h, _)| *h).collect()
    };

    if shortlist.is_empty() {
        println!(
            "\nNo candidate is referenced by the given process(es). Either the pipeline\n\
             belongs to a different process, or its buffers are not host-connected."
        );
        return 1;
    }

    println!("\nshortlist ({}):", shortlist.len());
    for h in shortlist.iter().take(40) {
        match findbuf::read_object("/dev/mem", *h, 0xc0) {
            Ok(o) => {
                let w = |i: usize| u32::from_le_bytes([o[i], o[i+1], o[i+2], o[i+3]]);
                println!(
                    "  {h:#010x}  +0x00={:#010x} +0x04={:#010x} +0x40={:#010x} +0xb8={:#04x}",
                    w(0), w(4), w(0x40), o[0xb8]
                );
            }
            Err(e) => println!("  {h:#010x}  (cannot read object: {e})"),
        }
    }
    println!(
        "\nThese passed the driver's own validity test, and their memory has already\n\
         been read successfully, so create_reader cannot fault on the deref that\n\
         oopsed before. Try:  mkra probe --buffer <handle>"
    );
    0
}

/// Recover real buffer handles from a running product process, read-only.
///
/// Scanning small integers finds nothing because a handle is not a dense index
/// — the driver rejects 1..64 with its own codes rather than EINVAL, which says
/// the value is being decoded, not merely bounds-checked.
///
/// `xc_buffer_create_host_connection` mallocs a 16-byte object laid out as
/// `{u32 fd; u32 buffer_handle; u32 reader_handle; u8 is_write}`, so a process
/// that owns one has, somewhere in its heap, a word equal to its xcodedrv file
/// descriptor followed by the two handles. That is a specific enough signature
/// to pick out, and reading `/proc/<pid>/mem` touches nothing.
fn find_handles(pid: u32, fds: &[u32]) -> std::io::Result<Vec<(u32, u32, u32, u8)>> {
    use std::io::{Read, Seek, SeekFrom};

    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let mut mem = std::fs::File::open(format!("/proc/{pid}/mem"))?;
    let mut hits = Vec::new();

    for line in maps.lines() {
        // Writable private anonymous memory only: that is where malloc lives,
        // and it keeps us away from device mappings that must not be read.
        let mut it = line.split_whitespace();
        let (Some(range), Some(perms)) = (it.next(), it.next()) else {
            continue;
        };
        if !perms.starts_with("rw") || !perms.contains('p') {
            continue;
        }
        let path = line.split_whitespace().nth(5).unwrap_or("");
        if !path.is_empty() && path != "[heap]" {
            continue;
        }
        let Some((lo, hi)) = range.split_once('-') else { continue };
        let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) else {
            continue;
        };
        let len = (hi - lo) as usize;
        if len == 0 || len > 64 << 20 {
            continue;
        }

        let mut buf = vec![0u8; len];
        if mem.seek(SeekFrom::Start(lo)).is_err() || mem.read_exact(&mut buf).is_err() {
            continue; // regions can vanish or refuse reads; not fatal
        }

        // The object is malloc'd, so it is at least 4-byte aligned.
        let mut off = 0;
        while off + 16 <= buf.len() {
            let w = |i: usize| {
                u32::from_le_bytes([buf[off + i], buf[off + i + 1], buf[off + i + 2], buf[off + i + 3]])
            };
            let (fd, bufh, rdr, flag) = (w(0), w(4), w(8), w(12));
            if fds.contains(&fd) && bufh != 0 && rdr != 0 && flag <= 1 {
                hits.push((fd, bufh, rdr, flag as u8));
            }
            off += 4;
        }
    }
    hits.sort_unstable();
    hits.dedup();
    Ok(hits)
}

/// xcodedrv file descriptors held by `pid`, from `/proc/<pid>/fd`.
fn xcodedrv_fds(pid: u32) -> Vec<u32> {
    let mut fds = Vec::new();
    if let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for e in dir.flatten() {
            if let Ok(target) = std::fs::read_link(e.path()) {
                if target.to_string_lossy().contains("xcodedrv") {
                    if let Some(n) = e.file_name().to_string_lossy().parse().ok() {
                        fds.push(n);
                    }
                }
            }
        }
    }
    fds.sort_unstable();
    fds
}


/// Refuse to attach a reader to a handle we have not proved is a buffer.
///
/// Every path that ends in `create_reader` goes through here. The watcher
/// incident on 2026-07-31 is why this is enforced at the point of use rather
/// than trusted to callers: a loose heuristic upstream fed unvalidated handles
/// straight into the driver, which is the exact mistake that oopsed the kernel
/// earlier the same day.
/// Refuse to hand a handle to `create_reader` unless the driver's own validity
/// test passes on it first.
///
/// `create_buffer_access_base` runs the handle through `p2uv_base` and then
/// dereferences the result *before* checking it, so a wrong handle oopses the
/// kernel (docs/ABI.md §9).  The check the driver eventually applies is
/// `buf_check_handle`: magic `0x5A5A5A5A` at +0x40.  We apply the same test
/// from user space first.
///
/// The read has to go through `xc_map`, not /dev/mem: buffer objects live in
/// the XCode6 window at ARM physical 0x1F000000, which is below `PHYS_OFFSET`
/// and therefore refused by /dev/mem entirely.  That is why the /dev/mem-based
/// gate could never clear a real handle.
fn gate_handle_via_map(drv: &Arc<XcDrv>, handle: u32, accept_risk: bool) -> bool {
    if !(graph::XC_WINDOW_START..graph::XC_WINDOW_END).contains(&handle) {
        eprintln!(
            "mkra: refusing handle {handle:#x}: outside the mapped XCode6 window \
             [{:#x},{:#x}). p2uv_base would translate it to an address the driver \
             has not mapped, which is exactly the case that oopses.",
            graph::XC_WINDOW_START, graph::XC_WINDOW_END
        );
        return accept_risk && {
            eprintln!("mkra: --i-accept-oops-risk given; proceeding anyway.");
            true
        };
    }
    let rd = graph::ObjReader::new(Arc::clone(drv));
    match rd.object(handle) {
        Ok(o) if o.valid() => {
            eprintln!(
                "mkra: handle {handle:#x} verified: magic {:#010x}, ring {:#x} bytes at {:#x}",
                o.magic, o.size, o.data
            );
            true
        }
        Ok(o) => {
            eprintln!(
                "mkra: refusing handle {handle:#x}: no buffer magic at +0x40 \
                 (found {:#010x}, want 0x5a5a5a5a).",
                o.magic
            );
            accept_risk && {
                eprintln!("mkra: --i-accept-oops-risk given; proceeding anyway.");
                true
            }
        }
        Err(e) => {
            eprintln!("mkra: cannot map handle {handle:#x} to check it: {e}");
            accept_risk && {
                eprintln!("mkra: --i-accept-oops-risk given; proceeding anyway.");
                true
            }
        }
    }
}

/// Try to attach a reader to each handle in a range.
///
/// Attaching is additive — each reader port carries its own read position — and
/// every successful port is destroyed immediately, so this does not consume
/// another process's data. It is still the most invasive thing mkra does, which
/// is why it lives behind an explicit subcommand.
fn scan_handles(drv: &Arc<XcDrv>, max: u32) {
    println!("\nscanning buffer handles 1..={max} with create_reader(0x37)");
    let mut found = Vec::new();
    for h in 1..=max {
        match drv.create_reader(h) {
            Ok(reader) => {
                println!("  handle {h:#x}: reader {reader:#x}  <- attachable");
                found.push(h);
                if let Err(e) = drv.destroy_reader(reader) {
                    println!("      warning: destroy_reader failed: {e}");
                }
            }
            Err(e) => {
                // Only report unexpected errors; EINVAL for a free slot is the
                // normal case and would drown the useful lines.
                let raw = e.raw_os_error().unwrap_or(0);
                if raw != 22 && raw != 2 && raw != 9 {
                    println!("  handle {h:#x}: {e}");
                }
            }
        }
    }
    if found.is_empty() {
        println!(
            "\nno attachable buffers.\n\
             If the LED is green and a channel is playing, this means `adsb` only builds the\n\
             remux/host-connection path while it is recording, and the borrow route in\n\
             DESIGN.md §4.4 does not apply — take the P3 (build our own pipeline) branch."
        );
    } else {
        println!(
            "\n{} attachable handle(s): {}\nNext: mkra probe --buffer <handle>",
            found.len(),
            found
                .iter()
                .map(|h| format!("{h:#x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}

/// Read from one buffer and report whether the bytes are plaintext MPEG-TS.
///
/// This is the measurement that settles handoff item A1.
fn probe_buffer(drv: &Arc<XcDrv>, handle: u32, opts: &Options) -> i32 {
    println!("\nprobing buffer {handle:#x} for {}s", opts.seconds);
    if !gate_handle_via_map(drv, handle, opts.accept_oops_risk) {
        return 2;
    }

    match drv.sbuf_status_raw(handle) {
        Ok(v) => {
            print!("  sbuf_info status:");
            for w in v.iter() {
                print!(" {w:#x}");
            }
            println!();
        }
        Err(e) => println!("  sbuf_info status failed: {e}"),
    }

    let mut src = match tssrc::XcReaderSource::attach(Arc::clone(drv), handle, 500) {
        Ok(s) => s,
        Err(e) => {
            println!("  create_reader({handle:#x}) failed: {e}");
            return 1;
        }
    };
    println!("  reader {:#x} attached", src.reader_handle());

    let deadline = Instant::now() + Duration::from_secs(opts.seconds);
    let mut buf = Vec::with_capacity(1 << 20);
    let mut reads = 0u64;
    let mut errors = 0u64;
    while Instant::now() < deadline {
        match src.read_into(&mut buf) {
            Ok(0) => {}
            Ok(_) => reads += 1,
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    println!("  read error: {e}");
                }
                if errors > 20 {
                    break;
                }
            }
        }
        if buf.len() > 8 << 20 {
            break;
        }
    }

    let (hits, misses) = src.map_stats();
    println!(
        "  {} bytes in {reads} reads ({errors} errors), map cache {hits} hit / {misses} miss",
        buf.len()
    );

    if buf.is_empty() {
        println!("  VERDICT: no data. The buffer exists but nothing is being written to it.");
        return 1;
    }

    let ratio = xcdrv::ts_sync_ratio(&buf);
    println!("  188-byte sync ratio: {ratio:.4}");
    print!("  first 32 bytes:");
    for b in buf.iter().take(32) {
        print!(" {b:02x}");
    }
    println!();

    if ratio > 0.99 {
        println!("  VERDICT: plaintext MPEG-TS. Handoff item A1 resolves to `go`.");
        0
    } else if ratio > 0.5 {
        println!("  VERDICT: mostly TS-shaped but lossy — inspect the source framing.");
        1
    } else {
        println!(
            "  VERDICT: not 188-byte TS. Either the data is scrambled, or this buffer\n\
             carries something else (audio/video ES rather than remux output)."
        );
        1
    }
}

// --- dump ----------------------------------------------------------------

fn cmd_dump(opts: &Options) -> i32 {
    let (Some(handle), Some(out)) = (opts.buffer, opts.out.clone()) else {
        eprintln!("mkra: dump needs --buffer and --out");
        return 2;
    };
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };
    if !gate_handle_via_map(&drv, handle, opts.accept_oops_risk) {
        return 2;
    }
    if opts.passive && opts.reader.is_some() {
        eprintln!("mkra: dump accepts only one of --reader and --passive");
        return 2;
    }
    let mut src: Box<dyn TsSource> = if opts.passive {
        match PassiveRingSource::open(Arc::clone(&drv), handle) {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("mkra: passive tap on {handle:#x}: {e}");
                return 1;
            }
        }
    } else if let Some(reader) = opts.reader {
        Box::new(tssrc::XcReaderSource::adopt(
            Arc::clone(&drv),
            handle,
            reader,
            500,
        ))
    } else {
        match tssrc::XcReaderSource::attach(Arc::clone(&drv), handle, 500) {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("mkra: create_reader({handle:#x}): {e}");
                return 1;
            }
        }
    };
    let mut file = match std::fs::File::create(&out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mkra: cannot create {out}: {e}");
            return 1;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(opts.seconds);
    let mut buf = Vec::with_capacity(256 * 1024);
    let mut total = 0u64;
    while Instant::now() < deadline {
        buf.clear();
        match src.read_into(&mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                if let Err(e) = file.write_all(&buf) {
                    eprintln!("mkra: write failed: {e}");
                    return 1;
                }
                total += n as u64;
            }
            Err(e) => {
                eprintln!("mkra: read failed: {e}");
                break;
            }
        }
    }
    eprintln!("mkra: wrote {total} bytes to {out}");
    0
}

/// Build, run, drain, and dismantle the SDK `ts_input_t` TSI-fed TS_RECODER
/// candidate on one driver file descriptor. Keeping the lifecycle here removes
/// the two remaining ambiguities in the Perl bench: cross-fd reader ownership
/// and extra PID commands that `ts_input_t::detect_stream` never issues.
fn cmd_tsrec(opts: &Options) -> i32 {
    let Some(out) = opts.out.clone() else {
        eprintln!("mkra: tsrec needs --out <path>");
        return 2;
    };
    if opts.tsi_port > 7 {
        eprintln!("mkra: --tsi-port must be 0..7");
        return 2;
    }
    // The vendor only instantiates the TS input while the box is operating;
    // keep the same explicit gate used by the read-only probe commands so a
    // standby run cannot be mistaken for a negative TS_RECODER result.
    warn_if_standby();
    let mut file = match std::fs::File::create(&out) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("mkra: cannot create {out}: {e}");
            return 1;
        }
    };
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(drv) => Arc::new(drv),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };

    let (mut device, mut buffer, mut connection, mut reader) = (0u32, 0u32, 0u32, 0u32);
    let mut running = false;
    let capture = (|| -> Result<(u64, Vec<u8>), String> {
        // `ts_input_t::open`/`set_active` configure the physical TSI before
        // `detect_stream` creates TS_RECODER.  Use serial mode (the SDK CLI
        // default) and that class's explicit flags; the product DEMUX differs
        // in its use_sync_signal bit.
        drv.configure_ts_recorder_physical_tsi(opts.tsi_port)
            .map_err(|e| format!("configure physical TSI{}: {e}", opts.tsi_port))?;
        println!("tsrec: physical TSI{} configured for SDK TS_RECODER", opts.tsi_port);
        device = drv.create_ts_recorder().map_err(|e| format!("create TS_RECODER: {e}"))?;
        println!("tsrec: device {device:#x}");
        buffer = drv
            .create_ts_recorder_buffer()
            .map_err(|e| format!("create TS_OUT: {e}"))?;
        println!("tsrec: buffer {buffer:#x}");
        connection = drv
            .connect_output(device, buffer)
            .map_err(|e| format!("connect output: {e}"))?;
        println!("tsrec: output connection {connection:#x}");
        reader = drv.create_reader(buffer).map_err(|e| format!("create reader: {e}"))?;
        println!("tsrec: reader {reader:#x}");
        drv.config_ts_recorder_tsi(device, opts.tsi_port)
            .map_err(|e| format!("configure TSI{}: {e}", opts.tsi_port))?;
        drv.config_ts_recorder(device)
            .map_err(|e| format!("configure TS_RECODER: {e}"))?;
        drv.run_device(device).map_err(|e| format!("run TS_RECODER: {e}"))?;
        running = true;
        drv.set_buffer_ready(buffer).map_err(|e| format!("set TS_OUT ready: {e}"))?;
        let status_before = match drv.sbuf_status_raw(buffer) {
            Ok(status) => {
                eprintln!("tsrec: ring status before capture {status:x?}");
                Some(status)
            }
            Err(e) => {
                eprintln!("mkra: warning: TS_OUT status before capture: {e}");
                None
            }
        };

        // From here the source owns and destroys the pre-TSI reader.  It uses
        // the same Arc<XcDrv>, hence the same fd used for every setup ioctl.
        let adopted = reader;
        reader = 0;
        let mut src = tssrc::XcReaderSource::adopt(
            Arc::clone(&drv),
            buffer,
            adopted,
            500,
        );
        let deadline = Instant::now() + Duration::from_secs(opts.seconds);
        let mut chunk = Vec::with_capacity(256 * 1024);
        let mut sample = Vec::with_capacity(1024 * 1024);
        let mut total = 0u64;
        while Instant::now() < deadline {
            chunk.clear();
            match src.read_into(&mut chunk) {
                Ok(0) => continue,
                Ok(n) => {
                    file.write_all(&chunk).map_err(|e| format!("write {out}: {e}"))?;
                    total += n as u64;
                    if sample.len() < 1024 * 1024 {
                        let take = (1024 * 1024 - sample.len()).min(chunk.len());
                        sample.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(e) => return Err(format!("read TS_OUT: {e}")),
            }
        }
        match drv.sbuf_status_raw(buffer) {
            Ok(status) => {
                eprintln!("tsrec: ring status after capture {status:x?}");
                if total == 0 && status_before.as_ref() == Some(&status) {
                    eprintln!(
                        "tsrec: no input progress: ring status/write pointer unchanged during capture"
                    );
                }
            }
            Err(e) => eprintln!("mkra: warning: TS_OUT status after capture: {e}"),
        }
        drop(src);
        Ok((total, sample))
    })();

    // Always unwind every object we acquired, even when setup or reading
    // failed.  Report all teardown failures instead of hiding later ones.
    if running {
        if let Err(e) = drv.stop_device(device) {
            eprintln!("mkra: warning: stop TS_RECODER {device:#x}: {e}");
        }
    }
    if reader != 0 {
        if let Err(e) = drv.destroy_reader(reader) {
            eprintln!("mkra: warning: destroy reader {reader:#x}: {e}");
        }
    }
    if connection != 0 {
        if let Err(e) = drv.disconnect(device, connection) {
            eprintln!("mkra: warning: disconnect {connection:#x}: {e}");
        }
    }
    if device != 0 {
        if let Err(e) = drv.destroy_device(device) {
            eprintln!("mkra: warning: destroy TS_RECODER {device:#x}: {e}");
        }
    }
    if buffer != 0 {
        if let Err(e) = drv.destroy_buffer(buffer) {
            eprintln!("mkra: warning: destroy TS_OUT {buffer:#x}: {e}");
        }
    }

    match capture {
        Ok((total, sample)) => {
            let ratio = xcdrv::ts_sync_ratio(&sample);
            println!("tsrec: wrote {total} bytes; 188-byte sync ratio {ratio:.4}");
            if total == 0 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("mkra: tsrec failed: {e}");
            1
        }
    }
}

/// Execute the recovered terrestrial takeover, or print it without hardware.
fn cmd_gr(opts: &Options) -> i32 {
    let freq_khz = match (opts.channel, opts.freq_khz) {
        (Some(_), f) if f != 0 => {
            eprintln!("mkra: gr accepts either --channel or --freq, not both");
            return 2;
        }
        (Some(ch), 0) => match gr::uhf_frequency_khz(ch) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("mkra: {e}");
                return 2;
            }
        },
        (None, f) if f != 0 => f,
        (None, 0) => {
            eprintln!("mkra: gr needs --channel <13..62> or --freq <kHz>");
            return 2;
        }
        _ => unreachable!(),
    };
    let nco = match gr::nco_word(opts.if_hz) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mkra: {e}");
            return 2;
        }
    };
    let writes = match gr::tuning_writes(freq_khz, opts.if_hz) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mkra: {e}");
            return 2;
        }
    };

    if !opts.dry_run {
        println!("GR live: RF {freq_khz} kHz, IF {} Hz, NCO {nco:#08x}", opts.if_hz);
        println!(
            "  attach-only I2C0 semaphore {:#010x}; one fixed sequence; no sweep",
            gr::I2C0_SEM_SHM_KEY
        );
        return match gr_i2c::tune(&writes) {
            Ok(result) => {
                let expected = [
                    freq_khz as u8,
                    (freq_khz >> 8) as u8,
                    ((freq_khz >> 16) & 0x0f) as u8,
                ];
                println!(
                    "  RF latch {:02x}/{:02x}/{:02x} expected {:02x}/{:02x}/{:02x}: {}",
                    result.rf_latch[0], result.rf_latch[1], result.rf_latch[2],
                    expected[0], expected[1], expected[2],
                    if result.rf_latch == expected { "MATCH" } else { "MISMATCH" }
                );
                println!(
                    "  ISDB-T lock reg=0x{:02x} polls={}: {}",
                    result.lock_value,
                    result.polls,
                    if result.locked { "LOCKED" } else { "NOT LOCKED" }
                );
                println!("IMPORTANT: reboot before another live test.");
                if result.locked && result.rf_latch == expected { 0 } else { 1 }
            }
            Err(e) => {
                eprintln!("mkra: GR live I2C failed: {e}");
                eprintln!("IMPORTANT: reboot before another live test.");
                1
            }
        };
    }

    println!("GR dry-run: RF {freq_khz} kHz, IF {} Hz, NCO {nco:#08x}", opts.if_hz);
    println!(
        "  attach existing I2C0 shared semaphore key {:#010x}; do not sem_init/recreate",
        gr::I2C0_SEM_SHM_KEY
    );
    println!("  one fixed sequence; no sweep");
    println!("  hold the shared lock for the complete takeover sequence");

    // Seven standby + eleven power-on + both vendor tables + three NCO
    // writes precede the TS-output RMW operations.
    const BEFORE_TSOUT: usize = 7 + 11 + 62 + 168 + 3;
    for (index, write) in writes.iter().enumerate() {
        if index == BEFORE_TSOUT {
            println!("  I2C0 RMW slave7=0x10 reg=0x0b mask=0x01 value=0x01  SC1501A TS output");
            println!("  I2C0 RMW slave7=0x10 reg=0xcc mask=0x80 value=0x80  SC1501A TS output");
        }
        let bytes = write.data.iter().map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>().join(" ");
        println!(
            "  I2C0 W slave7=0x{:02x} reg=0x{:02x} data={}  {}",
            write.slave7, write.reg, bytes, write.label
        );
        if write.slave7 == 0x60 && write.reg == 0x9b {
            println!("  delay 10 ms");
        }
    }
    println!("  I2C0 R slave7=0x60 reg=0x70 len=3  verify RF frequency latch");
    println!("  I2C0 R slave7=0x11 reg=0xa1 len=1, up to 301 times / 5 ms");
    println!("    locked iff (value & 0x90) == 0 && (value & 0x0f) >= 9");
    println!("  release I2C0 shared lock");
    println!("  on lock: use the same TSI0 -> DEMUX -> SYSTEM/TS PID=-1 path as `mkra demux`");
    println!("--dry-run: /dev/i2c-0, xcodedrv, and output files were not opened.");
    0
}

/// Execute the fixed `x_fin_isdbt` rollback sequence.  There are deliberately
/// no parameters: callers cannot turn this into a register-write utility.
fn cmd_gr_off() -> i32 {
    match gr_i2c::standby_isdbt() {
        Ok(()) => {
            println!("GR RF section entered vendor-defined standby");
            0
        }
        Err(e) => {
            eprintln!("mkra: GR standby failed: {e}");
            1
        }
    }
}

const GATE5B9_FW_LOG: &str = "/tmp/mt/fwlog.txt";
const GATE5B9_SF_RING_SIZE: usize = 0x7e000;
const GATE5B9_SF_PHYS_START: u32 = 0x5000_0000;
const GATE5B9_SF_PHYS_END: u32 = 0x5800_0000;
const GATE5B10_PDEV_PHYS_START: u32 = 0x1d00_0000;
const GATE5B10_PDEV_PHYS_END: u32 = 0x2000_0000;
const GATE5B10_MMR_OFFSETS: [u32; 11] = [
    0x6000, 0x6004, 0x6008, 0x600c, 0x6010, 0x6014, 0x61d0, 0x61d4, 0x61f0, 0x6270,
    0x6274,
];

// Superset used by the independent read-only known-good comparison. The last
// two CRC bytes were not needed for Gate 5B10's fixed TSI0 experiment but are
// part of xcode_tsi_status_check's eight-port register set.
const TSI_MMR_OBSERVE_OFFSETS: [u32; 13] = [
    0x6000, 0x6004, 0x6008, 0x600c, 0x6010, 0x6014, 0x61d0, 0x61d4, 0x61f0, 0x6270,
    0x6274, 0x6278, 0x627c,
];

fn parse_latest_sf_ring(text: &str) -> Option<(u32, usize)> {
    fn hex_after(line: &str, marker: &str) -> Option<u64> {
        let tail = line.split_once(marker)?.1;
        let digits = tail
            .char_indices()
            .find_map(|(i, c)| (!c.is_ascii_hexdigit()).then_some(&tail[..i]))
            .unwrap_or(tail);
        (!digits.is_empty())
            .then(|| u64::from_str_radix(digits, 16).ok())
            .flatten()
    }

    text.lines().rev().find_map(|line| {
        if !line.contains("setup_sf_ring:") {
            return None;
        }
        let base = u32::try_from(hex_after(line, "ring_buf_base=0x")?).ok()?;
        let size = usize::try_from(hex_after(line, "size=0x")?).ok()?;
        Some((base, size))
    })
}

fn validate_gate5b9_sf_ring(base: u32, size: usize) -> Result<(), String> {
    let end = base
        .checked_add(u32::try_from(size).map_err(|_| "SF ring size exceeds u32")?)
        .ok_or_else(|| "SF ring physical range overflows u32".to_string())?;
    if base & 0xfff != 0
        || size != GATE5B9_SF_RING_SIZE
        || base < GATE5B9_SF_PHYS_START
        || end > GATE5B9_SF_PHYS_END
    {
        return Err(format!(
            "refuse unexpected firmware SF ring: base={base:#010x} size={size:#x} end={end:#010x}"
        ));
    }
    Ok(())
}

fn wait_for_gate5b9_sf_ring() -> Result<(u32, usize), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(text) = std::fs::read_to_string(GATE5B9_FW_LOG) {
            if let Some((base, size)) = parse_latest_sf_ring(&text) {
                validate_gate5b9_sf_ring(base, size)?;
                return Ok((base, size));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "firmware SF ring was not reported in {GATE5B9_FW_LOG} within 2 s"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn parse_latest_demux_pdev(text: &str) -> Option<u32> {
    text.lines().rev().find_map(|line| {
        if !line.contains("demux_dev_create:") {
            return None;
        }
        let mut tail = line.split_once("pdev=")?.1;
        tail = tail.strip_prefix("0x").unwrap_or(tail);
        let digits = tail
            .char_indices()
            .find_map(|(i, c)| (!c.is_ascii_hexdigit()).then_some(&tail[..i]))
            .unwrap_or(tail);
        u32::from_str_radix(digits, 16).ok()
    })
}

fn wait_for_gate5b10_pdev() -> Result<u32, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(text) = std::fs::read_to_string(GATE5B9_FW_LOG) {
            if let Some(pdev) = parse_latest_demux_pdev(&text) {
                if pdev & 0xff != 0
                    || pdev < GATE5B10_PDEV_PHYS_START
                    || pdev.checked_add(0x7d0).is_none_or(|end| end > GATE5B10_PDEV_PHYS_END)
                {
                    return Err(format!("refuse unexpected firmware DEMUX pdev {pdev:#010x}"));
                }
                return Ok(pdev);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "firmware DEMUX pdev was not reported in {GATE5B9_FW_LOG} within 2 s"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn log_gate5b10_route_snapshot(
    drv: &Arc<XcDrv>,
    stage: &str,
    pdev: Option<u32>,
) -> Result<(), String> {
    let mut mmr = Vec::with_capacity(GATE5B10_MMR_OFFSETS.len());
    for offset in GATE5B10_MMR_OFFSETS {
        let value = drv
            .mmr_read32(offset)
            .map_err(|e| format!("read MMR+{offset:#x} at {stage}: {e}"))?;
        mmr.push(format!("{offset:04x}={value:08x}"));
    }
    eprintln!(
        "demux: TSI_ROUTE_MMR stage={stage} {}",
        mmr.join(" ")
    );
    if let Some(base) = pdev {
        let bytes = graph::ObjReader::new(Arc::clone(drv))
            .read_phys(base + 0x7c0, 16)
            .map_err(|e| format!("read DEMUX pdev route fields at {stage}: {e}"))?;
        let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        eprintln!(
            "demux: TSI_ROUTE_PDEV stage={stage} pdev={base:#010x} route_7c0={:#010x} word_7c4={:#010x} source_7c8={:#010x} flags_7cc={:#010x}",
            word(0), word(4), word(8), word(12)
        );
    }
    Ok(())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest
}

fn log_sf_snapshot(stage: &str, base: u32, bytes: &[u8]) {
    let (ts_ratio, stride, phase) = graph::ts_score_multi(bytes);
    eprintln!(
        "demux: SF_RING_SNAPSHOT stage={stage} base={base:#010x} size={:#x} fnv64={:#018x} distinct={} ts_ratio={ts_ratio:.6} stride={stride} phase={phase}",
        bytes.len(),
        fnv1a64(bytes),
        graph::distinct_bytes(bytes)
    );
}

/// Build the product's normal MPEG-TS DEMUX route and attach one output-all
/// SYSTEM/TS ring. The pipeline/AV_SYNC owner comes from `Dxd_InitSproInfo`,
/// and every input-side value through `run` comes from `Dxd_CreateRoute`.
/// The only added operation is the ARC-validated PID=-1 SYSTEM/TS output port.
fn cmd_demux(opts: &Options) -> i32 {
    let crossbar = opts.command == "demux-crossbar";
    let physical_port = if crossbar {0} else {opts.tsi_port};
    if physical_port > 7 {
        eprintln!("mkra: --tsi-port must be 0..7");
        return 2;
    }
    if opts.pids.len() > 1 || opts.pids.first().is_some_and(|&pid| pid > 0x1fff) {
        eprintln!("mkra: demux accepts at most one --pid in 0..8191");
        return 2;
    }
    if (opts.demux_no_filter && (!opts.pids.is_empty() || opts.demux_pat_section))
        || (opts.demux_pat_section && !opts.pids.is_empty())
    {
        eprintln!("mkra: demux --no-filter, --pid and --pat-section are mutually exclusive");
        return 2;
    }
    if opts.demux_ready_before_filter && !opts.demux_pat_section {
        eprintln!("mkra: --ready-before-filter requires --pat-section");
        return 2;
    }
    if opts.demux_observe_sf_ring && (!opts.demux_pat_section || physical_port != 0) {
        eprintln!("mkra: --observe-sf-ring requires --pat-section and --tsi-port 0");
        return 2;
    }
    if opts.demux_observe_tsi_route
        && (!opts.demux_observe_sf_ring || !opts.demux_pat_section || physical_port != 0)
    {
        eprintln!(
            "mkra: --observe-tsi-route requires --observe-sf-ring, --pat-section and --tsi-port 0"
        );
        return 2;
    }
    let demux_pid = if opts.demux_pat_section {
        0
    } else {
        opts.pids.first().copied().unwrap_or(u32::MAX)
    };
    let pid_label = if demux_pid == u32::MAX {
        "-1 (full TS)".to_string()
    } else {
        demux_pid.to_string()
    };
    if crossbar && opts.seconds > 30 {
        eprintln!("mkra: demux-crossbar is bounded to thirty seconds"); return 2;
    }
    if opts.dry_run {
        if crossbar { println!("EXPERIMENT: physical TSI0 -> config_ex logical TSI1 -> own DEMUX input1; restore both physical configs and identity crossbar after cleanup"); }

        println!("TSI electrical settings: negative_edge={} active_high={}",
                 !opts.tsi_rising_edge, !opts.tsi_valid_low);
        println!("DEMUX dry-run (symbolic handles are returned by the preceding ioctl):");
        println!("  ioctl 0x46         pipeline create: [OUT_PIPELINE]");
        println!("  ioctl 0x28         AV_SYNC create: [7,0,0,16,OUT_AVSYNC,0]");
        println!("  ioctl 0x000c002a   set AV_SYNC pipeline: [AVSYNC,PIPELINE]");
        println!(
            "  ioctl 0x50         physical TSI: [{},{},{},1,{},0,{},0,{},0]",
            physical_port, opts.tsi_packet, u32::from(opts.tsi_parallel), u32::from(opts.tsi_use_sync),
            u32::from(!opts.tsi_valid_low), u32::from(!opts.tsi_rising_edge)
        );
        println!(
            "RESULT gate=5E2-SYNC step=tsi_config ok=1 request=[{},{},{},1,{},0,{},0,{},0]",
            physical_port, opts.tsi_packet, u32::from(opts.tsi_parallel), u32::from(opts.tsi_use_sync),
            u32::from(!opts.tsi_valid_low), u32::from(!opts.tsi_rising_edge)
        );
        if crossbar {println!("  ioctl 0x53         copy configured physical0 fields; logical port=1, asi_port=0 (44B); restore saved port0/1 and identity map at exit");}
        println!("  ioctl 0x0259002a   AV_SYNC live mode: [AVSYNC,1,1,0,0,0,0,0]");
        println!("  ioctl 0x28         DEMUX create: [0x0c,0,32,0,OUT_DEVICE,0]");
        println!("  ioctl 0x000c002a   set pipeline: [DEVICE,PIPELINE]");
        println!(
            "  ioctl 0x01f4002a   direct logical TSI route: [DEVICE,0,{},0]", if crossbar {1} else {0}
        );
        println!("  ioctl 0x0001002a   run: [DEVICE,1,0]");
        if opts.demux_observe_resources {
            println!("  read-only resource snapshots: res_tool -l -A -a -t -s after run/reader/filter/ready/cleanup");
        }
        if opts.demux_observe_sf_ring {
            println!("  read-only Gate 5B9: parse {GATE5B9_FW_LOG} setup_sf_ring and compare its 0x7e000-byte physical ring before/after capture");
        }
        if opts.demux_observe_tsi_route {
            println!("  read-only Gate 5B10: volatile-load MMR TSI/crossbar registers and firmware DEMUX pdev route/source/flags at fixed lifecycle boundaries");
        }
        if opts.demux_wait_tsi_after_run {
            println!("  read-only 0x51    wait for TSI relock + CRC progress (bounded 10 s)");
        }
        if opts.demux_pat_section {
            println!("  ioctl 0x32         PAT section ring: [3,2,14,1,0x1000,0x40000,1,3,OUT_BUFFER]");
        } else {
            println!("  ioctl 0x32         TS ring: [3,1,2,1,0x1000,0x6000,188,3,OUT_BUFFER]");
        }
        println!("  ioctl 0x0008002a   connect output: [DEVICE,0,BUFFER,OUT_PORT]");
        println!("  ioctl 0x37         create reader: [BUFFER,OUT_READER]");
        if opts.demux_ready_before_filter {
            println!("  ioctl 0x00010035   set ready: BUFFER (before filter add)");
        }
        if opts.demux_no_filter {
            println!("  Gate 5A: no PID filter is added");
        } else if opts.demux_pat_section {
            println!("  ioctl 0x01f5002a   product PAT section add: [DEVICE,0,2,PORT,-2,1000,12,2,14]");
        } else {
            println!("  ioctl 0x01f5002a   PID add: [DEVICE,{pid_label},1,PORT,-2,1000,12,1,2]");
        }
        if !opts.demux_ready_before_filter {
            println!("  ioctl 0x00010035   set ready: BUFFER (value argument)");
        }
        println!("  read: 0x3b / commit: 0x3c");
        if opts.demux_no_filter {
            println!("  cleanup: destroy reader -> disconnect -> verify writer/readers=0 -> force-reset/destroy buffer -> stop/destroy DEMUX -> stop pipeline -> destroy AV_SYNC/pipeline");
        } else {
            println!("  cleanup: remove {pid_label} -> destroy reader -> disconnect -> verify writer/readers=0 -> force-reset/destroy buffer -> stop/destroy DEMUX -> stop pipeline -> destroy AV_SYNC/pipeline");
        }
        println!("--dry-run: driver and output file were not opened.");
        return 0;
    }

    if crossbar {
        let accepted = gr_i2c::read_output_regs().is_ok_and(|r| {
            r.lock & 0x20 != 0 && r.bank2_selection[..2] == [0x40,0x10]
            && r.bank0.iter().any(|&(reg,value)| reg == 0xcc && value & 0x80 != 0)
            && r.bank7.iter().any(|&(reg,value)| reg == 0x72 && value == 0)
        });
        if !accepted { eprintln!("mkra: demux-crossbar requires measured locked BS-1/TSID4010 profile; no configuration changed"); return 2; }
    }
    let Some(out) = opts.out.clone() else {
        eprintln!("mkra: demux needs --out <path>");
        return 2;
    };
    let mut file = match std::fs::File::create(&out) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("mkra: cannot create {out}: {e}");
            return 1;
        }
    };
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(drv) => Arc::new(drv),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };

    let saved_mapping = if crossbar {
        match drv.snapshot_tsi_mapping() {
            Ok(v) => {eprintln!("demux: saved TSI rollback image {v:?}"); Some(v)},
            Err(e) => {eprintln!("mkra: TSI snapshot refused: {e}"); return 1;}
        }
    } else {None};
    let (mut pipeline, mut av_sync, mut device, mut buffer, mut connection, mut reader) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut running = false;
    let mut tsi_touched = false;
    let mut device_stopped = false;
    let mut filter_added = false;
    let observe_resources = |stage: &str| -> Result<(), String> {
        if !opts.demux_observe_resources {
            return Ok(());
        }
        eprintln!(
            "demux: RESOURCE_SNAPSHOT_BEGIN t_ms={} stage={stage}",
            sys::unix_time_ms()
        );
        let output = std::process::Command::new("/disk/xcsdk/tools/res_tool")
            .args(["-l", "-A", "-a", "-t", "-s"])
            .output()
            .map_err(|e| format!("start read-only res_tool at {stage}: {e}"))?;
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        eprintln!(
            "demux: RESOURCE_SNAPSHOT_END t_ms={} stage={stage} status={}",
            sys::unix_time_ms(),
            output.status
        );
        if !output.status.success() {
            return Err(format!(
                "read-only res_tool failed at {stage}: {}",
                output.status
            ));
        }
        Ok(())
    };
    let capture = (|| -> Result<(u64, Vec<u8>), String> {
        let mut observed_pdev: Option<u32> = None;
        let log_tsi = |stage: &str| -> Result<(), String> {
            let (lock, crc) = drv
                .tsi_status(physical_port)
                .map_err(|e| format!("read TSI{} status at {stage}: {e}", physical_port))?;
            eprintln!(
                "demux: TSI{} status {stage}: lock={} crc={crc:02x}",
                physical_port,
                u8::from(lock)
            );
            Ok(())
        };
        let log_ring = |stage: &str, buffer: u32| -> Result<(), String> {
            let status = drv
                .sbuf_status_raw(buffer)
                .map_err(|e| format!("read ring status at {stage}: {e}"))?;
            eprintln!("demux: ring status {stage} {status:x?}");
            Ok(())
        };

        log_tsi("before physical config")?;
        if opts.demux_observe_tsi_route {
            log_gate5b10_route_snapshot(&drv, "before_physical_config", None)?;
        }
        pipeline = drv.create_pipeline().map_err(|e| format!("create pipeline: {e}"))?;
        av_sync = drv.create_av_sync().map_err(|e| format!("create AV_SYNC: {e}"))?;
        drv.set_pipeline(av_sync, pipeline)
            .map_err(|e| format!("set AV_SYNC pipeline: {e}"))?;
        tsi_touched = crossbar;
        drv.configure_physical_tsi_polarity(physical_port, opts.tsi_packet, opts.tsi_use_sync, !opts.tsi_rising_edge, !opts.tsi_valid_low, opts.tsi_parallel)
            .map_err(|e| format!("configure product MPEG-TS TSI{}: {e}", physical_port))?;
        if crossbar {drv.map_tsi_one_to_zero().map_err(|e|format!("map logical1 to physical0: {e}"))?;}
        log_tsi("after physical config")?;
        if opts.demux_observe_tsi_route {
            log_gate5b10_route_snapshot(&drv, "after_physical_config", None)?;
        }
        drv.set_av_sync_live_mode(av_sync)
            .map_err(|e| format!("set AV_SYNC live mode: {e}"))?;
        device = drv.create_mpeg_ts_demux().map_err(|e| format!("create DEMUX: {e}"))?;
        drv.set_pipeline(device, pipeline)
            .map_err(|e| format!("set DEMUX pipeline: {e}"))?;
        if opts.demux_observe_tsi_route {
            observed_pdev = Some(wait_for_gate5b10_pdev()?);
            log_gate5b10_route_snapshot(&drv, "before_demux_route", observed_pdev)?;
        }
        // `abs` keeps the TSD route m_port at zero for both live spro/system
        // values.  The physical setup port is a separate layer: BS system=2
        // maps to physical TSI1, while this product route remains port 0.
        if crossbar {drv.config_ts_recorder_tsi(device,1)} else {drv.config_tsi_product_route(device)}
            .map_err(|e| format!("configure DEMUX TSI route: {e}"))?;
        log_tsi("after DEMUX route")?;
        if opts.demux_observe_tsi_route {
            log_gate5b10_route_snapshot(&drv, "after_demux_route", observed_pdev)?;
        }
        // Dxd_CreateRoute runs the DEMUX before Dxd_SetAvPes and section
        // filters create their output ports. Preserve that proven order.
        drv.run_product_demux(device)
            .map_err(|e| format!("run DEMUX: {e}"))?;
        running = true;
        log_tsi("after DEMUX run")?;
        if opts.demux_observe_tsi_route {
            log_gate5b10_route_snapshot(&drv, "after_demux_run", observed_pdev)?;
        }
        observe_resources("after_demux_run")?;

        if opts.demux_wait_tsi_after_run {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut previous_locked_crc = None;
            loop {
                let (lock, crc) = drv
                    .tsi_status(physical_port)
                    .map_err(|e| format!("wait for TSI{} relock: {e}", physical_port))?;
                eprintln!(
                    "demux: TSI{} relock wait: lock={} crc={crc:02x}",
                    physical_port,
                    u8::from(lock)
                );
                if tsi_has_locked_crc_progress(&mut previous_locked_crc, lock, crc) {
                    eprintln!("demux: TSI{} relocked with CRC progress", physical_port);
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "TSI{} did not relock with CRC progress within 10 s",
                        physical_port
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        buffer = if opts.demux_pat_section {
            drv.create_demux_pat_section_buffer()
                .map_err(|e| format!("create DATA/PSI_SECTION ring: {e}"))?
        } else {
            drv.create_demux_ts_buffer()
                .map_err(|e| format!("create SYSTEM/TS ring: {e}"))?
        };
        log_tsi("after buffer create")?;
        connection = drv
            .connect_output(device, buffer)
            .map_err(|e| format!("connect DEMUX output: {e}"))?;
        eprintln!(
            "demux: connect_output bind=0 (0x00000000), returned connection={} (0x{connection:08x})",
            connection
        );
        log_tsi("after output connect")?;
        reader = drv.create_reader(buffer).map_err(|e| format!("create reader: {e}"))?;
        log_tsi("after reader create")?;
        observe_resources("after_ring_connect_reader")?;
        if opts.demux_ready_before_filter {
            drv.set_buffer_ready(buffer)
                .map_err(|e| format!("set PAT ring ready before filter: {e}"))?;
            log_tsi("after buffer ready, before filter")?;
            log_ring("after buffer ready, before filter", buffer)?;
        }
        if !opts.demux_no_filter {
            eprintln!(
                "demux: PID filter request output={} (0x{connection:08x}), pid={} (0x{demux_pid:08x}), type={}, media=2, subtype=14",
                connection,
                demux_pid,
                if opts.demux_pat_section { 2 } else { 1 }
            );
            if opts.demux_pat_section {
                drv.add_demux_pat_section(device, connection)
                    .map_err(|e| format!("add product PAT section filter: {e}"))?;
            } else {
                drv.add_demux_pid(device, demux_pid, connection)
                    .map_err(|e| format!("add PID {pid_label} filter: {e}"))?;
            }
            filter_added = true;
            log_tsi("after filter add")?;
            if opts.demux_observe_tsi_route {
                log_gate5b10_route_snapshot(&drv, "after_filter_add", observed_pdev)?;
            }
            observe_resources("after_pid_add")?;
            if opts.demux_inspect_pid_filter {
                match drv.demux_pid_filter_info(device, demux_pid) {
                    Ok((filter_type, filter_connection)) => {
                        eprintln!(
                            "demux: PID {pid_label} filter info: type={filter_type} connection=0x{filter_connection:08x}; expected type={} connection=0x{connection:08x}",
                            if opts.demux_pat_section { 2 } else { 1 }
                        );
                        let expected_type = if opts.demux_pat_section { 2 } else { 1 };
                        if filter_type != expected_type || filter_connection != connection {
                            return Err(format!(
                                "PID {pid_label} filter info mismatch: type={filter_type}, connection=0x{filter_connection:08x}; expected type={expected_type}, connection=0x{connection:08x}"
                            ));
                        }
                    }
                    Err(e) if is_xce_no_support(&e) => {
                        eprintln!(
                            "mkra: optional PID filter info selector 501 unsupported (XCE_NO_SUPPORT/-1003); continuing capture"
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "mkra: warning: optional PID {pid_label} filter info failed: {e}; continuing capture"
                        );
                    }
                }
            }
        }
        if !opts.demux_ready_before_filter {
            drv.set_buffer_ready(buffer)
                .map_err(|e| format!("set TS ring ready: {e}"))?;
            log_tsi("after buffer ready")?;
            if opts.demux_observe_tsi_route {
                log_gate5b10_route_snapshot(&drv, "after_buffer_ready", observed_pdev)?;
            }
            observe_resources("after_set_ready")?;
        } else {
            log_ring("immediately after filter add", buffer)?;
            for (delay_ms, label) in [
                (20, "+20 ms"),
                (30, "+50 ms"),
                (50, "+100 ms"),
                (150, "+250 ms"),
                (250, "+500 ms"),
                (500, "+1 s"),
                (1000, "+2 s"),
                (2000, "+4 s"),
                (4000, "+8 s"),
            ] {
                std::thread::sleep(Duration::from_millis(delay_ms));
                log_tsi(label)?;
                log_ring(label, buffer)?;
            }
        }
        let status_before = match drv.sbuf_status_raw(buffer) {
            Ok(status) => {
                eprintln!("demux: ring status before capture {status:x?}");
                Some(status)
            }
            Err(e) => {
                eprintln!("mkra: warning: ring status before capture: {e}");
                None
            }
        };
        let sf_before = if opts.demux_observe_sf_ring {
            let (base, size) = wait_for_gate5b9_sf_ring()?;
            let bytes = graph::ObjReader::new(Arc::clone(&drv))
                .read_phys(base, size)
                .map_err(|e| format!("read firmware SF ring before capture: {e}"))?;
            log_sf_snapshot("before_capture", base, &bytes);
            Some((base, bytes))
        } else {
            None
        };
        // Do not call xc_pipeline_start here.  The only product caller is the
        // [VID] presentation helper and its pipeline contains video decoder,
        // deinterlacer and clock-owner devices.  ARC demux_dev_set_run starts
        // the TSD input itself, and a live PID add enables the new filter.
        // The minimal AV_SYNC+DEMUX+host-ring graph was measured to reject
        // pipeline start with XCE_UNKNOWN (-1001).

        let adopted = reader;
        reader = 0;
        let mut src = tssrc::XcReaderSource::adopt(Arc::clone(&drv), buffer, adopted, 500);
        let deadline = Instant::now() + Duration::from_secs(opts.seconds);
        let mut chunk = Vec::with_capacity(256 * 1024);
        let mut sample = Vec::with_capacity(1024 * 1024);
        let mut total = 0u64;
        let mut next_tsi_sample = Instant::now();
        while Instant::now() < deadline {
            if Instant::now() >= next_tsi_sample {
                log_tsi("during capture")?;
                next_tsi_sample = Instant::now() + Duration::from_millis(500);
            }
            chunk.clear();
            match src.read_into(&mut chunk) {
                Ok(0) => continue,
                Ok(n) => {
                    file.write_all(&chunk).map_err(|e| format!("write {out}: {e}"))?;
                    total += n as u64;
                    if sample.len() < 1024 * 1024 {
                        let take = (1024 * 1024 - sample.len()).min(chunk.len());
                        sample.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(e) => return Err(format!("read DEMUX TS ring: {e}")),
            }
        }
        match drv.sbuf_status_raw(buffer) {
            Ok(status) => {
                eprintln!("demux: ring status after capture {status:x?}");
                if total == 0 && status_before.as_ref() == Some(&status) {
                    eprintln!(
                        "demux: no input progress: ring status/write pointer unchanged during capture"
                    );
                }
            }
            Err(e) => eprintln!("mkra: warning: ring status after capture: {e}"),
        }
        log_tsi("after capture")?;
        if opts.demux_observe_tsi_route {
            log_gate5b10_route_snapshot(&drv, "after_capture", observed_pdev)?;
        }
        if let Some((base, before)) = sf_before {
            let after = graph::ObjReader::new(Arc::clone(&drv))
                .read_phys(base, before.len())
                .map_err(|e| format!("read firmware SF ring after capture: {e}"))?;
            log_sf_snapshot("after_capture", base, &after);
            let changed = before
                .iter()
                .zip(after.iter())
                .filter(|(left, right)| left != right)
                .count();
            eprintln!(
                "demux: SF_RING_DIFF base={base:#010x} changed_bytes={changed} total_bytes={}",
                before.len()
            );
        }
        drop(src);
        Ok((total, sample))
    })();

    // Match the product's two-stage teardown. Dxd_DeleteAvPes first removes
    // the PID while the DEMUX is live, destroys its reader, disconnects the
    // output, force-resets and destroys the ring. Dxd_CloseRoute only then
    // stops/destroys the DEMUX and stops the pipeline.
    if filter_added {
        // Product Dxd_DeleteSecFilter passes remove type 1 even when the add
        // used section filter type 2 (`abs:0x2873ac..0x2873bc`).
        let removed = drv.remove_demux_pid(device, demux_pid);
        if let Err(e) = removed {
            eprintln!("mkra: warning: remove PID {pid_label} filter: {e}");
        }
    }
    if reader != 0 {
        if let Err(e) = drv.destroy_reader(reader) {
            eprintln!("mkra: warning: destroy reader {reader:#x}: {e}");
        }
    }
    let mut disconnect_retried = false;
    if connection != 0 {
        match drv.disconnect_product_demux(device, connection) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("mkra: warning: disconnect {connection:#x}: {e}");
                // The product keeps the writer alive until DEMUX stop when
                // disconnect fails.  Stop our own device, retry once, and
                // only then touch the ring; this prevents force-reset from
                // racing a live writer.
                if running {
                    if let Err(stop_err) = drv.stop_device(device) {
                        eprintln!("mkra: warning: stop DEMUX after disconnect failure: {stop_err}");
                    } else {
                        running = false;
                        device_stopped = true;
                    }
                }
                disconnect_retried = true;
                match drv.disconnect_product_demux(device, connection) {
                    Ok(()) => {}
                    Err(retry_err) => eprintln!(
                        "mkra: warning: disconnect retry {connection:#x}: {retry_err}"
                    ),
                }
            }
        }
    }
    // GET_STATUS words 8/9 are the vendor-named m_writer and
    // m_num_readers fields.  A successful disconnect return is not enough:
    // never reset or destroy a ring until both ownership fields read zero.
    let mut ownership_clear = buffer == 0;
    if buffer != 0 {
        match drv.sbuf_status_raw(buffer) {
            Ok(status) if status[8] == 0 && status[9] == 0 => ownership_clear = true,
            Ok(status) => eprintln!(
                "mkra: ring still owned after disconnect: writer={:#x} readers={}",
                status[8], status[9]
            ),
            Err(e) => eprintln!(
                "mkra: cannot prove ring ownership cleared after disconnect: {e}"
            ),
        }
    }
    if buffer != 0 && !ownership_clear && connection != 0 && !disconnect_retried {
        if running {
            if let Err(e) = drv.stop_device(device) {
                eprintln!("mkra: warning: stop DEMUX before ownership retry: {e}");
            } else {
                running = false;
                device_stopped = true;
            }
        }
        match drv.disconnect_product_demux(device, connection) {
            Ok(()) => {}
            Err(e) => eprintln!("mkra: warning: ownership disconnect retry {connection:#x}: {e}"),
        }
        match drv.sbuf_status_raw(buffer) {
            Ok(status) if status[8] == 0 && status[9] == 0 => ownership_clear = true,
            Ok(status) => eprintln!(
                "mkra: ownership remains after stop/retry: writer={:#x} readers={}",
                status[8], status[9]
            ),
            Err(e) => eprintln!(
                "mkra: cannot prove ownership cleared after stop/retry: {e}"
            ),
        }
    }
    if buffer != 0 {
        if ownership_clear {
            if let Err(e) = drv.force_reset_buffer(buffer) {
                eprintln!("mkra: warning: force-reset TS ring {buffer:#x}: {e}");
            }
            if let Err(e) = drv.destroy_buffer(buffer) {
                eprintln!("mkra: warning: destroy TS ring {buffer:#x}: {e}");
            }
        } else {
            eprintln!(
                "mkra: DIRTY_REBOOT_REQUIRED retaining TS ring {buffer:#x}; writer/readers did not clear",
            );
        }
    }
    if running && !device_stopped {
        if let Err(e) = drv.stop_device(device) {
            eprintln!("mkra: warning: stop DEMUX {device:#x}: {e}");
        }
    }
    if device != 0 {
        if ownership_clear || device_stopped {
            if let Err(e) = drv.destroy_device(device) {
                eprintln!("mkra: warning: destroy DEMUX {device:#x}: {e}");
            }
        } else {
            eprintln!(
                "mkra: warning: retaining DEMUX {device:#x}; device may still own the ring",
            );
        }
    }
    // Dxd_CloseRoute issues pipeline_stop before releasing the route even
    // though raw DEMUX activation belongs to mini-device run, not pl_start.
    if pipeline != 0 {
        if let Err(e) = drv.stop_pipeline(pipeline) {
            eprintln!("mkra: warning: stop pipeline {pipeline:#x}: {e}");
        }
    }
    if av_sync != 0 {
        if let Err(e) = drv.destroy_device(av_sync) {
            eprintln!("mkra: warning: destroy AV_SYNC {av_sync:#x}: {e}");
        }
    }
    if pipeline != 0 {
        if let Err(e) = drv.destroy_pipeline(pipeline) {
            eprintln!("mkra: warning: destroy pipeline {pipeline:#x}: {e}");
        }
    }
    if let Err(e) = observe_resources("after_cleanup") {
        eprintln!("mkra: DIRTY_REBOOT_REQUIRED cleanup resource snapshot failed: {e}");
    }

    if let Some(saved) = saved_mapping.as_ref().filter(|_| tsi_touched) {
        if let Err(e) = drv.restore_tsi_mapping(saved) {
            eprintln!("mkra: TSI_ROLLBACK_FAILED: {e}"); return 1;
        }
    }
    match capture {
        Ok((total, sample)) => {
            let ratio = xcdrv::ts_sync_ratio(&sample);
            if opts.demux_pat_section {
                let pats = valid_pat_sections(&sample);
                println!("demux: wrote {total} section bytes; valid PAT sections {pats}");
                return if total > 0 && pats > 0 { 0 } else { 1 };
            }
            println!("demux: wrote {total} bytes; 188-byte sync ratio {ratio:.4}");
            if opts.demux_no_filter {
                println!("demux: Gate 5A input binding lifecycle completed without a PID filter");
                0
            } else if total == 0 || ratio < 0.75 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("mkra: demux failed: {e}");
            1
        }
    }
}

/// BS4K: build a private REMUX on the product's live MMT-derived ES rings.
///
/// The whole sequence is `adsb`'s recording recipe with the recording removed:
/// create REMUX -> config -> TSOUT/TSINFO -> connect both outputs -> connect
/// the live video and audio ES rings as inputs -> declare program and streams
/// -> join the product pipeline -> set_ready -> run -> host-read TSOUT.
/// Teardown is the same list reversed, in the product's order
/// (`remux_remove_stream` before `disconnect`, always).
///
/// Nothing the product owns is created, stopped, reset or destroyed, and no
/// host reader is attached to a product buffer.
fn cmd_remux4k(opts: &Options) -> i32 {
    // 0xFFFF is the shared "unset" default of --tsid, which only the tuning
    // commands treat as a wildcard. Our generated PAT needs a real value.
    let tsid = if opts.tsid == 0xFFFF {
        remux4k::DEFAULT_TSID
    } else {
        opts.tsid
    };
    if opts.dry_run {
        println!("REMUX4K dry-run (symbolic handles are returned by the preceding ioctl):");
        println!("  read-only    live graph: MMT_DEMUX(me_type 0x25) -> VIDEO_ES/AUDIO_ES, pipeline");
        println!("  read-only    ioctl 0x00010036 GET_STATUS on both ES rings (media 3/6, 20MiB/2MiB)");
        println!("  ioctl 0x28         REMUX create: [0x0e,13,2,1,OUT_DEVICE,0]");
        println!("  ioctl 0x044c002a   remux config: [DEVICE,3,0x10,0,1,100,0]");
        println!("  ioctl 0x32         TSOUT create: [4,1,0,1,0x1000,0x468000,1,3,OUT_BUFFER]");
        if !opts.remux4k_no_tsinfo {
            println!("  ioctl 0x32         TSINFO create: [4,2,0x10,1,0x1000,0x20000,1,3,OUT_BUFFER]");
        }
        println!("  ioctl 0x0008002a   connect output TSOUT:  [DEVICE,0,TSOUT,OUT_CONN]");
        if !opts.remux4k_no_tsinfo {
            println!("  ioctl 0x0008002a   connect output TSINFO: [DEVICE,0,TSINFO,OUT_CONN]");
        }
        println!("  ioctl 0x0007002a   connect input video ES: [DEVICE,0,VIDEO_ES,OUT_CONN]");
        println!("  ioctl 0x0007002a   connect input audio ES: [DEVICE,0,AUDIO_ES,OUT_CONN]");
        println!(
            "  ioctl 0x044d002a   add_program: [DEVICE,{},{:#x},{:#x},{},1,0,0,0,0,0]",
            remux4k::PROGRAM_NUMBER,
            remux4k::PCR_PID,
            remux4k::PMT_PID,
            remux4k::PCR_INTERVAL
        );
        println!(
            "  ioctl 0x044f002a   add_video: [DEVICE,CONN_V,{},{:#x},{:#x},{:#x},0]",
            remux4k::PROGRAM_NUMBER,
            remux4k::VIDEO_PID,
            remux4k::VIDEO_STREAM_ID,
            remux4k::VIDEO_STREAM_TYPE
        );
        println!(
            "  ioctl 0x0450002a   add_audio: [DEVICE,CONN_A,{},{:#x},{:#x},{:#x},0]",
            remux4k::PROGRAM_NUMBER,
            remux4k::AUDIO_PID,
            remux4k::AUDIO_STREAM_ID,
            remux4k::AUDIO_STREAM_TYPE
        );
        println!("  ioctl 0x48         pipeline get_time -> ATS base for add_program");
        println!("  ioctl 0x0459002a   set_copy_permission: [DEVICE,0]");
        println!("  ioctl 0x000c002a   set pipeline: [DEVICE,PRODUCT_PIPELINE]");
        println!("  ioctl 0x00010035   set ready: TSOUT");
        println!("  ioctl 0x0001002a   run: [DEVICE,1,2000]");
        println!("  ioctl 0x37/0x3b     host read TSOUT and TSINFO (both must be drained)");
        println!("  watchdog: abort and disconnect if no output for {} ms", opts.remux4k_stall_abort_ms);
        println!("  generated in software: PAT (tsid {tsid:#06x}) + PMT every 100 ms");
        println!(
            "  cleanup: stop -> remove_stream(video,1) -> remove_stream(audio,1) -> \
             remove_program(1) -> disconnect video/audio/TSINFO/TSOUT -> destroy TSINFO/TSOUT -> \
             destroy REMUX"
        );
        println!("--dry-run: driver and output file were not opened.");
        return 0;
    }

    // `--discover-only` is the read-only half of the gate: it opens the
    // driver, walks the graph and issues GET_STATUS, and stops before the
    // first request that would create anything.
    if opts.remux4k_discover_only {
        let drv = match XcDrv::open_path(&opts.dev) {
            Ok(drv) => Arc::new(drv),
            Err(e) => {
                eprintln!("mkra: cannot open {}: {e}", opts.dev);
                return 1;
            }
        };
        return match remux4k::discover(&drv) {
            Ok(live) => {
                println!(
                    "remux4k: MMT_DEMUX={:#x} pipeline={:#x} video_es={:#x} readers={} audio_es={:#x} readers={}",
                    live.mmt_demux,
                    live.pipeline,
                    live.video_es,
                    live.video_readers,
                    live.audio_es,
                    live.audio_readers
                );
                println!("remux4k: discovery only; nothing was created");
                0
            }
            Err(e) => {
                eprintln!("mkra: remux4k: {e}");
                1
            }
        };
    }

    if opts.raw_stream && opts.remux4k_inject_psi {
        eprintln!("mkra: raw TTS output cannot include injected 188-byte PSI");
        return 2;
    }
    let Some(out) = opts.out.clone() else {
        eprintln!("mkra: remux4k needs --out <path> ('-' for stdout)");
        return 2;
    };
    let mut sink: Box<dyn Write> = if out == "-" {
        Box::new(io::stdout())
    } else {
        match std::fs::File::create(&out) {
            Ok(file) => Box::new(file),
            Err(e) => {
                eprintln!("mkra: cannot create {out}: {e}");
                return 1;
            }
        }
    };
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(drv) => Arc::new(drv),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };

    // The identity the emitted TS advertises comes from the product's own
    // filtered MMT-SI, so the PAT/PMT name the service the box is actually
    // showing rather than a placeholder.
    let live_service = si::live_service(si::ADSB_SI_DIR);
    let mut cfg = remux4k::TapConfig {
        stream_id: live_service.map(|s| s.tlv_stream_id).unwrap_or(1),
        allow_no_decoder: opts.remux4k_allow_no_decoder,
        raw_output: opts.raw_stream,
        service_id: live_service
            .map(|s| s.service_id)
            .unwrap_or(remux4k::PROGRAM_NUMBER as u16),
        with_tsinfo: !opts.remux4k_no_tsinfo,
        es_level_abort: opts.remux4k_es_level_abort,
        stall_abort_ms: opts.remux4k_stall_abort_ms,
        ..remux4k::TapConfig::default()
    };
    if let Some(sid) = opts.sid {
        let Some((tsid, _, _)) = channels::tuning_record(sid) else {
            eprintln!("mkra: remux4k --sid must name a known 4K service");
            return 2;
        };
        cfg.service_id = sid;
        cfg.stream_id = tsid;
        eprintln!("remux4k: explicit TS identity service {sid} stream {:#06x}; no retune", cfg.stream_id);
    }
    match live_service {
        Some(s) => eprintln!(
            "remux4k: SI cache service {} on TLV stream {:#06x} (network {:#06x})",
            s.service_id, s.tlv_stream_id, s.original_network_id
        ),
        None => eprintln!(
            "mkra: warning: could not read the tuned service from {}; the emitted PAT will \
             advertise TSID 1 / program {}",
            si::ADSB_SI_DIR,
            remux4k::PROGRAM_NUMBER
        ),
    }

    let mut tap = match remux4k::Tap::open(Arc::clone(&drv), cfg) {
        Ok(tap) => tap,
        Err(e) => {
            eprintln!("mkra: remux4k failed: {e}");
            return 1;
        }
    };

    let mut psi = remux4k::PsiGenerator::new(tsid);
    let deadline = Instant::now() + Duration::from_secs(opts.seconds);
    let mut next_psi = Instant::now();
    let (mut ts, mut sample) = (Vec::new(), Vec::new());
    let mut ts_total = 0u64;
    let mut validated_ts_total = 0u64;
    let mut raw_validator = remux4k::TtsUnwrap::new();
    let mut normalized = Vec::new();
    let mut pids: std::collections::BTreeMap<u16, usize> = std::collections::BTreeMap::new();
    let mut failure: Option<String> = None;
    while Instant::now() < deadline {
        ts.clear();
        match tap.read_into(&mut ts) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
        // The REMUX builds its own PAT/PMT from the program we declared, so a
        // second locally generated pair is opt-in only.
        if opts.remux4k_inject_psi && Instant::now() >= next_psi {
            let mut psi_packets = Vec::new();
            psi.emit(&mut psi_packets);
            if let Err(e) = sink.write_all(&psi_packets) {
                failure = Some(format!("write {out}: {e}"));
                break;
            }
            ts_total += psi_packets.len() as u64;
            next_psi = Instant::now() + Duration::from_millis(100);
        }
        if let Err(e) = sink.write_all(&ts) {
            failure = Some(format!("write {out}: {e}"));
            break;
        }
        ts_total += ts.len() as u64;
        let measured = if opts.raw_stream {
            normalized.clear();
            raw_validator.push(&ts, &mut normalized);
            &normalized
        } else { &ts };
        validated_ts_total += measured.len() as u64;
        if sample.len() < 1024 * 1024 {
            let take = (1024 * 1024 - sample.len()).min(measured.len());
            sample.extend_from_slice(&measured[..take]);
        }
        for (pid, n) in remux4k::pid_histogram(measured) {
            *pids.entry(pid).or_insert(0usize) += n;
        }
    }
    let raw_total = tap.raw_total;
    drop(tap);

    if let Some(e) = failure {
        eprintln!("mkra: remux4k stopped early: {e}");
    }
    let ratio = xcdrv::ts_sync_ratio(&sample);
    let mut hist: Vec<(u16, usize)> = pids.into_iter().collect();
    hist.sort_by(|a, b| b.1.cmp(&a.1));
    let top: Vec<String> = hist
        .iter()
        .take(8)
        .map(|(pid, n)| format!("{pid:#06x}:{n}"))
        .collect();
    if opts.raw_stream {
        println!("remux4k: TSOUT {raw_total} raw bytes -> {ts_total} TTS file bytes; validated {validated_ts_total} TS bytes; 188-byte sync ratio {ratio:.4}");
    } else {
        println!("remux4k: TSOUT {raw_total} raw bytes -> {ts_total} TS bytes; 188-byte sync ratio {ratio:.4}");
    }
    println!("remux4k: PIDs {}", top.join(" "));
    let has_video = hist
        .iter()
        .any(|(pid, n)| u32::from(*pid) == remux4k::VIDEO_PID && *n > 0);
    if ts_total > 0 && ratio > 0.99 && has_video {
        0
    } else {
        1
    }
}

/// Ask the product to change channel and wait for its own SI to agree.
///
/// The product republishes `AdsbSiMhSdtActual.dat` once the new service runs,
/// which is the same evidence `si::live_service` uses everywhere else, so the
/// switch is confirmed by the box rather than assumed from a send that cannot
/// be acknowledged. Measured: the new service appears within about 4 s.
fn tune_and_wait(service_id: u16, oid: &Arc<AtomicU32>) -> io::Result<si::LiveService> {
    tune_and_wait_mode(service_id, oid, tuner::TUNE_CHANNEL_CHANGE)
}

/// Ask the box to stop decoding. EXPERIMENTAL — see the caveat below.
///
/// The change-channel message with `NotDecode = 1` did tear the decode chain
/// down (driver graph 22 devices -> 11, the same shape the product's own
/// standby leaves) when the product's power state was standby and only our own
/// resume had started decode. Repeated from a normally powered-on state the
/// same message is accepted and ignored, so this is not yet a dependable
/// sleep: the product's real power state lives in its UIM/INI state machine,
/// which is not yet decoded. Waking, by contrast, is reliable (§`TUNE_RESUME`).
fn sleep_decode(service_id: u16, oid: &Arc<AtomicU32>) -> io::Result<()> {
    let Some((tsid, freq_khz, bid)) = channels::tuning_record(service_id) else {
        return Err(io::Error::other(format!(
            "service {service_id} is not in the advanced-BS table"
        )));
    };
    let req = tuner::ChangeChannelRequest {
        oid: oid.fetch_add(1, Ordering::Relaxed),
        tune: tuner::TUNE_CHANNEL_CHANGE,
        not_decode: 1,
        service_id,
        freq_khz,
        tlv_stream_id: tsid,
        bid,
    };
    let t = tuner::Tuner::open_qid(tuner::ADSB_COMM_QID, tuner::MSGH_LIST)?;
    eprintln!("mkra: idle — asking the box to stop decoding (service {service_id})");
    t.change_channel(&req)
}

/// The native presentation cache can retain a previous MH-SDT indefinitely
/// after BS2K stops HDMI decoding. Confirm native acquired package and live ES,
/// rather than waiting for the HDMI decoder or assigning the requested ID.
fn unified_tune4k(drv:&Arc<XcDrv>,asked:u16,oid:&Arc<AtomicU32>,force:bool)->io::Result<si::LiveService> {
    let send=|mode|->io::Result<()> {
        let (tsid,freq,bid)=channels::tuning_record(asked).ok_or_else(||io::Error::other("unknown 4K service"))?;
        tuner::Tuner::open_qid(tuner::ADSB_COMM_QID,tuner::MSGH_LIST)?.change_channel(&tuner::ChangeChannelRequest {
            oid:oid.fetch_add(1,Ordering::Relaxed),tune:mode,not_decode:0,service_id:asked,freq_khz:freq,tlv_stream_id:tsid,bid,
        })
    };
    let selection=native_selection::Reader::open()?;
    if force || selection.selected()?.map(|s|s.live.service_id)!=Some(asked) || remux4k::discover(drv).is_err() {
        send(tuner::TUNE_CHANNEL_CHANGE)?;
        std::thread::sleep(Duration::from_millis(1000));
    }
    let begin=Instant::now();let mut resumed=false;let mut last=String::from("native request/acquired package not settled on requested service");
    while begin.elapsed()<Duration::from_secs(11) {
        if let Some(selected)=selection.selected()? {
            if selected.live.service_id==asked {
                match remux4k::verify_fresh_es(drv) {
                    Ok(())=>{
                        // Both acquired package and requested channel must still agree
                        // after observing fresh writes on the same ES pair.
                        if selection.selected()?==Some(selected) {
                            eprintln!("mkra: unified 4K confirmed native acquired package service {} TLV {:#06x} + fresh stable video/audio ES",selected.live.service_id,selected.live.tlv_stream_id);
                            return Ok(selected.live);
                        }
                    }
                    Err(e)=>last=e.to_string(),
                }
            }
        }
        if !resumed && begin.elapsed()>Duration::from_secs(5) && remux4k::discover(drv).is_err() {
            send(tuner::TUNE_RESUME)?;resumed=true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(io::Error::other(format!("4K selection not confirmed for {asked}: {last}")))
}

fn tune_and_wait_mode(
    service_id: u16,
    oid: &Arc<AtomicU32>,
    tune: u32,
) -> io::Result<si::LiveService> {
    let Some((tsid, freq_khz, bid)) = channels::tuning_record(service_id) else {
        return Err(io::Error::other(format!(
            "service {service_id} is not in the advanced-BS table"
        )));
    };
    let req = tuner::ChangeChannelRequest {
        oid: oid.fetch_add(1, Ordering::Relaxed),
        tune,
        not_decode: 0,
        service_id,
        freq_khz,
        tlv_stream_id: tsid,
        bid,
    };
    let t = tuner::Tuner::open_qid(tuner::ADSB_COMM_QID, tuner::MSGH_LIST)?;
    eprintln!(
        "mkra: tuning to service {service_id} (TSID {tsid:#06x}, RF {freq_khz} kHz, key {bid:#x}, tune {tune})"
    );
    t.change_channel(&req)?;

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(live) = si::live_service(si::ADSB_SI_DIR) {
            if live.service_id == service_id {
                eprintln!(
                    "mkra: box is now on service {} (TLV stream {:#06x})",
                    live.service_id, live.tlv_stream_id
                );
                // The SI announces the new service before the graph is ready,
                // so wait for the product's own decoders to reattach.
                wait_for_decode_chain()?;
                return Ok(live);
            }
        }
    }
    Err(io::Error::other(format!(
        "asked the box for service {service_id} but its SI never confirmed the switch"
    )))
}

/// Park the tuner on a transponder that carries no signal here.
///
/// The decoders then have nothing to decode: measured, the product keeps its
/// devices allocated but the video ES ring only drains (12 -> 11 units in 8 s)
/// and no new data arrives, so the 4K decode block idles while the box stays
/// powered on. It is undone by tuning at any real service, which takes about
/// 4 s — the same cost as a channel change.
///
/// This is the dependable idle mode. The product's own standby is lighter
/// still (it powers the tuner down too) but can only be entered from the
/// remote, and `NotDecode = 1` is ignored unless the box is already in standby.
fn park_tuner(oid: &Arc<AtomicU32>, freq_khz: u32, tsid: u16) -> io::Result<()> {
    let req = tuner::ChangeChannelRequest {
        oid: oid.fetch_add(1, Ordering::Relaxed),
        tune: tuner::TUNE_CHANNEL_CHANGE,
        not_decode: 0,
        // Any service id is fine: nothing will be found at this frequency.
        service_id: 141,
        freq_khz,
        tlv_stream_id: tsid,
        bid: 1,
    };
    let t = tuner::Tuner::open_qid(tuner::ADSB_COMM_QID, tuner::MSGH_LIST)?;
    eprintln!("mkra: idle — parking the tuner at {freq_khz} kHz / TSID {tsid:#06x}");
    t.change_channel(&req)
}

/// Read the XCode6 chip temperature (ioctl 0x64, read-only).
fn cmd_temp(opts: &Options) -> i32 {
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };
    let n = opts.watch.max(1);
    for i in 0..n {
        match drv.temperature_raw() {
            Ok(w) => println!(
                "temp: raw {w:?}  (first word as °C: {})  t={}",
                w[0],
                sys::unix_time_ms()
            ),
            Err(e) => {
                eprintln!("mkra: temperature read failed: {e}");
                return 1;
            }
        }
        if i + 1 < n {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    0
}

/// Wait until the product has finished rebuilding its own decode chain.
///
/// The SI announces the new service before the graph is ready: measured, a tap
/// built at that moment found the ES rings but with the audio ring still at
/// zero readers, and then every ioctl against them returned `XCE_TIMEOUT`
/// (-1038), leaving a half-built REMUX behind. Both product decoders being
/// attached is the signal that the chain is live.
fn wait_for_decode_chain() -> io::Result<()> {
    let drv = match XcDrv::open_path(xcdrv::DEV_PATH) {
        Ok(d) => Arc::new(d),
        // Without the driver we cannot check; fall back to a fixed settle.
        Err(_) => {
            std::thread::sleep(Duration::from_millis(1500));
            return Ok(());
        }
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(live) = remux4k::discover(&drv) else {
            continue;
        };
        if live.video_readers >= 1 && live.audio_readers >= 1 {
            // Both decoders are attached. Give the firmware a moment to start
            // filling the rings before adding a second consumer.
            std::thread::sleep(Duration::from_millis(500));
            return Ok(());
        }
    }
    Err(io::Error::other(
        "the product did not finish rebuilding its decode chain within 10 s",
    ))
}

/// Ask the product to change channel, the way its own remote-control path does.
///
/// Everything here is the product's: the queue (0x36F), the message id
/// (0x6004), the ten-word body, and the per-service frequency/TSID/remote-key
/// values. The only thing mkra chooses is the operation id, which the receiver
/// merely echoes.
fn cmd_tune4k(opts: &Options) -> i32 {
    let Some(sid) = opts.sid else {
        eprintln!("mkra: tune4k needs --sid <service id>");
        return 2;
    };
    // `--freq`/`--tsid` override the table so a deliberately unreachable
    // transponder can be selected. Tuning at one that carries no signal here
    // is the cheapest way to make the product stop decoding while staying
    // powered on, and it is undone by tuning back to a real service.
    let record = channels::tuning_record(sid);
    let (tsid, freq_khz, bid) = match (record, opts.freq_khz, opts.tsid) {
        (_, f, ts) if f != 0 && ts != 0xFFFF => (ts, f, record.map(|r| r.2).unwrap_or(1)),
        (Some(r), _, _) => r,
        (None, _, _) => {
            eprintln!(
                "mkra: service {sid} is not in the advanced-BS table; pass --freq and --tsid to \
                 tune anyway"
            );
            return 2;
        }
    };
    let req = tuner::ChangeChannelRequest {
        oid: opts.oid,
        tune: opts.tune_mode,
        not_decode: u32::from(opts.no_decode),
        service_id: sid,
        freq_khz,
        tlv_stream_id: tsid,
        bid,
    };
    if let Err(e) = req.validate() {
        eprintln!("mkra: {e}");
        return 2;
    }
    let body = req.body();
    println!(
        "tune4k: service {sid} -> TSID {tsid:#06x}, RF {freq_khz} kHz, remote key {bid:#x}"
    );
    println!(
        "  qid {:#x}  msgId {:#x}  len {:#x}",
        tuner::ADSB_COMM_QID,
        tuner::MSG_ADSB_CHANGE_CHANNEL,
        0x28
    );
    println!("  body {body:?}");
    if opts.dry_run {
        println!("--dry-run: no message was sent.");
        return 0;
    }

    // A tap holding the live ES rings would make the product's own teardown
    // fail with XCE_IN_USE, so refuse to fire while one is attached.
    if let Ok(drv) = XcDrv::open_path(&opts.dev) {
        let drv = Arc::new(drv);
        if let Ok(live) = remux4k::discover(&drv) {
            if let Ok(st) = drv.sbuf_status_raw(live.video_es) {
                if st[9] > 1 {
                    eprintln!(
                        "mkra: the live video ES has {} readers — release the tap before tuning",
                        st[9]
                    );
                    return 1;
                }
            }
        }
    }

    let tuner = match tuner::Tuner::open_qid(tuner::ADSB_COMM_QID, tuner::MSGH_LIST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: cannot open ADSB comm queue: {e}");
            return 1;
        }
    };
    match tuner.change_channel(&req) {
        Ok(()) => {
            println!("tune4k: request sent");
            0
        }
        Err(e) => {
            eprintln!("mkra: tune4k failed: {e}");
            1
        }
    }
}

/// Require activity across two locked samples.  A CRC change observed only
/// between an unlocked sample and the first locked sample is not sufficient:
/// Gate 5B5 must prove that the recovered input remains active after relock.
fn tsi_has_locked_crc_progress(previous_locked_crc: &mut Option<u8>, lock: bool, crc: u8) -> bool {
    if !lock {
        *previous_locked_crc = None;
        return false;
    }
    let progressed = previous_locked_crc.is_some_and(|previous| previous != crc);
    *previous_locked_crc = Some(crc);
    progressed
}

/// Count complete MPEG-2 PAT sections in a flattened diagnostic sample.
/// Product section rings contain section bytes rather than 188-byte packets;
/// scanning also tolerates a small descriptor prefix before a section.
fn valid_pat_sections(data: &[u8]) -> usize {
    let mut count = 0;
    let mut off = 0;
    while off + 12 <= data.len() {
        if data[off] == 0x00 && data[off + 1] & 0x80 != 0 {
            let section_len = (((data[off + 1] & 0x0f) as usize) << 8) | data[off + 2] as usize;
            let end = off + 3 + section_len;
            if (9..=1021).contains(&section_len)
                && end <= data.len()
                && si::crc32_mpeg(&data[off..end]) == 0
            {
                count += 1;
                off = end;
                continue;
            }
        }
        off += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_accept_decimal_and_hex() {
        assert_eq!(parse_num("40772", "--port").unwrap(), 40772);
        assert_eq!(parse_num("0x1f", "--buffer").unwrap(), 31);
        assert_eq!(parse_num("0X1F", "--buffer").unwrap(), 31);
        assert!(parse_num("zz", "--port").is_err());
    }

    #[test]
    fn options_take_their_defaults() {
        let o = Options::parse(&["serve".into()]).unwrap();
        assert_eq!(o.command, "serve");
        assert_eq!(o.port, 40772);
        assert_eq!(o.max_clients, 4);
        assert_eq!(o.ring, 2 * 1024 * 1024);
        // Tuning must never be on unless asked for.
        assert!(!o.tune);
        assert!(o.buffer.is_none());
    }

    #[test]
    fn demux_dry_run_options_are_parsed_without_enabling_tuning() {
        let o = Options::parse(&[
            "demux".into(),
            "--out".into(),
            "/tmp/mt/bs2k.ts".into(),
            "--seconds".into(),
            "8".into(),
            "--tsi-port".into(),
            "0".into(),
            "--dry-run".into(),
        ])
        .unwrap();
        assert_eq!(o.command, "demux");
        assert_eq!(o.out.as_deref(), Some("/tmp/mt/bs2k.ts"));
        assert_eq!(o.seconds, 8);
        assert_eq!(o.tsi_port, 0);
        assert!(o.dry_run);
        assert!(!o.tune);
        assert!(!o.demux_no_filter);
        assert!(!o.demux_pat_section);
    }

    #[test]
    fn demux_gate5a_no_filter_is_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--no-filter".into(),
            "--out".into(),
            "/tmp/mt/gate5a.ts".into(),
        ])
        .unwrap();
        assert!(o.demux_no_filter);
        assert!(o.pids.is_empty());
        assert_eq!(o.tsi_port, 1);
    }

    #[test]
    fn demux_gate5b_product_pat_section_is_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--out".into(),
            "/tmp/mt/gate5b_pat.section".into(),
        ])
        .unwrap();
        assert!(o.demux_pat_section);
        assert!(o.pids.is_empty());
        assert_eq!(o.tsi_port, 1);
        assert!(!o.demux_wait_tsi_after_run);
        assert!(!o.demux_inspect_pid_filter);
        assert!(!o.demux_observe_resources);
        assert!(!o.demux_observe_sf_ring);
        assert!(!o.demux_observe_tsi_route);
        assert!(!o.demux_ready_before_filter);
    }

    #[test]
    fn demux_gate5b8_resource_observation_is_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--observe-resources".into(),
            "--tsi-port".into(),
            "0".into(),
            "--out".into(),
            "/tmp/mt/gate5b8.pat".into(),
        ])
        .unwrap();
        assert!(o.demux_pat_section);
        assert!(o.demux_observe_resources);
        assert!(!o.demux_ready_before_filter);
        assert_eq!(o.tsi_port, 0);
    }

    #[test]
    fn demux_gate5b9_sf_ring_observation_is_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--observe-resources".into(),
            "--observe-sf-ring".into(),
            "--tsi-port".into(),
            "0".into(),
            "--out".into(),
            "/tmp/mt/gate5b9.pat".into(),
        ])
        .unwrap();
        assert!(o.demux_pat_section);
        assert!(o.demux_observe_resources);
        assert!(o.demux_observe_sf_ring);
        assert!(!o.demux_ready_before_filter);
        assert_eq!(o.tsi_port, 0);
    }

    #[test]
    fn gate5b9_parses_latest_firmware_sf_ring() {
        let log = "\
T1 setup_sf_ring:0x100: ring_buf_base=0x51001000, size=0x7e000\n\
T2 unrelated\n\
T3 setup_sf_ring:0x200: ring_buf_base=0x51f41000, size=0x7e000\n";
        assert_eq!(
            parse_latest_sf_ring(log),
            Some((0x51f4_1000, GATE5B9_SF_RING_SIZE))
        );
        assert!(validate_gate5b9_sf_ring(0x51f4_1000, GATE5B9_SF_RING_SIZE).is_ok());
        assert!(validate_gate5b9_sf_ring(0x51f4_1001, GATE5B9_SF_RING_SIZE).is_err());
        assert!(validate_gate5b9_sf_ring(0x51f4_1000, 0x1000).is_err());
    }

    #[test]
    fn gate5b9_fnv_and_diff_inputs_are_deterministic() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        let before = [0u8, 1, 2, 3];
        let after = [0u8, 9, 2, 8];
        assert_eq!(
            before
                .iter()
                .zip(after.iter())
                .filter(|(left, right)| left != right)
                .count(),
            2
        );
    }

    #[test]
    fn gate5b10_route_observation_and_pdev_parse_are_fixed() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--observe-sf-ring".into(),
            "--observe-tsi-route".into(),
            "--tsi-port".into(),
            "0".into(),
            "--out".into(),
            "/tmp/mt/gate5b10.pat".into(),
        ])
        .unwrap();
        assert!(o.demux_observe_tsi_route);
        assert_eq!(
            parse_latest_demux_pdev(
                "T1 demux_dev_create: pdev=1e001100, rc=0\nT2 demux_dev_create: pdev=0x1e00b100, rc=0\n"
            ),
            Some(0x1e00_b100)
        );
    }

    #[test]
    fn tsi_mmr_observation_register_set_is_fixed_and_aligned() {
        assert_eq!(TSI_MMR_OBSERVE_OFFSETS.len(), 13);
        assert_eq!(TSI_MMR_OBSERVE_OFFSETS[0], 0x6000);
        assert_eq!(TSI_MMR_OBSERVE_OFFSETS[8], 0x61f0);
        assert_eq!(TSI_MMR_OBSERVE_OFFSETS[12], 0x627c);
        assert!(TSI_MMR_OBSERVE_OFFSETS.iter().all(|offset| offset & 3 == 0));
    }

    #[test]
    fn demux_gate5b5_waits_for_relock_only_when_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--wait-tsi-after-run".into(),
            "--tsi-port".into(),
            "0".into(),
            "--out".into(),
            "/tmp/mt/gate5b5.pat".into(),
        ])
        .unwrap();
        assert!(o.demux_pat_section);
        assert!(o.demux_wait_tsi_after_run);
        assert_eq!(o.tsi_port, 0);
        assert!(!o.demux_inspect_pid_filter);
    }

    #[test]
    fn demux_gate5b7_ready_before_filter_is_explicit() {
        let o = Options::parse(&[
            "demux".into(),
            "--pat-section".into(),
            "--inspect-pid-filter".into(),
            "--ready-before-filter".into(),
            "--tsi-port".into(),
            "0".into(),
            "--out".into(),
            "/tmp/mt/gate5b7.pat".into(),
        ])
        .unwrap();
        assert!(o.demux_pat_section);
        assert!(o.demux_inspect_pid_filter);
        assert!(o.demux_ready_before_filter);
        assert!(!o.demux_wait_tsi_after_run);
        assert_eq!(o.tsi_port, 0);
    }

    #[test]
    fn gate5b5_requires_crc_progress_across_two_locked_samples() {
        let mut previous = None;
        assert!(!tsi_has_locked_crc_progress(&mut previous, false, 0xe0));
        assert!(!tsi_has_locked_crc_progress(&mut previous, true, 0x10));
        assert!(!tsi_has_locked_crc_progress(&mut previous, true, 0x10));
        assert!(tsi_has_locked_crc_progress(&mut previous, true, 0x11));
    }

    #[test]
    fn gate5b5_unlock_resets_the_crc_progress_window() {
        let mut previous = None;
        assert!(!tsi_has_locked_crc_progress(&mut previous, true, 0x20));
        assert!(!tsi_has_locked_crc_progress(&mut previous, false, 0x21));
        assert!(!tsi_has_locked_crc_progress(&mut previous, true, 0x22));
        assert!(tsi_has_locked_crc_progress(&mut previous, true, 0x23));
    }

    #[test]
    fn demux_defaults_to_bs_physical_tsi1() {
        let o = Options::parse(&["demux".into()]).unwrap();
        assert_eq!(o.tsi_port, 1);
        let gr = Options::parse(&[
            "demux".into(),
            "--tsi-port".into(),
            "0".into(),
        ])
        .unwrap();
        assert_eq!(gr.tsi_port, 0);
    }

    #[test]
    fn xce_no_support_is_the_only_named_optional_info_result() {
        assert!(is_xce_no_support(&io::Error::from_raw_os_error(1003)));
        assert!(!is_xce_no_support(&io::Error::from_raw_os_error(1023)));
    }

    #[test]
    fn gr_dry_run_parses_channel_and_exact_default_if() {
        let o = Options::parse(&[
            "gr".into(),
            "--channel".into(),
            "13".into(),
            "--dry-run".into(),
        ])
        .unwrap();
        assert_eq!(o.command, "gr");
        assert_eq!(o.channel, Some(13));
        assert_eq!(o.if_hz, 3_550_000);
        assert!(o.dry_run);
        assert!(!o.tune);
    }

    #[test]
    fn gr_numeric_fields_do_not_silently_truncate() {
        assert!(Options::parse(&[
            "gr".into(), "--channel".into(), "300".into(), "--dry-run".into()
        ]).is_err());
        assert!(Options::parse(&[
            "gr".into(), "--if-hz".into(), "4294967296".into(), "--dry-run".into()
        ]).is_err());
    }

    #[test]
    fn options_parse_flags_and_values() {
        let args: Vec<String> = "serve --port 8080 --buffer 0x12 --tune --max-clients 2"
            .split(' ')
            .map(String::from)
            .collect();
        let o = Options::parse(&args).unwrap();
        assert_eq!(o.port, 8080);
        assert_eq!(o.buffer, Some(0x12));
        assert!(o.tune);
        assert_eq!(o.max_clients, 2);
    }

    #[test]
    fn bad_options_are_rejected_rather_than_ignored() {
        assert!(Options::parse(&["serve".into(), "--nope".into()]).is_err());
        assert!(Options::parse(&["serve".into(), "--port".into()]).is_err());
        assert!(Options::parse(&["probe".into(), "--commit".into(), "first".into()]).is_err());
    }
}

// --- graph / peek / tune -------------------------------------------------

/// Observe the live product's TSI receiver without changing any device state.
///
/// `mmr_read32` only obtains the open-time process mapping and volatile-loads
/// an aligned word. `tsi_status` is the vendor's read-only status ioctl. Keep
/// this command independent of graph creation and every configuration helper
/// so it can be used as the known-good green-state control for Gate 5B11.
fn cmd_tsi_mmr(opts: &Options) -> i32 {
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };

    println!("XCode6 TSI MMR snapshot (read-only):");
    for offset in TSI_MMR_OBSERVE_OFFSETS {
        match drv.mmr_read32(offset) {
            Ok(value) => println!("  MMR[{offset:#06x}]={value:#010x}"),
            Err(e) => {
                eprintln!("mkra: MMR read at {offset:#06x} failed: {e}");
                return 1;
            }
        }
    }

    println!("XCode6 TSI status ioctl 0x51 (read-only):");
    for port in 0..=1 {
        match drv.tsi_status(port) {
            Ok((locked, crc)) => println!(
                "  TSI{port}: lock={} crc=0x{crc:02x}",
                if locked { 1 } else { 0 }
            ),
            Err(e) => {
                eprintln!("mkra: TSI{port} status read failed: {e}");
                return 1;
            }
        }
    }
    println!("No configuration or graph write was issued.");
    0
}

/// Print the driver's live graph and decode every buffer object it names.
///
/// This is the safe replacement for `probe --scan`: handles come from the
/// driver's own tables, so no invented value ever reaches an ioctl.
fn cmd_graph(opts: &Options) -> i32 {
    let g = match graph::Graph::read() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("mkra: cannot read the driver graph: {e}");
            eprintln!("      needs /dev/kmem, kptr_restrict=0 and gps_XCode in kallsyms");
            return 1;
        }
    };
    println!("gps_XCode = {:#010x}", g.ctx);
    for name in ["device", "buffer", "pipeline", "port"] {
        let Some((count, ptr, rows)) = g.tables.get(name) else {
            continue;
        };
        println!("\n== {name}s  count={count}  table={ptr:#010x} ==");
        for r in rows {
            let ws: Vec<String> = r.words.iter().map(|w| format!("{w:08x}")).collect();
            println!("  [{:3}] {}", r.index, ws.join(" "));
        }
    }

    let handles = g.buffer_handles();
    println!("\n== {} buffer handle(s) ==", handles.len());

    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };
    let rd = graph::ObjReader::new(Arc::clone(&drv));

    let rounds = opts.watch.max(1);
    let mut first: BTreeMap<u32, [u32; 64]> = BTreeMap::new();
    let mut objs: BTreeMap<u32, graph::BufObj> = BTreeMap::new();

    for round in 0..rounds {
        if round > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        for &h in &handles {
            match rd.object(h) {
                Ok(o) if o.valid() => {
                    if round == 0 {
                        first.insert(h, o.raw);
                    }
                    objs.insert(h, o);
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    }

    println!(
        "\n{:<10} {:>9} {:>10} {:>9} {:>9} {:>9} {:>6}  {}",
        "handle", "size", "data", "wpos", "rpos", "fill", "uniq", "content"
    );
    for (&h, o) in &objs {
        let mut content = String::new();
        let mut uniq = 0;
        if opts.sniff && o.ring_sane() {
            let want = 8192usize.min(o.size as usize);
            let start = if (o.wpos as usize) >= want {
                o.data + o.wpos - want as u32
            } else {
                o.data
            };
            if let Ok(buf) = rd.read_phys(start, want) {
                uniq = graph::distinct_bytes(&buf);
                let (ts, ts_stride, tso) = graph::ts_score_multi(&buf);
                let (tlv, tlvo) = graph::tlv_score(&buf);
                if ts > 0.95 {
                    // Naming the stride matters: 192 means TTS (a 4-byte
                    // arrival timestamp per packet), which a client will not
                    // accept as bare MPEG-TS without stripping it.
                    let kind = if ts_stride == 192 { "TTS" } else { "MPEG-TS" };
                    content = format!("{kind} stride {ts_stride} ratio {ts:.3} phase {tso}");
                } else if tlv > 8 {
                    content = format!("TLV chain {tlv} at {tlvo}");
                } else {
                    content = format!("ts {ts:.2}/{ts_stride} tlv {tlv}");
                }
            }
        }
        println!(
            "{:#010x} {:>9x} {:>10x} {:>9x} {:>9x} {:>9x} {:>6}  {}",
            h, o.size, o.data, o.wpos, o.rpos, o.fill, uniq, content
        );
        if opts.watch > 0 {
            if let Some(f) = first.get(&h) {
                let moved: Vec<String> = (0..64)
                    .filter(|&i| f[i] != o.raw[i])
                    .map(|i| format!("+{:#04x}: {:08x}->{:08x}", i * 4, f[i], o.raw[i]))
                    .collect();
                if !moved.is_empty() {
                    println!("           moved: {}", moved.join("  "));
                }
            }
        }
        if opts.raw {
            for i in (0..64).step_by(4) {
                println!(
                    "           +{:#04x}: {:08x} {:08x} {:08x} {:08x}",
                    i * 4,
                    o.raw[i],
                    o.raw[i + 1],
                    o.raw[i + 2],
                    o.raw[i + 3]
                );
            }
        }
    }
    if opts.watch > 0 {
        println!("\n(`moved` lists the object words that changed across {rounds} samples;");
        println!(" a ring that is being written shows exactly one advancing offset)");
    }
    0
}

/// Map and hexdump XCode6 physical memory. Read-only.
fn cmd_peek(opts: &Options) -> i32 {
    let Some(phys) = opts.phys else {
        eprintln!("mkra: peek needs --phys");
        return 2;
    };
    let drv = match XcDrv::open_path(&opts.dev) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("mkra: cannot open {}: {e}", opts.dev);
            return 1;
        }
    };
    let rd = graph::ObjReader::new(drv);
    match rd.read_phys(phys, opts.len) {
        Ok(b) => {
            hexdump(&b, phys);
            let (ts, ts_stride, off) = graph::ts_score_multi(&b);
            let (tlv, tlvo) = graph::tlv_score(&b);
            println!("\nts ratio {ts:.3} stride {ts_stride} phase {off}   \
                      tlv chain {tlv} at {tlvo}   distinct bytes {}",
                     graph::distinct_bytes(&b));
            0
        }
        Err(e) => {
            eprintln!("mkra: xc_map({phys:#x}) failed: {e}");
            1
        }
    }
}

fn hexdump(b: &[u8], base: u32) {
    let mut prev: Option<&[u8]> = None;
    let mut star = false;
    for (i, row) in b.chunks(16).enumerate() {
        if prev == Some(row) {
            if !star {
                println!("*");
                star = true;
            }
            continue;
        }
        star = false;
        prev = Some(row);
        let hex: Vec<String> = row.iter().map(|c| format!("{c:02x}")).collect();
        let asc: String = row
            .iter()
            .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' })
            .collect();
        println!(
            "{:08x}  {:<47}  |{}|",
            base as usize + i * 16,
            hex.join(" "),
            asc
        );
    }
}

/// Read the tuner-board CPLD, or perform a tightly constrained and verified
/// XCode6 input-route change.  Register 2 is X6TSISL1: its low and high
/// nibbles select the sources for XCode6 TSI lanes 0 and 1 respectively.
fn cmd_cpld(opts: &Options) -> i32 {
    let Some(reg) = opts.cpld_reg else {
        eprintln!("mkra: cpld needs --reg <0..4>");
        return 2;
    };
    if reg > 4 {
        eprintln!("mkra: only documented CPLD registers 0..4 may be read");
        return 2;
    }
    if opts.cpld_set.is_some() && reg != 2 {
        eprintln!("mkra: CPLD writes are restricted to register 2 (X6TSISL1)");
        return 2;
    }
    if opts.cpld_set.is_some() != opts.cpld_expect.is_some() {
        eprintln!("mkra: --set and --expect must be supplied together");
        return 2;
    }

    let dev = match cpld::Cpld::open(cpld::DEV_PATH, opts.cpld_set.is_some()) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("mkra: cannot open/configure {}: {e}", cpld::DEV_PATH);
            return 1;
        }
    };

    if let (Some(value), Some(expected)) = (opts.cpld_set, opts.cpld_expect) {
        match dev.write_x6tsisl1(expected, value) {
            Ok(()) => println!(
                "CPLD reg 2 (X6TSISL1): {expected:#04x} -> {value:#04x} (readback verified)"
            ),
            Err(e) => {
                eprintln!("mkra: CPLD route change refused or failed: {e}");
                return 1;
            }
        }
    }

    match dev.read_reg(reg) {
        Ok(value) => {
            if reg == 2 {
                let source = |n: u8| match n {
                    0 => "TS1",
                    1 => "TS2",
                    2 => "TS3",
                    3 => "ASI(TS)",
                    0x0b => "ASI(TLV)",
                    _ => "UNKNOWN",
                };
                println!(
                    "CPLD reg 2 (X6TSISL1) = {value:#04x}  lane0={} lane1={}",
                    source(value & 0x0f),
                    source(value >> 4)
                );
            } else {
                println!("CPLD reg {reg} = {value:#04x}");
            }
            0
        }
        Err(e) => {
            eprintln!("mkra: CPLD register read failed: {e}");
            1
        }
    }
}

/// Command a tune without serving anything, so the frontend can be exercised
/// and observed on its own.
/// Post one `msgFedSetTuningStart` straight at a frontend decoder.
///
/// This bypasses the FEM, so nothing converts units or assigns a device for us.
/// It exists because the FEM entry point the product exposes
/// (`psfc_fed_AdbsSetTuningStart`) is advanced-BS only, and BS/CS110 HD can be
/// reached no other way.
///
/// `--dry-run` prints the exact bytes without sending them. Use it first: this
/// message goes to a vendor daemon that has crashed before on injected input.
fn cmd_tune_fed(opts: &Options, kind: &str) -> i32 {
    let req = match kind {
        "adbs" | "4k" => tuner::FedTuneRequest::advanced_bs(opts.tsid, opts.freq_khz),
        "isdb-s" | "isdb-s-fem" | "bs" | "cs" => {
            let Some(path) = opts.path else {
                eprintln!("mkra: --fed isdb-s needs --path <1..4>");
                return 2;
            };
            match tuner::FedTuneRequest::isdb_s(path, opts.tsid, opts.freq_khz) {
                Some(r) => r,
                None => {
                    eprintln!("mkra: --path {path} is not one of the four non-4K frontends");
                    return 2;
                }
            }
        }
        other => {
            eprintln!("mkra: --fed must be `adbs`, `isdb-s` or `isdb-s-fem`, got `{other}`");
            return 2;
        }
    };
    if let Err(e) = req.validate() {
        eprintln!("mkra: {e}");
        return 2;
    }

    let managed = kind == "isdb-s-fem";
    let qid = if managed { tuner::FEMD_QID } else { req.fed_qid };
    let msg_id = if managed { tuner::MSG_FEM_LEGACY_TUNING_START } else { tuner::MSG_FED_SET_TUNING_START };
    let freq = req.freq_word().unwrap();
    println!(
        "frontend {:?}\n  qid        {}\n  RF         {} kHz\n  freq word  {:#010x}{}\n  TSID       {:04X}\n  reply qid  {}",
        req.frontend,
        qid,
        req.rf_khz,
        freq,
        match req.frontend {
            tuner::Frontend::AdvancedBs => format!("  (IF kHz, sent verbatim)"),
            tuner::Frontend::IsdbS { .. } =>
                format!("  (packed BCD of RF/10; the FED computes IF {} kHz)",
                        tuner::rf_to_if_khz(req.rf_khz).unwrap()),
        },
        req.stream_id,
        req.reply_qid,
    );
    println!("  message id {msg_id:#x}");

    if opts.dry_run {
        println!("\n--dry-run: nothing was sent.");
        return 0;
    }

    let t = match tuner::Tuner::open_qid(qid, tuner::MSGH_LIST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: queue for qid {qid} unavailable: {e}");
            return 1;
        }
    };
    match if managed { t.tune_fem_legacy(&req) } else { t.tune_fed(&req) } {
        Ok(()) => {
            println!("\nposted msgId={msg_id:#x} to msqid {}", t.msqid());
            println!(
                "check it landed:  python3 tools/analysis/ipcmap.py --pid <tuner pid>\n\
                 then read the FED's msgrcv buffer with `mkra probe --peek`."
            );
            0
        }
        Err(e) => {
            eprintln!("mkra: tune failed: {e}");
            1
        }
    }
}

/// Assign or release a frontend device on a path.
///
/// Separate from `tune` on purpose: this changes who owns the demodulator, and
/// on this box there is only one, so it is the step that can take 4K reception
/// away. It is reversible with `--disconnect`.
fn cmd_fem_dev(opts: &Options) -> i32 {
    let Some(path) = opts.path else {
        eprintln!("mkra: fem-dev needs --path <1..6>");
        return 2;
    };
    let req = if opts.disconnect {
        tuner::FemDeviceRequest::disconnect(path)
    } else if let Some(dev) = opts.fem_dev {
        tuner::FemDeviceRequest::assign(path, dev)
    } else {
        eprintln!("mkra: fem-dev needs --dev <hex> or --disconnect");
        return 2;
    };
    if let Err(e) = req.validate() {
        eprintln!("mkra: {e}");
        return 2;
    }
    match req.dev {
        Some(d) => println!("FEM ChangeDevice: aDevOfPath[{path}] = {d:#x}"),
        None => println!("FEM DisconnectDevice: aDevOfPath[{path}] = 1"),
    }
    if opts.dry_run {
        println!("--dry-run: nothing was sent.");
        return 0;
    }
    let t = match tuner::Tuner::open(tuner::MSGH_LIST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: FEM queue unavailable: {e}");
            return 1;
        }
    };
    match t.set_device(&req) {
        Ok(()) => {
            println!("posted to FEM msqid {}", t.msqid());
            0
        }
        Err(e) => {
            eprintln!("mkra: failed: {e}");
            1
        }
    }
}

/// Put one BS/CS 2K frontend into RF standby through its vendor FED task.
fn cmd_fed_off(opts: &Options) -> i32 {
    let Some(path) = opts.path else {
        eprintln!("mkra: fed-off needs --path <1..4>");
        return 2;
    };
    let Some(dev) = opts.fem_dev else {
        eprintln!("mkra: fed-off needs the captured device bit via --dev <0x2|0x4|0x8>");
        return 2;
    };
    let Some(req) = tuner::FedPowerOffRequest::isdb_s(path, dev) else {
        eprintln!("mkra: --path {path} is not one of the four non-4K FED queues");
        return 2;
    };
    if let Err(e) = req.validate() {
        eprintln!("mkra: {e}");
        return 2;
    }
    println!(
        "FED tuner power off: qid {} path {} dev {:#x} msgId 0x0D payload[0]={:#x}",
        req.fed_qid, req.path, req.dev, req.dev
    );
    if opts.dry_run {
        println!("--dry-run: nothing was sent.");
        return 0;
    }
    let t = match tuner::Tuner::open_qid(req.fed_qid, tuner::MSGH_LIST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: queue for qid {} unavailable: {e}", req.fed_qid);
            return 1;
        }
    };
    match t.power_off_fed(&req) {
        Ok(()) => {
            println!("posted msgId 0x0D to msqid {}", t.msqid());
            0
        }
        Err(e) => {
            eprintln!("mkra: failed: {e}");
            1
        }
    }
}

fn cmd_tune(opts: &Options) -> i32 {
    if opts.freq_khz == 0 {
        eprintln!("mkra: tune needs --freq <kHz>");
        return 2;
    }
    if let Some(kind) = opts.fed.as_deref() {
        return cmd_tune_fed(opts, kind);
    }
    let req = tuner::TuneRequest {
        tuner_no: opts.tuner_no,
        freq_khz: opts.freq_khz,
        stream_id: opts.tsid,
        input_port_no: opts.input_port,
        reply_qid: opts.reply_qid,
    };
    if let Err(e) = req.validate() {
        eprintln!("mkra: {e}");
        return 2;
    }
    let t = match tuner::Tuner::open(tuner::MSGH_LIST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mkra: tuner queue unavailable: {e}");
            return 1;
        }
    };
    println!(
        "tuning: {} kHz TSID {:04X} tuner {} port {}",
        req.freq_khz, req.stream_id, req.tuner_no, req.input_port_no
    );
    match t.tune(&req) {
        Ok(()) => {
            println!("four messages posted to the tuner daemon");
            0
        }
        Err(e) => {
            eprintln!("mkra: tune failed: {e}");
            1
        }
    }
}

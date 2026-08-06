//! Commanding a tune without linking the product's libraries.
//!
//! `psfc_fed_AdbsSetTuningStart` lives in `libtuner.so`, which is linked
//! against glibc 2.15. A statically linked musl binary cannot call it. What it
//! *does* is fully recovered though (docs/ABI.md §7): it posts four System V
//! messages to the `tuner` daemon's queue. We post the same four directly.
//!
//! Bypassing `libsysapi` also side-steps its registration-table precondition —
//! the one that makes `eSysExSendMessage` silently return 2 when a process has
//! not initialised its own queue table (docs/ABI.md §7). That table is
//! `libsysapi`'s private bookkeeping; the kernel queue does not care.
//!
//! # Safety of this module
//! Sending these messages makes the box retune, which changes what the user is
//! watching. Nothing here runs unless `--tune` is passed; see DESIGN.md §6.3.

use std::ffi::c_void;
use std::io;

use crate::sys::{self, c_int, c_long};

/// The `tuner` daemon's product-internal queue id (0x1C7). This is the FEM
/// (front-end manager); it fans requests out to the per-frontend FED tasks.
pub const FEMD_QID: u32 = 455;

/// The advanced-BS/CS (4K) frontend decoder, `abs.T_FED_ADBS1`.
pub const FED_ADBS1_QID: u32 = 456;

/// The four non-4K frontend decoders, `abs.T_FED1`..`T_FED4`
/// (`vFedLocalMain`). On this box they serve **BS / CS110 HD only**.
///
/// Terrestrial is not reachable through them: the ISDB-T register sets exist in
/// `tuner` (`regset_ISDBT_6MHz`, `ofdmtune.c`) but every one of those functions
/// has zero callers and appears in no function-pointer table, so no message can
/// reach them. That is a property of the firmware, not of this box's state.
///
/// Observed 2026-07-31 by reading every product task's parked `msgrcv`
/// arguments out of `/proc/<tid>/syscall`. All four were alive and parked with
/// an all-zero receive buffer, i.e. **never messaged since boot** — the path is
/// open but unused. Re-read them with `tools/analysis/ipcmap.py` rather than
/// trusting these: queues are `IPC_PRIVATE` and the qid→msqid map is per boot.
pub const FED_NON4K_QIDS: [u32; 4] = [467, 468, 612, 631];

/// Where the product publishes its `qid-msqid` map at runtime.
pub const MSGH_LIST: &str = "/tmp/bdh_out_msgh.lst";

/// `msgFedSetTuningStart`. Identical id for the 4K and non-4K FED dispatchers
/// (`vFedAdbsLocalMain` / `vFedLocalMain`), recovered by
/// `tools/analysis/fed_dispatch.py`.
pub const MSG_FED_SET_TUNING_START: u32 = 2;

/// `msgFemRequestFedCtrlTunerPowerOff` handled by `vFedLocalMain`.
pub const MSG_FED_TUNER_POWER_OFF: u32 = 0x0D;

/// Local-oscillator frequency of a Japanese BS/CS-110 LNB, in kHz.
///
/// The FEM converts the RF downlink frequency to this intermediate frequency
/// before handing it to a satellite FED. Measured, not assumed: the live 4K FED
/// was parked holding `if_khz = 1_356_360` while tuned to BS-17, whose downlink
/// is 12_034_360 kHz, and 12_034_360 − 10_678_000 = 1_356_360 exactly. It also
/// reproduces the standard BS IF plan (BS-1 = 1_049_480 kHz, 19_180 kHz apart:
/// BS-17 = 1_049_480 + 16×19_180 = 1_356_360).
pub const LNB_LO_KHZ: u32 = 10_678_000;

/// RF downlink (kHz) → LNB intermediate frequency (kHz).
///
/// The advanced-BS FED takes this value directly. The non-4K FED does the same
/// subtraction itself and wants [`rf_to_bcd10`] instead.
pub fn rf_to_if_khz(rf_khz: u32) -> Option<u32> {
    rf_khz.checked_sub(LNB_LO_KHZ).filter(|&f| f > 0)
}

/// RF downlink (kHz) → the packed-BCD form the **non-4K** FED expects.
///
/// This is the single most important difference between the two frontends and
/// the easiest way to send a message that is accepted and silently does nothing.
/// `vFedSetTuningStart` (`tuner+0x14610`) decodes eight BCD nibbles to a binary
/// value `n`, then computes `freq_kHz = n * 10 - 10_678_000` and clamps the
/// result to the 950_000..=2_150_000 L-band window. So the field is
/// "RF kHz divided by 10, written as decimal digits in hex nibbles":
///
/// ```text
/// BS-1  RF 11_727_480 kHz -> 1_172_748 -> 0x01172748 -> IF 1_049_480 kHz
/// BS-15 RF 11_996_000 kHz -> 1_199_600 -> 0x01199600 -> IF 1_318_000 kHz
/// ```
///
/// `vFedAdbsSetTuningStart` (`tuner+0x10020`) does no such conversion — it
/// passes the word through untouched, which is why the 4K path uses plain kHz.
///
/// Returns `None` if the frequency is not a whole number of 10 kHz steps or
/// would fall outside the demodulator's clamp window, because in both cases the
/// frontend would tune somewhere other than where the caller asked.
pub fn rf_to_bcd10(rf_khz: u32) -> Option<u32> {
    if rf_khz % 10 != 0 {
        return None;
    }
    let if_khz = rf_to_if_khz(rf_khz)?;
    if !(950_000..=2_150_000).contains(&if_khz) {
        return None;
    }
    let mut n = rf_khz / 10;
    let mut bcd: u32 = 0;
    for shift in 0..8 {
        bcd |= (n % 10) << (shift * 4);
        n /= 10;
    }
    // Eight nibbles hold at most 99_999_999; anything left over would be lost.
    if n != 0 {
        return None;
    }
    Some(bcd)
}

/// `msgsnd` payload. 96 bytes on the wire, but the product passes 92 as
/// `msgsz` because `mtype` is not counted.
#[repr(C)]
struct SysVWire {
    mtype: c_long,       // +0x00 always 1
    _unused: [u32; 2],   // +0x04 sent uninitialised by the product
    self_ptr: u32,       // +0x0c self-reference, meaningless to the receiver
    _inner_head: u32,    // +0x10
    msg_id: u32,         // +0x14
    len: u32,            // +0x18
    src: u32,            // +0x1c libtuner always sends 1
    payload: [u8; 64],   // +0x20
}

const MSGSZ: usize = 0x5C; // 92

impl SysVWire {
    fn new(msg_id: u32, len: u32) -> Self {
        SysVWire {
            mtype: 1,
            _unused: [0; 2],
            self_ptr: 0,
            _inner_head: 0,
            msg_id,
            len,
            src: 1,
            payload: [0; 64],
        }
    }

    fn put_u32(&mut self, off: usize, v: u32) {
        self.payload[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u16(&mut self, off: usize, v: u16) {
        self.payload[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
}

/// Parameters of `psfc_fed_AdbsSetTuningStart`.
#[derive(Debug, Clone, Copy)]
pub struct TuneRequest {
    /// 1..=3. Values outside that range make the product return -1000.
    pub tuner_no: u16,
    pub freq_khz: u32,
    /// TSID. 0 or 0xFFFF means "unspecified".
    pub stream_id: u16,
    /// Must be >= 100. Only 101 selects portCode 3.
    pub input_port_no: u16,
    /// Product-internal queue that would receive the reply. We do not read
    /// replies (DESIGN.md §6.2), so this is only echoed into the message.
    pub reply_qid: u32,
}

impl TuneRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.tuner_no) {
            return Err(format!("tuner_no must be 1..3, got {}", self.tuner_no));
        }
        if self.input_port_no < 100 {
            return Err(format!(
                "input_port_no must be >= 100, got {}",
                self.input_port_no
            ));
        }
        if self.freq_khz < 1_000_000 || self.freq_khz > 30_000_000 {
            // Every advanced-BS downlink is around 12 GHz expressed in kHz;
            // anything outside this window is a unit mistake, and sending a
            // wild frequency to a live frontend is not worth risking.
            return Err(format!(
                "freq_khz {} is outside the plausible 1..30 GHz range",
                self.freq_khz
            ));
        }
        Ok(())
    }

    fn tuner_code(&self) -> u32 {
        8 + self.tuner_no as u32 // 1,2,3 -> 9,10,11
    }

    fn tuner_mask(&self) -> u32 {
        0x200 << (self.tuner_no as u32 - 1) // 1,2,3 -> 0x200,0x400,0x800
    }

    fn port_code(&self) -> u32 {
        if self.input_port_no == 101 {
            3
        } else {
            1
        }
    }

    /// `mode` in the msgId=1 payload, distinct from the transponder "mode"
    /// column in the channel tables (docs/ABI.md §7).
    fn mode(&self) -> u32 {
        if self.stream_id == 0xFFFF {
            1
        } else {
            2
        }
    }

    /// The four messages, in order, exactly as `libtuner.so` emits them.
    fn messages(&self) -> [SysVWire; 4] {
        // #1 and #3 share a buffer in the product (r7+0x40); two words of it
        // are initialised and the rest is whatever was on the stack.
        let mut m1 = SysVWire::new(3, 0x10);
        m1.put_u32(0x04, 0);
        m1.put_u32(0x08, self.tuner_mask());

        let mut m2 = SysVWire::new(0, 0x1C);
        m2.put_u32(0x00, self.tuner_code());
        m2.put_u32(0x04, 1);
        m2.put_u32(0x08, self.port_code());

        let mut m3 = SysVWire::new(2, 0x10);
        m3.payload = m1.payload;

        let mut m4 = SysVWire::new(1, 0x14);
        m4.put_u16(0x00, self.stream_id);
        m4.put_u32(0x04, self.freq_khz);
        m4.put_u32(0x08, 1);
        m4.put_u32(0x0C, self.mode());
        m4.put_u32(0x10, self.reply_qid);

        [m1, m2, m3, m4]
    }
}

/// Resolve the product-internal qid to a kernel `msqid`.
///
/// Every queue is created with `IPC_PRIVATE`, so there is no stable key to
/// `msgget` and the id changes every boot. The product writes the mapping to
/// `/tmp/bdh_out_msgh.lst` at startup; that file is the only stable route.
pub fn resolve_msqid(qid: u32, list_path: &str) -> io::Result<c_int> {
    let text = std::fs::read_to_string(list_path)?;
    parse_msgh_list(&text, qid).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no entry for qid {qid} in {list_path}"),
        )
    })
}

/// Find `<qid>-<msqid>` in the product's queue map.
pub fn parse_msgh_list(text: &str, qid: u32) -> Option<c_int> {
    let prefix = format!("{qid}-");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            // The value may be followed by more fields; take the leading digits.
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

/// A tune addressed straight at one frontend decoder, skipping the FEM.
///
/// This is the *only* route to the non-4K frontends: the FEM entry point we
/// recovered (`psfc_fed_AdbsSetTuningStart`, docs/ABI.md §7) is advanced-BS only,
/// while the FED dispatchers themselves share one vocabulary. Talking to a FED
/// directly means we also take over the FEM's job — in particular the RF→IF
/// conversion, which is why `if_khz` is spelled out here instead of `freq_khz`.
///
/// The layout below is not derived, it is a transcription of a real message
/// captured out of the running 4K FED's `msgrcv` buffer while it held a lock on
/// BS-17, measured on the box:
///
/// ```text
/// msgId=2  len=0x14  src=1
/// payload: 10 b1 14 00 | 48 b2 14 00 | 01 00 00 00 | 02 00 00 00 | 80 03 00 00
///          ^stream_id    ^if_khz       ^one          ^mode         ^reply_qid
///          0xB110        1_356_360     1             2             896
/// ```
///
/// Bytes +0x02..+0x04 are padding the product leaves uninitialised (it happened
/// to hold 0x0014, the message length). We send zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    /// Advanced BS/CS — ISDB-S3, MMT/TLV. `abs.T_FED_ADBS1`.
    AdvancedBs,
    /// BS / CS110 HD — ISDB-S, MPEG-TS. `abs.T_FED1..4`, `vFedLocalMain`.
    ///
    /// The `path` is 1..=6 and goes in the wire header's `src` field, not the
    /// payload: `vFedmMain` and the FED both read it from there.
    IsdbS { path: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct FedTuneRequest {
    /// Product-internal qid of the target FED (`FED_ADBS1_QID` or one of
    /// `FED_NON4K_QIDS`).
    pub fed_qid: u32,
    pub frontend: Frontend,
    /// TSID, or 0xFFFF for "unspecified".
    pub stream_id: u16,
    /// **RF downlink frequency in kHz**, as it appears in the channel tables.
    /// Each frontend's encoding is applied by [`message`](Self::message); the
    /// caller never does the conversion, because doing it in two places is how
    /// a tune ends up silently landing on the wrong transponder.
    pub rf_khz: u32,
    /// Queue that receives the completion notification. The 4K path uses 896
    /// (`abs.T_ADSB_TUM`); the non-4K FED does not read this field.
    pub reply_qid: u32,
}

impl FedTuneRequest {
    /// Reproduce the message captured from the running 4K frontend.
    pub fn advanced_bs(stream_id: u16, rf_khz: u32) -> Self {
        FedTuneRequest {
            fed_qid: FED_ADBS1_QID,
            frontend: Frontend::AdvancedBs,
            stream_id,
            rf_khz,
            reply_qid: 896,
        }
    }

    /// BS / CS110 HD on one of the four non-4K frontends.
    ///
    /// `path` is 1..=6 and selects both the queue (`Q_FED<path>`) and the value
    /// the FED reads out of the wire header.
    ///
    /// **This shares the physical demodulator with the 4K frontend**
    /// (`gAribTunerCnt == 1`, and `dev 0x2` and `dev 0x200` are the same
    /// `dnum`), so a successful tune here takes 4K reception away.
    pub fn isdb_s(path: u32, stream_id: u16, rf_khz: u32) -> Option<Self> {
        let idx = usize::try_from(path.checked_sub(1)?).ok()?;
        Some(FedTuneRequest {
            fed_qid: *FED_NON4K_QIDS.get(idx)?,
            frontend: Frontend::IsdbS { path },
            stream_id,
            rf_khz,
            reply_qid: 0,
        })
    }

    fn mode(&self) -> u32 {
        if self.stream_id == 0xFFFF {
            1
        } else {
            2
        }
    }

    /// The frequency word, already in the encoding this frontend expects.
    pub fn freq_word(&self) -> Option<u32> {
        match self.frontend {
            Frontend::AdvancedBs => rf_to_if_khz(self.rf_khz),
            Frontend::IsdbS { .. } => rf_to_bcd10(self.rf_khz),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let known = std::iter::once(FED_ADBS1_QID).chain(FED_NON4K_QIDS);
        if !known.clone().any(|q| q == self.fed_qid) {
            return Err(format!(
                "fed_qid {} is not a known frontend queue (expected one of {:?})",
                self.fed_qid,
                known.collect::<Vec<_>>()
            ));
        }
        if let Frontend::IsdbS { path } = self.frontend {
            if !(1..=6).contains(&path) {
                return Err(format!("path must be 1..6, got {path}"));
            }
            // The non-4K FED only performs its TSID-selection path when mode
            // is 2.  0xFFFF produces mode 1 and skips that path; 0x40F1 is a
            // second vendor sentinel in the candidate scan.  Both yielded a
            // silent no-data tune during the first live TS_RECODER attempt.
            if self.stream_id == 0xFFFF || self.stream_id == 0x40F1 {
                return Err(format!(
                    "ISDB-S needs an explicit, non-sentinel TSID (got {:04X}); \
                     use a real TSID such as 4010 on BS-1",
                    self.stream_id
                ));
            }
        }
        // Every Japanese satellite downlink this box can see is 11.7–12.8 GHz
        // expressed in kHz. Rejecting anything else catches the unit slips that
        // would otherwise be posted straight at a live frontend.
        if !(11_000_000..=13_000_000).contains(&self.rf_khz) {
            return Err(format!(
                "rf_khz {} is outside the 11–13 GHz satellite downlink band. \
                 Pass the RF frequency from the channel table, not an IF.",
                self.rf_khz
            ));
        }
        if self.freq_word().is_none() {
            return Err(format!(
                "rf_khz {} cannot be encoded for this frontend (IF outside the \
                 950–2150 MHz clamp, or not a whole 10 kHz step)",
                self.rf_khz
            ));
        }
        Ok(())
    }

    fn message(&self) -> Result<SysVWire, String> {
        self.validate()?;
        let freq = self.freq_word().ok_or("frequency not encodable")?;
        let mut m = SysVWire::new(MSG_FED_SET_TUNING_START, 0x14);
        // `src` carries the path for the non-4K FED. libtuner always sends 1,
        // which is also path 1, so the 4K case is unchanged.
        m.src = match self.frontend {
            Frontend::AdvancedBs => 1,
            Frontend::IsdbS { path } => path,
        };
        m.put_u16(0x00, self.stream_id);
        m.put_u32(0x04, freq);
        // +0x08 is an enumeration. The live 4K frontend was holding 1; the
        // non-4K handler treats 0 as "normal tune". Nothing measured tells us
        // what 1 means to `vFedLocalMain`, so each path keeps the value it was
        // actually observed or documented with rather than sharing a guess.
        m.put_u32(
            0x08,
            match self.frontend {
                Frontend::AdvancedBs => 1,
                Frontend::IsdbS { .. } => 0,
            },
        );
        m.put_u32(0x0C, self.mode());
        m.put_u32(0x10, self.reply_qid);
        Ok(m)
    }
}

/// Power one non-4K FED down through the vendor daemon.
///
/// This is the cleanup counterpart of [`FedTuneRequest`]. The product caller
/// passes the selected `dev` as the fourth argument of `eSysExSendMessage`
/// (`tuner:0xD19A..0xD1B6` and `0xE528..0xE544`), and the handler at
/// `tuner:0x13A8E` reads it as the first payload word. Older notes that called
/// this an empty payload were therefore unsafe.
#[derive(Debug, Clone, Copy)]
pub struct FedPowerOffRequest {
    pub fed_qid: u32,
    pub path: u32,
    pub dev: u32,
}

impl FedPowerOffRequest {
    /// Construct a BS/CS 2K power-off request.
    ///
    /// `path` selects a FED queue, while `dev` selects the physical frontend.
    /// They are independent axes: the proven Gate 4 sequence assigns dev
    /// `0x02` to path 2. Never derive one from the other.
    pub fn isdb_s(path: u32, dev: u32) -> Option<Self> {
        let idx = usize::try_from(path.checked_sub(1)?).ok()?;
        Some(FedPowerOffRequest {
            fed_qid: *FED_NON4K_QIDS.get(idx)?,
            path,
            dev,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        let Some(expected_qid) = self
            .path
            .checked_sub(1)
            .and_then(|i| FED_NON4K_QIDS.get(i as usize))
            .copied()
        else {
            return Err(format!(
                "power-off path must be 1..4, got {}",
                self.path
            ));
        };
        if self.fed_qid != expected_qid {
            return Err(format!(
                "path {} must target qid {}, got qid {}",
                self.path, expected_qid, self.fed_qid
            ));
        }
        if !DEV_ISDB_S.contains(&self.dev) {
            return Err(format!(
                "dev {:#x} is not a known ISDB-S frontend bit (expected one of {:x?})",
                self.dev, DEV_ISDB_S
            ));
        }
        Ok(())
    }

    fn message(&self) -> Result<SysVWire, String> {
        self.validate()?;
        let mut m = SysVWire::new(MSG_FED_TUNER_POWER_OFF, 0x10);
        m.src = self.path;
        m.put_u32(0, self.dev);
        Ok(m)
    }
}

/// Device bits the FEM understands, as they appear in `aDevOfPath[]`.
///
/// `1` is the "disconnected" marker the FEM writes on DisconnectDevice, not a
/// device. `0x2/0x4/0x8` are the non-4K (ISDB-S) frontends and `0x200/0x400/
/// 0x800` the advanced-BS ones; on this box `0x2` and `0x200` are two faces of
/// the same physical demodulator, so claiming one takes it from the other.
pub const DEV_DISCONNECTED: u32 = 1;
pub const DEV_ISDB_S: [u32; 3] = [0x2, 0x4, 0x8];
pub const DEV_ADVANCED_BS: [u32; 3] = [0x200, 0x400, 0x800];

/// `aDevOfPath[]` in the `tuner` daemon's `.data`, for a read-only check of
/// what each path currently owns before changing anything.
pub const A_DEV_OF_PATH_ADDR: u32 = 0x37604;

/// FEM message ids (`vFedmMain`).
pub const MSG_FEM_CHANGE_DEVICE: u32 = 2;
pub const MSG_FEM_DISCONNECT_DEVICE: u32 = 4;

/// Assign a device to a path, or give it back.
///
/// `aDevOfPath[path] = dev` is what the 4K path's own startup does: the third
/// message of `libtuner`'s four-message sequence is exactly this, carrying
/// `0x200` at payload+8. So this is the product's own mechanism, not a new one.
///
/// Reversible: [`FemDeviceRequest::disconnect`] puts the path back to
/// `DEV_DISCONNECTED`, which is where every unused path already sits.
#[derive(Debug, Clone, Copy)]
pub struct FemDeviceRequest {
    pub path: u32,
    /// `None` means disconnect.
    pub dev: Option<u32>,
}

impl FemDeviceRequest {
    pub fn assign(path: u32, dev: u32) -> Self {
        FemDeviceRequest { path, dev: Some(dev) }
    }

    pub fn disconnect(path: u32) -> Self {
        FemDeviceRequest { path, dev: None }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=6).contains(&self.path) {
            return Err(format!("path must be 1..6, got {}", self.path));
        }
        if let Some(dev) = self.dev {
            // Only bits the FEM's own vocabulary contains. An unknown device
            // makes LockTunerDevice grab a handle that does not exist and takes
            // the daemon down with it.
            let known = DEV_ISDB_S.iter().chain(DEV_ADVANCED_BS.iter());
            if !known.clone().any(|&d| d == dev) {
                return Err(format!(
                    "dev {dev:#x} is not a known frontend device (expected one of {:x?})",
                    known.collect::<Vec<_>>()
                ));
            }
        }
        Ok(())
    }

    fn message(&self) -> Result<SysVWire, String> {
        self.validate()?;
        let (id, dev) = match self.dev {
            Some(d) => (MSG_FEM_CHANGE_DEVICE, d),
            None => (MSG_FEM_DISCONNECT_DEVICE, 0),
        };
        let mut m = SysVWire::new(id, 0x10);
        m.src = self.path;
        m.put_u32(0x08, dev);
        Ok(m)
    }
}

pub struct Tuner {
    msqid: c_int,
}

impl Tuner {
    pub fn open(list_path: &str) -> io::Result<Self> {
        Ok(Tuner {
            msqid: resolve_msqid(FEMD_QID, list_path)?,
        })
    }

    /// Open a queue other than the FEM's — used for direct-to-FED tuning.
    pub fn open_qid(qid: u32, list_path: &str) -> io::Result<Self> {
        Ok(Tuner {
            msqid: resolve_msqid(qid, list_path)?,
        })
    }

    /// Assign or release a frontend device on a path, via the FEM.
    pub fn set_device(&self, req: &FemDeviceRequest) -> io::Result<()> {
        let msg = req
            .message()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let rc = unsafe {
            sys::msgsnd(
                self.msqid,
                &msg as *const SysVWire as *const c_void,
                MSGSZ,
                sys::IPC_NOWAIT,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!("msgsnd(msgId {}) to FEM msqid {}: {e}", msg.msg_id, self.msqid),
            ));
        }
        Ok(())
    }

    /// Post a single `msgFedSetTuningStart` at one frontend.
    ///
    /// Unlike [`Tuner::tune`] this bypasses the FEM entirely, so nothing
    /// re-selects the CPLD TS crossbar or powers the frontend on for us. It is
    /// the right call only when the target frontend is already initialised.
    pub fn tune_fed(&self, req: &FedTuneRequest) -> io::Result<()> {
        let msg = req
            .message()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let rc = unsafe {
            sys::msgsnd(
                self.msqid,
                &msg as *const SysVWire as *const c_void,
                MSGSZ,
                sys::IPC_NOWAIT,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "msgsnd(msgId 2) to fed qid {} (msqid {}): {e}",
                    req.fed_qid, self.msqid
                ),
            ));
        }
        Ok(())
    }

    /// Ask a non-4K FED to enter RF standby using the product's own message.
    pub fn power_off_fed(&self, req: &FedPowerOffRequest) -> io::Result<()> {
        let msg = req
            .message()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let rc = unsafe {
            sys::msgsnd(
                self.msqid,
                &msg as *const SysVWire as *const c_void,
                MSGSZ,
                sys::IPC_NOWAIT,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "msgsnd(msgId 0x0D) to fed qid {} (msqid {}): {e}",
                    req.fed_qid, self.msqid
                ),
            ));
        }
        Ok(())
    }

    pub fn msqid(&self) -> c_int {
        self.msqid
    }

    /// Post the tuning sequence. Fire-and-forget: the reply goes to a
    /// product-internal queue we have no way to claim, so lock is inferred
    /// from the stream actually producing packets (DESIGN.md §6.2).
    pub fn tune(&self, req: &TuneRequest) -> io::Result<()> {
        req.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        for (i, msg) in req.messages().iter().enumerate() {
            let rc = unsafe {
                sys::msgsnd(
                    self.msqid,
                    msg as *const SysVWire as *const c_void,
                    MSGSZ,
                    sys::IPC_NOWAIT,
                )
            };
            if rc < 0 {
                let e = io::Error::last_os_error();
                return Err(io::Error::new(
                    e.kind(),
                    format!("msgsnd #{} (msgId {}) to msqid {}: {e}", i + 1, msg.msg_id, self.msqid),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_layout_matches_the_recovered_format() {
        // 96 bytes total, with msgsnd told 92 because mtype is excluded.
        assert_eq!(std::mem::size_of::<SysVWire>(), 96);
        assert_eq!(MSGSZ, 92);
        let w = SysVWire::new(1, 0x14);
        let base = &w as *const _ as usize;
        assert_eq!(&w.msg_id as *const _ as usize - base, 0x14);
        assert_eq!(&w.len as *const _ as usize - base, 0x18);
        assert_eq!(&w.src as *const _ as usize - base, 0x1c);
        assert_eq!(&w.payload as *const _ as usize - base, 0x20);
    }

    #[test]
    fn derived_codes_match_the_disassembly() {
        let mk = |n| TuneRequest {
            tuner_no: n,
            freq_khz: 12_034_360,
            stream_id: 0xB110,
            input_port_no: 101,
            reply_qid: 0,
        };
        assert_eq!(mk(1).tuner_code(), 9);
        assert_eq!(mk(2).tuner_code(), 10);
        assert_eq!(mk(3).tuner_code(), 11);
        assert_eq!(mk(1).tuner_mask(), 0x200);
        assert_eq!(mk(2).tuner_mask(), 0x400);
        assert_eq!(mk(3).tuner_mask(), 0x800);
        assert_eq!(mk(1).port_code(), 3);

        let mut other = mk(1);
        other.input_port_no = 100;
        assert_eq!(other.port_code(), 1);
    }

    #[test]
    fn mode_depends_on_whether_a_stream_id_was_given() {
        let mut r = TuneRequest {
            tuner_no: 1,
            freq_khz: 12_034_360,
            stream_id: 0xFFFF,
            input_port_no: 101,
            reply_qid: 7,
        };
        assert_eq!(r.mode(), 1);
        r.stream_id = 0xB110;
        assert_eq!(r.mode(), 2);
    }

    #[test]
    fn message_sequence_is_id_3_0_2_1() {
        let r = TuneRequest {
            tuner_no: 1,
            freq_khz: 12_034_360,
            stream_id: 0xB110,
            input_port_no: 101,
            reply_qid: 42,
        };
        let m = r.messages();
        assert_eq!([m[0].msg_id, m[1].msg_id, m[2].msg_id, m[3].msg_id], [3, 0, 2, 1]);
        assert_eq!([m[0].len, m[1].len, m[2].len, m[3].len], [0x10, 0x1C, 0x10, 0x14]);
        assert!(m.iter().all(|x| x.mtype == 1 && x.src == 1));
        // Messages 1 and 3 carry the same buffer.
        assert_eq!(m[0].payload, m[2].payload);
    }

    #[test]
    fn tuning_payload_places_every_field_at_the_recovered_offset() {
        let r = TuneRequest {
            tuner_no: 2,
            freq_khz: 12_034_360,
            stream_id: 0xB110,
            input_port_no: 101,
            reply_qid: 0x1234,
        };
        let p = r.messages()[3].payload;
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 0xB110);
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 12_034_360);
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(p[12..16].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(p[16..20].try_into().unwrap()), 0x1234);
    }

    #[test]
    fn validation_rejects_out_of_range_arguments() {
        let ok = TuneRequest {
            tuner_no: 1,
            freq_khz: 12_034_360,
            stream_id: 0,
            input_port_no: 101,
            reply_qid: 0,
        };
        assert!(ok.validate().is_ok());

        let bad_tuner = TuneRequest { tuner_no: 4, ..ok };
        assert!(bad_tuner.validate().is_err());
        let bad_port = TuneRequest { input_port_no: 99, ..ok };
        assert!(bad_port.validate().is_err());
        // Unit slips in both directions: MHz instead of kHz, and Hz-scaled.
        assert!(TuneRequest { freq_khz: 12_034, ..ok }.validate().is_err());
        assert!(TuneRequest { freq_khz: 3_000_000_000, ..ok }.validate().is_err());
    }

    #[test]
    fn if_conversion_reproduces_the_captured_bs17_tune() {
        // The one measurement this whole path rests on.
        assert_eq!(rf_to_if_khz(12_034_360), Some(1_356_360));
        // Standard BS IF plan: BS-1 = 1_049_480, 19_180 apart.
        assert_eq!(rf_to_if_khz(11_727_480), Some(1_049_480));
        for ch in 1..=23u32 {
            let rf = 11_727_480 + (ch - 1) * 19_180;
            assert_eq!(rf_to_if_khz(rf), Some(1_049_480 + (ch - 1) * 19_180));
        }
        // A terrestrial RF would underflow, which is the caller's signal that
        // no LNB is involved.
        assert_eq!(rf_to_if_khz(557_000), None);
    }

    #[test]
    fn fed_message_matches_the_bytes_captured_from_the_live_frontend() {
        // The whole 4K path is validated against one real message read out of
        // abs.T_FED_ADBS1's msgrcv buffer while it held a lock on BS-17.
        let r = FedTuneRequest::advanced_bs(0xB110, 12_034_360);
        let m = r.message().expect("should encode");
        assert_eq!(m.msg_id, 2);
        assert_eq!(m.len, 0x14);
        assert_eq!(m.src, 1);
        assert_eq!(&m.payload[0..2], &[0x10, 0xb1]);
        assert_eq!(&m.payload[4..8], &[0x48, 0xb2, 0x14, 0x00]);
        assert_eq!(&m.payload[8..12], &[0x01, 0x00, 0x00, 0x00]);
        assert_eq!(&m.payload[12..16], &[0x02, 0x00, 0x00, 0x00]);
        assert_eq!(&m.payload[16..20], &[0x80, 0x03, 0x00, 0x00]);
    }

    #[test]
    fn non_4k_frequency_is_packed_bcd_of_rf_over_ten() {
        // vFedSetTuningStart decodes eight BCD nibbles, then n*10 - 10_678_000.
        assert_eq!(rf_to_bcd10(11_727_480), Some(0x0117_2748)); // BS-1
        assert_eq!(rf_to_bcd10(11_765_840), Some(0x0117_6584)); // BS-3
        assert_eq!(rf_to_bcd10(11_996_000), Some(0x0119_9600)); // BS-15
        assert_eq!(rf_to_bcd10(12_291_000), Some(0x0122_9100)); // CS110 ND2

        // Round-trip through the frontend's own arithmetic.
        for rf in [11_727_480u32, 11_996_000, 12_291_000] {
            let bcd = rf_to_bcd10(rf).unwrap();
            let mut n = 0u32;
            for shift in (0..8).rev() {
                n = n * 10 + ((bcd >> (shift * 4)) & 0xF);
            }
            assert_eq!(n * 10 - LNB_LO_KHZ, rf_to_if_khz(rf).unwrap());
        }

        // Outside the demodulator's 950-2150 MHz clamp, so unencodable.
        assert_eq!(rf_to_bcd10(11_000_000), None);
        // Not a whole 10 kHz step.
        assert_eq!(rf_to_bcd10(11_727_485), None);
    }

    #[test]
    fn the_two_frontends_encode_the_same_channel_differently() {
        // This is the failure this split exists to prevent: sending the 4K
        // encoding to a non-4K FED is accepted and tunes somewhere else.
        let rf = 11_996_000;
        let adbs = FedTuneRequest::advanced_bs(0x4011, rf);
        let isdb = FedTuneRequest::isdb_s(1, 0x4011, rf).unwrap();
        assert_eq!(adbs.freq_word(), Some(1_318_000));
        assert_eq!(isdb.freq_word(), Some(0x0119_9600));
        assert_ne!(adbs.freq_word(), isdb.freq_word());
    }

    #[test]
    fn isdb_s_request_targets_the_right_queue_and_puts_the_path_in_src() {
        for (path, qid) in [(1, 467), (2, 468), (3, 612), (4, 631)] {
            let r = FedTuneRequest::isdb_s(path, 0x4010, 11_727_480).unwrap();
            assert_eq!(r.fed_qid, qid);
            let m = r.message().expect("should encode");
            assert_eq!(m.src, path, "path must travel in the wire header");
            assert_eq!(&m.payload[4..8], &[0x48, 0x27, 0x17, 0x01]);
            // "normal tune", not the 4K path's 1.
            assert_eq!(&m.payload[8..12], &[0, 0, 0, 0]);
        }
        // Only four non-4K queues are known at runtime on this box.
        assert!(FedTuneRequest::isdb_s(0, 0, 11_727_480).is_none());
        assert!(FedTuneRequest::isdb_s(5, 0, 11_727_480).is_none());
    }

    #[test]
    fn fed_request_rejects_a_frequency_that_is_not_an_rf_downlink() {
        let base = FedTuneRequest::isdb_s(1, 0x4010, 11_727_480).unwrap();
        assert!(base.validate().is_ok());
        // Passing an IF where an RF belongs — the exact slip this catches.
        assert!(FedTuneRequest { rf_khz: 1_049_480, ..base }.validate().is_err());
        // MHz instead of kHz.
        assert!(FedTuneRequest { rf_khz: 11_727, ..base }.validate().is_err());
        assert!(FedTuneRequest { fed_qid: 999, ..base }.validate().is_err());
    }

    #[test]
    fn fed_power_off_carries_dev_in_the_first_payload_word() {
        // Gate 4's proven ownership pairing: path 2 takes physical dev 0x02.
        let r = FedPowerOffRequest::isdb_s(2, 0x2).unwrap();
        assert_eq!(r.fed_qid, FED_NON4K_QIDS[1]);
        assert_eq!(r.path, 2);
        assert_eq!(r.dev, 0x2);
        let m = r.message().unwrap();
        assert_eq!(m.msg_id, 0x0D);
        assert_eq!(m.len, 0x10);
        assert_eq!(m.src, 2);
        assert_eq!(u32::from_le_bytes(m.payload[0..4].try_into().unwrap()), 0x2);
        assert!(m.payload[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn fed_power_off_does_not_derive_dev_from_path() {
        assert!(FedPowerOffRequest::isdb_s(4, 0x2).unwrap().validate().is_ok());
        assert!(FedPowerOffRequest::isdb_s(2, 0x4).unwrap().validate().is_ok());
        assert!(FedPowerOffRequest::isdb_s(2, 0x200)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn fed_mode_follows_the_same_rule_as_the_fem_path() {
        let mut r = FedTuneRequest::advanced_bs(0xFFFF, 12_034_360);
        assert_eq!(r.mode(), 1);
        r.stream_id = 0xB110;
        assert_eq!(r.mode(), 2);
    }

    #[test]
    fn non_4k_rejects_wildcard_tsids_that_skip_selection() {
        let unspecified = FedTuneRequest::isdb_s(1, 0xFFFF, 11_727_480).unwrap();
        assert!(unspecified.validate().is_err());
        let vendor_sentinel = FedTuneRequest::isdb_s(1, 0x40F1, 11_996_000).unwrap();
        assert!(vendor_sentinel.validate().is_err());
        let real = FedTuneRequest::isdb_s(1, 0x4010, 11_727_480).unwrap();
        assert!(real.validate().is_ok());
        assert_eq!(real.mode(), 2);
    }

    #[test]
    fn fem_device_assignment_matches_the_products_own_third_message() {
        // libtuner's four-message sequence carries 0x200 at payload+8 in its
        // msgId=2; that IS ChangeDevice, and it is why aDevOfPath[1] == 0x200
        // on the live box. Our assignment must be the same shape.
        let r = FemDeviceRequest::assign(2, 0x2);
        let m = r.message().unwrap();
        assert_eq!(m.msg_id, MSG_FEM_CHANGE_DEVICE);
        assert_eq!(m.len, 0x10);
        assert_eq!(m.src, 2, "path travels in the wire header");
        assert_eq!(&m.payload[8..12], &[0x02, 0, 0, 0]);

        let d = FemDeviceRequest::disconnect(2).message().unwrap();
        assert_eq!(d.msg_id, MSG_FEM_DISCONNECT_DEVICE);
        assert_eq!(&d.payload[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn fem_device_assignment_refuses_a_device_the_fem_does_not_know() {
        // An unknown device makes LockTunerDevice grab a handle that is not
        // there. This guard is the difference between a retune and a crash.
        assert!(FemDeviceRequest::assign(2, 0x2).validate().is_ok());
        assert!(FemDeviceRequest::assign(1, 0x200).validate().is_ok());
        assert!(FemDeviceRequest::assign(2, 0x3).validate().is_err());
        assert!(FemDeviceRequest::assign(2, 0).validate().is_err());
        assert!(FemDeviceRequest::assign(2, DEV_DISCONNECTED).validate().is_err());
        assert!(FemDeviceRequest::assign(0, 0x2).validate().is_err());
        assert!(FemDeviceRequest::assign(7, 0x2).validate().is_err());
    }

    #[test]
    fn msqid_lookup_matches_only_the_exact_qid() {
        // Shape of the product's real file: `<qid>-<msqid>` per line.
        let list = "1-32768\n455-557073\n4550-99999\n1455-11111\n456-1\n";
        assert_eq!(parse_msgh_list(list, 455), Some(557073));
        assert_eq!(parse_msgh_list(list, 456), Some(1));
        assert_eq!(parse_msgh_list(list, 999), None);
    }
}

// --- the product's own channel change ---------------------------------------

/// Queue the product's ADSB communication task listens on (`0x36F`).
pub const ADSB_COMM_QID: u32 = 0x36F;

/// `msgId` of the change-channel request.
///
/// `abs:vEtmMain+0x308 @0x459a64` builds a ten-word body and calls
/// `eSysSendMessage(0x36F, 0x6004, body, 0x28, -1)` (send at `0x459af2`).
pub const MSG_ADSB_CHANGE_CHANNEL: u32 = 0x6004;

/// Fixed words the product always sends for a live 4K channel change.
///
/// Confirmed against ten consecutive remote-control changes captured on the
/// serial console (the product's serial console): the receiver prints the
/// body as `0, OID, 9, 0, b, SID, FREQ, TSID, 0, BID` every time, and the
/// sender fills those words from a struct at `+0x00 Dev`, `+0x04 Tune`,
/// `+0x08 OID(u8)`, `+0x09 BID(u8)`, `+0x0a NID(u16)`, `+0x0c SID(u16)`,
/// `+0x0e TSID(u16)`, `+0x10 FREQ(u32)`, `+0x14 NotDecode`.
const CHANGE_DEV: u32 = 9;

/// `Tune` selects why the product is tuning. Measured values: `0` for a
/// remote-control channel change, `3` when the product resumes from standby
/// (and re-sends this very message itself), `7` during the boot scan.
pub const TUNE_CHANNEL_CHANGE: u32 = 0;
pub const TUNE_RESUME: u32 = 3;

/// Advanced-BS network id (`0x000B`), the only one this box receives.
pub const ADVBS_NETWORK_ID: u32 = 0x0b;

/// A change-channel request in the product's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeChannelRequest {
    /// Operation id. The product increments it per change and the reply echoes
    /// it; it is not a channel parameter.
    pub oid: u32,
    /// Why we are tuning. See `TUNE_CHANNEL_CHANGE` / `TUNE_RESUME`.
    pub tune: u32,
    /// The product sends 0 for a normal change. Its name says it should select
    /// tuning without starting decode, which would be the cheap idle state,
    /// but that is unverified — no captured message has ever set it.
    pub not_decode: u32,
    pub service_id: u16,
    /// RF frequency in kHz, exactly as the product logs it (not the IF).
    pub freq_khz: u32,
    pub tlv_stream_id: u16,
    /// Remote-control key id (NHK=1, 日テレ=2, 朝日=3, TBS=4, テレ東=5,
    /// フジ=6, ショップ=9, QVC=0xa, 231=0xb).
    pub bid: u8,
}

impl ChangeChannelRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.service_id == 0 {
            return Err("service id must be non-zero".into());
        }
        if !(10_000_000..=13_000_000).contains(&self.freq_khz) {
            return Err(format!(
                "RF frequency {} kHz is outside the advanced-BS band",
                self.freq_khz
            ));
        }
        if self.tlv_stream_id == 0 || self.tlv_stream_id == 0xFFFF {
            return Err(format!("implausible TLV stream id {:#06x}", self.tlv_stream_id));
        }
        Ok(())
    }

    /// The ten-word body, in the order the product's receiver prints it.
    pub fn body(&self) -> [u32; 10] {
        [
            0,
            self.oid,
            CHANGE_DEV,
            self.tune,
            ADVBS_NETWORK_ID,
            u32::from(self.service_id),
            self.freq_khz,
            u32::from(self.tlv_stream_id),
            self.not_decode,
            u32::from(self.bid),
        ]
    }

    fn message(&self) -> Result<SysVWire, String> {
        self.validate()?;
        let mut m = SysVWire::new(MSG_ADSB_CHANGE_CHANNEL, 0x28);
        for (i, w) in self.body().iter().enumerate() {
            m.put_u32(i * 4, *w);
        }
        Ok(m)
    }
}

impl Tuner {
    /// Ask the product to change channel, exactly as its own UI layer does.
    ///
    /// This is deliberately not a frontend command: the product reacts by
    /// tearing down the TS units, powering the tuner off, re-tuning, and then
    /// rebuilding the MMT filters and ES rings for the new service. Driving
    /// only the frontend would leave those filters pointing at the old
    /// service's assets.
    ///
    /// Any tap on the live ES rings must be released before calling this, or
    /// the product's own buffer teardown fails with `XCE_IN_USE`.
    pub fn change_channel(&self, req: &ChangeChannelRequest) -> io::Result<()> {
        let msg = req
            .message()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let rc = unsafe {
            sys::msgsnd(
                self.msqid,
                &msg as *const SysVWire as *const c_void,
                MSGSZ,
                sys::IPC_NOWAIT,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "msgsnd(msgId {:#x}) to ADSB comm qid {:#x} (msqid {}): {e}",
                    msg.msg_id, ADSB_COMM_QID, self.msqid
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod change_channel_tests {
    use super::*;

    /// The BS日テレ4K change captured on the serial console:
    /// `OID(4) Dev(9) Tune(0) NID(11) SID(141) FREQ(11842560) TSID(45170)
    ///  NotDecode(0) BID(2)`, printed by the receiver as
    /// `0, 4, 9, 0, b, 141, 11842560, b072, 0, 2`.
    fn bs_nittele() -> ChangeChannelRequest {
        ChangeChannelRequest {
            oid: 4,
            tune: TUNE_CHANNEL_CHANGE,
            not_decode: 0,
            service_id: 141,
            freq_khz: 11_842_560,
            tlv_stream_id: 0xB072,
            bid: 2,
        }
    }

    #[test]
    fn body_matches_the_product_print() {
        assert_eq!(
            bs_nittele().body(),
            [0, 4, 9, 0, 0x0b, 141, 11_842_560, 0xb072, 0, 2]
        );
    }

    #[test]
    fn wire_header_matches_the_sender() {
        let m = bs_nittele().message().expect("valid");
        assert_eq!(m.mtype, 1);
        assert_eq!(m.msg_id, 0x6004);
        assert_eq!(m.len, 0x28); // eSysSendMessage's fourth argument
        assert_eq!(&m.payload[..4], &0u32.to_le_bytes());
        assert_eq!(&m.payload[20..24], &141u32.to_le_bytes());
        assert_eq!(&m.payload[24..28], &11_842_560u32.to_le_bytes());
        assert_eq!(&m.payload[28..32], &0xb072u32.to_le_bytes());
        assert_eq!(&m.payload[36..40], &2u32.to_le_bytes());
    }

    #[test]
    fn implausible_requests_are_refused_before_they_reach_the_box() {
        let mut r = bs_nittele();
        r.freq_khz = 1_164_560; // the IF, not the RF
        assert!(r.validate().is_err());
        r = bs_nittele();
        r.tlv_stream_id = 0xFFFF;
        assert!(r.validate().is_err());
        r = bs_nittele();
        r.service_id = 0;
        assert!(r.validate().is_err());
    }
}

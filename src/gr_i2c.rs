//! Live `/dev/i2c-0` transport for the recovered ISDB-T sequence.
//!
//! The product semaphore is taken before the first transaction and released
//! after lock polling.  `I2c0Guard` is attach-only: this module never creates
//! or reinitialises the vendor's glibc 2.15 shared semaphore.

use crate::gr::Write;

pub struct TuneResult {
    pub rf_latch: [u8; 3],
    pub lock_value: u8,
    pub polls: u16,
    pub locked: bool,
}

/// Read-only ISDB-S state sampled under the product I2C semaphore.
pub struct IsdbsStatus {
    pub lock: u8,
    pub ts_pin: u8,
    pub ts_serial: u8,
}

/// Result of the one-byte ISDB-S/TLV output-mode exclusion repair.
pub struct IsdbsTsExclusiveResult {
    pub before: u8,
    pub after: u8,
}

/// SC1501A registers that decide how the demodulator frames its serial output.
///
/// `regset_TLV_SPI_1BIT` (applied by `DMD_set_tsout` on every ISDB-S3 tune)
/// writes bank 7 `0x73/0x7b/0x7d` and `DMD_set_tsout` itself writes `0x74/0x75`.
/// `regset_ISDBS`, `regset_ISDBT_6MHz` and `regset_Recommend_DIFF` contain none
/// of `0x73/0x75/0x7b/0x7d`, and no other write site for `0x7b` exists anywhere
/// in `tuner`. A 4K session therefore leaves those four bytes in TLV state when
/// the same unit is later switched to ISDB-S. This snapshot is the read-only
/// test of that model; it changes nothing.
pub struct DmdOutputRegs {
    pub lock: u8,
    /// `(register, value)` for I2C 7-bit `0x10` (SC1501A bank 0).
    pub bank0: [(u8, u8); 6],
    /// ISDB-S selection controls, not a received PAT or a selected-stream lock.
    /// `tuner:0x2304e..0x230be` programs TSID at 2f/30 and mode at 31.
    pub bank2_selection: [u8; 3],
    /// `(register, value)` for I2C 7-bit `0x17` (SC1501A bank 7).
    pub bank7: [(u8, u8); 9],
}

/// Received monitor windows, independent of the requested TSID registers.
pub struct IsdbsTmcc {
    pub locks: [u8; 2],
    pub selectors: [u8; 2],
    pub indexed_tsids: [u16; 8],
    pub raw: [u8; 40],
}

/// DMD_isdbs_tmcc_tbl information 59..66: MSB-first fields at bit 189+16*n.
pub fn tmcc_tsid_fields(raw: &[u8; 40]) -> [u16; 8] {
    std::array::from_fn(|i| {
        let start = 189 + 16 * i;
        (start..start + 16).fold(0u16, |value, bit| {
            (value << 1) | u16::from((raw[bit / 8] >> (7 - bit % 8)) & 1)
        })
    })
}

/// Bank-0 registers sampled by [`DmdOutputRegs`], in report order.
pub const DMD_BANK0_REGS: [u8; 6] = [0x0b, 0x0c, 0x13, 0x16, 0x18, 0xcc];
/// Bank-7 registers sampled by [`DmdOutputRegs`], in report order.
pub const DMD_BANK7_REGS: [u8; 9] = [0x0a, 0x72, 0x73, 0x74, 0x75, 0x76, 0x7b, 0x7d, 0x8d];

/// Bank-7 bytes that `regset_TLV_SPI_1BIT` + `DMD_set_tsout(systype=0x0d)`
/// leave behind, and that no ISDB-S or ISDB-T path rewrites.
pub const TLV_SPI_1BIT_TSOUT: [(u8, u8); 4] = [(0x73, 0x03), (0x75, 0x01), (0x7b, 0x67), (0x7d, 0x81)];
/// The same four registers in the vendor's own full base set
/// `SC1501A_ISDB-S3_180517.rgs` (`tuner` `.data` `0x3b4c8`), i.e. the state a
/// `_DIFF_` regset is authored against.
pub const BASE_TSOUT: [(u8, u8); 4] = [(0x73, 0x24), (0x75, 0x00), (0x7b, 0x64), (0x7d, 0x01)];

pub fn is_isdbt_locked(value: u8) -> bool {
    (value & 0x90) == 0 && (value & 0x0f) >= 9
}

/// Before/after image of the four-register output-framing change.
pub struct TsoutRestoreResult {
    pub before: Vec<(u8, u8)>,
    pub after: Vec<(u8, u8)>,
}

/// Which of the two documented framing states to install.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TsoutTarget {
    /// Vendor full-base-set values: the state `regset_ISDBS` is a DIFF against.
    Base,
    /// Put the `regset_TLV_SPI_1BIT` values back (rollback).
    TlvSpi1Bit,
}

impl TsoutTarget {
    pub fn want(self) -> [(u8, u8); 4] {
        match self {
            TsoutTarget::Base => BASE_TSOUT,
            TsoutTarget::TlvSpi1Bit => TLV_SPI_1BIT_TSOUT,
        }
    }

    pub fn expect_before(self) -> [(u8, u8); 4] {
        match self {
            TsoutTarget::Base => TLV_SPI_1BIT_TSOUT,
            TsoutTarget::TlvSpi1Bit => BASE_TSOUT,
        }
    }
}

/// Bank-0 output-interface state, as `(register, value)`.
///
/// `0x16` bit 0 is set in `regset_TLV_SPI_1BIT/2BIT/8BIT` and clear in every
/// MPEG-TS standard table (`regset_ISDBS`, `regset_ISDBT_6MHz`, `regset_DVBC`,
/// `regset_DVBT_*`, `regset_QAMB_*`, `regset_ISDBC_*`); bit 5 is set only in
/// the two serial widths. `0x13` is written by no table except the TLV_SPI
/// ones. The working 4K path uses the 1-bit serial port on XCode6 TSI0.
/// Other physical wiring has not been established by continuity measurements.
pub const SERIAL_1BIT_IFACE: [(u8, u8); 3] = [(0x13, 0x04), (0x16, 0x21), (0xcc, 0x80)];
/// What `regset_ISDBS` + `DMD_set_tsout(case 0/1)` leave behind, measured on
/// this box in Gate 5C1/5C2.
pub const ISDBS_IFACE: [(u8, u8); 3] = [(0x13, 0x00), (0x16, 0x00), (0xcc, 0xc0)];

/// The single byte that differs between the live green-4K output configuration
/// (`0b=00 0c=00 13=00 16=21 18=00 cc=40`, measured 2026-08-04) and the ISDB-S
/// one (`0b=01 0c=00 13=00 16=00 18=00 cc=c0`), once the two documented
/// content selectors `0x0b` bit 0 and `0xcc` bit 6/7 are set aside.
///
/// Gate 5C3 wrote `0x13 = 0x04` as well, which no working state uses:
/// `DMD_set_tsout`'s ISDB-S3 branch clears that bit again at `tuner:0x271c6`,
/// and green 4K really reads `13 = 0x00`. Keep `0x13` and `0xcc` untouched here.
pub const MODE16_SERIAL: [(u8, u8); 1] = [(0x16, 0x21)];
pub const MODE16_ISDBS: [(u8, u8); 1] = [(0x16, 0x00)];

/// Which bank-0 output-interface state to install.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TsoutInterface {
    /// The 1-bit serial interface the 4K path uses, with `0xcc` selecting the
    /// MPEG-TS content (bit 7) and dropping TLV (bit 6). Gate 5C3.
    Serial1Bit,
    /// Back to the values `regset_ISDBS` and `DMD_set_tsout` produce.
    IsdbsDefault,
    /// Gate 5C4: only `0x16`, the one unexplained difference.
    Mode16Serial,
    /// Rollback of [`TsoutInterface::Mode16Serial`].
    Mode16Isdbs,
    /// Hypothesis: bit 5 selects serial width independently of bit 0 (TLV).
    /// This value is experimental, not a confirmed vendor TS profile.
    TsSerial20,
    /// Restore the exact pre-experiment byte.
    TsSerial20Restore,
}

impl TsoutInterface {
    pub fn want(self) -> &'static [(u8, u8)] {
        match self {
            TsoutInterface::Serial1Bit => &SERIAL_1BIT_IFACE,
            TsoutInterface::IsdbsDefault => &ISDBS_IFACE,
            TsoutInterface::Mode16Serial => &MODE16_SERIAL,
            TsoutInterface::Mode16Isdbs => &MODE16_ISDBS,
            TsoutInterface::TsSerial20 => &[(0x16, 0x20)],
            TsoutInterface::TsSerial20Restore => &MODE16_ISDBS,
        }
    }

    pub fn expect_before(self) -> &'static [(u8, u8)] {
        match self {
            TsoutInterface::Serial1Bit => &ISDBS_IFACE,
            TsoutInterface::IsdbsDefault => &SERIAL_1BIT_IFACE,
            TsoutInterface::Mode16Serial => &MODE16_ISDBS,
            TsoutInterface::Mode16Isdbs => &MODE16_SERIAL,
            TsoutInterface::TsSerial20 => &MODE16_ISDBS,
            TsoutInterface::TsSerial20Restore => &[(0x16, 0x20)],
        }
    }
}

/// How the four bank-7 output-framing bytes compare against the two states the
/// product binary defines for them.
#[derive(Debug, PartialEq, Eq)]
pub enum TsoutFraming {
    /// Exactly `regset_TLV_SPI_1BIT` + `DMD_set_tsout(0x0d)`. Correct while
    /// the unit is in ISDB-S3; stale residue once it is switched to ISDB-S.
    TlvSpi1Bit,
    /// Exactly the vendor full-base-set values.
    Base,
    /// Neither. Do not write anything; record the bytes and re-audit.
    Other,
}

/// Classify a bank-7 sample. `regs` is `(register, value)` in any order and may
/// contain registers outside the four that matter.
pub fn classify_tsout(regs: &[(u8, u8)]) -> TsoutFraming {
    let matches = |want: &[(u8, u8)]| {
        want.iter()
            .all(|(r, v)| regs.iter().any(|(rr, vv)| rr == r && vv == v))
    };
    if matches(&TLV_SPI_1BIT_TSOUT) {
        TsoutFraming::TlvSpi1Bit
    } else if matches(&BASE_TSOUT) {
        TsoutFraming::Base
    } else {
        TsoutFraming::Other
    }
}

#[cfg(all(target_os = "linux", target_arch = "arm"))]
mod arm_linux {
    use super::{
        is_isdbt_locked, DmdOutputRegs, IsdbsStatus, IsdbsTsExclusiveResult, TsoutRestoreResult,
        TuneResult, Write, DMD_BANK0_REGS, DMD_BANK7_REGS,
    };
    use crate::i2c_lock::I2c0Guard;
    use crate::sys;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    const I2C_RDWR: i32 = 0x0707;
    const I2C_M_RD: u16 = 0x0001;
    const BEFORE_TSOUT: usize = 7 + 11 + 62 + 168 + 3;

    #[repr(C)]
    struct I2cMsg {
        addr: u16,
        flags: u16,
        len: u16,
        buf: *mut u8,
    }

    #[repr(C)]
    struct I2cRdwrData {
        msgs: *mut I2cMsg,
        nmsgs: u32,
    }

    const _: [(); 12] = [(); std::mem::size_of::<I2cMsg>()];
    const _: [(); 8] = [(); std::mem::size_of::<I2cRdwrData>()];

    struct I2c0 {
        file: File,
    }

    impl I2c0 {
        fn open() -> io::Result<Self> {
            Ok(Self {
                file: OpenOptions::new().read(true).write(true).open("/dev/i2c-0")?,
            })
        }

        fn transfer(&self, msgs: &mut [I2cMsg]) -> io::Result<()> {
            let mut data = I2cRdwrData {
                msgs: msgs.as_mut_ptr(),
                nmsgs: msgs.len() as u32,
            };
            let rc = unsafe {
                sys::ioctl_ptr(
                    self.file.as_raw_fd(),
                    I2C_RDWR,
                    &mut data as *mut I2cRdwrData as *mut std::ffi::c_void,
                )?
            };
            if rc as usize != msgs.len() {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("I2C_RDWR completed {rc}/{} messages", msgs.len()),
                ));
            }
            Ok(())
        }

        fn write(&self, slave7: u8, reg: u8, bytes: &[u8]) -> io::Result<()> {
            let mut frame = Vec::with_capacity(bytes.len() + 1);
            frame.push(reg);
            frame.extend_from_slice(bytes);
            let mut msg = [I2cMsg {
                addr: u16::from(slave7),
                flags: 0,
                len: frame.len() as u16,
                buf: frame.as_mut_ptr(),
            }];
            self.transfer(&mut msg)
        }

        fn read(&self, slave7: u8, reg: u8, len: usize) -> io::Result<Vec<u8>> {
            let mut subaddr = [reg];
            let mut out = vec![0u8; len];
            let mut msgs = [
                I2cMsg {
                    addr: u16::from(slave7),
                    flags: 0,
                    len: 1,
                    buf: subaddr.as_mut_ptr(),
                },
                I2cMsg {
                    addr: u16::from(slave7),
                    flags: I2C_M_RD,
                    len: len as u16,
                    buf: out.as_mut_ptr(),
                },
            ];
            self.transfer(&mut msgs)?;
            Ok(out)
        }

        fn mask_write(&self, slave7: u8, reg: u8, mask: u8, value: u8) -> io::Result<()> {
            let old = self.read(slave7, reg, 1)?[0];
            let new = (old & !mask) | (value & mask);
            self.write(slave7, reg, &[new])
        }
    }

    fn standby_isdbt_on_bus(bus: &I2c0) -> io::Result<()> {
        for (reg, bytes) in [
            (0x74, &[0x12][..]),
            (0x67, &[0x00][..]),
            (0x5e, &[0x15, 0x00, 0x00][..]),
            (0x88, &[0x00][..]),
            (0x87, &[0xc0][..]),
        ] {
            bus.write(0x60, reg, bytes)?;
        }
        Ok(())
    }

    pub fn tune(writes: &[Write]) -> io::Result<TuneResult> {
        if writes.len() <= BEFORE_TSOUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "incomplete GR transaction plan",
            ));
        }
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;

        let operation = (|| -> io::Result<TuneResult> {
            for (index, write) in writes.iter().enumerate() {
                if index == BEFORE_TSOUT {
                    bus.mask_write(0x10, 0x0b, 0x01, 0x01)?;
                    bus.mask_write(0x10, 0xcc, 0x80, 0x80)?;
                }
                bus.write(write.slave7, write.reg, &write.data)?;
                if write.slave7 == 0x60 && write.reg == 0x9b {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }

            let latch = bus.read(0x60, 0x70, 3)?;
            let rf_latch = [latch[0], latch[1], latch[2]];
            let mut lock_value = 0xff;
            let mut polls = 0u16;
            let mut locked = false;
            for index in 0..301 {
                lock_value = bus.read(0x11, 0xa1, 1)?[0];
                polls = index + 1;
                if is_isdbt_locked(lock_value) {
                    locked = true;
                    break;
                }
                if index != 300 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(TuneResult { rf_latch, lock_value, polls, locked })
        })();

        let result = match operation {
            Ok(result) => result,
            Err(primary) => {
                // Preserve the first error, but mirror x_fin_isdbt as far as
                // the still-responsive bus allows before dropping the lock.
                let _ = standby_isdbt_on_bus(&bus);
                let _ = guard.release();
                return Err(primary);
            }
        };
        if !result.locked {
            standby_isdbt_on_bus(&bus)?;
        }
        guard.release()?;
        Ok(result)
    }

    /// Put only the terrestrial RF section into the vendor-defined standby
    /// state.  This is the exact `x_fin_isdbt` five-write sequence and uses
    /// the same product-wide I2C semaphore as tuning.
    pub fn standby_isdbt() -> io::Result<()> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = standby_isdbt_on_bus(&bus);
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Read the documented ISDB-S demodulator lock byte without changing RF
    /// state.  The product exposes this as SC1501A page `0x12`, register
    /// `0x5e`, bit 5.  Keep the same attach-only semaphore discipline as the
    /// GR takeover path so this diagnostic cannot race the vendor tuner.
    pub fn read_isdbs_status() -> io::Result<IsdbsStatus> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<IsdbsStatus> {
            Ok(IsdbsStatus {
                lock: bus.read(0x12, 0x5e, 1)?[0],
                // Tune_ISDB_S_SetTsid's documented TS-output controls.  These
                // are read back only; no mode or pin state is changed.
                ts_pin: bus.read(0x10, 0x0b, 1)?[0],
                ts_serial: bus.read(0x10, 0xcc, 1)?[0],
            })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Read the SC1501A output-framing registers without changing any of them.
    ///
    /// Bank 0 is I2C 7-bit `0x10`, bank 7 is `0x17` (`unit 0`: `bank + 0x10`,
    /// `tuner:0x2623a-0x2624e`).  Every register here is read-only in this
    /// command; the whole point is to separate "the model predicts TLV residue"
    /// from "the device actually holds TLV residue".
    pub fn read_output_regs() -> io::Result<DmdOutputRegs> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<DmdOutputRegs> {
            let lock = bus.read(0x12, 0x5e, 1)?[0];
            let mut bank0 = [(0u8, 0u8); 6];
            for (slot, reg) in bank0.iter_mut().zip(DMD_BANK0_REGS) {
                *slot = (reg, bus.read(0x10, reg, 1)?[0]);
            }
            let mut bank7 = [(0u8, 0u8); 9];
            for (slot, reg) in bank7.iter_mut().zip(DMD_BANK7_REGS) {
                *slot = (reg, bus.read(0x17, reg, 1)?[0]);
            }
            let selection = bus.read(0x12, 0x2f, 3)?;
            let bank2_selection = [selection[0], selection[1], selection[2]];
            Ok(DmdOutputRegs { lock, bank0, bank2_selection, bank7 })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Compare native info44 relative-TS0 selection with info45 TSID selection.
    /// Fixed diagnostic for BS-1/4010 only. Native tuner:23018 masks reg31
    /// with 0x0f and writes 8 for relative zero; 2304e clears bit3 for TSID.
    /// Verify a live received 4010 at relative zero before changing reg31.
    pub fn select_isdbs_relative_zero(restore: bool) -> io::Result<super::IsdbsTsExclusiveResult> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| {
            for (bank, reg, expected) in [
                (0x10, 0x0b, 1), (0x10, 0x16, 0), (0x17, 0x72, 0),
                (0x12, 0x2f, 0x40), (0x12, 0x30, 0x10),
                (0x12, 0x31, if restore { 8 } else { 0 }),
            ] {
                let got = bus.read(bank, reg, 1)?[0];
                if got != expected {
                    return Err(io::Error::other(format!("relative-TS0 guard {bank:02x}:{reg:02x}={got:02x}, expected {expected:02x}")));
                }
            }
            if !restore {
                if bus.read(0x12, 0x5e, 1)?[0] & 0x20 == 0 {
                    return Err(io::Error::other("relative-TS0 requires ISDB-S lock"));
                }
                let saved = bus.read(0x12, 0x62, 1)?[0];
                let monitor = (|| -> io::Result<[u8; 2]> {
                    bus.write(0x12, 0x62, &[0])?;
                    Ok([bus.read(0x12, 0x63, 1)?[0], bus.read(0x12, 0x64, 1)?[0]])
                })();
                let restored = bus.write(0x12, 0x62, &[saved]);
                restored?;
                if bus.read(0x12, 0x62, 1)?[0] != saved {
                    return Err(io::Error::other("relative-TS monitor selector restore failed"));
                }
                if monitor? != [0x40, 0x10] {
                    return Err(io::Error::other("received relative TS0 is not 4010"));
                }
            }
            let before = if restore { 8 } else { 0 };
            let after = if restore { 0 } else { 8 };
            bus.mask_write(0x12, 0x31, if restore { 8 } else { 15 }, after)?;
            if bus.read(0x12, 0x31, 1)?[0] != after {
                bus.write(0x12, 0x31, &[before])?;
                return Err(io::Error::other("relative-TS0 readback failed; rollback attempted"));
            }
            Ok(super::IsdbsTsExclusiveResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    /// Reproduce the vendor's two bounded ISDB-S monitor reads. Only the
    /// volatile monitor selectors 62/65 are written; both are restored under
    /// the same I2C semaphore even when reading fails. No tuning/output writes.
    /// Native references: tuner:0x251d0 (info45), 0x260fc (TMCC window).
    pub fn read_isdbs_tmcc() -> io::Result<super::IsdbsTmcc> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let first_lock = bus.read(0x12, 0x5e, 1)?[0];
        // The vendor get_info monitor does not require TS lock. Requiring it
        // here would hide the received TSID table when the requested TSID is
        // missing. Instead verify the known ISDB-S output signature, and
        // report both raw lock bytes without treating unlocked data as valid.
        if bus.read(0x10, 0x0b, 1)?[0] != 1
            || bus.read(0x10, 0x16, 1)?[0] != 0
            || bus.read(0x17, 0x72, 1)?[0] != 0 {
            return Err(io::Error::other("TMCC monitor requires the known ISDB-S profile"));
        }
        let selectors = [bus.read(0x12, 0x62, 1)?[0], bus.read(0x12, 0x65, 1)?[0]];
        let operation = (|| -> io::Result<super::IsdbsTmcc> {
            let mut indexed_tsids = [0u16; 8];
            for (i, slot) in indexed_tsids.iter_mut().enumerate() {
                bus.write(0x12, 0x62, &[i as u8])?;
                // The OEM reads these individually, so do the same.
                let hi = bus.read(0x12, 0x63, 1)?[0];
                let lo = bus.read(0x12, 0x64, 1)?[0];
                *slot = u16::from_be_bytes([hi, lo]);
            }
            let mut raw = [0u8; 40];
            for (i, slot) in raw.iter_mut().enumerate() {
                bus.write(0x12, 0x65, &[i as u8])?;
                *slot = bus.read(0x12, 0x66, 1)?[0];
            }
            let last_lock = bus.read(0x12, 0x5e, 1)?[0];
            Ok(super::IsdbsTmcc { locks: [first_lock, last_lock], selectors, indexed_tsids, raw })
        })();
        // Always attempt both restores and release, independently of errors.
        let restore_relative = bus.write(0x12, 0x62, &[selectors[0]]);
        let restore_window = bus.write(0x12, 0x65, &[selectors[1]]);
        let verify = (|| -> io::Result<()> {
            if bus.read(0x12, 0x62, 1)?[0] != selectors[0]
                || bus.read(0x12, 0x65, 1)?[0] != selectors[1] {
                return Err(io::Error::other("TMCC monitor selectors did not restore"));
            }
            Ok(())
        })();
        let release = guard.release();
        restore_relative?;
        restore_window?;
        verify?;
        release?;
        operation
    }

    /// Install one of the two documented output-framing states on bank 7.
    ///
    /// Gate 5C1 measured that switching the unit from ISDB-S3 to ISDB-S leaves
    /// `73/75/7b/7d` at `03/01/67/81` while `regset_ISDBS` does rewrite
    /// b0 `0x16` (`21 -> 00`) and b7 `0x72` (`02 -> 00`). The demodulator is
    /// therefore locked with MPEG-TS framing selected and the TLV-SPI 1-bit
    /// output stage still enabled (`7b` bit 0, `7d` bit 7 are set in all three
    /// `regset_TLV_SPI_*` tables and clear in the vendor full base set).
    ///
    /// Every precondition is compared before anything is written, and every
    /// byte is read back afterwards. The four target values come from the
    /// product binary only; nothing here is a guessed or swept value.
    pub fn set_tsout_framing(target: super::TsoutTarget) -> io::Result<TsoutRestoreResult> {
        let want = target.want();
        let expect = target.expect_before();
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<TsoutRestoreResult> {
            // The locked ISDB-S signature must hold, or this is not the state
            // the change was reasoned about.
            let lock = bus.read(0x12, 0x5e, 1)?[0];
            let ts_pin = bus.read(0x10, 0x0b, 1)?[0];
            let ts_mode = bus.read(0x10, 0x16, 1)?[0];
            let ts_enable = bus.read(0x10, 0xcc, 1)?[0];
            if lock & 0x20 == 0 || ts_pin != 0x01 || ts_mode != 0x00 || ts_enable & 0x80 == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refuse: not the locked ISDB-S signature (5e={lock:#04x} \
                         0b={ts_pin:#04x} 16={ts_mode:#04x} cc={ts_enable:#04x})"
                    ),
                ));
            }

            let mut before = Vec::new();
            for (reg, expected) in expect {
                let value = bus.read(0x17, reg, 1)?[0];
                if value != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("refuse: bank7 {reg:#04x} is {value:#04x}, expected {expected:#04x}"),
                    ));
                }
                before.push((reg, value));
            }

            for (reg, value) in want {
                bus.write(0x17, reg, &[value])?;
            }

            let mut after = Vec::new();
            for (reg, value) in want {
                let read = bus.read(0x17, reg, 1)?[0];
                if read != value {
                    return Err(io::Error::other(format!(
                        "bank7 {reg:#04x} readback {read:#04x}, wrote {value:#04x}"
                    )));
                }
                after.push((reg, read));
            }
            Ok(TsoutRestoreResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Switch the SC1501A bank-0 output interface between the ISDB-S values and
    /// the 1-bit serial configuration the 4K path uses.
    ///
    /// Gate 5C2 established that bank 7 alone is not the blocking difference.
    /// The remaining untested difference between the working 4K output and the
    /// ISDB-S output is bank 0 `0x13`/`0x16`: every MPEG-TS standard table
    /// leaves `0x16 = 0x00`, so the interface-enable bit and the serial-mode bit
    /// are clear, while the only output this board wires to XCode6 TSI0 is the
    /// 1-bit serial port.
    ///
    /// The bank-7 serial output stage (`0x7b = 0x67`, `0x7d = 0x81`) must still
    /// be in place, so this refuses if Gate 5C2's rollback was not applied.
    pub fn set_tsout_interface(
        target: super::TsoutInterface,
    ) -> io::Result<TsoutRestoreResult> {
        let want = target.want();
        let expect = target.expect_before();
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<TsoutRestoreResult> {
            let lock = bus.read(0x12, 0x5e, 1)?[0];
            let ts_pin = bus.read(0x10, 0x0b, 1)?[0];
            if lock & 0x20 == 0 || ts_pin != 0x01 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refuse: not locked ISDB-S with TS pin on (5e={lock:#04x} 0b={ts_pin:#04x})"),
                ));
            }
            for (reg, value) in [(0x7bu8, 0x67u8), (0x7d, 0x81)] {
                let read = bus.read(0x17, reg, 1)?[0];
                if read != value {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "refuse: bank7 {reg:#04x} is {read:#04x}, expected the serial output \
                             stage value {value:#04x}"
                        ),
                    ));
                }
            }

            let mut before = Vec::new();
            for &(reg, expected) in expect {
                let value = bus.read(0x10, reg, 1)?[0];
                if value != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("refuse: bank0 {reg:#04x} is {value:#04x}, expected {expected:#04x}"),
                    ));
                }
                before.push((reg, value));
            }

            for &(reg, value) in want {
                bus.write(0x10, reg, &[value])?;
            }

            let mut after = Vec::new();
            for &(reg, value) in want {
                let read = bus.read(0x10, reg, 1)?[0];
                if read != value {
                    return Err(io::Error::other(format!(
                        "bank0 {reg:#04x} readback {read:#04x}, wrote {value:#04x}"
                    )));
                }
                after.push((reg, read));
            }
            Ok(TsoutRestoreResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Gate 5D read-only snapshot of the watchlist registers.
    ///
    /// Each register is read twice, ~30 ms apart, so the caller can tell a
    /// stable configuration value from one the demodulator updates itself.
    /// Nothing is written.
    pub fn snapshot_watchlist() -> io::Result<Vec<(u8, u8, u8, u8)>> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<Vec<(u8, u8, u8, u8)>> {
            let mut first = Vec::with_capacity(crate::snapshot::WATCHLIST.len());
            for (bank, reg, _) in crate::snapshot::WATCHLIST {
                first.push((bank, reg, bus.read(crate::snapshot::bank_addr(bank), reg, 1)?[0]));
            }
            // Bank 0xff is the reporting tag for the RF front-end, which is a
            // different chip on the same bus, not an SC1501A bank.
            for (reg, _) in crate::snapshot::RF_WATCHLIST {
                first.push((0xff, reg, bus.read(crate::snapshot::RF_ADDR, reg, 1)?[0]));
            }
            std::thread::sleep(Duration::from_millis(30));
            let mut out = Vec::with_capacity(first.len());
            for (bank, reg, v1) in first {
                let addr = if bank == 0xff {
                    crate::snapshot::RF_ADDR
                } else {
                    crate::snapshot::bank_addr(bank)
                };
                let v2 = bus.read(addr, reg, 1)?[0];
                out.push((bank, reg, v1, v2));
            }
            Ok(out)
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Gate 5D-A1: make the SC1501A TS/TLV output selector exclusive again.
    ///
    /// `DMD_set_tsout` sets bank0 `0xcc` bit 7 for legacy ISDB-S/ISDB-T and
    /// bit 6 for ISDB-S3, and never clears the other one (`tuner:0x270ba` and
    /// `0x2711c`, both masked writes). A known-good 4K box reads `0x40`; after a
    /// 2K session it reads `0xc0`, and the product's own ISDB-S3 retune leaves
    /// it at `0xc0`. This restores bits[7:6] to `01b` and touches nothing else.
    ///
    /// Refuses unless the stale pattern is actually present.
    pub fn select_tlv_output_exclusive() -> io::Result<IsdbsTsExclusiveResult> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<IsdbsTsExclusiveResult> {
            let before = bus.read(0x10, 0xcc, 1)?[0];
            if before & 0xc0 != 0xc0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refuse: 0xcc is {before:#04x}, expected bits[7:6] = 11b"),
                ));
            }
            let target = (before & !0xc0) | 0x40;
            bus.write(0x10, 0xcc, &[target])?;
            let after = bus.read(0x10, 0xcc, 1)?[0];
            if after != target {
                return Err(io::Error::other(format!(
                    "0xcc readback {after:#04x}, wrote {target:#04x}"
                )));
            }
            Ok(IsdbsTsExclusiveResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Put the demodulator's TS output back to the measured quiescent bytes
    /// (`0x0b` bit 0 clear, `0xcc = 0x40`).
    ///
    /// The product cannot do this itself after a 2K tune: `Tune_ISDB_S_OfdmStandby`
    /// (`tuner:0x2097c`) accepts only dev `0x200/0x400/0x800`, so dev `0x02`
    /// falls through to "Tuner type Error" and the TS output stays enabled.
    /// That is why every gate so far has required a reboot. Cleanup only; this
    /// never runs before or during a measurement.
    pub fn quiesce_ts_output() -> io::Result<IsdbsTsExclusiveResult> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<IsdbsTsExclusiveResult> {
            let before = bus.read(0x10, 0xcc, 1)?[0];
            let pin = bus.read(0x10, 0x0b, 1)?[0];
            bus.write(0x10, 0x0b, &[pin & !0x01])?;
            bus.write(0x10, 0xcc, &[0x40])?;
            let after = bus.read(0x10, 0xcc, 1)?[0];
            let pin_after = bus.read(0x10, 0x0b, 1)?[0];
            if after != 0x40 || pin_after & 0x01 != 0 {
                return Err(io::Error::other(format!(
                    "quiesce readback mismatch: 0b={pin_after:#04x} cc={after:#04x}"
                )));
            }
            Ok(IsdbsTsExclusiveResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Remove only the stale ISDB-S3 TLV-output enable after the product has
    /// switched the same SC1501A unit to ISDB-S.
    ///
    /// The vendor's system-1 branch sets reg 0xcc bit 7 but never clears the
    /// system-13 branch's bit 6. A green -> red -> BS transition therefore
    /// measures 0xc0. Refuse every state except the fully measured locked
    /// ISDB-S signature, then perform one compare-before-write byte update
    /// under the product's process-shared I2C semaphore.
    pub fn make_isdbs_ts_output_exclusive() -> io::Result<IsdbsTsExclusiveResult> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<IsdbsTsExclusiveResult> {
            let lock = bus.read(0x12, 0x5e, 1)?[0];
            let ts_pin = bus.read(0x10, 0x0b, 1)?[0];
            let before = bus.read(0x10, 0xcc, 1)?[0];
            if lock & 0x20 == 0 || ts_pin != 0x01 || before != 0xc0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refuse non-measured ISDB-S signature: lock={lock:#04x} reg0b={ts_pin:#04x} regcc={before:#04x}"
                    ),
                ));
            }

            // Exact equivalent of vendor maskwrite(reg=0xcc, mask=0x40,
            // value=0), with the already-compared old byte held under the
            // same bus semaphore. Bit 7 and every unrelated bit are retained.
            bus.write(0x10, 0xcc, &[before & !0x40])?;
            let after = bus.read(0x10, 0xcc, 1)?[0];
            if after != 0x80 {
                return Err(io::Error::other(format!(
                    "ISDB-S exclusive TS readback mismatch: {before:#04x} -> {after:#04x}"
                )));
            }
            Ok(IsdbsTsExclusiveResult { before, after })
        })();
        let release = guard.release();
        match (result, release) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }
    /// Vendor DMD_set_tsout uses reg0b bit0=0 for output flag bit2=1.
    /// The same flag selects TLV_SPI_8BIT in the S3 branch. Test the
    /// corresponding MPEG-TS parallel profile; the electrical meaning remains
    /// inferred from the vendor code rather than a public register map.
    pub fn experiment_ts_parallel(restore: bool) -> io::Result<()> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<()> {
            if bus.read(0x12, 0x5e, 1)?[0] & 0x20 == 0 {
                return Err(io::Error::other("ISDB-S is not locked"));
            }
            let (before, after) = if restore { (0x00, 0x01) } else { (0x01, 0x00) };
            for (bank, reg, value) in [
                (0x10,0x0b,before), (0x10,0x13,0x00), (0x10,0x16,0x00),
                (0x10,0xcc,0x80), (0x17,0x72,0x00),
            ] {
                if bus.read(bank,reg,1)?[0] != value {
                    return Err(io::Error::other("TS parallel profile: unexpected starting register"));
                }
            }
            bus.write(0x10,0x0b,&[after])?;
            if bus.read(0x10,0x0b,1)?[0] != after {
                bus.write(0x10,0x0b,&[before])?;
                return Err(io::Error::other("TS parallel readback mismatch; restored starting byte"));
            }
            Ok(())
        })();
        let release = guard.release();
        result.and(release)
    }

    /// Experimental forwarding of legacy TS into the known 1-bit SPI output
    /// stage. Values come from regset_TLV_SPI_1BIT; whether bank7:72 selects
    /// the required forwarding path is the hypothesis, not an established fact.
    /// No unknown bits, permanent configuration, or unbounded register sweep.
    pub fn experiment_spi_forward(restore: bool) -> io::Result<()> {
        let guard = I2c0Guard::attach_and_take()?;
        let bus = I2c0::open()?;
        let result = (|| -> io::Result<()> {
            if bus.read(0x12, 0x5e, 1)?[0] & 0x20 == 0 {
                return Err(io::Error::other("ISDB-S is not locked"));
            }
            let (old16, old72, new16, new72) = if restore {
                (0x21, 0x02, 0x00, 0x00)
            } else {
                (0x00, 0x00, 0x21, 0x02)
            };
            for (bank, reg, value) in [
                (0x10, 0x0b, 0x01), (0x10, 0x13, 0x00),
                (0x10, 0xcc, 0x80), (0x10, 0x16, old16),
                (0x17, 0x72, old72), (0x17, 0x7b, 0x67), (0x17, 0x7d, 0x81),
            ] {
                let got = bus.read(bank, reg, 1)?[0];
                if got != value {
                    return Err(io::Error::other(format!(
                        "SPI hypothesis refused: bank {bank:#x} reg {reg:#x} is {got:#x}, expected {value:#x}")));
                }
            }
            let writes = [(0x10, 0x16, new16, old16), (0x17, 0x72, new72, old72)];
            for &(bank, reg, new, _) in &writes {
                // On a transfer/readback failure, attempt to restore both
                // already-compared original bytes before releasing the lock.
                let changed = bus.write(bank, reg, &[new]).and_then(|_| {
                    if bus.read(bank, reg, 1)?[0] != new {
                        return Err(io::Error::other("SPI hypothesis readback mismatch"));
                    }
                    Ok(())
                });
                if let Err(error) = changed {
                    for &(b, r, _, old) in writes.iter().rev() {
                        if let Err(rollback) = bus.write(b, r, &[old]) {
                            eprintln!("SPI rollback failed: {rollback}");
                        }
                    }
                    return Err(error);
                }
            }
            Ok(())
        })();
        let release = guard.release();
        result.and(release)
    }

}

#[cfg(all(target_os = "linux", target_arch = "arm"))]
pub use arm_linux::{
    make_isdbs_ts_output_exclusive, quiesce_ts_output, read_isdbs_status, read_output_regs,
    select_tlv_output_exclusive, set_tsout_framing, set_tsout_interface, snapshot_watchlist,
    standby_isdbt, tune, experiment_spi_forward, experiment_ts_parallel,
    read_isdbs_tmcc, select_isdbs_relative_zero,
};

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn select_isdbs_relative_zero(_restore: bool) -> std::io::Result<IsdbsTsExclusiveResult> {
    Err(std::io::Error::other("relative-TS0 diagnostic requires the ARM device build"))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn read_isdbs_tmcc() -> std::io::Result<IsdbsTmcc> {
    Err(std::io::Error::other("TMCC monitor requires the ARM device build"))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn set_tsout_interface(_target: TsoutInterface) -> std::io::Result<TsoutRestoreResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A output-interface write is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn snapshot_watchlist() -> std::io::Result<Vec<(u8, u8, u8, u8)>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A snapshot is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn select_tlv_output_exclusive() -> std::io::Result<IsdbsTsExclusiveResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A selector repair is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn quiesce_ts_output() -> std::io::Result<IsdbsTsExclusiveResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A TS-output quiesce is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn set_tsout_framing(_target: TsoutTarget) -> std::io::Result<TsoutRestoreResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A output-framing write is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn read_output_regs() -> std::io::Result<DmdOutputRegs> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SC1501A output-register read is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn tune(_writes: &[Write]) -> std::io::Result<TuneResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "GR live I2C is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn read_isdbs_status() -> std::io::Result<IsdbsStatus> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ISDB-S status read is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn make_isdbs_ts_output_exclusive() -> std::io::Result<IsdbsTsExclusiveResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ISDB-S exclusive TS repair is available only in the ARM device build",
    ))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn standby_isdbt() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "GR standby is available only in the ARM device build",
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn tmcc_tsid_fields_extract_all_eight_unaligned_msb_first_values() {
        let expected = [0x4010u16, 0x4011, 0x4012, 0xffff, 0, 1, 0x8000, 0xabcd];
        let mut bytes = [0u8; 40];
        // Fixed packed vector: 189 leading zero bits, eight big-endian
        // identifiers, three trailing zero bits. Do not reuse the extractor
        // indexing formula to manufacture this fixture.
        bytes[23..].copy_from_slice(&[
            0x02, 0x00, 0x82, 0x00, 0x8a, 0x00, 0x97, 0xff, 0xf8,
            0x00, 0x00, 0x00, 0x0c, 0x00, 0x05, 0x5e, 0x68,
        ]);
        assert_eq!(super::tmcc_tsid_fields(&bytes), expected);
    }
    use super::*;

    #[test]
    fn product_lock_predicate_is_exact() {
        assert!(is_isdbt_locked(0x09));
        assert!(is_isdbt_locked(0x0f));
        assert!(!is_isdbt_locked(0x08));
        assert!(!is_isdbt_locked(0x19));
        assert!(!is_isdbt_locked(0x89));
    }

    #[test]
    fn exclusive_ts_transition_clears_only_tlv_enable() {
        assert_eq!(0xc0 & !0x40, 0x80);
        assert_eq!(0xe5 & !0x40, 0xa5);
    }

    /// Byte-for-byte against `tuner` b4f6b070…:
    /// `regset_TLV_SPI_1BIT` (`.data 0x3c3d4`) writes b7 `73=15, 7b=67, 7d=81`;
    /// `DMD_set_tsout` case `0x0d` then writes b7 `73=03` (`tuner:0x27144`) and
    /// `74:75 = ctx[0xc68] = 0x0001` (`0x27176`/`0x271a4`).
    /// The vendor full base set (`.data 0x3b4c8`) holds `73=24, 75=00, 7b=64,
    /// 7d=01` for the same four registers.
    #[test]
    fn tsout_framing_constants_match_product_tables() {
        assert_eq!(TLV_SPI_1BIT_TSOUT, [(0x73, 0x03), (0x75, 0x01), (0x7b, 0x67), (0x7d, 0x81)]);
        assert_eq!(BASE_TSOUT, [(0x73, 0x24), (0x75, 0x00), (0x7b, 0x64), (0x7d, 0x01)]);
    }

    #[test]
    fn tsout_classifier_separates_the_two_product_states() {
        let residue = [(0x0a, 0x00), (0x73, 0x03), (0x74, 0x80), (0x75, 0x01), (0x7b, 0x67), (0x7d, 0x81)];
        let base = [(0x0a, 0x00), (0x73, 0x24), (0x74, 0x80), (0x75, 0x00), (0x7b, 0x64), (0x7d, 0x01)];
        let mixed = [(0x73, 0x03), (0x75, 0x00), (0x7b, 0x64), (0x7d, 0x01)];
        assert_eq!(classify_tsout(&residue), TsoutFraming::TlvSpi1Bit);
        assert_eq!(classify_tsout(&base), TsoutFraming::Base);
        assert_eq!(classify_tsout(&mixed), TsoutFraming::Other);
    }

    /// The two 8-bit and 2-bit TLV widths must never be silently accepted as
    /// the 1-bit residue: `ctx[0xc28] & 4` is clear on this unit (measured
    /// `0x0b = 0x01` in the ISDB-S branch), so only `7b = 0x67` can occur.
    /// The two directions must be exact inverses, so a rollback can never
    /// install a mixture of the two states.
    /// `0x16` bit0 is the interface enable and bit5 the serial-mode select:
    /// set in TLV_SPI_1BIT/2BIT (0x21), bit0 only in TLV_SPI_8BIT (0x01), and
    /// both clear in every MPEG-TS standard table (0x00).
    #[test]
    fn serial_interface_constants_match_the_tlv_spi_table() {
        assert_eq!(SERIAL_1BIT_IFACE, [(0x13, 0x04), (0x16, 0x21), (0xcc, 0x80)]);
        assert_eq!(ISDBS_IFACE, [(0x13, 0x00), (0x16, 0x00), (0xcc, 0xc0)]);
        assert_eq!(0x21 & 0x01, 0x01, "TLV_SPI_1BIT enables the interface");
        assert_eq!(0x21 & 0x20, 0x20, "TLV_SPI_1BIT selects serial");
        assert_eq!(0x01 & 0x20, 0x00, "TLV_SPI_8BIT is the parallel width");
        assert_eq!(0x00 & 0x21, 0x00, "regset_ISDBS leaves both bits clear");
        // The content selector keeps bit7 (TS) and drops bit6 (TLV).
        assert_eq!(0xc0u8 & !0x40, 0x80);
    }

    #[test]
    fn interface_targets_are_inverses() {
        assert_eq!(TsoutInterface::Serial1Bit.want(), SERIAL_1BIT_IFACE);
        assert_eq!(TsoutInterface::Serial1Bit.expect_before(), ISDBS_IFACE);
        assert_eq!(TsoutInterface::IsdbsDefault.want(), ISDBS_IFACE);
        assert_eq!(TsoutInterface::IsdbsDefault.expect_before(), SERIAL_1BIT_IFACE);
    }

    /// Gate 5C4 must touch exactly one register, and it must not be one of the
    /// two content selectors: `0x0b` bit 0 and `0xcc` bit 6/7 stay as
    /// `DMD_set_tsout` left them.
    #[test]
    fn mode16_gate_is_a_single_non_content_register() {
        assert_eq!(TsoutInterface::Mode16Serial.want(), [(0x16, 0x21)]);
        assert_eq!(TsoutInterface::Mode16Serial.expect_before(), [(0x16, 0x00)]);
        assert_eq!(TsoutInterface::Mode16Isdbs.want(), [(0x16, 0x00)]);
        for &(reg, _) in TsoutInterface::Mode16Serial.want() {
            assert_ne!(reg, 0x0b);
            assert_ne!(reg, 0xcc);
            assert_ne!(reg, 0x13, "0x13 = 0x04 is not part of any working state");
        }
    }

    #[test]
    fn tsout_targets_are_inverses() {
        assert_eq!(TsoutTarget::Base.want(), BASE_TSOUT);
        assert_eq!(TsoutTarget::Base.expect_before(), TLV_SPI_1BIT_TSOUT);
        assert_eq!(TsoutTarget::TlvSpi1Bit.want(), TLV_SPI_1BIT_TSOUT);
        assert_eq!(TsoutTarget::TlvSpi1Bit.expect_before(), BASE_TSOUT);
        for (a, b) in TsoutTarget::Base.want().iter().zip(TsoutTarget::TlvSpi1Bit.expect_before()) {
            assert_eq!(*a, b);
        }
    }

    /// Derived from four product values: bit 0 of `0x7b` and bit 7 of `0x7d`
    /// are set in every `regset_TLV_SPI_*` table (1bit 0x67, 2bit 0x63,
    /// 8bit 0x65 / all 0x81) and clear in the vendor full base set
    /// (0x64 / 0x01). Restoring base therefore turns the TLV-SPI output stage
    /// off rather than merely changing a width field.
    #[test]
    fn base_clears_the_tlv_spi_enable_bits() {
        let base: std::collections::BTreeMap<u8, u8> = BASE_TSOUT.iter().copied().collect();
        let tlv: std::collections::BTreeMap<u8, u8> = TLV_SPI_1BIT_TSOUT.iter().copied().collect();
        assert_eq!(tlv[&0x7b] & 0x01, 0x01);
        assert_eq!(base[&0x7b] & 0x01, 0x00);
        assert_eq!(tlv[&0x7d] & 0x80, 0x80);
        assert_eq!(base[&0x7d] & 0x80, 0x00);
        for width in [0x67u8, 0x63, 0x65] {
            assert_eq!(width & 0x01, 0x01, "every TLV_SPI width sets 0x7b bit0");
        }
    }

    #[test]
    fn other_tlv_widths_are_not_the_measured_residue() {
        for width in [0x63u8, 0x65] {
            let sample = [(0x73, 0x03), (0x75, 0x01), (0x7b, width), (0x7d, 0x81)];
            assert_eq!(classify_tsout(&sample), TsoutFraming::Other);
        }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn experiment_spi_forward(_restore: bool) -> std::io::Result<()> {
    Err(std::io::Error::other("SPI experiment is ARM-only"))
}

#[cfg(not(all(target_os = "linux", target_arch = "arm")))]
pub fn experiment_ts_parallel(_restore: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "ARM device only"))
}

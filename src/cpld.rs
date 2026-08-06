//! Minimal access to the tuner-board CPLD over `/dev/spidev0.1`.
//!
//! The product's `libcpld` writes `{ 0x02, reg, value }` in one transfer.  A
//! read is two transfers in one SPI message: TX `{ 0x03, reg }`, then RX one
//! byte while chip select remains asserted.  Keep this module deliberately
//! narrow: callers may only read the five documented registers, and writes
//! are limited to the XCode6 source selector with known-safe values.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

use crate::sys;

pub const DEV_PATH: &str = "/dev/spidev0.1";

const SPI_IOC_MESSAGE_1: i32 = 0x4020_6b00;
const SPI_IOC_MESSAGE_2: i32 = 0x4040_6b00;

const BITS_PER_WORD: u8 = 8;
const SPEED_HZ: u32 = 2_048_000;

/// X6TSISL1 values used by the product or obtained by changing exactly one
/// documented source nibble.  No other CPLD writes are accepted.
const SAFE_X6TSISL1: [u8; 5] = [0x22, 0x20, 0x02, 0x21, 0x12];

#[repr(C)]
struct SpiIocTransfer {
    tx_buf: u64,
    rx_buf: u64,
    len: u32,
    speed_hz: u32,
    delay_usecs: u16,
    bits_per_word: u8,
    cs_change: u8,
    pad: u32,
}

pub struct Cpld {
    file: File,
}

impl Cpld {
    pub fn open(path: &str, writable: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        Ok(Self { file })
    }

    fn write_transfer(&self, tx: [u8; 3]) -> io::Result<()> {
        let mut transfer = SpiIocTransfer {
            tx_buf: tx.as_ptr() as usize as u64,
            rx_buf: 0,
            len: 3,
            speed_hz: SPEED_HZ,
            delay_usecs: 0,
            bits_per_word: BITS_PER_WORD,
            cs_change: 0,
            pad: 0,
        };
        debug_assert_eq!(std::mem::size_of::<SpiIocTransfer>(), 32);
        unsafe {
            sys::ioctl_ptr(
                self.file.as_raw_fd(),
                SPI_IOC_MESSAGE_1,
                (&mut transfer as *mut SpiIocTransfer).cast::<c_void>(),
            )?;
        }
        Ok(())
    }

    pub fn read_reg(&self, reg: u8) -> io::Result<u8> {
        if reg > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only documented CPLD registers 0..4 may be read",
            ));
        }
        // Match libcpld's Csi_Read exactly.  The command is a two-byte TX
        // transfer followed, under the same asserted CS, by a one-byte RX
        // transfer.  A single three-byte full-duplex transfer returns 0xff on
        // this controller and is not equivalent.
        let tx = [0x03u8, reg];
        let mut value = 0u8;
        let mut transfers = [
            SpiIocTransfer {
                tx_buf: tx.as_ptr() as usize as u64,
                rx_buf: 0,
                len: 2,
                speed_hz: SPEED_HZ,
                delay_usecs: 0,
                bits_per_word: BITS_PER_WORD,
                cs_change: 0,
                pad: 0,
            },
            SpiIocTransfer {
                tx_buf: 0,
                rx_buf: (&mut value as *mut u8) as usize as u64,
                len: 1,
                speed_hz: SPEED_HZ,
                delay_usecs: 0,
                bits_per_word: BITS_PER_WORD,
                cs_change: 0,
                pad: 0,
            },
        ];
        unsafe {
            sys::ioctl_ptr(
                self.file.as_raw_fd(),
                SPI_IOC_MESSAGE_2,
                transfers.as_mut_ptr().cast::<c_void>(),
            )?;
        }
        Ok(value)
    }

    /// Compare-and-write the XCode6 input selector.  The expected-value check
    /// prevents a stale experiment from overwriting an unrecognised board
    /// state.  No default selector is assumed; the caller must supply a
    /// measured expected value and a separately justified target.
    pub fn write_x6tsisl1(&self, expected: u8, value: u8) -> io::Result<()> {
        if !SAFE_X6TSISL1.contains(&value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing undocumented X6TSISL1 value",
            ));
        }
        let actual = self.read_reg(2)?;
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("X6TSISL1 changed: expected {expected:#04x}, found {actual:#04x}"),
            ));
        }
        self.write_transfer([0x02, 2, value])?;
        let readback = self.read_reg(2)?;
        if readback != value {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("X6TSISL1 readback mismatch: wrote {value:#04x}, read {readback:#04x}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spi_transfer_matches_uapi_layout() {
        assert_eq!(std::mem::size_of::<SpiIocTransfer>(), 32);
    }

    #[test]
    fn known_safe_selector_values_are_explicit() {
        assert_eq!(SAFE_X6TSISL1, [0x22, 0x20, 0x02, 0x21, 0x12]);
    }
}

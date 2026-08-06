//! Gate 5D read-only state snapshot: SC1501A watchlist registers, XCode6 TSI
//! status, and the `tuner` process runtime flags/context.
//!
//! This module never writes anything. Every I2C access happens under the
//! product's own shared bus semaphore, and every register is read twice so the
//! report can separate a stable value from one that moves on its own.
//!
//! Register and memory watchlists were generated during the frozen BS/CS 2K
//! investigation (docs/LIMITATIONS.md),
//! re-derived independently from `tuner` b4f6b070…:
//!   * `0x3c748` Recommend_DIFF one-shot flags — `set_system` reads at
//!     `0x22704/0x22706` and writes at `0x22720/0x22722` (PC-relative).
//!   * `0x3c758` ISDB-S3 PSEQ one-shot flags — read `0x22798/0x2279a`,
//!     write `0x227bc/0x227be`.
//!   * `0x3c940` S3 context array, 3 entries of stride `0xc78` — GOT `0x37370`.
//! The ELF is ET_EXEC linked at base `0x8000`, so these are absolute virtual
//! addresses; a caller must still confirm the mapping in `/proc/<pid>/maps`.

/// `(bank, register, categories)` — the Gate 5D read-only watchlist.
pub const WATCHLIST: [(u8, u8, &str); 42] = [
    (0x00, 0x00, "system_table_delta"),
    (0x00, 0x01, "system_table_delta"),
    (0x00, 0x03, "system_table_delta"),
    (0x00, 0x05, "system_table_delta"),
    (0x00, 0x0b, "ts_tlv_output"),
    (0x00, 0x0c, "ts_tlv_output"),
    (0x00, 0x13, "ts_tlv_output"),
    (0x00, 0x16, "system_table_delta;ts_tlv_output"),
    (0x00, 0x18, "system_table_delta;ts_tlv_output"),
    (0x00, 0x1c, "system_table_delta"),
    (0x00, 0xcc, "highest_priority;ts_tlv_output"),
    (0x00, 0xd9, "ts_tlv_output"),
    (0x00, 0xf8, "pseq_status"),
    (0x02, 0x11, "system_table_delta"),
    (0x02, 0x5e, "demod_lock"),
    (0x04, 0x05, "common_bank4_control"),
    (0x04, 0x06, "common_bank4_control"),
    (0x04, 0x07, "common_bank4_control"),
    (0x04, 0x08, "common_bank4_control"),
    (0x04, 0x09, "common_bank4_control"),
    (0x04, 0x0a, "common_bank4_control"),
    (0x04, 0x0b, "common_bank4_control"),
    (0x04, 0x0d, "common_bank4_control"),
    (0x04, 0x0f, "common_bank4_control"),
    (0x04, 0x21, "common_bank4_control"),
    (0x04, 0x22, "common_bank4_control"),
    (0x04, 0x79, "common_bank4_control"),
    (0x04, 0x7a, "common_bank4_control"),
    (0x04, 0x7b, "common_bank4_control"),
    (0x04, 0x81, "common_bank4_control"),
    (0x04, 0xc9, "common_bank4_control"),
    (0x05, 0x78, "s3_post_setup"),
    (0x05, 0xe5, "system_table_delta"),
    (0x07, 0x0a, "ts_tlv_output"),
    (0x07, 0x72, "ts_tlv_output"),
    (0x07, 0x73, "ts_tlv_output"),
    (0x07, 0x74, "ts_tlv_output"),
    (0x07, 0x75, "ts_tlv_output"),
    (0x07, 0x76, "ts_tlv_output"),
    (0x07, 0x7b, "ts_tlv_output"),
    (0x07, 0x7d, "ts_tlv_output"),
    (0x07, 0x8d, "ts_tlv_output"),
];

/// `tuner` `.bss` addresses. Absolute: the ELF is ET_EXEC, link base `0x8000`.
pub const RECOMMEND_FLAGS: u32 = 0x0003_c748;
pub const PSEQ_FLAGS: u32 = 0x0003_c758;
pub const CONTEXT_BASE: u32 = 0x0003_c940;
pub const CONTEXT_STRIDE: u32 = 0x0c78;

/// Byte offsets inside one S3 context entry that the plan asks to record.
pub const CONTEXT_FIELDS: [(u32, u32, &str); 6] = [
    (0x000, 4, "unit"),
    (0x004, 4, "system"),
    (0x008, 8, "frequency_scaled"),
    (0x018, 8, "reference_clock"),
    (0xc28, 4, "output_mode_flags"),
    (0xc68, 2, "tlv_word"),
];

/// CXD2868ER (RF front-end, "SUT_PJ784") 7-bit I2C address. `tuner` addresses it
/// as 8-bit `0xC0`.
pub const RF_ADDR: u8 = 0x60;

/// RF registers that `x_tune_isdbs` (`tuner:0x1c7b0-0x1ca38`) writes, plus the
/// ISDB-T standby set the GR path uses. All of these are configuration the
/// vendor writes itself; none is a documented clear-on-read status register.
///
/// The `0x06` burst is the one place where the recovered sequence differs by
/// system: ISDB-S writes `{03,80,70,1e,02,24}` and ISDB-S3 `{03,00,b4,78,08,30}`.
/// If a 2K takeover leaves the front-end on the ISDB-S values, ISDB-S3 can never
/// lock again — which is what the box does.
pub const RF_WATCHLIST: [(u8, &str); 27] = [
    (0x01, "tune_seq"),
    (0x04, "pll_latch"),
    (0x05, "pll_latch"),
    (0x06, "system_burst"),
    (0x07, "system_burst"),
    (0x08, "system_burst"),
    (0x09, "system_burst"),
    (0x0a, "system_burst"),
    (0x0b, "system_burst"),
    (0x0c, "tune_seq"),
    (0x0d, "tune_seq"),
    (0x0e, "tune_seq"),
    (0x0f, "tune_seq"),
    (0x10, "pll_n"),
    (0x11, "pll_n"),
    (0x12, "pll_n"),
    (0x13, "pll_n"),
    (0x14, "pll_n"),
    (0x15, "tune_seq"),
    (0x40, "tune_seq"),
    (0x41, "tune_seq"),
    (0x43, "tune_seq"),
    (0x45, "tune_seq"),
    (0x48, "tune_seq"),
    (0x6a, "tune_seq"),
    (0x6b, "tune_seq"),
    (0x75, "tune_seq"),
];

/// SC1501A bank `n` lives at 7-bit I2C address `unit * 32 + 0x10 + n`
/// (`tuner:0x2623a-0x2624e`). This box is `unit = 0`.
pub fn bank_addr(bank: u8) -> u8 {
    0x10 + bank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_addresses_match_the_vendor_formula() {
        assert_eq!(bank_addr(0), 0x10);
        assert_eq!(bank_addr(2), 0x12);
        assert_eq!(bank_addr(4), 0x14);
        assert_eq!(bank_addr(5), 0x15);
        assert_eq!(bank_addr(7), 0x17);
    }

    #[test]
    fn watchlist_is_sorted_deduplicated_and_complete() {
        assert_eq!(WATCHLIST.len(), 42);
        for pair in WATCHLIST.windows(2) {
            let (b0, r0, _) = pair[0];
            let (b1, r1, _) = pair[1];
            assert!((b0, r0) < (b1, r1), "watchlist must be sorted and unique");
        }
        // The one register the plan calls highest priority must be present.
        assert!(WATCHLIST
            .iter()
            .any(|&(b, r, c)| b == 0 && r == 0xcc && c.contains("highest_priority")));
    }

    #[test]
    fn runtime_addresses_match_the_independently_derived_values() {
        assert_eq!(RECOMMEND_FLAGS, 0x3c748);
        assert_eq!(PSEQ_FLAGS, 0x3c758);
        assert_eq!(CONTEXT_BASE, 0x3c940);
        assert_eq!(CONTEXT_STRIDE, 0x0c78);
        // dev 0x200/0x400/0x800 map to context index 0/1/2.
        assert_eq!(CONTEXT_BASE + 2 * CONTEXT_STRIDE, 0x3e230);
    }

    #[test]
    fn context_fields_stay_inside_one_stride() {
        for (off, size, _) in CONTEXT_FIELDS {
            assert!(off + size <= CONTEXT_STRIDE, "field {off:#x} escapes the stride");
        }
    }
}

/// Read-only access to the live `tuner` process image.
///
/// The plan's memory Gates (A2/A3) may only run when the mapping is exactly
/// where the ET_EXEC image says it is, so this exposes the maps text as well as
/// the values and lets the caller refuse on a mismatch.
#[cfg(target_os = "linux")]
pub mod procmem {
    use std::fs;
    use std::io;

    /// Find the pid whose `/proc/<pid>/comm` is exactly `name`.
    pub fn find_pid(name: &str) -> io::Result<u32> {
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let file = entry.file_name();
            let text = file.to_string_lossy();
            if !text.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let comm = match fs::read_to_string(format!("/proc/{text}/comm")) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if comm.trim() == name {
                return text.parse::<u32>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "bad pid")
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no process named {name}"),
        ))
    }

    pub fn maps(pid: u32) -> io::Result<String> {
        fs::read_to_string(format!("/proc/{pid}/maps"))
    }

    /// True when some mapping in `maps` covers `addr` and is writable.
    pub fn is_writable(maps: &str, addr: u32) -> bool {
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let range = match parts.next() {
                Some(r) => r,
                None => continue,
            };
            let perms = parts.next().unwrap_or("");
            let (lo, hi) = match range.split_once('-') {
                Some((a, b)) => (a, b),
                None => continue,
            };
            let (lo, hi) = match (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            };
            if (lo..hi).contains(&u64::from(addr)) {
                return perms.contains('w');
            }
        }
        false
    }

    /// `pread` `len` bytes at `addr` from `/proc/<pid>/mem`. Read-only.
    pub fn read(pid: u32, addr: u32, len: usize) -> io::Result<Vec<u8>> {
        use std::os::fd::AsRawFd;
        // musl's off_t is 64-bit on every arch, so `pread` is the portable
        // entry point here; `pread64` is a glibc-only alias and does not exist
        // in the static musl the device build links against.
        extern "C" {
            fn pread(fd: i32, buf: *mut core::ffi::c_void, count: usize, off: i64) -> isize;
        }
        let file = fs::File::open(format!("/proc/{pid}/mem"))?;
        let mut out = vec![0u8; len];
        let mut done = 0usize;
        while done < len {
            // SAFETY: `out` owns `len` bytes; the offset is a plain address.
            let n = unsafe {
                pread(
                    file.as_raw_fd(),
                    out[done..].as_mut_ptr().cast(),
                    len - done,
                    i64::from(addr) + done as i64,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("short read at {addr:#x}"),
                ));
            }
            done += n as usize;
        }
        Ok(out)
    }
}

/// Host-build stub so the CLI compiles for offline tests. The device build
/// always takes the Linux implementation above.
#[cfg(not(target_os = "linux"))]
pub mod procmem {
    use std::io;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "/proc is available only in the device build",
        ))
    }

    pub fn find_pid(_name: &str) -> io::Result<u32> {
        unsupported()
    }
    pub fn maps(_pid: u32) -> io::Result<String> {
        unsupported()
    }
    pub fn read(_pid: u32, _addr: u32, _len: usize) -> io::Result<Vec<u8>> {
        unsupported()
    }
    /// Kept identical to the Linux version so the parser stays testable offline.
    pub fn is_writable(maps: &str, addr: u32) -> bool {
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let range = match parts.next() {
                Some(r) => r,
                None => continue,
            };
            let perms = parts.next().unwrap_or("");
            let (lo, hi) = match range.split_once('-') {
                Some((a, b)) => (a, b),
                None => continue,
            };
            let (lo, hi) = match (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            };
            if (lo..hi).contains(&u64::from(addr)) {
                return perms.contains('w');
            }
        }
        false
    }
}

#[cfg(test)]
mod maps_tests {
    /// A real `tuner` maps line shape: the ET_EXEC data/bss segment must come
    /// back writable, and a read-only text mapping must not.
    #[test]
    fn writable_lookup_matches_maps_permissions() {
        let maps = "\
00008000-00037000 r-xp 00000000 b3:03 1234 /usr/bin/tuner
00037000-0003f000 rw-p 0002f000 b3:03 1234 /usr/bin/tuner
b6f00000-b6f10000 r--p 00000000 00:00 0
";
        assert!(super::procmem::is_writable(maps, super::RECOMMEND_FLAGS));
        assert!(super::procmem::is_writable(maps, super::PSEQ_FLAGS));
        assert!(super::procmem::is_writable(maps, super::CONTEXT_BASE));
        assert!(!super::procmem::is_writable(maps, 0x9be8));
        assert!(!super::procmem::is_writable(maps, 0x40000000));
    }
}

/// Gate 5D-A2/A3: clear one `set_system` one-shot flag in the live `tuner`
/// process so the vendor's own regset replay happens on the next tune.
///
/// This is the only write this module can perform, it is exactly four bytes,
/// and every precondition is checked first:
///   * the process is named `tuner` and its executable hashes to the analysed
///     binary (checked by the caller, which has the hash),
///   * `/proc/<pid>/maps` shows a writable mapping covering the address,
///   * the current value is `1` (the one-shot has been consumed).
///
/// `set_system` reads and writes these tables itself, so restoring the flag to
/// zero makes SHARP's own `regset_PSEQ_ISDBS3` / `regset_Recommend_DIFF` run
/// again. No register table is reproduced here.
#[cfg(target_os = "linux")]
pub mod oneshot {
    use super::procmem;
    use std::fs;
    use std::io;

    pub fn clear(pid: u32, addr: u32) -> io::Result<(u32, u32)> {
        let maps = procmem::maps(pid)?;
        if !procmem::is_writable(&maps, addr) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{addr:#x} is not inside a writable mapping of pid {pid}"),
            ));
        }
        let before = procmem::read(pid, addr, 4)?;
        let old = u32::from_le_bytes([before[0], before[1], before[2], before[3]]);
        if old != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refuse: one-shot flag at {addr:#x} is {old:#x}, expected 1"),
            ));
        }
        write(pid, addr, &0u32.to_le_bytes())?;
        let after = procmem::read(pid, addr, 4)?;
        let new = u32::from_le_bytes([after[0], after[1], after[2], after[3]]);
        if new != 0 {
            return Err(io::Error::other(format!(
                "flag readback {new:#x} after writing 0"
            )));
        }
        Ok((old, new))
    }

    fn write(pid: u32, addr: u32, bytes: &[u8]) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn pwrite(fd: i32, buf: *const core::ffi::c_void, count: usize, off: i64) -> isize;
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{pid}/mem"))?;
        // SAFETY: `bytes` is a valid slice; the offset is a plain address.
        let n = unsafe {
            pwrite(
                file.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                i64::from(addr),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write at {addr:#x}: {n}/{}", bytes.len()),
            ));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub mod oneshot {
    use std::io;
    pub fn clear(_pid: u32, _addr: u32) -> io::Result<(u32, u32)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "one-shot flag clear is available only in the device build",
        ))
    }
}

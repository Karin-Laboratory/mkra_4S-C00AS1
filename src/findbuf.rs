//! Finding real buffer handles without guessing.
//!
//! Guessing is not an option: `xc_sbuf_create_read_func` turns the handle into
//! a pointer with `p2uv_base` and dereferences it *before* validating, so a
//! wrong value oopses the driver (proved on the unit, 2026-07-31).
//!
//! But the driver told us what a valid handle looks like. `buf_check_handle`
//! converts the handle and then tests one thing:
//!
//! ```text
//! buf_check_handle:
//!     bl   p2uv_base            ; handle -> object pointer
//!     ldr  r2, [r0, #0x40]
//!     movw r3, #0x5a5a
//!     movt r3, #0x5a5a
//!     cmp  r2, r3               ; magic 0x5A5A5A5A at object + 0x40
//!     bne  invalid              ; -> 0xFFFFFC02 (-1022), an error we saw
//! ```
//!
//! The handle is an address into physical RAM, and this kernel is built with
//! `CONFIG_STRICT_DEVMEM` unset, so `/dev/mem` can read that memory. We can
//! therefore apply the driver's own validity test from user space, read-only,
//! and only ever hand the driver a handle we have already proved is a buffer.
//!
//! False positives are still possible — `0x5A5A5A5A` is a popular filler — so
//! the candidates are intersected with the 32-bit values actually held by the
//! product processes that own the buffers. A value that is both a valid buffer
//! object and known to `adsb`/`abs` is very unlikely to be an accident.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// `[obj + 0x40] == BUF_MAGIC` is exactly what `buf_check_handle` tests.
pub const BUF_MAGIC: u32 = 0x5A5A_5A5A;
pub const MAGIC_OFFSET: u64 = 0x40;

/// Stop collecting candidates past this many, so a filler-heavy image cannot
/// exhaust a box with 300 MB free.
const MAX_CANDIDATES: usize = 200_000;

const CHUNK: usize = 1 << 20;

/// A physical memory range, from `/proc/iomem`.
#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub start: u64,
    pub end: u64, // inclusive, as /proc/iomem reports it
}

/// Parse the `System RAM` ranges out of `/proc/iomem`.
pub fn system_ram(iomem: &str) -> Vec<Range> {
    let mut out = Vec::new();
    for line in iomem.lines() {
        let Some((range, name)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != "System RAM" {
            continue;
        }
        let Some((a, b)) = range.trim().split_once('-') else {
            continue;
        };
        if let (Ok(start), Ok(end)) = (u64::from_str_radix(a.trim(), 16), u64::from_str_radix(b.trim(), 16))
        {
            out.push(Range { start, end });
        }
    }
    out
}

pub struct ScanReport {
    pub candidates: Vec<u32>,
    pub bytes_scanned: u64,
    pub truncated: bool,
    pub unreadable: Vec<(u64, String)>,
}

/// Scan physical RAM for buffer objects and return their handles.
///
/// A handle is the address of the object, and the magic sits `MAGIC_OFFSET`
/// into it, so a magic word found at physical `p` implies handle `p - 0x40`.
pub fn scan_physical(dev_mem: &str, ranges: &[Range]) -> io::Result<ScanReport> {
    let mut mem = File::open(dev_mem)?;
    let mut buf = vec![0u8; CHUNK];
    let mut report = ScanReport {
        candidates: Vec::new(),
        bytes_scanned: 0,
        truncated: false,
        unreadable: Vec::new(),
    };

    for r in ranges {
        let mut pos = r.start;
        while pos <= r.end {
            let want = CHUNK.min((r.end - pos + 1) as usize);
            if mem.seek(SeekFrom::Start(pos)).is_err() {
                report.unreadable.push((pos, "seek failed".into()));
                break;
            }
            match mem.read_exact(&mut buf[..want]) {
                Ok(()) => {}
                Err(e) => {
                    // Holes are normal: reserved windows inside a RAM range are
                    // not mapped for /dev/mem. Record and skip rather than stop.
                    report.unreadable.push((pos, e.to_string()));
                    pos += want as u64;
                    continue;
                }
            }
            report.bytes_scanned += want as u64;

            let mut off = 0;
            while off + 4 <= want {
                let w = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
                if w == BUF_MAGIC {
                    let phys = pos + off as u64;
                    if phys >= MAGIC_OFFSET {
                        let handle = phys - MAGIC_OFFSET;
                        if handle <= u32::MAX as u64 {
                            if report.candidates.len() >= MAX_CANDIDATES {
                                report.truncated = true;
                            } else {
                                report.candidates.push(handle as u32);
                            }
                        }
                    }
                }
                off += 4;
            }
            pos += want as u64;
        }
    }

    report.candidates.sort_unstable();
    report.candidates.dedup();
    Ok(report)
}

/// Which of `candidates` appear as 32-bit words in a process's writable memory.
///
/// Being referenced by the process that owns the pipeline is the second half of
/// the evidence; `candidates` must be sorted.
pub fn referenced_by(pid: u32, candidates: &[u32]) -> io::Result<Vec<u32>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let mut mem = File::open(format!("/proc/{pid}/mem"))?;
    let mut hits = Vec::new();

    for line in maps.lines() {
        let mut it = line.split_whitespace();
        let (Some(range), Some(perms)) = (it.next(), it.next()) else {
            continue;
        };
        if !perms.starts_with("rw") {
            continue;
        }
        // Anonymous, [heap], library .data/.bss and shared segments all count:
        // a handle can be parked in any of them, and the earlier heap-only scan
        // was the reason `abs` looked as if it held nothing.
        //
        // Device mappings are the exception. Reading MMIO through
        // /proc/<pid>/mem would touch hardware registers, which is not a
        // read-only operation in any useful sense.
        let path = line.split_whitespace().nth(5).unwrap_or("");
        if path.starts_with("/dev/") || path == "[vvar]" || path == "[vsyscall]" {
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
            continue;
        }
        let mut off = 0;
        while off + 4 <= buf.len() {
            let w = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            if candidates.binary_search(&w).is_ok() {
                hits.push(w);
            }
            off += 4;
        }
    }
    hits.sort_unstable();
    hits.dedup();
    Ok(hits)
}

/// Whether a handle passes the driver's own validity test, checked read-only.
///
/// This is the gate that must sit in front of every `create_reader`. The
/// driver dereferences the object *before* calling `buf_check_handle`, so a
/// handle that fails here can oops the kernel; a handle that passes here is
/// both a real buffer and backed by memory we have already read, which is what
/// makes the subsequent deref safe.
pub enum Validity {
    /// Magic present: the driver would accept this handle.
    Valid,
    /// Readable, but no magic. `create_reader` would return -1022 at best and
    /// oops at worst.
    NotABuffer(u32),
    /// Outside the range `/dev/mem` will serve. Handles at or above
    /// 0x7F000000 take `p2uv_base`'s windowed path into XCode6 memory, which
    /// this kernel refuses to map for /dev/mem, so they cannot be checked.
    Uncheckable(String),
}

pub fn validate(dev_mem: &str, handle: u32) -> Validity {
    match read_object(dev_mem, handle, (MAGIC_OFFSET as usize) + 4) {
        Ok(o) => {
            let i = MAGIC_OFFSET as usize;
            let magic = u32::from_le_bytes([o[i], o[i + 1], o[i + 2], o[i + 3]]);
            if magic == BUF_MAGIC {
                Validity::Valid
            } else {
                Validity::NotABuffer(magic)
            }
        }
        Err(e) => Validity::Uncheckable(e.to_string()),
    }
}

/// Read `len` bytes of the object a handle points at, for inspection.
pub fn read_object(dev_mem: &str, handle: u32, len: usize) -> io::Result<Vec<u8>> {
    let mut mem = File::open(dev_mem)?;
    mem.seek(SeekFrom::Start(handle as u64))?;
    let mut buf = vec![0u8; len];
    mem.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_ram_ranges_only() {
        // Shape of the real file on this unit.
        let iomem = "\
00000000-0000ffff : reserved
56000000-7f9fffff : System RAM
  56008000-566fffff : Kernel code
7fa00000-7fffffff : vixs
";
        let r = system_ram(iomem);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0x5600_0000);
        assert_eq!(r[0].end, 0x7f9f_ffff);
        // 666 MB, matching MemTotal.
        assert_eq!(r[0].end - r[0].start + 1, 0x29a0_0000);
    }

    #[test]
    fn ignores_malformed_lines() {
        assert!(system_ram("garbage\n:\nzz-yy : System RAM\n").is_empty());
        assert!(system_ram("").is_empty());
    }

    #[test]
    fn magic_and_offset_match_the_driver() {
        // buf_check_handle: movw r3,#0x5a5a / movt r3,#0x5a5a / ldr r2,[r0,#0x40]
        assert_eq!(BUF_MAGIC, 0x5A5A_5A5A);
        assert_eq!(MAGIC_OFFSET, 0x40);
    }

    #[test]
    fn scan_finds_a_planted_object_and_reports_its_handle() {
        // Stand in for /dev/mem with a file, and plant the magic where a real
        // buffer object would carry it.
        let path = std::env::temp_dir().join("mkra_fakemem.bin");
        let mut image = vec![0u8; 0x2000];
        let obj = 0x1000usize; // handle, relative to the range start
        image[obj + 0x40..obj + 0x44].copy_from_slice(&BUF_MAGIC.to_le_bytes());
        std::fs::write(&path, &image).unwrap();

        let r = scan_physical(
            path.to_str().unwrap(),
            &[Range { start: 0, end: image.len() as u64 - 1 }],
        )
        .unwrap();
        assert_eq!(r.candidates, vec![obj as u32]);
        assert!(!r.truncated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_ignores_magic_too_close_to_address_zero() {
        let path = std::env::temp_dir().join("mkra_fakemem2.bin");
        let mut image = vec![0u8; 0x100];
        // Magic at 0x10 would imply a handle of -0x30: must not be reported.
        image[0x10..0x14].copy_from_slice(&BUF_MAGIC.to_le_bytes());
        std::fs::write(&path, &image).unwrap();
        let r = scan_physical(
            path.to_str().unwrap(),
            &[Range { start: 0, end: image.len() as u64 - 1 }],
        )
        .unwrap();
        assert!(r.candidates.is_empty(), "got {:?}", r.candidates);
        let _ = std::fs::remove_file(&path);
    }
}

/// Read `len` bytes at a virtual address inside a running process.
///
/// Read-only inspection of live product state. Device mappings are refused:
/// reading MMIO through /proc/<pid>/mem would touch hardware registers.
pub fn peek_process(pid: u32, addr: u64, len: usize) -> io::Result<Vec<u8>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    for line in maps.lines() {
        let mut it = line.split_whitespace();
        let (Some(range), Some(_perms)) = (it.next(), it.next()) else {
            continue;
        };
        let path = line.split_whitespace().nth(5).unwrap_or("");
        let Some((lo, hi)) = range.split_once('-') else { continue };
        let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) else {
            continue;
        };
        if addr < lo || addr >= hi {
            continue;
        }
        if path.starts_with("/dev/") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{addr:#x} is inside the device mapping {path}; refusing to read MMIO"),
            ));
        }
        let mut mem = File::open(format!("/proc/{pid}/mem"))?;
        mem.seek(SeekFrom::Start(addr))?;
        let n = len.min((hi - addr) as usize);
        let mut buf = vec![0u8; n];
        mem.read_exact(&mut buf)?;
        return Ok(buf);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{addr:#x} is not mapped in pid {pid}"),
    ))
}

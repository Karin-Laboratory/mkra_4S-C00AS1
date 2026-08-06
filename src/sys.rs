//! The few libc entry points mkra needs, declared directly.
//!
//! Depending on the `libc` crate would be reasonable, but this build has to
//! stay reproducible offline on a machine with no vendored registry, and the
//! surface used here is six functions.  musl provides all of them.

#![allow(non_camel_case_types)]

use std::ffi::c_void;
use std::io;

pub type c_int = i32;
/// `long` on the device's 32-bit ARM ABI. Used for the System V `mtype` field,
/// which must match the kernel's `long` exactly.
pub type c_long = i32;
pub type size_t = usize;

extern "C" {
    /// musl declares `int ioctl(int, int, ...)`.  glibc uses `unsigned long`
    /// for the request; on ARM32 both are 32-bit and pass in the same register,
    /// so this declaration is also correct against the device's glibc if the
    /// binary is ever built dynamically.
    pub fn ioctl(fd: c_int, request: c_int, ...) -> c_int;

    /// Only the send half is declared. Replies from the tuner daemon go to a
    /// product-internal queue we have no way to claim, so mkra never receives
    /// (DESIGN.md §6.2).
    pub fn msgsnd(msqid: c_int, msgp: *const c_void, msgsz: size_t, msgflg: c_int) -> c_int;
}

pub const IPC_NOWAIT: c_int = 0o4000;

/// `ioctl(fd, request, arg)` returning the raw result, or the OS error.
///
/// # Safety
/// `arg` must point at a request object of exactly the size the driver's
/// handler copies in — see `xcdrv::Ioctl` for the recovered sizes.  A short
/// object means the driver reads past it.
pub unsafe fn ioctl_ptr(fd: c_int, request: c_int, arg: *mut c_void) -> io::Result<c_int> {
    let rc = ioctl(fd, request, arg);
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Issue one of xcdrv's legacy value-passed ioctls.
///
/// Several destroy handlers declare a four-byte request but interpret the
/// third syscall argument itself as the handle. Passing `&handle` is therefore
/// not equivalent: the driver sees the userspace pointer value as the handle.
pub unsafe fn ioctl_value(fd: c_int, request: c_int, arg: u32) -> io::Result<c_int> {
    let rc = ioctl(fd, request, arg);
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Read a whole file into a `String`, returning `None` for any failure.
///
/// Used for `/proc` and `/tmp` files where "absent" and "unreadable" call for
/// the same fallback and an error would only be noise.
pub fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Resident set size in bytes from `/proc/self/statm`, for `/api/status`.
pub fn rss_bytes() -> u64 {
    let Some(s) = read_text("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = s.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    pages * 4096
}

/// Seconds since boot from `/proc/uptime`, or `None` off-device.
pub fn uptime_secs() -> Option<f64> {
    read_text("/proc/uptime")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True when `[addr, addr+len)` is inside one of this process's own mappings.
///
/// `xc_sbuf_get_read` does not always hand back a physical address: for a
/// host-connected buffer the driver returns a pointer that is already mapped
/// into the caller (that is why `abs:remuxif_task` dereferences the reply
/// straight after `xc_buffer_host_read@0x2b895c` without mapping anything).
/// Deciding which it is by inspection is cheap, and it keeps a stray value
/// from being dereferenced.
pub fn user_mapping_contains(addr: u32, len: usize) -> bool {
    let Some(maps) = read_text("/proc/self/maps") else {
        return false;
    };
    let end = match (addr as u64).checked_add(len as u64) {
        Some(end) => end,
        None => return false,
    };
    for line in maps.lines() {
        let Some((range, rest)) = line.split_once(' ') else {
            continue;
        };
        // Only readable mappings can be copied out of.
        if !rest.starts_with('r') {
            continue;
        }
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) else {
            continue;
        };
        if u64::from(addr) >= lo && end <= hi {
            return true;
        }
    }
    false
}

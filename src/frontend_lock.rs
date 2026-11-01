//! Cooperative exclusion shared with the integrated mkra server.
use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
};

pub const LOCK_PATH: &str = "/tmp/mkra-tuner.lock";
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

#[derive(Debug)]
pub struct FrontendLock {
    _file: File,
}

impl FrontendLock {
    /// Acquire the same non-blocking lock used by the integrated `--bs2k`
    /// server. This must happen before opening the driver or touching tuning
    /// state. An idle API tuner is not proof that the server released it.
    pub fn acquire() -> io::Result<Self> {
        Self::acquire_at(LOCK_PATH)
    }

    fn acquire_at(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: flock only observes this valid open file descriptor; the
        // lock is released automatically when `file` is dropped.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another mkra process owns the tuner lock",
                ));
            }
            return Err(error);
        }
        Ok(Self { _file: file })
    }
}

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "mkra-frontend-lock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = path.to_str().unwrap();
        let first = FrontendLock::acquire_at(path).unwrap();
        assert_eq!(
            FrontendLock::acquire_at(path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        let second = FrontendLock::acquire_at(path).unwrap();
        drop(second);
        fs::remove_file(path).unwrap();
    }
}

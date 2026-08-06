//! Attachment-only implementation of the product's cross-process I2C lock.
//!
//! The shared object was created by glibc 2.15 as `struct new_sem`:
//! `u32 value, i32 private, u32 nwaiters`.  A statically linked musl binary
//! must not pass that object to musl's `sem_wait`, because `sem_t` is not a
//! cross-libc ABI.  These operations reproduce the glibc 2.15 ARM algorithm
//! directly and, critically, never call `IPC_CREAT` or `sem_init`.

#[cfg(all(target_os = "linux", target_arch = "arm"))]
mod arm_linux {
    use std::io;
    use std::os::raw::{c_int, c_long, c_void};
    use std::ptr;
    use std::sync::atomic::{fence, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    const I2C0_KEY: c_int = 0x3400_103b;
    const SHM_SIZE: usize = 16;
    const SHM_PERMS: c_int = 0o666;
    const SYS_FUTEX: c_long = 240;
    const FUTEX_WAIT: c_int = 0;
    const FUTEX_WAKE: c_int = 1;
    const EINTR: c_int = 4;
    const EAGAIN: c_int = 11;
    const ETIMEDOUT: c_int = 110;

    #[repr(C)]
    struct Timespec32 {
        tv_sec: i32,
        tv_nsec: i32,
    }

    #[repr(C)]
    struct Glibc215Sem {
        value: AtomicU32,
        private: i32,
        nwaiters: AtomicU32,
        _padding: u32,
    }

    extern "C" {
        fn shmget(key: c_int, size: usize, shmflg: c_int) -> c_int;
        fn shmat(shmid: c_int, shmaddr: *const c_void, shmflg: c_int) -> *mut c_void;
        fn shmdt(shmaddr: *const c_void) -> c_int;
        fn syscall(number: c_long, ...) -> c_long;
        fn __errno_location() -> *mut c_int;
    }

    fn errno() -> c_int {
        // SAFETY: musl exposes one valid thread-local errno pointer.
        unsafe { *__errno_location() }
    }

    unsafe fn futex_wait(value: *const AtomicU32) -> io::Result<()> {
        let wait = Timespec32 { tv_sec: 0, tv_nsec: 200_000_000 };
        let rc = syscall(
            SYS_FUTEX,
            value.cast::<u32>(),
            FUTEX_WAIT,
            0u32,
            &wait as *const Timespec32,
            ptr::null::<u32>(),
            0u32,
        );
        if rc == 0 {
            return Ok(());
        }
        match errno() {
            EINTR | EAGAIN | ETIMEDOUT => Ok(()),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }

    unsafe fn futex_wake(value: *const AtomicU32) -> io::Result<()> {
        let rc = syscall(
            SYS_FUTEX,
            value.cast::<u32>(),
            FUTEX_WAKE,
            1u32,
            ptr::null::<Timespec32>(),
            ptr::null::<u32>(),
            0u32,
        );
        if rc >= 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    fn try_take(sem: &Glibc215Sem) -> bool {
        let mut value = sem.value.load(Ordering::Relaxed);
        while value > 0 {
            match sem.value.compare_exchange_weak(
                value,
                value - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => value = actual,
            }
        }
        false
    }

    unsafe fn take(sem: &Glibc215Sem, timeout: Duration) -> io::Result<()> {
        if try_take(sem) {
            return Ok(());
        }
        sem.nwaiters.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        let result = loop {
            if try_take(sem) {
                break Ok(());
            }
            if Instant::now() >= deadline {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "product I2C0 semaphore stayed busy",
                ));
            }
            if let Err(e) = futex_wait(&sem.value) {
                break Err(e);
            }
        };
        sem.nwaiters.fetch_sub(1, Ordering::SeqCst);
        result
    }

    unsafe fn give(sem: &Glibc215Sem) -> io::Result<()> {
        let mut value = sem.value.load(Ordering::Relaxed);
        loop {
            if value == i32::MAX as u32 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "I2C semaphore overflow"));
            }
            match sem.value.compare_exchange_weak(
                value,
                value + 1,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => value = actual,
            }
        }
        fence(Ordering::SeqCst);
        if sem.nwaiters.load(Ordering::Relaxed) > 0 {
            futex_wake(&sem.value)?;
        }
        Ok(())
    }

    pub struct I2c0Guard {
        sem: *mut Glibc215Sem,
        held: bool,
    }

    impl I2c0Guard {
        /// Attach to the already-existing product semaphore and wait at most
        /// five seconds.  Absence is an error; this function cannot create or
        /// initialise shared state.
        pub fn attach_and_take() -> io::Result<Self> {
            // No IPC_CREAT: if the product has not prepared the semaphore,
            // fail closed rather than silently creating an incompatible one.
            let id = unsafe { shmget(I2C0_KEY, SHM_SIZE, SHM_PERMS) };
            if id < 0 {
                return Err(io::Error::last_os_error());
            }
            let addr = unsafe { shmat(id, ptr::null(), 0) };
            if addr as usize == usize::MAX {
                return Err(io::Error::last_os_error());
            }
            let sem = addr.cast::<Glibc215Sem>();
            // pshared sem_init stores private=0.  Validate both layout and a
            // sane value before any atomic modification.
            let private = unsafe { ptr::read_volatile(ptr::addr_of!((*sem).private)) };
            let value = unsafe { (*sem).value.load(Ordering::Relaxed) };
            if private != 0 || value > 1 {
                unsafe { shmdt(addr) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected product I2C semaphore private={private} value={value}"),
                ));
            }
            if let Err(e) = unsafe { take(&*sem, Duration::from_secs(5)) } {
                unsafe { shmdt(addr) };
                return Err(e);
            }
            Ok(Self { sem, held: true })
        }

        pub fn release(mut self) -> io::Result<()> {
            let result = unsafe { give(&*self.sem) };
            self.held = false;
            let sem = self.sem;
            std::mem::forget(self);
            unsafe { shmdt(sem.cast()) };
            result
        }
    }

    impl Drop for I2c0Guard {
        fn drop(&mut self) {
            if self.held {
                let _ = unsafe { give(&*self.sem) };
                self.held = false;
            }
            let _ = unsafe { shmdt(self.sem.cast()) };
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "arm"))]
pub use arm_linux::I2c0Guard;

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};
    use std::sync::atomic::AtomicU32;

    #[repr(C)]
    struct Layout {
        value: AtomicU32,
        private: i32,
        nwaiters: AtomicU32,
        padding: u32,
    }

    #[test]
    fn glibc_215_arm_sem_fits_the_vendor_16_byte_shm() {
        assert_eq!(size_of::<Layout>(), 16);
        assert_eq!(align_of::<Layout>(), 4);
        let sem = Layout {
            value: AtomicU32::new(1),
            private: 0,
            nwaiters: AtomicU32::new(0),
            padding: 0,
        };
        let base = &sem as *const Layout as usize;
        assert_eq!(&sem.private as *const i32 as usize - base, 4);
        assert_eq!(&sem.nwaiters as *const AtomicU32 as usize - base, 8);
    }
}

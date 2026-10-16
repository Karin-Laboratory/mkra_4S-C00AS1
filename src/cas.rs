//! Native serialized ICD transport, derived from the verified saved-TS probe.
//! Reuses the running card daemon; no init/reset/authentication or SPI helpers.
use std::os::{
    fd::AsRawFd,
    raw::{c_int, c_long, c_void},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    fs, io, mem,
    process::Command,
    thread,
    time::{Duration, Instant},
};
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static POISONED: AtomicBool = AtomicBool::new(false);
unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn msgsnd(id: c_int, msg: *const c_void, bytes: usize, flags: c_int) -> c_int;
    fn msgrcv(id: c_int, msg: *mut c_void, bytes: usize, kind: c_long, flags: c_int) -> isize;
    fn shmget(key: c_int, size: usize, flags: c_int) -> c_int;
    fn shmat(id: c_int, address: *const c_void, flags: c_int) -> *mut c_void;
    fn shmdt(address: *const c_void) -> c_int;
}
#[repr(C)]
struct Wire {
    kind: c_long,
    reserved: [u32; 3],
    head: u32,
    id: u32,
    len: u32,
    source: u32,
    payload: [u8; 64],
}
fn wire(id: u32) -> Wire {
    Wire {
        kind: 1,
        reserved: [0; 3],
        head: 0,
        id,
        len: 0,
        source: 0,
        payload: [0; 64],
    }
}
fn fail(s: impl Into<String>) -> io::Error {
    io::Error::other(s.into())
}
fn receive(reply: c_int, id: u32, len: u32, seconds: u64) -> io::Result<Wire> {
    let until = Instant::now() + Duration::from_secs(seconds);
    loop {
        let mut response = wire(0);
        let n = unsafe { msgrcv(reply, &mut response as *mut _ as *mut c_void, 92, 0, 0x800) };
        if n >= 0 {
            if n != 92 || response.kind != 1 || response.id != id || response.len != len {
                return Err(fail(
                    "unexpected reply shape; no additional request will be sent",
                ));
            }
            return Ok(response);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(42) && error.raw_os_error() != Some(4) {
            return Err(error);
        }
        if Instant::now() >= until {
            return Err(fail("response timed out; do not retry automatically"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn ecm_apdu(section: &[u8], p2: u8) -> io::Result<[u8; 255]> {
    if p2 != 2
        || section.len() < 13
        || section.len() > 255
        || ![0x82, 0x83].contains(&section[0])
        || (((section[1] as usize & 15) << 8) | section[2] as usize) + 3 != section.len()
    {
        return Err(fail("unsupported ECM section bounds"));
    }
    let mut crc = 0xffff_ffffu32;
    for &b in section {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
    }
    if crc != 0 {
        return Err(fail("ECM CRC mismatch"));
    }
    let mut apdu = [0u8; 255];
    // ADSB:76fb4..77006 removes the 8-byte PSI header and 4-byte CRC;
    // ADSB:76774..767cc wraps it as CLA90 / INS34 / P1=0 / Lc / data / Le0.
    // Card type2 and ordinary networks select P2=2 at ADSB:76828..76928.
    let body = &section[8..section.len() - 4];
    apdu[..5].copy_from_slice(&[0x90, 0x34, 0, p2, body.len() as u8]);
    apdu[5..5 + body.len()].copy_from_slice(body);
    Ok(apdu)
}
struct Shared(*mut u8);
impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            shmdt(self.0 as *const c_void);
        }
    }
}
fn queue(logical: u32) -> io::Result<c_int> {
    let rows = fs::read_to_string("/tmp/bdh_out_msgh.lst")?;
    let found: Vec<_> = rows
        .lines()
        .filter_map(|s| s.trim().split_once('-'))
        .filter(|(q, _)| q.parse::<u32>().ok() == Some(logical))
        .collect();
    if found.len() != 1 {
        return Err(fail("missing/ambiguous queue mapping"));
    }
    found[0].1.parse().map_err(|_| fail("invalid msqid"))
}
fn queue_state(id: c_int) -> io::Result<(u32, u32, u32)> {
    let rows = fs::read_to_string("/proc/sysvipc/msg")?;
    let fields: Vec<Vec<_>> = rows
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.get(1).and_then(|s| s.parse::<i32>().ok()) == Some(id))
        .collect();
    if fields.len() != 1 || fields[0].len() < 7 {
        return Err(fail("queue status unavailable"));
    }
    let f = &fields[0];
    Ok((
        f[3].parse().map_err(|_| fail("bad bytes"))?,
        f[4].parse().map_err(|_| fail("bad count"))?,
        f[6].parse().map_err(|_| fail("bad receiver"))?,
    ))
}
fn validate(request: c_int, reply: c_int, boot: &str) -> io::Result<()> {
    if fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() != boot {
        return Err(fail("boot changed"));
    }
    if queue(498)? != request || queue(499)? != reply {
        return Err(fail("mapping changed"));
    }
    let (b, n, tid) = queue_state(request)?;
    if b != 0 || n != 0 {
        return Err(fail("request queue occupied"));
    }
    let comm = fs::read_to_string(format!("/proc/{tid}/comm"))?;
    if comm.trim() != "abs.T_ICDM" {
        return Err(fail("unexpected receiver task"));
    }
    if fs::read_link(format!("/proc/{tid}/exe"))?.to_str() != Some("/usr/bin/card") {
        return Err(fail("unexpected daemon"));
    }
    let hash = Command::new("md5sum")
        .arg(format!("/proc/{tid}/exe"))
        .output()?;
    if !hash.status.success()
        || !String::from_utf8_lossy(&hash.stdout).starts_with("5e7c5215d81e883241831f5691874cde ")
    {
        return Err(fail("unverified card binary"));
    }
    let (b, n, _) = queue_state(reply)?;
    if b != 0 || n != 0 {
        return Err(fail("reply queue occupied; will not consume it"));
    }
    Ok(())
}
fn send_apdu(request: c_int, reply: c_int, apdu: &[u8]) -> io::Result<Vec<u8>> {
    if apdu.len() > 255
        || apdu.len() < 5
        || apdu[0] != 0x90
        || apdu[2] != 0
        || ![0, 2].contains(&apdu[3])
        || ![0x30, 0x34].contains(&apdu[1])
    {
        return Err(fail(
            "only verified ACAS parameter/ECM commands are enabled",
        ));
    }
    let rows = fs::read_to_string("/proc/sysvipc/shm")?;
    let found: Vec<Vec<_>> = rows
        .lines()
        .skip(1)
        .map(|s| s.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.first() == Some(&"872419354"))
        .collect();
    if found.len() != 1 || found[0].get(3) != Some(&"524") {
        return Err(fail("existing ICD shared-memory profile differs"));
    }
    let id = unsafe { shmget(0x3400_101a, 524, 0) };
    if id < 0 || found[0].get(1).and_then(|s| s.parse::<i32>().ok()) != Some(id) {
        return Err(fail("ICD segment changed"));
    }
    let ptr = unsafe { shmat(id, std::ptr::null(), 0) };
    if ptr as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    let shared = Shared(ptr as *mut u8);
    let mut message = wire(6);
    message.len = 4;
    message.payload[..4].copy_from_slice(&(apdu.len() as u32).to_le_bytes());
    unsafe {
        for i in 0..255 {
            std::ptr::write_volatile(shared.0.add(i), apdu.get(i).copied().unwrap_or(0));
        }
    }
    if unsafe { msgsnd(request, &message as *const _ as *const c_void, 92, 0x800) } < 0 {
        return Err(io::Error::last_os_error());
    }
    IN_FLIGHT.store(true, Ordering::SeqCst);
    receive(reply, 7, 0, 45)?;
    IN_FLIGHT.store(false, Ordering::SeqCst);
    let mut response = [0u8; 264];
    unsafe {
        for (i, b) in response.iter_mut().enumerate() {
            *b = std::ptr::read_volatile(shared.0.add(i));
        }
    }
    let result = u32::from_le_bytes(response[..4].try_into().unwrap());
    let length = u32::from_le_bytes(response[4..8].try_into().unwrap()) as usize;
    if result != 0 || !(2..=255).contains(&length) {
        return Err(fail(
            "native APDU transport did not return a bounded response",
        ));
    }
    let data = response[8..8 + length].to_vec();
    if data[length - 2..] != [0x90, 0] {
        return Err(fail("CAS rejected this command; no automatic retry"));
    }
    Ok(data)
}
fn parameters(request: c_int, reply: c_int) -> io::Result<([u8; 32], [u8; 8])> {
    let data = send_apdu(request, reply, &[0x90, 0x30, 0, 2, 0])?;
    if data.len() < 57 || data[4..6] != [0x21, 0] || data[6..8] != [0, 5] {
        return Err(fail("unexpected parameter response profile"));
    }
    Ok((
        data[16..48].try_into().unwrap(),
        data[48..56].try_into().unwrap(),
    ))
}

/// No Debug implementation: system parameters and control words stay in RAM.
pub struct NativeCard {
    boot: String,
}
impl NativeCard {
    pub fn open() -> io::Result<Self> {
        if !cfg!(all(target_arch = "arm", target_pointer_width = "32"))
            || mem::size_of::<Wire>() != 96
        {
            return Err(fail("native CAS requires verified ARM32 ABI"));
        }
        let card = Self {
            boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .into(),
        };
        card.transaction(|req, rep| {
            validate(req, rep, &card.boot)?;
            Ok(())
        })?;
        Ok(card)
    }
    fn transaction<T>(
        &self,
        operation: impl FnOnce(c_int, c_int) -> io::Result<T>,
    ) -> io::Result<T> {
        if POISONED.load(Ordering::SeqCst) {
            return Err(fail(
                "CAS transport latched after an error; inspect native state before restart",
            ));
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/tmp/card.lock")?;
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if unsafe { flock(lock.as_raw_fd(), 2 | 4) } == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(11) || Instant::now() >= until {
                return Err(fail("native card lock unavailable"));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let req = queue(498)?;
        let rep = queue(499)?;
        validate(req, rep, &self.boot)?;
        let result = operation(req, rep);
        if result.is_err() && IN_FLIGHT.load(Ordering::SeqCst) {
            POISONED.store(true, Ordering::SeqCst);
            // Retain serialization on ambiguous in-flight errors. No later request
            // from this process may overwrite shared memory or consume a late reply.
            std::mem::forget(lock);
        }
        result
    }
    pub fn parameters(&self) -> io::Result<([u8; 32], [u8; 8])> {
        self.transaction(parameters)
    }
    pub fn ecm(&self, section: &[u8]) -> io::Result<[[u8; 8]; 2]> {
        let apdu = ecm_apdu(section, 2)?;
        let len = section.len() - 12 + 6;
        self.transaction(|req, rep| {
            let data = send_apdu(req, rep, &apdu[..len])?;
            if data.len() != 25 || data[4..6] != [8, 0] {
                return Err(fail(
                    "ECM not entitled/unsupported response (requires 0800)",
                ));
            }
            Ok([
                data[6..14].try_into().unwrap(),
                data[14..22].try_into().unwrap(),
            ])
        })
    }
}

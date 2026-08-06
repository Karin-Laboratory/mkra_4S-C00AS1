//! Where transport stream bytes come from.
//!
//! Driver-backed capture and file-backed playback share this interface. The
//! BS/CS 2K path owns its DEMUX ring and reader; legacy graph-tap paths may
//! instead adopt or attach to product-owned buffers. Keeping both behind a
//! trait makes the Mirakurun surface testable on a laptop with `FileSource`.

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::xcdrv::{MapCache, XcDrv};

pub trait TsSource: Send {
    /// Append the next available bytes to `out`.
    ///
    /// Returns the number of bytes appended. `Ok(0)` means "nothing right now,
    /// try again" and is not an error — it is the normal result when the tuner
    /// has not locked yet. `Err` is terminal for the session.
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize>;

    /// Human-readable description for `/api/tuners`.
    fn describe(&self) -> String;
}

// --- driver-backed source ------------------------------------------------

/// Reads from a driver ring by attaching its own reader port.
///
/// The port is created on construction and destroyed on drop, so a crashed or
/// cancelled session does not leave a reader wedged against the writer.
pub struct XcReaderSource {
    drv: Arc<XcDrv>,
    maps: MapCache,
    buffer_handle: u32,
    reader: u32,
    timeout_ms: u32,
}

impl XcReaderSource {
    pub fn attach(
        drv: Arc<XcDrv>,
        buffer_handle: u32,
        timeout_ms: u32,
    ) -> io::Result<Self> {
        let reader = drv.create_reader(buffer_handle)?;
        Ok(XcReaderSource {
            maps: MapCache::new(Arc::clone(&drv), 16),
            drv,
            buffer_handle,
            reader,
            timeout_ms,
        })
    }

    /// Take ownership of a reader port created earlier in the same device
    /// graph.  This is needed for the vendor's TS-input sequence: the reader
    /// must exist before TSI/config/run, and the very same reader must then be
    /// drained.  Attaching a second reader while leaving the first idle makes
    /// ring backpressure disable the TS_RECODER input.
    pub fn adopt(
        drv: Arc<XcDrv>,
        buffer_handle: u32,
        reader: u32,
        timeout_ms: u32,
    ) -> Self {
        XcReaderSource {
            maps: MapCache::new(Arc::clone(&drv), 16),
            drv,
            buffer_handle,
            reader,
            timeout_ms,
        }
    }

    pub fn reader_handle(&self) -> u32 {
        self.reader
    }

    pub fn map_stats(&self) -> (u64, u64) {
        (self.maps.hits, self.maps.misses)
    }
}

impl TsSource for XcReaderSource {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let chunk = match self.drv.get_read(self.reader, true, self.timeout_ms) {
            Ok(chunk) => chunk,
            // XCE_TIMEOUT / XCE_CANCELLED are normal idle results from the
            // vendor ring API, not terminal stream errors.  A TS_RECODER may
            // need several polling intervals to acquire PAT/PMT and enable
            // output.  Treating the first 500 ms timeout as fatal made an
            // eight-second capture stop after its very first read attempt.
            Err(e) if matches!(e.raw_os_error(), Some(1038 | 1039)) => return Ok(0),
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(0);
        }
        let before = out.len();
        // Copy both halves before committing: if the second copy fails we must
        // not have told the driver the window is free.
        let first_size = chunk.first_size().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "driver returned wrapped size {} larger than total {}",
                    chunk.size1, chunk.size
                ),
            )
        })?;
        self.copy_piece(chunk.addr, first_size as usize, out)?;
        if chunk.size1 != 0 {
            self.copy_piece(chunk.addr1, chunk.size1 as usize, out)?;
        }
        self.drv.commit_read(self.reader, &chunk)?;
        Ok(out.len() - before)
    }

    fn describe(&self) -> String {
        format!(
            "xcodedrv buffer {:#x} reader {:#x}",
            self.buffer_handle, self.reader
        )
    }
}

impl XcReaderSource {
    /// Copy one read window out of the driver.
    ///
    /// The reply's address is a physical XCode address for the buffers whose
    /// storage lives in the XCode aperture, but a pointer already mapped into
    /// this process for host-memory buffers — `abs:remuxif_task` reads the
    /// reply directly, without mapping, which only works for the latter.
    /// Mapping a value that is already a user pointer reads unrelated physical
    /// memory and returns convincing-looking rubbish, so the two cases are
    /// separated by asking the kernel which one this is.
    fn copy_piece(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> io::Result<()> {
        if crate::sys::user_mapping_contains(addr, len) {
            let src = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
            out.extend_from_slice(src);
            return Ok(());
        }
        self.maps.copy_out(addr, len as u32, out)
    }
}

impl Drop for XcReaderSource {
    fn drop(&mut self) {
        // Cancel first: a reader parked in get_read cannot be destroyed, and a
        // buffer with a live reader cannot be destroyed either.
        if let Err(e) = self.drv.cancel_get_read(self.reader) {
            eprintln!(
                "mkra: warning: cancel_get_read({:#x}) failed: {e}",
                self.reader
            );
        }
        if let Err(e) = self.drv.destroy_reader(self.reader) {
            eprintln!(
                "mkra: warning: destroy_reader({:#x}) failed: {e}",
                self.reader
            );
        }
    }
}

// --- file-backed source --------------------------------------------------

/// Streams a TS file or FIFO, paced to a bitrate so that development on a
/// laptop exercises the same backpressure paths as a live tuner.
///
/// `bitrate_bps == 0` reads as fast as the file allows, which is what a FIFO
/// fed by another process wants.
pub struct FileSource {
    file: File,
    path: String,
    buf: Vec<u8>,
    bitrate_bps: u64,
    started: Instant,
    sent: u64,
    repeat: bool,
}

impl FileSource {
    pub fn open(path: &str, bitrate_bps: u64, repeat: bool) -> io::Result<Self> {
        Ok(FileSource {
            file: File::open(path)?,
            path: path.to_string(),
            buf: vec![0u8; 64 * 1024],
            bitrate_bps,
            started: Instant::now(),
            sent: 0,
            repeat,
        })
    }
}

impl TsSource for FileSource {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        if self.bitrate_bps > 0 {
            // Hold back if we are ahead of schedule, so a 30 Mbps stream really
            // takes a second per 30 Mbit rather than draining the file at once.
            let due = Duration::from_micros(self.sent * 8 * 1_000_000 / self.bitrate_bps.max(1));
            let elapsed = self.started.elapsed();
            if due > elapsed {
                let wait = (due - elapsed).min(Duration::from_millis(100));
                std::thread::sleep(wait);
                return Ok(0);
            }
        }
        let n = self.file.read(&mut self.buf)?;
        if n == 0 {
            if self.repeat {
                use std::io::Seek;
                self.file.seek(io::SeekFrom::Start(0))?;
                return Ok(0);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "end of TS file"));
        }
        out.extend_from_slice(&self.buf[..n]);
        self.sent += n as u64;
        Ok(n)
    }

    fn describe(&self) -> String {
        format!("file {}", self.path)
    }
}

// --- null source ---------------------------------------------------------

/// Produces nothing. Used when no backend is configured, so the API still
/// answers and `/api/tuners` reports honestly instead of the server refusing
/// to start.
pub struct NullSource;

impl TsSource for NullSource {
    fn read_into(&mut self, _out: &mut Vec<u8>) -> io::Result<usize> {
        std::thread::sleep(Duration::from_millis(50));
        Ok(0)
    }

    fn describe(&self) -> String {
        "none (no TS backend configured)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_ts(name: &str, packets: usize) -> String {
        let path = std::env::temp_dir().join(name);
        let mut f = File::create(&path).unwrap();
        for i in 0..packets {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p[1] = (i >> 8) as u8;
            p[2] = i as u8;
            f.write_all(&p).unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn file_source_reads_everything_then_reports_eof() {
        let path = temp_ts("mkra_test_a.ts", 100);
        let mut src = FileSource::open(&path, 0, false).unwrap();
        let mut out = Vec::new();
        loop {
            match src.read_into(&mut out) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(out.len(), 188 * 100);
        assert_eq!(out[0], 0x47);
        assert_eq!(crate::xcdrv::ts_sync_ratio(&out), 1.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_source_repeats_when_asked() {
        let path = temp_ts("mkra_test_b.ts", 4);
        let mut src = FileSource::open(&path, 0, true).unwrap();
        let mut out = Vec::new();
        // Enough iterations to wrap past the end at least twice.
        for _ in 0..20 {
            let _ = src.read_into(&mut out).unwrap();
        }
        assert!(out.len() > 188 * 4, "expected wrap-around, got {}", out.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn null_source_yields_nothing_without_erroring() {
        let mut out = Vec::new();
        assert_eq!(NullSource.read_into(&mut out).unwrap(), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn vendor_idle_error_numbers_are_preserved_by_io_error() {
        assert_eq!(io::Error::from_raw_os_error(1038).raw_os_error(), Some(1038));
        assert_eq!(io::Error::from_raw_os_error(1039).raw_os_error(), Some(1039));
    }
}

// --- passive ring tap ----------------------------------------------------

/// Reads a driver ring *without* attaching to it.
///
/// `create_reader` is the vendor's own way in, but it is not always available:
/// the section-filter rings are already owned by an internal port and refuse a
/// host connection outright (`-1035`), and creating one on a live ring changes
/// the product's behaviour if we ever fall behind.
///
/// The ring itself is just memory, and its object publishes both the base
/// address and the writer's position, so it can be followed by polling: read
/// `wpos`, copy everything between our own cursor and it, advance. Nothing is
/// written and no buffer ioctl is issued, so the product cannot tell.
///
/// The cost is that this is a *tap*, not a reservation: if the writer laps us
/// we lose bytes. That is detected (the gap exceeds the ring) and reported
/// rather than silently returning torn data.
pub struct PassiveRingSource {
    reader: crate::graph::ObjReader,
    handle: u32,
    pos: Option<u32>,
    pub lapped: u64,
    pub bytes: u64,
    idle: Duration,
}

impl PassiveRingSource {
    pub fn open(drv: Arc<XcDrv>, handle: u32) -> io::Result<Self> {
        let reader = crate::graph::ObjReader::new(drv);
        let o = reader.object(handle)?;
        if !o.valid() {
            return Err(io::Error::other(format!(
                "buffer {handle:#x} has no magic at +0x40 ({:#010x})",
                o.magic
            )));
        }
        if !o.ring_sane() {
            return Err(io::Error::other(format!(
                "buffer {handle:#x} is not a followable ring \
                 (size {:#x}, data {:#x}, wpos {:#x})",
                o.size, o.data, o.wpos
            )));
        }
        Ok(PassiveRingSource {
            reader,
            handle,
            pos: None,
            lapped: 0,
            bytes: 0,
            idle: Duration::from_millis(20),
        })
    }

    /// Largest copy we will do in one go, so a lapped ring cannot make us
    /// allocate the whole thing at once.
    const MAX_CHUNK: usize = 256 * 1024;
}

impl TsSource for PassiveRingSource {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let o = self.reader.object(self.handle)?;
        if !o.ring_sane() {
            std::thread::sleep(self.idle);
            return Ok(0);
        }
        let w = o.wpos;
        let pos = match self.pos {
            // Start at the writer so the first read is live data, not a
            // ring's worth of history that may be half-overwritten.
            None => {
                self.pos = Some(w);
                return Ok(0);
            }
            Some(p) => p,
        };
        if w == pos {
            std::thread::sleep(self.idle);
            return Ok(0);
        }
        let avail = if w > pos { w - pos } else { o.size - pos + w };
        if avail as usize > Self::MAX_CHUNK {
            self.lapped += 1;
        }
        let take = (avail as usize).min(Self::MAX_CHUNK) as u32;
        let before = out.len();
        if pos + take <= o.size {
            out.extend_from_slice(&self.reader.read_phys(o.data + pos, take as usize)?);
        } else {
            let first = o.size - pos;
            out.extend_from_slice(&self.reader.read_phys(o.data + pos, first as usize)?);
            out.extend_from_slice(&self.reader.read_phys(o.data, (take - first) as usize)?);
        }
        self.pos = Some((pos + take) % o.size);
        let n = out.len() - before;
        self.bytes += n as u64;
        Ok(n)
    }

    fn describe(&self) -> String {
        format!(
            "passive tap on xcodedrv buffer {:#x} ({} bytes, {} lap events)",
            self.handle, self.bytes, self.lapped
        )
    }
}

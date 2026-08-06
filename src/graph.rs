//! Reading the driver's own live graph instead of guessing at handles.
//!
//! Why this exists
//! ---------------
//! Session 3 brute-forced buffer handles into `create_reader` and oopsed the
//! kernel: `create_buffer_access_base` runs the handle through `p2uv_base` and
//! dereferences the result without checking it (docs/ABI.md).  So handles
//! must never be invented.  They do not have to be: `xcdrv.ko` keeps four
//! tables of everything it has built, and this box leaves them readable.
//!
//! What makes it readable
//! ----------------------
//! `kptr_restrict = 0`, `/proc/kallsyms` carries real addresses, and
//! `CONFIG_DEVKMEM` is on.  `gps_XCode` therefore resolves to a live pointer,
//! and the tables hang off it at fixed offsets recovered by disassembling
//! `init_graph_data`, `add_device_into_graph`, `add_buffer_into_graph`,
//! `add_pipeline_into_graph` and `add_port_into_graph`:
//!
//! ```text
//!   ctx+0x4dbd8 count / +0x4dbdc ptr   devices    stride 12   (cap 100)
//!   ctx+0x4dbe0 count / +0x4dbe4 ptr   buffers    stride 12   (cap 200)
//!   ctx+0x4dbe8 count / +0x4dbec ptr   pipelines  stride  4   (cap  32)
//!   ctx+0x4dbf0 count / +0x4dbf4 ptr   ports      stride 20   (cap 300)
//! ```
//!
//! The tables are in lowmem, so `/dev/kmem` returns them verbatim.  The buffer
//! *objects* they point at are not: they live in the XCode6 window at ARM
//! physical 0x1F000000 (160 MiB, `/proc/vmallocinfo`: `arch_map_io_memory
//! ... ioremap`).  `/dev/mem` refuses it because it is below `PHYS_OFFSET`
//! (0x56000000) and `/dev/kmem` zero-fills it because `vread()` treats
//! `VM_IOREMAP` as a hole — so objects are read with `xc_map` (ioctl 0x7A),
//! the same call the product makes on every TS chunk.
//!
//! Nothing in this module issues a buffer ioctl.  It is all reads.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::xcdrv::XcDrv;

pub const KMEM: &str = "/dev/kmem";
pub const KALLSYMS: &str = "/proc/kallsyms";

/// Offsets of the graph tables inside the per-chip context.
const GRAPH_TABLES: [(&str, u32, u32, usize, usize); 4] = [
    ("device", 0x4DBD8, 0x4DBDC, 12, 100),
    ("buffer", 0x4DBE0, 0x4DBE4, 12, 200),
    // init_graph_data allocates 128 bytes, and add_pipeline_into_graph walks
    // it in four-byte steps up to base+128: 32 bare handle slots.
    ("pipeline", 0x4DBE8, 0x4DBEC, 4, 32),
    ("port", 0x4DBF0, 0x4DBF4, 20, 300),
];

/// The XCode6 shared-memory window, from `/proc/vmallocinfo`. Handles outside
/// it cannot be mapped, and are reported rather than passed to any ioctl.
pub const XC_WINDOW_START: u32 = 0x1F00_0000;
pub const XC_WINDOW_END: u32 = 0x2900_0000;

pub struct Kmem {
    f: File,
}

impl Kmem {
    pub fn open() -> io::Result<Self> {
        Ok(Kmem {
            f: File::open(KMEM)?,
        })
    }

    pub fn read(&mut self, addr: u32, len: usize) -> io::Result<Vec<u8>> {
        self.f.seek(SeekFrom::Start(addr as u64))?;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            match self.f.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.truncate(got);
        Ok(buf)
    }

    pub fn u32(&mut self, addr: u32) -> io::Result<u32> {
        let b = self.read(addr, 4)?;
        if b.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("short read at {addr:#x}"),
            ));
        }
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Resolve a kernel symbol by name from `/proc/kallsyms`.
///
/// Preferred over computing `module_base + ELF offset` because kallsyms is
/// authoritative for the running kernel and needs no copy of the `.ko`.
pub fn ksym(name: &str) -> io::Result<u32> {
    let txt = std::fs::read_to_string(KALLSYMS)?;
    for line in txt.lines() {
        let mut it = line.split_whitespace();
        let (Some(addr), Some(_ty), Some(sym)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if sym == name {
            if let Ok(a) = u32::from_str_radix(addr, 16) {
                if a != 0 {
                    return Ok(a);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("kallsyms has no usable `{name}` (kptr_restrict?)"),
    ))
}

#[derive(Debug, Clone)]
pub struct Row {
    pub index: usize,
    pub words: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub ctx: u32,
    pub tables: BTreeMap<String, (u32, u32, Vec<Row>)>, // name -> (count, ptr, rows)
}

impl Graph {
    pub fn read() -> io::Result<Self> {
        let mut k = Kmem::open()?;
        let ctx = k.u32(ksym("gps_XCode")?)?;
        if ctx < 0x8000_0000 {
            return Err(io::Error::other(format!(
                "gps_XCode = {ctx:#x} is not a kernel pointer; refusing to walk it"
            )));
        }
        let mut tables = BTreeMap::new();
        for (name, coff, poff, stride, cap) in GRAPH_TABLES {
            let count = k.u32(ctx + coff)?;
            let ptr = k.u32(ctx + poff)?;
            let mut rows = Vec::new();
            if ptr >= 0x8000_0000 && count as usize <= cap {
                let n = cap.min(count as usize + 8);
                let raw = k.read(ptr, n * stride)?;
                for i in 0..n {
                    let base = i * stride;
                    if base + stride > raw.len() {
                        break;
                    }
                    let words: Vec<u32> = (0..stride / 4)
                        .map(|j| {
                            let o = base + j * 4;
                            u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]])
                        })
                        .collect();
                    if words.iter().any(|&w| w != 0) {
                        rows.push(Row { index: i, words });
                    }
                }
            }
            tables.insert(name.to_string(), (count, ptr, rows));
        }
        Ok(Graph { ctx, tables })
    }

    /// Every distinct buffer handle the driver has registered.
    pub fn buffer_handles(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .tables
            .get("buffer")
            .map(|(_, _, rows)| rows.iter().filter_map(|r| r.words.first().copied()).collect())
            .unwrap_or_default();
        // Ports name buffers too, and occasionally one the buffer table has
        // already recycled out; include them so nothing live is missed.
        if let Some((_, _, rows)) = self.tables.get("port") {
            for r in rows {
                for &w in &r.words[2..4.min(r.words.len())] {
                    if (XC_WINDOW_START..XC_WINDOW_END).contains(&w) && w & 0xFF == 0 {
                        v.push(w);
                    }
                }
            }
        }
        v.retain(|&h| h != 0);
        v.sort_unstable();
        v.dedup();
        v
    }
}

// --- buffer objects ------------------------------------------------------

/// Fields of an `xcdrv` buffer object, at offsets confirmed against the
/// driver's own checks (`buf_check_handle` reads +0x40) and against live
/// dumps of every buffer in the graph.
#[derive(Debug, Clone, Copy)]
pub struct BufObj {
    pub handle: u32,
    pub magic: u32,
    pub flags: u32,
    pub kptr: u32,
    pub desc: u32,
    pub size: u32,
    pub align: u32,
    pub fill: u32,
    pub data: u32,
    pub wpos: u32,
    pub rpos: u32,
    pub rpos2: u32,
    pub raw: [u32; 64],
}

impl Default for BufObj {
    fn default() -> Self {
        BufObj {
            handle: 0, magic: 0, flags: 0, kptr: 0, desc: 0, size: 0, align: 0,
            fill: 0, data: 0, wpos: 0, rpos: 0, rpos2: 0, raw: [0; 64],
        }
    }
}

impl BufObj {
    pub fn valid(&self) -> bool {
        self.magic == 0x5A5A_5A5A
    }

    /// Whether the ring looks self-consistent enough to read passively.
    pub fn ring_sane(&self) -> bool {
        self.valid()
            && self.size >= 0x1000
            && (XC_WINDOW_START..XC_WINDOW_END).contains(&self.data)
            && self.wpos < self.size
    }
}

pub struct ObjReader {
    drv: Arc<XcDrv>,
}

impl ObjReader {
    pub fn new(drv: Arc<XcDrv>) -> Self {
        ObjReader { drv }
    }

    /// Map `len` bytes of XCode6 physical memory and copy them out.
    ///
    /// The mapping is released before returning. `xc_map` takes page-aligned
    /// requests, so the range is widened and the copy offset back.
    pub fn read_phys(&self, phys: u32, len: usize) -> io::Result<Vec<u8>> {
        let base = phys & !0xFFF;
        let off = (phys - base) as usize;
        let size = ((off + len + 0xFFF) & !0xFFF) as u32;
        let vaddr = self.drv.map(base, size)?;
        let mut out = vec![0u8; len];
        // SAFETY: `vaddr` came from the driver's own mapping call and covers
        // `size` bytes, of which we touch `off + len <= size`. The memory is
        // written by hardware, so a torn read is possible; every caller
        // tolerates that (it costs at most one corrupt TS packet).
        unsafe {
            std::ptr::copy_nonoverlapping(
                (vaddr as usize + off) as *const u8,
                out.as_mut_ptr(),
                len,
            );
        }
        let _ = self.drv.unmap(vaddr, size);
        Ok(out)
    }

    pub fn object(&self, handle: u32) -> io::Result<BufObj> {
        if !(XC_WINDOW_START..XC_WINDOW_END).contains(&handle) {
            return Err(io::Error::other(format!(
                "handle {handle:#x} is outside the mapped XCode6 window \
                 [{XC_WINDOW_START:#x},{XC_WINDOW_END:#x})"
            )));
        }
        let b = self.read_phys(handle, 0x100)?;
        let w = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let mut raw = [0u32; 64];
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = w(i * 4);
        }
        Ok(BufObj {
            handle,
            magic: w(0x40),
            flags: w(0x48),
            kptr: w(0x4C),
            desc: w(0x6C),
            size: w(0x70),
            align: w(0x74),
            fill: w(0x78),
            data: w(0x7C),
            wpos: w(0x84),
            rpos: w(0x88),
            rpos2: w(0x8C),
            raw,
        })
    }
}

// --- content classification ---------------------------------------------

/// Best 188-byte sync-byte ratio over all phases, with the winning phase.
/// Packet strides worth testing, in bytes.
///
/// 188 is bare MPEG-TS. **192 is the one that matters here**: Japanese
/// recorders emit TTS (a 4-byte arrival timestamp in front of each packet), and
/// this box is no exception — `libvixs` contains both the string `"TTS Stream"`
/// and a method `ts_packet_retrieve_t::check_TS_or_TTS`, so the product itself
/// distinguishes the two. 204 and 208 are the Reed-Solomon-protected forms.
pub const TS_STRIDES: [usize; 4] = [188, 192, 204, 208];

/// Best sync-byte ratio over every stride in [`TS_STRIDES`], as
/// `(ratio, stride, phase)`.
///
/// Testing only 188 is not a partial answer, it is a wrong one. `gcd(188, 192)`
/// is 4, so a *perfect* 192-byte TTS stream sampled every 188 bytes lands on a
/// real sync byte roughly once every 48 samples, giving a ratio near 0.02–0.03
/// — indistinguishable from noise, and exactly the range in which several
/// buffers on this box were written off as "not TS" (docs/ABI.md).
pub fn ts_score_multi(buf: &[u8]) -> (f32, usize, usize) {
    let mut best = (0.0f32, TS_STRIDES[0], 0usize);
    for &stride in &TS_STRIDES {
        if buf.len() < stride * 8 {
            continue;
        }
        for off in 0..stride {
            let mut hits = 0u32;
            let mut total = 0u32;
            let mut i = off;
            while i < buf.len() {
                total += 1;
                if buf[i] == 0x47 {
                    hits += 1;
                }
                i += stride;
            }
            // Too few samples and the ratio is meaningless: a 3-sample window
            // that happens to hit two 0x47s scores 0.67 on pure noise.
            if total < 8 {
                continue;
            }
            let r = hits as f32 / total as f32;
            if r > best.0 {
                best = (r, stride, off);
            }
            if best.0 >= 1.0 {
                return best;
            }
        }
    }
    best
}

/// Sync ratio at the classic 188-byte stride only.
///
/// Kept because some callers want to say "bare TS specifically"; anything
/// deciding *whether this is a transport stream at all* must use
/// [`ts_score_multi`].
pub fn ts_score(buf: &[u8]) -> (f32, usize) {
    if buf.len() < 188 * 4 {
        return (0.0, 0);
    }
    let mut best = (0.0f32, 0usize);
    for off in 0..188 {
        let mut hits = 0u32;
        let mut total = 0u32;
        let mut i = off;
        while i < buf.len() {
            total += 1;
            if buf[i] == 0x47 {
                hits += 1;
            }
            i += 188;
        }
        if total == 0 {
            continue;
        }
        let r = hits as f32 / total as f32;
        if r > best.0 {
            best = (r, off);
        }
        if best.0 >= 1.0 {
            break;
        }
    }
    best
}

/// How many consecutive well-formed TLV packets (ARIB STD-B32) start at the
/// best position in `buf`.
///
/// A TLV packet is `0x7F`, a type in {0x01,0x02,0x03,0xFE,0xFF}, then a 16-bit
/// big-endian length. Chaining is a far stronger signal than byte frequency:
/// random data almost never chains past two.
pub fn tlv_score(buf: &[u8]) -> (usize, usize) {
    let mut best = (0usize, 0usize);
    let limit = buf.len().min(4096);
    for start in 0..limit {
        if buf[start] != 0x7F {
            continue;
        }
        let mut i = start;
        let mut n = 0usize;
        while i + 4 <= buf.len() {
            if buf[i] != 0x7F {
                break;
            }
            let t = buf[i + 1];
            if !matches!(t, 0x01..=0x03 | 0xFE | 0xFF) {
                break;
            }
            let l = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
            if l == 0 || l > 0xFFF0 {
                break;
            }
            i += 4 + l;
            n += 1;
        }
        if n > best.0 {
            best = (n, start);
        }
        if best.0 > 16 {
            break;
        }
    }
    best
}

pub fn distinct_bytes(buf: &[u8]) -> usize {
    let mut seen = [false; 256];
    for &b in buf.iter().take(8192) {
        seen[b as usize] = true;
    }
    seen.iter().filter(|&&x| x).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_score_finds_a_clean_stream_at_any_phase() {
        let mut v = vec![0u8; 17];
        for i in 0..40 {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p[1] = i as u8;
            v.extend_from_slice(&p);
        }
        let (r, off) = ts_score(&v);
        assert_eq!(off, 17);
        assert!(r > 0.99, "ratio {r}");
    }

    #[test]
    fn a_perfect_tts_stream_is_invisible_at_188_and_obvious_at_192() {
        // The bug this guards against. Build clean 192-byte TTS: 4 timestamp
        // bytes then a 188-byte packet starting 0x47.
        let mut v = Vec::new();
        for i in 0..400u32 {
            v.extend_from_slice(&i.to_be_bytes());
            v.push(0x47);
            v.extend(std::iter::repeat((i & 0x3F) as u8).take(187));
        }
        let (r188, _) = ts_score(&v);
        assert!(r188 < 0.10, "188-only scored {r188}, so the old detector saw noise");

        let (r, stride, phase) = ts_score_multi(&v);
        assert!(r > 0.99, "multi-stride scored {r}");
        assert_eq!(stride, 192);
        assert_eq!(phase, 4);
    }

    #[test]
    fn multi_stride_still_finds_plain_188_byte_ts() {
        let mut v = vec![0u8; 13];
        for i in 0..300 {
            v.push(0x47);
            v.extend(std::iter::repeat((i % 251) as u8).take(187));
        }
        let (r, stride, phase) = ts_score_multi(&v);
        assert!(r > 0.99);
        assert_eq!(stride, 188);
        assert_eq!(phase, 13);
    }

    #[test]
    fn multi_stride_rejects_noise_at_every_stride() {
        // Deterministic pseudo-noise; every stride must stay near 1/256.
        let mut v = vec![0u8; 200_000];
        let mut x: u32 = 0xDEAD_BEEF;
        for b in v.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 16) as u8;
        }
        let (r, _, _) = ts_score_multi(&v);
        assert!(r < 0.10, "noise scored {r}");
    }

    #[test]
    fn ts_score_rejects_noise() {
        let v: Vec<u8> = (0..4000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let (r, _) = ts_score(&v);
        assert!(r < 0.5, "noise scored {r}");
    }

    #[test]
    fn tlv_score_chains_well_formed_packets() {
        let mut v = vec![0xAAu8; 5];
        for _ in 0..6 {
            v.extend_from_slice(&[0x7F, 0x02, 0x00, 0x08]);
            v.extend_from_slice(&[0u8; 8]);
        }
        let (n, start) = tlv_score(&v);
        assert_eq!(start, 5);
        assert_eq!(n, 6);
    }

    #[test]
    fn tlv_score_rejects_noise() {
        let v: Vec<u8> = (0..4000u32).map(|i| (i.wrapping_mul(40503) >> 7) as u8).collect();
        assert!(tlv_score(&v).0 <= 2);
    }

    #[test]
    fn ring_sanity_requires_a_window_address() {
        let mut o = BufObj {
            magic: 0x5A5A5A5A,
            size: 0x10000,
            data: 0x2800_0000,
            wpos: 0x100,
            ..Default::default()
        };
        assert!(o.ring_sane());
        o.data = 0x5600_0000;
        assert!(!o.ring_sane(), "an address outside the window is not mappable");
        o.data = 0x2800_0000;
        o.wpos = o.size + 1;
        assert!(!o.ring_sane());
    }
}

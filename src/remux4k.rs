//! BS4K: build our own REMUX and tap the product's live MMT-derived ES rings.
//!
//! Why this exists
//! ---------------
//! The box converts MMT to MPEG-TS in hardware — that is what XCode6's REMUX
//! (`me_type 0x0E`) is for — but the product only assembles one while it is
//! recording. Everything needed to build a second, private one is measured:
//! `adsb:remux_xcif_open_remux @0x421f8` (create/config/TSOUT/TSINFO),
//! `remux_xcif_connect_avesbuff @0x42528` (the ES inputs) and
//! `remux_xcif_add_stream @0x42b5c` (program and stream declaration).
//!
//! What makes it possible without disturbing the product
//! ----------------------------------------------------
//! The live 4K ES rings are created by `adsb`'s FDC with room for more than
//! one consumer — video `m_max_reader = 4` (`adsb:videoSetFilter @0x5c178`,
//! create at `0x5c222`), audio `m_max_reader = 5` (`audioSetFilter @0x4a9e4`,
//! create at `0x4aa8c`) — and the product itself relies on that: while
//! recording, the video decoder and the recording REMUX read the *same* ring.
//! A second REMUX is the vendor's own topology, not a new one.
//!
//! What is deliberately not done here
//! ----------------------------------
//! * No `create_reader` on a product buffer. `destroy_reader` returns -1022 on
//!   this box and the slot is then lost until reboot
//!   (measured on the live graph). Device inputs are
//!   released by `disconnect`, which is a normal product call.
//! * No product device or buffer is ever stopped, destroyed or reconfigured.
//! * No PSI section rings are borrowed from the product. PAT/PMT are generated
//!   here instead, from the PIDs we ourselves declared to the REMUX.

use std::sync::Arc;

use crate::graph::Graph;
use crate::xcdrv::XcDrv;

/// `me_type` of the firmware-created MMT demultiplexer.
pub const ME_TYPE_MMT_DEMUX: u32 = 0x25;

/// Sizes and media types the live 4K ES rings must have. Both are the literal
/// `xc_buffer_create` arguments in `adsb`'s FDC, so a mismatch means the graph
/// is not in the state this code was written for and nothing should be built.
pub const VIDEO_ES_SIZE: u32 = 0x0140_0000; // 20 MiB, adsb:0x5c1ee
pub const AUDIO_ES_SIZE: u32 = 0x0020_0000; // 2 MiB,  adsb:0x4aa8c
pub const MEDIA_VIDEO_ES: u32 = 3;
pub const MEDIA_AUDIO_ES: u32 = 6;

/// Identity the emitted TS advertises. Defaults are the product's literals;
/// `TapConfig` overrides them with the real TLV stream id and service id so a
/// Mirakurun-style client can filter the stream by service.
///
/// Program layout. These are the product's own literals in
/// `remux_xcif_add_stream`, and they reproduce the PMT that was captured from
/// the recording ring: PCR 0x1001, PMT 0x0100, HEVC 0x1011, AAC 0x1100.
pub const PROGRAM_NUMBER: u32 = 1;
pub const PCR_PID: u32 = 0x1001;
pub const PMT_PID: u32 = 0x0100;
pub const PCR_INTERVAL: u32 = 100;
pub const VIDEO_PID: u32 = 0x1011;
pub const AUDIO_PID: u32 = 0x1100;
pub const VIDEO_STREAM_ID: u32 = 0xe0;
pub const AUDIO_STREAM_ID: u32 = 0xc0;
pub const VIDEO_STREAM_TYPE: u32 = 0x24; // HEVC
pub const AUDIO_STREAM_TYPE: u32 = 0x11; // AAC LATM

/// Transport stream id put in our generated PAT. The product's recording used
/// the real network TSID (0xB110 for BS-17); we do not read the MMT SI here,
/// so a fixed private value is used and reported.
pub const DEFAULT_TSID: u16 = 0x4000;

pub const TS_PACKET: usize = 188;
pub const TTS_PACKET: usize = 192;

/// What `discover` found in the live driver graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveEs {
    pub mmt_demux: u32,
    pub pipeline: u32,
    pub video_es: u32,
    pub audio_es: u32,
    pub video_readers: u32,
    pub audio_readers: u32,
}

/// One row of the driver's port table: `[port, type, a, b, extra]`, stride 20.
/// `type == 1` means "device `a` writes buffer `b`".
const PORT_TYPE_DEVICE_WRITES_BUFFER: u32 = 1;

/// Find the running MMT demultiplexer, its pipeline, and the video/audio ES
/// rings it writes.
///
/// Read-only: `/dev/kmem` for the graph tables plus `GET_STATUS` (ioctl
/// 0x00010036) for each candidate buffer, which is the same pair the EPG tap
/// has been running against the product for months.
pub fn discover(drv: &Arc<XcDrv>) -> Result<LiveEs, String> {
    let graph = Graph::read().map_err(|e| format!("read live graph: {e}"))?;
    let devices = graph
        .tables
        .get("device")
        .map(|(_, _, rows)| rows.clone())
        .unwrap_or_default();
    let ports = graph
        .tables
        .get("port")
        .map(|(_, _, rows)| rows.clone())
        .unwrap_or_default();

    let mut found: Option<LiveEs> = None;
    for dev in devices.iter().filter(|r| r.words.get(1) == Some(&ME_TYPE_MMT_DEMUX)) {
        let handle = dev.words[0];
        let pipeline = dev.words.get(2).copied().unwrap_or(0);
        if pipeline == 0 {
            continue;
        }
        let mut video = None;
        let mut audio = None;
        for port in &ports {
            if port.words.len() < 4
                || port.words[1] != PORT_TYPE_DEVICE_WRITES_BUFFER
                || port.words[2] != handle
            {
                continue;
            }
            let buffer = port.words[3];
            let status = match drv.sbuf_status_raw(buffer) {
                Ok(status) => status,
                // A buffer that has just been recycled is not an error: skip
                // it rather than abandoning the whole search.
                Err(_) => continue,
            };
            let (size, media, subtype, readers) = (status[2], status[6], status[7], status[9]);
            if media == MEDIA_VIDEO_ES && subtype == 0 && size == VIDEO_ES_SIZE {
                video = Some((buffer, readers));
            } else if media == MEDIA_AUDIO_ES && subtype == 0 && size == AUDIO_ES_SIZE {
                audio = Some((buffer, readers));
            }
        }
        if let (Some((video_es, video_readers)), Some((audio_es, audio_readers))) = (video, audio) {
            if found.is_some() {
                return Err(
                    "more than one MMT_DEMUX is writing a full 4K ES pair; refusing to guess"
                        .to_string(),
                );
            }
            found = Some(LiveEs {
                mmt_demux: handle,
                pipeline,
                video_es,
                audio_es,
                video_readers,
                audio_readers,
            });
        }
    }
    found.ok_or_else(|| {
        "no live 4K ES pair found: need an MMT_DEMUX writing a 20 MiB VIDEO_ES and a 2 MiB \
         AUDIO_ES. Put the box in the green (watching) state first"
            .to_string()
    })
}

// --- TTS -> TS -------------------------------------------------------------

/// Strips the 4-byte arrival timestamp the REMUX puts in front of every packet.
///
/// `me_format = 3` produces 192-byte TTS records — `[4-byte ATS][188-byte TS]`
/// — which is what was measured on the product's recording ring (`ratio 1.000
/// stride 192 phase 4`). Mirakurun wants plain 188-byte TS, so the timestamp is
/// dropped here rather than downstream.
#[derive(Default)]
pub struct TtsUnwrap {
    /// Bytes not yet consumed, carried into the next chunk.
    carry: Vec<u8>,
    locked: bool,
    pub packets: u64,
    pub resyncs: u64,
}

impl TtsUnwrap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw ring bytes, receive whole 188-byte TS packets.
    ///
    /// Each chunk is scanned in place and at most one memmove is done for the
    /// unconsumed tail. The first version drained the front of a shared buffer
    /// once per packet, which is quadratic in the chunk size: at the driver's
    /// 288 KiB read window it could not keep up with the stream on this CPU,
    /// the ring overflowed, and packets were lost.
    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if self.carry.is_empty() {
            let consumed = self.scan(input, out);
            self.carry.extend_from_slice(&input[consumed..]);
            self.trim_carry();
            return;
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(input);
        let consumed = self.scan(&buf, out);
        buf.drain(..consumed);
        self.carry = buf;
        self.trim_carry();
    }

    /// Emit every whole record in `buf` and report how many bytes were used.
    fn scan(&mut self, buf: &[u8], out: &mut Vec<u8>) -> usize {
        let mut pos = 0usize;
        loop {
            if !self.locked {
                match Self::find_phase(&buf[pos..]) {
                    Some(offset) => {
                        pos += offset;
                        self.locked = true;
                    }
                    None => return pos,
                }
            }
            if buf.len() - pos < TTS_PACKET {
                return pos;
            }
            if buf[pos + 4] != 0x47 {
                // Lost framing (writer restart, dropped window): re-search
                // rather than emit garbage.
                self.locked = false;
                self.resyncs += 1;
                pos += 1;
                continue;
            }
            out.extend_from_slice(&buf[pos + 4..pos + TTS_PACKET]);
            self.packets += 1;
            pos += TTS_PACKET;
        }
    }

    /// While unlocked the carry only needs enough context to find a phase; a
    /// stream that never syncs must not grow it without bound.
    fn trim_carry(&mut self) {
        let keep = 4 * TTS_PACKET;
        if !self.locked && self.carry.len() > keep {
            let drop = self.carry.len() - 3 * TTS_PACKET;
            self.carry.drain(..drop);
        }
    }

    /// Offset of the first byte of a TTS record, confirmed by three
    /// consecutive sync bytes so a 0x47 inside the payload cannot lock us.
    fn find_phase(buf: &[u8]) -> Option<usize> {
        let need = 3;
        if buf.len() < TTS_PACKET * need {
            return None;
        }
        let last = buf.len() - TTS_PACKET * need;
        (0..=last).find(|&start| (0..need).all(|k| buf[start + k * TTS_PACKET + 4] == 0x47))
    }
}

// --- generated PSI ---------------------------------------------------------

fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn section(table_id: u8, table_id_ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
    // 5 bytes of the fixed tail after section_length, plus body, plus CRC.
    let section_length = 5 + body.len() + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(table_id);
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&table_id_ext.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.extend_from_slice(body);
    let crc = crc32_mpeg(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

fn section_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; TS_PACKET] {
    let mut p = [0xFFu8; TS_PACKET];
    p[0] = 0x47;
    p[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // payload_unit_start_indicator
    p[2] = (pid & 0xFF) as u8;
    p[3] = 0x10 | (cc & 0x0F); // payload only
    p[4] = 0x00; // pointer_field
    let n = section.len().min(TS_PACKET - 5);
    p[5..5 + n].copy_from_slice(&section[..n]);
    p
}

/// PAT + PMT for the program we declared to the REMUX.
///
/// The product sources these from real section rings via `add_psi`; those
/// rings belong to `adsb`'s recording path and are not safely reachable from
/// outside, so the two tables are generated from the same PID constants that
/// were programmed into the REMUX. Everything else in the stream — PES, PCR,
/// timing — comes from the hardware.
pub struct PsiGenerator {
    pat: Vec<u8>,
    pmt: Vec<u8>,
    cc_pat: u8,
    cc_pmt: u8,
}

impl PsiGenerator {
    pub fn new(tsid: u16) -> Self {
        let mut pat_body = Vec::new();
        pat_body.extend_from_slice(&(PROGRAM_NUMBER as u16).to_be_bytes());
        pat_body.extend_from_slice(&(0xE000u16 | PMT_PID as u16).to_be_bytes());

        let mut pmt_body = Vec::new();
        pmt_body.extend_from_slice(&(0xE000u16 | PCR_PID as u16).to_be_bytes());
        pmt_body.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length = 0
        for (stream_type, pid) in [
            (VIDEO_STREAM_TYPE as u8, VIDEO_PID as u16),
            (AUDIO_STREAM_TYPE as u8, AUDIO_PID as u16),
        ] {
            pmt_body.push(stream_type);
            pmt_body.extend_from_slice(&(0xE000u16 | pid).to_be_bytes());
            pmt_body.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length = 0
        }

        Self {
            pat: section(0x00, tsid, 0, &pat_body),
            pmt: section(0x02, PROGRAM_NUMBER as u16, 0, &pmt_body),
            cc_pat: 0,
            cc_pmt: 0,
        }
    }

    /// Emit one PAT and one PMT packet, advancing their continuity counters.
    pub fn emit(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&section_packet(0x0000, self.cc_pat, &self.pat));
        out.extend_from_slice(&section_packet(PMT_PID as u16, self.cc_pmt, &self.pmt));
        self.cc_pat = self.cc_pat.wrapping_add(1) & 0x0F;
        self.cc_pmt = self.cc_pmt.wrapping_add(1) & 0x0F;
    }
}

/// Count packets per PID in a TS sample, for the capture report.
pub fn pid_histogram(ts: &[u8]) -> Vec<(u16, usize)> {
    let mut counts: std::collections::BTreeMap<u16, usize> = std::collections::BTreeMap::new();
    for packet in ts.chunks_exact(TS_PACKET) {
        if packet[0] != 0x47 {
            continue;
        }
        let pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
        *counts.entry(pid).or_default() += 1;
    }
    let mut v: Vec<(u16, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tts_stream(packets: usize, pid: u16) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..packets {
            v.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // arrival timestamp
            let mut p = [0u8; TS_PACKET];
            p[0] = 0x47;
            p[1] = (pid >> 8) as u8;
            p[2] = (pid & 0xFF) as u8;
            p[3] = 0x10 | (i as u8 & 0x0F);
            v.extend_from_slice(&p);
        }
        v
    }

    #[test]
    fn tts_unwrap_drops_the_timestamp_and_keeps_188_byte_packets() {
        let mut u = TtsUnwrap::new();
        let mut out = Vec::new();
        u.push(&tts_stream(10, 0x1011), &mut out);
        assert_eq!(out.len(), 10 * TS_PACKET);
        assert_eq!(u.packets, 10);
        assert!(out.chunks_exact(TS_PACKET).all(|p| p[0] == 0x47));
    }

    #[test]
    fn tts_unwrap_locks_onto_a_stream_that_starts_mid_record() {
        let full = tts_stream(12, 0x1011);
        let mut u = TtsUnwrap::new();
        let mut out = Vec::new();
        u.push(&full[100..], &mut out); // start inside the first record
        assert_eq!(out.len(), 11 * TS_PACKET);
        assert!(out.chunks_exact(TS_PACKET).all(|p| p[0] == 0x47));
    }

    #[test]
    fn tts_unwrap_survives_being_fed_one_byte_at_a_time() {
        let full = tts_stream(8, 0x1100);
        let mut u = TtsUnwrap::new();
        let mut out = Vec::new();
        for b in &full {
            u.push(std::slice::from_ref(b), &mut out);
        }
        assert_eq!(out.len(), 8 * TS_PACKET);
    }

    #[test]
    fn crc32_matches_the_reference_vector() {
        // The MPEG-2 CRC-32 of "123456789" is 0x0376E6E7.
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn generated_psi_is_self_consistent() {
        let mut g = PsiGenerator::new(DEFAULT_TSID);
        let mut out = Vec::new();
        g.emit(&mut out);
        assert_eq!(out.len(), 2 * TS_PACKET);

        for packet in out.chunks_exact(TS_PACKET) {
            assert_eq!(packet[0], 0x47);
            assert_eq!(packet[1] & 0x40, 0x40, "payload_unit_start_indicator");
            let len = 3 + (((packet[6] as usize & 0x0F) << 8) | packet[7] as usize);
            let section = &packet[5..5 + len];
            assert_eq!(crc32_mpeg(section), 0, "section CRC must verify to zero");
        }
    }

    #[test]
    fn pat_points_at_the_pmt_and_pmt_lists_both_streams() {
        let mut g = PsiGenerator::new(0x4010);
        let mut out = Vec::new();
        g.emit(&mut out);
        let pat = &out[..TS_PACKET];
        assert_eq!(((pat[1] as u16 & 0x1F) << 8) | pat[2] as u16, 0x0000);
        // program_number 1 -> PMT PID 0x0100
        assert_eq!(&pat[13..17], &[0x00, 0x01, 0xE1, 0x00]);

        let pmt = &out[TS_PACKET..];
        assert_eq!(((pmt[1] as u16 & 0x1F) << 8) | pmt[2] as u16, PMT_PID as u16);
        assert_eq!(&pmt[13..17], &[0xF0, 0x01, 0xF0, 0x00]); // PCR 0x1001, no descriptors
        assert_eq!(&pmt[17..22], &[0x24, 0xF0, 0x11, 0xF0, 0x00]); // HEVC on 0x1011
        assert_eq!(&pmt[22..27], &[0x11, 0xF1, 0x00, 0xF0, 0x00]); // AAC on 0x1100
    }

    #[test]
    fn continuity_counters_advance_per_pid() {
        let mut g = PsiGenerator::new(DEFAULT_TSID);
        let mut out = Vec::new();
        for _ in 0..3 {
            g.emit(&mut out);
        }
        let cc: Vec<u8> = out.chunks_exact(TS_PACKET).map(|p| p[3] & 0x0F).collect();
        assert_eq!(cc, vec![0, 0, 1, 1, 2, 2]);
    }

    #[test]
    fn pid_histogram_counts_by_pid() {
        let mut ts = Vec::new();
        let mut u = TtsUnwrap::new();
        u.push(&tts_stream(5, 0x1011), &mut ts);
        u.push(&tts_stream(2, 0x1100), &mut ts);
        let hist = pid_histogram(&ts);
        assert_eq!(hist[0], (0x1011, 5));
        assert_eq!(hist[1], (0x1100, 2));
    }
}

// --- the tap as a reusable TS source ---------------------------------------

use std::io;
use std::time::{Duration, Instant};

use crate::tssrc::{TsSource, XcReaderSource};

/// Knobs the gate command and the server both need.
#[derive(Clone, Copy, Debug)]
pub struct TapConfig {
    /// transport_stream_id for the generated PAT (the real TLV stream id).
    pub stream_id: u16,
    /// program_number / service_id for the generated PAT and PMT.
    pub service_id: u16,
    /// Create and connect the REMUX's TSINFO side channel. The product does;
    /// measured on this box it makes the REMUX stop taking input after a few
    /// seconds, so the default is off. Diagnostic use only.
    pub with_tsinfo: bool,
    /// Release the live ES rings once the source ring has backed up this far.
    pub es_level_abort: u32,
    /// Backstop for the same condition, in milliseconds (0 disables).
    pub stall_abort_ms: u64,
    /// Log recovered gaps longer than this. Normal jitter is 150..390 ms.
    pub gap_log_ms: u64,
}

impl Default for TapConfig {
    fn default() -> Self {
        TapConfig {
            stream_id: 1,
            service_id: PROGRAM_NUMBER as u16,
            with_tsinfo: false,
            es_level_abort: 48,
            stall_abort_ms: 5000,
            gap_log_ms: 150,
        }
    }
}

/// A private REMUX wired to the product's live 4K ES rings.
///
/// Construction builds the whole graph and leaves it running; `read_into`
/// returns plain 188-byte TS; dropping it tears everything down in the
/// product's own order. Every failure path releases whatever was already
/// built, because holding an input connection while not consuming is what
/// starves the product's decoder.
pub struct Tap {
    drv: Arc<XcDrv>,
    cfg: TapConfig,
    live: LiveEs,
    device: u32,
    tsout: u32,
    tsinfo: u32,
    conn_tsout: u32,
    conn_tsinfo: u32,
    conn_video: u32,
    conn_audio: u32,
    running: bool,
    streams_added: bool,
    program_added: bool,
    src: Option<XcReaderSource>,
    info_src: Option<XcReaderSource>,
    info_scratch: Vec<u8>,
    raw: Vec<u8>,
    unwrap: TtsUnwrap,
    last_data: Instant,
    pub raw_total: u64,
    pub longest_gap: Duration,
}

impl Tap {
    pub fn open(drv: Arc<XcDrv>, cfg: TapConfig) -> io::Result<Self> {
        let live = discover(&drv).map_err(io::Error::other)?;
        // Refuse to build into a half-rebuilt graph. Measured: right after a
        // channel change the ES rings exist but the product's audio decoder
        // has not reattached yet, and every ioctl against them then returns
        // XCE_TIMEOUT — which leaves a half-built REMUX behind that stops the
        // product from rebuilding at all. Both product decoders being attached
        // is the cheap, reliable signal that the chain is live.
        if live.video_readers == 0 || live.audio_readers == 0 {
            return Err(io::Error::other(format!(
                "the product's decode chain is not up yet (video readers {}, audio readers {})",
                live.video_readers, live.audio_readers
            )));
        }
        eprintln!(
            "remux4k: MMT_DEMUX={:#x} pipeline={:#x} video_es={:#x} (readers {}) audio_es={:#x} (readers {})",
            live.mmt_demux, live.pipeline, live.video_es, live.video_readers,
            live.audio_es, live.audio_readers
        );
        let mut tap = Tap {
            drv,
            cfg,
            live,
            device: 0,
            tsout: 0,
            tsinfo: 0,
            conn_tsout: 0,
            conn_tsinfo: 0,
            conn_video: 0,
            conn_audio: 0,
            running: false,
            streams_added: false,
            program_added: false,
            src: None,
            info_src: None,
            info_scratch: Vec::with_capacity(64 * 1024),
            raw: Vec::with_capacity(320 * 1024),
            unwrap: TtsUnwrap::new(),
            last_data: Instant::now(),
            raw_total: 0,
            longest_gap: Duration::from_millis(0),
        };
        if let Err(e) = tap.build() {
            tap.teardown();
            return Err(e);
        }
        Ok(tap)
    }

    /// `adsb`'s recording recipe with the recording removed.
    fn build(&mut self) -> io::Result<()> {
        let d = Arc::clone(&self.drv);
        self.device = d.create_remux()?;
        d.config_remux(self.device, u32::from(self.cfg.stream_id))?;
        self.tsout = d.create_remux_tsout_buffer()?;
        self.conn_tsout = d.connect_output(self.device, self.tsout)?;
        if self.cfg.with_tsinfo {
            self.tsinfo = d.create_remux_tsinfo_buffer()?;
            self.conn_tsinfo = d.connect_output(self.device, self.tsinfo)?;
        }
        // The first request the quota can refuse: -1035 here would mean the
        // live ES ring has no spare consumer slot.
        self.conn_video = d.connect_input(self.device, self.live.video_es)?;
        self.conn_audio = d.connect_input(self.device, self.live.audio_es)?;

        let ats_base = d.pipeline_get_time(self.live.pipeline).unwrap_or_else(|e| {
            eprintln!("mkra: warning: pipeline get_time: {e}; using 0 as the ATS base");
            0
        });
        let program = u32::from(self.cfg.service_id);
        d.remux_add_program(self.device, program, PCR_PID, PMT_PID, PCR_INTERVAL, ats_base)?;
        self.program_added = true;
        d.remux_config_video(self.device, self.conn_video)?;
        d.remux_add_video(
            self.device, self.conn_video, program, VIDEO_PID, VIDEO_STREAM_ID, VIDEO_STREAM_TYPE,
        )?;
        d.remux_add_audio(
            self.device, self.conn_audio, program, AUDIO_PID, AUDIO_STREAM_ID, AUDIO_STREAM_TYPE,
        )?;
        self.streams_added = true;
        d.remux_set_copy_permission(self.device, 0)?;
        d.set_pipeline(self.device, self.live.pipeline)?;
        d.set_buffer_ready(self.tsout)?;
        d.run_device(self.device)?;
        self.running = true;

        let reader = d.create_reader(self.tsout)?;
        self.src = Some(XcReaderSource::adopt(Arc::clone(&d), self.tsout, reader, 500));
        if self.tsinfo != 0 {
            d.set_buffer_ready(self.tsinfo)?;
            let info_reader = d.create_reader(self.tsinfo)?;
            self.info_src = Some(XcReaderSource::adopt(
                Arc::clone(&d),
                self.tsinfo,
                info_reader,
                20,
            ));
        }
        self.last_data = Instant::now();
        Ok(())
    }

    /// True once the tap has stopped producing and the live ring is backing
    /// up because of it.
    fn must_release(&self) -> Option<String> {
        let idle = self.last_data.elapsed();
        if idle >= Duration::from_millis(300) {
            if let Ok(st) = self.drv.sbuf_status_raw(self.live.video_es) {
                let level = st[5];
                if level >= self.cfg.es_level_abort {
                    return Some(format!(
                        "live video ES backed up to level {level:#x} during a {} ms gap",
                        idle.as_millis()
                    ));
                }
            }
        }
        if self.cfg.stall_abort_ms > 0
            && idle >= Duration::from_millis(self.cfg.stall_abort_ms)
        {
            return Some(format!("no output for {} ms", idle.as_millis()));
        }
        None
    }

    /// Reverse of `build`, in `remux_seq_stop` -> `remux_xcif_close_remux`
    /// order: readers, stop, remove streams, remove program, disconnect
    /// inputs then outputs, destroy buffers, destroy the device.
    fn teardown(&mut self) {
        self.src = None;
        self.info_src = None;
        let d = Arc::clone(&self.drv);
        if self.running {
            if let Err(e) = d.stop_device(self.device) {
                eprintln!("mkra: warning: stop REMUX {:#x}: {e}", self.device);
            }
            self.running = false;
        }
        if self.streams_added {
            for (label, conn) in [("video", self.conn_video), ("audio", self.conn_audio)] {
                if let Err(e) = d.remux_remove_stream(self.device, conn, 1) {
                    eprintln!("mkra: warning: remove {label} stream: {e}");
                }
            }
            self.streams_added = false;
        }
        if self.program_added {
            if let Err(e) = d.remux_remove_program(self.device, u32::from(self.cfg.service_id)) {
                eprintln!("mkra: warning: remove program: {e}");
            }
            self.program_added = false;
        }
        for (label, conn) in [
            ("video ES input", self.conn_video),
            ("audio ES input", self.conn_audio),
            ("TSINFO output", self.conn_tsinfo),
            ("TSOUT output", self.conn_tsout),
        ] {
            if conn != 0 {
                if let Err(e) = d.disconnect(self.device, conn) {
                    eprintln!("mkra: warning: disconnect {label} {conn:#x}: {e}");
                }
            }
        }
        self.conn_video = 0;
        self.conn_audio = 0;
        self.conn_tsinfo = 0;
        self.conn_tsout = 0;
        for (label, buffer) in [("TSINFO", self.tsinfo), ("TSOUT", self.tsout)] {
            if buffer != 0 {
                if let Err(e) = d.destroy_buffer(buffer) {
                    eprintln!("mkra: warning: destroy {label} {buffer:#x}: {e}");
                }
            }
        }
        self.tsinfo = 0;
        self.tsout = 0;
        if self.device != 0 {
            if let Err(e) = d.destroy_device(self.device) {
                eprintln!("mkra: warning: destroy REMUX {:#x}: {e}", self.device);
            }
            self.device = 0;
        }
    }
}

impl TsSource for Tap {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        if let Some(why) = self.must_release() {
            // Returning an error ends the session, which drops the tap and
            // frees the live rings. Holding on is what starves the product.
            return Err(io::Error::other(format!(
                "remux4k: releasing the live ES rings: {why} (raw_total={})",
                self.raw_total
            )));
        }
        if let Some(info) = self.info_src.as_mut() {
            self.info_scratch.clear();
            if let Err(e) = info.read_into(&mut self.info_scratch) {
                eprintln!("mkra: warning: drain TSINFO: {e}");
            }
        }
        self.raw.clear();
        let Some(src) = self.src.as_mut() else {
            return Err(io::Error::other("remux4k: TSOUT reader is gone"));
        };
        let n = src.read_into(&mut self.raw)?;
        if n == 0 {
            return Ok(0);
        }
        self.raw_total += n as u64;
        let gap = self.last_data.elapsed();
        if gap >= Duration::from_millis(self.cfg.gap_log_ms) {
            eprintln!(
                "remux4k: gap {} ms recovered at raw_total={}",
                gap.as_millis(),
                self.raw_total
            );
        }
        if gap > self.longest_gap {
            self.longest_gap = gap;
        }
        self.last_data = Instant::now();
        let before = out.len();
        let raw = std::mem::take(&mut self.raw);
        self.unwrap.push(&raw, out);
        self.raw = raw;
        Ok(out.len() - before)
    }

    fn describe(&self) -> String {
        format!(
            "BS4K REMUX tap (device {:#x}, TSOUT {:#x}, live video ES {:#x})",
            self.device, self.tsout, self.live.video_es
        )
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        eprintln!(
            "remux4k: closing tap after {} bytes (longest recovered gap {} ms, {} TTS packets, {} resyncs)",
            self.raw_total,
            self.longest_gap.as_millis(),
            self.unwrap.packets,
            self.unwrap.resyncs
        );
        self.teardown();
    }
}

//! MMT-SI parsing: the EPG, service names and channel table, straight from the
//! broadcast.
//!
//! Advanced BS/CS carries its service information as MMT-SI (ARIB STD-B60),
//! not as the MPEG-2 PSI/SI that a normal Japanese tuner emits. The product
//! caches complete sections, verbatim and back to back, in `/tmp/adsb/Adsb*.dat`
//! — no header, no index, no per-record wrapper — so parsing those files gives
//! us the same tables the box itself is working from.
//!
//! Layouts here were measured against real captures rather than taken from the
//! standard: ARIB STD-B60 is the reference; the parser below was cross-checked against
//! established them, and `testdata/*.bin` are real sections from a live antenna
//! feed. Where MMT-SI differs from the MPEG-2 forms people know, it is called
//! out — those differences are exactly where a from-memory implementation goes
//! wrong (text lengths are 16-bit, descriptor tags are 16-bit, and the tag
//! decides how wide the length field is).

use std::collections::BTreeMap;

// --- table ids ------------------------------------------------------------

pub const TID_TLV_NIT_ACTUAL: u8 = 0x40;
pub const TID_TLV_NIT_OTHER: u8 = 0x41;
pub const TID_MH_EIT_PF: u8 = 0x8B;
/// 0x8C..=0x93 basic schedule, 0x94..=0x9B extended schedule.
pub const TID_MH_EIT_SCH_FIRST: u8 = 0x8C;
pub const TID_MH_EIT_SCH_LAST: u8 = 0x9B;
pub const TID_MH_SDT_ACTUAL: u8 = 0x9F;
pub const TID_MH_SDT_OTHER: u8 = 0xA0;
pub const TID_MH_TOT: u8 = 0xA1;
pub const TID_AMT: u8 = 0xFE;

pub fn is_mh_eit(tid: u8) -> bool {
    tid == TID_MH_EIT_PF || (TID_MH_EIT_SCH_FIRST..=TID_MH_EIT_SCH_LAST).contains(&tid)
}

/// Sections that use the MPEG-2 "long form" header — `table_id(8)`,
/// `section_syntax_indicator(1)`, `reserved(3)`, `section_length(12)` — and end
/// with a CRC-32. Everything else in this firmware uses the MMT table form
/// (`table_id(8) version(8) length(16)`) and carries no CRC.
pub fn is_long_form(tid: u8) -> bool {
    (0x82..=0xA6).contains(&tid) || tid == TID_TLV_NIT_ACTUAL || tid == TID_TLV_NIT_OTHER
        || tid == TID_AMT
}

/// Total on-wire length of the section beginning at `buf[off]`, or `None` if
/// the header does not fit.
pub fn section_total_len(buf: &[u8], off: usize) -> Option<usize> {
    if is_long_form(*buf.get(off)?) {
        let n = ((*buf.get(off + 1)? as usize) << 8 | *buf.get(off + 2)? as usize) & 0x0FFF;
        Some(3 + n)
    } else {
        let n = (*buf.get(off + 2)? as usize) << 8 | *buf.get(off + 3)? as usize;
        Some(4 + n)
    }
}

// --- CRC-32/MPEG-2 --------------------------------------------------------

/// CRC-32/MPEG-2 over `data`. A well-formed long-form section including its
/// trailing CRC yields 0.
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
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

// --- section iteration ----------------------------------------------------

/// Walk the sections of a `/tmp/adsb/Adsb*.dat` slab.
///
/// The files are pre-allocated and zero filled, so the first `table_id == 0`
/// marks the end of valid data.
pub struct Sections<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Sections<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Sections { buf, off: 0 }
    }
}

impl<'a> Iterator for Sections<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.off + 4 > self.buf.len() || self.buf[self.off] == 0 {
            return None;
        }
        let total = section_total_len(self.buf, self.off)?;
        if total <= 4 || self.off + total > self.buf.len() {
            return None;
        }
        let sec = &self.buf[self.off..self.off + total];
        self.off += total;
        Some(sec)
    }
}

// --- byte helpers ---------------------------------------------------------

fn u16b(b: &[u8], i: usize) -> u16 {
    ((b[i] as u16) << 8) | b[i + 1] as u16
}

// --- descriptors ----------------------------------------------------------

pub const D_MH_VIDEO_COMPONENT: u16 = 0x8010;
pub const D_MH_CONTENT: u16 = 0x8012;
pub const D_MH_AUDIO_COMPONENT: u16 = 0x8014;
pub const D_MH_SERVICE: u16 = 0x8019;
pub const D_MH_SHORT_EVENT: u16 = 0xF001;
pub const D_MH_EXTENDED_EVENT: u16 = 0xF002;

pub const TD_NETWORK_NAME: u8 = 0x40;
pub const TD_SERVICE_LIST: u8 = 0x41;
pub const TD_SAT_DELIVERY: u8 = 0x43;
pub const TD_REMOTE_KEY: u8 = 0xCD;

/// Width of an MMT-SI `descriptor_length` field, which the tag range decides
/// (ARIB STD-B60). Getting this wrong desynchronises the whole loop, which is
/// why it is a function rather than a constant.
fn mmt_desc_len_width(tag: u16) -> usize {
    match tag {
        0x0000..=0x3FFF => 1,
        0x4000..=0x6FFF => 2,
        0x7000..=0xEFFF => 1,
        _ => 2, // 0xF000..=0xFFFF
    }
}

/// Iterate an MMT-SI descriptor loop (16-bit tags).
pub fn each_mmt_descriptor(b: &[u8], mut f: impl FnMut(u16, &[u8])) {
    let mut i = 0usize;
    while i + 2 <= b.len() {
        let tag = u16b(b, i);
        i += 2;
        let w = mmt_desc_len_width(tag);
        if i + w > b.len() {
            return;
        }
        let len = if w == 1 {
            b[i] as usize
        } else {
            u16b(b, i) as usize
        };
        i += w;
        if i + len > b.len() {
            return;
        }
        f(tag, &b[i..i + len]);
        i += len;
    }
}

/// Iterate a TLV-SI descriptor loop, which uses the DVB form: 8-bit tag,
/// 8-bit length.
pub fn each_tlv_descriptor(b: &[u8], mut f: impl FnMut(u8, &[u8])) {
    let mut i = 0usize;
    while i + 2 <= b.len() {
        let (tag, len) = (b[i], b[i + 1] as usize);
        i += 2;
        if i + len > b.len() {
            return;
        }
        f(tag, &b[i..i + len]);
        i += len;
    }
}

// --- text -----------------------------------------------------------------

/// Decode broadcast MMT-SI text.
///
/// ARIB STD-B60 mandates UTF-8 for advanced BS, so this is *not* the STD-B24
/// 8-bit encoding used elsewhere in this project. B24 control codes are still
/// embedded for line breaks and character sizing; drop them but keep real
/// characters such as U+3000.
///
/// The product's own NVRAM copies (`AdsbBsSlist.dat` and friends) *are* B24 and
/// must not be passed here.
pub fn decode_text(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let o = c as u32;
        if o == 0x0A || o == 0x0D {
            out.push('\n');
        } else if o < 0x20 || (0x7F..=0x9F).contains(&o) {
            continue;
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// --- time -----------------------------------------------------------------

const MJD_UNIX_EPOCH: i64 = 40587;
const JST_OFFSET_SECS: i64 = 9 * 3600;

fn bcd2(b: u8) -> i64 {
    (b >> 4) as i64 * 10 + (b & 0x0F) as i64
}

/// 40-bit `start_time` (16-bit MJD + 24-bit BCD HHMMSS, JST) → Unix seconds UTC.
/// All-ones means "undefined".
pub fn parse_mjd_bcd_time(b: &[u8]) -> Option<i64> {
    if b.len() < 5 || b[..5] == [0xFF; 5] {
        return None;
    }
    let mjd = ((b[0] as i64) << 8) | b[1] as i64;
    let secs = bcd2(b[2]) * 3600 + bcd2(b[3]) * 60 + bcd2(b[4]);
    Some((mjd - MJD_UNIX_EPOCH) * 86400 + secs - JST_OFFSET_SECS)
}

/// 24-bit BCD HHMMSS → seconds. All-ones means "undefined".
pub fn parse_bcd_duration(b: &[u8]) -> Option<i64> {
    if b.len() < 3 || b[..3] == [0xFF; 3] {
        return None;
    }
    Some(bcd2(b[0]) * 3600 + bcd2(b[1]) * 60 + bcd2(b[2]))
}

// --- Mirakurun id composition --------------------------------------------

/// Mirakurun composes ids by decimal concatenation, not bit packing.
pub fn mirakurun_service_id(network_id: u16, service_id: u16) -> u64 {
    network_id as u64 * 100_000 + service_id as u64
}

pub fn mirakurun_program_id(network_id: u16, service_id: u16, event_id: u16) -> u64 {
    mirakurun_service_id(network_id, service_id) * 100_000 + event_id as u64
}

// --- parsed shapes --------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Genre {
    pub lv1: u8,
    pub lv2: u8,
    pub un1: u8,
    pub un2: u8,
}

/// MH-video_component_descriptor (0x8010).
///
/// ```text
/// reserved(4) | video_resolution(4)
/// reserved(2) | video_scan_flag(1) | video_frame_rate(5)
/// component_tag(16)                  <- 16 bit; the MPEG-2 TS form is 8 bit
/// video_transfer_characteristics(4) | reserved(4)
/// ISO_639_language_code(24) | text
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoInfo {
    pub resolution: Option<&'static str>,
    pub aspect: Option<&'static str>,
    pub progressive: bool,
    pub frame_rate_code: u8,
    pub component_tag: u16,
    pub transfer_characteristics: Option<&'static str>,
}

/// MH-audio_component_descriptor (0x8014).
///
/// ```text
/// reserved(4) | stream_content(4)     (3 = MPEG-4 audio)
/// component_type(8) | component_tag(16) | stream_type(8) | simulcast_group_tag(8)
/// ES_multi_lingual(1) | main_component(1) | quality(2) | sampling_rate(3) | reserved(1)
/// ISO_639_language_code(24) [| second ISO_639 if ES_multi_lingual] | text
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioInfo {
    pub stream_content: u8,
    pub component_type: u8,
    pub component_tag: u16,
    pub is_main: bool,
    pub sampling_rate: Option<u32>,
    pub langs: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub id: u64,
    pub event_id: u16,
    pub service_id: u16,
    pub network_id: u16,
    /// Milliseconds since the Unix epoch, as Mirakurun reports times.
    pub start_at_ms: i64,
    pub duration_ms: i64,
    pub is_free: bool,
    pub name: String,
    pub description: String,
    pub extended: Vec<(String, String)>,
    pub genres: Vec<Genre>,
    pub video: Option<VideoInfo>,
    pub audios: Vec<AudioInfo>,
}

impl Program {
    /// Fold a later sighting of the same event into this one.
    ///
    /// Timing and the free-CA flag are taken from the newer section — those are
    /// the fields a schedule update exists to change. Everything descriptive is
    /// only filled in, never cleared, because a section that omits a descriptor
    /// is saying nothing about it rather than saying it is empty.
    fn absorb(&mut self, other: Program) {
        self.start_at_ms = other.start_at_ms;
        self.duration_ms = other.duration_ms;
        self.is_free = other.is_free;

        if !other.name.is_empty() {
            self.name = other.name;
        }
        if !other.description.is_empty() {
            self.description = other.description;
        }
        if !other.genres.is_empty() {
            self.genres = other.genres;
        }
        if other.video.is_some() {
            self.video = other.video;
        }
        if !other.audios.is_empty() {
            self.audios = other.audios;
        }
        // `extended` is a union keyed by caption. Each event carries the same
        // caption across several 0xF002 descriptors, so a plain extend would
        // duplicate every row.
        for (k, v) in other.extended {
            match self.extended.iter_mut().find(|(ek, _)| *ek == k) {
                Some(slot) if slot.1.is_empty() => slot.1 = v,
                Some(_) => {}
                None => self.extended.push((k, v)),
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SiService {
    pub service_id: u16,
    pub network_id: u16,
    pub tlv_stream_id: u16,
    pub service_type: u8,
    pub name: String,
    pub provider: String,
}

#[derive(Debug, Clone, Default)]
pub struct Transponder {
    pub tlv_stream_id: u16,
    pub network_id: u16,
    /// Downlink frequency in kHz — the unit `psfc_fed_AdbsSetTuningStart` takes.
    pub freq_khz: u32,
    pub symbol_rate_kbaud: u32,
    pub polarisation: u8,
    pub service_ids: Vec<u16>,
}

/// Everything recovered from one pass over a set of `.dat` slabs.
#[derive(Debug, Clone, Default)]
pub struct SiDb {
    pub network_id: u16,
    pub network_name: String,
    pub transponders: Vec<Transponder>,
    pub services: BTreeMap<u64, SiService>,
    pub programs: BTreeMap<u64, Program>,
    /// Broadcast clock from MH-TOT, Unix seconds UTC.
    pub broadcast_time: Option<i64>,
    /// Remote-control key id per service, from the TLV-NIT 0xCD descriptor.
    pub remote_keys: BTreeMap<u16, u8>,
}

const RESOLUTIONS: [Option<&str>; 8] = [
    None,
    Some("180p"),
    Some("240p"),
    Some("480p"),
    Some("720p"),
    Some("1080p"),
    Some("2160p"),
    Some("4320p"),
];

const ASPECTS: [Option<&str>; 5] = [
    None,
    Some("4:3"),
    Some("16:9(pan)"),
    Some("16:9"),
    Some(">16:9"),
];

const TRANSFER: [Option<&str>; 6] = [
    None,
    Some("BT.709"),
    Some("IEC61966-2-4"),
    Some("BT.2020(SDR)"),
    Some("SMPTE2084(PQ)"),
    Some("BT.2100(HLG)"),
];

/// Name a 4-bit coded field, or `None` if the broadcaster used a value this
/// build does not know. Every one of these fields is a nibble, so no table can
/// be assumed to cover the range -- indexing them directly is an out-of-bounds
/// panic waiting for a stream we have not seen.
fn lookup(table: &[Option<&'static str>], code: u8) -> Option<&'static str> {
    table.get(code as usize).copied().flatten()
}

fn sampling_rate(code: u8) -> Option<u32> {
    match code {
        1 => Some(16_000),
        2 => Some(22_050),
        3 => Some(24_000),
        5 => Some(32_000),
        6 => Some(44_100),
        7 => Some(48_000),
        _ => None,
    }
}

impl SiDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one slab (the whole contents of an `Adsb*.dat` file). Safe to call
    /// repeatedly; later versions of a table overwrite earlier ones by id.
    pub fn ingest(&mut self, slab: &[u8]) {
        for sec in Sections::new(slab) {
            self.ingest_section(sec);
        }
    }

    pub fn ingest_section(&mut self, sec: &[u8]) {
        let tid = sec[0];
        // Long-form sections carry a CRC; a bad one means we resynchronised onto
        // garbage and must not trust anything we would read out of it.
        if is_long_form(tid) && crc32_mpeg(sec) != 0 {
            return;
        }
        if is_mh_eit(tid) {
            self.parse_mh_eit(sec);
        } else if tid == TID_MH_SDT_ACTUAL || tid == TID_MH_SDT_OTHER {
            self.parse_mh_sdt(sec);
        } else if tid == TID_TLV_NIT_ACTUAL {
            self.parse_tlv_nit(sec);
        } else if tid == TID_MH_TOT {
            self.broadcast_time = parse_mjd_bcd_time(&sec[3..]);
        }
    }

    /// MH-EIT: `service_id` at +3, `TLV_stream_id` at +8, `original_network_id`
    /// at +10, event loop from +14 until `section_length - 4` (the CRC).
    fn parse_mh_eit(&mut self, sec: &[u8]) {
        if sec.len() < 18 {
            return;
        }
        let seclen = (u16b(sec, 1) & 0x0FFF) as usize;
        let service_id = u16b(sec, 3);
        let network_id = u16b(sec, 10);
        let end = (3 + seclen).saturating_sub(4).min(sec.len());
        let mut p = 14usize;
        while p + 12 <= end {
            let event_id = u16b(sec, p);
            let start = parse_mjd_bcd_time(&sec[p + 2..]);
            let dur = parse_bcd_duration(&sec[p + 7..]);
            let free_ca = (sec[p + 10] >> 4) & 1;
            let dll = (u16b(sec, p + 10) & 0x0FFF) as usize;
            p += 12;
            if p + dll > end {
                return;
            }
            let desc = &sec[p..p + dll];
            p += dll;

            let (Some(start), Some(dur)) = (start, dur) else {
                continue;
            };
            let mut prog = Program {
                id: mirakurun_program_id(network_id, service_id, event_id),
                event_id,
                service_id,
                network_id,
                start_at_ms: start * 1000,
                duration_ms: dur * 1000,
                is_free: free_ca == 0,
                ..Default::default()
            };
            let mut ext_items: Vec<(String, String)> = Vec::new();
            each_mmt_descriptor(desc, |tag, pl| match tag {
                D_MH_SHORT_EVENT => {
                    // ISO-639(3) | name_len(8) | name | text_len(16) | text.
                    // The 16-bit text length is the MMT-SI difference from the
                    // MPEG-2 short_event_descriptor.
                    if pl.len() < 4 {
                        return;
                    }
                    let nl = pl[3] as usize;
                    if 4 + nl + 2 > pl.len() {
                        return;
                    }
                    prog.name = decode_text(&pl[4..4 + nl]);
                    let o = 4 + nl;
                    let tl = u16b(pl, o) as usize;
                    if o + 2 + tl <= pl.len() {
                        prog.description = decode_text(&pl[o + 2..o + 2 + tl]);
                    }
                }
                D_MH_EXTENDED_EVENT => {
                    // num/last(8) | ISO-639(3) | length_of_items(16) | items | text
                    if pl.len() < 6 {
                        return;
                    }
                    let loi = u16b(pl, 4) as usize;
                    let mut o = 6usize;
                    let stop = (o + loi).min(pl.len());
                    while o + 1 <= stop {
                        let dl = pl[o] as usize;
                        o += 1;
                        if o + dl + 2 > pl.len() {
                            return;
                        }
                        let key = decode_text(&pl[o..o + dl]);
                        o += dl;
                        let il = u16b(pl, o) as usize;
                        o += 2;
                        if o + il > pl.len() {
                            return;
                        }
                        ext_items.push((key, decode_text(&pl[o..o + il])));
                        o += il;
                    }
                }
                D_MH_CONTENT => {
                    for c in pl.chunks_exact(2) {
                        prog.genres.push(Genre {
                            lv1: c[0] >> 4,
                            lv2: c[0] & 0x0F,
                            un1: c[1] >> 4,
                            un2: c[1] & 0x0F,
                        });
                    }
                }
                D_MH_VIDEO_COMPONENT => {
                    if pl.len() >= 5 {
                        prog.video = Some(VideoInfo {
                            // video_resolution is a 4-bit field, so it can hold
                            // values the table does not name. Index fallibly:
                            // an unknown resolution must degrade to None, not
                            // panic the server on the device.
                            resolution: lookup(&RESOLUTIONS, pl[0] >> 4),
                            aspect: lookup(&ASPECTS, pl[0] & 0x0F),
                            progressive: (pl[1] >> 5) & 1 == 1,
                            frame_rate_code: pl[1] & 0x1F,
                            component_tag: u16b(pl, 2),
                            transfer_characteristics: lookup(&TRANSFER, pl[4] >> 4),
                        });
                    }
                }
                D_MH_AUDIO_COMPONENT => {
                    if pl.len() >= 10 {
                        let multilingual = pl[6] >> 7 == 1;
                        let mut langs = vec![String::from_utf8_lossy(&pl[7..10]).into_owned()];
                        if multilingual && pl.len() >= 13 {
                            langs.push(String::from_utf8_lossy(&pl[10..13]).into_owned());
                        }
                        prog.audios.push(AudioInfo {
                            stream_content: pl[0] & 0x0F,
                            component_type: pl[1],
                            component_tag: u16b(pl, 2),
                            is_main: (pl[6] >> 6) & 1 == 1,
                            sampling_rate: sampling_rate((pl[6] >> 1) & 0x07),
                            langs,
                        });
                    }
                }
                _ => {}
            });
            prog.extended = ext_items;
            // The same event appears in more than one table -- present/following
            // (0x8B), basic schedule (0x8C..) and extended schedule (0x94..) --
            // and they do not carry the same descriptors. Overwriting outright
            // means whichever section arrived last decides whether the guide has
            // a title, so merge instead: newer timing wins, existing text is
            // never blanked.
            match self.programs.get_mut(&prog.id) {
                Some(existing) => existing.absorb(prog),
                None => {
                    self.programs.insert(prog.id, prog);
                }
            }
        }
    }

    /// MH-SDT: `TLV_stream_id` at +3, `original_network_id` at +8, service loop
    /// from +11.
    fn parse_mh_sdt(&mut self, sec: &[u8]) {
        if sec.len() < 16 {
            return;
        }
        let seclen = (u16b(sec, 1) & 0x0FFF) as usize;
        let tlv_stream_id = u16b(sec, 3);
        let network_id = u16b(sec, 8);
        let end = (3 + seclen).saturating_sub(4).min(sec.len());
        let mut p = 11usize;
        while p + 5 <= end {
            let service_id = u16b(sec, p);
            let dll = (u16b(sec, p + 3) & 0x0FFF) as usize;
            p += 5;
            if p + dll > end {
                return;
            }
            let desc = &sec[p..p + dll];
            p += dll;

            let mut rec = SiService {
                service_id,
                network_id,
                tlv_stream_id,
                ..Default::default()
            };
            each_mmt_descriptor(desc, |tag, pl| {
                if tag == D_MH_SERVICE && pl.len() >= 2 {
                    // service_type(8) | provider_len(8) | provider | name_len(8) | name
                    rec.service_type = pl[0];
                    let plen = pl[1] as usize;
                    if 2 + plen < pl.len() {
                        rec.provider = decode_text(&pl[2..2 + plen]);
                        let o = 2 + plen;
                        let nlen = pl[o] as usize;
                        if o + 1 + nlen <= pl.len() {
                            rec.name = decode_text(&pl[o + 1..o + 1 + nlen]);
                        }
                    }
                }
            });
            let key = mirakurun_service_id(network_id, service_id);
            // Keep a name we already have if this section did not carry one.
            match self.services.get_mut(&key) {
                Some(existing) if rec.name.is_empty() => {
                    existing.tlv_stream_id = tlv_stream_id;
                }
                _ => {
                    self.services.insert(key, rec);
                }
            }
        }
    }

    /// TLV-NIT: `network_id` at +3, network descriptors at +8, then the
    /// TLV stream loop.
    fn parse_tlv_nit(&mut self, sec: &[u8]) {
        if sec.len() < 14 {
            return;
        }
        let seclen = (u16b(sec, 1) & 0x0FFF) as usize;
        self.network_id = u16b(sec, 3);
        let end = (3 + seclen).saturating_sub(4).min(sec.len());
        let ndl = (u16b(sec, 8) & 0x0FFF) as usize;
        let mut p = 10usize;
        if p + ndl > end {
            return;
        }
        let net_desc = &sec[p..p + ndl];
        p += ndl;

        let mut name = String::new();
        let mut keys: BTreeMap<u16, u8> = BTreeMap::new();
        each_tlv_descriptor(net_desc, |tag, pl| match tag {
            TD_NETWORK_NAME => name = decode_text(pl),
            TD_REMOTE_KEY => {
                // count(8) then N x { key_id(8), service_id(16), 0xFFFF }
                if pl.is_empty() {
                    return;
                }
                let n = pl[0] as usize;
                for i in 0..n {
                    let o = 1 + i * 5;
                    if o + 3 > pl.len() {
                        break;
                    }
                    keys.insert(u16b(pl, o + 1), pl[o]);
                }
            }
            _ => {}
        });
        if !name.is_empty() {
            self.network_name = name;
        }
        if !keys.is_empty() {
            self.remote_keys = keys;
        }

        if p + 2 > end {
            return;
        }
        let loop_len = (u16b(sec, p) & 0x0FFF) as usize;
        p += 2;
        let loop_end = (p + loop_len).min(end);
        let mut found = Vec::new();
        while p + 6 <= loop_end {
            let tlv_stream_id = u16b(sec, p);
            let network_id = u16b(sec, p + 2);
            let dll = (u16b(sec, p + 4) & 0x0FFF) as usize;
            p += 6;
            if p + dll > loop_end {
                break;
            }
            let desc = &sec[p..p + dll];
            p += dll;

            let mut tp = Transponder {
                tlv_stream_id,
                network_id,
                ..Default::default()
            };
            each_tlv_descriptor(desc, |tag, pl| match tag {
                TD_SERVICE_LIST => {
                    for c in pl.chunks_exact(3) {
                        tp.service_ids.push(((c[0] as u16) << 8) | c[1] as u16);
                    }
                }
                TD_SAT_DELIVERY if pl.len() >= 11 => {
                    // frequency: 8 BCD digits, GHz with 4 decimals
                    //   011842 56 -> 11.84256 GHz -> 11_842_560 kHz
                    let mut f: u32 = 0;
                    for &b in &pl[0..4] {
                        f = f * 100 + (b >> 4) as u32 * 10 + (b & 0x0F) as u32;
                    }
                    tp.freq_khz = f * 10; // 8 digits are 0.1 kHz units of GHz
                    tp.polarisation = (pl[6] >> 5) & 3;
                    // symbol rate: 7 BCD digits, Mbaud with 4 decimals
                    //   0337561 -> 33.7561 Mbaud -> 33_756 kBaud
                    let mut s: u32 = 0;
                    for &b in &pl[7..10] {
                        s = s * 100 + (b >> 4) as u32 * 10 + (b & 0x0F) as u32;
                    }
                    s = s * 10 + (pl[10] >> 4) as u32;
                    tp.symbol_rate_kbaud = s / 10;
                }
                _ => {}
            });
            found.push(tp);
        }
        if !found.is_empty() {
            self.transponders = found;
        }
    }

    /// Feed bytes read straight off a hardware section ring.
    ///
    /// This is the source that actually works in practice. The product only
    /// rewrites `/tmp/adsb/Adsb*.dat` while it is presenting a service — on a
    /// box with no display attached those files can stay empty for the whole
    /// session — but the section filters run whenever the frontend is locked,
    /// so the rings carry SI even in standby.
    ///
    /// Ring records are framed (`flags | flags | 00 | u16be length | section`),
    /// but we deliberately do not trust that framing: a passive tap starts at an
    /// arbitrary offset and can be overtaken mid-record. Instead we scan for a
    /// byte that begins a section whose CRC-32 comes out zero, which is
    /// self-validating and needs no framing assumption at all.
    ///
    /// Returns how many sections were accepted.
    pub fn ingest_ring(&mut self, buf: &[u8]) -> usize {
        let mut off = 0usize;
        let mut n = 0usize;
        while off + 8 <= buf.len() {
            let tid = buf[off];
            // Only long-form tables are self-checking, and they are the ones
            // carrying everything we want (EIT/SDT/NIT/TOT/AMT).
            if tid == 0 || !is_long_form(tid) {
                off += 1;
                continue;
            }
            let Some(total) = section_total_len(buf, off) else {
                off += 1;
                continue;
            };
            if total <= 8 || off + total > buf.len() {
                off += 1;
                continue;
            }
            let sec = &buf[off..off + total];
            if crc32_mpeg(sec) != 0 {
                off += 1;
                continue;
            }
            self.ingest_section(sec);
            n += 1;
            off += total;
        }
        n
    }

    /// Programs for one service, ordered by start time.
    pub fn programs_of(&self, network_id: u16, service_id: u16) -> Vec<&Program> {
        let mut v: Vec<&Program> = self
            .programs
            .values()
            .filter(|p| p.network_id == network_id && p.service_id == service_id)
            .collect();
        v.sort_by_key(|p| p.start_at_ms);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real sections captured from a live antenna feed on 2026-07-30.
    const EIT_PF: &[u8] = include_bytes!("../testdata/mh_eit_pf.bin");
    const EIT_SCH: &[u8] = include_bytes!("../testdata/mh_eit_sched.bin");
    const SDT: &[u8] = include_bytes!("../testdata/mh_sdt_actual.bin");
    const NIT: &[u8] = include_bytes!("../testdata/tlv_nit_actual.bin");

    #[test]
    fn captured_sections_have_the_length_their_header_claims() {
        for (name, sec) in [("eit_pf", EIT_PF), ("eit_sch", EIT_SCH), ("sdt", SDT), ("nit", NIT)] {
            assert_eq!(
                section_total_len(sec, 0),
                Some(sec.len()),
                "{name}: header length disagrees with the captured section"
            );
        }
    }

    #[test]
    fn captured_sections_pass_crc32_mpeg() {
        // If this ever fails the CRC implementation is wrong, not the capture:
        // these bytes came off a working receiver.
        for (name, sec) in [("eit_pf", EIT_PF), ("eit_sch", EIT_SCH), ("sdt", SDT), ("nit", NIT)] {
            assert!(is_long_form(sec[0]), "{name} should be long form");
            assert_eq!(crc32_mpeg(sec), 0, "{name}: CRC mismatch");
        }
    }

    #[test]
    fn descriptor_length_width_follows_the_tag_range() {
        assert_eq!(mmt_desc_len_width(0x0001), 1);
        assert_eq!(mmt_desc_len_width(0x4000), 2);
        assert_eq!(mmt_desc_len_width(D_MH_VIDEO_COMPONENT), 1);
        assert_eq!(mmt_desc_len_width(D_MH_SERVICE), 1);
        // The two that carry programme text are 16-bit; treating them as 8-bit
        // silently truncates every title.
        assert_eq!(mmt_desc_len_width(D_MH_SHORT_EVENT), 2);
        assert_eq!(mmt_desc_len_width(D_MH_EXTENDED_EVENT), 2);
    }

    #[test]
    fn mh_eit_yields_named_events_with_sane_times() {
        let mut db = SiDb::new();
        db.ingest_section(EIT_PF);
        db.ingest_section(EIT_SCH);
        assert!(db.programs.len() >= 2, "got {} programs", db.programs.len());

        let named = db.programs.values().filter(|p| !p.name.is_empty()).count();
        assert!(named > 0, "no event carried a decodable name");

        for p in db.programs.values() {
            assert_eq!(p.network_id, 11);
            assert_eq!(p.id, mirakurun_program_id(p.network_id, p.service_id, p.event_id));
            // 2020-01-01 .. 2040-01-01, in ms.
            assert!(
                (1_577_836_800_000..2_208_988_800_000).contains(&p.start_at_ms),
                "start {} out of range for event {}",
                p.start_at_ms,
                p.event_id
            );
            assert!(p.duration_ms > 0 && p.duration_ms <= 24 * 3600 * 1000);
        }
    }

    #[test]
    fn event_text_decodes_as_utf8_japanese() {
        let mut db = SiDb::new();
        db.ingest_section(EIT_SCH);
        let with_text = db
            .programs
            .values()
            .find(|p| !p.name.is_empty() && !p.description.is_empty())
            .expect("no event with both a name and a description");
        // Real broadcast text: must contain non-ASCII and no control characters.
        assert!(with_text.name.chars().any(|c| (c as u32) > 0x7F));
        assert!(!with_text.name.chars().any(|c| (c as u32) < 0x20));
        assert!(with_text.description.chars().count() > 5);
    }

    #[test]
    fn advanced_bs_events_are_2160p_with_japanese_audio() {
        let mut db = SiDb::new();
        db.ingest_section(EIT_SCH);
        let p = db
            .programs
            .values()
            .find(|p| p.video.is_some() && !p.audios.is_empty())
            .expect("no event with both a video and an audio component");
        let v = p.video.as_ref().unwrap();
        // Advanced BS is 4K/59.94p; frame_rate_code 8 is 60000/1001.
        assert_eq!(v.resolution, Some("2160p"));
        assert_eq!(v.aspect, Some("16:9"));
        assert!(v.progressive);
        assert_eq!(v.frame_rate_code, 8);
        assert!(matches!(
            v.transfer_characteristics,
            Some("BT.2020(SDR)") | Some("BT.2100(HLG)")
        ));

        let a = &p.audios[0];
        assert_eq!(a.stream_content, 3); // MPEG-4 audio
        assert_eq!(a.sampling_rate, Some(48_000));
        assert!(a.is_main);
        assert_eq!(a.langs[0], "jpn");
    }

    #[test]
    fn tlv_nit_recovers_the_transponder_plan() {
        let mut db = SiDb::new();
        db.ingest_section(NIT);
        assert_eq!(db.network_id, 0x000B);
        assert!(db.network_name.contains('高'), "name={}", db.network_name);
        assert!(!db.transponders.is_empty());

        // Cross-check against the frequencies recovered independently from the
        // product's NVRAM (docs/HOW_IT_WORKS.md).
        let bs17 = db
            .transponders
            .iter()
            .find(|t| t.tlv_stream_id == 0xB110)
            .expect("BS-17 / TSID B110 missing");
        assert_eq!(bs17.freq_khz, 12_034_360);
        assert_eq!(bs17.symbol_rate_kbaud, 33_756);
        assert!(bs17.service_ids.contains(&101));

        let bs7 = db
            .transponders
            .iter()
            .find(|t| t.tlv_stream_id == 0xB070)
            .expect("BS-7 / TSID B070 missing");
        assert_eq!(bs7.freq_khz, 11_842_560);
        assert!(bs7.service_ids.contains(&151));
    }

    #[test]
    fn mh_sdt_names_services() {
        let mut db = SiDb::new();
        db.ingest_section(SDT);
        assert!(!db.services.is_empty());
        for (key, s) in &db.services {
            assert_eq!(*key, mirakurun_service_id(s.network_id, s.service_id));
            assert_eq!(s.network_id, 11);
        }
    }

    // 12 KB read off the live MH-EIT ring over HTTP (`--passive --raw-stream`).
    const RING_TAP: &[u8] = include_bytes!("../testdata/section_ring_tap.bin");

    #[test]
    fn a_raw_ring_tap_resynchronises_and_yields_programs() {
        let mut db = SiDb::new();
        let n = db.ingest_ring(RING_TAP);
        assert!(n >= 8, "only {n} sections recovered from the ring tap");
        assert!(!db.programs.is_empty());
        assert!(db.programs.values().any(|p| !p.name.is_empty()));
    }

    #[test]
    fn ring_ingest_starting_mid_section_still_recovers() {
        // A passive tap has no idea where a record begins. Chop the fixture at
        // a deliberately awkward point and check we still resynchronise.
        let mut db = SiDb::new();
        let n = db.ingest_ring(&RING_TAP[37..]);
        assert!(n >= 6, "resync failed: only {n} sections");
        assert!(!db.programs.is_empty());
    }

    #[test]
    fn ring_ingest_rejects_pure_noise() {
        // Deterministic pseudo-noise; must not produce a single "section".
        let mut noise = vec![0u8; 8192];
        let mut x: u32 = 0x1234_5678;
        for b in noise.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 16) as u8;
        }
        let mut db = SiDb::new();
        assert_eq!(db.ingest_ring(&noise), 0);
        assert!(db.programs.is_empty());
    }

    #[test]
    fn a_schedule_section_does_not_blank_a_richer_present_following_entry() {
        // Ingest order matters only if the merge is wrong. Both orders must
        // produce the same, non-empty, text for any event present in both.
        let mut a = SiDb::new();
        a.ingest_section(EIT_PF);
        a.ingest_section(EIT_SCH);
        let mut b = SiDb::new();
        b.ingest_section(EIT_SCH);
        b.ingest_section(EIT_PF);

        assert_eq!(a.programs.len(), b.programs.len());
        let named_a = a.programs.values().filter(|p| !p.name.is_empty()).count();
        let named_b = b.programs.values().filter(|p| !p.name.is_empty()).count();
        assert_eq!(named_a, named_b, "ingest order changed how many events are named");

        // And merging must never lose a title that a single source had.
        let mut pf_only = SiDb::new();
        pf_only.ingest_section(EIT_PF);
        for (id, p) in &pf_only.programs {
            if p.name.is_empty() {
                continue;
            }
            assert!(
                !a.programs[id].name.is_empty(),
                "event {id} lost its name when the schedule section was merged"
            );
        }
    }

    #[test]
    fn extended_text_is_unioned_without_duplicating_captions() {
        let mut db = SiDb::new();
        db.ingest_section(EIT_PF);
        db.ingest_section(EIT_SCH);
        db.ingest_section(EIT_PF); // a carousel repeat
        for p in db.programs.values() {
            let mut keys: Vec<&str> = p.extended.iter().map(|(k, _)| k.as_str()).collect();
            let before = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(before, keys.len(), "duplicate caption in event {}", p.id);
        }
    }

    #[test]
    fn an_unknown_coded_field_degrades_instead_of_panicking() {
        // video_resolution is 4 bits but only 0..7 are named; indexing the
        // table directly would panic on any stream using 8..15.
        assert_eq!(lookup(&RESOLUTIONS, 6), Some("2160p"));
        assert_eq!(lookup(&RESOLUTIONS, 15), None);
        assert_eq!(lookup(&ASPECTS, 15), None);
        assert_eq!(lookup(&TRANSFER, 15), None);

        // Exercise it through the real parser: force the resolution nibble to
        // an unnamed value in a copy of a real section and re-CRC it.
        let mut sec = EIT_SCH.to_vec();
        let mut patched = false;
        for i in 0..sec.len().saturating_sub(4) {
            if u16b(&sec, i) == D_MH_VIDEO_COMPONENT {
                sec[i + 3] = 0xF3; // resolution nibble 15
                patched = true;
                break;
            }
        }
        assert!(patched, "fixture has no video component descriptor");
        let n = sec.len();
        let crc = crc32_mpeg(&sec[..n - 4]);
        sec[n - 4..].copy_from_slice(&crc.to_be_bytes());

        let mut db = SiDb::new();
        db.ingest_section(&sec); // must not panic
        assert!(db
            .programs
            .values()
            .any(|p| p.video.as_ref().is_some_and(|v| v.resolution.is_none())));
    }

    #[test]
    fn mirakurun_ids_concatenate_decimally() {
        assert_eq!(mirakurun_service_id(11, 101), 1_100_101);
        assert_eq!(mirakurun_program_id(11, 101, 21_408), 110_010_121_408);
    }

    #[test]
    fn time_conversion_matches_a_known_broadcast_timestamp() {
        // 2026-07-30T18:00:00+09:00 == 1785402000 UTC seconds.
        // MJD for 2026-07-30 is 61251.
        let b = [0xEF, 0x43, 0x18, 0x00, 0x00];
        assert_eq!(parse_mjd_bcd_time(&b), Some(1_785_402_000));
        assert_eq!(parse_bcd_duration(&[0x00, 0x59, 0x00]), Some(3540));
        assert_eq!(parse_mjd_bcd_time(&[0xFF; 5]), None);
        assert_eq!(parse_bcd_duration(&[0xFF; 3]), None);
    }

    #[test]
    fn a_zero_filled_slab_terminates_the_section_walk() {
        let mut slab = EIT_PF.to_vec();
        slab.resize(EIT_PF.len() + 4096, 0);
        assert_eq!(Sections::new(&slab).count(), 1);
    }

    #[test]
    fn a_corrupt_section_is_dropped_rather_than_half_parsed() {
        let mut bad = EIT_SCH.to_vec();
        bad[40] ^= 0xFF; // break the CRC
        let mut db = SiDb::new();
        db.ingest_section(&bad);
        assert!(db.programs.is_empty());
    }

    #[test]
    fn text_decoding_strips_arib_control_codes() {
        assert_eq!(decode_text(b"\x0eABC\x0f"), "ABC");
        // U+3000 counts as whitespace, so it trims at the edges like any other
        // space but survives inside a name -- which is what service names such
        // as "ＮＨＫ　ＢＳＰ４Ｋ" depend on.
        assert_eq!(decode_text("　ＮＨＫ　ＢＳＰ４Ｋ　".as_bytes()), "ＮＨＫ　ＢＳＰ４Ｋ");
        assert_eq!(decode_text(b"a\x0db"), "a\nb");
    }
}

// --- which service is the box actually showing --------------------------

/// The service the product currently has selected.
///
/// `adsb` keeps its filtered MMT-SI in `/tmp/adsb` as pre-allocated slabs that
/// it rewrites in place. Two of them answer the question outright, with no
/// ioctl and no contact with the driver at all:
///
/// * `AdsbSiMhSdtActual.dat` — MH-SDT for the tuned TLV stream. Its header
///   carries the TLV stream id and the first service loop entry is the tuned
///   service (measured: `9f f0 59 | b072 | c3 00 00 | 000b | ff | 008d`).
/// * `AdsbSiMpt.dat` — the MPT of the *selected* package only. Its
///   `MMT_package_id` equals that service id for broadcast (measured: `008d`).
///
/// Both are cross-checked here, because agreeing on 141 from two independently
/// filtered tables is much stronger than trusting either alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveService {
    pub tlv_stream_id: u16,
    pub original_network_id: u16,
    pub service_id: u16,
}

pub const ADSB_SI_DIR: &str = "/tmp/adsb";

/// Parse `AdsbSiMhSdtActual.dat`'s header. Layout is ARIB STD-B60 MH-SDT:
/// `table_id(1) | flags+length(2) | TLV_stream_id(2) | version/flags(3) |
///  original_network_id(2) | reserved(1) | service_id(2) ...`
fn parse_mh_sdt_actual(buf: &[u8]) -> Option<LiveService> {
    if buf.len() < 15 || buf[0] != TID_MH_SDT_ACTUAL {
        return None;
    }
    let total = section_total_len(buf, 0)?;
    if total > buf.len() || crc32_mpeg(&buf[..total]) != 0 {
        return None;
    }
    Some(LiveService {
        tlv_stream_id: u16::from_be_bytes([buf[3], buf[4]]),
        original_network_id: u16::from_be_bytes([buf[8], buf[9]]),
        service_id: u16::from_be_bytes([buf[11], buf[12]]),
    })
}

/// `MMT_package_id` from `AdsbSiMpt.dat`: `table_id(1) | version(1) |
/// length(2) | MPT_mode(1) | MMT_package_id_length(1) | MMT_package_id(n)`.
/// Broadcast uses a two-byte id that equals the service id.
fn parse_mpt_package_id(buf: &[u8]) -> Option<u16> {
    if buf.len() < 8 || buf[0] != 0x20 {
        return None;
    }
    let id_len = buf[5] as usize;
    if id_len != 2 {
        return None;
    }
    Some(u16::from_be_bytes([buf[6], buf[7]]))
}

/// Read the two tables and return the tuned service when they agree.
pub fn live_service(dir: &str) -> Option<LiveService> {
    let sdt = std::fs::read(format!("{dir}/AdsbSiMhSdtActual.dat")).ok()?;
    let live = parse_mh_sdt_actual(&sdt)?;
    match std::fs::read(format!("{dir}/AdsbSiMpt.dat")).ok().as_deref().and_then(parse_mpt_package_id) {
        // Disagreement means the box is mid-change; refusing to answer is
        // better than naming the wrong service to a client.
        Some(pkg) if pkg != live.service_id => None,
        _ => Some(live),
    }
}

/// Selected MPT identity independent of the presentation-only MH-SDT cache.
/// Callers must additionally verify fresh, stable live ES before streaming.
pub fn selected_mpt_service(dir: &str) -> Option<u16> {
    selected_mpt_bytes(&std::fs::read(format!("{dir}/AdsbSiMpt.dat")).ok()?)
}

fn selected_mpt_bytes(mpt: &[u8]) -> Option<u16> {
    let total = section_total_len(mpt, 0)?;
    if total < 8 || total > mpt.len() { return None; }
    let sid = parse_mpt_package_id(mpt)?;
    crate::channels::tuning_record(sid)?;
    Some(sid)
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Bytes measured on the box at 20:40 on 2026-08-05, tuned to BS日テレ4K.
    fn sdt_fixture() -> Vec<u8> {
        let mut v = vec![
            0x9f, 0xf0, 0x00, // table_id, syntax+length (patched below)
            0xb0, 0x72, // TLV_stream_id
            0xc3, 0x00, 0x00, // version, section numbers
            0x00, 0x0b, // original_network_id
            0xff, // reserved
            0x00, 0x8d, // service_id
            0xff, 0x00, 0x48, // flags, descriptor loop length
        ];
        v.extend(std::iter::repeat_n(0u8, 0x48));
        let section_len = v.len() - 3 + 4;
        v[1] = 0xf0 | ((section_len >> 8) as u8 & 0x0f);
        v[2] = (section_len & 0xff) as u8;
        let crc = crc32_mpeg(&v);
        v.extend_from_slice(&crc.to_be_bytes());
        v
    }

    #[test]
    fn mh_sdt_actual_names_the_tuned_stream_and_service() {
        let live = parse_mh_sdt_actual(&sdt_fixture()).expect("parses");
        assert_eq!(live.tlv_stream_id, 0xB072);
        assert_eq!(live.original_network_id, 0x000B);
        assert_eq!(live.service_id, 141);
    }

    #[test]
    fn a_corrupt_section_is_refused_rather_than_guessed() {
        let mut bad = sdt_fixture();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(parse_mh_sdt_actual(&bad).is_none());
    }

    #[test]
    fn selected_mpt_rejects_truncation_unknown_service_and_short_length() {
        let mpt = [0x20, 0x54, 0, 4, 0xfc, 2, 0, 141];
        assert_eq!(selected_mpt_bytes(&mpt), Some(141));
        assert!(selected_mpt_bytes(&mpt[..7]).is_none());
        let mut bad=mpt; bad[3]=0; assert!(selected_mpt_bytes(&bad).is_none());
        bad=mpt; bad[7]=99; assert!(selected_mpt_bytes(&bad).is_none());
        bad=mpt; bad[0]=0x21; assert!(selected_mpt_bytes(&bad).is_none());
    }

    #[test]
    fn mpt_package_id_is_the_service_id() {
        // Measured head of AdsbSiMpt.dat.
        let mpt = [0x20, 0x54, 0x03, 0x76, 0xfc, 0x02, 0x00, 0x8d, 0x00, 0x12];
        assert_eq!(parse_mpt_package_id(&mpt), Some(141));
    }
}

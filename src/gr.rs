//! Byte-exact ISDB-T tuning plan recovered from the product `tuner` binary.
//!
//! This module builds the byte-exact plan consumed by both dry-run output and
//! the live transport. Product-wide semaphore attachment and rollback are in
//! `gr_i2c`; the source of truth is `docs/LIMITATIONS.md` (this path is frozen and does not produce a stream).

pub const ADC_HZ: u32 = 25_000_000;
pub const DEFAULT_IF_HZ: u32 = 3_550_000;
pub const I2C0_SEM_SHM_KEY: u32 = 0x3400_103b;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Write {
    pub slave7: u8,
    pub reg: u8,
    pub data: Vec<u8>,
    pub label: &'static str,
}

impl Write {
    fn rf(reg: u8, data: &[u8], label: &'static str) -> Self {
        Self { slave7: 0x60, reg, data: data.to_vec(), label }
    }

    fn demod(page: u8, reg: u8, value: u8, label: &'static str) -> Self {
        Self { slave7: 0x10 + page, reg, data: vec![value], label }
    }
}

/// UHF physical channels used in Japan. The product's own table uses integer
/// kHz and rounds the 1/7 MHz offset to 143 kHz.
pub fn uhf_frequency_khz(channel: u8) -> Result<u32, String> {
    if !(13..=62).contains(&channel) {
        return Err(format!("GR UHF channel must be 13..62, got {channel}"));
    }
    Ok(473_143 + u32::from(channel - 13) * 6_000)
}

/// Vendor SC1501A algorithm at tuner:0x229a0:
/// round(IF / 25 MHz * 2^24), emitted most-significant byte first.
pub fn nco_word(if_hz: u32) -> Result<u32, String> {
    if if_hz == 0 || if_hz >= ADC_HZ {
        return Err(format!("GR IF must be in 1..{} Hz, got {if_hz}", ADC_HZ - 1));
    }
    let numerator = u64::from(if_hz) * (1u64 << 24);
    Ok(((numerator + u64::from(ADC_HZ / 2)) / u64::from(ADC_HZ)) as u32)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("bad embedded hex byte {b:#x}")),
    }
}

fn decode_regset(hex: &str, label: &'static str) -> Result<Vec<Write>, String> {
    if hex.len() % 6 != 0 {
        return Err(format!("{label}: regset hex length is not a triple multiple"));
    }
    let mut raw = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        raw.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    let mut out = Vec::new();
    let mut terminated = false;
    for triple in raw.chunks_exact(3) {
        if triple == [0xff, 0xff, 0xff] {
            terminated = true;
            break;
        }
        out.push(Write::demod(triple[0], triple[1], triple[2], label));
    }
    if !terminated {
        return Err(format!("{label}: missing ff/ff/ff terminator"));
    }
    Ok(out)
}

/// Full plan through demodulator sequencer start. RMW operations for TS output
/// and read-only lock polling are printed separately by the CLI because their
/// values are not unconditional writes.
pub fn tuning_writes(freq_khz: u32, if_hz: u32) -> Result<Vec<Write>, String> {
    if freq_khz > 0x000f_ffff {
        return Err(format!("GR frequency {freq_khz} kHz exceeds the 20-bit RF field"));
    }

    let mut out = Vec::new();

    // x_fin_isdbs: hand the shared RF IC from satellite to terrestrial.
    out.extend([
        Write::rf(0x15, &[0x12], "RF satellite standby"),
        Write::rf(0x43, &[0x05], "RF satellite standby"),
        Write::rf(0x45, &[0x01], "RF satellite standby"),
        Write::rf(0x0c, &[0x14, 0x00, 0x00], "RF satellite standby"),
        Write::rf(0x01, &[0x00], "RF satellite standby"),
        Write::rf(0x05, &[0x00], "RF satellite standby"),
        Write::rf(0x04, &[0xc0], "RF satellite standby"),
    ]);

    // x_pon_isdbt(0). The 10 ms delay follows the 0x9b write.
    out.extend([
        Write::rf(0x99, &[0x7a, 0x01], "RF ISDB-T power-on"),
        Write::rf(0x81, &[0x18, 0x84, 0xa4, 0x80, 0x01, 0x10, 0xc4, 0x40,
                          0x10, 0x00, 0x45, 0x75, 0x07, 0x08, 0x00, 0x00],
                  "RF ISDB-T power-on"),
        Write::rf(0x91, &[0x10, 0x20, 0x0a, 0x00], "RF ISDB-T power-on"),
        Write::rf(0x9b, &[0x00], "RF ISDB-T power-on; delay 10 ms after"),
        Write::rf(0x74, &[0x12], "RF ISDB-T power-on"),
        Write::rf(0x67, &[0x00], "RF ISDB-T power-on"),
        Write::rf(0x5e, &[0x15, 0x00, 0x00], "RF ISDB-T power-on"),
        Write::rf(0x88, &[0x00], "RF ISDB-T power-on"),
        Write::rf(0x87, &[0xc0], "RF ISDB-T power-on"),
        Write::rf(0x80, &[0x01], "RF ISDB-T power-on"),
        Write::rf(0x85, &[0x01], "RF ISDB-T power-on"),
    ]);

    out.extend(decode_regset(RECOMMEND_HEX, "SC1501A Recommend_DIFF")?);
    out.extend(decode_regset(ISDBT6_HEX, "SC1501A ISDB-T 6MHz")?);

    let nco = nco_word(if_hz)?;
    out.extend([
        Write::demod(0, 0x1e, (nco >> 16) as u8, "SC1501A ISDB-T NCO"),
        Write::demod(0, 0x1f, (nco >> 8) as u8, "SC1501A ISDB-T NCO"),
        Write::demod(0, 0x20, nco as u8, "SC1501A ISDB-T NCO"),
    ]);

    out.extend([
        Write::rf(0x74, &[0x12, 0xf9, 0x0f, 0x25, 0x44], "RF ISDB-T tune"),
        Write::rf(0x8b, &[0x45], "RF ISDB-T tune"),
        Write::rf(0x8d, &[0x00, 0x08], "RF ISDB-T tune"),
        Write::rf(0x79, &[0xa1], "RF ISDB-T tune"),
        Write::rf(0x7d, &[0x00], "RF ISDB-T tune"),
        Write::rf(0x91, &[0x0a, 0x0f], "RF ISDB-T tune"),
        Write::rf(0x9c, &[0x90, 0x00], "RF ISDB-T tune"),
        Write::rf(0x87, &[0xc4, 0x40], "RF ISDB-T tune"),
        Write::rf(0x5e, &[0xee, 0x02, 0x9e], "RF ISDB-T tune"),
        Write::rf(0x61, &[0x67, 0x03, 0x38, 0x1e, 0x02, 0x24, 0x00], "RF ISDB-T tune"),
        Write::rf(0x68, &[0x00, 0x87, 0x00, 0x0d, 0x33, 0x00, 0x17, 0x1b], "RF ISDB-T tune"),
        Write::rf(0x70, &[freq_khz as u8, (freq_khz >> 8) as u8,
                          ((freq_khz >> 16) & 0x0f) as u8, 0xff, 0x11],
                  "RF ISDB-T tune"),
        Write::rf(0x88, &[0x00], "RF ISDB-T tune-end"),
        Write::rf(0x87, &[0xc0], "RF ISDB-T tune-end"),
        Write::demod(1, 0xf4, 0x9f, "SC1501A ISDB-T sequencer start"),
    ]);

    Ok(out)
}

// Exact `.data` blobs from evidence/device/rootfs/usr/bin/tuner.
// VA 0x39c38 / file 0x29c38, 62 triples + terminator.
const RECOMMEND_HEX: &str = concat!(
    "00009400018c000552000710001000001100001408001644001880001c01001d010085050097e100bc2500c60100cd41",
    "017700050240051015052c9d052dff052e020530cb053439053667053762053b30053c18054137054337054de4056e4c",
    "0570ff0575c905771405876905884a058931058a24058b1d058c1d05951005960805970f05e580070203071ec0071fa8",
    "072064072164073281073310073b0707748007768007911707951807961907972e07982f079934079a35ffffff",
);

// VA 0x38a5c / file 0x28a5c, 168 triples + terminator.
const ISDBT6_HEX: &str = concat!(
    "00001800018e0002210003d4000421000540000740000b00000c00001000001100001200001600001800001a10001c00",
    "001d01002ac4002be0002c0000391c003a29003d1c003e2900411c00422900451c004629006317006550006c82008440",
    "0085250097e100c60100c90000cd4100d9000101120102ab01039601047801072601091b01130201169f011c00012301",
    "012402012c12013e0001466801507001540c0155e00161000168f0016904016a03017c0001e50002110203005003090e",
    "030b140318000319000323c903240203253003266903271f033ded033e96033f4a0342c803432d034448034907034af0",
    "0358570390ed039d6503e10803e2ae0400b004056004065f0407270408b00409ff040ac0040baa040cbb040dee040eaa",
    "040faa04101004116204120d04137704141d04151c0421000422a0042e00042f0c04303b04320c0434b10435ed043660",
    "0437cc04386c04397d043ab1043bed0443970446a504477e04481d0449a5044a76044ba5044c76044da4044e76045055",
    "04534f0454c004557304624b047912047ae2047b0004814004c98505e500070203071ec0071fa8072064072164073310",
    "073b07077200077480077680078d0007911707d31f07d4ff081501081602081ac6081d6e081fa80820f0082f01084300",
    "084a02084b02084c02084d02084ebb084fbb0850bb0851bbffffff",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_uhf_table_formula_is_exact() {
        assert_eq!(uhf_frequency_khz(13).unwrap(), 473_143);
        assert_eq!(uhf_frequency_khz(27).unwrap(), 557_143);
        assert_eq!(uhf_frequency_khz(62).unwrap(), 767_143);
        assert!(uhf_frequency_khz(12).is_err());
        assert!(uhf_frequency_khz(63).is_err());
    }

    #[test]
    fn default_if_produces_vendor_nco() {
        assert_eq!(nco_word(DEFAULT_IF_HZ).unwrap(), 0x24_5a_1d);
        assert!(nco_word(0).is_err());
        assert!(nco_word(ADC_HZ).is_err());
    }

    #[test]
    fn embedded_vendor_tables_have_exact_counts_and_pages() {
        let recommend = decode_regset(RECOMMEND_HEX, "recommend").unwrap();
        let isdbt = decode_regset(ISDBT6_HEX, "isdbt").unwrap();
        assert_eq!(recommend.len(), 62);
        assert_eq!(isdbt.len(), 168);
        let pages: std::collections::BTreeSet<u8> =
            isdbt.iter().map(|w| w.slave7 - 0x10).collect();
        assert_eq!(pages, [0, 1, 2, 3, 4, 5, 7, 8].into_iter().collect());
    }

    #[test]
    fn ch13_plan_contains_frequency_and_nco_bytes() {
        let writes = tuning_writes(473_143, DEFAULT_IF_HZ).unwrap();
        assert_eq!(writes.len(), 7 + 11 + 62 + 168 + 3 + 15);
        assert!(writes.iter().any(|w| w.slave7 == 0x60
            && w.reg == 0x70
            && w.data == [0x37, 0x38, 0x07, 0xff, 0x11]));
        let nco: Vec<u8> = writes.iter()
            .filter(|w| w.slave7 == 0x10 && (0x1e..=0x20).contains(&w.reg))
            .map(|w| w.data[0]).collect();
        assert_eq!(nco, [0x24, 0x5a, 0x1d]);
    }

    #[test]
    fn tokyo_nhk_g_ch27_has_fixed_rf_bytes() {
        let writes = tuning_writes(uhf_frequency_khz(27).unwrap(), DEFAULT_IF_HZ).unwrap();
        assert!(writes.iter().any(|w| w.slave7 == 0x60
            && w.reg == 0x70
            && w.data == [0x57, 0x80, 0x08, 0xff, 0x11]));
    }
}

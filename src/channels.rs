//! The advanced-BS (ISDB-S3) channel table, and its mapping onto Mirakurun's
//! channel/service model.
//!
//! Source of truth is a TSV recovered from the
//! product's own NVRAM and cross-checked against live SI captured with the
//! antenna connected (docs/HOW_IT_WORKS.md — all ten records matched, and the
//! frequencies match the published BS downlink plan). It is compiled in so the
//! server does not depend on anything under `/mmc`, which we must not write.
//!
//! Advanced CS (ND-23) is deliberately absent: the product's `sl0901.dat` is a
//! 2017 factory default and `acs_scan_nd23` is a zero-byte "never scanned"
//! flag, so those records are not trustworthy (docs/LIMITATIONS.md).

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// `(service_id, transport_stream_id, freq_kHz, BS slot, name)`
const ADVBS: &[(u16, u16, u32, u8, &str)] = &[
    (101, 0xB110, 12_034_360, 17, "NHK　BSP4K"),
    (102, 0xB0E0, 11_976_820, 14, "NHK　BS8K"),
    (141, 0xB072, 11_842_560, 7, "BS日テレ　4K"),
    (151, 0xB070, 11_842_560, 7, "BS朝日　4K"),
    (161, 0xB111, 12_034_360, 17, "BS−TBS　4K"),
    (171, 0xB071, 11_842_560, 7, "BSテレ東4K"),
    (181, 0xB112, 12_034_360, 17, "BSフジ4K"),
    (211, 0xB0B0, 11_919_280, 11, "ショップチャンネル"),
    (221, 0xB0B2, 11_919_280, 11, "4K QVC"),
    // The product's NVRAM has an empty name for 231; `from_records` substitutes
    // a placeholder rather than us inventing a broadcaster.
    (231, 0xB2B1, 11_919_280, 11, ""),
];

/// Remote-control key id the product puts in its change-channel request.
///
/// Measured: ten consecutive remote changes on 2026-08-05 logged
/// `BhvTvAdsbTuneChangeChannel ... BID(n)` for every service, and the values
/// are the familiar Japanese remote key numbering (NHK 1, 日テレ 2, 朝日 3,
/// TBS 4, テレ東 5, フジ 6) with the shopping channels above them.
const BROADCASTER_KEY: &[(u16, u8)] = &[
    (101, 1),
    (102, 1),
    (141, 2),
    (151, 3),
    (161, 4),
    (171, 5),
    (181, 6),
    (211, 9),
    (221, 0xa),
    (231, 0xb),
];

/// Everything the product's change-channel request needs for one service.
pub fn tuning_record(service_id: u16) -> Option<(u16, u32, u8)> {
    let (_, tsid, freq, _, _) = ADVBS.iter().copied().find(|r| r.0 == service_id)?;
    let bid = BROADCASTER_KEY
        .iter()
        .copied()
        .find(|r| r.0 == service_id)
        .map(|r| r.1)?;
    Some((tsid, freq, bid))
}

/// network_id of the advanced-BS network (0x000B), from the live TLV-NIT.
pub const NETWORK_ID: u16 = 11;

/// ARIB service type. 4K services are carried as digital TV services; the
/// product's own service list stores 0x01 here.
const SERVICE_TYPE: u8 = 0x01;

#[derive(Clone, Debug)]
pub struct Service {
    pub service_id: u16,
    pub network_id: u16,
    pub name: String,
    pub service_type: u8,
}

impl Service {
    /// Mirakurun's `ServiceItemId`: the network id followed by the service id
    /// zero-padded to five digits, read as one decimal number.
    pub fn item_id(&self) -> u64 {
        self.network_id as u64 * 100_000 + self.service_id as u64
    }
}

#[derive(Clone, Debug)]
pub struct Channel {
    /// Mirakurun channel key, e.g. `BS17_0`.
    pub key: String,
    /// Human label, e.g. `BS-17 (12034.36MHz TSID B110)`.
    pub name: String,
    /// BS slot number. Folded into `name` and `key` at construction; retained
    /// because it is the identity a person reads off the product's own UI.
    #[cfg_attr(not(test), allow(dead_code))]
    pub slot: u8,
    pub freq_khz: u32,
    pub tsid: u16,
    pub services: Vec<Service>,
    ch_type: String,
}

/// Stock Mirakurun's `ChannelType` enum is GR / BS / CS / SKY, with no member
/// for advanced BS, so `BS` is the safe default for an arbitrary client.
pub const DEFAULT_CHANNEL_TYPE: &str = "BS";

/// The 4K forks (Mirakurun-BS4K, DMirakurun) and the BS4K BonDriver use this
/// instead, and `BonDriver_LinuxMirakurunBS4K` filters on it via CHANNEL_TYPES.
/// Selected with `--channel-type BS4K`; named here so the value has one home.
#[cfg_attr(not(test), allow(dead_code))]
pub const BS4K_CHANNEL_TYPE: &str = "BS4K";

impl Channel {
    /// Mirakurun `ChannelType`. Configurable because the two client families
    /// disagree about how to name advanced BS; see DESIGN.md §2.3.
    pub fn ch_type(&self) -> &str {
        &self.ch_type
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChannelTable {
    pub channels: Vec<Channel>,
}

impl ChannelTable {
    pub fn builtin() -> Self {
        Self::from_records(ADVBS.iter().map(|&(sid, tsid, freq, slot, name)| Record {
            service_id: sid,
            tsid,
            freq_khz: freq,
            slot,
            name: name.to_string(),
        }))
    }

    /// Parse that same TSV layout, so a rescan
    /// can be dropped in without rebuilding. Unknown columns are ignored and
    /// the header row is required (column order is looked up by name).
    pub fn from_tsv(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next().ok_or("empty channel table")?;
        let cols: Vec<&str> = header.split('\t').map(|s| s.trim()).collect();
        let idx = |name: &str| -> Result<usize, String> {
            cols.iter()
                .position(|c| *c == name)
                .ok_or_else(|| format!("channel table has no `{name}` column"))
        };
        let (i_sid, i_tsid, i_freq, i_slot, i_name) = (
            idx("service_id")?,
            idx("transport_stream_id")?,
            idx("freq_khz")?,
            idx("slot")?,
            idx("name")?,
        );

        let mut records = Vec::new();
        for (n, line) in lines.enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let need = [i_sid, i_tsid, i_freq, i_slot, i_name].into_iter().max().unwrap();
            if f.len() <= need {
                continue; // short row: skip rather than fail the whole table
            }
            let freq: u32 = f[i_freq].trim().parse().unwrap_or(0);
            if freq == 0 {
                continue; // unscanned placeholder (e.g. the advanced-CS row)
            }
            let tsid_raw = f[i_tsid].trim();
            let tsid = u16::from_str_radix(tsid_raw.trim_start_matches("0x"), 16)
                .or_else(|_| tsid_raw.parse())
                .map_err(|_| format!("row {}: bad transport_stream_id {tsid_raw:?}", n + 2))?;
            let slot = f[i_slot]
                .trim()
                .trim_start_matches("BS-")
                .trim_start_matches("BS")
                .parse()
                .unwrap_or(0);
            records.push(Record {
                service_id: f[i_sid]
                    .trim()
                    .parse()
                    .map_err(|_| format!("row {}: bad service_id", n + 2))?,
                tsid,
                freq_khz: freq,
                slot,
                name: f[i_name].trim().to_string(),
            });
        }
        if records.is_empty() {
            return Err("channel table has no usable rows".into());
        }
        Ok(Self::from_records(records.into_iter()))
    }

    /// Group services into channels. One transport stream (one TSID) is one
    /// Mirakurun channel; several TSIDs share a transponder frequency, and the
    /// channel key numbers them within the transponder in TSID order, which is
    /// how Mirakurun names BS relative streams.
    fn from_records(records: impl Iterator<Item = Record>) -> Self {
        let mut by_ts: BTreeMap<(u8, u16), (u32, Vec<Service>)> = BTreeMap::new();
        for r in records {
            let e = by_ts.entry((r.slot, r.tsid)).or_insert((r.freq_khz, Vec::new()));
            e.1.push(Service {
                service_id: r.service_id,
                network_id: NETWORK_ID,
                name: if r.name.trim().is_empty() {
                    format!("Service {}", r.service_id)
                } else {
                    r.name
                },
                service_type: SERVICE_TYPE,
            });
        }

        let mut per_slot: BTreeMap<u8, u32> = BTreeMap::new();
        let mut channels = Vec::new();
        for ((slot, tsid), (freq_khz, mut services)) in by_ts {
            services.sort_by_key(|s| s.service_id);
            let idx = per_slot.entry(slot).or_insert(0);
            let mut name = String::new();
            let _ = write!(
                name,
                "BS-{slot} ({}.{:02}MHz TSID {tsid:04X})",
                freq_khz / 1000,
                (freq_khz % 1000) / 10
            );
            channels.push(Channel {
                key: format!("BS{slot}_{idx}"),
                name,
                slot,
                freq_khz,
                tsid,
                services,
                ch_type: DEFAULT_CHANNEL_TYPE.to_string(),
            });
            *idx += 1;
        }
        ChannelTable { channels }
    }

    /// Rename the `ChannelType` reported for every channel.
    pub fn set_channel_type(&mut self, ch_type: &str) {
        for c in &mut self.channels {
            c.ch_type = ch_type.to_string();
        }
    }

    pub fn find(&self, ch_type: &str, key: &str) -> Option<&Channel> {
        self.channels
            .iter()
            .find(|c| c.ch_type().eq_ignore_ascii_case(ch_type) && c.key == key)
    }

    /// Locate a service by its Mirakurun item id, with the channel it sits on.
    pub fn find_service(&self, item_id: u64) -> Option<(&Channel, &Service)> {
        self.channels
            .iter()
            .find_map(|c| c.services.iter().find(|s| s.item_id() == item_id).map(|s| (c, s)))
    }

    pub fn services(&self) -> impl Iterator<Item = (&Channel, &Service)> {
        self.channels.iter().flat_map(|c| c.services.iter().map(move |s| (c, s)))
    }
}

struct Record {
    service_id: u16,
    tsid: u16,
    freq_khz: u32,
    slot: u8,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_groups_by_transport_stream() {
        let t = ChannelTable::builtin();
        // Ten services across ten transport streams on four transponders.
        assert_eq!(t.channels.len(), 10);
        assert_eq!(t.services().count(), 10);

        let keys: Vec<&str> = t.channels.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"BS7_0"));
        assert!(keys.contains(&"BS17_0"));
        assert!(keys.contains(&"BS14_0"));
    }

    #[test]
    fn channel_keys_are_indexed_within_a_transponder() {
        let t = ChannelTable::builtin();
        let bs17: Vec<&Channel> = t.channels.iter().filter(|c| c.slot == 17).collect();
        assert_eq!(bs17.len(), 3);
        let mut sorted = bs17.clone();
        sorted.sort_by_key(|c| c.tsid);
        // Index order must follow TSID order, and every one shares a frequency.
        assert_eq!(sorted[0].key, "BS17_0");
        assert_eq!(sorted[1].key, "BS17_1");
        assert_eq!(sorted[2].key, "BS17_2");
        assert!(bs17.iter().all(|c| c.freq_khz == 12_034_360));
    }

    #[test]
    fn service_item_id_follows_mirakurun() {
        let t = ChannelTable::builtin();
        let (_, s) = t.find_service(1_100_101).expect("NHK BSP4K");
        assert_eq!(s.service_id, 101);
        assert_eq!(s.network_id, 11);
        // 11 followed by 00101.
        assert_eq!(s.item_id(), 1_100_101);
    }

    #[test]
    fn lookup_is_case_insensitive_on_type_only() {
        let t = ChannelTable::builtin();
        assert!(t.find("BS", "BS17_0").is_some());
        assert!(t.find("bs", "BS17_0").is_some());
        assert!(t.find("BS", "bs17_0").is_none());
        assert!(t.find("GR", "BS17_0").is_none());
    }

    #[test]
    fn channel_type_can_be_switched_to_bs4k() {
        let mut t = ChannelTable::builtin();
        t.set_channel_type(BS4K_CHANNEL_TYPE);
        assert!(t.channels.iter().all(|c| c.ch_type() == "BS4K"));
        // Lookups must follow the new type, and the old one must stop matching,
        // otherwise a client filtering on CHANNEL_TYPES would see a channel it
        // then cannot open.
        assert!(t.find("BS4K", "BS17_0").is_some());
        assert!(t.find("bs4k", "BS17_0").is_some());
        assert!(t.find("BS", "BS17_0").is_none());
    }

    #[test]
    fn tsv_parsing_matches_the_builtin_table() {
        let tsv = "rec\tservice_id\ttransport_stream_id\tfreq_khz\tslot\tname\n\
                   0\t101\t0xB110\t12034360\tBS-17\tNHK BSP4K\n\
                   1\t161\t0xB111\t12034360\tBS-17\tBS-TBS 4K\n\
                   2\t821\t0xC090\t0\t\tJ SPORTS 1\n";
        let t = ChannelTable::from_tsv(tsv).unwrap();
        // The zero-frequency advanced-CS placeholder must be dropped.
        assert_eq!(t.channels.len(), 2);
        assert_eq!(t.channels[0].key, "BS17_0");
        assert_eq!(t.channels[0].freq_khz, 12_034_360);
        assert_eq!(t.channels[1].tsid, 0xB111);
    }

    #[test]
    fn tsv_without_required_columns_is_rejected() {
        assert!(ChannelTable::from_tsv("a\tb\n1\t2\n").is_err());
        assert!(ChannelTable::from_tsv("").is_err());
    }

    #[test]
    fn blank_service_names_get_a_placeholder() {
        let t = ChannelTable::builtin();
        assert!(t.services().all(|(_, s)| !s.name.trim().is_empty()));
    }
}

#[cfg(test)]
mod tuning_record_tests {
    use super::*;

    #[test]
    fn every_service_has_a_tuning_record() {
        for (sid, tsid, freq, _, _) in ADVBS.iter().copied() {
            let got = tuning_record(sid).expect("record");
            assert_eq!((got.0, got.1), (tsid, freq), "service {sid}");
        }
    }

    #[test]
    fn records_match_the_serial_log() {
        // BhvTvAdsbTuneChangeChannel lines captured 2026-08-05.
        for (sid, tsid, freq, bid) in [
            (141u16, 0xB072u16, 11_842_560u32, 2u8),
            (101, 0xB110, 12_034_360, 1),
            (151, 0xB070, 11_842_560, 3),
            (161, 0xB111, 12_034_360, 4),
            (171, 0xB071, 11_842_560, 5),
            (181, 0xB112, 12_034_360, 6),
            (211, 0xB0B0, 11_919_280, 9),
            (221, 0xB0B2, 11_919_280, 0xa),
            (231, 0xB2B1, 11_919_280, 0xb),
            (102, 0xB0E0, 11_976_820, 1),
        ] {
            assert_eq!(tuning_record(sid), Some((tsid, freq, bid)), "service {sid}");
        }
    }

    #[test]
    fn an_unknown_service_has_none() {
        assert_eq!(tuning_record(999), None);
    }
}

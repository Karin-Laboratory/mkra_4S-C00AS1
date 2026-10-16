//! Streaming PSI/ECM tracking and CPU MULTI2. No keys are logged or written.
use crate::{cas::NativeCard, multi2};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
};
fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
pub trait Card: Send {
    fn ecm(&mut self, section: &[u8]) -> io::Result<[[u8; 8]; 2]>;
}
impl Card for NativeCard {
    fn ecm(&mut self, s: &[u8]) -> io::Result<[[u8; 8]; 2]> {
        NativeCard::ecm(self, s)
    }
}
pub fn crc(s: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in s {
        c ^= (b as u32) << 24;
        for _ in 0..8 {
            c = (c << 1) ^ if c & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
        }
    }
    c
}
/// Reassembles sections across packets; discards incomplete sections on CC gaps.
#[derive(Default)]
struct Sections {
    bytes: Vec<u8>,
    cc: Option<u8>,
}
impl Sections {
    fn append(&mut self, mut data: &[u8], new: bool, out: &mut Vec<Vec<u8>>) -> io::Result<()> {
        if self.bytes.is_empty() && !new {
            return Ok(());
        }
        while !data.is_empty() {
            if self.bytes.is_empty() && data[0] == 0xff {
                break;
            }
            let need = if self.bytes.len() < 3 {
                3 - self.bytes.len()
            } else {
                let n = 3 + (((self.bytes[1] & 15) as usize) << 8) + self.bytes[2] as usize;
                if !(7..=4096).contains(&n) {
                    self.bytes.clear();
                    return Err(bad("PSI section bounds"));
                }
                n - self.bytes.len()
            };
            let take = need.min(data.len());
            self.bytes.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.bytes.len() >= 3 {
                let n = 3 + (((self.bytes[1] & 15) as usize) << 8) + self.bytes[2] as usize;
                if n == self.bytes.len() {
                    out.push(std::mem::take(&mut self.bytes));
                    if !new {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
    fn push(&mut self, p: &[u8], offset: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let cc = p[3] & 15;
        if self.cc == Some(cc) {
            return Ok(out);
        } // repeated payload packet
        if self.cc.is_some_and(|v| (v + 1) & 15 != cc) {
            self.bytes.clear();
        }
        self.cc = Some(cc);
        if p[1] & 0x40 != 0 {
            let pointer = *p.get(offset).ok_or_else(|| bad("missing PSI pointer"))? as usize;
            let start = offset + 1 + pointer;
            if start > 188 {
                return Err(bad("PSI pointer bounds"));
            }
            self.append(&p[offset + 1..start], false, &mut out)?;
            self.bytes.clear();
            self.append(&p[start..], true, &mut out)?;
        } else {
            self.append(&p[offset..], false, &mut out)?;
        }
        Ok(out)
    }
}
fn payload(p: &[u8]) -> io::Result<Option<usize>> {
    if p.len() != 188 || p[0] != 0x47 || p[1] & 0x80 != 0 || p[3] & 0x30 == 0 {
        return Err(bad("TS sync/TEI/header invalid"));
    }
    let off = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    if off > 188 {
        return Err(bad("TS adaptation bounds"));
    }
    Ok(if p[3] & 0x10 != 0 && off < 188 {
        Some(off)
    } else {
        None
    })
}
fn ca(descriptors: &[u8]) -> io::Result<Option<u16>> {
    let mut n = 0;
    let mut found = None;
    while n < descriptors.len() {
        if n + 2 > descriptors.len() {
            return Err(bad("descriptor header truncated"));
        }
        let len = descriptors[n + 1] as usize;
        let end = n + 2 + len;
        if end > descriptors.len() {
            return Err(bad("descriptor truncated"));
        }
        if descriptors[n] == 9 && len >= 4 && descriptors[n + 2..n + 4] == [0, 5] {
            let pid = (((descriptors[n + 4] & 31) as u16) << 8) | descriptors[n + 5] as u16;
            if found.is_some_and(|v| v != pid) {
                return Err(bad("multiple CA0005 ECM routes in one descriptor scope"));
            }
            found = Some(pid);
        }
        n = end;
    }
    Ok(found)
}
struct Key {
    section: Vec<u8>,
    work: [multi2::WorkKey; 2],
}
pub struct Decoder<C: Card> {
    card: C,
    system: [u8; 32],
    iv: [u8; 8],
    tsid: u16,
    wanted_service: Option<u16>,
    wanted_pmt: Option<u16>,
    sections: BTreeMap<u16, Sections>,
    pmts: BTreeSet<u16>,
    seen_pmts: BTreeSet<u16>,
    routes: BTreeMap<u16, u16>,
    keys: BTreeMap<u16, Key>,
    fragment: Vec<u8>,
    waiting: VecDeque<[u8; 188]>,
    pat: bool,
    started: bool,
    scrambled_pids: BTreeSet<u16>,
    pub packets: u64,
    pub decrypted: u64,
    pub updates: u64,
    pub encrypted_passthrough: BTreeMap<u16, u64>,
}
impl<C: Card> Decoder<C> {
    pub fn new(card: C, system: [u8; 32], iv: [u8; 8], tsid: u16) -> Self {
        Self {
            card,
            system,
            iv,
            tsid,
            wanted_service: None,
            wanted_pmt: None,
            sections: BTreeMap::from([(0, Sections::default())]),
            pmts: BTreeSet::new(),
            seen_pmts: BTreeSet::new(),
            routes: BTreeMap::new(),
            keys: BTreeMap::new(),
            fragment: Vec::new(),
            waiting: VecDeque::new(),
            pat: false,
            started: false,
            scrambled_pids: BTreeSet::new(),
            packets: 0,
            decrypted: 0,
            updates: 0,
            encrypted_passthrough: BTreeMap::new(),
        }
    }
    pub fn select_service(mut self, service: u16) -> Self {
        self.wanted_service = Some(service);
        self
    }
    fn section(&mut self, pid: u16, s: Vec<u8>) -> io::Result<()> {
        if crc(&s) != 0 {
            return Err(bad("PSI/ECM CRC failed"));
        }
        if pid == 0 && s[0] == 0 {
            if s.len() < 12 || u16::from_be_bytes([s[3], s[4]]) != self.tsid {
                return Err(bad("received PAT TSID differs from requested channel"));
            }
            if s[5] & 1 == 0 {
                return Ok(());
            }
            // Multi-section PAT changes are deliberately refused until merged.
            if s[6] != 0 || s[7] != 0 {
                return Err(bad("multi-section PAT unsupported"));
            }
            let mut pmts = BTreeSet::new();
            self.wanted_pmt = None;
            for r in s[8..s.len() - 4].chunks_exact(4) {
                if r[..2] != [0, 0] {
                    let pmt = (((r[2] & 31) as u16) << 8) | r[3] as u16;
                    pmts.insert(pmt);
                    if self.wanted_service == Some(u16::from_be_bytes([r[0], r[1]])) {
                        self.wanted_pmt = Some(pmt);
                    }
                }
            }
            if self.wanted_service.is_some() && self.wanted_pmt.is_none() {
                return Err(bad("requested service is absent from received PAT"));
            }
            if pmts.is_empty() || pmts.len() > 64 {
                return Err(bad("PAT program bounds"));
            }
            if self.pat && pmts != self.pmts {
                return Err(bad("PAT program map changed; reopen the stream"));
            }
            for &p in &pmts {
                self.sections.entry(p).or_default();
            }
            self.pmts = pmts;
            self.pat = true;
        } else if self.pmts.contains(&pid) && s[0] == 2 {
            if s.len() < 16 {
                return Err(bad("PMT too short"));
            }
            if self.wanted_pmt == Some(pid)
                && self.wanted_service != Some(u16::from_be_bytes([s[3], s[4]]))
            {
                return Err(bad(
                    "requested PAT service differs from received PMT service",
                ));
            }
            if s[5] & 1 == 0 {
                return Ok(());
            }
            if s[6] != 0 || s[7] != 0 {
                return Err(bad("multi-section PMT unsupported"));
            }
            let end = s.len() - 4;
            let len = (((s[10] & 15) as usize) << 8) | s[11] as usize;
            if 12 + len > end {
                return Err(bad("PMT program descriptors truncated"));
            }
            let program_ca = ca(&s[12..12 + len])?;
            let mut n = 12 + len;
            while n < end {
                if n + 5 > end {
                    return Err(bad("PMT ES header truncated"));
                }
                let es = (((s[n + 1] & 31) as u16) << 8) | s[n + 2] as u16;
                let size = (((s[n + 3] & 15) as usize) << 8) | s[n + 4] as usize;
                if n + 5 + size > end {
                    return Err(bad("PMT ES descriptors truncated"));
                }
                if let Some(ecm) = ca(&s[n + 5..n + 5 + size])?
                    .or(program_ca)
                    .filter(|&p| p != 0x1fff)
                {
                    if self.routes.get(&es).is_some_and(|&old| old != ecm) {
                        return Err(bad("PMT CA routing changed; reopen stream"));
                    }
                    self.routes.insert(es, ecm);
                    self.sections.entry(ecm).or_default();
                }
                n += 5 + size;
            }
            self.seen_pmts.insert(pid);
        } else if self.routes.values().any(|&p| p == pid) && [0x82, 0x83].contains(&s[0]) {
            if self.keys.get(&pid).is_some_and(|k| k.section == s) {
                return Ok(());
            }
            let controls = self.card.ecm(&s)?;
            let work = [
                multi2::schedule(&self.system, &controls[0]),
                multi2::schedule(&self.system, &controls[1]),
            ];
            self.keys.insert(pid, Key { section: s, work });
            self.updates += 1;
        }
        Ok(())
    }
    fn ecm_for_pid(&self, pid: u16) -> Option<u16> {
        self.routes.get(&pid).copied().or_else(|| {
            if self.pmts != self.seen_pmts {
                return None;
            }
            let first = self.routes.values().next().copied()?;
            if self.routes.values().all(|&v| v == first) {
                Some(first)
            } else {
                None
            }
        })
    }
    fn emit(&mut self, mut p: [u8; 188], out: &mut Vec<u8>) -> io::Result<()> {
        let sc = p[3] >> 6;
        if sc != 0 {
            if ![2, 3].contains(&sc) {
                return Err(bad("reserved TS scrambling state"));
            }
            let pid = (((p[1] & 31) as u16) << 8) | p[2] as u16;
            // Full multiplexes can contain scrambled data PIDs absent from
            // the active PMTs (measured 0x4e0/0x4e1 on BS1). A sole ECM domain
            // is unambiguous. Selected-service sessions preserve unassociated
            // ciphertext and flags; strict diagnostics reject ambiguity.
            let ecm = match self.ecm_for_pid(pid) {
                Some(ecm) => ecm,
                None if self.wanted_service.is_some() && !self.routes.contains_key(&pid) => {
                    // An unfiltered multiplex may include ancillary PIDs whose
                    // PMT is off-air. Preserve ciphertext AND its scrambling
                    // flag rather than guessing a key or ending the main service.
                    *self.encrypted_passthrough.entry(pid).or_default() += 1;
                    out.extend_from_slice(&p);
                    self.packets += 1;
                    return Ok(());
                }
                None => return Err(bad("scrambled PID has ambiguous/missing ECM routing")),
            };
            let key = self
                .keys
                .get(&ecm)
                .ok_or_else(|| bad("scrambled PID has no accepted ECM"))?;
            if let Some(offset) = payload(&p)? {
                multi2::decrypt(
                    &mut p[offset..],
                    &self.iv,
                    &key.work[if sc == 2 { 1 } else { 0 }],
                );
                self.decrypted += 1;
            }
            p[3] &= 63;
        }
        out.extend_from_slice(&p);
        self.packets += 1;
        Ok(())
    }
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        self.fragment.extend_from_slice(bytes);
        let count = self.fragment.len() / 188;
        for i in 0..count {
            let p: [u8; 188] = self.fragment[i * 188..(i + 1) * 188].try_into().unwrap();
            let offset = payload(&p)?;
            let pid = (((p[1] & 31) as u16) << 8) | p[2] as u16;
            if p[3] >> 6 == 0 {
                if let (Some(offset), Some(assembler)) = (offset, self.sections.get_mut(&pid)) {
                    let ss = assembler.push(&p, offset)?;
                    for s in ss {
                        self.section(pid, s)?;
                    }
                }
            }
            if !self.started {
                self.waiting.push_back(p);
                if p[3] >> 6 != 0 {
                    self.scrambled_pids.insert(pid);
                }
                if self.waiting.len() > 22_000 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData,format!(
                        "BS2K startup exceeded 4 MiB: PMTs={:?} seen={:?} ECMs={:?} accepted={:?} scrambled_PIDs={:?}",
                        self.pmts,self.seen_pmts,self.routes.values().collect::<BTreeSet<_>>(),self.keys.keys().collect::<Vec<_>>(),self.scrambled_pids)));
                }
                if self.pat
                    && self.wanted_pmt.map_or(self.pmts == self.seen_pmts, |pid| {
                        self.seen_pmts.contains(&pid)
                    })
                    && (self.routes.is_empty() || !self.keys.is_empty())
                    && if self.scrambled_pids.is_empty() {
                        self.routes.values().all(|pid| self.keys.contains_key(pid))
                    } else {
                        self.scrambled_pids.iter().all(|&pid| {
                            self.ecm_for_pid(pid).map_or(
                                self.wanted_service.is_some() && !self.routes.contains_key(&pid),
                                |ecm| self.keys.contains_key(&ecm),
                            )
                        })
                    }
                {
                    self.started = true;
                    while let Some(p) = self.waiting.pop_front() {
                        self.emit(p, out)?;
                    }
                }
            } else {
                self.emit(p, out)?;
            }
        }
        self.fragment.drain(..count * 188);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: usize,
    }
    impl Card for Fake {
        fn ecm(&mut self, _: &[u8]) -> io::Result<[[u8; 8]; 2]> {
            self.calls += 1;
            Ok([[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87]; 2])
        }
    }
    fn section(mut b: Vec<u8>) -> Vec<u8> {
        let len = b.len() + 4 - 3;
        b[1] = (b[1] & 0xf0) | ((len >> 8) as u8 & 15);
        b[2] = len as u8;
        let c = crc(&b);
        b.extend(c.to_be_bytes());
        assert_eq!(crc(&b), 0);
        b
    }
    fn packet(pid: u16, cc: u8, s: &[u8]) -> [u8; 188] {
        let mut p = [255; 188];
        p[..5].copy_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | cc, 0]);
        p[5..5 + s.len()].copy_from_slice(s);
        p
    }
    fn tables() -> Vec<[u8; 188]> {
        let pat = section(vec![0, 0xb0, 0, 0x40, 0x10, 0xc1, 0, 0, 0, 151, 0xe1, 0]);
        let pmt = section(vec![
            2, 0xb0, 0, 0, 151, 0xc1, 0, 0, 0xe1, 0x40, 0xf0, 6, 9, 4, 0, 5, 0xe1, 0x21, 2, 0xe1,
            0x40, 0xf0, 0, 6, 0xe1, 0x46, 0xf0, 6, 9, 4, 0, 5, 0xff, 0xff,
        ]);
        let ecm = section(vec![0x82, 0xf0, 0, 0, 0, 0xc1, 0, 0, 1, 2, 3, 4, 5]);
        vec![
            packet(0, 0, &pat),
            packet(0x100, 0, &pmt),
            packet(0x121, 0, &ecm),
        ]
    }
    #[test]
    fn incremental_startup_deduplicates_ecm_and_decrypts_without_touching_psi() {
        let sys = std::array::from_fn(|i| i as u8);
        let iv = std::array::from_fn(|i| 0xf0 + i as u8);
        let mut d = Decoder::new(Fake { calls: 0 }, sys, iv, 0x4010);
        let mut out = Vec::new();
        let ts = tables();
        let data: Vec<u8> = ts.iter().flatten().copied().collect();
        for chunk in data.chunks(17) {
            d.push(chunk, &mut out).unwrap();
        }
        assert_eq!(out, data);
        assert_eq!(d.card.calls, 1);
        d.push(&packet(0x121, 1, &ts[2][5..22]), &mut out).unwrap();
        assert_eq!(d.card.calls, 1);
        // Independent C++ reference's 184-byte ciphertext, with even/odd same synthetic key.
        let hex = include_str!("../tools/fixtures/multi2-reference.txt")
            .lines()
            .find(|s| s.starts_with("184 "))
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1;
        let cipher: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mut p = [0u8; 188];
        p[..4].copy_from_slice(&[0x47, 1, 0x40, 0x90]);
        p[4..].copy_from_slice(&cipher);
        out.clear();
        d.push(&p, &mut out).unwrap();
        assert_eq!(out[3], 0x10);
        assert_eq!(
            &out[4..],
            &(0..184)
                .map(|i| ((i * 37 + 11) & 255) as u8)
                .collect::<Vec<_>>()
        );
        assert_eq!(d.decrypted, 1);
    }
    #[test]
    fn unused_advertised_ecm_does_not_block_an_active_scrambled_stream() {
        let sys = std::array::from_fn(|i| i as u8);
        let iv = std::array::from_fn(|i| 0xf0 + i as u8);
        let mut d = Decoder::new(Fake { calls: 0 }, sys, iv, 0x4010);
        let mut out = Vec::new();
        let mut ts = tables();
        let n = 3 + (((ts[1][6] & 15) as usize) << 8) + ts[1][7] as usize;
        let mut pmt = ts[1][5..5 + n - 4].to_vec();
        // A second elementary stream advertises ECM122 but never has packets.
        pmt.extend_from_slice(&[0x0f, 0xe1, 0x42, 0xf0, 6, 9, 4, 0, 5, 0xe1, 0x22]);
        ts[1] = packet(0x100, 0, &section(pmt));
        let hex = include_str!("../tools/fixtures/multi2-reference.txt")
            .lines()
            .find(|s| s.starts_with("184 "))
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1;
        let cipher: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mut media = [0u8; 188];
        media[..4].copy_from_slice(&[0x47, 1, 0x40, 0x90]);
        media[4..].copy_from_slice(&cipher);
        d.push(&ts[0], &mut out).unwrap();
        d.push(&ts[1], &mut out).unwrap();
        d.push(&media, &mut out).unwrap();
        assert!(out.is_empty());
        d.push(&ts[2], &mut out).unwrap();
        assert_eq!(out.len(), 4 * 188);
        assert_eq!(d.card.calls, 1);
        assert_eq!(d.decrypted, 1);
        assert_eq!(
            &out[2 * 188 + 4..3 * 188],
            &(0..184)
                .map(|i| ((i * 37 + 11) & 255) as u8)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn requested_service_starts_without_a_missing_unrelated_pmt() {
        let mut ts = tables();
        let pat = section(vec![
            0, 0xb0, 0, 0x40, 0x10, 0xc1, 0, 0, 0, 151, 0xe1, 0, 3, 0xe7, 0xe2, 0,
        ]);
        ts[0] = packet(0, 0, &pat);
        let mut d = Decoder::new(Fake { calls: 0 }, [0; 32], [0; 8], 0x4010).select_service(151);
        let mut out = Vec::new();
        for p in &ts {
            d.push(p, &mut out).unwrap();
        }
        assert_eq!(out.len(), 3 * 188);
        assert_eq!(d.card.calls, 1);
        let mut wrong =
            Decoder::new(Fake { calls: 0 }, [0; 32], [0; 8], 0x4010).select_service(152);
        assert!(wrong.push(&ts[0], &mut Vec::new()).is_err());
        let mut unknown = [0u8; 188];
        unknown[..4].copy_from_slice(&[0x47, 2, 0x10, 0x90]);
        out.clear();
        d.push(&unknown, &mut out).unwrap();
        assert_eq!(&out, &unknown);
        assert_eq!(out[3] >> 6, 2); // Unknown ciphertext is never labelled clear.
        assert_eq!(d.encrypted_passthrough.get(&0x210), Some(&1));
    }
    #[test]
    fn wrong_tsid_and_unknown_scrambled_pid_fail_closed() {
        let mut d = Decoder::new(Fake { calls: 0 }, [0; 32], [0; 8], 0x4011);
        let mut out = Vec::new();
        assert!(d.push(&tables()[0], &mut out).is_err());
        assert!(out.is_empty());
        let mut d = Decoder::new(Fake { calls: 0 }, [0; 32], [0; 8], 0x4010);
        for p in tables() {
            d.push(&p, &mut out).unwrap();
        }
        out.clear();
        d.routes.insert(0x142, 0x122);
        let mut p = [0u8; 188];
        p[..4].copy_from_slice(&[0x47, 2, 0, 0x90]);
        assert!(d.push(&p, &mut out).is_err());
        assert!(out.is_empty());
    }
    #[test]
    fn fragmented_sections_and_cc_gap_do_not_mix_bytes() {
        let s = section(vec![0x82, 0xf0, 0, 0, 0, 0xc1, 0, 0, 1, 2, 3, 4, 5]);
        let mut a = Sections::default();
        let mut p = [255; 188];
        p[..5].copy_from_slice(&[0x47, 0x41, 0x21, 0x30, 176]);
        p[181] = 0;
        p[182..].copy_from_slice(&s[..6]);
        assert!(a.push(&p, 181).unwrap().is_empty());
        let mut next = [255; 188];
        next[..4].copy_from_slice(&[0x47, 1, 0x21, 0x11]);
        next[4..4 + s.len() - 6].copy_from_slice(&s[6..]);
        assert_eq!(a.push(&next, 4).unwrap(), vec![s.clone()]);
        let mut a = Sections::default();
        a.push(&p, 181).unwrap();
        next[3] = 0x12;
        assert!(a.push(&next, 4).unwrap().is_empty());
    }
}

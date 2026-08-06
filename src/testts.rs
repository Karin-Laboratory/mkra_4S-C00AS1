//! A synthetic but structurally valid MPEG-TS, so the streaming path can be
//! exercised with a real client while the driver question is still open.
//!
//! It emits PAT, PMT and SDT on a repeating cycle, padded with null packets to
//! a target bitrate. There is no video or audio elementary stream — a tuner
//! client will lock onto the transport, list the service, and report packets
//! arriving, which is exactly what needs proving: HTTP framing, chunk pacing,
//! ring backpressure and packet alignment all the way to the client.
//!
//! What it does NOT prove is anything about the tuner hardware.

use std::io;
use std::time::{Duration, Instant};

use crate::stream::TS_PACKET;
use crate::tssrc::TsSource;

const PID_PAT: u16 = 0x0000;
const PID_SDT: u16 = 0x0011;
const PID_PMT: u16 = 0x1000;
const PID_PCR: u16 = 0x0100;
const PID_NULL: u16 = 0x1FFF;

/// MPEG-2 CRC-32, as used by PSI sections (poly 0x04C11DB7, MSB-first, no
/// final inversion).
fn crc32_mpeg(data: &[u8]) -> u32 {
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

/// Wrap a PSI section in a single 188-byte packet with a pointer_field.
///
/// Every section here is far below 183 bytes, so one packet is always enough
/// and no continuation logic is needed.
fn section_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; TS_PACKET] {
    assert!(section.len() <= TS_PACKET - 5, "section too long for one packet");
    let mut p = [0xFFu8; TS_PACKET];
    p[0] = 0x47;
    // payload_unit_start_indicator = 1
    p[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F); // payload only
    p[4] = 0x00; // pointer_field
    p[5..5 + section.len()].copy_from_slice(section);
    p
}

/// Build a PSI section: header, body, CRC.
fn psi_section(table_id: u8, table_id_ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(body.len() + 12);
    s.push(table_id);
    // section_syntax_indicator=1, reserved bits, length filled in below
    let length = 9 + body.len(); // 5 bytes of header after length + body + 4 CRC
    s.push(0xB0 | ((length >> 8) as u8 & 0x0F));
    s.push(length as u8);
    s.extend_from_slice(&table_id_ext.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1)); // current_next_indicator = 1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.extend_from_slice(body);
    let crc = crc32_mpeg(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

fn pat(tsid: u16, service_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&service_id.to_be_bytes());
    body.extend_from_slice(&(0xE000 | PID_PMT).to_be_bytes());
    psi_section(0x00, tsid, 0, &body)
}

fn pmt(service_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(0xE000 | PID_PCR).to_be_bytes());
    body.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length = 0
    body.push(0x24); // stream_type 0x24 = HEVC, which is what advanced BS carries
    body.extend_from_slice(&(0xE000 | PID_PCR).to_be_bytes());
    body.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length = 0
    psi_section(0x02, service_id, 0, &body)
}

/// Minimal SDT so a client shows a name rather than an unnamed service.
///
/// The name is kept to ASCII: those code points are identical in ARIB STD-B24
/// 8-bit and UTF-8, so it renders correctly without an encoder.
fn sdt(tsid: u16, network_id: u16, service_id: u16, name: &str) -> Vec<u8> {
    let name: Vec<u8> = name.bytes().filter(|b| b.is_ascii_graphic() || *b == b' ').collect();
    let provider = b"mkra";

    let mut svc_desc = Vec::new();
    svc_desc.push(0x48); // service_descriptor
    svc_desc.push((3 + provider.len() + name.len()) as u8);
    svc_desc.push(0x01); // service_type: digital TV
    svc_desc.push(provider.len() as u8);
    svc_desc.extend_from_slice(provider);
    svc_desc.push(name.len() as u8);
    svc_desc.extend_from_slice(&name);

    let mut body = Vec::new();
    body.extend_from_slice(&network_id.to_be_bytes());
    body.push(0xFF); // reserved_future_use
    body.extend_from_slice(&service_id.to_be_bytes());
    body.push(0xFC); // EIT_schedule_flag = 0, EIT_present_following_flag = 0
    // running_status = 4 (running), free_CA_mode = 0, descriptors_loop_length
    body.extend_from_slice(&(0x8000 | svc_desc.len() as u16).to_be_bytes());
    body.extend_from_slice(&svc_desc);
    psi_section(0x42, tsid, 0, &body)
}

fn null_packet(cc: u8) -> [u8; TS_PACKET] {
    let mut p = [0xFFu8; TS_PACKET];
    p[0] = 0x47;
    p[1] = (PID_NULL >> 8) as u8;
    p[2] = PID_NULL as u8;
    p[3] = 0x10 | (cc & 0x0F);
    p
}

pub struct TestPatternSource {
    tsid: u16,
    network_id: u16,
    service_id: u16,
    name: String,
    bitrate_bps: u64,
    started: Instant,
    sent_bytes: u64,
    packets: u64,
    cc: [u8; 4], // PAT, PMT, SDT, null
}

impl TestPatternSource {
    pub fn new(
        tsid: u16,
        network_id: u16,
        service_id: u16,
        name: &str,
        bitrate_bps: u64,
    ) -> Self {
        TestPatternSource {
            tsid,
            network_id,
            service_id,
            name: name.to_string(),
            bitrate_bps: bitrate_bps.max(1),
            started: Instant::now(),
            sent_bytes: 0,
            packets: 0,
            cc: [0; 4],
        }
    }

    fn next_cc(&mut self, i: usize) -> u8 {
        let v = self.cc[i];
        self.cc[i] = (v + 1) & 0x0F;
        v
    }
}

impl TsSource for TestPatternSource {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        // Pace to the requested bitrate so the client sees a realistic rate and
        // the ring's backpressure behaves as it would on air.
        let due = Duration::from_micros(self.sent_bytes * 8 * 1_000_000 / self.bitrate_bps);
        let elapsed = self.started.elapsed();
        if due > elapsed {
            let wait = (due - elapsed).min(Duration::from_millis(50));
            std::thread::sleep(wait);
            return Ok(0);
        }

        // One batch is ~50 ms of stream; PSI repeats at the head of each batch,
        // comfortably more often than the 500 ms clients expect.
        let batch = ((self.bitrate_bps / 8 / 20) as usize / TS_PACKET).max(4);
        let before = out.len();
        for _ in 0..batch {
            let n = self.packets;
            self.packets += 1;
            if n % 100 == 0 {
                let cc = self.next_cc(0);
                out.extend_from_slice(&section_packet(PID_PAT, cc, &pat(self.tsid, self.service_id)));
            } else if n % 100 == 1 {
                let cc = self.next_cc(1);
                out.extend_from_slice(&section_packet(PID_PMT, cc, &pmt(self.service_id)));
            } else if n % 100 == 2 {
                let cc = self.next_cc(2);
                let s = sdt(self.tsid, self.network_id, self.service_id, &self.name);
                out.extend_from_slice(&section_packet(PID_SDT, cc, &s));
            } else {
                let cc = self.next_cc(3);
                out.extend_from_slice(&null_packet(cc));
            }
        }
        let n = out.len() - before;
        self.sent_bytes += n as u64;
        Ok(n)
    }

    fn describe(&self) -> String {
        format!(
            "test pattern (TSID {:04X}, service {}, {} bit/s)",
            self.tsid, self.service_id, self.bitrate_bps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vector: the CRC of an empty PAT body, checked against the
    /// well-known MPEG-2 CRC-32 of the ASCII string "123456789".
    #[test]
    fn crc32_matches_the_reference_vector() {
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376E6E7);
    }

    #[test]
    fn sections_carry_a_self_consistent_crc() {
        for section in [pat(0xB110, 101), pmt(101), sdt(0xB110, 11, 101, "mkra test")] {
            // A section's CRC covers everything before it; recomputing over the
            // whole section including the CRC must yield zero.
            assert_eq!(crc32_mpeg(&section), 0, "bad CRC in section {:02X}", section[0]);
        }
    }

    #[test]
    fn section_length_field_matches_the_actual_length() {
        for section in [pat(0xB110, 101), pmt(101), sdt(0xB110, 11, 101, "x")] {
            let declared = (((section[1] as usize & 0x0F) << 8) | section[2] as usize) + 3;
            assert_eq!(declared, section.len());
        }
    }

    #[test]
    fn every_emitted_packet_is_188_bytes_and_starts_with_sync() {
        let mut s = TestPatternSource::new(0xB110, 11, 101, "mkra test", 8_000_000);
        let mut out = Vec::new();
        for _ in 0..50 {
            let _ = s.read_into(&mut out).unwrap();
            if out.len() > 188 * 200 {
                break;
            }
        }
        assert!(!out.is_empty());
        assert_eq!(out.len() % TS_PACKET, 0);
        assert_eq!(crate::xcdrv::ts_sync_ratio(&out), 1.0);
    }

    #[test]
    fn psi_tables_all_appear_within_a_cycle() {
        let mut s = TestPatternSource::new(0xB110, 11, 101, "mkra test", 8_000_000);
        let mut out = Vec::new();
        while out.len() < 188 * 300 {
            let _ = s.read_into(&mut out).unwrap();
        }
        let pids: Vec<u16> = out
            .chunks_exact(TS_PACKET)
            .map(|p| (((p[1] as u16) & 0x1F) << 8) | p[2] as u16)
            .collect();
        assert!(pids.contains(&PID_PAT), "no PAT");
        assert!(pids.contains(&PID_PMT), "no PMT");
        assert!(pids.contains(&PID_SDT), "no SDT");
        assert!(pids.contains(&PID_NULL), "no stuffing");
    }

    #[test]
    fn continuity_counters_increment_per_pid() {
        let mut s = TestPatternSource::new(0xB110, 11, 101, "t", 8_000_000);
        let mut out = Vec::new();
        while out.len() < 188 * 300 {
            let _ = s.read_into(&mut out).unwrap();
        }
        let mut last: std::collections::BTreeMap<u16, u8> = Default::default();
        for p in out.chunks_exact(TS_PACKET) {
            let pid = (((p[1] as u16) & 0x1F) << 8) | p[2] as u16;
            let cc = p[3] & 0x0F;
            if let Some(prev) = last.insert(pid, cc) {
                assert_eq!(cc, (prev + 1) & 0x0F, "continuity break on PID {pid:#x}");
            }
        }
    }
}

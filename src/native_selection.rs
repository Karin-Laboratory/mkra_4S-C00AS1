//! Read-only selected-service confirmation from the verified OEM ADSB image.
//!
//! Presentation SI files can remain stale after BS2K. The live TUM session
//! contains the request at +0x10 and acquired package identity at +0x30.
//! A request alone is insufficient: during measured 141->161 tuning, +0x1c
//! already named 161 while +0x30 still named 141 and +0x28/+0x2c were busy.
use crate::{channels, si::LiveService, snapshot::procmem};
use std::{fs, io, process::Command};

const SESSION: u32 = 0x0039_a3fc;
const PREFIX_LEN: usize = 64;
const ADSB_MD5: &str = "44df75ed0aa1d98ffe30239695b1e5bf";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub live: LiveService,
    pub oid: u8,
}

pub struct Reader {
    pid: u32,
}
impl Reader {
    pub fn open() -> io::Result<Self> {
        let pid = procmem::find_pid("adsb")?;
        let exe = format!("/proc/{pid}/exe");
        let hash = Command::new("md5sum").arg(&exe).output()?;
        if !hash.status.success()
            || String::from_utf8_lossy(&hash.stdout)
                .split_whitespace()
                .next()
                != Some(ADSB_MD5)
        {
            return Err(io::Error::other(
                "unrecognised adsb image; refusing fixed-address selection read",
            ));
        }
        let maps = procmem::maps(pid)?;
        if !procmem::is_writable(&maps, SESSION)
            || !procmem::is_writable(&maps, SESSION + PREFIX_LEN as u32 - 1)
        {
            return Err(io::Error::other(
                "ADSB session outside expected writable mapping",
            ));
        }
        Ok(Self { pid })
    }
    pub fn selected(&self) -> io::Result<Option<Selection>> {
        if fs::read_to_string(format!("/proc/{}/comm", self.pid))?.trim() != "adsb" {
            return Err(io::Error::other("ADSB process identity changed"));
        }
        Ok(parse(&procmem::read(self.pid, SESSION, PREFIX_LEN)?))
    }
}

fn parse(bytes: &[u8]) -> Option<Selection> {
    if bytes.len() != PREFIX_LEN {
        return None;
    }
    let word = |o| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let half = |o| u16::from_le_bytes(bytes[o..o + 2].try_into().unwrap());
    // +0x28 and +0x2c are TUM activity/state; both must settle. +0x30 is
    // acquired package SID/NID, consumed by native service/asset setup.
    if word(0x10) != 9 || word(0x24) != 0 || word(0x28) != 0 || word(0x2c) != 0 {
        return None;
    }
    let sid = half(0x30);
    let nid = half(0x32);
    if nid != channels::NETWORK_ID || half(0x1a) != nid || half(0x1c) != sid {
        return None;
    }
    let (tsid, freq, bid) = channels::tuning_record(sid)?;
    if half(0x1e) != tsid || word(0x20) != freq || bytes[0x19] != bid {
        return None;
    }
    Some(Selection {
        live: LiveService {
            service_id: sid,
            original_network_id: nid,
            tlv_stream_id: tsid,
        },
        oid: bytes[0x18],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(sid: u16) -> Vec<u8> {
        let (tsid, freq, bid) = channels::tuning_record(sid).unwrap();
        let mut b = vec![0; 64];
        b[16..20].copy_from_slice(&9u32.to_le_bytes());
        b[24] = 122;
        b[25] = bid;
        b[26..28].copy_from_slice(&11u16.to_le_bytes());
        b[28..30].copy_from_slice(&sid.to_le_bytes());
        b[30..32].copy_from_slice(&tsid.to_le_bytes());
        b[32..36].copy_from_slice(&freq.to_le_bytes());
        b[48..50].copy_from_slice(&sid.to_le_bytes());
        b[50..52].copy_from_slice(&11u16.to_le_bytes());
        b
    }
    #[test]
    fn synthetic_141_to_161_state_is_unconfirmed_until_package_acquisition() {
        let mut bytes = fixture(161);
        let confirmed = parse(&bytes).unwrap();
        assert_eq!(confirmed.live.service_id, 161);
        assert_eq!(confirmed.oid, 122);
        bytes[40..44].copy_from_slice(&2u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&1u32.to_le_bytes());
        bytes[48..50].copy_from_slice(&141u16.to_le_bytes());
        assert!(parse(&bytes).is_none());
        bytes[40..48].fill(0);
        assert!(
            parse(&bytes).is_none(),
            "idle flags alone do not confirm new package"
        );
    }

    #[test]
    fn confirms_all_six_main_4k_services_from_acquired_identity() {
        for sid in [101, 141, 151, 161, 171, 181] {
            assert_eq!(parse(&fixture(sid)).unwrap().live.service_id, sid);
        }
    }
    #[test]
    fn new_request_with_previous_acquired_package_is_not_confirmation() {
        let mut b = fixture(161);
        b[48..50].copy_from_slice(&141u16.to_le_bytes());
        assert!(parse(&b).is_none());
    }
    #[test]
    fn rejects_busy_wrong_rf_tsid_network_and_incomplete_state() {
        for offset in [16, 25, 26, 28, 30, 32, 36, 40, 44, 48, 50] {
            let mut b = fixture(141);
            b[offset] ^= 1;
            assert!(parse(&b).is_none(), "offset {offset}");
        }
        assert!(parse(&fixture(141)[..63]).is_none());
    }
}

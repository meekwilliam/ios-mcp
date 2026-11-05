// h.264 instead of hevc, many windows pcs can't decode hevc without a paid store extension, idevice's offer lists
// both and the phone picks hevc, so this offers only h.264

use std::{
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, write::ZlibEncoder};
use idevice::core_device::CallInfoBlob;

use crate::{Error, Result};

const PAYLOAD_TYPE: u8 = 123;

pub fn offer(call_id: &str, call_info: &CallInfoBlob, ssrc: u32) -> Result<Vec<u8>> {
    let blob = media_blob(ssrc, ntp_now());
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib.write_all(&blob)?;
    let blob = zlib.finish()?;

    let mut offer = plist::Dictionary::new();
    offer.insert("avcMediaStreamOptionCallID".into(), call_id.into());
    offer.insert("avcMediaStreamNegotiatorMediaBlob".into(), plist::Value::Data(blob));
    // 5 is coredevice screen sharing, the audio offer uses 6
    offer.insert("avcMediaStreamNegotiatorMode".into(), 5.into());
    offer.insert(
        "avcMediaStreamOptionRemoteEndpointInfo".into(),
        plist::Value::Data(call_info.encode()),
    );
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &plist::Value::Dictionary(offer)).map_err(|e| Error::Other(e.to_string()))?;
    Ok(out)
}

// values are what device hub sends, see idevice's negotiation.rs
fn media_blob(ssrc: u32, ntp_time: u64) -> Vec<u8> {
    fn format(e: &mut Proto, variant: u64) {
        e.message(2, |m| {
            m.uint(1, 1);
            m.uint(2, variant);
            m.uint(3, 0xc3c3);
            m.uint(4, 0);
        });
    }
    fn setting(e: &mut Proto, id: u64, value: u64, flags: Option<u64>) {
        e.message(9, |m| {
            m.uint(1, id);
            m.uint(2, value);
            if let Some(flags) = flags {
                m.uint(3, flags);
            }
        });
    }

    let mut e = Proto::default();
    e.uint(1, 1);
    e.uint(2, 1);
    e.message(5, |m| {
        m.uint(1, ssrc as u64);
        m.uint(2, 0);
        m.message(3, |c| {
            c.uint(1, PAYLOAD_TYPE as u64);
            format(c, 1);
            format(c, 2);
            format(c, 1);
            format(c, 2);
            c.bytes(3, b"FLS;SW:1;");
            c.uint(4, 1);
        });
        m.uint(7, 1);
        m.uint(8, 63);
        m.uint(12, 1);
    });
    e.bytes(6, b"Viceroy 1.7.0");
    e.uint(8, 0);
    setting(&mut e, 0, 20_000_000, Some(0x18000));
    setting(&mut e, 4074, 0, Some(0x4000));
    setting(&mut e, 0, 60_000_000, Some(0x40000));
    setting(&mut e, 0, 40_000_000, Some(0x3000));
    setting(&mut e, 16, 4100, None);
    setting(&mut e, 0, 6_000_000, Some(0x20000));
    setting(&mut e, 4, 6500, None);
    setting(&mut e, 0, 100_000_000, Some(0x100000));
    setting(&mut e, 1, 299, None);
    setting(&mut e, 0, 75_000_000, Some(0x80000));
    e.uint(13, ntp_time);
    e.uint(14, 2);
    e.uint(16, 0);
    e.uint(18, 1);
    e.0
}

fn ntp_now() -> u64 {
    const UNIX_TO_NTP: u64 = 2_208_988_800;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    ((now.as_secs() + UNIX_TO_NTP) << 32) | ((now.subsec_nanos() as u64) << 32) / 1_000_000_000
}

#[derive(Default)]
struct Proto(Vec<u8>);

impl Proto {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn uint(&mut self, field: u64, v: u64) {
        self.varint(field << 3);
        self.varint(v);
    }

    fn bytes(&mut self, field: u64, v: &[u8]) {
        self.varint((field << 3) | 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
    }

    fn message(&mut self, field: u64, build: impl FnOnce(&mut Proto)) {
        let mut inner = Proto::default();
        build(&mut inner);
        self.bytes(field, &inner.0);
    }
}

// rfc 6184 plus apple's own nal type 18, a quicktime `avc1` description with the sps and pps in its `avcC` box
#[derive(Default)]
pub struct Depacketizer {
    out: Vec<u8>,
    fragment: Option<Vec<u8>>,
}

impl Depacketizer {
    pub fn push(&mut self, payload: &[u8]) {
        let Some(&header) = payload.first() else { return };
        match header & 0x1F {
            1..=23 => self.nal(payload),
            24 => {
                let mut rest = &payload[1..];
                while rest.len() >= 2 {
                    let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    let Some(nal) = rest.get(2..2 + size) else { break };
                    self.nal(nal);
                    rest = &rest[2 + size..];
                }
            }
            28 if payload.len() > 2 => {
                let fu = payload[1];
                if fu & 0x80 != 0 {
                    self.fragment = Some(vec![(header & 0xE0) | (fu & 0x1F)]);
                }
                if let Some(fragment) = self.fragment.as_mut() {
                    fragment.extend_from_slice(&payload[2..]);
                }
                if fu & 0x40 != 0
                    && let Some(fragment) = self.fragment.take()
                {
                    self.nal(&fragment);
                }
            }
            _ => {}
        }
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn nal(&mut self, nal: &[u8]) {
        if nal[0] & 0x1F == 18 {
            for set in parameter_sets(nal) {
                self.nal(set);
            }
            return;
        }
        self.out.extend_from_slice(&[0, 0, 0, 1]);
        match nal[0] & 0x1F {
            7 => self
                .out
                .extend_from_slice(&in_order(nal).unwrap_or_else(|| nal.to_vec())),
            _ => self.out.extend_from_slice(nal),
        }
    }
}

// the phone's sps leaves out max_num_reorder_frames, so decoders hold a dpb's worth of frames, about 300 ms, in case
// they come out of order, they never do
fn in_order(sps: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(&sps[1..]);
    let mut r = Bits { data: &rbsp, at: 0 };
    let profile = r.u(8)?;
    r.u(16)?;
    r.ue()?;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        if r.ue()? == 3 {
            r.u(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.u(1)?;
        if r.u(1)? == 1 {
            return None;
        }
    }
    r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let refs = r.ue()?;
    r.u(1)?;
    r.ue()?;
    r.ue()?;
    if r.u(1)? == 0 {
        r.u(1)?;
    }
    r.u(1)?;
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }

    let mut w = Writer::default();
    let vui_flag = r.at;
    if r.u(1)? == 0 {
        w.copy(&rbsp, vui_flag);
        w.bit(1);
        w.bits(0, 9);
    } else {
        if r.u(1)? == 1 && r.u(8)? == 255 {
            r.u(32)?;
        }
        if r.u(1)? == 1 {
            r.u(1)?;
        }
        if r.u(1)? == 1 {
            r.u(4)?;
            if r.u(1)? == 1 {
                r.u(24)?;
            }
        }
        if r.u(1)? == 1 {
            r.ue()?;
            r.ue()?;
        }
        if r.u(1)? == 1 {
            r.u(65)?;
        }
        let nal_hrd = r.u(1)? == 1 && r.hrd().is_some();
        let vcl_hrd = r.u(1)? == 1 && r.hrd().is_some();
        if nal_hrd || vcl_hrd {
            r.u(1)?;
        }
        r.u(1)?;
        let restriction = r.at;
        if r.u(1)? == 1 {
            return None;
        }
        w.copy(&rbsp, restriction);
    }
    w.bit(1);
    w.bit(1);
    w.ue(2);
    w.ue(1);
    w.ue(16);
    w.ue(16);
    w.ue(0);
    w.ue(refs.max(1));
    w.bit(1);
    while w.len % 8 != 0 {
        w.bit(0);
    }
    Some([&[sps[0]][..], &escape(&w.data)].concat())
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: usize) -> Option<u64> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = (v << 1) | ((byte >> (7 - self.at % 8)) & 1) as u64;
            self.at += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let v = self.ue()? as i64;
        Some(if v % 2 == 1 { (v + 1) / 2 } else { -v / 2 })
    }

    fn hrd(&mut self) -> Option<()> {
        let count = self.ue()? + 1;
        self.u(8)?;
        for _ in 0..count {
            self.ue()?;
            self.ue()?;
            self.u(1)?;
        }
        self.u(20)?;
        Some(())
    }
}

#[derive(Default)]
struct Writer {
    data: Vec<u8>,
    len: usize,
}

impl Writer {
    fn bit(&mut self, b: u8) {
        if self.len % 8 == 0 {
            self.data.push(0);
        }
        if b != 0 {
            *self.data.last_mut().unwrap() |= 0x80 >> (self.len % 8);
        }
        self.len += 1;
    }

    fn bits(&mut self, v: u64, n: usize) {
        for i in (0..n).rev() {
            self.bit(((v >> i) & 1) as u8);
        }
    }

    fn ue(&mut self, v: u64) {
        let n = 64 - (v + 1).leading_zeros() as usize;
        self.bits(0, n - 1);
        self.bits(v + 1, n);
    }

    fn copy(&mut self, from: &[u8], bits: usize) {
        for i in 0..bits {
            self.bit((from[i / 8] >> (7 - i % 8)) & 1);
        }
    }
}

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_reorder_limit_to_the_phones_sps() {
        let sps = [
            0x27, 0x64, 0x00, 0x33, 0x4b, 0x04, 0xc5, 0x14, 0x05, 0x10, 0x16, 0x1a, 0x6e, 0x04, 0x04, 0x04, 0x04,
        ];
        let out = super::in_order(&sps).expect("rewritten");
        assert!(out.len() > sps.len());
        assert!(
            super::in_order(&out).is_none(),
            "second pass sees the restriction already there"
        );
    }
}

fn parameter_sets(packet: &[u8]) -> Vec<&[u8]> {
    let mut sets = Vec::new();
    let Some(at) = packet.windows(4).position(|w| w == b"avcC") else {
        return sets;
    };
    let avcc = &packet[at + 4..];
    let Some(&count) = avcc.get(5) else { return sets };
    let mut rest = &avcc[6..];
    let mut remaining = (count & 0x1F) as usize;
    let mut pps_next = true;
    loop {
        while remaining > 0 {
            if rest.len() < 2 {
                return sets;
            }
            let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            let Some(set) = rest.get(2..2 + size) else { return sets };
            sets.push(set);
            rest = &rest[2 + size..];
            remaining -= 1;
        }
        if !pps_next || rest.is_empty() {
            return sets;
        }
        remaining = rest[0] as usize;
        rest = &rest[1..];
        pps_next = false;
    }
}

pub fn has_key_frame(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 5)
}

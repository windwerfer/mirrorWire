//! Video frames travel as unreliable QUIC datagrams. Each encoded frame is split
//! into fragments that each carry a fixed header, so the receiver can reassemble
//! frames, detect gaps, and measure per-stage latency without extra round trips.

use bytes::{BufMut, Bytes, BytesMut};
use std::collections::BTreeMap;

pub const HEADER_LEN: usize = 34;
const WIRE_VERSION: u8 = 1;
const FLAG_KEYFRAME: u8 = 0b0000_0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    pub capture_us: u64,
    pub encoded_us: u64,
    pub sent_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    pub keyframe: bool,
    pub frame_id: u32,
    pub index: u16,
    pub count: u16,
    pub timing: FrameTiming,
}

impl FragmentHeader {
    fn write(&self, buf: &mut BytesMut) {
        buf.put_u8(WIRE_VERSION);
        buf.put_u8(if self.keyframe { FLAG_KEYFRAME } else { 0 });
        buf.put_u32(self.frame_id);
        buf.put_u16(self.index);
        buf.put_u16(self.count);
        buf.put_u64(self.timing.capture_us);
        buf.put_u64(self.timing.encoded_us);
        buf.put_u64(self.timing.sent_us);
    }

    pub fn parse(datagram: &[u8]) -> Option<(Self, &[u8])> {
        if datagram.len() < HEADER_LEN || datagram[0] != WIRE_VERSION {
            return None;
        }
        let u16_at = |o: usize| u16::from_be_bytes([datagram[o], datagram[o + 1]]);
        let u32_at = |o: usize| u32::from_be_bytes(datagram[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_be_bytes(datagram[o..o + 8].try_into().unwrap());
        let header = Self {
            keyframe: datagram[1] & FLAG_KEYFRAME != 0,
            frame_id: u32_at(2),
            index: u16_at(6),
            count: u16_at(8),
            timing: FrameTiming {
                capture_us: u64_at(10),
                encoded_us: u64_at(18),
                sent_us: u64_at(26),
            },
        };
        if header.count == 0 || header.index >= header.count {
            return None;
        }
        Some((header, &datagram[HEADER_LEN..]))
    }
}

/// Splits one encoded frame into datagrams no larger than `max_datagram`.
pub fn fragment(
    frame_id: u32,
    keyframe: bool,
    timing: FrameTiming,
    payload: &[u8],
    max_datagram: usize,
) -> Vec<Bytes> {
    let chunk = max_datagram.saturating_sub(HEADER_LEN).max(1);
    let count = payload.len().div_ceil(chunk).max(1);
    assert!(count <= u16::MAX as usize, "frame too large to fragment");
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * chunk;
        let end = (start + chunk).min(payload.len());
        let mut buf = BytesMut::with_capacity(HEADER_LEN + end - start);
        FragmentHeader {
            keyframe,
            frame_id,
            index: index as u16,
            count: count as u16,
            timing,
        }
        .write(&mut buf);
        buf.extend_from_slice(&payload[start..end]);
        out.push(buf.freeze());
    }
    out
}

#[derive(Debug, Clone)]
pub struct AssembledFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub timing: FrameTiming,
    pub data: Vec<u8>,
    pub fragments: u16,
    pub first_fragment_us: u64,
    pub completed_us: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyEvent {
    /// Number of frames skipped (never completed) before the delivered one.
    pub lost_frames: u32,
}

struct Partial {
    keyframe: bool,
    timing: FrameTiming,
    parts: Vec<Option<Bytes>>,
    received: u16,
    bytes: usize,
    first_us: u64,
}

/// Reassembles fragments into frames and delivers them strictly in order.
/// A frame that completes after a newer one has been delivered is dropped, and
/// any frames skipped over are reported as lost.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    max_age_us: u64,
}

impl Reassembler {
    pub fn new(max_age_us: u64) -> Self {
        Self {
            partial: BTreeMap::new(),
            last_delivered: None,
            max_age_us,
        }
    }

    pub fn push(&mut self, datagram: Bytes, now_us: u64) -> Option<(AssembledFrame, ReassemblyEvent)> {
        let (header, _) = FragmentHeader::parse(&datagram)?;
        if let Some(last) = self.last_delivered {
            if header.frame_id <= last {
                return None;
            }
        }
        self.evict_stale(now_us);
        let entry = self.partial.entry(header.frame_id).or_insert_with(|| Partial {
            keyframe: header.keyframe,
            timing: header.timing,
            parts: vec![None; header.count as usize],
            received: 0,
            bytes: 0,
            first_us: now_us,
        });
        let slot = entry.parts.get_mut(header.index as usize)?;
        if slot.is_some() {
            return None;
        }
        entry.bytes += datagram.len() - HEADER_LEN;
        *slot = Some(datagram.slice(HEADER_LEN..));
        entry.received += 1;
        if entry.received as usize != entry.parts.len() {
            return None;
        }

        let done = self.partial.remove(&header.frame_id)?;
        let lost_frames = match self.last_delivered {
            Some(last) => header.frame_id - last - 1,
            None => 0,
        };
        self.partial.retain(|&id, _| id > header.frame_id);
        self.last_delivered = Some(header.frame_id);

        let mut data = Vec::with_capacity(done.bytes);
        for part in done.parts.iter().flatten() {
            data.extend_from_slice(part);
        }
        Some((
            AssembledFrame {
                frame_id: header.frame_id,
                keyframe: done.keyframe,
                timing: done.timing,
                data,
                fragments: header.count,
                first_fragment_us: done.first_us,
                completed_us: now_us,
            },
            ReassemblyEvent { lost_frames },
        ))
    }

    pub fn pending(&self) -> usize {
        self.partial.len()
    }

    fn evict_stale(&mut self, now_us: u64) {
        let max_age = self.max_age_us;
        self.partial
            .retain(|_, p| now_us.saturating_sub(p.first_us) <= max_age);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: FrameTiming = FrameTiming {
        capture_us: 1,
        encoded_us: 2,
        sent_us: 3,
    };

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn roundtrip_out_of_order() {
        let data = payload(10_000);
        let mut frags = fragment(7, true, T, &data, 1200);
        frags.reverse();
        let mut r = Reassembler::new(1_000_000);
        let mut out = None;
        for f in frags {
            if let Some(x) = r.push(f, 10) {
                out = Some(x);
            }
        }
        let (frame, ev) = out.expect("frame assembled");
        assert_eq!(frame.data, data);
        assert!(frame.keyframe);
        assert_eq!(frame.timing, T);
        assert_eq!(ev.lost_frames, 0);
    }

    #[test]
    fn detects_loss_and_drops_late() {
        let mut r = Reassembler::new(1_000_000);
        let f1 = fragment(1, false, T, &payload(3000), 1200);
        let f2 = fragment(2, false, T, &payload(3000), 1200);
        let f3 = fragment(3, false, T, &payload(100), 1200);
        for f in &f1 {
            r.push(f.clone(), 0);
        }
        r.push(f2[0].clone(), 0);
        let (frame, ev) = r.push(f3[0].clone(), 0).unwrap();
        assert_eq!(frame.frame_id, 3);
        assert_eq!(ev.lost_frames, 1);
        assert!(r.push(f2[1].clone(), 0).is_none());
        assert_eq!(r.pending(), 0);
    }

    #[test]
    fn empty_payload_is_one_fragment() {
        let frags = fragment(0, false, T, &[], 1200);
        assert_eq!(frags.len(), 1);
        let mut r = Reassembler::new(1);
        assert!(r.push(frags[0].clone(), 0).unwrap().0.data.is_empty());
    }
}

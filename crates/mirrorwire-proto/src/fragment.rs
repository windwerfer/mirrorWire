//! Video frames travel as unreliable QUIC datagrams. Each encoded frame is split
//! into fragments that each carry a fixed header, so the receiver can reassemble
//! frames, detect gaps, and measure per-stage latency without extra round trips.

use bytes::{BufMut, Bytes, BytesMut};
use crate::fec::{block_len, recover_block, FecHeader, LinkStats};
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
    /// Data fragments the frame was split into (parity not counted).
    pub fragments: u16,
    pub first_fragment_us: u64,
    pub completed_us: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyEvent {
    /// Number of frames skipped (never completed) before the delivered one.
    pub lost_frames: u32,
    /// Data fragments of this frame that were rebuilt from FEC parity.
    pub recovered: u32,
}

/// One received video packet, version 1 (plain) or 2 (FEC).
struct Packet<'a> {
    frame_id: u32,
    keyframe: bool,
    timing: FrameTiming,
    index: usize,
    data_count: usize,
    fec: Option<FecHeader>,
    payload: &'a [u8],
}

fn parse_packet(d: &[u8]) -> Option<Packet<'_>> {
    if let Some((h, payload)) = FragmentHeader::parse(d) {
        return Some(Packet {
            frame_id: h.frame_id,
            keyframe: h.keyframe,
            timing: h.timing,
            index: h.index as usize,
            data_count: h.count as usize,
            fec: None,
            payload,
        });
    }
    let (h, payload) = FecHeader::parse(d)?;
    Some(Packet {
        frame_id: h.frame_id,
        keyframe: h.keyframe,
        timing: h.timing,
        index: h.index as usize,
        data_count: h.data_count as usize,
        fec: Some(h),
        payload,
    })
}

struct Partial {
    keyframe: bool,
    timing: FrameTiming,
    /// Layout of the first FEC packet (all later ones must match).
    fec: Option<FecHeader>,
    data: Vec<Option<Bytes>>,
    have_data: usize,
    parity: Vec<Option<Bytes>>,
    /// Shards (data + parity) received per FEC block, and blocks already
    /// complete or rebuilt.
    block_have: Vec<u16>,
    block_done: Vec<bool>,
    recovered: u32,
    bytes: usize,
    first_us: u64,
}

impl Partial {
    fn new(p: &Packet<'_>, now_us: u64) -> Self {
        let (parity, blocks) = p.fec.map_or((0, 0), |h| (h.parity_count as usize, h.blocks as usize));
        Self {
            keyframe: p.keyframe,
            timing: p.timing,
            fec: p.fec,
            data: vec![None; p.data_count],
            have_data: 0,
            parity: vec![None; parity],
            block_have: vec![0; blocks],
            block_done: vec![false; blocks],
            recovered: 0,
            bytes: 0,
            first_us: now_us,
        }
    }

    /// Stores one packet; returns false for duplicates and mismatches.
    fn add(&mut self, p: &Packet<'_>, datagram: &Bytes, offset: usize) -> bool {
        match (&self.fec, &p.fec) {
            (None, None) if p.data_count == self.data.len() => {}
            (Some(a), Some(b)) if a.same_layout(b) => {}
            _ => return false,
        }
        let body = datagram.slice(offset..offset + p.payload.len());
        let k = self.data.len();
        let slot = if p.index < k { &mut self.data[p.index] } else { &mut self.parity[p.index - k] };
        if slot.is_some() {
            return false;
        }
        *slot = Some(body);
        if p.index < k {
            self.have_data += 1;
            self.bytes += p.payload.len();
        }
        if let Some(h) = self.fec {
            let blocks = h.blocks as usize;
            let b = if p.index < k { p.index % blocks } else { (p.index - k) % blocks };
            self.block_have[b] += 1;
            self.try_recover(&h, b);
        }
        true
    }

    fn try_recover(&mut self, h: &FecHeader, b: usize) {
        let blocks = h.blocks as usize;
        let k = self.data.len();
        let kb = block_len(k, blocks, b);
        if self.block_done[b] || (self.block_have[b] as usize) < kb {
            return;
        }
        self.block_done[b] = true;
        let missing = (b..k).step_by(blocks).any(|i| self.data[i].is_none());
        if !missing {
            return;
        }
        for (i, bytes) in recover_block(h, b, &self.data, &self.parity).unwrap_or_default() {
            if self.data[i].is_none() {
                self.bytes += bytes.len();
                self.data[i] = Some(bytes);
                self.have_data += 1;
                self.recovered += 1;
            }
        }
    }

    fn complete(&self) -> bool {
        self.have_data == self.data.len()
    }
}

/// Partial frames kept at once (bounds memory against a flood of frame ids).
const MAX_PARTIAL: usize = 64;
/// Frames this far behind the newest are final for loss accounting.
const METER_LAG: u32 = 30;

/// Reassembles fragments into frames and delivers them strictly in order.
/// A frame that completes after a newer one has been delivered is dropped, and
/// any frames skipped over are reported as lost. Version-2 (FEC) packets are
/// rebuilt from parity as soon as enough of a block has arrived.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    max_age_us: u64,
    /// frame_id -> (packets sent, data packets, packets received).
    meter: BTreeMap<u32, (u32, u32, u32)>,
    newest: u32,
    stats: LinkStats,
}

impl Reassembler {
    pub fn new(max_age_us: u64) -> Self {
        Self {
            partial: BTreeMap::new(),
            last_delivered: None,
            max_age_us,
            meter: BTreeMap::new(),
            newest: 0,
            stats: LinkStats::default(),
        }
    }

    pub fn push(&mut self, datagram: Bytes, now_us: u64) -> Option<(AssembledFrame, ReassemblyEvent)> {
        let packet = parse_packet(&datagram)?;
        let offset = datagram.len() - packet.payload.len();
        self.meter(&packet);
        if let Some(last) = self.last_delivered {
            if packet.frame_id <= last {
                return None;
            }
        }
        self.evict_stale(now_us);
        if !self.partial.contains_key(&packet.frame_id) && self.partial.len() >= MAX_PARTIAL {
            self.partial.pop_first();
        }
        let frame_id = packet.frame_id;
        let entry = self
            .partial
            .entry(frame_id)
            .or_insert_with(|| Partial::new(&packet, now_us));
        let before = entry.recovered;
        if !entry.add(&packet, &datagram, offset) {
            return None;
        }
        self.stats.recovered += (entry.recovered - before) as u64;
        if !entry.complete() {
            return None;
        }

        let done = self.partial.remove(&frame_id)?;
        let lost_frames = match self.last_delivered {
            Some(last) => frame_id - last - 1,
            None => 0,
        };
        self.stats.unrecoverable_frames += lost_frames as u64;
        self.partial.retain(|&id, _| id > frame_id);
        self.last_delivered = Some(frame_id);

        let mut data = Vec::with_capacity(done.bytes);
        for part in done.data.iter().flatten() {
            data.extend_from_slice(part);
        }
        Some((
            AssembledFrame {
                frame_id,
                keyframe: done.keyframe,
                timing: done.timing,
                data,
                fragments: done.data.len() as u16,
                first_fragment_us: done.first_us,
                completed_us: now_us,
            },
            ReassemblyEvent { lost_frames, recovered: done.recovered },
        ))
    }

    pub fn pending(&self) -> usize {
        self.partial.len()
    }

    /// Cumulative packet counters (loss is counted once a frame is
    /// [`METER_LAG`] frames old, so late packets still count as received).
    pub fn link_stats(&self) -> LinkStats {
        self.stats
    }

    fn meter(&mut self, p: &Packet<'_>) {
        let total = p.data_count as u32 + p.fec.map_or(0, |h| h.parity_count as u32);
        if p.frame_id.saturating_add(METER_LAG) < self.newest {
            return; // already finalized
        }
        let e = self.meter.entry(p.frame_id).or_insert((total, p.data_count as u32, 0));
        e.2 = e.2.saturating_add(1);
        if p.frame_id > self.newest {
            self.newest = p.frame_id;
        }
        while let Some((&id, _)) = self.meter.first_key_value() {
            if id.saturating_add(METER_LAG) >= self.newest && self.meter.len() <= 4 * METER_LAG as usize {
                break;
            }
            let (_, (total, data, got)) = self.meter.pop_first().expect("non-empty");
            self.stats.expected += total as u64;
            self.stats.received += got.min(total) as u64;
            self.stats.data_expected += data as u64;
            self.stats.parity_expected += (total - data) as u64;
        }
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
        assert_eq!(ev.recovered, 0);
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
        assert_eq!(r.link_stats().unrecoverable_frames, 1);
    }

    #[test]
    fn empty_payload_is_one_fragment() {
        let frags = fragment(0, false, T, &[], 1200);
        assert_eq!(frags.len(), 1);
        let mut r = Reassembler::new(1);
        assert!(r.push(frags[0].clone(), 0).unwrap().0.data.is_empty());
    }
}

#[cfg(test)]
mod meter_tests {
    use super::*;

    #[test]
    fn link_stats_count_loss_after_lag() {
        let t = FrameTiming { capture_us: 0, encoded_us: 0, sent_us: 0 };
        let mut r = Reassembler::new(u64::MAX);
        for id in 0..100u32 {
            let pkts = crate::fec::fec_fragment(id, false, t, &vec![1u8; 11_000], 1200, 0.2);
            for (i, p) in pkts.iter().enumerate() {
                if i != 3 {
                    r.push(p.clone(), 0);
                }
            }
        }
        let s = r.link_stats();
        // 10 data + 2 parity per frame, one lost each; 69 frames are final.
        assert_eq!((s.expected, s.received), (69 * 12, 69 * 11));
        assert_eq!((s.data_expected, s.parity_expected), (69 * 10, 69 * 2));
        assert_eq!(s.recovered, 100);
        assert_eq!(s.unrecoverable_frames, 0);
        assert!((s.loss_pct() - 100.0 / 12.0).abs() < 0.01);
        assert!((s.overhead_pct() - 20.0).abs() < 0.01);
    }
}

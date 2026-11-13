//! Forward error correction for video datagrams (wire version 2).
//!
//! Each frame's payload is cut into equal-size data shards (the last one is
//! sent unpadded). Data shard `i` belongs to block `i % blocks`, so
//! consecutive packets land in different blocks and a short burst of loss
//! costs each block only a packet or two. Every block gets Reed-Solomon
//! parity shards (`reed-solomon-simd`); the viewer rebuilds a block as soon as
//! any `k` of its `k + m` shards have arrived, without a round trip.
//!
//! Only viewers that list [`CAP_FEC`] in their hello get version-2 packets;
//! older viewers keep getting plain version-1 fragments.

use crate::fragment::FrameTiming;
use bytes::{BufMut, Bytes, BytesMut};
use std::borrow::Cow;

/// Hello capability: the viewer reassembles version-2 (FEC) video packets
/// and sends `link_report` messages.
pub const CAP_FEC: &str = "fec-rs1";
pub const FEC_WIRE_VERSION: u8 = 2;
pub const FEC_HEADER_LEN: usize = 44;
/// Data shards per block before a frame is split into more blocks.
pub const MAX_BLOCK_DATA: usize = 64;
const MAX_BLOCKS: usize = 255;
/// Bigger claimed frames are rejected (hostile input).
pub const MAX_FRAME_LEN: usize = 64 << 20;
const FLAG_KEYFRAME: u8 = 0b0000_0001;

/// Presenter setting: no FEC, adaptive, or a fixed overhead percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FecMode {
    Off,
    #[default]
    Auto,
    /// Parity as a percentage of data packets for delta frames (1–100);
    /// keyframes get 1.5× that.
    Fixed(u8),
}

impl FecMode {
    /// `off`, `auto`, or a percentage such as `20` / `20%`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "off" | "none" | "0" | "0%" => Some(FecMode::Off),
            "auto" => Some(FecMode::Auto),
            _ => {
                let n: u8 = s.strip_suffix('%').unwrap_or(&s).trim().parse().ok()?;
                (1..=100).contains(&n).then_some(FecMode::Fixed(n))
            }
        }
    }

    /// Inverse of [`FecMode::parse`] (what goes into settings.json and
    /// `stream_info.fec`).
    pub fn as_string(&self) -> String {
        match self {
            FecMode::Off => "off".into(),
            FecMode::Auto => "auto".into(),
            FecMode::Fixed(n) => format!("{n}%"),
        }
    }

    /// Parity overhead as a fraction of data packets, or `None` when FEC is
    /// off. `loss_pct` is the packet loss the viewer last reported;
    /// `boost_pct` is extra parity added after frames could not be rebuilt
    /// (bursty loss needs more than the average loss suggests).
    ///
    /// Auto: delta frames 10 % + 3 × loss + boost (10–60 %), keyframes
    /// 25 % + 5 × loss + 1.5 × boost (25–100 %). Keyframes get more because
    /// losing one costs a keyframe round trip and stalls every frame after it.
    pub fn overhead(&self, loss_pct: f32, boost_pct: f32, keyframe: bool) -> Option<f32> {
        let finite = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        let (loss, boost) = (finite(loss_pct), finite(boost_pct));
        let pct = match *self {
            FecMode::Off => return None,
            FecMode::Auto if keyframe => (25.0 + 5.0 * loss + 1.5 * boost).clamp(25.0, 100.0),
            FecMode::Auto => (10.0 + 3.0 * loss + boost).clamp(10.0, 60.0),
            FecMode::Fixed(n) if keyframe => (n as f32 * 1.5).min(100.0),
            FecMode::Fixed(n) => n as f32,
        };
        Some(pct / 100.0)
    }
}

/// Number of parity packets for `data` data packets in `blocks` blocks: the
/// overhead rounded up, at least one per block (two for keyframes).
pub fn parity_count(data: usize, blocks: usize, overhead: f32, keyframe: bool) -> usize {
    let min_per_block = if keyframe { 2 } else { 1 };
    let want = (data as f32 * overhead.clamp(0.0, 1.0)).ceil() as usize;
    want.max(blocks * min_per_block)
        .min(u16::MAX as usize - data)
}

/// Shards `0..total` are dealt round-robin over `blocks`; this is how many
/// block `b` gets.
pub fn block_len(total: usize, blocks: usize, b: usize) -> usize {
    total / blocks + usize::from(b < total % blocks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecHeader {
    pub keyframe: bool,
    pub frame_id: u32,
    /// `0..data_count` are data shards, `data_count..data_count+parity_count` parity.
    pub index: u16,
    pub data_count: u16,
    pub parity_count: u16,
    pub frame_len: u32,
    pub shard_len: u16,
    pub blocks: u8,
    pub timing: FrameTiming,
}

impl FecHeader {
    fn write(&self, buf: &mut BytesMut) {
        buf.put_u8(FEC_WIRE_VERSION);
        buf.put_u8(if self.keyframe { FLAG_KEYFRAME } else { 0 });
        buf.put_u32(self.frame_id);
        buf.put_u16(self.index);
        buf.put_u16(self.data_count);
        buf.put_u16(self.parity_count);
        buf.put_u32(self.frame_len);
        buf.put_u16(self.shard_len);
        buf.put_u8(self.blocks);
        buf.put_u8(0);
        buf.put_u64(self.timing.capture_us);
        buf.put_u64(self.timing.encoded_us);
        buf.put_u64(self.timing.sent_us);
    }

    /// Parses and validates a version-2 packet. Inconsistent headers (from a
    /// broken or hostile sender) return `None`.
    pub fn parse(d: &[u8]) -> Option<(Self, &[u8])> {
        if d.len() < FEC_HEADER_LEN || d[0] != FEC_WIRE_VERSION {
            return None;
        }
        let u16_at = |o: usize| u16::from_be_bytes([d[o], d[o + 1]]);
        let u32_at = |o: usize| u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_be_bytes(d[o..o + 8].try_into().unwrap());
        let h = Self {
            keyframe: d[1] & FLAG_KEYFRAME != 0,
            frame_id: u32_at(2),
            index: u16_at(6),
            data_count: u16_at(8),
            parity_count: u16_at(10),
            frame_len: u32_at(12),
            shard_len: u16_at(16),
            blocks: d[18],
            timing: FrameTiming {
                capture_us: u64_at(20),
                encoded_us: u64_at(28),
                sent_us: u64_at(36),
            },
        };
        let payload = &d[FEC_HEADER_LEN..];
        if !h.layout_ok() || payload.len() != h.payload_len(h.index as usize) {
            return None;
        }
        Some((h, payload))
    }

    fn layout_ok(&self) -> bool {
        let (k, sl, len) = (
            self.data_count as usize,
            self.shard_len as usize,
            self.frame_len as usize,
        );
        k >= 1
            && sl >= 2
            && sl % 2 == 0
            && self.blocks >= 1
            && self.blocks as usize <= k
            && (self.index as usize) < k + self.parity_count as usize
            && k + self.parity_count as usize <= u16::MAX as usize
            && len <= MAX_FRAME_LEN
            && len <= k * sl
            && (k == 1 || len > (k - 1) * sl)
    }

    /// Bytes carried by packet `index` (data shards are unpadded).
    pub fn payload_len(&self, index: usize) -> usize {
        let sl = self.shard_len as usize;
        if index >= self.data_count as usize {
            return sl;
        }
        sl.min((self.frame_len as usize).saturating_sub(index * sl))
    }

    /// Whether two packets describe the same frame layout.
    pub fn same_layout(&self, o: &Self) -> bool {
        (
            self.data_count,
            self.parity_count,
            self.frame_len,
            self.shard_len,
            self.blocks,
        ) == (
            o.data_count,
            o.parity_count,
            o.frame_len,
            o.shard_len,
            o.blocks,
        )
    }
}

/// Splits one encoded frame into data and parity packets no larger than
/// `max_datagram`. Data packets come first, in order (consecutive ones belong
/// to different blocks), then the parity packets, also interleaved.
pub fn fec_fragment(
    frame_id: u32,
    keyframe: bool,
    timing: FrameTiming,
    payload: &[u8],
    max_datagram: usize,
    overhead: f32,
) -> Vec<Bytes> {
    let max_shard = (max_datagram.saturating_sub(FEC_HEADER_LEN) & !1).max(2);
    let shard_len = if payload.len() < max_shard {
        (payload.len().max(1) + 1) & !1
    } else {
        max_shard
    };
    let k = payload.len().div_ceil(shard_len).max(1);
    assert!(
        k < u16::MAX as usize && payload.len() <= MAX_FRAME_LEN,
        "frame too large to fragment"
    );
    let blocks = k.div_ceil(MAX_BLOCK_DATA).min(MAX_BLOCKS);
    let parity = parity_count(k, blocks, overhead, keyframe);
    let mut header = FecHeader {
        keyframe,
        frame_id,
        index: 0,
        data_count: k as u16,
        parity_count: parity as u16,
        frame_len: payload.len() as u32,
        shard_len: shard_len as u16,
        blocks: blocks as u8,
        timing,
    };
    let shard = |i: usize| -> Cow<'_, [u8]> {
        let start = (i * shard_len).min(payload.len());
        let end = (start + shard_len).min(payload.len());
        let s = &payload[start..end];
        if s.len() == shard_len {
            Cow::Borrowed(s)
        } else {
            let mut v = s.to_vec();
            v.resize(shard_len, 0);
            Cow::Owned(v)
        }
    };
    let packet = |h: &FecHeader, body: &[u8]| {
        let mut buf = BytesMut::with_capacity(FEC_HEADER_LEN + body.len());
        h.write(&mut buf);
        buf.extend_from_slice(body);
        buf.freeze()
    };

    let mut out = Vec::with_capacity(k + parity);
    for i in 0..k {
        header.index = i as u16;
        let len = header.payload_len(i);
        out.push(packet(
            &header,
            &payload[i * shard_len..i * shard_len + len],
        ));
    }
    // recovery[b][pos] is parity shard `b + pos * blocks`.
    let mut recovery: Vec<Vec<Vec<u8>>> = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let m = block_len(parity, blocks, b);
        if m == 0 {
            recovery.push(Vec::new());
            continue;
        }
        let originals: Vec<Cow<'_, [u8]>> = (b..k).step_by(blocks).map(shard).collect();
        recovery
            .push(reed_solomon_simd::encode(originals.len(), m, &originals).unwrap_or_default());
    }
    for j in 0..parity {
        let Some(body) = recovery[j % blocks].get(j / blocks) else {
            continue;
        };
        header.index = (k + j) as u16;
        out.push(packet(&header, body));
    }
    out
}

/// Rebuilds the missing data shards of block `b` from whatever data and
/// parity shards are present. Returns `(data index, unpadded bytes)`.
pub(crate) fn recover_block(
    h: &FecHeader,
    b: usize,
    data: &[Option<Bytes>],
    parity: &[Option<Bytes>],
) -> Option<Vec<(usize, Bytes)>> {
    let (k, p, blocks, sl) = (
        h.data_count as usize,
        h.parity_count as usize,
        h.blocks as usize,
        h.shard_len as usize,
    );
    let (kb, mb) = (block_len(k, blocks, b), block_len(p, blocks, b));
    if mb == 0 {
        return None;
    }
    let originals = (0..kb).filter_map(|pos| {
        data.get(b + pos * blocks)?.as_ref().map(|d| {
            let mut v = d.to_vec();
            v.resize(sl, 0);
            (pos, v)
        })
    });
    let recovery = (0..mb).filter_map(|pos| {
        parity
            .get(b + pos * blocks)?
            .as_ref()
            .map(|r| (pos, r.clone()))
    });
    let restored = reed_solomon_simd::decode(kb, mb, originals, recovery).ok()?;
    Some(
        restored
            .into_iter()
            .map(|(pos, mut v)| {
                let i = b + pos * blocks;
                v.truncate(h.payload_len(i));
                (i, Bytes::from(v))
            })
            .collect(),
    )
}

/// Packet counters the viewer keeps (cumulative). Differences between two
/// snapshots give the rates shown in the HUD and sent in `link_report`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    /// Packets the presenter sent for frames we heard about.
    pub expected: u64,
    pub received: u64,
    pub data_expected: u64,
    pub parity_expected: u64,
    /// Data packets rebuilt from parity.
    pub recovered: u64,
    /// Frames that never completed (skipped over).
    pub unrecoverable_frames: u64,
}

impl LinkStats {
    pub fn since(&self, earlier: &LinkStats) -> LinkStats {
        LinkStats {
            expected: self.expected.saturating_sub(earlier.expected),
            received: self.received.saturating_sub(earlier.received),
            data_expected: self.data_expected.saturating_sub(earlier.data_expected),
            parity_expected: self.parity_expected.saturating_sub(earlier.parity_expected),
            recovered: self.recovered.saturating_sub(earlier.recovered),
            unrecoverable_frames: self
                .unrecoverable_frames
                .saturating_sub(earlier.unrecoverable_frames),
        }
    }

    pub fn loss_pct(&self) -> f32 {
        if self.expected == 0 {
            return 0.0;
        }
        100.0 * self.expected.saturating_sub(self.received) as f32 / self.expected as f32
    }

    pub fn overhead_pct(&self) -> f32 {
        if self.data_expected == 0 {
            return 0.0;
        }
        100.0 * self.parity_expected as f32 / self.data_expected as f32
    }
}

/// What the viewer HUD shows about the link (updated every window).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LinkHud {
    /// Parity packets as a percentage of data packets (0 without FEC).
    pub overhead_pct: f32,
    pub loss_pct: f32,
    pub recovered_per_s: f32,
    /// Frames lost since connecting.
    pub unrecoverable: u64,
}

impl LinkHud {
    /// One HUD line, e.g. `fec +14%  loss 1.2%  rebuilt 35/s  unrecoverable 0`.
    pub fn line(&self) -> String {
        let fec = if self.overhead_pct > 0.0 {
            format!(
                "fec +{:.0}%  rebuilt {:.0}/s",
                self.overhead_pct, self.recovered_per_s
            )
        } else {
            "fec off".to_owned()
        };
        format!(
            "{fec}  loss {:.1}%  unrecoverable frames {}",
            self.loss_pct, self.unrecoverable
        )
    }
}

/// Turns cumulative [`LinkStats`] into ~0.5 s windows for the HUD and the
/// presenter's `link_report`.
#[derive(Debug, Default)]
pub struct LinkWindow {
    last: LinkStats,
    start_us: Option<u64>,
    pub hud: LinkHud,
}

impl LinkWindow {
    pub const PERIOD_US: u64 = 500_000;

    /// Returns the finished window (to report) once [`Self::PERIOD_US`] has
    /// passed since the previous one.
    pub fn tick(&mut self, now: LinkStats, now_us: u64) -> Option<LinkStats> {
        let start = *self.start_us.get_or_insert(now_us);
        let elapsed = now_us.saturating_sub(start);
        if elapsed < Self::PERIOD_US {
            return None;
        }
        let window = now.since(&self.last);
        self.hud = LinkHud {
            overhead_pct: window.overhead_pct(),
            loss_pct: window.loss_pct(),
            recovered_per_s: window.recovered as f32 * 1e6 / elapsed as f32,
            unrecoverable: now.unrecoverable_frames,
        };
        self.last = now;
        self.start_us = Some(now_us);
        Some(window)
    }
}

/// Test helper: drops packets like a lossy link. With `burst <= 1` losses are
/// independent; otherwise a two-state (Gilbert) model drops whole runs of
/// mean length `burst` while keeping the average loss at `loss`.
pub struct LossSim {
    rng: u64,
    p_enter: f64,
    p_leave: f64,
    loss: f64,
    bursty: bool,
    in_burst: bool,
}

impl LossSim {
    pub fn new(loss: f64, burst: f64, seed: u64) -> Self {
        let loss = loss.clamp(0.0, 0.95);
        let bursty = burst > 1.0;
        let p_leave = if bursty { 1.0 / burst } else { 1.0 };
        Self {
            rng: seed | 1,
            p_enter: loss * p_leave / (1.0 - loss),
            p_leave,
            loss,
            bursty,
            in_burst: false,
        }
    }

    fn uniform(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Whether the next packet is lost.
    pub fn drop_next(&mut self) -> bool {
        if self.loss <= 0.0 {
            return false;
        }
        if !self.bursty {
            return self.uniform() < self.loss;
        }
        let u = self.uniform();
        self.in_burst = if self.in_burst {
            u >= self.p_leave
        } else {
            u < self.p_enter
        };
        self.in_burst
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::Reassembler;

    const T: FrameTiming = FrameTiming {
        capture_us: 1,
        encoded_us: 2,
        sent_us: 3,
    };

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn deliver(packets: &[Bytes], keep: impl Fn(usize) -> bool) -> Option<Vec<u8>> {
        let mut r = Reassembler::new(1_000_000);
        let mut out = None;
        for (i, p) in packets.iter().enumerate() {
            if keep(i) {
                if let Some((f, _)) = r.push(p.clone(), 0) {
                    out = Some(f.data);
                }
            }
        }
        out
    }

    #[test]
    fn mode_parse_and_policy() {
        assert_eq!(FecMode::parse("auto"), Some(FecMode::Auto));
        assert_eq!(FecMode::parse("OFF"), Some(FecMode::Off));
        assert_eq!(FecMode::parse("20%"), Some(FecMode::Fixed(20)));
        assert_eq!(FecMode::parse("101"), None);
        assert_eq!(FecMode::parse("fast"), None);
        for m in [FecMode::Off, FecMode::Auto, FecMode::Fixed(35)] {
            assert_eq!(FecMode::parse(&m.as_string()), Some(m));
        }
        assert_eq!(FecMode::Off.overhead(5.0, 0.0, true), None);
        assert_eq!(FecMode::Auto.overhead(0.0, 0.0, false), Some(0.10));
        assert_eq!(FecMode::Auto.overhead(5.0, 0.0, false), Some(0.25));
        assert_eq!(FecMode::Auto.overhead(5.0, 0.0, true), Some(0.50));
        assert_eq!(FecMode::Auto.overhead(5.0, 10.0, false), Some(0.35));
        assert_eq!(FecMode::Auto.overhead(30.0, 30.0, false), Some(0.60));
        assert_eq!(
            FecMode::Auto.overhead(f32::NAN, f32::INFINITY, true),
            Some(0.25)
        );
        assert_eq!(FecMode::Fixed(80).overhead(0.0, 30.0, true), Some(1.0));
        assert_eq!(
            FecMode::Fixed(20).overhead(0.0, 30.0, false),
            Some(0.2),
            "boost is auto only"
        );
        assert_eq!(parity_count(1, 1, 0.1, false), 1);
        assert_eq!(parity_count(300, 5, 0.25, true), 75);
        assert_eq!(parity_count(300, 5, 0.0, true), 10);
    }

    #[test]
    fn header_roundtrip_and_validation() {
        let pkts = fec_fragment(9, true, T, &payload(5000), 1200, 0.2);
        let (h, body) = FecHeader::parse(&pkts[0]).unwrap();
        assert_eq!(
            (h.frame_id, h.keyframe, h.index, h.frame_len, h.timing),
            (9, true, 0, 5000, T)
        );
        assert_eq!(body.len(), h.shard_len as usize);
        assert!(pkts.iter().all(|p| p.len() <= 1200));
        let mut bad = pkts[0].to_vec();
        bad[18] = 0; // zero blocks
        assert!(FecHeader::parse(&bad).is_none());
        let mut bad = pkts[0].to_vec();
        bad[12..16].copy_from_slice(&u32::MAX.to_be_bytes()); // frame_len too big
        assert!(FecHeader::parse(&bad).is_none());
        assert!(
            FecHeader::parse(&pkts[0][..FEC_HEADER_LEN + 3]).is_none(),
            "truncated"
        );
        assert!(FecHeader::parse(&[2u8; 10]).is_none());
    }

    #[test]
    fn recovers_any_k_of_n() {
        for (len, overhead) in [
            (0usize, 0.1f32),
            (1, 0.1),
            (1199, 0.2),
            (10_000, 0.2),
            (300_000, 0.25),
        ] {
            let data = payload(len);
            let pkts = fec_fragment(1, len > 100_000, T, &data, 1200, overhead);
            let (h, _) = FecHeader::parse(&pkts[0]).unwrap();
            let (k, p, blocks) = (
                h.data_count as usize,
                h.parity_count as usize,
                h.blocks as usize,
            );
            assert_eq!(pkts.len(), k + p);
            // Lose as many data packets in each block as it has parity.
            let lost: std::collections::HashSet<usize> = (0..blocks)
                .flat_map(|b| (b..k).step_by(blocks).take(block_len(p, blocks, b)))
                .collect();
            assert_eq!(
                deliver(&pkts, |i| !lost.contains(&i)).as_deref(),
                Some(&data[..]),
                "len {len}"
            );
            // Parity alone plus the remaining data also works when data comes last.
            let mut rev = pkts.clone();
            rev.reverse();
            assert_eq!(
                deliver(&rev, |i| !lost.contains(&(k + p - 1 - i))).as_deref(),
                Some(&data[..])
            );
        }
    }

    #[test]
    fn interleaving_covers_a_burst() {
        // 300 data packets in 5 blocks, 2 parity each at minimum: a burst of
        // 10 consecutive data packets costs each block exactly 2.
        let data = payload(300 * 1156);
        let pkts = fec_fragment(1, true, T, &data, 1200, 0.0);
        assert_eq!(
            deliver(&pkts, |i| !(40..50).contains(&i)).as_deref(),
            Some(&data[..])
        );
        assert_eq!(
            deliver(&pkts, |i| !(40..51).contains(&i)),
            None,
            "one too many"
        );
    }

    #[test]
    fn hostile_mixed_layouts_do_not_panic() {
        let a = fec_fragment(3, false, T, &payload(4000), 1200, 0.5);
        let b = fec_fragment(3, false, T, &payload(9000), 1000, 0.5);
        let mut r = Reassembler::new(1_000_000);
        for p in a.iter().take(2).chain(b.iter()).chain(a.iter().skip(2)) {
            let _ = r.push(p.clone(), 0);
        }
        let mut junk = a[0].to_vec();
        junk[FEC_HEADER_LEN..].fill(0xff);
        let _ = r.push(Bytes::from(junk), 0);
    }

    #[test]
    fn link_window_reports_every_period() {
        let mut w = LinkWindow::default();
        let s = |e, r, rec| LinkStats {
            expected: e,
            received: r,
            data_expected: e * 4 / 5,
            parity_expected: e / 5,
            recovered: rec,
            unrecoverable_frames: 1,
        };
        assert!(w.tick(s(0, 0, 0), 1_000).is_none());
        assert!(w.tick(s(50, 50, 0), 300_000).is_none());
        let win = w.tick(s(1000, 980, 10), 501_000).unwrap();
        assert_eq!((win.expected, win.received, win.recovered), (1000, 980, 10));
        assert!((w.hud.loss_pct - 2.0).abs() < 1e-3);
        assert!((w.hud.overhead_pct - 25.0).abs() < 1e-3);
        assert!((w.hud.recovered_per_s - 20.0).abs() < 1e-3);
        assert_eq!(w.hud.unrecoverable, 1);
        assert!(w.tick(s(1100, 1080, 10), 600_000).is_none());
    }

    #[test]
    fn loss_sim_matches_target() {
        for (loss, burst) in [(0.02, 1.0), (0.05, 1.0), (0.05, 4.0)] {
            let mut sim = LossSim::new(loss, burst, 12345);
            let (mut dropped, mut runs, mut prev) = (0usize, 0usize, false);
            let n = 400_000;
            for _ in 0..n {
                let d = sim.drop_next();
                dropped += usize::from(d);
                runs += usize::from(d && !prev);
                prev = d;
            }
            let rate = dropped as f64 / n as f64;
            assert!((rate - loss).abs() < loss * 0.1, "{loss} {burst}: {rate}");
            let mean_run = dropped as f64 / runs as f64;
            assert!(
                (mean_run - burst.max(1.0 / (1.0 - loss))).abs() < 0.5,
                "run {mean_run}"
            );
        }
        assert!(!LossSim::new(0.0, 4.0, 1).drop_next());
    }
}

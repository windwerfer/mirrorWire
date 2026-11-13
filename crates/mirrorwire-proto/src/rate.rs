//! Adaptive bitrate for the presenter: an AIMD controller driven by the
//! viewer's `link_report` (packet loss) and the QUIC RTT.
//!
//! The controller owns the **total budget** on the wire (video + FEC
//! parity). [`encoder_kbps`] turns it into the encoder target by leaving
//! room for the parity the FEC setting will add, so FEC never pushes the
//! stream above the budget (on congested Wi-Fi, extra parity only made the
//! loss worse).
//!
//! * Congestion: loss clearly above what the link loses anyway, sustained
//!   for two reports (or one heavy report), a queue building up (RTT well
//!   above the lowest seen), or the presenter dropping frames for backlog.
//!   Then the budget is cut multiplicatively (×0.85, ×0.75 heavy, ×0.6
//!   severe), at most once per second, never below the floor.
//! * Clean (little loss, no queue): the budget climbs additively by 4 % of
//!   the ceiling per second, starting 4 s after the last cut.
//! * Loss that does not go away when the rate is cut is learned as the
//!   link's random loss (Wi-Fi retries, interference); FEC handles it, and
//!   it does not keep pushing the rate down.
//!
//! Time comes in as milliseconds so the logic is testable without a clock.

/// Lowest budget the controller goes to (kbit/s), or the ceiling if lower.
pub const FLOOR_KBPS: u32 = 3_000;
/// Smallest additive step per second (kbit/s).
const MIN_STEP_KBPS: f32 = 250.0;
const MIN_DECREASE_GAP_MS: u64 = 1_000;
const INCREASE_AFTER_CUT_MS: u64 = 4_000;
/// How long after a cut the loss is watched to see whether it followed the rate.
const PROBE_MS: u64 = 3_000;
const MAX_RANDOM_LOSS: f32 = 15.0;

/// One feedback sample. `loss_pct` is `None` when only the presenter's own
/// view is known (send window without a viewer report).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub loss_pct: Option<f32>,
    pub rtt_ms: Option<f32>,
    /// Delta frames the presenter dropped because the send queue was full.
    pub backlog_drops: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveBitrate {
    ceiling_kbps: u32,
    budget_kbps: f32,
    min_rtt_ms: Option<f32>,
    last_sample_ms: Option<u64>,
    last_cut_ms: Option<u64>,
    lossy_streak: u32,
    congested: bool,
    /// Loss the link shows regardless of the rate (learned, decays).
    random_loss_pct: f32,
    /// Loss when the last cut happened, and the samples since (probe).
    probe: Option<Probe>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Probe {
    loss_before: f32,
    until_ms: u64,
    sum: f32,
    n: u32,
}

impl AdaptiveBitrate {
    pub fn new(ceiling_kbps: u32) -> Self {
        let ceiling_kbps = ceiling_kbps.max(1);
        Self {
            ceiling_kbps,
            budget_kbps: ceiling_kbps as f32,
            min_rtt_ms: None,
            last_sample_ms: None,
            last_cut_ms: None,
            lossy_streak: 0,
            congested: false,
            random_loss_pct: 0.0,
            probe: None,
        }
    }

    pub fn ceiling_kbps(&self) -> u32 {
        self.ceiling_kbps
    }

    pub fn budget_kbps(&self) -> u32 {
        self.budget_kbps.round() as u32
    }

    pub fn congested(&self) -> bool {
        self.congested
    }

    /// Loss currently treated as the link's background loss (percent).
    pub fn random_loss_pct(&self) -> f32 {
        self.random_loss_pct
    }

    pub fn floor_kbps(&self) -> u32 {
        FLOOR_KBPS.min(self.ceiling_kbps)
    }

    /// The user's bitrate setting. Lowering it caps the budget at once;
    /// raising it lets the budget climb there.
    pub fn set_ceiling(&mut self, ceiling_kbps: u32) {
        let ceiling_kbps = ceiling_kbps.max(1);
        if ceiling_kbps == self.ceiling_kbps {
            return;
        }
        self.ceiling_kbps = ceiling_kbps;
        self.budget_kbps = self
            .budget_kbps
            .min(ceiling_kbps as f32)
            .max(self.floor_kbps() as f32);
    }

    /// A new viewer: start again from the ceiling with nothing learned.
    pub fn reset(&mut self) {
        *self = Self::new(self.ceiling_kbps);
    }

    /// Feed one sample; returns the new budget (kbit/s).
    pub fn on_sample(&mut self, s: Sample, now_ms: u64) -> u32 {
        let dt_ms = self
            .last_sample_ms
            .map(|t| now_ms.saturating_sub(t))
            .unwrap_or(500)
            .min(1_000);
        self.last_sample_ms = Some(now_ms);

        let queue_growing = match s.rtt_ms.filter(|r| r.is_finite() && *r > 0.0) {
            Some(rtt) => {
                let min = self.min_rtt_ms.get_or_insert(rtt);
                // Follow route changes slowly upwards, instantly downwards.
                *min = if rtt < *min {
                    rtt
                } else {
                    *min + 0.005 * (rtt - *min)
                };
                rtt > *min * 1.5 + 25.0
            }
            None => false,
        };

        let loss = s
            .loss_pct
            .filter(|l| l.is_finite())
            .map(|l| l.clamp(0.0, 100.0));
        if let Some(l) = loss {
            self.learn_random_loss(l, now_ms);
        }
        let excess = loss
            .map(|l| (l - self.random_loss_pct).max(0.0))
            .unwrap_or(0.0);
        let raw = loss.unwrap_or(0.0);

        let lossy = excess >= 3.0 || queue_growing;
        self.lossy_streak = if lossy { self.lossy_streak + 1 } else { 0 };
        let heavy = excess >= 10.0;
        self.congested = heavy || s.backlog_drops > 0 || self.lossy_streak >= 2;

        let floor = self.floor_kbps() as f32;
        let ceiling = self.ceiling_kbps as f32;
        if self.congested {
            let may_cut = self
                .last_cut_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= MIN_DECREASE_GAP_MS);
            if may_cut {
                let factor = if excess >= 20.0 {
                    0.6
                } else if heavy {
                    0.75
                } else {
                    0.85
                };
                self.budget_kbps = (self.budget_kbps * factor).max(floor);
                self.last_cut_ms = Some(now_ms);
                if loss.is_some() && self.probe.is_none() {
                    self.probe = Some(Probe {
                        loss_before: raw,
                        until_ms: now_ms + PROBE_MS,
                        sum: 0.0,
                        n: 0,
                    });
                }
            }
        } else if excess < 1.5 && !queue_growing {
            let rested = self
                .last_cut_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= INCREASE_AFTER_CUT_MS);
            if rested {
                let step = (ceiling * 0.04).max(MIN_STEP_KBPS) * dt_ms as f32 / 1000.0;
                self.budget_kbps = (self.budget_kbps + step).min(ceiling);
            }
        }
        self.budget_kbps = self.budget_kbps.clamp(floor, ceiling);
        self.budget_kbps()
    }

    /// After a cut, watch the loss for a few seconds. If it stays (≥ 70 % of
    /// what it was), it did not come from our rate: remember it as the
    /// link's background loss. The estimate decays so it is re-learned.
    fn learn_random_loss(&mut self, loss: f32, now_ms: u64) {
        self.random_loss_pct *= 0.995;
        if let Some(p) = &mut self.probe {
            if now_ms < p.until_ms {
                p.sum += loss;
                p.n += 1;
                return;
            }
            let p = self.probe.take().expect("probe");
            if p.n > 0 {
                let after = p.sum / p.n as f32;
                if after >= 0.7 * p.loss_before {
                    self.random_loss_pct =
                        self.random_loss_pct.max(after * 0.9).min(MAX_RANDOM_LOSS);
                }
            }
        }
    }
}

/// Encoder target that keeps video + parity within `budget_kbps`, given the
/// FEC overhead (parity / data, e.g. 0.25) applied to delta frames.
pub fn encoder_kbps(budget_kbps: u32, fec_overhead: Option<f32>) -> u32 {
    let o = fec_overhead
        .filter(|o| o.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    ((budget_kbps as f32) / (1.0 + o)).round().max(100.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loss(l: f32) -> Sample {
        Sample {
            loss_pct: Some(l),
            rtt_ms: Some(5.0),
            backlog_drops: 0,
        }
    }

    /// Feed `n` reports 500 ms apart starting at `*t`.
    fn feed(c: &mut AdaptiveBitrate, s: Sample, n: usize, t: &mut u64) -> u32 {
        let mut b = c.budget_kbps();
        for _ in 0..n {
            *t += 500;
            b = c.on_sample(s, *t);
        }
        b
    }

    #[test]
    fn clean_link_stays_at_the_ceiling() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        assert_eq!(feed(&mut c, loss(0.0), 20, &mut t), 40_000);
        assert!(!c.congested());
    }

    #[test]
    fn sustained_loss_cuts_multiplicatively_once_per_second() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        // One moderate report is not yet congestion.
        assert_eq!(feed(&mut c, loss(5.0), 1, &mut t), 40_000);
        assert_eq!(feed(&mut c, loss(5.0), 1, &mut t), 34_000);
        assert!(c.congested());
        // 500 ms later: no second cut yet.
        assert_eq!(feed(&mut c, loss(5.0), 1, &mut t), 34_000);
        assert_eq!(feed(&mut c, loss(5.0), 1, &mut t), 28_900);
        // Heavy loss cuts at once and harder.
        let mut h = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        assert_eq!(feed(&mut h, loss(12.0), 1, &mut t), 30_000);
        let mut severe = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        assert_eq!(feed(&mut severe, loss(24.0), 1, &mut t), 24_000);
    }

    #[test]
    fn floor_and_ceiling_hold() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        assert_eq!(feed(&mut c, loss(30.0), 40, &mut t), FLOOR_KBPS);
        // A ceiling below the floor wins.
        let mut low = AdaptiveBitrate::new(2_000);
        assert_eq!(feed(&mut low, loss(30.0), 10, &mut t), 2_000);
        c.set_ceiling(2_500);
        assert_eq!(c.budget_kbps(), 2_500);
        c.set_ceiling(10_000);
        assert_eq!(
            c.budget_kbps(),
            FLOOR_KBPS,
            "raising the setting climbs from the floor, no jump"
        );
    }

    #[test]
    fn recovers_additively_after_a_pause() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        let cut = feed(&mut c, loss(12.0), 1, &mut t);
        assert_eq!(cut, 30_000);
        // Clean, but within 4 s of the cut: hold.
        assert_eq!(feed(&mut c, loss(0.0), 6, &mut t), 30_000);
        // From 4 s on: +4 % of the ceiling (1.6 Mbit) per second, i.e. 800 per report.
        let b = feed(&mut c, loss(0.0), 2, &mut t);
        assert_eq!(b, 30_800);
        assert_eq!(feed(&mut c, loss(0.0), 2, &mut t), 32_400);
        assert_eq!(feed(&mut c, loss(0.0), 40, &mut t), 40_000);
    }

    #[test]
    fn queue_growth_and_backlog_count_as_congestion() {
        let mut c = AdaptiveBitrate::new(20_000);
        let mut t = 0;
        let rtt = |ms| Sample {
            loss_pct: Some(0.0),
            rtt_ms: Some(ms),
            backlog_drops: 0,
        };
        feed(&mut c, rtt(4.0), 4, &mut t);
        assert_eq!(c.budget_kbps(), 20_000);
        assert_eq!(feed(&mut c, rtt(80.0), 2, &mut t), 17_000);
        let mut b = AdaptiveBitrate::new(20_000);
        let drop = Sample {
            loss_pct: None,
            rtt_ms: None,
            backlog_drops: 2,
        };
        assert_eq!(feed(&mut b, drop, 1, &mut t), 17_000);
        // Garbage input is ignored, not a panic.
        let junk = Sample {
            loss_pct: Some(f32::NAN),
            rtt_ms: Some(f32::INFINITY),
            backlog_drops: 0,
        };
        feed(&mut b, junk, 3, &mut t);
    }

    #[test]
    fn loss_that_ignores_the_rate_is_learned_as_random() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        // 6 % loss that never changes, whatever the rate (bad Wi-Fi, no congestion).
        let first = feed(&mut c, loss(6.0), 2, &mut t);
        assert_eq!(first, 34_000);
        feed(&mut c, loss(6.0), 30, &mut t);
        assert!(c.random_loss_pct() > 4.0, "{}", c.random_loss_pct());
        let settled = c.budget_kbps();
        assert!(
            settled > FLOOR_KBPS * 3,
            "kept cutting on random loss: {settled}"
        );
        // And it climbs back while the loss stays at that level.
        let later = feed(&mut c, loss(6.0), 40, &mut t);
        assert!(later > settled, "{settled} -> {later}");
        // Real congestion on top is still seen.
        assert!(feed(&mut c, loss(20.0), 2, &mut t) < later);
    }

    #[test]
    fn reset_and_encoder_share() {
        let mut c = AdaptiveBitrate::new(40_000);
        let mut t = 0;
        feed(&mut c, loss(30.0), 10, &mut t);
        c.reset();
        assert_eq!(c.budget_kbps(), 40_000);
        assert_eq!(encoder_kbps(20_000, None), 20_000);
        assert_eq!(encoder_kbps(20_000, Some(0.25)), 16_000);
        assert_eq!(encoder_kbps(20_000, Some(f32::NAN)), 20_000);
        assert_eq!(
            encoder_kbps(20_000, Some(5.0)),
            10_000,
            "overhead capped at 100 %"
        );
        assert_eq!(encoder_kbps(0, None), 100);
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Estimates `presenter_clock - viewer_clock` from ping/pong samples, keeping the
/// sample with the smallest round trip as the most trustworthy one.
#[derive(Debug, Default, Clone)]
pub struct ClockSync {
    best_rtt_us: Option<u64>,
    offset_us: i64,
    samples: u32,
}

impl ClockSync {
    pub fn add_sample(&mut self, t_send_us: u64, t_presenter_us: u64, t_recv_us: u64) {
        let rtt = t_recv_us.saturating_sub(t_send_us);
        let midpoint = (t_send_us + t_recv_us) / 2;
        let offset = t_presenter_us as i64 - midpoint as i64;
        self.samples += 1;
        if self.best_rtt_us.is_none_or(|best| rtt <= best) {
            self.best_rtt_us = Some(rtt);
            self.offset_us = offset;
        }
    }

    pub fn offset_us(&self) -> i64 {
        self.offset_us
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.best_rtt_us
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    pub fn to_viewer_clock(&self, presenter_us: u64) -> u64 {
        (presenter_us as i64 - self.offset_us).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_from_symmetric_rtt() {
        let mut c = ClockSync::default();
        c.add_sample(1_000, 6_500, 2_000);
        assert_eq!(c.offset_us(), 5_000);
        c.add_sample(10_000, 20_000, 14_000);
        assert_eq!(c.offset_us(), 5_000);
        assert_eq!(c.to_viewer_clock(6_500), 1_500);
    }
}

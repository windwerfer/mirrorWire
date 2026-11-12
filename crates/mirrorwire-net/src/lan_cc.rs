//! A large-window congestion controller for a trusted LAN.
//!
//! Quinn's default BBR/Cubic pacing tracks the application's average send
//! rate, so a 40 Mbit/s encode is spread across the whole frame interval
//! (~40 ms of "reassembly" for a 1080p frame). On a LAN we want to burst
//! the fragments as fast as the path allows, then idle until the next frame.

use quinn::congestion::{Controller, ControllerFactory, ControllerMetrics};
use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

/// 4 MiB, enough for several 1080p I-frames on the wire at once.
pub const LAN_WINDOW: u64 = 4 * 1024 * 1024;
/// 8 Gbit/s pacing — above a typical 1 Gbit LAN, so a frame is sent as a burst.
const LAN_PACING_BITS_PER_SEC: u64 = 8_000_000_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct LanBurstConfig;

pub fn factory() -> Arc<LanBurstConfig> {
    Arc::new(LanBurstConfig)
}

#[derive(Clone)]
struct LanBurst {
    window: u64,
}

impl Controller for LanBurst {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        if is_persistent_congestion {
            self.window = (self.window / 2).max(256 * 1024);
        }
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.window
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = ControllerMetrics::default();
        metrics.congestion_window = self.window;
        metrics.pacing_rate = Some(LAN_PACING_BITS_PER_SEC);
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        LAN_WINDOW
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl ControllerFactory for LanBurstConfig {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LanBurst { window: LAN_WINDOW })
    }
}

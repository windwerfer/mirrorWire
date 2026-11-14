//! Where the viewer pipeline stands, for the HUD: frames received, sent to
//! the decoder, pictures out of the decoder, pictures shown. When one stage
//! keeps getting frames but passes nothing on, [`PipelineHud::stall`] names
//! that stage, so a photo of the screen tells which step is stuck.
//!
//! Pure logic, host-testable (no Android APIs).

/// Running totals since connect. Each stage only ever counts up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Complete frames out of the reassembler.
    pub received: u64,
    /// Frames thrown away while waiting for a keyframe.
    pub skipped_waiting_key: u64,
    /// Times the decode queue overflowed and was flushed.
    pub backlog_flushes: u64,
    /// Keyframe requests sent to the presenter.
    pub key_requests: u64,
    /// Access units handed to MediaCodec.
    pub queued: u64,
    /// Access units MediaCodec had no input buffer for.
    pub input_full: u64,
    /// Output buffers MediaCodec released to the image reader.
    pub rendered: u64,
    /// Pictures decoded but not shown because a newer frame was already waiting.
    pub superseded: u64,
    /// Pictures converted and handed to the UI.
    pub decoded: u64,
    /// Decode errors.
    pub errors: u64,
    /// Pictures the UI uploaded to its texture.
    pub presented: u64,
}

impl Counters {
    fn delta(&self, old: &Counters) -> Counters {
        Counters {
            received: self.received.saturating_sub(old.received),
            skipped_waiting_key: self
                .skipped_waiting_key
                .saturating_sub(old.skipped_waiting_key),
            backlog_flushes: self.backlog_flushes.saturating_sub(old.backlog_flushes),
            key_requests: self.key_requests.saturating_sub(old.key_requests),
            queued: self.queued.saturating_sub(old.queued),
            input_full: self.input_full.saturating_sub(old.input_full),
            rendered: self.rendered.saturating_sub(old.rendered),
            superseded: self.superseded.saturating_sub(old.superseded),
            decoded: self.decoded.saturating_sub(old.decoded),
            errors: self.errors.saturating_sub(old.errors),
            presented: self.presented.saturating_sub(old.presented),
        }
    }
}

/// Rates over the last window, plus how long a stage has been stuck.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineHud {
    pub received_fps: f64,
    pub decoded_fps: f64,
    pub presented_fps: f64,
    /// Frames per second dropped while waiting for a keyframe.
    pub skipped_fps: f64,
    pub key_requests: u64,
    pub backlog_flushes: u64,
    pub input_full: u64,
    pub errors: u64,
    /// Pictures skipped (decoded, not shown) per second because the decoder ran late.
    pub superseded_fps: f64,
    /// Seconds frames have been arriving while no picture came out of the decoder.
    pub no_picture_s: f64,
    /// Seconds pictures have been decoded while none were shown.
    pub not_shown_s: f64,
    /// Seconds every frame has been skipped while waiting for a keyframe.
    pub waiting_key_s: f64,
}

/// A stall is reported after this long, so one slow keyframe does not flash a warning.
pub const STALL_AFTER_S: f64 = 2.0;

impl PipelineHud {
    /// `decoded 13.6 fps  shown 13.6 fps` plus the counters that explain drops.
    pub fn line(&self) -> String {
        let mut s = format!(
            "in {:.1}  decoded {:.1}  shown {:.1} fps",
            self.received_fps, self.decoded_fps, self.presented_fps
        );
        if self.superseded_fps > 0.0 {
            s.push_str(&format!("  late, not shown {:.0}/s", self.superseded_fps));
        }
        if self.skipped_fps > 0.0 {
            s.push_str(&format!("  waiting key {:.0}/s", self.skipped_fps));
        }
        if self.key_requests > 0
            || self.backlog_flushes > 0
            || self.input_full > 0
            || self.errors > 0
        {
            s.push_str(&format!(
                "\nkey req {}  flushes {}  input full {}  errors {}",
                self.key_requests, self.backlog_flushes, self.input_full, self.errors
            ));
        }
        s
    }

    /// A human-readable warning when the picture is stuck, naming the stage.
    pub fn stall(&self) -> Option<String> {
        if self.waiting_key_s >= STALL_AFTER_S {
            Some(format!(
                "Picture frozen: every frame is dropped while waiting for a keyframe ({:.0} s, {} requests sent). The presenter may not answer keyframe requests.",
                self.waiting_key_s, self.key_requests
            ))
        } else if self.no_picture_s >= STALL_AFTER_S {
            Some(format!(
                "Picture frozen: frames arrive, but the decoder has returned no picture for {:.0} s.",
                self.no_picture_s
            ))
        } else if self.not_shown_s >= STALL_AFTER_S {
            Some(format!(
                "Picture frozen: frames decode, but none were shown for {:.0} s.",
                self.not_shown_s
            ))
        } else {
            None
        }
    }
}

/// Turns counter snapshots into [`PipelineHud`] rates and stall timers.
#[derive(Debug, Default)]
pub struct PipelineWindow {
    last: Counters,
    last_us: Option<u64>,
    hud: PipelineHud,
}

impl PipelineWindow {
    /// Feed the current totals; returns the updated HUD when at least
    /// `period_us` passed since the last update.
    pub fn tick(&mut self, now: Counters, now_us: u64, period_us: u64) -> Option<&PipelineHud> {
        let Some(last_us) = self.last_us else {
            self.last_us = Some(now_us);
            self.last = now;
            return None;
        };
        let elapsed_us = now_us.saturating_sub(last_us);
        if elapsed_us < period_us.max(1) {
            return None;
        }
        let secs = elapsed_us as f64 / 1e6;
        let d = now.delta(&self.last);
        let h = &mut self.hud;
        h.received_fps = d.received as f64 / secs;
        h.decoded_fps = d.decoded as f64 / secs;
        h.presented_fps = d.presented as f64 / secs;
        h.skipped_fps = d.skipped_waiting_key as f64 / secs;
        h.superseded_fps = d.superseded as f64 / secs;
        h.key_requests = now.key_requests;
        h.backlog_flushes = now.backlog_flushes;
        h.input_full = now.input_full;
        h.errors = now.errors;
        let grow = |t: &mut f64, stuck: bool| *t = if stuck { *t + secs } else { 0.0 };
        grow(
            &mut h.waiting_key_s,
            d.received > 0 && d.skipped_waiting_key >= d.received,
        );
        grow(
            &mut h.no_picture_s,
            d.received > 0
                && d.decoded == 0
                && d.superseded == 0
                && d.skipped_waiting_key < d.received,
        );
        grow(&mut h.not_shown_s, d.decoded > 0 && d.presented == 0);
        self.last = now;
        self.last_us = Some(now_us);
        Some(&self.hud)
    }

    pub fn hud(&self) -> &PipelineHud {
        &self.hud
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    fn run(steps: &[Counters]) -> PipelineHud {
        let mut w = PipelineWindow::default();
        for (i, c) in steps.iter().enumerate() {
            w.tick(*c, i as u64 * S, S);
        }
        w.hud().clone()
    }

    fn c(received: u64, skipped: u64, decoded: u64, presented: u64) -> Counters {
        Counters {
            received,
            skipped_waiting_key: skipped,
            decoded,
            presented,
            ..Default::default()
        }
    }

    #[test]
    fn healthy_stream_has_no_stall_and_rates_match() {
        let h = run(&[
            c(0, 0, 0, 0),
            c(30, 0, 30, 30),
            c(60, 0, 60, 60),
            c(90, 0, 90, 90),
        ]);
        assert_eq!(h.stall(), None);
        assert!((h.decoded_fps - 30.0).abs() < 1e-9);
        assert!((h.presented_fps - 30.0).abs() < 1e-9);
        assert!(
            h.line()
                .starts_with("in 30.0  decoded 30.0  shown 30.0 fps"),
            "{}",
            h.line()
        );
    }

    #[test]
    fn first_picture_then_decoder_silent_is_reported() {
        // The owner's report: one picture, then frames keep arriving but nothing decodes.
        let h = run(&[
            c(1, 0, 1, 1),
            c(15, 0, 1, 1),
            c(29, 0, 1, 1),
            c(43, 0, 1, 1),
        ]);
        let msg = h.stall().expect("stall");
        assert!(msg.contains("decoder has returned no picture"), "{msg}");
    }

    #[test]
    fn late_pictures_skipped_by_a_working_decoder_are_not_a_stall() {
        let late = |n: u64| Counters {
            received: n * 30,
            superseded: n * 30,
            ..Default::default()
        };
        let h = run(&[late(0), late(1), late(2), late(3)]);
        assert_eq!(h.stall(), None);
        assert!(h.line().contains("late, not shown 30/s"), "{}", h.line());
    }

    #[test]
    fn decoded_but_not_shown_is_reported() {
        let h = run(&[
            c(0, 0, 0, 0),
            c(30, 0, 30, 1),
            c(60, 0, 60, 1),
            c(90, 0, 90, 1),
        ]);
        assert!(h.stall().expect("stall").contains("none were shown"));
    }

    #[test]
    fn endless_keyframe_wait_is_reported() {
        let h = run(&[
            c(0, 0, 0, 0),
            c(10, 10, 0, 0),
            c(20, 20, 0, 0),
            c(30, 30, 0, 0),
        ]);
        assert!(h.stall().expect("stall").contains("waiting for a keyframe"));
    }

    #[test]
    fn static_screen_is_not_a_stall() {
        // No frames arrive at all: nothing to show, nothing stuck.
        let h = run(&[c(5, 0, 5, 5), c(5, 0, 5, 5), c(5, 0, 5, 5), c(5, 0, 5, 5)]);
        assert_eq!(h.stall(), None);
    }

    #[test]
    fn one_slow_second_recovers() {
        let h = run(&[
            c(0, 0, 0, 0),
            c(10, 0, 0, 0),
            c(20, 0, 10, 10),
            c(30, 0, 20, 20),
        ]);
        assert_eq!(h.stall(), None);
        assert_eq!(h.no_picture_s, 0.0);
    }
}

use super::FrameSource;
use crate::frame::{PixelFormat, RawFrame};
use crate::pattern::{stamp_marker, MARKER_BLOCK};
use anyhow::Result;
use mirrorwire_proto::clock::now_us;
use std::time::{Duration, Instant};

/// Moving diagonal gradient with a bouncing box and a frame-number marker,
/// paced to a fixed frame rate.
pub struct TestPattern {
    width: u32,
    height: u32,
    fps: u32,
    seq: u64,
    start: Instant,
    gradient: Vec<u8>,
    spare: Option<Vec<u8>>,
    limit: Option<u64>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32, limit: Option<u64>) -> Self {
        let period = width as usize;
        let mut gradient = Vec::with_capacity(period * 2 * 4);
        for i in 0..period * 2 {
            let t = (i % period) as f32 / period as f32 * std::f32::consts::TAU;
            let r = (127.5 + 127.5 * t.sin()) as u8;
            let g = (127.5 + 127.5 * (t + 2.094).sin()) as u8;
            let b = (127.5 + 127.5 * (t + 4.188).sin()) as u8;
            gradient.extend_from_slice(&[b, g, r, 255]);
        }
        Self {
            width,
            height,
            fps: fps.max(1),
            seq: 0,
            start: Instant::now(),
            gradient,
            spare: None,
            limit,
        }
    }

    fn render(&self, data: &mut [u8], seq: u64) {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let shift = (seq as usize * 6) % w;
        for y in 0..h {
            let off = (shift + y / 2) % w * 4;
            data[y * stride..(y + 1) * stride].copy_from_slice(&self.gradient[off..off + stride]);
        }

        let size = (h / 5).max(8);
        let span_x = (w - size).max(1);
        let span_y = (h - size - MARKER_BLOCK).max(1);
        let bounce = |pos: usize, span: usize| {
            let p = pos % (2 * span);
            if p < span { p } else { 2 * span - p }
        };
        let bx = bounce(seq as usize * 9, span_x);
        let by = MARKER_BLOCK + bounce(seq as usize * 5, span_y);
        for y in by..(by + size).min(h) {
            data[y * stride + bx * 4..y * stride + (bx + size).min(w) * 4].fill(255);
        }

        stamp_marker(data, stride, w, h, seq as u32);
    }
}

impl FrameSource for TestPattern {
    fn next_frame(&mut self) -> Result<Option<RawFrame>> {
        if self.limit.is_some_and(|l| self.seq >= l) {
            return Ok(None);
        }
        let due = self.start + Duration::from_secs_f64(self.seq as f64 / self.fps as f64);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let capture_us = now_us();
        let len = self.width as usize * self.height as usize * 4;
        let mut data = self.spare.take().filter(|b| b.len() == len).unwrap_or_else(|| vec![0; len]);
        self.render(&mut data, self.seq);
        let frame = RawFrame {
            seq: self.seq,
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            format: PixelFormat::Bgra,
            data,
            capture_us,
        };
        self.seq += 1;
        Ok(Some(frame))
    }

    fn recycle(&mut self, buffer: Vec<u8>) {
        self.spare = Some(buffer);
    }

    fn describe(&self) -> String {
        format!("test pattern {}x{}@{}", self.width, self.height, self.fps)
    }
}

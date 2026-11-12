use crate::ViewArgs;
use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use mirrorwire_media::decode::{luma, Decoder, HwDecode, RgbaConverter, YuvImage};
use mirrorwire_media::pattern::read_marker;
use mirrorwire_net::identity::Identity;
use mirrorwire_net::pairing::ConnectInfo;
use mirrorwire_net::diag::{ConnectStepId, ConnectTrace, HANDSHAKE_TIMEOUT};
use mirrorwire_net::transport::{ms, ViewerSession};
use mirrorwire_proto::clock::{now_us, ClockSync};
use mirrorwire_proto::control::StreamInfo;
use mirrorwire_proto::fragment::{AssembledFrame, Reassembler};
use mirrorwire_proto::stats::StageStats;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const STAGES: &[&str] = &[
    "capture_to_encoded",
    "encoded_to_sent",
    "network",
    "reassembly",
    "decode_wait",
    "decode",
    "convert",
    "capture_to_decoded",
];

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub struct DisplayFrame {
    pub width: u32,
    pub height: u32,
    pub picture: Picture,
    pub capture_us_viewer: u64,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub enum Picture {
    Rgba(Vec<u8>),
    Yuv(YuvImage),
}

/// The viewer window should leave the connecting panel once the session is
/// up or the first frame has been decoded — not only after a texture exists
/// (the texture was previously created only in the video view, so the UI
/// could stay on "connecting…" forever while the stream ran).
#[cfg_attr(not(any(test, feature = "gui")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerUi {
    Connecting,
    Video,
}

#[cfg_attr(not(any(test, feature = "gui")), allow(dead_code))]
pub fn viewer_ui(connected: bool, first_frame: bool, has_picture: bool) -> ViewerUi {
    if has_picture || first_frame || connected {
        ViewerUi::Video
    } else {
        ViewerUi::Connecting
    }
}

#[derive(Clone, Default)]
pub struct ViewerStatus {
    pub connected: bool,
    pub error: Option<String>,
    pub stream: Option<StreamInfo>,
    pub decoder: String,
    pub last_second: String,
    pub trace: ConnectTrace,
    pub source_fps: f64,
    pub bitrate_mbps: f64,
    pub decode_ms: f64,
    pub native_width: u32,
    pub native_height: u32,
    pub first_frame: bool,
}

#[derive(Default)]
pub struct Totals {
    pub frames: u64,
    pub bytes: u64,
    pub lost: u64,
    pub skipped_waiting_keyframe: u64,
    pub keyframes: u64,
    pub marker_checked: u64,
    pub marker_mismatch: u64,
    pub keyframe_requests: u64,
    pub decode_errors: u64,
}

pub struct ViewerOptions {
    pub hwdec: bool,
    pub simulate_loss: f64,
    pub rgba: bool,
    pub name: String,
}

pub struct ViewerHandle {
    pub status: Arc<Mutex<ViewerStatus>>,
    pub frames: Receiver<DisplayFrame>,
    pub stop: Arc<AtomicBool>,
    pub done: Receiver<(Totals, StageStats, f64)>,
}

impl ViewerHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub fn spawn(
    runtime: &tokio::runtime::Handle,
    identity: Arc<Identity>,
    target: ConnectInfo,
    opts: ViewerOptions,
    on_frame: Option<Arc<dyn Fn() + Send + Sync>>,
) -> ViewerHandle {
    let status = Arc::new(Mutex::new(ViewerStatus::default()));
    let (frames_tx, frames) = bounded::<DisplayFrame>(1);
    let (done_tx, done) = bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let drain = frames.clone();
    let handle = ViewerHandle {
        status: status.clone(),
        frames,
        stop: stop.clone(),
        done,
    };
    runtime.spawn(async move {
        let result = session(
            identity,
            target,
            opts,
            status.clone(),
            (frames_tx, drain),
            stop,
            on_frame,
        )
        .await;
        match result {
            Ok(summary) => {
                let _ = done_tx.send(summary);
            }
            Err(e) => {
                tracing::error!("viewer: {e:#}");
                let mut s = status.lock().expect("status lock");
                s.connected = false;
                s.error = Some(format!("{e:#}"));
            }
        }
    });
    handle
}

struct ToDecode {
    frame: AssembledFrame,
}

async fn session(
    identity: Arc<Identity>,
    target: ConnectInfo,
    opts: ViewerOptions,
    status: Arc<Mutex<ViewerStatus>>,
    (frames_tx, frames_drain): (Sender<DisplayFrame>, Receiver<DisplayFrame>),
    stop: Arc<AtomicBool>,
    on_frame: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<(Totals, StageStats, f64)> {
    let trace = {
        let s = status.lock().expect("status lock");
        s.trace.clone()
    };
    let session = ViewerSession::connect_traced(
        &identity,
        &target,
        &opts.name,
        &trace,
        HANDSHAKE_TIMEOUT,
    )
    .await?;
    tracing::info!(
        "connected to {} ({}x{}@{} {} via {})",
        target.addr,
        session.stream_info.width,
        session.stream_info.height,
        session.stream_info.fps,
        session.stream_info.codec,
        session.stream_info.encoder
    );
    {
        let mut s = status.lock().expect("status lock");
        s.connected = true;
        s.error = None;
        s.stream = Some(session.stream_info.clone());
        s.native_width = session.stream_info.width;
        s.native_height = session.stream_info.height;
    }

    let opts_loss = opts.simulate_loss;
    let self_healing = session.stream_info.intra_refresh;
    let need_keyframe = Arc::new(AtomicBool::new(true));
    let (decode_tx, decode_rx) = bounded::<ToDecode>(4);
    let control = session.control_sender();
    let clock = session.clock.clone();
    let decode_status = status.clone();
    let decode_need_key = need_keyframe.clone();
    let decode_thread = std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            decode_loop(
                decode_rx,
                opts,
                clock,
                decode_status,
                frames_tx,
                frames_drain,
                decode_need_key,
                control,
                on_frame,
            )
        })?;

    let mut reassembler = Reassembler::new(500_000);
    let mut got_datagram = false;
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let mut requests = 0u64;
    let mut lost = 0u64;
    let mut skipped = 0u64;
    let started = Instant::now();
    let mut rng = now_us() | 1;
    let drop_threshold = (opts_loss.clamp(0.0, 1.0) * u64::MAX as f64) as u64;
    let close_reason = loop {
        if stop.load(Ordering::Acquire) {
            break None;
        }
        let datagram = tokio::select! {
            d = session.conn.read_datagram() => d,
            _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
        };
        let datagram = match datagram {
            Ok(d) => d,
            Err(e) => break Some(e.to_string()),
        };
        if !got_datagram {
            got_datagram = true;
            trace.ok(
                ConnectStepId::FirstDatagram,
                format!("{} byte QUIC datagram from {}", datagram.len(), target.addr),
            );
        }
        if drop_threshold > 0 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            if rng < drop_threshold {
                continue;
            }
        }
        let Some((frame, event)) = reassembler.push(datagram, now_us()) else {
            continue;
        };
        if event.lost_frames > 0 {
            lost += event.lost_frames as u64;
            if !self_healing {
                need_keyframe.store(true, Ordering::Release);
            }
        }
        if need_keyframe.load(Ordering::Acquire) {
            if !frame.keyframe {
                skipped += 1;
                if last_request.elapsed() > Duration::from_millis(100) {
                    session.request_keyframe("recovering from loss");
                    last_request = Instant::now();
                    requests += 1;
                }
                continue;
            }
            need_keyframe.store(false, Ordering::Release);
        }
        match decode_tx.try_send(ToDecode { frame }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::debug!("decoder backlog, dropping frame");
                need_keyframe.store(true, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => break Some("decoder stopped".into()),
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    session.close();
    drop(decode_tx);
    let (mut totals, all) = decode_thread
        .join()
        .map_err(|_| anyhow!("decode thread panicked"))??;
    totals.lost = lost;
    totals.skipped_waiting_keyframe = skipped;
    totals.keyframe_requests += requests;
    if let Some(reason) = close_reason {
        tracing::info!("connection closed: {reason}");
    }
    status.lock().expect("status lock").connected = false;
    Ok((totals, all, elapsed))
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    rx: Receiver<ToDecode>,
    opts: ViewerOptions,
    clock: Arc<Mutex<ClockSync>>,
    status: Arc<Mutex<ViewerStatus>>,
    frames_tx: Sender<DisplayFrame>,
    frames_drain: Receiver<DisplayFrame>,
    need_keyframe: Arc<AtomicBool>,
    control: tokio::sync::mpsc::UnboundedSender<mirrorwire_proto::control::Control>,
    on_frame: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<(Totals, StageStats)> {
    let mut decoder = Decoder::new(if opts.hwdec {
        HwDecode::Auto
    } else {
        HwDecode::None
    })?;
    let mut decoder_name = decoder.name().to_owned();
    {
        let mut s = status.lock().expect("status lock");
        s.decoder = decoder_name.clone();
        s.trace.ok(ConnectStepId::Decoder, decoder_name.clone());
    }
    tracing::info!("decoder: {decoder_name}");
    let mut converter = if opts.rgba {
        Some(RgbaConverter::default())
    } else {
        None
    };
    let mut window = StageStats::new(STAGES);
    let mut all = StageStats::new(STAGES);
    let mut totals = Totals::default();
    let mut window_start = Instant::now();
    let (mut w_frames, mut w_bytes) = (0u64, 0u64);
    let mut last_marker: Option<(u32, u32)> = None;
    let mut consecutive_hw_errors = 0u32;
    let mut first_frame = true;

    while let Ok(ToDecode { frame }) = rx.recv() {
        let offset = clock.lock().expect("clock lock").clone();
        let decode_started = now_us();
        let decoded = match decoder.decode(&frame.data) {
            Ok(Some(d)) => {
                consecutive_hw_errors = 0;
                d
            }
            Ok(None) => continue,
            Err(e) => {
                totals.decode_errors += 1;
                let err_text = format!("{e}");
                let transfer_fail = err_text.contains("hw frame download");
                if decoder.is_hardware() {
                    consecutive_hw_errors += 1;
                }
                if decoder.is_hardware() && (transfer_fail || consecutive_hw_errors >= 3) {
                    tracing::warn!("hardware decode failed ({e}); falling back to software H.264");
                    decoder = Decoder::software_after_hw_failure()?;
                    decoder_name = decoder.name().to_owned();
                    status.lock().expect("status lock").decoder = decoder_name.clone();
                    consecutive_hw_errors = 0;
                } else {
                    tracing::warn!("decode error: {e}");
                }
                need_keyframe.store(true, Ordering::Release);
                let _ = control.send(mirrorwire_proto::control::Control::RequestKeyframe {
                    reason: "decode error".into(),
                });
                totals.keyframe_requests += 1;
                continue;
            }
        };
        let decoded_us = now_us();
        let (y, stride, w, h) = luma(decoded);
        if let Some(marker) = read_marker(y, stride, w, h) {
            if let Some((prev_marker, prev_id)) = last_marker {
                totals.marker_checked += 1;
                if marker.wrapping_sub(prev_marker) != frame.frame_id.wrapping_sub(prev_id) {
                    totals.marker_mismatch += 1;
                }
            }
            last_marker = Some((marker, frame.frame_id));
        }

        let capture_us_viewer = offset.to_viewer_clock(frame.timing.capture_us);
        let (convert_ms, out_frame) = if opts.rgba {
            let mut rgba = Vec::new();
            let (width, height) = converter
                .as_mut()
                .expect("rgba converter")
                .convert(decoded, &mut rgba)?;
            (
                Some(ms(now_us(), decoded_us)),
                Some(DisplayFrame {
                    width,
                    height,
                    picture: Picture::Rgba(rgba),
                    capture_us_viewer,
                }),
            )
        } else {
            let img = YuvImage::from_frame(decoded)?;
            (
                Some(ms(now_us(), decoded_us)),
                Some(DisplayFrame {
                    width: img.width,
                    height: img.height,
                    picture: Picture::Yuv(img),
                    capture_us_viewer,
                }),
            )
        };

        if first_frame {
            first_frame = false;
            decoder_name = decoder.name().to_owned();
            let width = out_frame.as_ref().map(|f| f.width).unwrap_or(0);
            let height = out_frame.as_ref().map(|f| f.height).unwrap_or(0);
            let mut s = status.lock().expect("status lock");
            s.trace.ok(
                ConnectStepId::FirstFrame,
                format!("{width}x{height} via {decoder_name}"),
            );
            s.decoder = decoder_name.clone();
            s.native_width = width;
            s.native_height = height;
            s.first_frame = true;
        }

        let t = frame.timing;
        let mut record = |name: &str, v: f64| {
            window.record(name, v);
            all.record(name, v);
        };
        record("capture_to_encoded", ms(t.encoded_us, t.capture_us));
        record("encoded_to_sent", ms(t.sent_us, t.encoded_us));
        if offset.samples() > 0 {
            // One-way arrival of the first fragment; reassembly is recorded
            // separately so a late last packet is not blamed on "the network".
            record(
                "network",
                ms(frame.first_fragment_us, offset.to_viewer_clock(t.sent_us)).max(0.0),
            );
            record(
                "capture_to_decoded",
                ms(decoded_us, offset.to_viewer_clock(t.capture_us)).max(0.0),
            );
        }
        record(
            "reassembly",
            ms(frame.completed_us, frame.first_fragment_us),
        );
        record("decode_wait", ms(decode_started, frame.completed_us).max(0.0));
        record("decode", ms(decoded_us, decode_started).max(0.0));
        if let Some(c) = convert_ms {
            record("convert", c);
        }

        totals.frames += 1;
        totals.bytes += frame.data.len() as u64;
        totals.keyframes += u64::from(frame.keyframe);
        w_frames += 1;
        w_bytes += frame.data.len() as u64;

        if let Some(display) = out_frame {
            if let Err(TrySendError::Full(display)) = frames_tx.try_send(display) {
                let _ = frames_drain.try_recv();
                let _ = frames_tx.try_send(display);
            }
            if let Some(cb) = &on_frame {
                cb();
            }
        }

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let source_fps = w_frames as f64 / secs;
            let bitrate = w_bytes as f64 * 8.0 / secs / 1e6;
            let decode_ms = window
                .summaries()
                .into_iter()
                .find(|(n, _)| *n == "decode")
                .map(|(_, s)| s.mean)
                .unwrap_or(0.0);
            let line = format!(
                "source {:.1} fps, {:.2} Mbit/s, rtt {:.2} ms, ms mean/p95/max:{}",
                source_fps,
                bitrate,
                offset.rtt_us().unwrap_or(0) as f64 / 1000.0,
                window.report()
            );
            tracing::info!("viewer: {line}");
            {
                let mut s = status.lock().expect("status lock");
                s.last_second = line;
                s.source_fps = source_fps;
                s.bitrate_mbps = bitrate;
                s.decode_ms = decode_ms;
            }
            window.clear();
            window_start = Instant::now();
            (w_frames, w_bytes) = (0, 0);
        }
    }
    Ok((totals, all))
}

pub fn run_headless(args: ViewArgs, config_dir: &Path) -> Result<()> {
    let uri = args
        .uri
        .clone()
        .ok_or_else(|| anyhow!("headless mode needs a mirrorwire:// URI"))?;
    let target = ConnectInfo::parse(&uri)?;
    let identity = Arc::new(Identity::load_or_create(config_dir)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = spawn(
        runtime.handle(),
        identity,
        target,
        ViewerOptions {
            hwdec: args.want_hwdec(),
            simulate_loss: args.simulate_loss / 100.0,
            rgba: args.rgba,
            name: args
                .name
                .clone()
                .unwrap_or_else(|| format!("{} (headless)", crate::default_name())),
        },
        None,
    );

    let deadline = args
        .duration
        .map(|d| Instant::now() + Duration::from_secs_f64(d));
    loop {
        while handle.frames.try_recv().is_ok() {}
        if let Ok(summary) = handle.done.recv_timeout(Duration::from_millis(50)) {
            print_summary(&summary);
            return Ok(());
        }
        if let Some(err) = handle.status.lock().expect("status lock").error.clone() {
            return Err(anyhow!(err));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            handle.stop();
            let summary = handle.done.recv_timeout(Duration::from_secs(5))?;
            print_summary(&summary);
            return Ok(());
        }
    }
}

fn print_summary((totals, stats, secs): &(Totals, StageStats, f64)) {
    println!("--- viewer summary ({secs:.1} s) ---");
    println!(
        "frames {}  avg fps {:.1}  avg bitrate {:.2} Mbit/s  keyframes {}",
        totals.frames,
        totals.frames as f64 / secs,
        totals.bytes as f64 * 8.0 / secs / 1e6,
        totals.keyframes
    );
    println!(
        "lost {}  skipped-until-keyframe {}  keyframe requests {}  decode errors {}",
        totals.lost,
        totals.skipped_waiting_keyframe,
        totals.keyframe_requests,
        totals.decode_errors
    );
    println!(
        "frame marker checks {}  mismatches {}",
        totals.marker_checked, totals.marker_mismatch
    );
    println!(
        "{:<20} {:>8} {:>8} {:>8} {:>8}",
        "stage (ms)", "mean", "p50", "p95", "max"
    );
    for (name, s) in stats.summaries() {
        if s.count > 0 {
            println!(
                "{name:<20} {:>8.2} {:>8.2} {:>8.2} {:>8.2}",
                s.mean, s.p50, s.p95, s.max
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{viewer_ui, ViewerUi};

    #[test]
    fn stays_connecting_until_session_is_up() {
        assert_eq!(viewer_ui(false, false, false), ViewerUi::Connecting);
    }

    #[test]
    fn switches_to_video_once_connected() {
        assert_eq!(viewer_ui(true, false, false), ViewerUi::Video);
    }

    #[test]
    fn switches_to_video_on_first_decoded_frame() {
        assert_eq!(viewer_ui(false, true, false), ViewerUi::Video);
    }

    #[test]
    fn switches_to_video_when_a_picture_is_ready() {
        assert_eq!(viewer_ui(false, false, true), ViewerUi::Video);
    }
}

//! One viewer session: QUIC (mirrorwire-net), fragment reassembly, keyframe
//! recovery, and a decode thread. A compact copy of the desktop viewer loop
//! (`mirrorwire-app/src/viewer.rs`) without FFmpeg.

use crate::codec::{DecodedImage, Decoder, Fed};
use crate::codec_list::CodecEntry;
use crate::stats::{Counters, PipelineHud, PipelineWindow};
use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use mirrorwire_net::diag::{ConnectStepId, ConnectTrace, HANDSHAKE_TIMEOUT};
use mirrorwire_net::identity::Identity;
use mirrorwire_net::pairing::ConnectInfo;
use mirrorwire_net::transport::ViewerSession;
use mirrorwire_proto::clock::now_us;
use mirrorwire_proto::control::{Control, StreamInfo};
use mirrorwire_proto::cursor::ViewerCursor;
use mirrorwire_proto::fec::{LinkHud, LinkWindow};
use mirrorwire_proto::fragment::{AssembledFrame, Reassembler};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct Status {
    pub connected: bool,
    pub error: Option<String>,
    pub stream: Option<StreamInfo>,
    pub decoder: String,
    pub trace: ConnectTrace,
    pub fps: f64,
    pub mbps: f64,
    /// Time from handing a frame to MediaCodec until its picture came out.
    pub decode_ms: f64,
    /// YUV → RGBA conversion of the last picture.
    pub convert_ms: f64,
    pub lost: u64,
    pub first_frame: bool,
    /// How the presenter sends the cursor: `separate`, `embedded`, `hidden`.
    pub cursor_mode: String,
    /// Cursor sent as coordinates, once connected.
    pub cursor: Option<Arc<Mutex<ViewerCursor>>>,
    /// FEC overhead, packet loss and rebuilt packets (last ~0.5 s).
    pub link: LinkHud,
    /// Frames in / decoded / shown per second, and where the picture is stuck.
    pub pipeline: PipelineHud,
}

/// Pipeline totals, written by the network loop, the decode thread and the UI.
#[derive(Default)]
pub struct PipelineCounters {
    received: AtomicU64,
    skipped_waiting_key: AtomicU64,
    backlog_flushes: AtomicU64,
    key_requests: AtomicU64,
    queued: AtomicU64,
    input_full: AtomicU64,
    rendered: AtomicU64,
    superseded: AtomicU64,
    decoded: AtomicU64,
    errors: AtomicU64,
    /// Counted by the UI when it uploads a picture to its texture.
    pub presented: AtomicU64,
}

impl PipelineCounters {
    fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Counters {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Counters {
            received: g(&self.received),
            skipped_waiting_key: g(&self.skipped_waiting_key),
            backlog_flushes: g(&self.backlog_flushes),
            key_requests: g(&self.key_requests),
            queued: g(&self.queued),
            input_full: g(&self.input_full),
            rendered: g(&self.rendered),
            superseded: g(&self.superseded),
            decoded: g(&self.decoded),
            errors: g(&self.errors),
            presented: g(&self.presented),
        }
    }
}

pub struct Handle {
    pub status: Arc<Mutex<Status>>,
    pub frames: Receiver<DecodedImage>,
    pub recycle: Sender<Vec<u8>>,
    pub counters: Arc<PipelineCounters>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn(
    runtime: &tokio::runtime::Handle,
    identity: Arc<Identity>,
    target: ConnectInfo,
    name: String,
    decoders: Vec<CodecEntry>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Handle {
    let status = Arc::new(Mutex::new(Status::default()));
    let (frames_tx, frames) = bounded::<DecodedImage>(1);
    let (recycle, recycle_rx) = bounded::<Vec<u8>>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(PipelineCounters::default());
    let handle = Handle {
        status: status.clone(),
        frames: frames.clone(),
        recycle,
        counters: counters.clone(),
        stop: stop.clone(),
    };
    let channels = Channels { frames_tx, frames_drain: frames, recycle_rx };
    runtime.spawn(async move {
        let result = run(identity, target, name, decoders, status.clone(), channels, counters, stop, repaint.clone()).await;
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected = false;
        if let Err(e) = result {
            tracing::error!("viewer: {e:#}");
            s.error = Some(format!("{e:#}"));
        }
        drop(s);
        repaint();
    });
    handle
}

/// Picture hand-over between the decode thread and the UI.
struct Channels {
    frames_tx: Sender<DecodedImage>,
    frames_drain: Receiver<DecodedImage>,
    recycle_rx: Receiver<Vec<u8>>,
}

#[allow(clippy::too_many_arguments)]
async fn run(
    identity: Arc<Identity>,
    target: ConnectInfo,
    name: String,
    decoders: Vec<CodecEntry>,
    status: Arc<Mutex<Status>>,
    channels: Channels,
    counters: Arc<PipelineCounters>,
    stop: Arc<AtomicBool>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Result<()> {
    let trace = status.lock().unwrap_or_else(|e| e.into_inner()).trace.clone();
    let session = ViewerSession::connect_traced(&identity, &target, &name, &trace, HANDSHAKE_TIMEOUT).await?;
    let info = session.stream_info.clone();
    tracing::info!("connected: {}x{}@{} via {}", info.width, info.height, info.fps, info.encoder);
    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected = true;
        s.error = None;
        s.stream = Some(info.clone());
        s.cursor_mode = session.cursor_mode();
        s.cursor = Some(session.cursor.clone());
    }
    repaint();
    let mut stream_updates = session.stream_updates.clone();

    let need_keyframe = Arc::new(AtomicBool::new(true));
    // Bounded, flushed on overflow (then we wait for a keyframe). Deeper
    // than the desktop viewer's 2: a phone's decoder takes several frame
    // times for a keyframe, and with a 2-deep queue every keyframe overflowed
    // it, which threw the stream away and asked for the next keyframe: the
    // picture stalled after the first frame (measured on the emulator: ~1
    // picture/s, 2+ flushes per second). Frames that wait are still decoded,
    // only their YUV → RGBA copy is skipped (see `Decoder::decode`).
    let (decode_tx, decode_rx) = bounded::<AssembledFrame>(DECODE_QUEUE);
    let decode_flush = decode_rx.clone();
    let control = session.control_sender();
    let dec_status = status.clone();
    let dec_need_key = need_keyframe.clone();
    let dec_trace = trace.clone();
    let dec_repaint = repaint.clone();
    let dec_counters = counters.clone();
    let decode_thread = std::thread::Builder::new().name("decode".into()).spawn(move || {
        let ctx = DecodeCtx {
            status: dec_status,
            channels,
            need_keyframe: dec_need_key,
            control,
            trace: dec_trace,
            repaint: dec_repaint,
            counters: dec_counters,
        };
        decode_loop(decode_rx, info, decoders, ctx)
    })?;

    let self_healing = session.stream_info.intra_refresh;
    let mut reassembler = Reassembler::new(500_000);
    let mut got_datagram = false;
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let mut lost = 0u64;
    let mut window = (Instant::now(), 0u64, 0u64);
    let mut link_window = LinkWindow::default();
    let mut pipeline_window = PipelineWindow::default();
    let close_reason = loop {
        if stop.load(Ordering::Acquire) {
            break None;
        }
        // Before waiting for a datagram, so a stall with no traffic still updates the HUD.
        if let Some(hud) = pipeline_window.tick(counters.snapshot(), now_us(), 1_000_000) {
            if let Some(warning) = hud.stall() {
                tracing::warn!("{warning} [{}]", hud.line().replace('\n', "; "));
            } else {
                tracing::info!("pipeline: {}", hud.line().replace('\n', "; "));
            }
            status.lock().unwrap_or_else(|e| e.into_inner()).pipeline = hud.clone();
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
            trace.ok(ConnectStepId::FirstDatagram, format!("{} byte datagram", datagram.len()));
        }
        if stream_updates.has_changed().unwrap_or(false) {
            let info = stream_updates.borrow_and_update().clone();
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.cursor_mode = info.cursor.clone().unwrap_or_else(|| "embedded".into());
            s.stream = Some(info);
        }
        if let Some(w) = link_window.tick(reassembler.link_stats(), now_us()) {
            session.report_link(&w);
            status.lock().unwrap_or_else(|e| e.into_inner()).link = link_window.hud;
        }
        if session.handle_pointer_datagram(&datagram) {
            repaint();
            continue;
        }
        let Some((frame, event)) = reassembler.push(datagram, now_us()) else { continue };
        PipelineCounters::add(&counters.received, 1);
        window.1 += 1;
        window.2 += frame.data.len() as u64;
        if window.0.elapsed() >= Duration::from_secs(1) {
            let secs = window.0.elapsed().as_secs_f64();
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.fps = window.1 as f64 / secs;
            s.mbps = window.2 as f64 * 8.0 / secs / 1e6;
            s.lost = lost;
            window = (Instant::now(), 0, 0);
        }
        if event.lost_frames > 0 {
            lost += event.lost_frames as u64;
            if !self_healing {
                need_keyframe.store(true, Ordering::Release);
            }
        }
        if need_keyframe.load(Ordering::Acquire) {
            if !frame.keyframe {
                PipelineCounters::add(&counters.skipped_waiting_key, 1);
                if last_request.elapsed() > Duration::from_millis(100) {
                    session.request_keyframe("recovering from loss");
                    PipelineCounters::add(&counters.key_requests, 1);
                    last_request = Instant::now();
                }
                continue;
            }
            need_keyframe.store(false, Ordering::Release);
        }
        match decode_tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                while decode_flush.try_recv().is_ok() {}
                PipelineCounters::add(&counters.backlog_flushes, 1);
                need_keyframe.store(true, Ordering::Release);
                if last_request.elapsed() > Duration::from_millis(100) {
                    session.request_keyframe("decoder backlog");
                    PipelineCounters::add(&counters.key_requests, 1);
                    last_request = Instant::now();
                }
            }
            Err(TrySendError::Disconnected(_)) => break Some("decoder stopped".into()),
        }
    };
    session.close();
    drop(decode_tx);
    decode_thread.join().map_err(|_| anyhow!("decode thread panicked"))??;
    if let Some(reason) = close_reason {
        tracing::info!("connection closed: {reason}");
        if !stop.load(Ordering::Acquire) {
            status.lock().unwrap_or_else(|e| e.into_inner()).error = Some(format!("connection closed: {reason}"));
        }
    }
    Ok(())
}

/// Everything the decode thread shares with the session.
struct DecodeCtx {
    status: Arc<Mutex<Status>>,
    channels: Channels,
    need_keyframe: Arc<AtomicBool>,
    control: tokio::sync::mpsc::UnboundedSender<Control>,
    trace: ConnectTrace,
    repaint: Arc<dyn Fn() + Send + Sync>,
    counters: Arc<PipelineCounters>,
}

impl DecodeCtx {
    fn request_keyframe(&self, reason: &str) {
        self.need_keyframe.store(true, Ordering::Release);
        let _ = self.control.send(Control::RequestKeyframe { reason: reason.into() });
        PipelineCounters::add(&self.counters.key_requests, 1);
    }

    /// Hands a picture to the UI; the newest picture wins.
    fn show(&self, img: DecodedImage, decoder: &Decoder, first: &mut bool) {
        {
            let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ms) = decoder.latency_ms() {
                s.decode_ms = ms;
            }
            s.convert_ms = decoder.convert_ms();
            s.decoder = decoder.name().to_owned();
            s.first_frame = true;
        }
        if *first {
            *first = false;
            self.trace.ok(ConnectStepId::FirstFrame, format!("{}x{} via {}", img.width, img.height, decoder.name()));
        }
        PipelineCounters::add(&self.counters.decoded, 1);
        if let Err(TrySendError::Full(img)) = self.channels.frames_tx.try_send(img) {
            let _ = self.channels.frames_drain.try_recv();
            let _ = self.channels.frames_tx.try_send(img);
        }
        (self.repaint)();
    }
}

/// Frames that may wait for the decoder before the queue is flushed.
const DECODE_QUEUE: usize = 6;

/// How long the decode thread waits for a frame before it polls MediaCodec
/// for pictures that finished late.
const POLL_EVERY: Duration = Duration::from_millis(8);

fn decode_loop(rx: Receiver<AssembledFrame>, info: StreamInfo, decoders: Vec<CodecEntry>, ctx: DecodeCtx) -> Result<()> {
    let mut decoder = Decoder::new(info.width, info.height, &decoders);
    let mut first = true;
    let mut synced = crate::codec::Counts::default();
    loop {
        let frame = match rx.recv_timeout(POLL_EVERY) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Ok(buf) = ctx.channels.recycle_rx.try_recv() {
            decoder.recycle(buf);
        }
        let result = match &frame {
            // A newer frame is already waiting: decode this one, don't show it.
            Some(f) => decoder.decode(&f.data, f.keyframe, f.completed_us, rx.is_empty()),
            None => decoder.poll().map(Fed::Queued),
        };
        // Mirror the decoder's own counters into the shared ones.
        let now = decoder.counts();
        PipelineCounters::add(&ctx.counters.queued, now.queued - synced.queued);
        PipelineCounters::add(&ctx.counters.input_full, now.input_full - synced.input_full);
        PipelineCounters::add(&ctx.counters.rendered, now.rendered - synced.rendered);
        PipelineCounters::add(&ctx.counters.superseded, now.superseded - synced.superseded);
        synced = now;
        match result {
            Ok(Fed::Queued(Some(img))) => ctx.show(img, &decoder, &mut first),
            Ok(Fed::Queued(None)) | Ok(Fed::NotOpen) => {
                if frame.is_some() {
                    ctx.status.lock().unwrap_or_else(|e| e.into_inner()).decoder = decoder.name().to_owned();
                }
            }
            Ok(Fed::InputFull(img)) => {
                // The dropped frame is a reference for the next ones: without
                // a keyframe the picture would stay broken or frozen.
                tracing::warn!("decoder input full, frame dropped; asking for a keyframe");
                ctx.request_keyframe("decoder input full");
                if let Some(img) = img {
                    ctx.show(img, &decoder, &mut first);
                }
            }
            Err(e) => {
                tracing::warn!("decode error: {e:#}");
                PipelineCounters::add(&ctx.counters.errors, 1);
                ctx.request_keyframe("decode error");
                let mut s = ctx.status.lock().unwrap_or_else(|e| e.into_inner());
                s.decoder = format!("{} (error: {e:#})", decoder.name());
            }
        }
    }
    Ok(())
}

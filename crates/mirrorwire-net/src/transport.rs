//! QUIC transport. One reliable bidirectional stream carries control messages
//! (hello, pairing, keyframe requests, clock pings); video goes out as
//! unreliable datagrams so a lost packet never stalls later frames.

use crate::diag::{
    format_candidates, is_logical_reject, is_unreachable, no_answer_message, push_attempt,
    ConnectStepId, ConnectTrace, IncomingAttempt, HANDSHAKE_TIMEOUT,
};
use crate::identity::{short_fingerprint, Identity};
use crate::pairing::{ConnectInfo, PairedViewers};
use crate::tls;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use mirrorwire_proto::clock::{now_us, ClockSync};
use mirrorwire_proto::control::{Control, StreamInfo};
use mirrorwire_proto::cursor::{
    is_pointer_datagram, CursorEvent, CursorImage, PointerUpdate, ViewerCursor, CAP_CURSOR_METADATA,
    MAX_IMAGE_STREAM,
};
use mirrorwire_proto::fec::{fec_fragment, FecHeader, FecMode, LinkStats, CAP_FEC};
use mirrorwire_proto::fragment::{fragment, FrameTiming};
use mirrorwire_proto::stats::StageStats;
use mirrorwire_proto::PROTOCOL_VERSION;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Presenter datagram queue. At Wi-Fi rates 2 MiB is ~100–150 ms of video.
pub const DATAGRAM_SEND_BUFFER: usize = 2 << 20;

/// Whether a delta frame of `frame_len` bytes should be dropped because the
/// datagram queue is too full to take it (a keyframe is always queued).
pub fn should_drop_for_backlog(keyframe: bool, frame_len: usize, free_space: usize) -> bool {
    !keyframe && free_space < frame_len + frame_len / 16 + 4096
}

/// Ceiling for [`FecState::boost_pct`].
const MAX_FEC_BOOST: f32 = 30.0;

/// Presenter-side FEC state, shared with the app: the setting, and what the
/// current viewer last reported about the link.
#[derive(Debug)]
pub struct FecControl {
    state: Mutex<FecState>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FecState {
    pub mode: FecMode,
    /// The connected viewer can rebuild FEC packets.
    pub viewer_fec: bool,
    /// Smoothed packet loss the viewer reports (percent); drives auto mode.
    pub loss_pct: f32,
    /// Data packets the viewer rebuilt from parity, per second.
    pub recovered_per_s: f32,
    /// Frames the viewer could not rebuild since it connected.
    pub unrecoverable: u64,
    /// Extra parity (percentage points, auto mode) added while frames are
    /// still lost: +5 per report with unrecoverable frames, -1 per clean one.
    pub boost_pct: f32,
    pub reports: u64,
    last_report: Option<Instant>,
}

impl FecState {
    /// Overhead currently applied to (delta, key) frames, or `None` when no
    /// FEC is sent (setting off, or an older viewer).
    pub fn overheads(&self) -> Option<(f32, f32)> {
        if !self.viewer_fec {
            return None;
        }
        Some((
            self.mode.overhead(self.loss_pct, self.boost_pct, false)?,
            self.mode.overhead(self.loss_pct, self.boost_pct, true)?,
        ))
    }
}

impl FecControl {
    pub fn new(mode: FecMode) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(FecState { mode, ..Default::default() }) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FecState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> FecState {
        *self.lock()
    }

    pub fn set_mode(&self, mode: FecMode) {
        self.lock().mode = mode;
    }

    fn viewer_connected(&self, fec: bool) {
        let mut s = self.lock();
        let mode = s.mode;
        *s = FecState { mode, viewer_fec: fec, ..Default::default() };
    }

    fn overhead(&self, keyframe: bool) -> Option<f32> {
        let s = self.lock();
        if !s.viewer_fec {
            return None;
        }
        s.mode.overhead(s.loss_pct, s.boost_pct, keyframe)
    }

    fn on_report(&self, loss_permille: u32, recovered: u32, unrecoverable: u32) {
        let mut s = self.lock();
        let loss = (loss_permille.min(1000) as f32) / 10.0;
        // Rise fast, decay slowly: a loss burst should get more parity at once.
        let w = if loss > s.loss_pct { 0.5 } else { 0.15 };
        s.loss_pct = if s.reports == 0 { loss } else { s.loss_pct + w * (loss - s.loss_pct) };
        let now = Instant::now();
        let secs = s
            .last_report
            .map(|t| now.duration_since(t).as_secs_f32())
            .filter(|d| *d > 0.05)
            .unwrap_or(0.5);
        s.recovered_per_s = recovered as f32 / secs;
        s.unrecoverable += unrecoverable as u64;
        s.boost_pct = if unrecoverable > 0 {
            (s.boost_pct + 5.0 * unrecoverable.min(3) as f32).min(MAX_FEC_BOOST)
        } else {
            (s.boost_pct - 1.0).max(0.0)
        };
        s.reports += 1;
        s.last_report = Some(now);
    }
}

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.datagram_receive_buffer_size(Some(32 << 20));
    // Bounded so frames can't pile up inside QUIC: the send loop drops a
    // delta frame (and asks for a keyframe) instead of queueing behind this.
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    t.send_window(8 * 1024 * 1024);
    t.receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    t.stream_receive_window(quinn::VarInt::from_u32(4 * 1024 * 1024));
    // Default initial RTT is 333 ms, which paces a small window down to
    // tens of kilobits. A LAN RTT is a couple of milliseconds.
    t.initial_rtt(Duration::from_millis(2));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("valid idle timeout")));
    t.congestion_controller_factory(crate::lan_cc::factory());
    Arc::new(t)
}

async fn write_msg(send: &mut SendStream, msg: &Control) -> Result<()> {
    send.write_all(&msg.encode()).await?;
    Ok(())
}

async fn read_msg(recv: &mut RecvStream) -> Result<Control> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = Control::check_len(u32::from_be_bytes(len))?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Control::decode_body(&body)
}

#[derive(Debug, Clone)]
pub struct OutgoingFrame {
    /// Consecutive per encoded frame; a gap tells the viewer a frame was lost.
    pub frame_id: u32,
    pub keyframe: bool,
    pub data: Bytes,
    pub capture_us: u64,
    pub encoded_us: u64,
}

pub struct PresenterConfig {
    pub bind: SocketAddr,
    pub stream_info: StreamInfo,
    /// Code an unpaired viewer must present. `None` accepts any viewer.
    pub pairing_code: Option<String>,
    pub config_dir: PathBuf,
    /// Incoming handshake / pairing attempts for the presenter GUI.
    pub attempts: std::sync::Arc<Mutex<Vec<IncomingAttempt>>>,
    /// Called when a viewer is accepted, with whether it can draw a
    /// separately sent cursor ([`CAP_CURSOR_METADATA`]).
    pub on_viewer: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    /// Cursor position and image changes from the capture source (the
    /// sending half lives with the capture thread). Sent only to viewers that
    /// can draw them.
    pub cursor_events: Option<mpsc::Receiver<CursorEvent>>,
    /// FEC setting and link feedback (see [`FecControl`]).
    pub fec: Arc<FecControl>,
}

type Callback = Arc<dyn Fn() + Send + Sync>;

/// The viewer currently receiving the stream.
#[derive(Clone)]
struct ActiveViewer {
    conn: Connection,
    /// Listed [`CAP_CURSOR_METADATA`] in its hello.
    cursor: bool,
    /// Listed [`CAP_FEC`] in its hello.
    fec: bool,
}

pub struct Presenter {
    pub frames: mpsc::Sender<OutgoingFrame>,
    pub local_addr: SocketAddr,
    pub viewer_connected: watch::Receiver<bool>,
    stream_info: Arc<watch::Sender<StreamInfo>>,
    fec: Arc<FecControl>,
    endpoint: Endpoint,
}

impl Presenter {
    pub fn start(identity: &Identity, cfg: PresenterConfig, on_keyframe_request: Callback) -> Result<Self> {
        let tls = tls::server_config(identity)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
        server.transport_config(transport_config());
        let endpoint = Endpoint::server(server, cfg.bind).context("binding QUIC endpoint")?;
        let local_addr = endpoint.local_addr()?;

        let (frames, frames_rx) = mpsc::channel::<OutgoingFrame>(8);
        let (current_tx, current_rx) = watch::channel::<Option<ActiveViewer>>(None);
        let (connected_tx, viewer_connected) = watch::channel(false);
        let current_tx = Arc::new(current_tx);
        let mut info = cfg.stream_info;
        info.fec = Some(cfg.fec.snapshot().mode.as_string());
        let stream_info = Arc::new(watch::channel(info).0);

        tokio::spawn(send_loop(frames_rx, current_rx.clone(), on_keyframe_request.clone(), cfg.fec.clone()));
        if let Some(cursor_rx) = cfg.cursor_events {
            tokio::spawn(cursor_loop(cursor_rx, current_rx));
        }

        let shared = Arc::new(SessionShared {
            stream_info: stream_info.clone(),
            pairing_code: cfg.pairing_code,
            paired: Mutex::new(PairedViewers::load(&cfg.config_dir)),
            current: current_tx,
            connected: connected_tx,
            on_keyframe_request,
            on_viewer: cfg.on_viewer,
            attempts: cfg.attempts,
            fec: cfg.fec.clone(),
        });
        let ep = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_viewer(incoming, shared).await {
                        tracing::warn!("viewer session ended: {e:#}");
                    }
                });
            }
        });

        Ok(Self { frames, local_addr, viewer_connected, stream_info, fec: cfg.fec, endpoint })
    }

    /// Change the stream description; connected viewers get a fresh
    /// `stream_info` (older viewers ignore it), new viewers get the new one.
    pub fn update_stream_info(&self, f: impl FnOnce(&mut StreamInfo)) {
        self.stream_info.send_if_modified(|info| {
            let before = info.clone();
            f(info);
            *info != before
        });
    }

    /// Change the FEC setting live; viewers see it in `stream_info.fec`.
    pub fn set_fec_mode(&self, mode: FecMode) {
        self.fec.set_mode(mode);
        self.update_stream_info(|i| i.fec = Some(mode.as_string()));
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"presenter stopped");
    }
}

struct SessionShared {
    stream_info: Arc<watch::Sender<StreamInfo>>,
    pairing_code: Option<String>,
    paired: Mutex<PairedViewers>,
    current: Arc<watch::Sender<Option<ActiveViewer>>>,
    connected: watch::Sender<bool>,
    on_keyframe_request: Callback,
    on_viewer: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    attempts: Arc<Mutex<Vec<IncomingAttempt>>>,
    fec: Arc<FecControl>,
}

async fn handle_viewer(incoming: quinn::Incoming, shared: Arc<SessionShared>) -> Result<()> {
    let remote = incoming.remote_address();
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            push_attempt(
                &shared.attempts,
                IncomingAttempt::new(remote, None, false, format!("handshake failed: {e:#}")),
            );
            return Err(e.into());
        }
    };
    let remote = conn.remote_address();
    let fp = match tls::peer_fingerprint(&conn) {
        Some(fp) => fp,
        None => {
            push_attempt(
                &shared.attempts,
                IncomingAttempt::new(remote, None, false, "handshake failed: no certificate"),
            );
            bail!("viewer sent no certificate");
        }
    };
    let (mut send, mut recv) = conn.accept_bi().await?;

    let Control::Hello { protocol, name, pairing_code, caps } = read_msg(&mut recv).await? else {
        push_attempt(
            &shared.attempts,
            IncomingAttempt::new(remote, None, false, "expected hello on control stream"),
        );
        bail!("expected hello from {remote}");
    };
    if protocol != PROTOCOL_VERSION {
        let reason = format!("protocol {protocol} unsupported");
        write_msg(&mut send, &Control::Rejected { reason: reason.clone() }).await?;
        push_attempt(
            &shared.attempts,
            IncomingAttempt::new(remote, Some(name.clone()), false, reason),
        );
        bail!("viewer {remote} speaks protocol {protocol}");
    }
    let authorized = match &shared.pairing_code {
        None => true,
        Some(code) => {
            let mut paired = shared.paired.lock().expect("paired lock");
            if paired.contains(&fp) {
                true
            } else if pairing_code.as_deref() == Some(code.as_str()) {
                paired.add(&fp, &name)?;
                tracing::info!("paired new viewer {name} ({})", crate::identity::short_fingerprint(&fp));
                true
            } else {
                false
            }
        }
    };
    if !authorized {
        let reason = "not paired: pairing code required".to_owned();
        write_msg(&mut send, &Control::Rejected { reason: reason.clone() }).await?;
        send.finish()?;
        push_attempt(
            &shared.attempts,
            IncomingAttempt::new(remote, Some(name.clone()), false, "pairing rejected: wrong or missing code"),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        bail!("rejected unpaired viewer {name} at {remote}");
    }

    let viewer_cursor = caps.iter().any(|c| c == CAP_CURSOR_METADATA);
    let viewer_fec = caps.iter().any(|c| c == CAP_FEC);
    let mut info_rx = shared.stream_info.subscribe();
    let stream_info = info_rx.borrow_and_update().clone();
    write_msg(&mut send, &Control::StreamInfo(stream_info)).await?;
    push_attempt(
        &shared.attempts,
        IncomingAttempt::new(remote, Some(name.clone()), true, "connected"),
    );
    tracing::info!(
        "viewer {name} connected from {remote} (cursor as coordinates: {}, FEC: {})",
        if viewer_cursor { "supported" } else { "not supported, older viewer" },
        if viewer_fec { "supported" } else { "not supported, older viewer" }
    );
    shared.fec.viewer_connected(viewer_fec);
    let active = ActiveViewer { conn: conn.clone(), cursor: viewer_cursor, fec: viewer_fec };
    if let Some(old) = shared.current.send_replace(Some(active)) {
        old.conn.close(1u32.into(), b"replaced by another viewer");
    }
    shared.connected.send_replace(true);
    if let Some(cb) = &shared.on_viewer {
        cb(viewer_cursor);
    }
    (shared.on_keyframe_request)();

    // One writer owns the send half: pongs from the reader below, and a
    // fresh stream_info whenever the presenter changes it.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Control>();
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                m = out_rx.recv() => match m {
                    Some(m) => m,
                    None => return,
                },
                changed = info_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    Control::StreamInfo(info_rx.borrow_and_update().clone())
                }
            };
            if write_msg(&mut send, &msg).await.is_err() {
                return;
            }
        }
    });

    let result = async {
        loop {
            let msg = match read_msg(&mut recv).await {
                Ok(m) => m,
                Err(_) if conn.close_reason().is_some() => return Ok(()),
                Err(e) => return Err(e),
            };
            match msg {
                Control::Ping { t_viewer_us } => {
                    let _ = out_tx.send(Control::Pong { t_viewer_us, t_presenter_us: now_us() });
                }
                Control::RequestKeyframe { reason } => {
                    tracing::debug!("keyframe requested: {reason}");
                    (shared.on_keyframe_request)();
                }
                Control::LinkReport { loss_permille, packets, recovered, unrecoverable } => {
                    tracing::debug!(
                        "viewer link: loss {:.1}% of {packets} packets, {recovered} rebuilt, {unrecoverable} frames lost",
                        loss_permille as f32 / 10.0
                    );
                    shared.fec.on_report(loss_permille, recovered, unrecoverable);
                }
                Control::Bye => return Ok::<_, anyhow::Error>(()),
                other => tracing::debug!("ignoring {other:?}"),
            }
        }
    }
    .await;
    writer.abort();

    shared.current.send_if_modified(|cur| {
        if cur.as_ref().is_some_and(|c| c.conn.stable_id() == conn.stable_id()) {
            *cur = None;
            true
        } else {
            false
        }
    });
    if shared.current.borrow().is_none() {
        shared.connected.send_replace(false);
    }
    tracing::info!("viewer {name} disconnected");
    result
}

/// Forwards cursor changes to the current viewer: positions as datagrams
/// (with a fresh sequence number), images on their own unidirectional
/// stream. A newly connected viewer first gets every cached image and the
/// last position, so it can draw the cursor before it moves.
async fn cursor_loop(mut events: mpsc::Receiver<CursorEvent>, mut current: watch::Receiver<Option<ActiveViewer>>) {
    const CACHE: usize = 32;
    let mut seq = 0u32;
    let mut last: Option<PointerUpdate> = None;
    let mut images: std::collections::VecDeque<Arc<CursorImage>> = Default::default();
    let target = |current: &watch::Receiver<Option<ActiveViewer>>| {
        current.borrow().as_ref().filter(|v| v.cursor).map(|v| v.conn.clone())
    };
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                None => return,
                Some(CursorEvent::Image(img)) => {
                    let img = Arc::new(img);
                    images.retain(|i| i.id != img.id);
                    if images.len() >= CACHE {
                        images.pop_front();
                    }
                    images.push_back(img.clone());
                    if let Some(conn) = target(&current) {
                        send_cursor_image(conn, img);
                    }
                }
                Some(CursorEvent::Pointer(mut p)) => {
                    seq = seq.wrapping_add(1);
                    p.seq = seq;
                    last = Some(p);
                    if let Some(conn) = target(&current) {
                        let _ = conn.send_datagram(Bytes::copy_from_slice(&p.encode()));
                    }
                }
            },
            changed = current.changed() => {
                if changed.is_err() {
                    return;
                }
                let Some(conn) = target(&current) else { continue };
                for img in &images {
                    send_cursor_image(conn.clone(), img.clone());
                }
                if let Some(mut p) = last {
                    seq = seq.wrapping_add(1);
                    p.seq = seq;
                    last = Some(p);
                    let _ = conn.send_datagram(Bytes::copy_from_slice(&p.encode()));
                }
            }
        }
    }
}

fn send_cursor_image(conn: Connection, img: Arc<CursorImage>) {
    tokio::spawn(async move {
        let result = async {
            let mut s = conn.open_uni().await?;
            s.write_all(&img.encode()).await?;
            s.finish()?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!("cursor image {} not sent: {e:#}", img.id);
        }
    });
}

async fn send_loop(
    mut frames: mpsc::Receiver<OutgoingFrame>,
    current: watch::Receiver<Option<ActiveViewer>>,
    on_keyframe_request: Callback,
    fec: Arc<FecControl>,
) {
    let mut stats = StageStats::new(&["capture_to_encoded", "encoded_to_sent", "send"]);
    let mut window_start = Instant::now();
    let (mut window_frames, mut window_bytes, mut window_keyframes) = (0u32, 0usize, 0u32);
    let mut backlog_drops = 0u32;
    let (mut window_parity, mut window_packets) = (0usize, 0usize);

    while let Some(frame) = frames.recv().await {
        let Some((conn, viewer_fec)) = current.borrow().as_ref().map(|v| (v.conn.clone(), v.fec)) else { continue };
        let Some(max) = conn.max_datagram_size() else { continue };
        let sent_us = now_us();
        let timing = FrameTiming {
            capture_us: frame.capture_us,
            encoded_us: frame.encoded_us,
            sent_us,
        };
        let overhead = if viewer_fec { fec.overhead(frame.keyframe) } else { None };
        let packets = match overhead {
            Some(o) => fec_fragment(frame.frame_id, frame.keyframe, timing, &frame.data, max, o),
            None => fragment(frame.frame_id, frame.keyframe, timing, &frame.data, max),
        };
        let wire_len: usize = packets.iter().map(|p| p.len()).sum();
        if should_drop_for_backlog(frame.keyframe, wire_len, conn.datagram_send_buffer_space()) {
            // The network is behind (big scene change on Wi-Fi). Sending this
            // frame would only add latency; the viewer recovers with the
            // keyframe we ask for instead.
            backlog_drops += 1;
            tracing::debug!("network backlog: dropped frame {} ({} bytes)", frame.frame_id, frame.data.len());
            on_keyframe_request();
            continue;
        }
        // Burst every fragment (parity last); the congestion controller's
        // pacer spreads them out. send_datagram queues immediately; only wait
        // when the datagram send buffer is full so a frame is not paced out
        // one packet at a time.
        let mut send_err = false;
        window_packets += packets.len();
        if overhead.is_some() {
            window_parity += packets
                .first()
                .and_then(|p| FecHeader::parse(p))
                .map_or(0, |(h, _)| h.parity_count as usize);
        }
        for datagram in packets {
            if conn.send_datagram(datagram.clone()).is_ok() {
                continue;
            }
            if let Err(e) = conn.send_datagram_wait(datagram).await {
                tracing::debug!("datagram send failed: {e}");
                send_err = true;
                break;
            }
        }
        let done_us = now_us();
        let send_ms = ms(done_us, sent_us);
        if send_ms > 8.0 || send_err {
            tracing::info!(
                "QUIC send of frame {} took {send_ms:.1} ms{}",
                frame.frame_id,
                if send_err { " (truncated)" } else { "" }
            );
        }
        stats.record("capture_to_encoded", ms(frame.encoded_us, frame.capture_us));
        stats.record("encoded_to_sent", ms(sent_us, frame.encoded_us));
        stats.record("send", ms(done_us, sent_us));
        window_frames += 1;
        window_bytes += frame.data.len();
        window_keyframes += u32::from(frame.keyframe);

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            tracing::info!(
                "presenter: {:.1} fps, {:.2} Mbit/s, {} keyframes, {} dropped for backlog, mtu {}, {}, ms mean/p95/max:{}",
                window_frames as f64 / secs,
                window_bytes as f64 * 8.0 / secs / 1e6,
                window_keyframes,
                backlog_drops,
                max,
                fec_summary(&fec.snapshot(), window_parity, window_packets),
                stats.report()
            );
            backlog_drops = 0;
            (window_parity, window_packets) = (0, 0);
            stats.clear();
            window_start = Instant::now();
            (window_frames, window_bytes, window_keyframes) = (0, 0, 0);
        }
    }
}

fn fec_summary(s: &FecState, parity: usize, packets: usize) -> String {
    match s.overheads() {
        None if !s.viewer_fec => "fec off (viewer without FEC)".into(),
        None => "fec off".into(),
        Some(_) => format!(
            "fec {} ~{:.0}% parity packets (boost +{:.0}), viewer loss {:.1}%, rebuilt {:.0}/s, unrecoverable {}",
            s.mode.as_string(),
            100.0 * parity as f64 / packets.saturating_sub(parity).max(1) as f64,
            s.boost_pct,
            s.loss_pct,
            s.recovered_per_s,
            s.unrecoverable
        ),
    }
}

pub fn ms(later_us: u64, earlier_us: u64) -> f64 {
    (later_us as i64 - earlier_us as i64) as f64 / 1000.0
}

pub struct ViewerSession {
    pub conn: Connection,
    pub stream_info: StreamInfo,
    /// Newest `stream_info` (the presenter re-sends it when e.g. the cursor
    /// mode changes).
    pub stream_updates: watch::Receiver<StreamInfo>,
    /// Cursor sent as coordinates: latest position and cached images.
    pub cursor: Arc<Mutex<ViewerCursor>>,
    pub clock: Arc<Mutex<ClockSync>>,
    control: mpsc::UnboundedSender<Control>,
    _endpoint: Endpoint,
}

impl ViewerSession {
    pub async fn connect(identity: &Identity, target: &ConnectInfo, viewer_name: &str) -> Result<Self> {
        Self::connect_traced(identity, target, viewer_name, &ConnectTrace::new(), HANDSHAKE_TIMEOUT).await
    }

    pub async fn connect_traced(
        identity: &Identity,
        target: &ConnectInfo,
        viewer_name: &str,
        trace: &ConnectTrace,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        let addrs = target.candidates();
        trace.start(
            ConnectStepId::Resolve,
            format!("trying {} (UDP)", format_candidates(target)),
        );
        if addrs.is_empty() {
            let msg = "no addresses to try".to_owned();
            trace.fail(ConnectStepId::Resolve, msg.clone());
            bail!("{msg}");
        }
        trace.ok(
            ConnectStepId::Resolve,
            format!("{} address(es): {}", addrs.len(), format_candidates(target)),
        );

        let mut last_err: Option<anyhow::Error> = None;
        for (i, addr) in addrs.iter().enumerate() {
            trace.start(
                ConnectStepId::Handshake,
                format!("QUIC handshake to {addr} (UDP), try {}/{}", i + 1, addrs.len()),
            );
            match connect_one(identity, target, viewer_name, *addr, trace, handshake_timeout).await {
                Ok(session) => return Ok(session),
                Err(e) => {
                    tracing::warn!("connect to {addr} failed: {e:#}");
                    if is_logical_reject(&e) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        let cause = last_err
            .as_ref()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        let msg = if last_err.as_ref().is_some_and(is_unreachable) || last_err.is_some() {
            no_answer_message(&addrs, Some(cause.trim()))
        } else {
            format!("failed to connect to {}: {cause}", format_candidates(target))
        };
        trace.fail(ConnectStepId::Handshake, msg.clone());
        trace.set_error(msg.clone(), Some(msg.clone()));
        bail!("{msg}")
    }

    pub fn request_keyframe(&self, reason: &str) {
        let _ = self.control.send(Control::RequestKeyframe { reason: reason.to_owned() });
    }

    /// If `datagram` is a pointer update, apply it and return true (the
    /// caller then skips video reassembly). Malformed ones are dropped.
    pub fn handle_pointer_datagram(&self, datagram: &[u8]) -> bool {
        if !is_pointer_datagram(datagram) {
            return false;
        }
        if let Some(p) = PointerUpdate::decode(datagram) {
            self.cursor.lock().unwrap_or_else(|e| e.into_inner()).apply(p);
        }
        true
    }

    /// The cursor mode the presenter currently reports (`separate`,
    /// `embedded`, `hidden`); older presenters report nothing (= embedded).
    pub fn cursor_mode(&self) -> String {
        self.stream_updates
            .borrow()
            .cursor
            .clone()
            .unwrap_or_else(|| mirrorwire_proto::control::CURSOR_EMBEDDED.to_owned())
    }

    /// The presenter's FEC setting, or `None` for an older presenter (which
    /// must not get `link_report` messages).
    pub fn presenter_fec(&self) -> Option<String> {
        self.stream_updates.borrow().fec.clone()
    }

    /// Tell the presenter how the link did over `window` (a difference of
    /// [`LinkStats`] snapshots). No-op for older presenters.
    pub fn report_link(&self, window: &LinkStats) {
        if self.presenter_fec().is_none() || window.expected == 0 {
            return;
        }
        let loss = window.expected.saturating_sub(window.received);
        let _ = self.control.send(Control::LinkReport {
            loss_permille: (loss * 1000 / window.expected).min(1000) as u32,
            packets: window.expected.min(u32::MAX as u64) as u32,
            recovered: window.recovered.min(u32::MAX as u64) as u32,
            unrecoverable: window.unrecoverable_frames.min(u32::MAX as u64) as u32,
        });
    }

    pub fn control_sender(&self) -> mpsc::UnboundedSender<Control> {
        self.control.clone()
    }

    pub fn close(&self) {
        let _ = self.control.send(Control::Bye);
        self.conn.close(0u32.into(), b"viewer closed");
    }
}

async fn connect_one(
    identity: &Identity,
    target: &ConnectInfo,
    viewer_name: &str,
    addr: SocketAddr,
    trace: &ConnectTrace,
    handshake_timeout: Duration,
) -> Result<ViewerSession> {
    let tls = tls::client_config(identity, &target.fingerprint)?;
    let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    client.transport_config(transport_config());
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(client);

    let connecting = endpoint
        .connect(addr, crate::identity::CERT_SERVER_NAME)
        .with_context(|| format!("connecting to {addr}"))?;
    let conn = match tokio::time::timeout(handshake_timeout, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let err = anyhow!(e).context(format!("connecting to {addr}"));
            if is_unreachable(&err) {
                let msg = no_answer_message(&[addr], Some(&format!("{err:#}")));
                trace.fail(ConnectStepId::Handshake, msg.clone());
                bail!("{msg}");
            }
            trace.fail(ConnectStepId::Handshake, format!("{err:#}"));
            return Err(err);
        }
        Err(_) => {
            let msg = no_answer_message(
                &[addr],
                Some(&format!("timed out after {}s", handshake_timeout.as_secs())),
            );
            trace.fail(ConnectStepId::Handshake, msg.clone());
            bail!("{msg}");
        }
    };
    trace.ok(ConnectStepId::Handshake, format!("connected to {addr}"));

    match crate::tls::peer_fingerprint(&conn) {
        Some(fp) if fp.eq_ignore_ascii_case(&target.fingerprint) => {
            trace.ok(
                ConnectStepId::Cert,
                format!("fingerprint matches ({})", short_fingerprint(&fp)),
            );
        }
        Some(fp) => {
            let msg = format!(
                "presenter fingerprint mismatch: expected {}, got {}",
                short_fingerprint(&target.fingerprint),
                short_fingerprint(&fp)
            );
            trace.fail(ConnectStepId::Cert, msg.clone());
            bail!("{msg}");
        }
        None => {
            let msg = "presenter sent no certificate".to_owned();
            trace.fail(ConnectStepId::Cert, msg.clone());
            bail!("{msg}");
        }
    }

    trace.start(ConnectStepId::Control, "opening control stream");
    let (mut send, mut recv) = conn.open_bi().await?;
    trace.ok(ConnectStepId::Control, "control stream open");

    let pairing_note = if target.pairing_code.as_ref().is_some_and(|c| !c.is_empty()) {
        "sending pairing code"
    } else {
        "no pairing code (already paired, or pairing is off)"
    };
    trace.start(ConnectStepId::Pairing, pairing_note);
    write_msg(
        &mut send,
        &Control::Hello {
            protocol: PROTOCOL_VERSION,
            name: viewer_name.to_owned(),
            pairing_code: target.pairing_code.clone(),
            caps: vec![CAP_CURSOR_METADATA.to_owned(), CAP_FEC.to_owned()],
        },
    )
    .await?;
    let stream_info = match tokio::time::timeout(handshake_timeout, read_msg(&mut recv)).await {
        Ok(Ok(Control::StreamInfo(info))) => {
            trace.ok(ConnectStepId::Pairing, "accepted");
            info
        }
        Ok(Ok(Control::Rejected { reason })) => {
            let msg = format!("presenter rejected connection: {reason}");
            trace.fail(ConnectStepId::Pairing, reason);
            bail!("{msg}");
        }
        Ok(Ok(other)) => {
            let msg = format!("unexpected reply {other:?}");
            trace.fail(ConnectStepId::Pairing, msg.clone());
            bail!("{msg}");
        }
        Ok(Err(e)) => {
            trace.fail(ConnectStepId::Pairing, format!("{e:#}"));
            return Err(e);
        }
        Err(_) => {
            let msg = no_answer_message(
                &[addr],
                Some("timed out waiting for pairing / stream info"),
            );
            trace.fail(ConnectStepId::Pairing, msg.clone());
            bail!("{msg}");
        }
    };

    let clock = Arc::new(Mutex::new(ClockSync::default()));
    let (control, control_rx) = mpsc::unbounded_channel();
    let (info_tx, stream_updates) = watch::channel(stream_info.clone());
    tokio::spawn(control_loop(send, recv, control_rx, clock.clone(), info_tx));
    let cursor = Arc::new(Mutex::new(ViewerCursor::default()));
    tokio::spawn(accept_cursor_images(conn.clone(), cursor.clone()));
    Ok(ViewerSession {
        conn,
        stream_info,
        stream_updates,
        cursor,
        clock,
        control,
        _endpoint: endpoint,
    })
}

/// Cursor images arrive one per unidirectional stream (bounded size).
async fn accept_cursor_images(conn: Connection, cursor: Arc<Mutex<ViewerCursor>>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let cursor = cursor.clone();
        tokio::spawn(async move {
            match stream.read_to_end(MAX_IMAGE_STREAM).await {
                Ok(bytes) => match CursorImage::decode(&bytes) {
                    Ok(img) => cursor.lock().unwrap_or_else(|e| e.into_inner()).add_image(img),
                    Err(e) => tracing::debug!("dropping cursor image: {e:#}"),
                },
                Err(e) => tracing::debug!("cursor image stream failed: {e}"),
            }
        });
    }
}

async fn control_loop(
    mut send: SendStream,
    mut recv: RecvStream,
    mut outgoing: mpsc::UnboundedReceiver<Control>,
    clock: Arc<Mutex<ClockSync>>,
    stream_info: watch::Sender<StreamInfo>,
) {
    let reader = async {
        loop {
            match read_msg(&mut recv).await {
                Ok(Control::Pong { t_viewer_us, t_presenter_us }) => {
                    clock.lock().expect("clock lock").add_sample(t_viewer_us, t_presenter_us, now_us());
                }
                Ok(Control::StreamInfo(info)) => {
                    tracing::info!("presenter updated stream info: cursor {:?}, {} fps", info.cursor, info.fps);
                    stream_info.send_replace(info);
                }
                Ok(other) => tracing::debug!("ignoring {other:?}"),
                Err(e) => {
                    tracing::debug!("control stream closed: {e:#}");
                    return;
                }
            }
        }
    };
    let writer = async {
        let mut ping = tokio::time::interval(Duration::from_millis(250));
        loop {
            let msg = tokio::select! {
                _ = ping.tick() => Control::Ping { t_viewer_us: now_us() },
                msg = outgoing.recv() => match msg {
                    Some(m) => m,
                    None => return,
                },
            };
            if write_msg(&mut send, &msg).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::pairing::ConnectInfo;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn connect_timeout_reports_firewall_hint() {
        let identity = Identity::generate().unwrap();
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        drop(sock);
        let target = ConnectInfo {
            addr,
            addrs: Vec::new(),
            fingerprint: identity.fingerprint(),
            pairing_code: None,
            name: Some("timeout-test".into()),
        };
        let started = std::time::Instant::now();
        let result = ViewerSession::connect_traced(
            &identity,
            &target,
            "tester",
            &ConnectTrace::new(),
            Duration::from_secs(2),
        )
        .await;
        let err = match result {
            Ok(_) => panic!("expected connect to time out"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout too long: {:?}",
            started.elapsed()
        );
        assert!(msg.contains(&format!("{addr} (UDP)")), "{msg}");
        assert!(msg.contains("firewall-cmd"), "{msg}");
        assert!(msg.contains("QUIC"), "{msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cursor_reaches_a_capable_viewer() {
        use mirrorwire_proto::cursor::PointerTool;
        let dir = std::env::temp_dir().join(format!("mirrorwire-cursor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let presenter_id = Identity::generate().unwrap();
        let (cursor_tx, cursor_rx) = mpsc::channel(16);
        let seen_caps = Arc::new(Mutex::new(None));
        let caps_slot = seen_caps.clone();
        let presenter = Presenter::start(
            &presenter_id,
            PresenterConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                stream_info: StreamInfo {
                    width: 64,
                    height: 64,
                    fps: 60,
                    codec: "h264".into(),
                    encoder: "test".into(),
                    bitrate_kbps: 1,
                    intra_refresh: false,
                    cursor: Some("separate".into()),
                    fec: None,
                },
                pairing_code: None,
                config_dir: dir.clone(),
                attempts: Default::default(),
                on_viewer: Some(Arc::new(move |ok| *caps_slot.lock().unwrap() = Some(ok))),
                cursor_events: Some(cursor_rx),
                fec: FecControl::new(FecMode::Auto),
            },
            Arc::new(|| {}),
        )
        .unwrap();
        // Sent before anyone connects: cached and replayed on connect.
        let image = CursorImage { id: 5, width: 2, height: 2, hotspot_x: 1, hotspot_y: 1, rgba: vec![255; 16] };
        cursor_tx.send(CursorEvent::Image(image.clone())).await.unwrap();
        let pointer = PointerUpdate {
            seq: 0,
            tool: PointerTool::Cursor,
            visible: true,
            x: 10,
            y: 20,
            frame_w: 64,
            frame_h: 64,
            image_id: 5,
            capture_us: 1,
        };
        cursor_tx.send(CursorEvent::Pointer(pointer)).await.unwrap();

        let target = ConnectInfo {
            addr: presenter.local_addr,
            addrs: Vec::new(),
            fingerprint: presenter_id.fingerprint(),
            pairing_code: None,
            name: None,
        };
        let viewer = ViewerSession::connect(&Identity::generate().unwrap(), &target, "test").await.unwrap();
        assert_eq!(viewer.cursor_mode(), "separate");
        let deadline = Instant::now() + Duration::from_secs(5);
        while viewer.cursor.lock().unwrap().current().is_none() {
            assert!(Instant::now() < deadline, "cursor never arrived");
            if let Ok(Ok(d)) = tokio::time::timeout(Duration::from_millis(50), viewer.conn.read_datagram()).await {
                assert!(viewer.handle_pointer_datagram(&d));
            }
        }
        let (p, img) = viewer.cursor.lock().unwrap().current().unwrap();
        assert_eq!((p.x, p.y, p.image_id), (10, 20, 5));
        assert_eq!(*img, image);
        assert_eq!(*seen_caps.lock().unwrap(), Some(true));

        // Video goes out as FEC packets to a viewer that lists the cap, and
        // its link report reaches the presenter's FEC state.
        assert_eq!(viewer.presenter_fec().as_deref(), Some("auto"));
        let payload = Bytes::from(vec![7u8; 20_000]);
        presenter
            .frames
            .send(OutgoingFrame { frame_id: 1, keyframe: true, data: payload.clone(), capture_us: 1, encoded_us: 2 })
            .await
            .unwrap();
        let mut reassembler = mirrorwire_proto::fragment::Reassembler::new(u64::MAX);
        let mut first_dropped = false;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = viewer.conn.read_datagram().await.unwrap();
                if viewer.handle_pointer_datagram(&d) {
                    continue;
                }
                assert_eq!(d[0], mirrorwire_proto::fec::FEC_WIRE_VERSION);
                if !first_dropped {
                    first_dropped = true; // lose one packet: parity must cover it
                    continue;
                }
                if let Some((f, ev)) = reassembler.push(d, 0) {
                    return (f, ev);
                }
            }
        })
        .await
        .expect("FEC frame");
        assert_eq!(frame.0.data, payload);
        assert_eq!(frame.1.recovered, 1);
        assert!(presenter.fec.snapshot().viewer_fec);
        viewer.report_link(&LinkStats { expected: 1000, received: 950, recovered: 40, ..Default::default() });
        let deadline = Instant::now() + Duration::from_secs(5);
        while presenter.fec.snapshot().reports == 0 {
            assert!(Instant::now() < deadline, "link report never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fec = presenter.fec.snapshot();
        assert!((fec.loss_pct - 5.0).abs() < 0.01, "{}", fec.loss_pct);
        assert_eq!(fec.overheads(), Some((0.25, 0.5)));
        viewer.report_link(&LinkStats { expected: 1000, received: 950, unrecoverable_frames: 1, ..Default::default() });
        while presenter.fec.snapshot().reports < 2 {
            assert!(Instant::now() < deadline, "second link report never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fec = presenter.fec.snapshot();
        assert_eq!((fec.boost_pct, fec.unrecoverable), (5.0, 1));
        assert_eq!(fec.overheads(), Some((0.30, 0.575)));
        presenter.set_fec_mode(FecMode::Off);
        assert_eq!(presenter.fec.snapshot().overheads(), None);

        // A changed cursor mode reaches the connected viewer.
        presenter.update_stream_info(|i| i.cursor = Some("embedded".into()));
        let mut updates = viewer.stream_updates.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while updates.borrow_and_update().cursor.as_deref() != Some("embedded")
                || updates.borrow().fec.as_deref() != Some("off")
            {
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("stream_info update");
        viewer.close();
        presenter.close();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backlog_drops_only_delta_frames() {
        assert!(!should_drop_for_backlog(false, 50_000, DATAGRAM_SEND_BUFFER));
        assert!(should_drop_for_backlog(false, 50_000, 40_000));
        assert!(!should_drop_for_backlog(true, 500_000, 0), "keyframes always go out");
    }

    #[test]
    fn no_answer_message_is_stable() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 201, 132)), 47_800);
        let msg = no_answer_message(&[addr], None);
        assert!(msg.contains("192.168.201.132:47800 (UDP)"));
        assert!(msg.contains("sudo firewall-cmd --add-port=47800/udp"));
    }
}

//! One viewer session: QUIC (mirrorwire-net), fragment reassembly, keyframe
//! recovery, and a decode thread. A compact copy of the desktop viewer loop
//! (`mirrorwire-app/src/viewer.rs`) without FFmpeg.

use crate::codec::{DecodedImage, Decoder};
use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use mirrorwire_net::diag::{ConnectStepId, ConnectTrace, HANDSHAKE_TIMEOUT};
use mirrorwire_net::identity::Identity;
use mirrorwire_net::pairing::ConnectInfo;
use mirrorwire_net::transport::ViewerSession;
use mirrorwire_proto::clock::now_us;
use mirrorwire_proto::control::{Control, StreamInfo};
use mirrorwire_proto::fragment::{AssembledFrame, Reassembler};
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub decode_ms: f64,
    pub lost: u64,
    pub first_frame: bool,
}

pub struct Handle {
    pub status: Arc<Mutex<Status>>,
    pub frames: Receiver<DecodedImage>,
    pub recycle: Sender<Vec<u8>>,
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
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Handle {
    let status = Arc::new(Mutex::new(Status::default()));
    let (frames_tx, frames) = bounded::<DecodedImage>(1);
    let (recycle, recycle_rx) = bounded::<Vec<u8>>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = Handle { status: status.clone(), frames: frames.clone(), recycle, stop: stop.clone() };
    runtime.spawn(async move {
        let result = run(identity, target, name, status.clone(), frames_tx, frames, recycle_rx, stop, repaint.clone()).await;
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

#[allow(clippy::too_many_arguments)]
async fn run(
    identity: Arc<Identity>,
    target: ConnectInfo,
    name: String,
    status: Arc<Mutex<Status>>,
    frames_tx: Sender<DecodedImage>,
    frames_drain: Receiver<DecodedImage>,
    recycle_rx: Receiver<Vec<u8>>,
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
    }
    repaint();

    let need_keyframe = Arc::new(AtomicBool::new(true));
    let (decode_tx, decode_rx) = bounded::<AssembledFrame>(4);
    let control = session.control_sender();
    let dec_status = status.clone();
    let dec_need_key = need_keyframe.clone();
    let dec_trace = trace.clone();
    let dec_repaint = repaint.clone();
    let decode_thread = std::thread::Builder::new().name("decode".into()).spawn(move || {
        decode_loop(
            decode_rx, info, dec_status, frames_tx, frames_drain, recycle_rx, dec_need_key, control, dec_trace,
            dec_repaint,
        )
    })?;

    let self_healing = session.stream_info.intra_refresh;
    let mut reassembler = Reassembler::new(500_000);
    let mut got_datagram = false;
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let mut lost = 0u64;
    let mut window = (Instant::now(), 0u64, 0u64);
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
            trace.ok(ConnectStepId::FirstDatagram, format!("{} byte datagram", datagram.len()));
        }
        let Some((frame, event)) = reassembler.push(datagram, now_us()) else { continue };
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
                if last_request.elapsed() > Duration::from_millis(100) {
                    session.request_keyframe("recovering from loss");
                    last_request = Instant::now();
                }
                continue;
            }
            need_keyframe.store(false, Ordering::Release);
        }
        match decode_tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => need_keyframe.store(true, Ordering::Release),
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

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    rx: Receiver<AssembledFrame>,
    info: StreamInfo,
    status: Arc<Mutex<Status>>,
    frames_tx: Sender<DecodedImage>,
    frames_drain: Receiver<DecodedImage>,
    recycle_rx: Receiver<Vec<u8>>,
    need_keyframe: Arc<AtomicBool>,
    control: tokio::sync::mpsc::UnboundedSender<Control>,
    trace: ConnectTrace,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Result<()> {
    let mut decoder = Decoder::new(info.width, info.height);
    let mut first = true;
    while let Ok(frame) = rx.recv() {
        if let Ok(buf) = recycle_rx.try_recv() {
            decoder.recycle(buf);
        }
        let t0 = now_us();
        match decoder.decode(&frame.data, frame.keyframe, frame.completed_us) {
            Ok(Some(img)) => {
                let decode_ms = (now_us().saturating_sub(t0)) as f64 / 1000.0;
                {
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                    s.decode_ms = decode_ms;
                    s.decoder = decoder.name().to_owned();
                    s.first_frame = true;
                }
                if first {
                    first = false;
                    trace.ok(ConnectStepId::FirstFrame, format!("{}x{} via {}", img.width, img.height, decoder.name()));
                }
                // Newest picture wins: drop an undisplayed one.
                if let Err(TrySendError::Full(img)) = frames_tx.try_send(img) {
                    let _ = frames_drain.try_recv();
                    let _ = frames_tx.try_send(img);
                }
                repaint();
            }
            Ok(None) => {
                status.lock().unwrap_or_else(|e| e.into_inner()).decoder = decoder.name().to_owned();
            }
            Err(e) => {
                tracing::warn!("decode error: {e:#}");
                need_keyframe.store(true, Ordering::Release);
                let _ = control.send(Control::RequestKeyframe { reason: "decode error".into() });
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.decoder = format!("{} (error: {e:#})", decoder.name());
            }
        }
    }
    Ok(())
}

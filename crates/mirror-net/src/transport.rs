//! QUIC transport. One reliable bidirectional stream carries control messages
//! (hello, pairing, keyframe requests, clock pings); video goes out as
//! unreliable datagrams so a lost packet never stalls later frames.

use crate::identity::Identity;
use crate::pairing::{ConnectInfo, PairedViewers};
use crate::tls;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use mirror_proto::clock::{now_us, ClockSync};
use mirror_proto::control::{Control, StreamInfo};
use mirror_proto::fragment::{fragment, FrameTiming};
use mirror_proto::stats::StageStats;
use mirror_proto::PROTOCOL_VERSION;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.datagram_receive_buffer_size(Some(16 << 20));
    t.datagram_send_buffer_size(4 << 20);
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("valid idle timeout")));
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
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
}

type Callback = Arc<dyn Fn() + Send + Sync>;

pub struct Presenter {
    pub frames: mpsc::Sender<OutgoingFrame>,
    pub local_addr: SocketAddr,
    pub viewer_connected: watch::Receiver<bool>,
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
        let (current_tx, current_rx) = watch::channel::<Option<Connection>>(None);
        let (connected_tx, viewer_connected) = watch::channel(false);
        let current_tx = Arc::new(current_tx);

        tokio::spawn(send_loop(frames_rx, current_rx));

        let shared = Arc::new(SessionShared {
            stream_info: cfg.stream_info,
            pairing_code: cfg.pairing_code,
            paired: Mutex::new(PairedViewers::load(&cfg.config_dir)),
            current: current_tx,
            connected: connected_tx,
            on_keyframe_request,
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

        Ok(Self { frames, local_addr, viewer_connected, endpoint })
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"presenter stopped");
    }
}

struct SessionShared {
    stream_info: StreamInfo,
    pairing_code: Option<String>,
    paired: Mutex<PairedViewers>,
    current: Arc<watch::Sender<Option<Connection>>>,
    connected: watch::Sender<bool>,
    on_keyframe_request: Callback,
}

async fn handle_viewer(incoming: quinn::Incoming, shared: Arc<SessionShared>) -> Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    let fp = tls::peer_fingerprint(&conn).ok_or_else(|| anyhow!("viewer sent no certificate"))?;
    let (mut send, mut recv) = conn.accept_bi().await?;

    let Control::Hello { protocol, name, pairing_code } = read_msg(&mut recv).await? else {
        bail!("expected hello from {remote}");
    };
    if protocol != PROTOCOL_VERSION {
        write_msg(&mut send, &Control::Rejected { reason: format!("protocol {protocol} unsupported") }).await?;
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
        write_msg(&mut send, &Control::Rejected { reason: "not paired: pairing code required".into() }).await?;
        send.finish()?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        bail!("rejected unpaired viewer {name} at {remote}");
    }

    write_msg(&mut send, &Control::StreamInfo(shared.stream_info.clone())).await?;
    tracing::info!("viewer {name} connected from {remote}");
    if let Some(old) = shared.current.send_replace(Some(conn.clone())) {
        old.close(1u32.into(), b"replaced by another viewer");
    }
    shared.connected.send_replace(true);
    (shared.on_keyframe_request)();

    let result = async {
        loop {
            let msg = match read_msg(&mut recv).await {
                Ok(m) => m,
                Err(_) if conn.close_reason().is_some() => return Ok(()),
                Err(e) => return Err(e),
            };
            match msg {
                Control::Ping { t_viewer_us } => {
                    write_msg(&mut send, &Control::Pong { t_viewer_us, t_presenter_us: now_us() }).await?;
                }
                Control::RequestKeyframe { reason } => {
                    tracing::debug!("keyframe requested: {reason}");
                    (shared.on_keyframe_request)();
                }
                Control::Bye => return Ok::<_, anyhow::Error>(()),
                other => tracing::debug!("ignoring {other:?}"),
            }
        }
    }
    .await;

    shared.current.send_if_modified(|cur| {
        if cur.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id()) {
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

async fn send_loop(mut frames: mpsc::Receiver<OutgoingFrame>, current: watch::Receiver<Option<Connection>>) {
    let mut stats = StageStats::new(&["capture_to_encoded", "encoded_to_sent", "send"]);
    let mut window_start = Instant::now();
    let (mut window_frames, mut window_bytes, mut window_keyframes) = (0u32, 0usize, 0u32);

    while let Some(frame) = frames.recv().await {
        let Some(conn) = current.borrow().clone() else { continue };
        let Some(max) = conn.max_datagram_size() else { continue };
        let sent_us = now_us();
        let timing = FrameTiming {
            capture_us: frame.capture_us,
            encoded_us: frame.encoded_us,
            sent_us,
        };
        for datagram in fragment(frame.frame_id, frame.keyframe, timing, &frame.data, max) {
            if let Err(e) = conn.send_datagram_wait(datagram).await {
                tracing::debug!("datagram send failed: {e}");
                break;
            }
        }
        let done_us = now_us();
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
                "presenter: {:.1} fps, {:.2} Mbit/s, {} keyframes, mtu {}, ms mean/p95/max:{}",
                window_frames as f64 / secs,
                window_bytes as f64 * 8.0 / secs / 1e6,
                window_keyframes,
                max,
                stats.report()
            );
            stats.clear();
            window_start = Instant::now();
            (window_frames, window_bytes, window_keyframes) = (0, 0, 0);
        }
    }
}

pub fn ms(later_us: u64, earlier_us: u64) -> f64 {
    (later_us as i64 - earlier_us as i64) as f64 / 1000.0
}

pub struct ViewerSession {
    pub conn: Connection,
    pub stream_info: StreamInfo,
    pub clock: Arc<Mutex<ClockSync>>,
    control: mpsc::UnboundedSender<Control>,
    _endpoint: Endpoint,
}

impl ViewerSession {
    pub async fn connect(identity: &Identity, target: &ConnectInfo, viewer_name: &str) -> Result<Self> {
        let tls = tls::client_config(identity, &target.fingerprint)?;
        let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
        client.transport_config(transport_config());
        let bind: SocketAddr = if target.addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse()?;
        let mut endpoint = Endpoint::client(bind)?;
        endpoint.set_default_client_config(client);

        let conn = endpoint
            .connect(target.addr, crate::identity::CERT_SERVER_NAME)?
            .await
            .with_context(|| format!("connecting to {}", target.addr))?;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_msg(
            &mut send,
            &Control::Hello {
                protocol: PROTOCOL_VERSION,
                name: viewer_name.to_owned(),
                pairing_code: target.pairing_code.clone(),
            },
        )
        .await?;
        let stream_info = match read_msg(&mut recv).await? {
            Control::StreamInfo(info) => info,
            Control::Rejected { reason } => bail!("presenter rejected connection: {reason}"),
            other => bail!("unexpected reply {other:?}"),
        };

        let clock = Arc::new(Mutex::new(ClockSync::default()));
        let (control, control_rx) = mpsc::unbounded_channel();
        tokio::spawn(control_loop(send, recv, control_rx, clock.clone()));
        Ok(Self { conn, stream_info, clock, control, _endpoint: endpoint })
    }

    pub fn request_keyframe(&self, reason: &str) {
        let _ = self.control.send(Control::RequestKeyframe { reason: reason.to_owned() });
    }

    pub fn control_sender(&self) -> mpsc::UnboundedSender<Control> {
        self.control.clone()
    }

    pub fn close(&self) {
        let _ = self.control.send(Control::Bye);
        self.conn.close(0u32.into(), b"viewer closed");
    }
}

async fn control_loop(
    mut send: SendStream,
    mut recv: RecvStream,
    mut outgoing: mpsc::UnboundedReceiver<Control>,
    clock: Arc<Mutex<ClockSync>>,
) {
    let reader = async {
        loop {
            match read_msg(&mut recv).await {
                Ok(Control::Pong { t_viewer_us, t_presenter_us }) => {
                    clock.lock().expect("clock lock").add_sample(t_viewer_us, t_presenter_us, now_us());
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

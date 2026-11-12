//! Screen capture through the xdg-desktop-portal ScreenCast interface and PipeWire.
//! Needs no special privileges and works inside a Flatpak sandbox. The portal
//! restore token is persisted so the share dialog is shown only once.
//!
//! Status: compiles, not yet tested against a real KDE Plasma session.

use super::FrameSource;
use crate::frame::{PixelFormat, RawFrame};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use mirrorwire_proto::clock::now_us;
use pipewire as pw;
use pw::spa;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub struct PortalOptions {
    pub restore_token_path: PathBuf,
    pub show_cursor: bool,
    pub allow_virtual: bool,
    pub fps: u32,
}

struct Negotiated {
    fd: OwnedFd,
    node_id: u32,
    size: Option<(i32, i32)>,
}

pub struct PortalSource {
    frames: Receiver<RawFrame>,
    pool: Sender<Vec<u8>>,
    description: String,
    stop_portal: Option<tokio::sync::oneshot::Sender<()>>,
    stop_pw: pw::channel::Sender<()>,
    threads: Vec<JoinHandle<()>>,
    finished: bool,
}

impl PortalSource {
    pub fn open(options: PortalOptions) -> Result<Self> {
        let (negotiated_tx, negotiated_rx) = bounded::<Result<Negotiated>>(1);
        let (stop_portal, stop_portal_rx) = tokio::sync::oneshot::channel::<()>();
        let portal_opts = options.clone();
        let portal_thread = std::thread::Builder::new()
            .name("portal".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = negotiated_tx.send(Err(e.into()));
                        return;
                    }
                };
                rt.block_on(async move {
                    match start_session(&portal_opts).await {
                        Ok((proxy, session, negotiated)) => {
                            let _ = negotiated_tx.send(Ok(negotiated));
                            let _ = stop_portal_rx.await;
                            let _ = session.close().await;
                            drop(proxy);
                        }
                        Err(e) => {
                            let _ = negotiated_tx.send(Err(e));
                        }
                    }
                });
            })?;

        let negotiated = negotiated_rx
            .recv()
            .map_err(|_| anyhow!("portal thread exited"))??;
        let description = format!(
            "portal screencast node {} {:?}",
            negotiated.node_id, negotiated.size
        );

        let (frames_tx, frames) = bounded::<RawFrame>(2);
        let (pool, pool_rx) = bounded::<Vec<u8>>(4);
        let (stop_pw, stop_pw_rx) = pw::channel::channel::<()>();
        let fps = options.fps;
        let pw_thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                if let Err(e) = run_pipewire(negotiated, fps, frames_tx, pool_rx, stop_pw_rx) {
                    tracing::error!("pipewire capture failed: {e:#}");
                }
            })?;

        Ok(Self {
            frames,
            pool,
            description,
            stop_portal: Some(stop_portal),
            stop_pw,
            threads: vec![portal_thread, pw_thread],
            finished: false,
        })
    }
}

impl FrameSource for PortalSource {
    fn next_frame(&mut self) -> Result<Option<RawFrame>> {
        Ok(self.frames.recv().ok())
    }

    fn poll_frame(&mut self, timeout: std::time::Duration) -> Result<Option<RawFrame>> {
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                self.finished = true;
                Ok(None)
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn recycle(&mut self, buffer: Vec<u8>) {
        let _ = self.pool.try_send(buffer);
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

impl Drop for PortalSource {
    fn drop(&mut self) {
        let _ = self.stop_pw.send(());
        if let Some(stop) = self.stop_portal.take() {
            let _ = stop.send(());
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

async fn start_session(
    opts: &PortalOptions,
) -> Result<(
    Screencast,
    ashpd::desktop::Session<Screencast>,
    Negotiated,
)> {
    let proxy = Screencast::new().await.context("connecting to ScreenCast portal")?;
    let session = proxy.create_session(Default::default()).await?;
    let restore_token = std::fs::read_to_string(&opts.restore_token_path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());

    let mut sources = SourceType::MONITOR;
    if opts.allow_virtual && proxy.available_source_types().await?.contains(SourceType::VIRTUAL) {
        sources |= SourceType::VIRTUAL;
    }
    let cursor = if opts.show_cursor { CursorMode::EMBEDDED } else { CursorMode::HIDDEN };
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(sources)
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore_token.as_deref()),
        )
        .await?
        .response()?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .context("screen share was cancelled or denied")?;

    if let Some(token) = streams.restore_token() {
        if let Some(dir) = opts.restore_token_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&opts.restore_token_path, token)?;
    }

    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no streams"))?;
    let negotiated = Negotiated {
        node_id: stream.pipe_wire_node_id(),
        size: stream.size(),
        fd: proxy.open_pipe_wire_remote(&session, Default::default()).await?,
    };
    Ok((proxy, session, negotiated))
}

struct StreamState {
    format: spa::param::video::VideoInfoRaw,
    seq: u64,
    frames: Sender<RawFrame>,
    pool: Receiver<Vec<u8>>,
}

fn map_format(f: spa::param::video::VideoFormat) -> Option<PixelFormat> {
    use spa::param::video::VideoFormat as V;
    match f {
        V::BGRA => Some(PixelFormat::Bgra),
        V::BGRx => Some(PixelFormat::Bgrx),
        V::RGBA => Some(PixelFormat::Rgba),
        V::RGBx => Some(PixelFormat::Rgbx),
        _ => None,
    }
}

fn run_pipewire(
    negotiated: Negotiated,
    fps: u32,
    frames: Sender<RawFrame>,
    pool: Receiver<Vec<u8>>,
    stop: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(negotiated.fd, None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "mirrorwire-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = StreamState {
        format: Default::default(),
        seq: 0,
        frames,
        pool,
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Err(e) = state.format.parse(param) {
                tracing::warn!("unparseable stream format: {e:?}");
                return;
            }
            let size = state.format.size();
            tracing::info!(
                "pipewire format {:?} {}x{}",
                state.format.format(),
                size.width,
                size.height
            );
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(format) = map_format(state.format.format()) else { return };
            let size = state.format.size();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let chunk = data.chunk();
            let (offset, len, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride().max(0) as usize,
            );
            let Some(mem) = data.data() else { return };
            if len == 0 || offset + len > mem.len() {
                return;
            }
            let capture_us = now_us();
            let mut out = state.pool.try_recv().unwrap_or_default();
            out.clear();
            out.extend_from_slice(&mem[offset..offset + len]);
            let frame = RawFrame {
                seq: state.seq,
                width: size.width,
                height: size.height,
                stride: if stride == 0 { size.width as usize * 4 } else { stride },
                format,
                data: out,
                capture_us,
            };
            state.seq += 1;
            if let Err(TrySendError::Full(_)) = state.frames.try_send(frame) {
                tracing::trace!("encoder busy, dropped captured frame");
            }
        })
        .register()?;

    let format_pod = build_format_pod(fps)?;
    let mut params = [spa::pod::Pod::from_bytes(&format_pod).ok_or_else(|| anyhow!("bad format pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(negotiated.node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let quit = mainloop.clone();
    let _stop = stop.attach(mainloop.loop_(), move |_| quit.quit());
    mainloop.run();
    Ok(())
}

fn build_format_pod(fps: u32) -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::utils::{Fraction, Rectangle, SpaTypes};
    let obj = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
    );
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing format pod: {e:?}"))?
    .0
    .into_inner();
    if bytes.is_empty() {
        bail!("empty format pod");
    }
    Ok(bytes)
}

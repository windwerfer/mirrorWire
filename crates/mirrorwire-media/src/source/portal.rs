//! Screen capture through the xdg-desktop-portal ScreenCast interface and PipeWire.
//! Needs no special privileges and works inside a Flatpak sandbox. The portal
//! restore token is persisted so the share dialog is shown only once.
//!
//! When `dmabuf` is enabled the stream first offers DMA-BUF with explicit
//! modifiers, so the compositor hands over GPU buffers that the VAAPI encoder
//! imports without any CPU copy. Shared memory (MemFd/MemPtr) stays offered
//! as the second choice, and the encoder can ask for a renegotiation to shared
//! memory at runtime (`FrameSource::disable_dmabuf`).

use super::FrameSource;
use crate::frame::{DmaBufFrame, PixelFormat, RawFrame, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use mirrorwire_proto::clock::now_us;
use pipewire as pw;
use pw::spa;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub struct PortalOptions {
    pub restore_token_path: PathBuf,
    pub show_cursor: bool,
    pub allow_virtual: bool,
    pub fps: u32,
    /// Offer DMA-BUF (zero-copy) before shared memory. Only useful when the
    /// encoder can import DMA-BUFs (VAAPI); x264 needs shared memory.
    pub dmabuf: bool,
}

/// Modifiers we offer for DMA-BUF. Only single-plane layouts: VAAPI's
/// DRM-PRIME import in FFmpeg takes one buffer object per frame, so the
/// Intel CCS layouts (which add an aux plane) are left out. `INVALID` lets
/// the compositor fall back to an implicit (driver-chosen) layout.
const I915_FORMAT_MOD_X_TILED: u64 = (1 << 56) | 1;
const I915_FORMAT_MOD_Y_TILED: u64 = (1 << 56) | 2;
const OFFERED_MODIFIERS: [u64; 4] = [
    I915_FORMAT_MOD_Y_TILED,
    I915_FORMAT_MOD_X_TILED,
    DRM_FORMAT_MOD_LINEAR,
    DRM_FORMAT_MOD_INVALID,
];

/// State shared between the PipeWire thread and the encoder side.
#[derive(Default)]
struct Shared {
    /// Set by the encoder side when it cannot use DMA-BUF frames.
    dmabuf_disabled: AtomicBool,
    /// Why DMA-BUF was turned off, for the log and status.
    dmabuf_reason: Mutex<Option<String>>,
    /// Human-readable negotiated buffer type, e.g. "DMA-BUF modifier 0x…".
    negotiated: Mutex<String>,
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
    shared: Arc<Shared>,
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
        let dmabuf = options.dmabuf;
        let shared = Arc::new(Shared::default());
        let pw_shared = shared.clone();
        let pw_thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                let cfg = PwConfig { fps, dmabuf, shared: pw_shared };
                if let Err(e) = run_pipewire(negotiated, cfg, frames_tx, pool_rx, stop_pw_rx) {
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
            shared,
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

    fn disable_dmabuf(&mut self, reason: &str) {
        if !self.shared.dmabuf_disabled.swap(true, Ordering::AcqRel) {
            tracing::warn!("DMA-BUF capture turned off ({reason}); renegotiating shared memory");
            *self.shared.dmabuf_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.to_owned());
        }
    }

    fn describe(&self) -> String {
        let negotiated = self.shared.negotiated.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = self.description.clone();
        if !negotiated.is_empty() {
            out.push_str(", ");
            out.push_str(&negotiated);
        }
        if let Some(reason) = self.shared.dmabuf_reason.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            out.push_str(&format!(" (DMA-BUF off: {reason})"));
        }
        out
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
    let restore_token = tokio::fs::read_to_string(&opts.restore_token_path)
        .await
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
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&opts.restore_token_path, token).await?;
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

struct PwConfig {
    fps: u32,
    dmabuf: bool,
    shared: Arc<Shared>,
}

struct StreamState {
    format: spa::param::video::VideoInfoRaw,
    seq: u64,
    frames: Sender<RawFrame>,
    pool: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
    /// The negotiated format carries a modifier: buffers are DMA-BUFs.
    is_dmabuf: bool,
    /// Shared-memory-only format pod, sent when DMA-BUF gets turned off.
    shm_pod: Vec<u8>,
    renegotiated: bool,
    dropped_busy: u64,
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

fn set_negotiated(shared: &Shared, text: String) {
    *shared.negotiated.lock().unwrap_or_else(|e| e.into_inner()) = text;
}

fn run_pipewire(
    negotiated: Negotiated,
    cfg: PwConfig,
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

    let shm_pod = build_format_pod(cfg.fps, None)?;
    let state = StreamState {
        format: Default::default(),
        seq: 0,
        frames,
        pool,
        shared: cfg.shared.clone(),
        is_dmabuf: false,
        shm_pod: shm_pod.clone(),
        renegotiated: false,
        dropped_busy: 0,
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|stream, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Err(e) = state.format.parse(param) {
                tracing::warn!("unparseable stream format: {e:?}");
                return;
            }
            let flags = state.format.flags().bits();
            let has_modifier = flags & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            let size = state.format.size();
            let fr = state.format.framerate();
            let max_fr = state.format.max_framerate();
            if has_modifier && flags & spa::sys::SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0 {
                // The compositor still has to pick one modifier; it sends a
                // fixed format next.
                tracing::info!("pipewire: waiting for the compositor to fix the DMA-BUF modifier");
                return;
            }
            state.is_dmabuf = has_modifier;
            let kind = if has_modifier {
                format!("DMA-BUF modifier {:#x}", state.format.modifier())
            } else {
                "shared memory".to_owned()
            };
            tracing::info!(
                "pipewire format {:?} {}x{} framerate {}/{} max {}/{} buffers {kind}",
                state.format.format(),
                size.width,
                size.height,
                fr.num,
                fr.denom,
                max_fr.num,
                max_fr.denom,
            );
            set_negotiated(&state.shared, kind);
            match build_buffers_pod(has_modifier) {
                Ok(pod) => {
                    if let Some(pod) = spa::pod::Pod::from_bytes(&pod) {
                        if let Err(e) = stream.update_params(&mut [pod]) {
                            tracing::warn!("pipewire: setting buffer params failed: {e}");
                        }
                    }
                }
                Err(e) => tracing::warn!("pipewire: buffer params: {e:#}"),
            }
        })
        .process(|stream, state| {
            if state.is_dmabuf && state.shared.dmabuf_disabled.load(Ordering::Acquire) {
                if !state.renegotiated {
                    state.renegotiated = true;
                    if let Some(pod) = spa::pod::Pod::from_bytes(&state.shm_pod) {
                        match stream.update_params(&mut [pod]) {
                            Ok(()) => tracing::info!("pipewire: asked for shared-memory buffers"),
                            Err(e) => tracing::warn!("pipewire: renegotiation failed: {e}"),
                        }
                    }
                }
                // Drain without using DMA-BUF frames until the new format lands.
                let _ = stream.dequeue_buffer();
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(format) = map_format(state.format.format()) else { return };
            let size = state.format.size();
            let datas = buffer.datas_mut();
            let n_planes = datas.len();
            let Some(data) = datas.first_mut() else { return };
            let chunk = data.chunk();
            if (chunk.flags().bits() as u32) & spa::sys::SPA_CHUNK_FLAG_CORRUPTED != 0 {
                return;
            }
            let (offset, len, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride().max(0) as usize,
            );
            let capture_us = now_us();
            let stride = if stride == 0 { size.width as usize * 4 } else { stride };

            let frame = if data.type_() == spa::buffer::DataType::DmaBuf {
                if n_planes != 1 {
                    let reason = format!("{n_planes}-plane DMA-BUF is not supported");
                    state.shared.dmabuf_disabled.store(true, Ordering::Release);
                    *state.shared.dmabuf_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                    return;
                }
                let raw_fd = data.fd();
                if raw_fd < 0 {
                    return;
                }
                // SAFETY: PipeWire owns this fd for as long as the buffer is
                // mapped into our stream; we only borrow it to duplicate it.
                let fd = match unsafe { BorrowedFd::borrow_raw(raw_fd) }.try_clone_to_owned() {
                    Ok(fd) => fd,
                    Err(e) => {
                        tracing::warn!("dup of DMA-BUF fd failed: {e}");
                        return;
                    }
                };
                // The buffer goes back to the compositor when `buffer` drops
                // at the end of this callback. The compositor cycles through
                // its pool (several buffers), and the encoder imports the
                // frame within a few milliseconds, long before this buffer
                // comes round again.
                RawFrame {
                    seq: state.seq,
                    width: size.width,
                    height: size.height,
                    stride,
                    format,
                    data: Vec::new(),
                    capture_us,
                    dmabuf: Some(DmaBufFrame {
                        fd,
                        size: data.as_raw().maxsize as usize,
                        offset: offset as u32,
                        pitch: stride as u32,
                        modifier: state.format.modifier(),
                    }),
                }
            } else {
                let Some(mem) = data.data() else { return };
                if len == 0 || offset.checked_add(len).is_none_or(|end| end > mem.len()) {
                    return;
                }
                let mut out = state.pool.try_recv().unwrap_or_default();
                out.clear();
                out.extend_from_slice(&mem[offset..offset + len]);
                RawFrame {
                    seq: state.seq,
                    width: size.width,
                    height: size.height,
                    stride,
                    format,
                    data: out,
                    capture_us,
                    dmabuf: None,
                }
            };
            state.seq += 1;
            if let Err(TrySendError::Full(_)) = state.frames.try_send(frame) {
                state.dropped_busy += 1;
                tracing::trace!("encoder busy, dropped captured frame ({} so far)", state.dropped_busy);
            }
        })
        .register()?;

    // Offer DMA-BUF first (when enabled), shared memory second.
    let dmabuf_pod = if cfg.dmabuf {
        Some(build_format_pod(cfg.fps, Some(&OFFERED_MODIFIERS))?)
    } else {
        None
    };
    let mut params: Vec<&spa::pod::Pod> = Vec::with_capacity(2);
    if let Some(bytes) = &dmabuf_pod {
        params.push(spa::pod::Pod::from_bytes(bytes).ok_or_else(|| anyhow!("bad DMA-BUF format pod"))?);
    }
    params.push(spa::pod::Pod::from_bytes(&shm_pod).ok_or_else(|| anyhow!("bad format pod"))?);
    tracing::info!(
        "pipewire: offering {} at up to {} fps",
        if cfg.dmabuf { "DMA-BUF (zero-copy) then shared memory" } else { "shared memory only" },
        cfg.fps
    );
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

fn serialize_object(obj: spa::pod::Object) -> Result<Vec<u8>> {
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing pod: {e:?}"))?
    .0
    .into_inner();
    if bytes.is_empty() {
        bail!("empty pod");
    }
    Ok(bytes)
}

/// EnumFormat for raw 32-bit RGB. With `modifiers` set, the format also
/// carries a mandatory modifier list (DMA-BUF); without it, shared memory.
/// Both ask for up to `fps` (KWin uses `maxFramerate` to pace damage frames).
fn build_format_pod(fps: u32, modifiers: Option<&[u64]>) -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
    let fps = fps.clamp(1, 240);
    let mut obj = spa::pod::object!(
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
        // Variable rate (damage-driven) is fine; 0/1 means "as it comes".
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
        spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    );
    if let Some(mods) = modifiers {
        let first = *mods.first().ok_or_else(|| anyhow!("empty modifier list"))?;
        obj.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::from_bits_retain(
                spa::sys::SPA_POD_PROP_FLAG_MANDATORY | spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE,
            ),
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: first as i64,
                    alternatives: mods.iter().map(|m| *m as i64).collect(),
                },
            ))),
        });
    }
    serialize_object(obj)
}

/// Buffers param: which memory types we accept for the negotiated format.
fn build_buffers_pod(dmabuf: bool) -> Result<Vec<u8>> {
    use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, SpaTypes};
    let types: i32 = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    let obj = spa::pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range { default: 8, min: 4, max: 16 },
                ))),
            },
            Property {
                key: spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags { default: types, flags: Vec::new() },
                ))),
            },
        ],
    };
    serialize_object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pods_serialize() {
        let shm = build_format_pod(60, None).unwrap();
        let dma = build_format_pod(60, Some(&OFFERED_MODIFIERS)).unwrap();
        assert!(dma.len() > shm.len());
        assert!(spa::pod::Pod::from_bytes(&shm).is_some());
        assert!(spa::pod::Pod::from_bytes(&dma).is_some());
        assert!(spa::pod::Pod::from_bytes(&build_buffers_pod(true).unwrap()).is_some());
        assert!(spa::pod::Pod::from_bytes(&build_buffers_pod(false).unwrap()).is_some());
    }
}

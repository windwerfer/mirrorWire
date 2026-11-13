use crate::applog::LogStore;
use crate::presenter::{self, PresentPhase, PresenterHandle};
use crate::viewer::{self, viewer_ui, Picture, ViewerHandle, ViewerOptions, ViewerUi};
use crate::yuv_glow::{YuvPaint, YuvRenderer};
use crate::{GuiLaunch, RendererChoice, ViewArgs};
use anyhow::{anyhow, Result};
use eframe::egui;
use mirrorwire_media::encode::{EncoderBackend, EncoderConfig, RateControl, DEFAULT_BITRATE_KBPS, DEFAULT_QP};
use mirrorwire_net::diag::{ConnectStep, FirewallHint, IncomingAttempt, StepState};
use mirrorwire_net::discovery::{Browser, Presenter};
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::ConnectInfo;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PRESENT_AVAILABLE: bool = cfg!(all(target_os = "linux", feature = "portal"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedRenderer {
    Glow,
    Wgpu,
}

impl ResolvedRenderer {
    fn label(self) -> &'static str {
        match self {
            Self::Glow => "OpenGL (glow)",
            Self::Wgpu => "wgpu",
        }
    }

    fn eframe(self) -> eframe::Renderer {
        match self {
            Self::Glow => eframe::Renderer::Glow,
            Self::Wgpu => eframe::Renderer::Wgpu,
        }
    }
}

enum Screen {
    Home,
    Present,
    View,
}

pub fn run(
    launch: GuiLaunch,
    args: ViewArgs,
    config_dir: &Path,
    renderer: RendererChoice,
    log: LogStore,
    present_port: u16,
) -> Result<()> {
    let identity = Arc::new(Identity::load_or_create(config_dir)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    let initial = args.uri.as_deref().map(ConnectInfo::parse).transpose()?;
    let name = args.name.clone().unwrap_or_else(crate::default_name);
    let config_dir = config_dir.to_path_buf();
    let start_in_view = matches!(launch, GuiLaunch::View);

    let preferred = match renderer {
        RendererChoice::Gl => ResolvedRenderer::Glow,
        RendererChoice::Wgpu => ResolvedRenderer::Wgpu,
        RendererChoice::Auto => ResolvedRenderer::Wgpu,
    };
    let allow_fallback = !matches!(renderer, RendererChoice::Gl);

    tracing::info!(
        "GUI renderer preference: {:?} (YUV→RGB on the CPU via swscale; OpenGL default on macOS x86_64)",
        renderer
    );

    let make_app = {
        let identity = identity.clone();
        let config_dir = config_dir.clone();
        let handle = handle.clone();
        let name = name.clone();
        let initial = initial.clone();
        let log = log.clone();
        let hwdec = args.want_hwdec();
        move |used: ResolvedRenderer| {
            let identity = identity.clone();
            let config_dir = config_dir.clone();
            let handle = handle.clone();
            let name = name.clone();
            let initial = initial.clone();
            let log = log.clone();
            Box::new(move |cc: &eframe::CreationContext<'_>| {
                let mut app = MirrorApp {
                    identity,
                    runtime: handle,
                    config_dir,
                    browser: Browser::start()
                        .map_err(|e| tracing::warn!("mDNS browse unavailable: {e:#}"))
                        .ok(),
                    hwdec,
                    name,
                    uri_input: String::new(),
                    code_input: String::new(),
                    error: None,
                    watching: None,
                    presenting: None,
                    texture: None,
                    qr_texture: None,
                    show_stats: true,
                    fullscreen: false,
                    display_latency_ms: 0.0,
                    screen: if start_in_view {
                        Screen::View
                    } else {
                        Screen::Home
                    },
                    renderer: used,
                    can_go_home: !start_in_view,
                    log,
                    last_target: None,
                    present_port,
                    pixel_exact: false,
                    yuv: None,
                    yuv_gpu: Arc::new(Mutex::new(None)),
                    copy_note: None,
                    display_server: None,
                    bitrate_mbps: DEFAULT_BITRATE_KBPS as f64 / 1000.0,
                    rc: RateControl::Auto,
                    qp: DEFAULT_QP,
                    keyint_sec: 10,
                    encoder: EncoderBackend::Auto,
                };
                if let Some(target) = initial {
                    app.screen = Screen::View;
                    app.connect(&cc.egui_ctx, target);
                }
                Ok(Box::new(app) as Box<dyn eframe::App>)
            }) as eframe::AppCreator<'_>
        }
    };

    let result = try_run_native(preferred, make_app(preferred));
    let result = match result {
        Err(e) if allow_fallback && preferred == ResolvedRenderer::Wgpu => {
            tracing::warn!("wgpu renderer failed ({e}); falling back to OpenGL (glow)");
            try_run_native(ResolvedRenderer::Glow, make_app(ResolvedRenderer::Glow))
        }
        other => other,
    };

    drop(runtime);
    result
}

fn try_run_native(renderer: ResolvedRenderer, creator: eframe::AppCreator<'_>) -> Result<()> {
    tracing::info!("starting GUI with {}", renderer.label());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MirrorWire")
            .with_inner_size([1280.0, 720.0]),
        renderer: renderer.eframe(),
        ..Default::default()
    };
    eframe::run_native("MirrorWire", options, creator).map_err(|e| anyhow!("gui: {e}"))
}

struct MirrorApp {
    identity: Arc<Identity>,
    runtime: tokio::runtime::Handle,
    config_dir: PathBuf,
    browser: Option<Browser>,
    hwdec: bool,
    name: String,
    uri_input: String,
    code_input: String,
    error: Option<String>,
    watching: Option<ViewerHandle>,
    presenting: Option<PresenterHandle>,
    texture: Option<egui::TextureHandle>,
    qr_texture: Option<egui::TextureHandle>,
    show_stats: bool,
    fullscreen: bool,
    display_latency_ms: f64,
    screen: Screen,
    renderer: ResolvedRenderer,
    can_go_home: bool,
    log: LogStore,
    last_target: Option<ConnectInfo>,
    present_port: u16,
    pixel_exact: bool,
    yuv: Option<Arc<mirrorwire_media::decode::YuvImage>>,
    yuv_gpu: Arc<Mutex<Option<YuvRenderer>>>,
    copy_note: Option<(Instant, bool, String)>,
    /// "Wayland (native)", "X11 (XWayland)", "macOS"…, read from the real window handle.
    display_server: Option<String>,
    bitrate_mbps: f64,
    rc: RateControl,
    qp: u32,
    keyint_sec: u32,
    encoder: EncoderBackend,
}

impl MirrorApp {
    fn connect(&mut self, ctx: &egui::Context, target: ConnectInfo) {
        let repaint = ctx.clone();
        self.error = None;
        self.texture = None;
        self.yuv = None;
        self.last_target = Some(target.clone());
        self.watching = Some(viewer::spawn(
            &self.runtime,
            self.identity.clone(),
            target,
            ViewerOptions {
                hwdec: self.hwdec,
                simulate_loss: 0.0,
                rgba: self.renderer != ResolvedRenderer::Glow,
                name: self.name.clone(),
            },
            Some(Arc::new(move || repaint.request_repaint())),
        ));
    }

    fn disconnect(&mut self, ctx: &egui::Context) {
        if let Some(h) = self.watching.take() {
            h.stop();
        }
        self.texture = None;
        self.yuv = None;
        if self.fullscreen {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    fn start_presenting(&mut self, ctx: &egui::Context) {
        self.error = None;
        self.qr_texture = None;
        let repaint = ctx.clone();
        self.presenting = Some(presenter::spawn_for_gui(
            self.runtime.clone(),
            self.config_dir.clone(),
            (*self.identity).clone(),
            self.name.clone(),
            self.present_port,
            Some(Arc::new(move || repaint.request_repaint())),
        ));
        self.screen = Screen::Present;
    }

    fn stop_presenting(&mut self) {
        if let Some(h) = self.presenting.take() {
            h.stop();
        }
        self.qr_texture = None;
    }

    fn go_home(&mut self, ctx: &egui::Context) {
        self.disconnect(ctx);
        self.stop_presenting();
        self.error = None;
        self.screen = Screen::Home;
    }

    fn footer(&self, ui: &mut egui::Ui) {
        ui.separator();
        ui.weak(format!(
            "Renderer: {}  ·  Window system: {}",
            self.renderer.label(),
            self.display_server.as_deref().unwrap_or("?")
        ));
        ui.weak(format!("Log: {}", self.log.path().display()));
    }

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("MirrorWire");
            ui.add_space(8.0);
            ui.label("Share this screen, or view a presenter on the network.");
            ui.add_space(24.0);
            let present = ui.add_enabled(
                PRESENT_AVAILABLE,
                egui::Button::new("Present this screen").min_size(egui::vec2(240.0, 36.0)),
            );
            if present.clicked() {
                self.start_presenting(ui.ctx());
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.weak("UDP port:");
                ui.add(egui::DragValue::new(&mut self.present_port).range(1..=65535));
            });
            if !PRESENT_AVAILABLE {
                ui.add_space(4.0);
                ui.weak("Screen capture is not available on this platform yet.");
            }
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new("View").min_size(egui::vec2(240.0, 36.0)))
                .clicked()
            {
                self.error = None;
                self.screen = Screen::View;
            }
            if let Some(err) = &self.error {
                ui.add_space(16.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
        });
        ui.add_space(24.0);
        self.footer(ui);
    }

    fn present_ui(&mut self, ui: &mut egui::Ui) {
        let Some(handle) = &self.presenting else {
            self.screen = Screen::Home;
            return;
        };
        let status = handle.poll();

        if ui.button("Stop presenting").clicked() {
            self.stop_presenting();
            self.screen = Screen::Home;
            return;
        }

        ui.add_space(8.0);
        ui.heading(format!("Presenting: {}", status.name));
        ui.add_space(8.0);

        match status.phase {
            PresentPhase::Starting | PresentPhase::WaitingForCapture => {
                ui.label("Waiting for the screen share dialog…");
                ui.weak("Choose the display or window to share, then come back to this window for the pairing code.");
            }
            PresentPhase::Streaming => {
                if let Some(code) = &status.pairing_code {
                    ui.label("Pairing code (first connection only):");
                    ui.label(egui::RichText::new(code).monospace().size(42.0).strong());
                } else {
                    ui.label("Pairing is off; any viewer can connect.");
                }
                ui.add_space(8.0);
                if let Some(uri) = &status.uri {
                    if self.qr_texture.is_none() {
                        match qr_color_image(uri) {
                            Ok(image) => {
                                self.qr_texture = Some(ui.ctx().load_texture(
                                    "presenter-qr",
                                    image,
                                    egui::TextureOptions::NEAREST,
                                ));
                            }
                            Err(e) => self.error = Some(format!("QR code: {e:#}")),
                        }
                    }
                    if let Some(tex) = &self.qr_texture {
                        let side = 240.0;
                        ui.image((tex.id(), egui::vec2(side, side)));
                    }
                    ui.add_space(4.0);
                    ui.monospace(uri);
                    if ui.button("Copy link").clicked() {
                        copy_with_feedback(ui.ctx(), uri.clone(), &mut self.copy_note);
                    }
                    show_copy_note(ui, &self.copy_note);
                }
                ui.add_space(8.0);
                if status.viewer_connected {
                    ui.colored_label(
                        egui::Color32::from_rgb(0, 180, 80),
                        "A viewer is connected.",
                    );
                } else {
                    ui.label("Waiting for a viewer…");
                }
                if let Some(addr) = &status.listen_addr {
                    ui.weak(format!("Listening on {addr}"));
                }
                if let (Some(enc), Some(src)) = (&status.encoder, &status.source) {
                    // The HUD name follows live encoder switches; `status.encoder` is the first one.
                    let enc = if status.hud.encoder.is_empty() { enc } else { &status.hud.encoder };
                    ui.weak(format!("Encoder: {enc}  ·  Source: {src}"));
                }
                if let Some(fp) = &status.fingerprint {
                    ui.weak(format!("Fingerprint: {}", short_fingerprint(fp)));
                }
                ui.weak(format!("UDP port {}", status.port));
                if !status.advertised.is_empty() {
                    ui.weak(format!("Advertised: {}", status.advertised.join(", ")));
                }
                if let Some(sum) = &status.rc_summary {
                    ui.weak(sum);
                }
                let hud = &status.hud;
                let path = if hud.path.is_empty() {
                    status.conversion.as_deref().unwrap_or("?")
                } else {
                    hud.path.as_str()
                };
                let zero_copy = path == mirrorwire_media::encode::path::DMABUF_ZERO_COPY;
                ui.horizontal(|ui| {
                    ui.weak("Capture path:");
                    if zero_copy {
                        ui.colored_label(egui::Color32::from_rgb(0, 180, 80), path);
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(230, 160, 40), path);
                    }
                });
                if let Some(note) = &hud.fallback_note {
                    ui.colored_label(egui::Color32::from_rgb(230, 160, 40), note);
                }
                if hud.capture_w > 0 {
                    ui.weak(format!(
                        "Capture {}x{}  →  encode {}x{}  ·  source {:.1} fps (+{:.1} repeats/s)  ·  map/pack {:.1} ms  convert {:.1} ms  upload {:.1} ms  encode {:.1} ms (avg {:.1} ms)",
                        hud.capture_w,
                        hud.capture_h,
                        status.settings.width,
                        status.settings.height,
                        hud.source_fps,
                        hud.repeat_fps,
                        hud.pack_ms,
                        hud.convert_ms,
                        hud.upload_ms,
                        hud.encode_ms,
                        hud.encode_avg_ms
                    ));
                }
                quality_controls(ui, self, &status.settings);
                firewall_panel(ui, status.firewall.as_ref(), &mut self.copy_note);
                attempts_panel(ui, &status.attempts);
                log_panel(ui, &self.log, &mut self.copy_note);
            }
            PresentPhase::Failed => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    status.error.as_deref().unwrap_or("presenter failed"),
                );
            }
            PresentPhase::Stopped => {
                ui.label("Presenter stopped.");
            }
        }

        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        self.footer(ui);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }

    fn browse_ui(&mut self, ui: &mut egui::Ui) {
        if self.can_go_home && ui.button("Back").clicked() {
            self.go_home(ui.ctx());
            return;
        }
        ui.heading("MirrorWire viewer");
        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        ui.separator();
        ui.label("Presenters on this network:");
        let found: Vec<Presenter> = self
            .browser
            .as_mut()
            .map(|b| b.poll().to_vec())
            .unwrap_or_default();
        if found.is_empty() {
            ui.weak("searching…");
        }
        ui.horizontal(|ui| {
            ui.label("Pairing code (first connection only):");
            ui.add(egui::TextEdit::singleline(&mut self.code_input).desired_width(80.0));
        });
        let mut chosen = None;
        for p in &found {
            ui.horizontal(|ui| {
                if ui.button("Connect").clicked() {
                    chosen = Some(ConnectInfo {
                        addr: p.addr,
                        addrs: p.addrs.clone(),
                        fingerprint: p.fingerprint.clone(),
                        pairing_code: Some(self.code_input.trim().to_owned())
                            .filter(|c| !c.is_empty()),
                        name: Some(p.name.clone()),
                    });
                }
                let addrs = if p.addrs.len() > 1 {
                    p.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                } else {
                    p.addr.to_string()
                };
                ui.label(format!(
                    "{}  {}  {}  [{}]",
                    p.name,
                    addrs,
                    p.resolution.as_deref().unwrap_or(""),
                    short_fingerprint(&p.fingerprint)
                ));
            });
        }
        ui.separator();
        ui.label("Or paste the mirrorwire:// link from the presenter's QR code:");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.uri_input).desired_width(500.0));
            if ui.button("Connect").clicked() {
                match ConnectInfo::parse(&self.uri_input) {
                    Ok(t) => chosen = Some(t),
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
        });
        ui.checkbox(
            &mut self.hwdec,
            "Hardware decoding (VideoToolbox / VAAPI, software fallback)",
        );
        ui.checkbox(&mut self.pixel_exact, "1:1 pixel-exact display (no scale)");
        ui.weak("On macOS, VideoToolbox is the default. An older GPU (HD 6770M on OCLP) may not decode H.264; the viewer then uses a tuned software decoder and shows which one is active.");
        if let Some(target) = chosen {
            self.connect(ui.ctx(), target);
        }
        log_panel(ui, &self.log, &mut self.copy_note);
        self.footer(ui);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }

    fn video_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(handle) = &self.watching else { return };
        let mut latest = None;
        while let Ok(f) = handle.frames.try_recv() {
            latest = Some(f);
        }
        if let Some(f) = latest {
            self.display_latency_ms = mirrorwire_net::transport::ms(
                mirrorwire_proto::clock::now_us(),
                f.capture_us_viewer,
            );
            match f.picture {
                Picture::Rgba(rgba) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize],
                        &rgba,
                    );
                    let filter = if self.pixel_exact {
                        egui::TextureOptions::NEAREST
                    } else {
                        egui::TextureOptions::LINEAR
                    };
                    match &mut self.texture {
                        Some(tex) => tex.set(image, filter),
                        None => self.texture = Some(ctx.load_texture("video", image, filter)),
                    }
                    self.yuv = None;
                }
                Picture::Yuv(img) => {
                    self.yuv = Some(Arc::new(img));
                    self.texture = None;
                }
            }
        }
        let status = handle.status.lock().expect("status lock").clone();

        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        let ppp = ui.ctx().pixels_per_point();
        let mut scale_info = String::new();
        let (tex_w, tex_h) = if let Some(tex) = &self.texture {
            (tex.size()[0] as f32, tex.size()[1] as f32)
        } else if let Some(yuv) = &self.yuv {
            (yuv.width as f32, yuv.height as f32)
        } else {
            (0.0, 0.0)
        };
        if tex_w > 0.0 && tex_h > 0.0 {
            let native_pts = egui::vec2(tex_w / ppp, tex_h / ppp);
            let draw_size = if self.pixel_exact {
                native_pts
            } else {
                let fit = (rect.width() / native_pts.x).min(rect.height() / native_pts.y);
                native_pts * fit
            };
            let draw = egui::Rect::from_center_size(rect.center(), draw_size);
            if let Some(yuv) = &self.yuv {
                ui.painter().add(YuvPaint::new(
                    yuv.clone(),
                    self.yuv_gpu.clone(),
                    self.pixel_exact,
                ).callback(draw));
            } else if let Some(tex) = &self.texture {
                ui.painter().image(
                    tex.id(),
                    draw,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            let drawn_px = draw_size * ppp;
            let sx = drawn_px.x / tex_w;
            let sy = drawn_px.y / tex_h;
            let convert = if self.yuv.is_some() { "GL YUV shader" } else { "CPU RGBA" };
            scale_info = format!(
                "texture {tex_w:.0}x{tex_h:.0}  window {:.0}x{:.0} px  ppp {ppp:.2}  scale {sx:.2}x/{sy:.2}x  {}  {convert}",
                rect.width() * ppp,
                rect.height() * ppp,
                if self.pixel_exact { "1:1 nearest" } else { "fit linear" },
            );
        }
        if self.show_stats {
            let stream = status.stream.as_ref();
            let text = format!(
                "{}x{}  source {:.1} fps  {:.2} Mbit/s  {} ({})\n{}  decode {:.2} ms  {}  capture->texture {:.1} ms\nwindow: {}\n{}\n{}\nS: stats  1: 1:1 pixels  F / double-click: fullscreen  Esc: disconnect",
                status.native_width.max(stream.map(|s| s.width).unwrap_or(0)),
                status.native_height.max(stream.map(|s| s.height).unwrap_or(0)),
                status.source_fps,
                status.bitrate_mbps,
                stream.map(|s| s.codec.as_str()).unwrap_or("h264"),
                stream.map(|s| s.encoder.as_str()).unwrap_or("?"),
                status.decoder,
                status.decode_ms,
                self.renderer.label(),
                self.display_latency_ms,
                self.display_server.as_deref().unwrap_or("?"),
                status.last_second,
                scale_info,
            );
            ui.painter().text(
                rect.left_top() + egui::vec2(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                text,
                egui::FontId::monospace(12.0),
                egui::Color32::from_rgb(0, 255, 120),
            );
        }

        let response = ui.interact(rect, ui.scope_id().with("video"), egui::Sense::click());
        let (toggle_fs, toggle_stats, toggle_1to1, leave) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::F) || response.double_clicked(),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Num1),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if toggle_stats {
            self.show_stats = !self.show_stats;
        }
        if toggle_1to1 {
            self.pixel_exact = !self.pixel_exact;
        }
        if toggle_fs {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        let failed = status.error.clone();
        if leave || failed.is_some() && self.has_picture() && !status.connected {
            self.error = failed;
            self.disconnect(&ctx);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }

    fn has_picture(&self) -> bool {
        self.texture.is_some() || self.yuv.is_some()
    }

    fn should_show_video(&self) -> bool {
        let Some(handle) = &self.watching else {
            return false;
        };
        let status = handle.status.lock().expect("status lock");
        viewer_ui(status.connected, status.first_frame, self.has_picture()) == ViewerUi::Video
    }

    fn connecting_ui(&mut self, ui: &mut egui::Ui) {
        let handle = match &self.watching {
            Some(h) => h,
            None => return,
        };
        let status = handle.status.lock().expect("status lock").clone();
        if ui.button("Back").clicked() {
            self.disconnect(ui.ctx());
            return;
        }
        ui.add_space(8.0);
        ui.heading("Connecting");
        if let Some(t) = &self.last_target {
            ui.monospace(format!(
                "Target: {}  {}",
                t.name.clone().unwrap_or_default(),
                t.addr
            ));
        }
        ui.add_space(8.0);
        let progress = status.trace.snapshot();
        for step in &progress.steps {
            step_row(ui, step);
        }
        if let Some(err) = progress.error.as_ref().or(status.error.as_ref()) {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::LIGHT_RED, err);
            if err.contains("firewall-cmd") {
                ui.horizontal(|ui| {
                    if ui.button("Copy firewall command").clicked() {
                        let text = err
                            .split("On Fedora:")
                            .nth(1)
                            .map(|s| s.trim().to_owned())
                            .unwrap_or_else(|| err.clone());
                        copy_with_feedback(ui.ctx(), text, &mut self.copy_note);
                    }
                    show_copy_note(ui, &self.copy_note);
                });
            }
        } else {
            ui.add_space(8.0);
            ui.label("connecting…");
            if !status.decoder.is_empty() {
                ui.weak(format!("Decoder: {}", status.decoder));
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Retry").clicked() {
                if let Some(target) = self.last_target.clone() {
                    self.disconnect(ui.ctx());
                    self.connect(ui.ctx(), target);
                }
            }
            if ui.button("Cancel").clicked() {
                self.disconnect(ui.ctx());
            }
        });
        log_panel(ui, &self.log, &mut self.copy_note);
        self.footer(ui);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
}

impl eframe::App for MirrorApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.display_server.is_none() {
            let label = crate::session::window_system(frame);
            tracing::info!("window system: {label}");
            self.display_server = Some(label);
        }
        match self.screen {
            Screen::Home => {
                egui::CentralPanel::default().show(ui, |ui| self.home_ui(ui));
            }
            Screen::Present => {
                egui::CentralPanel::default().show(ui, |ui| self.present_ui(ui));
            }
            Screen::View if self.should_show_video() => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| self.video_ui(ui));
            }
            Screen::View if self.watching.is_some() => {
                egui::CentralPanel::default().show(ui, |ui| self.connecting_ui(ui));
            }
            Screen::View => {
                egui::CentralPanel::default().show(ui, |ui| self.browse_ui(ui));
            }
        }
    }
}

impl Drop for MirrorApp {
    fn drop(&mut self) {
        if let Some(h) = self.watching.take() {
            h.stop();
        }
        if let Some(h) = self.presenting.take() {
            h.stop();
        }
    }
}

fn qr_color_image(uri: &str) -> Result<egui::ColorImage> {
    let (n, modules) = mirrorwire_net::pairing::qr_matrix(uri)?;
    let quiet = 4;
    let side = n + quiet * 2;
    let mut rgba = vec![255u8; side * side * 4];
    for y in 0..n {
        for x in 0..n {
            if modules[y * n + x] {
                let i = ((y + quiet) * side + (x + quiet)) * 4;
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
            }
        }
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [side, side],
        &rgba,
    ))
}


fn step_row(ui: &mut egui::Ui, step: &ConnectStep) {
    let (mark, color) = match step.state {
        StepState::Pending => ("·", egui::Color32::GRAY),
        StepState::Active => ("…", egui::Color32::from_rgb(230, 180, 40)),
        StepState::Ok => ("✓", egui::Color32::from_rgb(0, 180, 80)),
        StepState::Failed => ("✗", egui::Color32::LIGHT_RED),
    };
    ui.horizontal(|ui| {
        ui.colored_label(color, mark);
        ui.monospace(step.at.clone().unwrap_or_else(|| "        ".into()));
        ui.strong(step.id.label());
        if !step.detail.is_empty() {
            ui.label(&step.detail);
        }
    });
}

/// Copy through egui's own clipboard (winit: smithay-clipboard on Wayland,
/// the system pasteboard on macOS). It reports no errors back, so the note
/// keeps the manual fallback in view.
fn copy_with_feedback(ctx: &egui::Context, text: String, note: &mut Option<(Instant, bool, String)>) {
    ctx.copy_text(text);
    *note = Some((
        Instant::now(),
        true,
        "Copied. If pasting doesn't work, the text is selectable — use Cmd/Ctrl+C.".into(),
    ));
}

fn show_copy_note(ui: &mut egui::Ui, note: &Option<(Instant, bool, String)>) {
    let Some((at, ok, msg)) = note else { return };
    if at.elapsed().as_secs() >= 4 {
        return;
    }
    let color = if *ok {
        egui::Color32::from_rgb(0, 180, 80)
    } else {
        egui::Color32::from_rgb(230, 160, 40)
    };
    ui.colored_label(color, msg);
}

fn log_panel(ui: &mut egui::Ui, log: &LogStore, note: &mut Option<(Instant, bool, String)>) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.weak(format!("Log: {}", log.path().display()));
        if ui.button("Copy log").clicked() {
            copy_with_feedback(ui.ctx(), log.snapshot(), note);
        }
        show_copy_note(ui, note);
    });
    if let Some(err) = log.open_error() {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
    let text = log.snapshot();
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(text).monospace().size(11.0))
                    .selectable(true)
                    .wrap(),
            );
        });
}

fn firewall_panel(
    ui: &mut egui::Ui,
    hint: Option<&FirewallHint>,
    note: &mut Option<(Instant, bool, String)>,
) {
    let Some(hint) = hint.filter(|h| h.show) else { return };
    ui.add_space(8.0);
    let color = if hint.prominent {
        egui::Color32::from_rgb(230, 160, 40)
    } else {
        egui::Color32::GRAY
    };
    ui.colored_label(color, &hint.message);
    ui.horizontal(|ui| {
        ui.monospace(&hint.command);
        if ui.button("Copy command").clicked() {
            copy_with_feedback(ui.ctx(), hint.command.clone(), note);
        }
        show_copy_note(ui, note);
    });
}

fn attempts_panel(ui: &mut egui::Ui, attempts: &[IncomingAttempt]) {
    ui.add_space(8.0);
    ui.strong("Connection attempts");
    if attempts.is_empty() {
        ui.weak("No connection attempts received yet");
        return;
    }
    for a in attempts.iter().rev().take(12) {
        let color = if a.ok {
            egui::Color32::from_rgb(0, 180, 80)
        } else {
            egui::Color32::LIGHT_RED
        };
        let name = a.viewer_name.as_deref().unwrap_or("unknown viewer");
        ui.colored_label(color, format!("{}  {}  {}  {}", a.at, a.peer, name, a.outcome));
    }
}

fn quality_controls(ui: &mut egui::Ui, app: &mut MirrorApp, live: &EncoderConfig) {
    ui.add_space(8.0);
    ui.strong("Quality");
    ui.horizontal(|ui| {
        ui.label("Encoder");
        egui::ComboBox::from_id_salt("encoder")
            .selected_text(app.encoder.as_str())
            .show_ui(ui, |ui| {
                for b in EncoderBackend::selectable() {
                    ui.selectable_value(&mut app.encoder, b, b.as_str());
                }
            });
        ui.label("Bitrate (Mbit/s)");
        ui.add(egui::DragValue::new(&mut app.bitrate_mbps).range(1.0..=80.0).speed(0.5));
        ui.label("RC");
        egui::ComboBox::from_id_salt("rc")
            .selected_text(app.rc.as_str())
            .show_ui(ui, |ui| {
                for rc in RateControl::all() {
                    ui.selectable_value(&mut app.rc, *rc, rc.as_str());
                }
            });
        if matches!(app.rc, RateControl::Cqp | RateControl::Icq) {
            ui.label("QP");
            ui.add(egui::DragValue::new(&mut app.qp).range(10..=40));
        }
        ui.label("Keyint (s)");
        ui.add(egui::DragValue::new(&mut app.keyint_sec).range(1..=60));
        if ui.button("Apply").clicked() {
            if let Some(h) = &app.presenting {
                let mut cfg = live.clone();
                cfg.bitrate_kbps = (app.bitrate_mbps * 1000.0).round().max(1.0) as u32;
                cfg.rate_control = app.rc;
                cfg.qp = app.qp;
                cfg.keyint = Some(app.keyint_sec.saturating_mul(cfg.fps.max(1)));
                cfg.backend = app.encoder;
                h.apply_settings(cfg);
            }
        }
    });
    ui.weak("Apply restarts the encoder with an IDR. filmcraft is the optional pure-Rust encoder (CPU, for comparison; auto never picks it). Switching away from vaapi uses shared-memory capture until the next start. 4:4:4 chroma is not available with VAAPI on UHD 620 or with VideoToolbox decode.");
}


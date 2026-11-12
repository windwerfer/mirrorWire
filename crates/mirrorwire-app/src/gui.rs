use crate::presenter::{self, PresentPhase, PresenterHandle};
use crate::viewer::{self, ViewerHandle, ViewerOptions};
use crate::{GuiLaunch, RendererChoice, ViewArgs};
use anyhow::{anyhow, Result};
use eframe::egui;
use mirrorwire_net::discovery::{Browser, Presenter};
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::ConnectInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        let hwdec = args.hwdec;
        move |used: ResolvedRenderer| {
            let identity = identity.clone();
            let config_dir = config_dir.clone();
            let handle = handle.clone();
            let name = name.clone();
            let initial = initial.clone();
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
}

impl MirrorApp {
    fn connect(&mut self, ctx: &egui::Context, target: ConnectInfo) {
        let repaint = ctx.clone();
        self.error = None;
        self.texture = None;
        self.watching = Some(viewer::spawn(
            &self.runtime,
            self.identity.clone(),
            target,
            ViewerOptions {
                hwdec: self.hwdec,
                simulate_loss: 0.0,
                rgba: true,
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
            "Renderer: {}  ·  video colour conversion: CPU (swscale YUV→RGBA)",
            self.renderer.label()
        ));
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
                        ui.ctx().copy_text(uri.clone());
                    }
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
                    ui.weak(format!("Encoder: {enc}  ·  Source: {src}"));
                }
                if let Some(fp) = &status.fingerprint {
                    ui.weak(format!("Fingerprint: {}", short_fingerprint(fp)));
                }
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
                        fingerprint: p.fingerprint.clone(),
                        pairing_code: Some(self.code_input.trim().to_owned())
                            .filter(|c| !c.is_empty()),
                        name: Some(p.name.clone()),
                    });
                }
                ui.label(format!(
                    "{}  {}  {}  [{}]",
                    p.name,
                    p.addr,
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
        ui.checkbox(&mut self.hwdec, "Hardware decoding");
        ui.weak("If VideoToolbox or VAAPI is missing, the viewer falls back to software H.264 and logs it.");
        if let Some(target) = chosen {
            self.connect(ui.ctx(), target);
        }
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
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [f.width as usize, f.height as usize],
                &f.rgba,
            );
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR))
                }
            }
        }
        let status = handle.status.lock().expect("status lock").clone();

        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        if let Some(tex) = &self.texture {
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let draw = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(
                tex.id(),
                draw,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            let msg = status.error.clone().unwrap_or_else(|| "connecting…".into());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(20.0),
                egui::Color32::GRAY,
            );
        }
        if self.show_stats {
            let text = format!(
                "{}  |  {}  |  {}  |  capture->texture {:.1} ms\n{}\nS: stats  F / double-click: fullscreen  Esc: disconnect",
                status
                    .stream
                    .as_ref()
                    .map(|s| format!("{}x{}@{} {} ({})", s.width, s.height, s.fps, s.codec, s.encoder))
                    .unwrap_or_default(),
                status.decoder,
                self.renderer.label(),
                self.display_latency_ms,
                status.last_second
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
        let (toggle_fs, toggle_stats, leave) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::F) || response.double_clicked(),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if toggle_stats {
            self.show_stats = !self.show_stats;
        }
        if toggle_fs {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        let failed = status.error.clone();
        if leave || failed.is_some() && self.texture.is_some() && !status.connected {
            self.error = failed;
            self.disconnect(&ctx);
        } else if let Some(err) = failed.filter(|_| self.texture.is_none()) {
            self.error = Some(err);
            self.watching = None;
        }
    }
}

impl eframe::App for MirrorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.screen {
            Screen::Home => {
                egui::CentralPanel::default().show(ui, |ui| self.home_ui(ui));
            }
            Screen::Present => {
                egui::CentralPanel::default().show(ui, |ui| self.present_ui(ui));
            }
            Screen::View if self.watching.is_some() => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| self.video_ui(ui));
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

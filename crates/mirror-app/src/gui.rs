use crate::viewer::{self, ViewerHandle, ViewerOptions};
use crate::ViewArgs;
use anyhow::{anyhow, Result};
use eframe::egui;
use mirror_net::discovery::{Browser, Presenter};
use mirror_net::identity::{short_fingerprint, Identity};
use mirror_net::pairing::ConnectInfo;
use std::path::Path;
use std::sync::Arc;

pub fn run(args: ViewArgs, config_dir: &Path) -> Result<()> {
    let identity = Arc::new(Identity::load_or_create(config_dir)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let initial = args.uri.as_deref().map(ConnectInfo::parse).transpose()?;
    let name = args.name.clone().unwrap_or_else(crate::default_name);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mirror")
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mirror",
        options,
        Box::new(move |cc| {
            let mut app = ViewerApp {
                identity,
                runtime,
                browser: Browser::start().map_err(|e| tracing::warn!("mDNS browse unavailable: {e:#}")).ok(),
                hwdec: args.hwdec,
                name,
                uri_input: String::new(),
                code_input: String::new(),
                error: None,
                watching: None,
                texture: None,
                show_stats: true,
                fullscreen: false,
                display_latency_ms: 0.0,
            };
            if let Some(target) = initial {
                app.connect(&cc.egui_ctx, target);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("gui: {e}"))
}

struct ViewerApp {
    identity: Arc<Identity>,
    runtime: tokio::runtime::Runtime,
    browser: Option<Browser>,
    hwdec: bool,
    name: String,
    uri_input: String,
    code_input: String,
    error: Option<String>,
    watching: Option<ViewerHandle>,
    texture: Option<egui::TextureHandle>,
    show_stats: bool,
    fullscreen: bool,
    display_latency_ms: f64,
}

impl ViewerApp {
    fn connect(&mut self, ctx: &egui::Context, target: ConnectInfo) {
        let repaint = ctx.clone();
        self.error = None;
        self.texture = None;
        self.watching = Some(viewer::spawn(
            self.runtime.handle(),
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

    fn browse_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("mirror viewer");
        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        ui.separator();
        ui.label("Presenters on this network:");
        let found: Vec<Presenter> = self.browser.as_mut().map(|b| b.poll().to_vec()).unwrap_or_default();
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
                        pairing_code: Some(self.code_input.trim().to_owned()).filter(|c| !c.is_empty()),
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
        ui.label("Or paste the mirror:// link from the presenter's QR code:");
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
        if let Some(target) = chosen {
            self.connect(ui.ctx(), target);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }

    fn video_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(handle) = &self.watching else { return };
        let mut latest = None;
        while let Ok(f) = handle.frames.try_recv() {
            latest = Some(f);
        }
        if let Some(f) = latest {
            self.display_latency_ms = mirror_net::transport::ms(mirror_proto::clock::now_us(), f.capture_us_viewer);
            let image = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
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
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(20.0), egui::Color32::GRAY);
        }
        if self.show_stats {
            let text = format!(
                "{}  |  {}  |  capture->texture {:.1} ms\n{}\nS: stats  F / double-click: fullscreen  Esc: disconnect",
                status
                    .stream
                    .as_ref()
                    .map(|s| format!("{}x{}@{} {} ({})", s.width, s.height, s.fps, s.codec, s.encoder))
                    .unwrap_or_default(),
                status.decoder,
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

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.watching.is_some() {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.video_ui(ui));
        } else {
            egui::CentralPanel::default().show(ui, |ui| self.browse_ui(ui));
        }
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        if let Some(h) = self.watching.take() {
            h.stop();
        }
    }
}

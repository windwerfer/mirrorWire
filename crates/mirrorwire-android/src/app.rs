//! egui UI for the Android viewer: discovered presenters, a pairing-code
//! keypad, `mirrorwire://` links (typed, or opened from the camera app's QR
//! scan via the Java shell), and the full-screen video view.

use crate::session::{self, Handle};
use eframe::egui;
use mirrorwire_net::discovery::Browser;
use mirrorwire_net::identity::{short_fingerprint, Identity};
use mirrorwire_net::pairing::ConnectInfo;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::platform::android::activity::AndroidApp;

/// Written by the Java shell when a `mirrorwire://` link opens the app.
const PENDING_URI_FILE: &str = "pending_uri.txt";

pub fn run(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("mirrorwire"),
    );
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp/mirrorwire"));
    tracing::info!("MirrorWire Android viewer {} starting, data {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
    let result = eframe::run_native(
        "MirrorWire",
        options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, data_dir)))),
    );
    if let Err(e) = result {
        tracing::error!("eframe: {e}");
    }
}

struct ViewerApp {
    runtime: tokio::runtime::Runtime,
    identity: Option<Arc<Identity>>,
    data_dir: PathBuf,
    browser: Option<Browser>,
    browse_error: Option<String>,
    uri_input: String,
    code: String,
    error: Option<String>,
    session: Option<Handle>,
    target_name: String,
    texture: Option<egui::TextureHandle>,
    picture_size: (u32, u32),
    show_overlay: bool,
    last_uri_check: Instant,
    /// Texture of the separately sent cursor, by image id.
    cursor_tex: Option<(u32, egui::TextureHandle)>,
}

impl ViewerApp {
    fn new(cc: &eframe::CreationContext<'_>, data_dir: PathBuf) -> Self {
        cc.egui_ctx.set_pixels_per_point(cc.egui_ctx.pixels_per_point().max(2.0));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let mut error = None;
        let identity = match Identity::load_or_create(&data_dir) {
            Ok(id) => Some(Arc::new(id)),
            Err(e) => {
                error = Some(format!("Could not create this device's certificate: {e:#}"));
                None
            }
        };
        let (browser, browse_error) = {
            let _guard = runtime.enter();
            match Browser::start() {
                Ok(b) => (Some(b), None),
                Err(e) => (None, Some(format!("Network discovery (mDNS) unavailable: {e:#}"))),
            }
        };
        Self {
            runtime,
            identity,
            data_dir,
            browser,
            browse_error,
            uri_input: String::new(),
            code: String::new(),
            error,
            session: None,
            target_name: String::new(),
            texture: None,
            picture_size: (0, 0),
            show_overlay: true,
            last_uri_check: Instant::now() - Duration::from_secs(1),
            cursor_tex: None,
        }
    }

    fn connect(&mut self, ctx: &egui::Context, mut target: ConnectInfo) {
        let Some(identity) = self.identity.clone() else { return };
        if target.pairing_code.is_none() && self.code.len() == 6 {
            target.pairing_code = Some(self.code.clone());
        }
        self.error = None;
        self.texture = None;
        self.cursor_tex = None;
        self.target_name = target.name.clone().unwrap_or_else(|| target.addr.to_string());
        let repaint = ctx.clone();
        // Written by the Java shell at start-up (MediaCodecList has no NDK API).
        let decoders = std::fs::read_to_string(self.data_dir.join(crate::codec_list::CODEC_LIST_FILE))
            .map(|t| crate::codec_list::parse(&t))
            .unwrap_or_default();
        tracing::info!(
            "H.264 decoders on this device: {}",
            decoders.iter().map(|d| format!("{}{}", d.name, if d.hardware { " (hw)" } else { "" })).collect::<Vec<_>>().join(", ")
        );
        self.session = Some(session::spawn(
            self.runtime.handle(),
            identity,
            target,
            "Android".into(),
            decoders,
            Arc::new(move || repaint.request_repaint()),
        ));
    }

    fn disconnect(&mut self) {
        if let Some(s) = self.session.take() {
            s.stop();
        }
        self.texture = None;
    }

    /// Links opened from outside (QR code scanned with the camera app).
    fn check_pending_uri(&mut self, ctx: &egui::Context) {
        if self.last_uri_check.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_uri_check = Instant::now();
        let path = self.data_dir.join(PENDING_URI_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let _ = std::fs::remove_file(&path);
        let uri = text.trim();
        if uri.is_empty() || uri.len() > 4096 {
            return;
        }
        match ConnectInfo::parse(uri) {
            Ok(info) => {
                self.disconnect();
                self.connect(ctx, info);
            }
            Err(e) => self.error = Some(format!("That link is not a MirrorWire link: {e:#}")),
        }
    }

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("MirrorWire viewer");
        ui.add_space(8.0);
        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
            ui.add_space(6.0);
        }

        ui.label("Pairing code (shown on the presenter):");
        ui.horizontal(|ui| {
            let shown: String = (0..6).map(|i| self.code.chars().nth(i).unwrap_or('_')).collect();
            ui.label(egui::RichText::new(shown).monospace().size(28.0));
        });
        keypad(ui, &mut self.code);
        ui.weak("Only needed the first time; the presenter then remembers this device.");
        ui.separator();

        ui.label("Presenters on this network:");
        let found: Vec<_> = self.browser.as_mut().map(|b| b.poll().to_vec()).unwrap_or_default();
        if let Some(e) = &self.browse_error {
            ui.colored_label(egui::Color32::from_rgb(230, 160, 40), e);
        } else if found.is_empty() {
            ui.weak("Searching… (phone and presenter must be on the same Wi-Fi)");
        }
        let mut pick = None;
        for p in &found {
            let label = format!("{}  ·  {}{}", p.name, p.addr, p.resolution.as_deref().map(|r| format!("  ·  {r}")).unwrap_or_default());
            if ui.add(egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 48.0))).clicked() {
                pick = Some(ConnectInfo {
                    addr: p.addr,
                    addrs: p.addrs.clone(),
                    fingerprint: p.fingerprint.clone(),
                    pairing_code: None,
                    name: Some(p.name.clone()),
                });
            }
        }
        if let Some(target) = pick {
            let ctx = ui.ctx().clone();
            self.connect(&ctx, target);
        }
        ui.separator();

        ui.label("Or a mirrorwire:// link:");
        ui.add(egui::TextEdit::singleline(&mut self.uri_input).desired_width(f32::INFINITY).hint_text("mirrorwire://…"));
        if ui.add(egui::Button::new("Connect to link").min_size(egui::vec2(200.0, 44.0))).clicked() {
            match ConnectInfo::parse(self.uri_input.trim()) {
                Ok(info) => {
                    let ctx = ui.ctx().clone();
                    self.connect(&ctx, info);
                }
                Err(e) => self.error = Some(format!("Not a MirrorWire link: {e:#}")),
            }
        }
        ui.weak("Tip: scan the presenter's QR code with the camera app; the link opens MirrorWire directly.");
        ui.separator();
        if let Some(id) = &self.identity {
            ui.weak(format!("This device: {}", short_fingerprint(&id.fingerprint())));
        }
        ui.weak(format!("MirrorWire {}  ·  logs: adb logcat -s mirrorwire", env!("CARGO_PKG_VERSION")));
    }

    fn connecting_ui(&mut self, ui: &mut egui::Ui, status: &session::Status) {
        ui.heading(format!("Connecting to {}…", self.target_name));
        let progress = status.trace.snapshot();
        for step in &progress.steps {
            let mark = match step.state {
                mirrorwire_net::diag::StepState::Pending => "·",
                mirrorwire_net::diag::StepState::Active => "…",
                mirrorwire_net::diag::StepState::Ok => "✓",
                mirrorwire_net::diag::StepState::Failed => "✗",
            };
            ui.label(format!("{mark} {}  {}", step.id.label(), step.detail));
        }
        if let Some(err) = &status.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        if let Some(advice) = &progress.advice {
            ui.weak(advice);
        }
        if ui.add(egui::Button::new("Back").min_size(egui::vec2(160.0, 44.0))).clicked() {
            if let Some(err) = status.error.clone() {
                self.error = Some(err);
            }
            self.disconnect();
        }
    }

    fn video_ui(&mut self, ui: &mut egui::Ui, status: &session::Status) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        if let Some(tex_id) = self.texture.as_ref().map(|t| t.id()) {
            let (w, h) = (self.picture_size.0.max(1) as f32, self.picture_size.1.max(1) as f32);
            let scale = (rect.width() / w).min(rect.height() / h);
            let size = egui::vec2(w * scale, h * scale);
            let img_rect = egui::Rect::from_center_size(rect.center(), size);
            ui.painter().image(tex_id, img_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            if status.cursor_mode == "separate" {
                let current = status.cursor.as_ref().and_then(|c| c.lock().unwrap_or_else(|e| e.into_inner()).current());
                if let Some((p, img)) = current {
                    self.draw_cursor(ui, &p, &img, img_rect);
                }
            }
        } else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Waiting for the first picture…", egui::FontId::proportional(18.0), egui::Color32::GRAY);
        }
        let response = ui.interact(rect, ui.scope_id().with("video"), egui::Sense::click());
        if response.clicked() {
            self.show_overlay = !self.show_overlay;
        }
        if self.show_overlay {
            let stream = status.stream.as_ref();
            let text = format!(
                "{}x{}  {:.1} fps  {:.2} Mbit/s  presenter encodes with {}\n{}\ndecode {:.1} ms  to RGBA {:.1} ms  lost {}\n{}\ncursor {}\n{}\ntap: hide",
                stream.map(|s| s.width).unwrap_or(0),
                stream.map(|s| s.height).unwrap_or(0),
                status.fps,
                status.mbps,
                stream.map(|s| s.encoder.as_str()).unwrap_or("?"),
                status.decoder,
                status.decode_ms,
                status.convert_ms,
                status.lost,
                status.pipeline.line(),
                if status.cursor_mode.is_empty() { "?" } else { status.cursor_mode.as_str() },
                status.link.line(),
            );
            let galley = ui.painter().layout(
                text,
                egui::FontId::monospace(12.0),
                egui::Color32::from_rgb(0, 255, 120),
                rect.width() - 170.0,
            );
            let pos = rect.left_top() + egui::vec2(8.0, 40.0);
            ui.painter().galley(pos, galley.clone(), egui::Color32::from_rgb(0, 255, 120));
            if let Some(warning) = status.pipeline.stall() {
                let warn = ui.painter().layout(warning, egui::FontId::proportional(15.0), egui::Color32::from_rgb(255, 90, 90), rect.width() - 16.0);
                let at = pos + egui::vec2(0.0, galley.size().y + 8.0);
                ui.painter().rect_filled(egui::Rect::from_min_size(at, warn.size()).expand(4.0), 4.0, egui::Color32::from_black_alpha(200));
                ui.painter().galley(at, warn, egui::Color32::from_rgb(255, 90, 90));
            }
            let button = egui::Rect::from_min_size(rect.right_top() + egui::vec2(-150.0, 40.0), egui::vec2(140.0, 44.0));
            if ui.put(button, egui::Button::new("Disconnect")).clicked() {
                self.disconnect();
            }
        }
    }
}

impl ViewerApp {
    /// The presenter's cursor as a textured quad over the video (same
    /// mapping as the desktop viewer). Drawn inside this app's own surface,
    /// so no overlay permission is involved.
    fn draw_cursor(
        &mut self,
        ui: &egui::Ui,
        p: &mirrorwire_proto::cursor::PointerUpdate,
        img: &mirrorwire_proto::cursor::CursorImage,
        video: egui::Rect,
    ) {
        let rect = mirrorwire_proto::cursor::RectF { x: video.min.x, y: video.min.y, w: video.width(), h: video.height() };
        let Some(r) = mirrorwire_proto::cursor::cursor_rect(p, img, rect) else { return };
        if self.cursor_tex.as_ref().is_none_or(|(id, _)| *id != img.id) {
            let image = egui::ColorImage::from_rgba_premultiplied([img.width as usize, img.height as usize], &img.rgba);
            let tex = ui.ctx().load_texture("cursor", image, egui::TextureOptions::LINEAR);
            self.cursor_tex = Some((img.id, tex));
        }
        if let Some((_, tex)) = &self.cursor_tex {
            ui.painter().with_clip_rect(video).image(
                tex.id(),
                egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h)),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.check_pending_uri(&ctx);
        let status = self.session.as_ref().map(|s| s.status());
        if let Some(s) = &self.session {
            if let Ok(img) = s.frames.try_recv() {
                let image = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
                self.picture_size = (img.width, img.height);
                match &mut self.texture {
                    Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
                }
                let _ = s.recycle.try_send(img.rgba);
                s.counters.presented.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        match status {
            Some(st) if st.connected => {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.video_ui(ui, &st));
            }
            Some(st) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(32.0); // status bar
                    self.connecting_ui(ui, &st)
                });
            }
            None => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(32.0); // status bar
                    egui::ScrollArea::vertical().show(ui, |ui| self.home_ui(ui));
                });
            }
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// Big on-screen digits: soft keyboards are unreliable in a NativeActivity.
fn keypad(ui: &mut egui::Ui, code: &mut String) {
    let size = egui::vec2(72.0, 52.0);
    egui::Grid::new("keypad").spacing(egui::vec2(8.0, 8.0)).show(ui, |ui| {
        for row in [["1", "2", "3"], ["4", "5", "6"], ["7", "8", "9"], ["⌫", "0", "C"]] {
            for key in row {
                if ui.add(egui::Button::new(egui::RichText::new(key).size(22.0)).min_size(size)).clicked() {
                    match key {
                        "⌫" => {
                            code.pop();
                        }
                        "C" => code.clear(),
                        d if code.len() < 6 => code.push_str(d),
                        _ => {}
                    }
                }
            }
            ui.end_row();
        }
    });
}

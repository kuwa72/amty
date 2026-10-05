use crate::app::App;
use eframe::egui;
use crate::config::{ApprovalMode, Config, McpServerConf, ProviderKind};
use crate::session::Session;
use crate::types::{AppEvent, Block, EvKind, Role};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

fn app_icon() -> egui::viewport::IconData {
    let bytes = include_bytes!("../assets/amty-icon.png");
    let img = image::load_from_memory(bytes).expect("embedded icon");
    let rgba = img.to_rgba8();
    egui::viewport::IconData {
        rgba: rgba.to_vec(),
        width: img.width(),
        height: img.height(),
    }
}

pub fn run(rt: tokio::runtime::Runtime, app: Arc<App>) -> eframe::Result<()> {
    let handle = rt.handle().clone();
    std::thread::spawn(move || rt.block_on(std::future::pending::<()>()));
    let icon = app_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([560.0, 380.0])
            .with_app_id("amty")
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "amty",
        options,
        Box::new(move |cc| {
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(handle, app)))
        }),
    )
}

/// egui's bundled fonts have no CJK glyphs; attach an OS font when found.
fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans W3.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        // Windows (+ WSL mounts)
        "C:/Windows/Fonts/YuGothM.ttc",
        "C:/Windows/Fonts/meiryo.ttc",
        "C:/Windows/Fonts/msyh.ttc",
        "/mnt/c/Windows/Fonts/YuGothM.ttc",
        "/mnt/c/Windows/Fonts/meiryo.ttc",
        "/mnt/c/Windows/Fonts/msyh.ttc",
        "/mnt/c/Windows/Fonts/msgothic.ttc",
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/ipafont-gothic/ipag.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            let mut data = egui::FontData::from_owned(bytes);
            // egui centers a fallback face's box rather than sharing the
            // baseline — compute the y_offset that puts the CJK baseline on
            // the primary face's, from the fonts' own hhea metrics.
            if let Some((p, c)) = fonts
                .families
                .get(&egui::FontFamily::Proportional)
                .and_then(|f| f.first())
                .and_then(|n| fonts.font_data.get(n))
                .and_then(|d| face_metrics(&d.font))
                .and_then(|p| face_metrics(&data.font).map(|c| (p, c)))
            {
                let hl = p.ascent - p.descent + p.leading;
                let hc = c.ascent - c.descent + c.leading;
                data.tweak.y_offset_factor = p.ascent - c.ascent - 0.5 * (hl - hc);
            }
            fonts
                .font_data
                .insert("cjk".into(), std::sync::Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
}

/// Vertical metrics of a sfnt face (em fractions) read from head/hhea —
/// enough to align baselines between two fonts. Handles .ttf/.otf and the
/// first face of a .ttc.
struct FaceMetrics {
    ascent: f32,
    descent: f32,
    leading: f32,
}

fn face_metrics(data: &[u8]) -> Option<FaceMetrics> {
    let mut off = 0usize;
    if data.get(..4) == Some(b"ttcf") {
        off = u32::from_be_bytes(data.get(12..16)?.try_into().ok()?) as usize;
    }
    let tables = u16::from_be_bytes(data.get(off + 4..off + 6)?.try_into().ok()?) as usize;
    let (mut head, mut hhea, mut os2) = (None, None, None);
    for i in 0..tables {
        let r = off + 12 + i * 16;
        let toff = u32::from_be_bytes(data.get(r + 8..r + 12)?.try_into().ok()?) as usize;
        match data.get(r..r + 4) {
            Some(b"head") => head = Some(toff),
            Some(b"hhea") => hhea = Some(toff),
            Some(b"OS/2") => os2 = Some(toff),
            _ => {}
        }
    }
    let i16at = |p: usize| -> Option<f32> {
        Some(i16::from_be_bytes(data.get(p..p + 2)?.try_into().ok()?) as f32)
    };
    let u16at = |p: usize| -> Option<f32> {
        Some(u16::from_be_bytes(data.get(p..p + 2)?.try_into().ok()?) as f32)
    };
    let upm = u16at(head? + 18)?;
    let h = hhea?;
    // mirror skrifa's Metrics: OS/2 typo metrics when USE_TYPO_METRICS
    // (fsSelection bit 7) is set, else hhea, else OS/2 typo/win as a last
    // resort when hhea is all zeros.
    let (mut a, mut d, mut l) = (i16at(h + 4)?, i16at(h + 6)?, i16at(h + 8)?);
    if let Some(o) = os2 {
        let use_typo = u16at(o + 62).map(|s| (s as u16) & 0x80 != 0).unwrap_or(false);
        if use_typo {
            a = i16at(o + 68)?;
            d = i16at(o + 70)?;
            l = i16at(o + 72)?;
        } else if a == 0.0 && d == 0.0 {
            let (ta, td) = (i16at(o + 68).unwrap_or(0.0), i16at(o + 70).unwrap_or(0.0));
            if ta != 0.0 || td != 0.0 {
                a = ta;
                d = td;
                l = i16at(o + 72).unwrap_or(0.0);
            } else {
                a = u16at(o + 74).unwrap_or(0.0);
                d = -u16at(o + 76).unwrap_or(0.0);
                l = 0.0;
            }
        }
    }
    Some(FaceMetrics {
        ascent: a / upm,
        descent: d / upm,
        leading: l / upm,
    })
}

struct PendingView {
    id: String,
    tool: String,
    detail: String,
}

struct SettingsBuf {
    cfg: Config,
    sel_provider: String,
    sel_mcp: Option<String>,
    new_provider: String,
    new_mcp: String,
    /// active tab index (providers / mcp / skills / behavior)
    tab: usize,
}

/// `YYYY-MM-DD HH:MM` (UTC) from a unix timestamp — no date crate dependency.
fn stamp(secs: u64) -> String {    let days = (secs / 86_400) as i64;
    let (h, m) = ((secs / 3600) % 24, (secs / 60) % 60);
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

pub struct GuiApp {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    rx: broadcast::Receiver<AppEvent>,
    current: String,
    input: String,
    /// streamed text not yet persisted into the session
    live_text: HashMap<String, String>,
    /// currently executing tool label per session
    live_tool: HashMap<String, String>,
    running: HashMap<String, bool>,
    /// last error per session, shown in the header until cleared
    last_error: HashMap<String, String>,
    pending: Vec<PendingView>,
    /// cached view of the current session (refreshed on Touched)
    view: Option<Session>,
    view_dirty: bool,
    toast: Option<(String, Instant)>,
    mark: egui_commonmark::CommonMarkCache,
    /// last-frame rect of each message's header row, for hover-reveal actions
    row_rects: HashMap<usize, egui::Rect>,
    /// session currently being renamed inline (sidebar)
    rename_id: Option<String>,
    rename_buf: String,
    settings: Option<SettingsBuf>,
    show_mcp: bool,
    /// catalog entry selected for install (id into catalog::CATALOG)
    cat_sel: Option<&'static str>,
    /// env field buffers for the selected catalog entry
    cat_env: HashMap<String, String>,
    /// keys-file source path inputs, per catalog id
    cat_keys: HashMap<String, String>,
}

impl GuiApp {
    fn new(rt: tokio::runtime::Handle, app: Arc<App>) -> Self {
        let rx = app.events.subscribe();
        let current = app.sessions.list().first().map(|s| s.id.clone()).unwrap_or_default();
        Self {
            app,
            rt,
            rx,
            current,
            input: String::new(),
            live_text: HashMap::new(),
            live_tool: HashMap::new(),
            running: HashMap::new(),
            last_error: HashMap::new(),
            pending: vec![],
            view: None,
            view_dirty: true,
            toast: None,
            mark: egui_commonmark::CommonMarkCache::default(),
            row_rects: HashMap::new(),
            settings: None,
            show_mcp: false,
            rename_id: None,
            rename_buf: String::new(),
            cat_sel: None,
            cat_env: HashMap::new(),
            cat_keys: HashMap::new(),
        }
    }

    /// Install a catalog entry with the env values collected in `cat_env`.
    /// Mirrors POST /v1/mcp-install without going through HTTP.
    fn catalog_install(&mut self, id: &'static str) {
        let fail_lbl = self.tr("save failed:");
        match crate::catalog::build_conf(id, self.cat_env.clone().into_iter().collect()) {
            Ok((name, conf)) => {
                {
                    let mut cfg = self.app.cfg.write().unwrap();
                    cfg.mcp_servers.insert(name.clone(), conf);
                    if let Err(e) = cfg.save(&self.app.cfg_path) {
                        drop(cfg);
                        self.toast(format!("{fail_lbl} {e}"));
                        return;
                    }
                }
                self.app.mcp.update_servers(self.app.mcp_servers_map());
                let mcp = self.app.mcp.clone();
                self.rt.spawn(async move { mcp.reconcile().await });
                self.cat_sel = None;
                self.cat_env.clear();
                self.toast(format!("{}{name}", self.tr("installed ")));
            }
            Err(e) => self.toast(format!("{}{e}", self.tr("install failed: "))),
        }
    }

    fn on_event(&mut self, ev: AppEvent) {
        match ev.kind {
            EvKind::Text { text } => {
                *self.live_text.entry(ev.session.clone()).or_default() += &text;
            }
            EvKind::ToolStart { name, detail } => {
                self.live_tool.insert(ev.session.clone(), format!("{name}: {detail}"));
            }
            EvKind::ToolEnd { .. } => {
                self.live_tool.remove(&ev.session);
            }
            EvKind::ApprovalNeeded { approval_id, tool, detail } => {
                self.pending.push(PendingView { id: approval_id, tool, detail });
            }
            EvKind::ApprovalResolved { approval_id, .. } => {
                self.pending.retain(|p| p.id != approval_id);
            }
            EvKind::Running { running } => {
                self.running.insert(ev.session.clone(), running);
            }
            EvKind::Error { message } => {
                self.last_error.insert(ev.session.clone(), message);
                self.live_text.remove(&ev.session);
                self.live_tool.remove(&ev.session);
                self.view_dirty = true;
            }
            EvKind::Compacted { dropped } => {
                self.toast(format!("{} ({dropped})", self.tr("context compacted")));
                self.view_dirty = true;
            }
            EvKind::Touched | EvKind::Done => {
                // store now has everything streamed so far
                self.live_text.remove(&ev.session);
                if matches!(ev.kind, EvKind::Done) {
                    self.live_tool.remove(&ev.session);
                }
                self.view_dirty = true;
            }
        }
    }

    fn refresh_view(&mut self) {
        if self.view_dirty {
            self.view = if self.current.is_empty() {
                None
            } else {
                self.app.sessions.resolve(&self.current).and_then(|id| self.app.sessions.get(&id))
            };
            if let Some(v) = &self.view {
                self.current = v.id.clone();
            }
            self.view_dirty = false;
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn tr(&self, key: &'static str) -> &'static str {
        crate::i18n::t(&self.app.cfg.read().unwrap().lang, key)
    }

    fn ensure_session(&mut self) -> String {
        if self.current.is_empty() {
            self.current = self.app.sessions.create(None, None);
        }
        self.current.clone()
    }

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let sid = self.ensure_session();
        let app = self.app.clone();
        let sid2 = sid.clone();
        let input_empty = self.app.is_running(&sid);
        if input_empty {
            self.toast("already running in this session");
            return;
        }
        self.input.clear();
        self.last_error.remove(&sid);
        self.view_dirty = true;
        let app_ref = self.app.clone();
        let sid_err = sid.clone();
        self.rt.spawn(async move {
            if let Err(e) = crate::agent::start_run(&app, &sid2, text) {
                app_ref.emit(&sid_err, EvKind::Error { message: e.to_string() });
            }
        });
    }

    fn session_provider(&self) -> (String, String) {
        let cfg = self.app.cfg.read().unwrap();
        let (p, m) = self
            .view
            .as_ref()
            .map(|s| (s.provider.clone(), s.model.clone()))
            .unwrap_or((None, None));
        let prov = p.unwrap_or_else(|| cfg.provider.clone());
        let model = m.or_else(|| cfg.providers.get(&prov).map(|c| c.model.clone())).unwrap_or_default();
        (prov, model)
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        self.refresh_view();

        // drag & drop: dropped file paths are appended to the input box;
        // paint a drop hint while files are hovered over the window
        let (dropped, hovering) = ctx.input(|i| {
            (
                i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>(),
                !i.raw.hovered_files.is_empty(),
            )
        });
        for p in &dropped {
            let s = p.display().to_string();
            let s = if s.contains(char::is_whitespace) { format!("\"{s}\"") } else { s };
            if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
                self.input.push(' ');
            }
            self.input.push_str(&s);
        }
        if hovering {
            let rect = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dnd_hint"),
            ));
            painter.rect_filled(rect, 4.0, egui::Color32::from_rgba_unmultiplied(30, 60, 120, 80));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                self.tr("drop files to insert their paths"),
                egui::FontId::proportional(18.0),
                egui::Color32::WHITE,
            );
        }

        let any_running = self.running.values().any(|r| *r);
        let auth_running = self.app.auth.lock().unwrap().values().any(|a| a.running);
        if any_running || auth_running || !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // ----- left: sessions -----
        egui::Panel::left("side").min_size(200.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("amty");
                if ui.button(self.tr("＋ new")).clicked() {
                    self.current = self.app.sessions.create(None, None);
                    self.view_dirty = true;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("sessions").show(ui, |ui| {
                let metas = self.app.sessions.list();
                let mut rename: Option<(String, String)> = None;
                let mut delete: Option<String> = None;
                for meta in metas {
                    let selected = meta.id == self.current;
                    let running = *self.running.get(&meta.id).unwrap_or(&false);
                    if self.rename_id.as_deref() == Some(meta.id.as_str()) {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.rename_buf)
                                .desired_width(f32::INFINITY),
                        );
                        resp.request_focus();
                        let done = resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if done || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            rename = Some((meta.id.clone(), self.rename_buf.trim().to_string()));
                        }
                        continue;
                    }
                    let label = if running { format!("● {}", meta.title) } else { meta.title.clone() };
                    let resp = ui.selectable_label(selected, label);
                    let hint = format!(
                        "{}\n{} {} · {}",
                        meta.title,
                        stamp(meta.updated),
                        meta.n_messages,
                        self.tr("messages"),
                    );
                    let resp = resp.on_hover_text(hint);
                    resp.context_menu(|ui| {
                        if ui.button(self.tr("rename")).clicked() {
                            self.rename_id = Some(meta.id.clone());
                            self.rename_buf = meta.title.clone();
                            ui.close();
                        }
                        if ui.button(self.tr("delete")).clicked() {
                            delete = Some(meta.id.clone());
                            ui.close();
                        }
                    });
                    if resp.clicked() {
                        self.current = meta.id.clone();
                        self.view_dirty = true;
                    }
                }
                if let Some((id, title)) = rename {
                    self.rename_id = None;
                    if !title.is_empty() {
                        self.app.sessions.set_title(&id, &title);
                        self.view_dirty = true;
                    }
                }
                if let Some(id) = delete {
                    self.app.sessions.delete(&id);
                    if self.current == id {
                        self.current = String::new();
                    }
                    self.view_dirty = true;
                }
            });
            ui.separator();
            // provider / model quick switch (per-session override)
            let (prov, model) = self.session_provider();
            let names: Vec<String> = self.app.cfg.read().unwrap().providers.keys().cloned().collect();
            let mut sel = prov.clone();
            egui::ComboBox::from_label(self.tr("provider"))
                .selected_text(&sel)
                .show_ui(ui, |ui| {
                    for n in &names {
                        ui.selectable_value(&mut sel, n.clone(), n);
                    }
                });
            if sel != prov {
                let sid = self.ensure_session();
                self.app.sessions.set_fields(&sid, Some(sel), None);
                self.view_dirty = true;
            }
            ui.horizontal(|ui| {
                ui.label(self.tr("model"));
                let mut m = model.clone();
                if ui.add(egui::TextEdit::singleline(&mut m).desired_width(150.0)).lost_focus() && m != model {
                    let sid = self.ensure_session();
                    self.app.sessions.set_fields(&sid, None, Some(m));
                    self.view_dirty = true;
                }
                let kind = self.app.cfg.read().unwrap().providers.get(&prov).map(|p| p.kind);
                let (suggestions, err) = match kind {
                    Some(crate::config::ProviderKind::CommandCode) => {
                        let conf = self.app.cfg.read().unwrap().providers.get(&prov).cloned().unwrap_or_default();
                        crate::provider::cmdc_models(&conf)
                    }
                    _ => (kind.map(crate::provider::model_suggestions).unwrap_or_default(), None),
                };
                if !suggestions.is_empty() {
                    let label = match err {
                        None => format!("▾ ({})", suggestions.len()),
                        Some(_) => format!("▾ ({}) ⚠", suggestions.len()),
                    };
                    ui.menu_button(label, |ui| {
                        if let Some(e) = &err {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 200, 80),
                                format!("{}: {}", self.tr("cmdc --list-models unavailable — showing fallback list"), e),
                            );
                            ui.separator();
                        }
                        egui::ScrollArea::vertical()
                            .max_height(400.0)
                            .show(ui, |ui| {
                                for s in suggestions {
                                    if ui.button(&s).clicked() {
                                        let sid = self.ensure_session();
                                        self.app.sessions.set_fields(&sid, None, Some(s));
                                        self.view_dirty = true;
                                        ui.close();
                                    }
                                }
                            });
                    });
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(self.tr("⚙ settings")).clicked() {
                    if self.settings.is_none() {
                        let cfg = self.app.cfg.read().unwrap().clone();
                        self.settings = Some(SettingsBuf {
                            sel_provider: cfg.provider.clone(),
                            cfg,
                            sel_mcp: None,
                            new_provider: String::new(),
                            new_mcp: String::new(),
                            tab: 0,
                        });
                    }
                }
                if ui.button(self.tr("mcp")).clicked() {
                    self.show_mcp = !self.show_mcp;
                }
            });
        });

        // ----- bottom: input -----
        egui::Panel::bottom("input").show(ui, |ui| {
            // pending approvals sit right above the input row so they can't
            // hide behind the chat or drift off-screen like a floating window
            let mut resolved: Vec<(String, bool)> = vec![];
            for p in &self.pending {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 80),
                            format!("⚠ {} {}", self.tr("approve:"), p.tool),
                        );
                        if ui.button(self.tr("allow")).clicked() {
                            resolved.push((p.id.clone(), true));
                        }
                        if ui.button(self.tr("deny")).clicked() {
                            resolved.push((p.id.clone(), false));
                        }
                    });
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(p.detail.chars().take(600).collect::<String>())
                                .monospace()
                                .small(),
                        )
                        .wrap(),
                    );
                });
            }
            for (id, allow) in resolved {
                self.app.approvals.resolve(&id, allow);
                self.pending.retain(|p| p.id != id);
            }
            ui.add_space(4.0);
            let mut bypass = self.app.cfg.read().unwrap().approval == ApprovalMode::Auto;
            let frame = if bypass {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .corner_radius(4)
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 90, 90)))
                    .fill(egui::Color32::from_rgba_unmultiplied(120, 30, 30, 60))
            } else {
                egui::Frame::new()
            };
            ui.horizontal(|ui| {
                // right-to-left: the right-edge widgets claim their width first,
                // so the text field can use exactly what is left over
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    frame
                        .show(ui, |ui| {
                            ui.checkbox(&mut bypass, self.tr("bypass"));
                        })
                        .response
                        .on_hover_text(self.tr("shell and file writes run without asking"));
                    let running = *self.running.get(&self.current).unwrap_or(&false);
                    if running && ui.button(self.tr("stop")).clicked() {
                        self.app.cancel(&self.current);
                    }
                    let mut do_send = ui.button(self.tr("send")).clicked();
                    let hint = self.tr("message (Enter to send, Shift+Enter for newline)");
                    let resp = ui.add_sized(
                        [ui.available_width().max(80.0), 64.0],
                        egui::TextEdit::multiline(&mut self.input).hint_text(hint),
                    );
                    let ime = ui.ctx().input(|i| {
                        i.events.iter().any(|e| matches!(e, egui::Event::Ime(_)))
                            || i.raw.events.iter().any(|e| matches!(e, egui::Event::Ime(_)))
                    });
                    if resp.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
                        && !ime
                    {
                        do_send = true;
                    }
                    if do_send {
                        self.send();
                        resp.request_focus();
                    }
                });
            });
            if bypass != (self.app.cfg.read().unwrap().approval == ApprovalMode::Auto) {
                let mut cfg = self.app.cfg.write().unwrap();
                cfg.approval = if bypass { ApprovalMode::Auto } else { ApprovalMode::Ask };
                if let Err(e) = cfg.save(&self.app.cfg_path) {
                    drop(cfg);
                    self.toast(format!("{} {e}", self.tr("save failed:")));
                }
            }
        });

        // ----- center: chat -----
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some((msg, at)) = &self.toast {
                if at.elapsed() < Duration::from_secs(4) {
                    ui.label(egui::RichText::new(msg).color(egui::Color32::from_rgb(255, 200, 80)));
                } else {
                    self.toast = None;
                }
            }
            let (prov, model) = self.session_provider();
            ui.horizontal_wrapped(|ui| {
                if let Some(s) = &self.view {
                    ui.heading(&s.title);
                } else {
                    ui.heading("amty");
                }
                ui.weak(format!("{prov} / {model}"));
                if let Some(t) = self.live_tool.get(&self.current) {
                    // mcp__<srv>__<tool> -> <tool>, detail kept but truncated
                    let shown = match t.split_once(": ") {
                        Some((n, d)) => format!("{}: {d}", short_tool(n)),
                        None => short_tool(t).to_string(),
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(format!("⏳ {shown}")).weak())
                            .truncate(),
                    )
                    .on_hover_text(t);
                } else if *self.running.get(&self.current).unwrap_or(&false) {
                    ui.weak(format!("⏳ {}", self.tr("working…")));
                }
            });
            if let Some(err) = self.last_error.get(&self.current) {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 100, 100),
                    format!("{} {}", self.tr("error:"), err),
                );
            }
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                // Wrap labels even inside horizontal layouts (markdown table
                // cells, tool/status rows) — otherwise they clip at the
                // frame's right edge.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                let lang = self.app.cfg.read().unwrap().lang.clone();
                let mut action = None;
                let running_now = *self.running.get(&self.current).unwrap_or(&false);
                let mut row_rects = std::mem::take(&mut self.row_rects);
                if let Some(s) = self.view.clone() {
                    let mut i = 0;
                    while i < s.messages.len() {
                        if is_tool_only(&s.messages[i]) {
                            let n = tool_run_len(&s.messages, i);
                            // only the trailing run is live while the agent works
                            let live = running_now && i + n == s.messages.len();
                            render_tool_run(ui, &s.messages, i, n, &lang, live);
                            i += n;
                        } else {
                            if let Some(a) = render_message(ui, &mut self.mark, &mut row_rects, &s.messages[i], &lang, i, running_now) {
                                action = Some(a);
                            }
                            i += 1;
                        }
                    }
                } else {
                    ui.weak(self.tr("send a message to start"));
                }
                self.row_rects = row_rects;
                if let Some(t) = self.live_text.get(&self.current) {
                    if !t.is_empty() {
                        ui.colored_label(ui.visuals().weak_text_color(), self.tr("assistant"));
                        egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.mark, &linkify(t));
                    }
                }
                match action {
                    Some(MsgAction::Copy(text)) => {
                        ui.ctx().copy_text(text);
                        let m = self.tr("copied");
                        self.toast(m);
                    }
                    Some(MsgAction::Save(text)) => {
                        let m = match export_text(&text) {
                            Ok(p) => format!("{} {}", self.tr("saved to"), p.display()),
                            Err(e) => format!("{} {e}", self.tr("save failed:")),
                        };
                        self.toast(m);
                    }
                    Some(MsgAction::Rewind(i)) => {
                        self.app.sessions.truncate(&self.current, i + 1);
                        self.view_dirty = true;
                        let m = self.tr("rewound");
                        self.toast(m);
                    }
                    Some(MsgAction::Fork(i)) => {
                        if let Some(id) = self.app.sessions.fork(&self.current, i + 1) {
                            self.current = id;
                            self.view_dirty = true;
                            let m = self.tr("forked");
                            self.toast(m);
                        }
                    }
                    None => {}
                }
            });
        });

        if self.show_mcp {
            let lang = self.app.cfg.read().unwrap().lang.clone();
            let ja = lang == "ja";
            let t = |k: &'static str| crate::i18n::t(&lang, k);
            let mut install_target: Option<&'static str> = None;
            let mut auth_target: Option<&'static str> = None;
            let mut keys_target: Option<(&'static str, String)> = None;
            let mut cred_prompt: Option<&'static str> = None;
            let mut remove_target: Option<String> = None;
            let mut toggle_target: Option<(String, bool)> = None;
            // statuses are collected once so the summary bar and the list agree
            let statuses = self.app.mcp.statuses();
            let connected = statuses.iter().filter(|(_, s, e)| *e && matches!(s, crate::mcp::Status::Connected(_))).count();
            let failed = statuses.iter().filter(|(_, s, e)| *e && matches!(s, crate::mcp::Status::Failed(_))).count();
            let tools: usize = statuses
                .iter()
                .filter_map(|(_, s, _)| match s {
                    crate::mcp::Status::Connected(n) => Some(*n),
                    _ => None,
                })
                .sum();
            egui::Window::new(t("mcp servers")).open(&mut self.show_mcp).default_size([520.0, 460.0]).show(&ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // ----- summary: one line for "is anything wrong?" -----
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(t("connected")).strong());
                        ui.weak(format!("{connected}/{}", statuses.len()));
                        ui.separator();
                        ui.weak(format!("{tools} {}", t("tools")));
                        if failed > 0 {
                            ui.colored_label(egui::Color32::from_rgb(255, 120, 120), format!("{failed} {}", t("failed:")));
                        }
                    });
                    ui.separator();
                    ui.strong(t("servers"));
                    // ----- configured servers: name, status, enable toggle, remove -----
                    for (name, status, enabled) in &statuses {
                        let st = match status {
                            crate::mcp::Status::Disabled => t("disabled").to_string(),
                            crate::mcp::Status::Connecting => t("connecting…").to_string(),
                            crate::mcp::Status::Connected(n) => format!("{n} {}", t("tools")),
                            crate::mcp::Status::Failed(e) => format!("{} {e}", t("failed:")),
                        };
                        let bad = matches!(status, crate::mcp::Status::Failed(_));
                        ui.horizontal(|ui| {
                            let mut on = *enabled;
                            if ui.checkbox(&mut on, "").changed() {
                                toggle_target = Some((name.clone(), on));
                            }
                            ui.label(egui::RichText::new(name).monospace());
                            if bad {
                                ui.colored_label(egui::Color32::from_rgb(255, 120, 120), st);
                            } else {
                                ui.weak(st);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("🗑").on_hover_text(t("remove")).clicked() {
                                    remove_target = Some(name.clone());
                                }
                            });
                        });
                    }
                    if statuses.is_empty() {
                        ui.weak(t("no servers configured"));
                    }
                    ui.separator();
                    ui.strong(t("catalog"));
                    if !crate::catalog::node_available() {
                        ui.colored_label(egui::Color32::from_rgb(255, 200, 80), t("Node.js (npx) is required for npm-based entries"));
                    }
                    let installed = self.app.cfg.read().unwrap().mcp_servers.clone();
                    for entry in crate::catalog::CATALOG {
                        let is_installed = installed.contains_key(entry.id);
                        let open = self.cat_sel == Some(entry.id);
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(entry.label).strong());
                                if is_installed {
                                    ui.weak(t("installed"));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if is_installed {
                                        if ui.small_button("🗑").on_hover_text(t("remove")).clicked() {
                                            remove_target = Some(entry.id.to_string());
                                        }
                                    } else if open {
                                        if ui.small_button("✕").clicked() {
                                            self.cat_sel = None;
                                        }
                                    } else if ui.small_button(t("install")).clicked() {
                                        self.cat_sel = Some(entry.id);
                                        self.cat_env.clear();
                                    }
                                });
                            });
                            if !is_installed || open {
                                ui.weak(if ja { entry.desc_ja } else { entry.desc });
                            }
                            // OAuth keys file placement
                            if entry.keys.is_some() {
                                let dst = crate::catalog::keys_path(entry.id);
                                let have = dst.as_ref().map(|p| p.exists()).unwrap_or(false);
                                if have {
                                    ui.weak(format!("✓ {}", dst.unwrap().display()));
                                } else {
                                    ui.horizontal(|ui| {
                                        ui.label(t("keys json:"));
                                        let v = self.cat_keys.entry(entry.id.to_string()).or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(v)
                                                .desired_width(170.0)
                                                .hint_text(t("path to gcp-oauth.keys.json")),
                                        );
                                        if !v.trim().is_empty() && ui.button(t("place")).clicked() {
                                            keys_target = Some((entry.id, v.trim().to_string()));
                                        }
                                    });
                                }
                            }
                            // OAuth/auth runner (package `auth` cmd or hosted oauth)
                            if entry.auth_cmd.is_some() || entry.oauth.is_some() {
                                let running = self.app.auth.lock().unwrap().get(entry.id).map(|a| a.running).unwrap_or(false);
                                ui.horizontal(|ui| {
                                    if running {
                                        ui.weak(t("auth running…"));
                                    } else if ui.button(t("auth")).clicked() {
                                        // needs_client && no client id yet → open the
                                        // install form for creds instead of erroring
                                        let needs_creds = entry.oauth.map(|o| o.needs_client).unwrap_or(false)
                                            && installed
                                                .get(entry.id)
                                                .and_then(|s| s.oauth_client_id.clone())
                                                .unwrap_or_default()
                                                .is_empty();
                                        if needs_creds {
                                            cred_prompt = Some(entry.id);
                                        } else {
                                            auth_target = Some(entry.id);
                                        }
                                    }
                                    if !running {
                                        ui.weak(t("(browser may open)"));
                                    }
                                });
                                if let Some(tail) = self.app.auth_log_tail(entry.id, 600) {
                                    if !tail.trim().is_empty() {
                                        ui.add(egui::Label::new(
                                            egui::RichText::new(tail.trim_end()).monospace().small(),
                                        ).wrap());
                                    }
                                }
                            }
                            if open {
                                // hosted oauth entries that need a user-registered client
                                if entry.oauth.map(|o| o.needs_client).unwrap_or(false) {
                                    for (key, secret) in [("OAUTH_CLIENT_ID", false), ("OAUTH_CLIENT_SECRET", true)] {
                                        ui.horizontal(|ui| {
                                            ui.label(key);
                                            let v = self.cat_env.entry(key.to_string()).or_default();
                                            ui.add(
                                                egui::TextEdit::singleline(v)
                                                    .password(secret)
                                                    .desired_width(200.0),
                                            );
                                        });
                                    }
                                }
                                for spec in entry.env {
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{}{}", spec.key, if spec.required { format!(" ({})", t("required")) } else { String::new() }));
                                        let v = self.cat_env.entry(spec.key.to_string()).or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(v)
                                                .password(spec.secret)
                                                .desired_width(200.0)
                                                .hint_text(if ja { spec.hint_ja } else { spec.hint }),
                                        );
                                    });
                                }
                                ui.weak(format!("{} {}", t("setup:"), if ja { entry.note_ja } else { entry.note }));
                                if ui.button(format!("{} {}", t("install"), entry.label)).clicked() {
                                    install_target = Some(entry.id);
                                }
                            }
                        });
                    }
                });
            });
            if let Some((name, on)) = toggle_target {
                {
                    let mut cfg = self.app.cfg.write().unwrap();
                    if let Some(s) = cfg.mcp_servers.get_mut(&name) {
                        s.enabled = on;
                    }
                    if let Err(e) = cfg.save(&self.app.cfg_path) {
                        drop(cfg);
                        self.toast(format!("{} {e}", t("save failed:")));
                    }
                }
                self.app.mcp.update_servers(self.app.mcp_servers_map());
                let mcp = self.app.mcp.clone();
                self.rt.spawn(async move { mcp.reconcile().await });
            }
            if let Some(name) = remove_target {
                {
                    let mut cfg = self.app.cfg.write().unwrap();
                    cfg.mcp_servers.remove(&name);
                    if let Err(e) = cfg.save(&self.app.cfg_path) {
                        drop(cfg);
                        self.toast(format!("{} {e}", t("save failed:")));
                    }
                }
                self.app.mcp.update_servers(self.app.mcp_servers_map());
                let mcp = self.app.mcp.clone();
                self.rt.spawn(async move { mcp.reconcile().await });
                self.toast(format!("{} {name}", self.tr("removed:")));
            }
            if let Some(id) = cred_prompt {
                self.cat_sel = Some(id);
                self.cat_env.clear();
                self.toast(t("enter the OAuth client id/secret, then install"));
            }
            if let Some(id) = install_target {
                self.catalog_install(id);
            }
            if let Some(id) = auth_target {
                let started = self.app.clone();
                match started.start_auth(id) {
                    Ok(_) => self.toast(t("auth started")),
                    Err(e) => self.toast(format!("{} {e}", t("auth failed: "))),
                }
            }
            if let Some((id, src)) = keys_target {
                match crate::catalog::place_keys(id, std::path::Path::new(&src)) {
                    Ok(dst) => {
                        self.app.mcp.update_servers(self.app.mcp_servers_map());
                        let mcp = self.app.mcp.clone();
                        self.rt.spawn(async move { mcp.reconcile().await });
                        self.toast(format!("{} {}", t("placed:"), dst.display()));
                    }
                    Err(e) => self.toast(format!("{} {e}", t("place failed:"))),
                }
            }
        }

        if self.settings.is_some() {
            let mut buf = self.settings.take().unwrap();
            let mut open = true;
            let mut save = false;
            let mut import = false;
            let mut skill_rm: Option<String> = None;
            let lang = self.app.cfg.read().unwrap().lang.clone();
            settings_window(&ctx, &lang, &mut open, &mut buf, &mut save, &mut import, &mut skill_rm);
            if let Some(name) = skill_rm {
                let dir = crate::config::skills_dir().join(&name);
                match std::fs::remove_dir_all(&dir) {
                    Ok(_) => self.toast(format!("{} {name}", self.tr("removed:"))),
                    Err(e) => self.toast(format!("{} {e}", self.tr("remove failed:"))),
                }
            }
            if import {
                match crate::config::import_claude(&mut buf.cfg) {
                    Ok(added) => self.toast(format!("{} {}", added.len(), self.tr("server(s) imported:"))),
                    Err(e) => self.toast(format!("{} {e}", self.tr("import failed:"))),
                }
            }
            if save {
                {
                    *self.app.cfg.write().unwrap() = buf.cfg.clone();
                    if let Err(e) = buf.cfg.save(&self.app.cfg_path) {
                        self.toast(format!("{} {e}", self.tr("save failed:")));
                    }
                }
                let servers = self
                    .app
                    .cfg
                    .read()
                    .unwrap()
                    .mcp_servers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                self.app.mcp.update_servers(servers);
                let mcp = self.app.mcp.clone();
                self.rt.spawn(async move { mcp.reconcile().await });
                self.view_dirty = true;
                self.toast(self.tr("saved"));
            }
            if open {
                self.settings = Some(buf);
            }
        }
    }
}

enum MsgAction {
    Copy(String),
    Save(String),
    /// keep messages[..=idx] in this session
    Rewind(usize),
    /// new session with messages[..=idx]
    Fork(usize),
}

/// A tool-only message is part of a run: the assistant message holding only
/// `tool_use` blocks plus the user message holding the matching results.
/// Synthetic results backfilled into a text message keep that message out.
fn is_tool_only(msg: &crate::types::ChatMessage) -> bool {
    !msg.blocks.is_empty()
        && msg
            .blocks
            .iter()
            .all(|b| matches!(b, Block::ToolUse { .. } | Block::ToolResult { .. }))
}

/// Number of messages from `start` belonging to one tool run: every following
/// message that holds tool calls/results only, i.e. no assistant prose.
fn tool_run_len(msgs: &[crate::types::ChatMessage], start: usize) -> usize {
    let mut n = 0;
    while start + n < msgs.len() && is_tool_only(&msgs[start + n]) {
        n += 1;
    }
    n
}

fn tool_use_count(msgs: &[crate::types::ChatMessage], start: usize) -> usize {
    (start..start + tool_run_len(msgs, start))
        .flat_map(|i| msgs[i].blocks.iter())
        .filter(|b| matches!(b, Block::ToolUse { .. }))
        .count()
}

/// `mcp__<srv>__<tool>` -> `<tool>`, other names pass through.
fn short_tool(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((_, tool)) = rest.split_once("__") {
            return tool;
        }
    }
    name
}

fn render_tool_msg(ui: &mut egui::Ui, msg: &crate::types::ChatMessage, lang: &str, key: &str) {
    let t = |k: &'static str| crate::i18n::t(lang, k);
    for (bi, b) in msg.blocks.iter().enumerate() {
        match b {
            Block::Text { .. } => {}
            Block::ToolUse { name, input, .. } => {
                egui::CollapsingHeader::new(format!("🔧 {name}"))
                    .id_salt(format!("{key}_tu_{bi}"))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(input.to_string()).monospace().small(),
                        ).wrap());
                    });
            }
            Block::ToolResult { content, is_error, .. } => {
                let head = if *is_error { t("✗ result (error)") } else { t("✓ result") };
                egui::CollapsingHeader::new(head)
                    .id_salt(format!("{key}_tr_{bi}"))
                    .show(ui, |ui| {
                        let t = crate::types::truncate_preview(content, 3000);
                        ui.add(egui::Label::new(egui::RichText::new(t).monospace().small()).wrap());
                    });
            }
        }
    }
}

/// One run of tool calls collapsed into a single summary row: "🔧 N tools",
/// the trace of tool names, folded by default. `live` keeps it open and
/// appends a spinner while the agent is working on it.
fn render_tool_run(ui: &mut egui::Ui, msgs: &[crate::types::ChatMessage], start: usize, len: usize, lang: &str, live: bool) {
    let t = |k: &'static str| crate::i18n::t(lang, k);
    let end = start + len;
    let count = tool_use_count(msgs, start);
    let names: Vec<&str> = (start..end)
        .flat_map(|i| msgs[i].blocks.iter())
        .filter_map(|b| match b {
            Block::ToolUse { name, .. } => Some(short_tool(name)),
            _ => None,
        })
        .collect();
    let mut trace = names.join(", ");
    if names.len() > 4 {
        trace = format!("{} +{}", names[..4].join(", "), names.len() - 4);
    }
    let head = format!("🔧 {count} {}", t("tools"));
    egui::CollapsingHeader::new(head)
        .id_salt(format!("run_{start}"))
        .default_open(live)
        .show(ui, |ui| {
            if live {
                ui.weak(format!("⏳ {}", t("working…")));
            }
            for i in start..end {
                render_tool_msg(ui, &msgs[i], lang, &format!("{start}_{i}"));
            }
        })
        .header_response
        .on_hover_text(trace);
    ui.add_space(8.0);
}

fn render_message(ui: &mut egui::Ui, mark: &mut egui_commonmark::CommonMarkCache, rows: &mut HashMap<usize, egui::Rect>, msg: &crate::types::ChatMessage, lang: &str, idx: usize, running: bool) -> Option<MsgAction> {
    let t = |k: &'static str| crate::i18n::t(lang, k);
    let (label, color) = match msg.role {
        Role::User => (t("you"), egui::Color32::from_rgb(120, 180, 255)),
        Role::Assistant => (t("assistant"), egui::Color32::from_rgb(160, 220, 160)),
    };
    let mut action = None;
    let is_tool_msg = msg.blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }));
    if !is_tool_msg {
        // actions appear once the header row is hovered: the row's rect is
        // kept from the previous frame so the buttons don't shift the layout
        // the moment they show up (defer pattern)
        let hovered = match rows.get(&idx) {
            Some(r) => ui.ctx().pointer_hover_pos().map(|p| r.contains(p)).unwrap_or(false),
            None => false,
        };
        let head = ui.horizontal(|ui| {
            ui.colored_label(color, label);
            let text = msg.text_content();
            if hovered && !text.trim().is_empty() {
                if ui.small_button(t("copy")).clicked() {
                    action = Some(MsgAction::Copy(text.clone()));
                }
                if ui.small_button(t("save")).clicked() {
                    action = Some(MsgAction::Save(text));
                }
            }
            if hovered {
                ui.menu_button("⋯", |ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        if ui.button(t("rewind to here")).clicked() {
                            action = Some(MsgAction::Rewind(idx));
                            ui.close();
                        }
                        if ui.button(t("fork from here")).clicked() {
                            action = Some(MsgAction::Fork(idx));
                            ui.close();
                        }
                    });
                });
            }
        });
        // the row rect is recorded with the full available width so hovering
        // anywhere on the message's header band reveals the actions
        let r = head.response.rect;
        let full = ui.max_rect();
        rows.insert(idx, egui::Rect::from_min_max(
            egui::pos2(full.left(), r.top()),
            egui::pos2(full.right(), r.bottom()),
        ));
    }
    for (bi, b) in msg.blocks.iter().enumerate() {
        match b {
            Block::Text { text } => {
                egui_commonmark::CommonMarkViewer::new().show(ui, mark, &linkify(text));
            }
            Block::ToolUse { name, input, .. } => {
                egui::CollapsingHeader::new(format!("🔧 {name}"))
                    .id_salt(format!("tu_{idx}_{bi}"))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(input.to_string()).monospace().small(),
                        ).wrap());
                    });
            }
            Block::ToolResult { content, is_error, .. } => {
                let head = if *is_error { t("✗ result (error)") } else { t("✓ result") };
                egui::CollapsingHeader::new(head)
                    .id_salt(format!("tr_{idx}_{bi}"))
                    .show(ui, |ui| {
                        let t = crate::types::truncate_preview(content, 3000);
                        ui.add(egui::Label::new(egui::RichText::new(t).monospace().small()).wrap());
                    });
            }
        }
    }
    ui.add_space(8.0);
    action
}

/// Write a message's text to a file (Downloads dir when known, else home)
/// and return the path used.
fn export_text(text: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("amty-{secs}.md"));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Wrap bare http(s) URLs in `<…>` autolinks so the markdown viewer renders
/// them clickable. Fenced code blocks, inline code spans, and URLs already
/// inside markdown link/autolink syntax are left untouched.
fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        let fence = t.starts_with("```") || t.starts_with("~~~");
        if in_fence || fence {
            in_fence ^= fence;
            out.push_str(line);
            continue;
        }
        out.push_str(&linkify_line(line));
    }
    out
}

fn linkify_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut rest = line;
    let mut backticks = 0usize;
    loop {
        let pos = rest
            .find("http://")
            .into_iter()
            .chain(rest.find("https://"))
            .min();
        let Some(pos) = pos else {
            out.push_str(rest);
            break;
        };
        let (before, from) = rest.split_at(pos);
        backticks += before.matches('`').count();
        // skip inside `inline code` or when preceded by a markdown link char
        let skip = backticks % 2 == 1
            || matches!(before.chars().last(), Some('(' | '[' | '<' | '=' | '"' | '\''));
        let mut end = from.len();
        for (i, ch) in from.char_indices() {
            if matches!(ch, ' ' | '\t' | '\n' | '\r' | '<' | '>' | '"' | '\'') {
                end = i;
                break;
            }
        }
        // drop trailing sentence punctuation and unbalanced ')'
        let mut url = &from[..end];
        while let Some(c) = url.chars().last() {
            let unbalanced = c == ')' && url.matches(')').count() > url.matches('(').count();
            if matches!(c, '.' | ',' | ';' | ':' | '!' | '?') || unbalanced {
                url = &url[..url.len() - c.len_utf8()];
            } else {
                break;
            }
        }
        out.push_str(before);
        if skip || url.len() <= "https://".len() {
            out.push_str(url);
        } else {
            out.push('<');
            out.push_str(url);
            out.push('>');
        }
        rest = &from[url.len()..];
    }
    out
}

fn settings_window(ctx: &egui::Context, lang: &str, open: &mut bool, buf: &mut SettingsBuf, save: &mut bool, import: &mut bool, skill_rm: &mut Option<String>) {
    let t = |k: &'static str| crate::i18n::t(lang, k);
    egui::Window::new(t("settings")).open(open).default_size([560.0, 480.0]).show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(t("language:"));
                egui::ComboBox::from_id_salt("lang")
                    .selected_text(&buf.cfg.lang)
                    .show_ui(ui, |ui| {
                        for l in crate::i18n::LANGS {
                            ui.selectable_value(&mut buf.cfg.lang, l.to_string(), *l);
                        }
                    });
            });
            ui.separator();
            // ----- tabs: providers / mcp / skills / behavior -----
            let tabs = [t("providers"), t("mcp"), t("skills"), t("behavior")];
            ui.horizontal_wrapped(|ui| {
                for (i, label) in tabs.iter().enumerate() {
                    if ui.selectable_label(buf.tab == i, *label).clicked() {
                        buf.tab = i;
                    }
                }
            });
            ui.separator();
            if buf.tab == 0 {
            ui.horizontal_wrapped(|ui| {
                for name in buf.cfg.providers.keys().cloned().collect::<Vec<_>>() {
                    let is_default = buf.cfg.provider == name;
                    let mut text = name.clone();
                    if is_default {
                        text.push_str(" ★");
                    }
                    let resp = ui.selectable_label(buf.sel_provider == name, text);
                    let resp = if is_default {
                        resp.on_hover_text(t("default"))
                    } else {
                        resp
                    };
                    if resp.clicked() {
                        buf.sel_provider = name;
                    }
                    if !is_default {
                        resp.context_menu(|ui| {
                            if ui.button(t("set as default")).clicked() {
                                buf.cfg.provider = buf.sel_provider.clone();
                                ui.close();
                            }
                            if ui.button(t("delete")).clicked() {
                                buf.cfg.providers.remove(&buf.sel_provider);
                                ui.close();
                            }
                        });
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(t("add:"));
                ui.text_edit_singleline(&mut buf.new_provider);
                if ui.button("＋").clicked() && !buf.new_provider.trim().is_empty() {
                    let name = buf.new_provider.trim().to_string();
                    buf.cfg.providers.entry(name.clone()).or_insert_with(|| crate::config::ProviderConf {
                        kind: ProviderKind::OpenAi,
                        ..Default::default()
                    });
                    buf.sel_provider = name;
                    buf.new_provider.clear();
                }
            });
            if let Some(p) = buf.cfg.providers.get_mut(&buf.sel_provider) {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("kind");
                        egui::ComboBox::from_id_salt("pk")
                            .selected_text(match p.kind {
                                ProviderKind::Anthropic => "anthropic",
                                ProviderKind::OpenAi => "openai",
                                ProviderKind::CommandCode => "commandcode",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut p.kind, ProviderKind::Anthropic, "anthropic");
                                ui.selectable_value(&mut p.kind, ProviderKind::OpenAi, "openai (compatible)");
                                ui.selectable_value(&mut p.kind, ProviderKind::CommandCode, "commandcode");
                            });
                    });
                    egui::Grid::new("pgrid").num_columns(2).show(ui, |ui| {
                        ui.label("model");
                        ui.text_edit_singleline(&mut p.model);
                        ui.end_row();
                        ui.label("api_key_env");
                        let mut v = p.api_key_env.clone().unwrap_or_default();
                        if ui.text_edit_singleline(&mut v).changed() {
                            p.api_key_env = if v.is_empty() { None } else { Some(v) };
                        }
                        ui.end_row();
                        ui.label("api_key");
                        let mut v = p.api_key.clone().unwrap_or_default();
                        if ui.add(egui::TextEdit::singleline(&mut v).password(true).desired_width(220.0)).changed() {
                            p.api_key = if v.is_empty() { None } else { Some(v) };
                        }
                        ui.end_row();
                        ui.label("base_url");
                        let mut v = p.base_url.clone().unwrap_or_default();
                        if ui.text_edit_singleline(&mut v).changed() {
                            p.base_url = if v.is_empty() { None } else { Some(v) };
                        }
                        ui.end_row();
                        ui.label("max_tokens");
                        let mut v = p.max_tokens.map(|n| n.to_string()).unwrap_or_default();
                        if ui.text_edit_singleline(&mut v).changed() {
                            p.max_tokens = v.parse().ok();
                        }
                        ui.end_row();
                    });
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(t("default provider:"));
                let names: Vec<String> = buf.cfg.providers.keys().cloned().collect();
                egui::ComboBox::from_id_salt("defp").selected_text(&buf.cfg.provider).show_ui(ui, |ui| {
                    for n in names {
                        ui.selectable_value(&mut buf.cfg.provider, n.clone(), n);
                    }
                });
            });
            }
            if buf.tab == 1 {
            ui.heading(t("mcp servers"));
            if buf.cfg.mcp_servers.is_empty() {
                ui.weak(t("no servers configured"));
            }
            for name in buf.cfg.mcp_servers.keys().cloned().collect::<Vec<_>>() {
                ui.horizontal(|ui| {
                    let mut en = buf.cfg.mcp_servers.get(&name).map(|s| s.enabled).unwrap_or(false);
                    if ui.checkbox(&mut en, "").on_hover_text(t("enabled")).changed() {
                        if let Some(s) = buf.cfg.mcp_servers.get_mut(&name) {
                            s.enabled = en;
                        }
                    }
                    let target = buf.cfg.mcp_servers.get(&name).and_then(|s| {
                        s.url.clone().or_else(|| Some(format!("npx {}", s.command)))
                    });
                    let resp = ui.selectable_label(buf.sel_mcp.as_deref() == Some(name.as_str()), &name);
                    let resp = match &target {
                        Some(v) => resp.on_hover_text(v),
                        None => resp,
                    };
                    if resp.clicked() {
                        buf.sel_mcp = Some(name.clone());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("🗑").on_hover_text(t("delete")).clicked() {
                            buf.cfg.mcp_servers.remove(&name);
                            if buf.sel_mcp.as_deref() == Some(name.as_str()) {
                                buf.sel_mcp = None;
                            }
                        }
                    });
                });
            }
            ui.horizontal(|ui| {
                ui.label(t("add:"));
                ui.text_edit_singleline(&mut buf.new_mcp);
                if ui.button("＋").clicked() && !buf.new_mcp.trim().is_empty() {
                    let name = buf.new_mcp.trim().to_string();
                    buf.cfg.mcp_servers.entry(name.clone()).or_insert_with(McpServerConf::default);
                    buf.sel_mcp = Some(name);
                    buf.new_mcp.clear();
                }
            });
            if let Some(name) = buf.sel_mcp.clone() {
                if let Some(s) = buf.cfg.mcp_servers.get_mut(&name) {
                    ui.group(|ui| {
                        egui::Grid::new("mgrid").num_columns(2).show(ui, |ui| {
                            ui.label(t("command"));
                            ui.text_edit_singleline(&mut s.command);
                            ui.end_row();
                            ui.label("url (hosted)");
                            let mut u = s.url.clone().unwrap_or_default();
                            if ui.text_edit_singleline(&mut u).changed() {
                                s.url = if u.trim().is_empty() { None } else { Some(u.trim().to_string()) };
                            }
                            ui.end_row();
                            ui.label("oauth client id");
                            let mut cid = s.oauth_client_id.clone().unwrap_or_default();
                            if ui.text_edit_singleline(&mut cid).changed() {
                                s.oauth_client_id = if cid.trim().is_empty() { None } else { Some(cid.trim().to_string()) };
                            }
                            ui.end_row();
                            ui.label("oauth client secret");
                            let mut cs = s.oauth_client_secret.clone().unwrap_or_default();
                            if ui.add(egui::TextEdit::singleline(&mut cs).password(true)).changed() {
                                s.oauth_client_secret = if cs.trim().is_empty() { None } else { Some(cs.trim().to_string()) };
                            }
                            ui.end_row();
                            ui.label(t("args (space sep)"));
                            let mut a = s.args.join(" ");
                            if ui.text_edit_singleline(&mut a).changed() {
                                s.args = a.split_whitespace().map(String::from).collect();
                            }
                            ui.end_row();
                            ui.label(t("env (K=V per line)"));
                            let mut e = s.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
                            if ui.add(egui::TextEdit::multiline(&mut e).desired_rows(3)).changed() {
                                s.env = e.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                            }
                            ui.end_row();
                        });
                    });
                }
            }
            }
            if buf.tab == 2 {
            ui.heading(t("skills"));
            for s in crate::config::skills() {
                ui.horizontal(|ui| {
                    ui.label(&s.name);
                    ui.weak(if s.managed { "amty" } else { t("shared") });
                    if s.managed && ui.small_button("✕").clicked() {
                        *skill_rm = Some(s.name.clone());
                    }
                });
                ui.add(
                    egui::Label::new(egui::RichText::new(format!("{} — {}", s.desc, s.path.display())).weak().small())
                        .wrap(),
                );
            }
            ui.weak(t("to add a skill: ask in chat (npx skills add works if Node.js is installed), or drop a SKILL.md in the skills dir"));
            }
            if buf.tab == 3 {
            ui.heading(t("behavior"));
            ui.horizontal(|ui| {
                ui.label(t("approval:"));
                egui::ComboBox::from_id_salt("ap")
                    .selected_text(match buf.cfg.approval {
                        ApprovalMode::Ask => "ask",
                        ApprovalMode::Auto => "auto",
                        ApprovalMode::Allowlist => "allowlist",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut buf.cfg.approval, ApprovalMode::Ask, "ask");
                        ui.selectable_value(&mut buf.cfg.approval, ApprovalMode::Auto, "auto");
                        ui.selectable_value(&mut buf.cfg.approval, ApprovalMode::Allowlist, "allowlist");
                    });
            });
            ui.label(t("allowed command prefixes (one per line):"));
            let mut ac = buf.cfg.allow_commands.join("\n");
            if ui.add(egui::TextEdit::multiline(&mut ac).desired_rows(3).desired_width(400.0)).changed() {
                buf.cfg.allow_commands = ac.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
            ui.label(t("allowed write paths (one per line):"));
            let mut ap = buf.cfg.allow_paths.join("\n");
            if ui.add(egui::TextEdit::multiline(&mut ap).desired_rows(3).desired_width(400.0)).changed() {
                buf.cfg.allow_paths = ap.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
            ui.label(t("system prompt:"));
            ui.add(egui::TextEdit::multiline(&mut buf.cfg.system_prompt).desired_rows(4).desired_width(520.0));
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(t("import Claude Desktop mcpServers")).clicked() {
                *import = true;
            }
            if ui.button(t("save")).clicked() {
                *save = true;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkify_wraps_bare_urls() {
        assert_eq!(linkify("see https://example.com/x now"), "see <https://example.com/x> now");
        // url at end of line — newline must not leak into the autolink
        assert_eq!(linkify("url https://a.b/c\n"), "url <https://a.b/c>\n");
        assert_eq!(linkify("https://a.b/c"), "<https://a.b/c>");
        // trailing punctuation stays outside
        assert_eq!(linkify("see https://a.b/c."), "see <https://a.b/c>.");
    }

    #[test]
    fn linkify_leaves_markdown_and_code_alone() {
        assert_eq!(linkify("[t](https://a.b)"), "[t](https://a.b)");
        assert_eq!(linkify("<https://a.b>"), "<https://a.b>");
        assert_eq!(linkify("`https://a.b`"), "`https://a.b`");
        assert_eq!(linkify("```\nhttps://a.b\n```"), "```\nhttps://a.b\n```");
    }

    #[test]
    fn short_tool_strips_mcp_prefix() {
        assert_eq!(short_tool("mcp__windows-mcp__Snapshot"), "Snapshot");
        assert_eq!(short_tool("shell"), "shell");
    }

    #[test]
    fn tool_runs_group_calls_with_their_results() {
        use crate::types::{Block, ChatMessage};
        let use_msg = ChatMessage {
            role: crate::types::Role::Assistant,
            blocks: vec![
                Block::ToolUse { id: "a".into(), name: "shell".into(), input: serde_json::json!({}) },
                Block::ToolUse { id: "b".into(), name: "fs_read".into(), input: serde_json::json!({}) },
            ],
        };
        let res_msg = ChatMessage::tool_results(vec![
            Block::ToolResult { tool_use_id: "a".into(), content: "x".into(), is_error: false },
            Block::ToolResult { tool_use_id: "b".into(), content: "y".into(), is_error: false },
        ]);
        let msgs = vec![ChatMessage::user("hi"), use_msg, res_msg];
        // the run starts after the user text, covers call+result, and counts 2 tools
        assert_eq!(tool_run_len(&msgs, 1), 2);
        assert_eq!(tool_use_count(&msgs, 1), 2);
        // a plain user message is not part of a run
        assert!(is_tool_only(&msgs[1]));
        assert!(!is_tool_only(&msgs[0]));
    }

    #[test]
    fn consecutive_tool_rounds_merge_into_one_run() {
        use crate::types::{Block, ChatMessage};
        let call = |id: &str| ChatMessage {
            role: crate::types::Role::Assistant,
            blocks: vec![Block::ToolUse { id: id.into(), name: "shell".into(), input: serde_json::json!({}) }],
        };
        let res = |id: &str| {
            ChatMessage::tool_results(vec![Block::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }])
        };
        let msgs = vec![
            ChatMessage::user("hi"),
            call("a"),
            res("a"),
            call("b"),
            res("b"),
            ChatMessage::user("stop"),
        ];
        // both rounds plus their results collapse into a single group
        assert_eq!(tool_run_len(&msgs, 1), 4);
        assert_eq!(tool_use_count(&msgs, 1), 2);
    }
}

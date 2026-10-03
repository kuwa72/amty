use crate::app::App;
use eframe::egui;
use crate::config::{ApprovalMode, Config, McpServerConf, ProviderKind};
use crate::session::Session;
use crate::types::{AppEvent, Block, EvKind, Role};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub fn run(rt: tokio::runtime::Runtime, app: Arc<App>) -> eframe::Result<()> {
    let handle = rt.handle().clone();
    std::thread::spawn(move || rt.block_on(std::future::pending::<()>()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([560.0, 380.0])
            .with_app_id("amty"),
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
            fonts
                .font_data
                .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
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
    settings: Option<SettingsBuf>,
    show_mcp: bool,
    /// catalog entry selected for install (id into catalog::CATALOG)
    cat_sel: Option<&'static str>,
    /// env field buffers for the selected catalog entry
    cat_env: HashMap<String, String>,
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
            settings: None,
            show_mcp: false,
            cat_sel: None,
            cat_env: HashMap::new(),
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
                for meta in self.app.sessions.list() {
                    let selected = meta.id == self.current;
                    let running = *self.running.get(&meta.id).unwrap_or(&false);
                    let label = if running { format!("● {}", meta.title) } else { meta.title.clone() };
                    if ui.selectable_label(selected, label).clicked() {
                        self.current = meta.id.clone();
                        self.view_dirty = true;
                    }
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
                let suggestions = kind.map(crate::provider::model_suggestions).unwrap_or_default();
                if !suggestions.is_empty() {
                    ui.menu_button("▾", |ui| {
                        for s in suggestions {
                            if ui.button(&s).clicked() {
                                let sid = self.ensure_session();
                                self.app.sessions.set_fields(&sid, None, Some(s));
                                self.view_dirty = true;
                                ui.close();
                            }
                        }
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
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 90.0).max(100.0);
                let hint = self.tr("message (Enter to send, Shift+Enter for newline)");
                let resp = ui.add_sized(
                    [w, 64.0],
                    egui::TextEdit::multiline(&mut self.input).hint_text(hint),
                );
                let mut do_send = ui.button(self.tr("send")).clicked();
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
                let running = *self.running.get(&self.current).unwrap_or(&false);
                if running && ui.button(self.tr("stop")).clicked() {
                    self.app.cancel(&self.current);
                }
            });
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
            ui.horizontal(|ui| {
                if let Some(s) = &self.view {
                    ui.heading(&s.title);
                } else {
                    ui.heading("amty");
                }
                ui.weak(format!("{prov} / {model}"));
                if let Some(t) = self.live_tool.get(&self.current) {
                    ui.weak(format!("⏳ {t}"));
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
                let lang = self.app.cfg.read().unwrap().lang.clone();
                if let Some(s) = self.view.clone() {
                    for msg in &s.messages {
                        render_message(ui, &mut self.mark, msg, &lang);
                    }
                } else {
                    ui.weak(self.tr("send a message to start"));
                }
                if let Some(t) = self.live_text.get(&self.current) {
                    if !t.is_empty() {
                        ui.colored_label(ui.visuals().weak_text_color(), self.tr("assistant"));
                        egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.mark, t);
                    }
                }
            });
        });

        // ----- approvals -----
        let mut resolved: Vec<(String, bool)> = vec![];
        for p in &self.pending {
            egui::Window::new(format!("{} {}", self.tr("approve:"), p.tool))
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(&p.detail);
                    ui.horizontal(|ui| {
                        if ui.button(self.tr("allow")).clicked() {
                            resolved.push((p.id.clone(), true));
                        }
                        if ui.button(self.tr("deny")).clicked() {
                            resolved.push((p.id.clone(), false));
                        }
                    });
                });
        }
        for (id, allow) in resolved {
            self.app.approvals.resolve(&id, allow);
            self.pending.retain(|p| p.id != id);
        }

        if self.show_mcp {
            let lang = self.app.cfg.read().unwrap().lang.clone();
            let ja = lang == "ja";
            let t = |k: &'static str| crate::i18n::t(&lang, k);
            let mut install_target: Option<&'static str> = None;
            let mut auth_target: Option<&'static str> = None;
            egui::Window::new(t("mcp servers")).open(&mut self.show_mcp).default_size([440.0, 380.0]).show(&ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (name, status, enabled) in self.app.mcp.statuses() {
                        let st = match status {
                            crate::mcp::Status::Disabled => t("disabled").to_string(),
                            crate::mcp::Status::Connecting => t("connecting…").to_string(),
                            crate::mcp::Status::Connected(n) => format!("{n} {}", t("tools")),
                            crate::mcp::Status::Failed(e) => format!("{} {e}", t("failed:")),
                        };
                        ui.horizontal(|ui| {
                            ui.label(format!("{name}:"));
                            ui.weak(st);
                            ui.weak(if enabled { t("enabled") } else { t("off") });
                        });
                    }
                    ui.separator();
                    ui.heading(t("catalog"));
                    if !crate::catalog::node_available() {
                        ui.colored_label(egui::Color32::from_rgb(255, 200, 80), t("Node.js (npx) is required to install these"));
                    }
                    let installed = self.app.cfg.read().unwrap().mcp_servers.clone();
                    for entry in crate::catalog::CATALOG {
                        let is_installed = installed.contains_key(entry.id);
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(entry.label).strong());
                                if is_installed {
                                    ui.weak(t("installed"));
                                } else {
                                    ui.weak(format!("npx {}", entry.package));
                                    if self.cat_sel == Some(entry.id) {
                                        if ui.small_button("✕").clicked() {
                                            self.cat_sel = None;
                                        }
                                    } else if ui.button(t("install")).clicked() {
                                        self.cat_sel = Some(entry.id);
                                        self.cat_env.clear();
                                    }
                                }
                            });
                            ui.weak(if ja { entry.desc_ja } else { entry.desc });
                            // OAuth/auth runner (google-family servers)
                            if entry.auth_cmd.is_some() {
                                let running = self.app.auth.lock().unwrap().get(entry.id).map(|a| a.running).unwrap_or(false);
                                ui.horizontal(|ui| {
                                    if running {
                                        ui.weak(t("auth running…"));
                                    } else if ui.button(t("auth")).clicked() {
                                        auth_target = Some(entry.id);
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
                            if self.cat_sel == Some(entry.id) {
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
            if let Some(id) = install_target {
                self.catalog_install(id);
            }
            if let Some(id) = auth_target {
                let started = self.app.clone();
                match started.start_auth(id) {
                    Ok(_) => self.toast(t("auth started")),
                    Err(e) => self.toast(format!("{} {e}", t("install failed: "))),
                }
            }
        }

        if self.settings.is_some() {
            let mut buf = self.settings.take().unwrap();
            let mut open = true;
            let mut save = false;
            let mut import = false;
            let lang = self.app.cfg.read().unwrap().lang.clone();
            settings_window(&ctx, &lang, &mut open, &mut buf, &mut save, &mut import);
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

fn render_message(ui: &mut egui::Ui, mark: &mut egui_commonmark::CommonMarkCache, msg: &crate::types::ChatMessage, lang: &str) {
    let t = |k: &'static str| crate::i18n::t(lang, k);
    let (label, color) = match msg.role {
        Role::User => (t("you"), egui::Color32::from_rgb(120, 180, 255)),
        Role::Assistant => (t("assistant"), egui::Color32::from_rgb(160, 220, 160)),
    };
    let is_tool_msg = msg.blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }));
    if !is_tool_msg {
        ui.colored_label(color, label);
    }
    for b in &msg.blocks {
        match b {
            Block::Text { text } => {
                egui_commonmark::CommonMarkViewer::new().show(ui, mark, text);
            }
            Block::ToolUse { name, input, .. } => {
                egui::CollapsingHeader::new(format!("🔧 {name}"))
                    .id_salt(format!("tu_{name}_{}", msg.blocks.len()))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(input.to_string()).monospace().small(),
                        ).wrap());
                    });
            }
            Block::ToolResult { content, is_error, .. } => {
                let head = if *is_error { t("✗ result (error)") } else { t("✓ result") };
                egui::CollapsingHeader::new(head)
                    .id_salt(format!("tr_{}", content.as_ptr() as usize))
                    .show(ui, |ui| {
                        let t = if content.len() > 3000 { format!("{}…", &content[..3000]) } else { content.clone() };
                        ui.add(egui::Label::new(egui::RichText::new(t).monospace().small()).wrap());
                    });
            }
        }
    }
    ui.add_space(8.0);
}

fn settings_window(ctx: &egui::Context, lang: &str, open: &mut bool, buf: &mut SettingsBuf, save: &mut bool, import: &mut bool) {
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
            ui.heading(t("providers"));
            ui.horizontal(|ui| {
                for name in buf.cfg.providers.keys().cloned().collect::<Vec<_>>() {
                    if ui.selectable_label(buf.sel_provider == name, &name).clicked() {
                        buf.sel_provider = name;
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

            ui.separator();
            ui.heading(t("mcp servers"));
            for name in buf.cfg.mcp_servers.keys().cloned().collect::<Vec<_>>() {
                ui.horizontal(|ui| {
                    let mut en = buf.cfg.mcp_servers.get(&name).map(|s| s.enabled).unwrap_or(false);
                    if ui.checkbox(&mut en, "").changed() {
                        if let Some(s) = buf.cfg.mcp_servers.get_mut(&name) {
                            s.enabled = en;
                        }
                    }
                    if ui.selectable_label(buf.sel_mcp.as_deref() == Some(name.as_str()), &name).clicked() {
                        buf.sel_mcp = Some(name.clone());
                    }
                    if ui.small_button("✕").clicked() {
                        buf.cfg.mcp_servers.remove(&name);
                        if buf.sel_mcp.as_deref() == Some(name.as_str()) {
                            buf.sel_mcp = None;
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label(t("add:"));
                ui.text_edit_singleline(&mut buf.new_mcp);
                if ui.button("＋").clicked() && !buf.new_mcp.trim().is_empty() {
                    let name = buf.new_mcp.trim().to_string();
                    buf.cfg.mcp_servers.entry(name.clone()).or_insert(McpServerConf {
                        command: String::new(),
                        args: vec![],
                        env: Default::default(),
                        enabled: true,
                    });
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
            ui.separator();
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

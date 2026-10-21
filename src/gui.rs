use crate::app::App;
use eframe::egui;
use crate::config::{ApprovalMode, Config, McpServerConf, ProviderKind};
use crate::session::Session;
use crate::types::{AppEvent, Block, EvKind, Role};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    let gpu_lost = Arc::new(AtomicBool::new(false));
    let flag = gpu_lost.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "amty",
            options,
            Box::new(move |cc| {
                install_cjk_fonts(&cc.egui_ctx);
                guard_gpu_loss(cc, flag.clone());
                Ok(Box::new(GuiApp::new(handle, app)))
            }),
        )
    }));
    match result {
        Ok(r) => r,
        Err(payload) => {
            if gpu_lost.load(Ordering::SeqCst) {
                // A panic escaping wgpu-hal on device removal gets the same
                // treatment as the orderly path: relaunch, exit cleanly.
                respawn_self();
                Ok(())
            } else {
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// Spawn a fresh copy of this binary (GUI mode). Used when the GPU device is
/// lost: wgpu can't migrate a device to another adapter, so the only way to
/// keep running is a new process. Sessions persist continuously, so the new
/// instance picks up where this one left off.
fn respawn_self() {
    // Both the device-lost callback and the panic path below may try to
    // restart us — make sure only the first one does.
    static RESPAWNED: AtomicBool = AtomicBool::new(false);
    if RESPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// A removed dGPU/eGPU (or a driver reset) kills the wgpu device; without
/// these hooks wgpu panics on the first uncaptured error and the app dies.
/// On a real loss (`Unknown` reason — `Destroyed` is normal teardown) we
/// relaunch ourselves and close. Errors from the dying device are then
/// swallowed instead of panicking; errors with no device loss still panic,
/// preserving the default "loud failure" behavior for genuine bugs.
fn guard_gpu_loss(cc: &eframe::CreationContext, gpu_lost: Arc<AtomicBool>) {
    let Some(state) = &cc.wgpu_render_state else {
        return;
    };
    let ctx = cc.egui_ctx.clone();
    let lost = gpu_lost.clone();
    state.device.set_device_lost_callback(move |reason, msg| {
        if reason != eframe::wgpu::DeviceLostReason::Unknown {
            return;
        }
        lost.store(true, Ordering::SeqCst);
        eprintln!("amty: GPU device lost ({msg}); restarting");
        respawn_self();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    });
    state.device.on_uncaptured_error(Arc::new(move |err| {
        if gpu_lost.load(Ordering::SeqCst) {
            eprintln!("amty: ignoring wgpu error after device loss: {err}");
        } else {
            panic!("{err}");
        }
    }));
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

/// If the window ended up fully off-screen — e.g. the monitor it was on was
/// unplugged while running, or a display arrangement changed — recenter it on
/// the nearest monitor. eframe already clamps the position it restores at
/// startup; this catches disconnects that happen later. Runs on a 1 s repaint
/// heartbeat so a monitor removal rescues the window even if no winit event
/// reaches us.
#[cfg(target_os = "windows")]
fn rescue_offscreen_window(ctx: &egui::Context) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        MONITOR_DEFAULTTONULL,
    };

    ctx.request_repaint_after(Duration::from_secs(1));

    let (outer, minimized) =
        ctx.input(|i| (i.viewport().outer_rect, i.viewport().minimized));
    let Some(outer) = outer else { return };
    if minimized == Some(true) {
        return; // iconic rect (-32000,-32000) is not the real position
    }
    let ppp = ctx.pixels_per_point();
    let px = |v: f32| (v * ppp).round() as i32;
    let rect = RECT {
        left: px(outer.min.x),
        top: px(outer.min.y),
        right: px(outer.max.x),
        bottom: px(outer.max.y),
    };
    unsafe {
        if !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_null() {
            return; // still intersects some display
        }
        let hmon = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        if hmon.is_null() {
            return;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi) == 0 {
            return;
        }
        let w = mi.rcWork;
        let x = w.left + (w.right - w.left - (rect.right - rect.left)) / 2;
        let y = w.top + (w.bottom - w.top - (rect.bottom - rect.top)) / 2;
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            x as f32 / ppp,
            y as f32 / ppp,
        )));
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
    /// active tab index (providers / mcp / skills / behavior / evolve)
    tab: usize,
    /// cached MEMORY.md contents for the evolve tab (loaded once per open)
    memory: String,
}

/// `YYYY-MM-DD HH:MM` (UTC) from a unix timestamp — lives in `session` so the
/// CLI prints the same format.
fn stamp(secs: u64) -> String {
    crate::session::fmt_stamp(secs)
}

/// i18n key for a `SearchHit.role` value (English key = the fallback).
fn role_label(role: &str) -> &'static str {
    match role {
        "title" => "title",
        "user" => "you",
        "assistant" => "assistant",
        _ => "tool",
    }
}

pub struct GuiApp {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    rx: broadcast::Receiver<AppEvent>,
    current: String,
    /// composer buffer for `current` only — swapped per session (drafts
    /// persist in each session's meta line)
    input: String,
    /// `Some` = `input` was edited since the last draft persist; the value
    /// is the last edit time for the debounced flush
    draft_dirty: Option<Instant>,
    /// images attached to the next message (clipboard paste / file drop),
    /// shown as thumbnails under the input box and sent with the text
    pending_images: Vec<PendingImage>,
    /// decoded textures for images in the session view, keyed by content id
    /// (base64 length + size) so we decode each image only once
    image_tex: HashMap<String, egui::TextureHandle>,
    /// streamed text not yet persisted into the session
    live_text: HashMap<String, String>,
    /// currently executing tool label per session
    live_tool: HashMap<String, String>,
    /// running token totals for the session's current turn (header display)
    live_usage: HashMap<String, crate::types::Usage>,
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
    /// sidebar search box: live title filter, Enter = full-text search
    search_buf: String,
    /// full-text hits from the last Enter (`None` = title filtering only)
    search_hits: Option<Vec<crate::session::SearchHit>>,
    /// value of `search_buf` the current `search_hits` were computed for
    search_for: String,
    /// Windows: Ctrl+V is currently held (rising-edge debounce for image paste)
    paste_hot: bool,
}

impl GuiApp {
    fn new(rt: tokio::runtime::Handle, app: Arc<App>) -> Self {
        let rx = app.events.subscribe();
        // Reopen the session that was selected when we last closed (falls back
        // to the most recently updated one). That session carries its own
        // provider/model, so the same pair comes back with it.
        let current = app
            .cfg
            .read()
            .unwrap()
            .last_session
            .clone()
            .and_then(|id| app.sessions.resolve(&id))
            .or_else(|| app.sessions.list().first().map(|s| s.id.clone()))
            .unwrap_or_default();
        let input = app.sessions.draft(&current).unwrap_or_default();
        Self {
            app,
            rt,
            rx,
            current,
            input,
            draft_dirty: None,
            pending_images: Vec::new(),
            image_tex: HashMap::new(),
            live_text: HashMap::new(),
            live_tool: HashMap::new(),
            live_usage: HashMap::new(),
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
            search_buf: String::new(),
            search_hits: None,
            search_for: String::new(),
            paste_hot: false,
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
            EvKind::Usage { cumulative, .. } => {
                // show running session spend in the status/toast line
                self.live_usage.insert(ev.session.clone(), cumulative);
                self.view_dirty = true;
            }
            EvKind::Compacted { dropped } => {
                self.toast(format!("{} ({dropped})", self.tr("context compacted")));
                self.view_dirty = true;
            }
            EvKind::Evolved { summary } => {
                self.toast(format!("{}: {summary}", self.tr("self-evolved")));
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
            let prev_id = self.view.as_ref().map(|s| s.id.clone());
            self.view = if self.current.is_empty() {
                None
            } else {
                self.app.sessions.resolve(&self.current).and_then(|id| self.app.sessions.get(&id))
            };
            if let Some(v) = &self.view {
                self.current = v.id.clone();
            }
            // decoded image textures belong to the viewed session — drop
            // them on switch so other sessions' images stop holding memory
            if self.view.as_ref().map(|s| s.id.as_str()) != prev_id.as_deref() {
                self.image_tex.clear();
            }
            self.view_dirty = false;
            self.remember_last_session();
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn tr(&self, key: &'static str) -> &'static str {
        crate::i18n::t(&self.app.cfg.read().unwrap().lang, key)
    }
    /// Remember which session is selected, so the next launch reopens it (and
    /// with it the session's provider/model). `last_session` holds the *id*;
    /// `sessions.resolve` accepts a unique prefix too, so a stale id simply
    /// falls back to the most recent session on startup.
    fn remember_last_session(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let mut cfg = self.app.cfg.write().unwrap();
        if cfg.last_session.as_deref() == Some(self.current.as_str()) {
            return;
        }
        cfg.last_session = Some(self.current.clone());
        if let Err(e) = cfg.save(&self.app.cfg_path) {
            eprintln!("amty: could not save last_session: {e}");
        }
    }

    fn ensure_session(&mut self) -> String {
        if self.current.is_empty() {
            let (p, m) = self.session_provider();
            let id = self.app.sessions.create(Some(p), Some(m));
            self.swap_draft(id);
            self.remember_last_session();
        }
        self.current.clone()
    }

    /// Swap the composer buffer when `current` changes: the outgoing
    /// session's draft is persisted and the incoming session's draft is
    /// loaded, so text typed for one chat can never be sent to another.
    /// Text typed while no session was selected (before first send)
    /// carries into the newly selected session.
    fn swap_draft(&mut self, new_id: String) {
        let old = std::mem::replace(&mut self.current, new_id);
        if old.is_empty() && !self.input.is_empty() {
            self.draft_dirty = Some(Instant::now());
            return;
        }
        if !old.is_empty() {
            self.app.sessions.set_draft(&old, &self.input);
        }
        self.input = self.app.sessions.draft(&self.current).unwrap_or_default();
        self.draft_dirty = None;
    }

    /// Flush a dirty composer draft to the current session's meta line.
    fn flush_draft(&mut self) {
        if !self.current.is_empty() {
            self.app.sessions.set_draft(&self.current, &self.input);
        }
        self.draft_dirty = None;
    }

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() && self.pending_images.is_empty() {
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
        let images = std::mem::take(&mut self.pending_images);
        let blocks = message_blocks(&text, images);
        self.input.clear();
        self.app.sessions.set_draft(&sid, "");
        self.draft_dirty = None;
        self.last_error.remove(&sid);
        self.view_dirty = true;
        let app_ref = self.app.clone();
        let sid_err = sid.clone();
        self.rt.spawn(async move {
            if let Err(e) = crate::agent::start_run_blocks(&app, &sid2, blocks) {
                app_ref.emit(&sid_err, EvKind::Error { message: e.to_string() });
            }
        });
    }

    /// Attach a clipboard image result: push on success, toast on failure.
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
        #[cfg(target_os = "windows")]
        rescue_offscreen_window(&ctx);
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        self.refresh_view();
        // debounced draft persist: the composer buffer lives in GUI memory,
        // so a dirty draft is flushed to the session meta ~1s after typing
        // stops (switching sessions or sending flushes it immediately)
        if let Some(t) = self.draft_dirty {
            if t.elapsed() >= Duration::from_secs(1) {
                self.flush_draft();
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        // drag & drop: dropped file paths are appended to the input box;
        // paint a drop hint while files are hovered over the window
        let (dropped, hovering) = ctx.input(|i| {
            (
                i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>(),
                !i.raw.hovered_files.is_empty(),
            )
        });
        for p in &dropped {
            // an image file is attached as an image, everything else becomes a path
            if is_image_path(p) {
                match std::fs::read(p).map_err(|e| e.to_string()).and_then(|b| PendingImage::from_bytes(&b)) {
                    Ok(img) => self.pending_images.push(img),
                    Err(e) => self.toast(format!("{} {e}", self.tr("image load failed:"))),
                }
                continue;
            }
            let s = p.display().to_string();
            let s = if s.contains(char::is_whitespace) { format!("\"{s}\"") } else { s };
            if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
                self.input.push(' ');
            }
            self.input.push_str(&s);
            self.draft_dirty = Some(Instant::now());
        }

        // Ctrl+V with an image on the clipboard (a screenshot) attaches it.
        // egui's Paste event only carries text, and egui-winit swallows the raw
        // Key::V event entirely when the clipboard holds no text (image-only
        // paste), so key_pressed(V) never fires. On Windows we therefore poll
        // the keyboard directly: the rising edge of Ctrl+V with an image on
        // the clipboard attaches it (a rising-edge latch debounces repeats).
        // On other platforms the egui event still works for image-only
        // clipboards there because egui-winit emits Paste for the physical
        // key fallback path; the key_pressed check stays for non-Windows.
        let attach_now = if cfg!(target_os = "windows") {
            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_SHIFT, VK_V,
                };
                let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
                let ctrl_held = down(VK_CONTROL as _)
                    || down(VK_LCONTROL as _)
                    || down(VK_RCONTROL as _);
                let held = ctrl_held && down(VK_V as _);
                let rising = held && !self.paste_hot;
                self.paste_hot = held;
                // GetAsyncKeyState is *global*: without a focus check amty
                // steals clipboard images when the user pastes into another
                // app while amty sits in the background.
                let focused = ctx
                    .input(|i| i.viewport().focused)
                    .unwrap_or(true);
                rising && ctrl_held && !down(VK_SHIFT as _) && focused
            }
            #[cfg(not(target_os = "windows"))]
            {
                false
            }
        } else {
            ctx.input(|i| {
                i.key_pressed(egui::Key::V) && (i.modifiers.command || i.modifiers.ctrl)
            })
        };
        if attach_now {
            match clipboard_image() {
                Some(Ok(img)) => self.pending_images.push(img),
                Some(Err(e)) => {
                    self.toast(format!("{} {e}", self.tr("image load failed:")));
                }
                // no image on the clipboard (or clipboard unreadable): let the
                // normal text paste path do its thing
                None => {}
            }
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
        // 250 outer keeps the provider combo + model row readable; users can
        // still drag narrower down to 180 without the footer breaking.
        egui::Panel::left("side").default_size(250.0).min_size(180.0).show(ui, |ui| {
            // The provider/model/settings footer is claimed FIRST (as a nested
            // bottom panel) and the session list then gets whatever height is
            // left. Previously the list was an unbounded ScrollArea in the same
            // top-down flow, so it grew with the content and pushed the footer
            // off the bottom of the screen instead of scrolling.
            egui::Panel::bottom("side_footer")
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.separator();
                    // provider / model quick switch (per-session override)
                    let (prov, model) = self.session_provider();
                    let names: Vec<String> = self.app.cfg.read().unwrap().providers.keys().cloned().collect();
                    let mut sel = prov.clone();
                    // size the combo to the leftover room after its label, so
                    // it never crosses the panel edge on narrow sidebars
                    let label_w = {
                        let galley = ui.painter().layout_no_wrap(
                            self.tr("provider").to_owned(),
                            egui::TextStyle::Body.resolve(ui.style()),
                            egui::Color32::WHITE,
                        );
                        galley.size().x + ui.spacing().item_spacing.x
                    };
                    egui::ComboBox::from_label(self.tr("provider"))
                        .width((ui.available_width() - label_w).max(ui.spacing().combo_width))
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
                    // model suggestions are fetched before the row so the
                    // ▾ button's real width can be measured and reserved
                    let kind = self.app.cfg.read().unwrap().providers.get(&prov).map(|p| p.kind);
                    let (suggestions, err) = match kind {
                        Some(crate::config::ProviderKind::CommandCode) => {
                            let conf = self.app.cfg.read().unwrap().providers.get(&prov).cloned().unwrap_or_default();
                            crate::provider::cmdc_models(&conf)
                        }
                        Some(crate::config::ProviderKind::OpenCode) => {
                            let conf = self.app.cfg.read().unwrap().providers.get(&prov).cloned().unwrap_or_default();
                            crate::provider::opencode_models(&conf)
                        }
                        _ => (kind.map(crate::provider::model_suggestions).unwrap_or_default(), None),
                    };
                    let suggestions_label = if suggestions.is_empty() {
                        None
                    } else {
                        Some(match &err {
                            None => format!("▾ ({})", suggestions.len()),
                            Some(_) => format!("▾ ({}) ⚠", suggestions.len()),
                        })
                    };
                    ui.horizontal(|ui| {
                        ui.label(self.tr("model"));
                        // keep room for the model-suggestions button, no matter
                        // how narrow the user dragged the side panel — measure
                        // the real text instead of guessing a fixed reserve
                        let menu_w = match &suggestions_label {
                            Some(lbl) => {
                                let galley = ui.painter().layout_no_wrap(
                                    lbl.clone(),
                                    egui::TextStyle::Button.resolve(ui.style()),
                                    egui::Color32::WHITE,
                                );
                                galley.size().x + ui.spacing().button_padding.x * 2.0 + 4.0
                            }
                            None => 0.0,
                        };
                        let edit_w = (ui.available_width() - menu_w).max(50.0);
                        let mut m = model.clone();
                        if ui.add(egui::TextEdit::singleline(&mut m).desired_width(edit_w)).lost_focus() && m != model {
                            let sid = self.ensure_session();
                            self.app.sessions.set_fields(&sid, None, Some(m));
                            self.view_dirty = true;
                        }
                        if let Some(lbl) = &suggestions_label {
                            ui.menu_button(lbl, |ui| {
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
                                    memory: std::fs::read_to_string(crate::config::memory_path())
                                        .unwrap_or_default(),
                                });
                            }
                        }
                        if ui.button(self.tr("mcp")).clicked() {
                            self.show_mcp = !self.show_mcp;
                        }
                    });
                });

            ui.horizontal(|ui| {
                ui.heading("amty");
                if ui.button(self.tr("＋ new")).clicked() {
                    let (p, m) = self.session_provider();
                    let id = self.app.sessions.create(Some(p), Some(m));
                    self.swap_draft(id);
                    self.view_dirty = true;
                    self.remember_last_session();
                }
            });
            // ----- search: typing filters titles, Enter searches message text -----
            // This row must never exceed the panel's width: egui 0.36 persists a
            // panel's *content* width as its size (PanelState.outer_rect), so an
            // overflowing row makes the sidebar grow a little every frame. The
            // ✕ button's real width (padding included) is measured and the
            // spacing between the two widgets subtracted, instead of guessing.
            ui.horizontal(|ui| {
                let hint = self.tr("search title, then Enter for full text");
                let show_clear = !self.search_buf.is_empty();
                let spacing = ui.spacing().item_spacing.x;
                let clear_w = if show_clear {
                    // ⏎/✕ — measured, not assumed: button padding varies with style
                    let galley = ui.painter().layout_no_wrap(
                        "✕".to_owned(),
                        egui::TextStyle::Button.resolve(ui.style()),
                        egui::Color32::WHITE,
                    );
                    galley.size().x + ui.spacing().button_padding.x * 2.0 + spacing
                } else {
                    0.0
                };
                let edit_w = (ui.available_width() - clear_w).max(24.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search_buf)
                        .hint_text(hint)
                        .desired_width(edit_w),
                );
                if show_clear && ui.small_button("✕").on_hover_text(self.tr("clear")).clicked() {
                    self.search_buf.clear();
                    self.search_hits = None;
                }
                let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if entered || (self.search_hits.is_some() && self.search_buf != self.search_for) {
                    // re-run on Enter, and refresh live while a result list is
                    // already shown (so the box stays honest after an edit)
                    if self.search_buf.trim().is_empty() {
                        self.search_hits = None;
                        self.search_for.clear();
                    } else if entered || self.search_hits.is_some() {
                        self.search_hits = Some(self.app.sessions.search(&self.search_buf, 50));
                        self.search_for = self.search_buf.clone();
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("sessions")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                if let Some(hits) = self.search_hits.clone() {
                    // ----- full-text result list: click a hit to open it -----
                    if hits.is_empty() {
                        ui.weak(self.tr("no matches"));
                    }
                    for (n, h) in hits.iter().enumerate() {
                        let head = format!("{} · {}", h.title, self.tr(role_label(&h.role)));
                        let sub = format!("{} {}\n{}", h.id, crate::session::fmt_stamp(h.updated), h.snippet);
                        // truncating labels/buttons keep each row inside the
                        // panel width (long unbroken snippets would otherwise
                        // widen the sidebar permanently — see the search row)
                        let resp = ui.add(
                            egui::Button::selectable(h.id == self.current, head)
                                .truncate()
                                .small(),
                        );
                        if resp.on_hover_text(sub).clicked() {
                            self.swap_draft(h.id.clone());
                            self.view_dirty = true;
                            self.remember_last_session();
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&h.snippet)
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .truncate(),
                        );
                        if n + 1 < hits.len() {
                            ui.separator();
                        }
                    }
                    return;
                }
                let filter = self.search_buf.trim().to_lowercase();
                let metas: Vec<_> = self
                    .app
                    .sessions
                    .list()
                    .into_iter()
                    .filter(|m| filter.is_empty() || m.title.to_lowercase().contains(&filter))
                    .collect();
                let mut delete: Option<String> = None;
                for meta in metas {
                    let selected = meta.id == self.current;
                    let running = *self.running.get(&meta.id).unwrap_or(&false);
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
                            // the edit field lives in the right pane's header —
                            // select the session so it shows up there
                            self.rename_id = Some(meta.id.clone());
                            self.rename_buf = meta.title.clone();
                            self.swap_draft(meta.id.clone());
                            self.view_dirty = true;
                            ui.close();
                        }
                        if ui.button(self.tr("delete")).clicked() {
                            delete = Some(meta.id.clone());
                            ui.close();
                        }
                    });
                    if resp.clicked() {
                        self.swap_draft(meta.id.clone());
                        self.view_dirty = true;
                        self.remember_last_session();
                    }
                }
                if let Some(id) = delete {
                    self.app.sessions.delete(&id);
                    self.app.purge_tool_cache(&id);
                    // drop retained per-session UI state for the deleted id
                    self.live_text.remove(&id);
                    self.live_tool.remove(&id);
                    self.live_usage.remove(&id);
                    self.running.remove(&id);
                    self.last_error.remove(&id);
                    if self.current == id {
                        self.swap_draft(String::new());
                    }
                    self.view_dirty = true;
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
            // ----- session files tray: paths created by tool calls -----
            // sits right above the input row like a task tray; a row opens
            // on click, right-click offers reveal/copy. Hidden when empty.
            if let Some(files) = self.view.as_ref().map(|s| s.files.clone()).filter(|f| !f.is_empty()) {
                let (t_open, t_reveal, t_copy, t_missing) = (
                    self.tr("open"),
                    self.tr("reveal in file manager"),
                    self.tr("copy path"),
                    self.tr("(no longer exists)"),
                );
                egui::CollapsingHeader::new(format!("📁 {} ({})", self.tr("session files"), files.len()))
                    .id_salt("session_files")
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("session_files_rows")
                            .max_height(140.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for f in &files {
                                    let missing = !std::path::Path::new(&f.path).exists();
                                    let mut text = egui::RichText::new(&f.path).monospace().small();
                                    if missing {
                                        text = text.weak();
                                    }
                                    let resp = ui.add(
                                        egui::Label::new(text).truncate().sense(egui::Sense::click()),
                                    );
                                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                                    let resp = resp.on_hover_text(format!(
                                        "{}\n{} · {}{}",
                                        f.path,
                                        f.tool,
                                        stamp(f.ts),
                                        if missing { format!("\n{t_missing}") } else { String::new() },
                                    ));
                                    if resp.clicked() {
                                        open_path(&f.path);
                                    }
                                    resp.context_menu(|ui| {
                                        if ui.button(t_open).clicked() {
                                            open_path(&f.path);
                                            ui.close();
                                        }
                                        if ui.button(t_reveal).clicked() {
                                            reveal_path(&f.path);
                                            ui.close();
                                        }
                                        if ui.button(t_copy).clicked() {
                                            ui.ctx().copy_text(f.path.clone());
                                            ui.close();
                                        }
                                    });
                                }
                            });
                    });
            }
            // thumbnails of images queued for the next message, each removable
            if !self.pending_images.is_empty() {
                let mut remove: Option<usize> = None;
                ui.horizontal_wrapped(|ui| {
                    for (i, img) in self.pending_images.iter_mut().enumerate() {
                        ui.vertical(|ui| {
                            let handle = img.texture(ui.ctx());
                            let (w, h) = (handle.size()[0] as f32, handle.size()[1] as f32);
                            let scale = (96.0 / w.max(1.0)).min(96.0 / h.max(1.0)).min(1.0);
                            ui.add(egui::Image::new((handle.id(), egui::vec2(w * scale, h * scale))).corner_radius(3.0));
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                });
                if let Some(i) = remove {
                    self.pending_images.remove(i);
                }
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
                    // Multiline TextEdit grows with content and has no internal
                    // scrolling — wrap it in a capped ScrollArea so a huge paste
                    // can't push the bottom panel (and the buttons) off-screen.
                    // The cap tracks window height so tall windows get a taller
                    // input before it starts scrolling.
                    let input_max_h = (ui.ctx().content_rect().height() * 0.3).clamp(120.0, 320.0);
                    let resp = egui::ScrollArea::vertical()
                        .id_salt("input_scroll")
                        .max_height(input_max_h)
                        .auto_shrink([false, true])
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.input)
                                    .hint_text(hint)
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY),
                            )
                        })
                        .inner;
                    if resp.changed() {
                        self.draft_dirty = Some(Instant::now());
                    }
                    // Text-edit style context menu. Cut/Copy/Select-all are
                    // forwarded as synthetic events — the focused TextEdit
                    // consumes them on the next frame (right-click doesn't
                    // steal its focus). Paste reads the clipboard ourselves:
                    // text becomes a Paste event, an image is attached.
                    resp.context_menu(|ui| {
                        let push = |ui: &mut egui::Ui, e: egui::Event| {
                            ui.ctx().input_mut(|i| i.events.push(e));
                            ui.close();
                        };
                        if ui.button(self.tr("cut")).clicked() {
                            push(ui, egui::Event::Cut);
                        }
                        if ui.button(self.tr("copy")).clicked() {
                            push(ui, egui::Event::Copy);
                        }
                        if ui.button(self.tr("paste")).clicked() {
                            let text = arboard::Clipboard::new()
                                .and_then(|mut c| c.get_text())
                                .ok()
                                .filter(|s| !s.is_empty());
                            match text {
                                Some(t) => push(ui, egui::Event::Paste(t)),
                                None => {
                                    if let Some(Ok(img)) = clipboard_image() {
                                        self.pending_images.push(img);
                                    }
                                    ui.close();
                                }
                            }
                        }
                        if ui.button(self.tr("select all")).clicked() {
                            push(
                                ui,
                                egui::Event::Key {
                                    key: egui::Key::A,
                                    physical_key: None,
                                    pressed: true,
                                    repeat: false,
                                    modifiers: egui::Modifiers::COMMAND,
                                },
                            );
                        }
                    });
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
                    let (sid, title) = (s.id.clone(), s.title.clone());
                    if self.rename_id.as_deref() == Some(sid.as_str()) {
                        // rename in the header: a wide field in the main pane,
                        // where IME works (the sidebar inline editor didn't)
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.rename_buf)
                                .desired_width(320.0),
                        );
                        if !resp.has_focus() {
                            resp.request_focus();
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            let t = self.rename_buf.trim().to_string();
                            if !t.is_empty() {
                                self.app.sessions.set_title(&sid, &t);
                            }
                            self.rename_id = None;
                            self.view_dirty = true;
                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            self.rename_id = None;
                        }
                    } else {
                        let r = ui.heading(&title);
                        if r.double_clicked() {
                            self.rename_id = Some(sid.clone());
                            self.rename_buf = title.clone();
                        }
                        r.on_hover_text(self.tr("double-click to rename"));
                    }
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
                // running token usage: the number that explains the bill, since
                // a long turn re-reads its (cached) prefix on every step
                if let Some(u) = self.live_usage.get(&self.current).filter(|u| !u.is_zero()) {
                    let total = u.input + u.output + u.cache_write;
                    let pct = if total > 0 { 100.0 * u.cache_read as f64 / total as f64 } else { 0.0 };
                    ui.weak(format!(
                        "↑{} ↓{} ⚡{:.0}%",
                        fmt_tokens(u.input), fmt_tokens(u.output), pct
                    ))
                    .on_hover_text(format!(
                        "in {} ({} uncached)\nout {}\ncache read {}\ncache write {}\nreasoning {}",
                        u.input, u.uncached_input(), u.output, u.cache_read, u.cache_write, u.reasoning
                    ));
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
                // egui's default scroll bar floats and allocates no width, so it
                // is drawn *over* the rightmost glyphs; reserve space instead.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::thin();
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
                            egui::Frame::new()
                                .inner_margin(egui::Margin { left: 0, right: 12, top: 0, bottom: 0 })
                                .show(ui, |ui| render_tool_run(ui, &s.messages, i, n, &lang, live));
                            i += n;
                        } else {
                            // keep glyphs clear of the right edge / scroll bar
                            let a = egui::Frame::new()
                                .inner_margin(egui::Margin { left: 0, right: 12, top: 0, bottom: 0 })
                                .show(ui, |ui| {
                                    render_message(ui, &mut self.mark, &mut self.image_tex, &mut row_rects, &s.messages[i], &lang, i, running_now)
                                })
                                .inner;
                            if let Some(a) = a {
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
                        egui::Frame::new()
                            .inner_margin(egui::Margin { left: 0, right: 12, top: 0, bottom: 0 })
                            .show(ui, |ui| {
                                render_md(ui, &mut self.mark, t);
                            });
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
                            self.swap_draft(id);
                            self.view_dirty = true;
                            self.remember_last_session();
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

    /// Last chance to persist the composer draft: the debounced flush may
    /// not have fired yet when the window closes.
    fn on_exit(&mut self) {
        self.flush_draft();
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

/// Open a path with the OS default handler (associated app / file manager).
fn open_path(path: &str) {
    #[cfg(target_os = "windows")]
    let r = {
        let mut c = std::process::Command::new("explorer");
        crate::catalog::no_window(&mut c);
        c.arg(path).spawn()
    };
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(not(any(unix, windows)))]
    let r: std::io::Result<std::process::Child> = Err(std::io::Error::other("unsupported platform"));
    let _ = r;
}

/// Reveal a path in the OS file manager. Linux has no standard "select this
/// file" call, so the containing folder is opened instead.
fn reveal_path(path: &str) {
    #[cfg(target_os = "windows")]
    let r = {
        let mut c = std::process::Command::new("explorer");
        crate::catalog::no_window(&mut c);
        c.arg(format!("/select,{path}")).spawn()
    };
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").args(["-R", path]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = {
        let p = std::path::Path::new(path);
        std::process::Command::new("xdg-open")
            .arg(p.parent().unwrap_or(p))
            .spawn()
    };
    #[cfg(not(any(unix, windows)))]
    let r: std::io::Result<std::process::Child> = Err(std::io::Error::other("unsupported platform"));
    let _ = r;
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
/// Compact token counts for the header: 1234 → 1.2k, 1234567 → 1.2M.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

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
            Block::Image { .. } => {}
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

#[allow(clippy::too_many_arguments)]
fn render_message(ui: &mut egui::Ui, mark: &mut egui_commonmark::CommonMarkCache, tex: &mut HashMap<String, egui::TextureHandle>, rows: &mut HashMap<usize, egui::Rect>, msg: &crate::types::ChatMessage, lang: &str, idx: usize, running: bool) -> Option<MsgAction> {
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
        // the ⋯ popup opens on a layer below the row, so the pointer leaves
        // the row rect as soon as it opens; keep the actions rendered while
        // the popup is open or the menu could never be clicked
        let menu_id = ui.id().with(idx);
        let show_actions = hovered || egui::Popup::is_id_open(ui.ctx(), menu_id);
        let head = ui.horizontal(|ui| {
            ui.colored_label(color, label);
            let text = msg.text_content();
            if show_actions && !text.trim().is_empty() {
                if ui.small_button(t("copy")).clicked() {
                    action = Some(MsgAction::Copy(text.clone()));
                }
                if ui.small_button(t("save")).clicked() {
                    action = Some(MsgAction::Save(text));
                }
            }
                        // action handlers below in the same function
            if show_actions {
                let more = ui.add(egui::Button::new("⋯"));
                egui::Popup::menu(&more).id(menu_id).show(|ui| {
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
                render_md(ui, mark, text);
            }
            Block::Image { media_type, data, width, height } => {
                render_attached_image(ui, tex, media_type, data, *width, *height);
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

/// A markdown segment: plain markdown, or a parsed pipe table.
enum Seg {
    Md(String),
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
}

/// Split `text` into markdown and pipe-table segments, in order. A table is a
/// `|…|` header line followed by a `|---|` ruler; contiguous `|` lines after
/// the ruler form the body. Fenced code blocks are never scanned.
fn split_tables(text: &str) -> Vec<Seg> {
    let lines: Vec<&str> = text.lines().collect();
    let is_ruler = |l: &str| {
        let t = l.trim();
        t.starts_with('|')
            && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
            && t.contains('-')
    };
    let cells = |l: &str| -> Vec<String> {
        l.trim().trim_matches('|').split('|').map(|c| c.trim().to_string()).collect()
    };
    let mut segs = Vec::new();
    let mut md = String::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            md.push_str(l);
            md.push('\n');
            i += 1;
            continue;
        }
        if !in_fence && t.starts_with('|') && i + 1 < lines.len() && is_ruler(lines[i + 1]) {
            if !md.trim().is_empty() {
                segs.push(Seg::Md(std::mem::take(&mut md)));
            } else {
                md.clear();
            }
            let headers = cells(l);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(cells(lines[i]));
                i += 1;
            }
            segs.push(Seg::Table { headers, rows });
            continue;
        }
        md.push_str(l);
        md.push('\n');
        i += 1;
    }
    if !md.trim().is_empty() {
        segs.push(Seg::Md(md));
    }
    segs
}

/// Strip inline markdown (`**`, `` ` ``, `~~`, `[label](url)` → `label`) —
/// table cells render as plain wrapped labels, not nested markdown.
fn inline_text(s: &str) -> String {
    let mut t = s.replace("**", "").replace("__", "").replace("~~", "").replace('`', "");
    let mut out = String::with_capacity(t.len());
    while let Some(i) = t.find("](") {
        let before = &t[..i];
        if let Some(open) = before.rfind('[') {
            out.push_str(&before[..open]);
            out.push_str(&before[open + 1..]);
        } else {
            out.push_str(before);
        }
        let rest = &t[i + 2..];
        let close = rest.find(')').unwrap_or(rest.len());
        t = rest.get(close + 1..).unwrap_or("").to_string();
    }
    out.push_str(&t);
    out
}

/// Render a pipe table natively. We lay out every cell into a `Galley`
/// wrapped at the column's width, compute row heights ourselves, allocate one
/// rect for the whole table, and paint each cell through a per-cell clip —
/// so a cell can never paint into the next column no matter what egui's
/// layout helpers would have done.
fn render_table(ui: &mut egui::Ui, headers: &[String], rows: &[Vec<String>], avail: f32) {
    fn tw(s: &str) -> f32 {
        // rough display width in points at egui's default font size
        s.chars().map(|c| if c.is_ascii() { 7.0 } else { 14.0 }).sum()
    }
    let cols = headers.len().max(1);
    let usable = (avail - 20.0).max(120.0);
    let mut nat: Vec<f32> = headers.iter().map(|h| tw(h).max(48.0)).collect();
    nat.resize(cols, 48.0);
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            // a single cell may not claim more than 60% of the row
            nat[i] = nat[i].max(tw(c).min(usable * 0.6));
        }
    }
    let total: f32 = nat.iter().sum();
    let widths: Vec<f32> = if total <= usable {
        nat
    } else {
        let floor = 48.0f32.min(usable / cols as f32);
        let mut w: Vec<f32> = nat.iter().map(|n| (n / total * usable).max(floor)).collect();
        let sum: f32 = w.iter().sum();
        if sum > usable {
            let f = usable / sum;
            for x in &mut w {
                *x *= f;
            }
        }
        w
    };

    let pad = 5.0f32;
    let gap = 10.0f32;
    let font = ui.style().text_styles[&egui::TextStyle::Body].clone();
    let text_c = ui.visuals().text_color();
    let head_c = ui.visuals().strong_text_color();
    let ctx = ui.ctx().clone();
    let mk = |s: &str, w: f32, c: egui::Color32| -> std::sync::Arc<egui::Galley> {
        ctx.fonts_mut(|f| f.layout(inline_text(s), font.clone(), c, (w - 2.0 * pad).max(8.0)))
    };
    let head_g: Vec<_> = (0..cols)
        .map(|i| mk(headers.get(i).map(|s| s.as_str()).unwrap_or(""), widths[i], head_c))
        .collect();
    let row_g: Vec<Vec<_>> = rows
        .iter()
        .map(|r| (0..cols).map(|i| mk(r.get(i).map(|s| s.as_str()).unwrap_or(""), widths[i], text_c)).collect())
        .collect();

    let head_h = head_g.iter().map(|g| g.size().y).fold(0.0, f32::max) + 2.0 * pad;
    let row_hs: Vec<f32> = row_g
        .iter()
        .map(|r| r.iter().map(|g| g.size().y).fold(0.0, f32::max) + 2.0 * pad)
        .collect();
    let table_w = widths.iter().sum::<f32>() + gap * (cols - 1) as f32;
    let table_h = head_h + row_hs.iter().sum::<f32>() + 2.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(table_w.min(avail), table_h),
        egui::Sense::hover(),
    );
    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);

    // column x offsets
    let mut xs = Vec::with_capacity(cols);
    let mut x = rect.min.x;
    for w in &widths {
        xs.push(x);
        x += w + gap;
    }
    // header
    let mut y = rect.min.y;
    for (i, g) in head_g.iter().enumerate() {
        let cell = egui::Rect::from_min_max(
            egui::pos2(xs[i], y),
            egui::pos2(xs[i] + widths[i], y + head_h),
        );
        p.with_clip_rect(cell).galley(egui::pos2(xs[i] + pad, y + pad), g.clone(), head_c);
    }
    y += head_h;
    p.hline(rect.min.x..=rect.max.x, y, stroke);
    // body rows (striped)
    let stripe = ui.visuals().faint_bg_color;
    for (ri, (r, rh)) in row_g.iter().zip(&row_hs).enumerate() {
        if ri % 2 == 1 {
            p.rect_filled(
                egui::Rect::from_min_size(egui::pos2(rect.min.x, y), egui::vec2(rect.width(), *rh)),
                0.0,
                stripe,
            );
        }
        for (i, g) in r.iter().enumerate() {
            let cell = egui::Rect::from_min_max(
                egui::pos2(xs[i], y),
                egui::pos2(xs[i] + widths[i], y + rh),
            );
            p.with_clip_rect(cell).galley(egui::pos2(xs[i] + pad, y + pad), g.clone(), text_c);
        }
        y += rh;
    }
    ui.add_space(4.0);
}

/// Render a message's markdown: pipe tables are split out and drawn natively
/// (see `render_table`); everything else goes to egui_commonmark.
fn render_md(ui: &mut egui::Ui, mark: &mut egui_commonmark::CommonMarkCache, text: &str) {
    let avail = ui.available_width();
    for seg in split_tables(text) {
        match seg {
            Seg::Md(t) => {
                egui_commonmark::CommonMarkViewer::new().show(ui, mark, &linkify(&t));
            }
            Seg::Table { headers, rows } => render_table(ui, &headers, &rows, avail),
        }
    }
}

/// An image waiting to be sent: base64 PNG bytes plus its natural size.
struct PendingImage {
    media_type: String,
    /// base64 (no data: prefix) — this is what goes into Block::Image
    data: String,
    width: u32,
    height: u32,
    /// texture handle for the thumbnail, created lazily
    tex: Option<egui::TextureHandle>,
}

/// Safety cap so a huge screenshot can't blow up memory / the request body.
const MAX_IMAGE_EDGE: u32 = 4096;

/// Encode an already-decoded image once: downscale if needed, then PNG → base64.
fn encode_image(img: image::DynamicImage) -> Result<PendingImage, String> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("image has zero size".into());
    }
    let img = if w.max(h) > MAX_IMAGE_EDGE {
        let scale = MAX_IMAGE_EDGE as f32 / w.max(h) as f32;
        // Triangle is plenty for model input and much faster than Lanczos3 on
        // a 4K screenshot, which matters because this runs on the UI thread.
        img.resize(
            ((w as f32 * scale) as u32).max(1),
            ((h as f32 * scale) as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let (nw, nh) = (img.width(), img.height());
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("png encode failed: {e}"))?;
    use base64::Engine as _;
    Ok(PendingImage {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        width: nw,
        height: nh,
        tex: None,
    })
}

/// Decode arbitrary image bytes and (re-)encode them as a sendable PNG.
/// Decoding here is also the validation step: pasting non-image clipboard data
/// (e.g. an HTML fragment) or dropping a mislabelled file fails cleanly.
fn decode_image(bytes: &[u8]) -> Result<PendingImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("not an image: {e}"))?;
    encode_image(img)
}

impl PendingImage {
    /// Build from encoded file bytes (drag & drop of an image file).
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        decode_image(bytes)
    }

    /// Decoded RGBA for the thumbnail / pasted-image preview.
    fn rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.decode(&self.data).ok()?;
        let img = image::load_from_memory(&png).ok()?.to_rgba8();
        Some((img.width(), img.height(), img.into_raw()))
    }

    fn texture(&mut self, ctx: &egui::Context) -> &egui::TextureHandle {
        if self.tex.is_none() {
            if let Some((w, h, rgba)) = self.rgba() {
                let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                self.tex = Some(ctx.load_texture(
                    format!("paste-{}x{}-{}", w, h, self.data.len()),
                    ci,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        self.tex.as_ref().expect("image failed to decode")
    }
}

/// Cache key for a decoded session image.
fn image_key(media_type: &str, data: &str, w: u32, h: u32) -> String {
    format!("{media_type}:{w}x{h}:{}", data.len())
}

/// Render an image block inside a chat message as a clickable thumbnail.
fn render_attached_image(
    ui: &mut egui::Ui,
    tex: &mut HashMap<String, egui::TextureHandle>,
    media_type: &str,
    data: &str,
    width: u32,
    height: u32,
) {
    let key = image_key(media_type, data, width, height);
    if !tex.contains_key(&key) {
        let Some((w, h, rgba)) = rgba_from_b64(data) else {
            ui.weak(format!("[image {width}x{height} — could not decode]"));
            return;
        };
        let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        tex.insert(key.clone(), ui.ctx().load_texture(&key, ci, egui::TextureOptions::LINEAR));
    }
    let handle = tex.get(&key).expect("just inserted");
    let (w, h) = (handle.size()[0] as f32, handle.size()[1] as f32);
    let avail = ui.available_width().max(64.0);
    let scale = (avail / w).min(1.0).min(220.0 / h.max(1.0));
    let size = egui::vec2((w * scale).max(32.0), (h * scale).max(32.0));
    let resp = ui.add(
        egui::Image::new((handle.id(), size))
            .corner_radius(4.0)
            .sense(egui::Sense::click()),
    );
    // clicking opens the full-size viewer
    if resp.clicked() {
        ui.ctx().memory_mut(|m| m.data.insert_temp(egui::Id::new("amty_viewer"), key.clone()));
    }
    resp.on_hover_text(format!("{w:.0} x {h:.0} (click to enlarge)"));
}

/// Decode base64 PNG bytes to RGBA.
fn rgba_from_b64(data: &str) -> Option<(u32, u32, Vec<u8>)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some((img.width(), img.height(), img.into_raw()))
}

/// Extensions we treat as images when a file is dropped on the window.
fn is_image_path(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
    )
}

/// Build the blocks for an outgoing user message. Images come first so the
/// model sees them before the question, matching the common convention.
fn message_blocks(text: &str, images: Vec<PendingImage>) -> Vec<crate::types::Block> {
    let mut blocks: Vec<crate::types::Block> = images
        .into_iter()
        .map(|i| crate::types::Block::Image {
            media_type: i.media_type,
            data: i.data,
            width: i.width,
            height: i.height,
        })
        .collect();
    if !text.trim().is_empty() {
        blocks.push(crate::types::Block::Text { text: text.to_string() });
    } else if blocks.is_empty() {
        // tool/API callers may still pass empty text; keep the message valid
        blocks.push(crate::types::Block::Text { text: String::new() });
    }
    blocks
}

/// Grab an image from the OS clipboard, if any.
fn clipboard_image() -> Option<Result<PendingImage, String>> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    // the raw clipboard RGBA is already an image — encode it directly instead
    // of PNG-encoding and re-decoding it again
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    Some(encode_image(image::DynamicImage::ImageRgba8(rgba)))
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
        out.push_str(&pathify_line(&linkify_line(line)));
    }
    out
}

/// `file://` URL for an absolute path (unix /a/b, drive C:\a\b, UNC \\h\s).
fn file_url(p: &str) -> String {
    let s = p.replace('\\', "/");
    if s.starts_with("//") {
        format!("file:{s}")        // UNC → file://host/share
    } else if s.starts_with('/') {
        format!("file://{s}")      // unix abs → file:///a/b
    } else {
        format!("file:///{s}")     // drive letter → file:///C:/a
    }
}

/// True when `tok` looks like an absolute filesystem path.
fn is_abs_path(tok: &str) -> bool {
    let b = tok.as_bytes();
    if b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return true;
    }
    if tok.starts_with("\\\\") || tok.starts_with("//") || tok.starts_with("~/") {
        return tok.len() > 4;
    }
    tok.len() > 4 && tok.starts_with('/') && tok[1..].contains('/')
}

/// Wrap absolute file paths in `file://` markdown links so clicking them in
/// the chat opens the file/folder with the OS handler. Runs after the URL
/// pass, skipping autolinks `<…>`, inline code, and markdown link targets.
fn pathify_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 48);
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut bt = 0usize; // backtick parity
    while i < line.len() {
        if bytes[i] == b'`' {
            bt += 1;
            out.push('`');
            i += 1;
            continue;
        }
        // inside <autolink> or `code` — copy through
        let in_code = bt % 2 == 1;
        let boundary = i == 0
            || matches!(bytes[i - 1], b' ' | b'\t' | b'(' | b'"' | b'\'');
        let rest = &line[i..];
        // skip markdown link target `](...)` and autolink `<...>` contexts
        let in_link_target = boundary && out.ends_with("](");
        if !in_code && boundary && !in_link_target && !rest.starts_with('<') {
            // candidate: absolute path token
            let end = rest
                .find(|c: char| matches!(c, ' ' | '\t' | '(' | '<' | '"' | '\''))
                .unwrap_or(rest.len());
            let mut tok = &rest[..end];
            // strip leading '(' isn't needed — '(' is a boundary
            while let Some(c) = tok.chars().last() {
                let unbalanced = c == ')' && tok.matches(')').count() > tok.matches('(').count();
                if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')') && (unbalanced || c != ')') {
                    tok = &tok[..tok.len() - c.len_utf8()];
                } else {
                    break;
                }
            }
            if is_abs_path(tok) {
                let (disp, url) = if let Some(h) = tok.strip_prefix("~/") {
                    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
                    (tok.to_string(), file_url(&format!("{home}/{h}")))
                } else {
                    (tok.to_string(), file_url(tok))
                };
                out.push_str(&format!("[{disp}]({url})"));
                i += tok.len();
                continue;
            }
        }
        let ch_len = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        i += ch_len;
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
            let tabs = [t("providers"), t("mcp"), t("skills"), t("behavior"), t("evolve")];
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
                        buf.sel_provider = name.clone();
                    }
                    if !is_default {
                        resp.context_menu(|ui| {
                            if ui.button(t("set as default")).clicked() {
                                buf.cfg.provider = name.clone();
                                ui.close();
                            }
                            if ui.button(t("delete")).clicked() {
                                buf.cfg.providers.remove(&name);
                                if buf.sel_provider == name {
                                    if let Some(first) = buf.cfg.providers.keys().next().cloned() {
                                        buf.sel_provider = first;
                                    }
                                }
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
                                ProviderKind::OpenCode => "opencode",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut p.kind, ProviderKind::Anthropic, "anthropic");
                                ui.selectable_value(&mut p.kind, ProviderKind::OpenAi, "openai (compatible)");
                                ui.selectable_value(&mut p.kind, ProviderKind::CommandCode, "commandcode");
                                ui.selectable_value(&mut p.kind, ProviderKind::OpenCode, "opencode");
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
                        ui.label("context_window");
                        let mut v = p.context_window.map(|n| n.to_string()).unwrap_or_default();
                        if ui.text_edit_singleline(&mut v).changed() {
                            p.context_window = v.parse().ok();
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
            if buf.tab == 4 {
            ui.heading(t("evolve"));
            ui.weak(t("self-evolution: reflection into MEMORY.md/skills, learned approvals, self-rebuild + idle restart"));
            ui.checkbox(&mut buf.cfg.evolve.reflect, t("reflect after runs into MEMORY.md"));
            ui.checkbox(&mut buf.cfg.evolve.skills, t("reflection may create skills"));
            ui.checkbox(&mut buf.cfg.evolve.self_modify, t("self_update tool (rebuild + restart)"));
            ui.checkbox(&mut buf.cfg.evolve.auto_restart, t("auto-restart when idle after rebuild"));
            ui.checkbox(&mut buf.cfg.evolve.learn_approvals, t("auto-allow repeatedly approved commands"));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(t("memory:"));
                ui.weak(crate::config::memory_path().display().to_string());
                if ui.small_button(t("reload")).clicked() {
                    buf.memory = std::fs::read_to_string(crate::config::memory_path())
                        .unwrap_or_default();
                }
            });
            ui.add(
                egui::TextEdit::multiline(&mut buf.memory.as_str())
                    .desired_rows(12)
                    .desired_width(520.0),
            );
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

    /// The sidebar grows a little every frame when a row overflows the panel's
    /// width: egui 0.36 persists the panel's *content* rect as its size
    /// (`PanelState`), so overflow feeds back into the next frame.
    ///
    /// Reproduces the reported "typing in the title search widens the left
    /// pane" bug: the ✕ button appears with text, and a width budget that
    /// forgets the widget spacing (or under-reserves the button) adds a few
    /// pixels per frame. Widths must stay stable across many frames.
    #[test]
    fn search_row_does_not_grow_the_side_panel() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("side");

        // Run a few frames with an empty box, then many frames with text in it.
        let mut widths: Vec<Option<f32>> = Vec::new();
        for frame in 0..40 {
            let text = if frame < 5 { "" } else { "セッション検索のテスト" };
            let mut out = ctx.run_ui(Default::default(), |root| {
                egui::Panel::left("side")
                    .default_size(250.0)
                    .min_size(180.0)
                    .show(root, |ui| {
                        // same shape as the real row: measured ✕ budget + spacing
                        let mut buf = text.to_string();
                        ui.horizontal(|ui| {
                            let show_clear = !buf.is_empty();
                            let spacing = ui.spacing().item_spacing.x;
                            let clear_w = if show_clear {
                                let galley = ui.painter().layout_no_wrap(
                                    "✕".to_owned(),
                                    egui::TextStyle::Button.resolve(ui.style()),
                                    egui::Color32::WHITE,
                                );
                                galley.size().x + ui.spacing().button_padding.x * 2.0 + spacing
                            } else {
                                0.0
                            };
                            let edit_w = (ui.available_width() - clear_w).max(24.0);
                            ui.add(
                                egui::TextEdit::singleline(&mut buf)
                                    .hint_text("search")
                                    .desired_width(edit_w),
                            );
                            if show_clear {
                                ui.small_button("✕");
                            }
                        });
                    });
            });
            // epaint panics if a frame's texture deltas are dropped unapplied
            out.textures_delta.clear();
            widths.push(
                ctx.data_mut(|d| d.get_persisted::<egui::containers::panel::PanelState>(id))
                    .map(|s| s.size().x),
            );
        }

        let typed: Vec<f32> = widths[5..].iter().map(|w| w.expect("panel state stored")).collect();
        let first = typed[0];
        let last = *typed.last().unwrap();
        assert!(
            (first - last).abs() < 1.0,
            "panel width kept drifting while typing: {first} -> {last} (all: {typed:?})"
        );
        assert!(last < 300.0, "panel width drifted past its default size: {last}");
    }

    /// The width budget itself: the edit field plus the ✕ button plus the
    /// spacing between them must fit in the available width.
    #[test]
    fn search_width_budget_fits_the_button() {
        let spacing: f32 = 8.0;
        let button_padding: f32 = 4.0;
        let glyph: f32 = 10.0;
        let available: f32 = 250.0;
        let clear_w = glyph + button_padding * 2.0 + spacing;
        let edit_w = (available - clear_w).max(24.0);
        assert!(edit_w + clear_w <= available, "budget overflows: {edit_w} + {clear_w} > {available}");
        // a narrow panel must still leave a usable field, never a negative one
        let narrow: f32 = 30.0;
        let edit_narrow = (narrow - clear_w).max(24.0);
        assert!(edit_narrow >= 24.0);
    }

    #[test]
    fn image_blocks_roundtrip_and_encode() {
        // build a real PNG in memory, then run it through the same path a
        // clipboard paste takes (decode -> maybe resize -> re-encode -> base64)
        let mut src = image::RgbaImage::new(64, 32);
        for (x, y, p) in src.enumerate_pixels_mut() {
            *p = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        src.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();

        let img = PendingImage::from_bytes(&png).expect("clipboard PNG should decode");
        assert_eq!((img.width, img.height), (64, 32));
        assert_eq!(img.media_type, "image/png");

        // the stored bytes must be valid image data again (thumbnail + resend)
        let (w, h, rgba) = img.rgba().expect("stored base64 must decode");
        assert_eq!((w, h), (64, 32));
        assert_eq!(rgba.len(), 64 * 32 * 4);

        // and it must survive being written to the session log and read back
        let msg = crate::types::ChatMessage::user_blocks(message_blocks("look at this", vec![img]));
        let line = serde_json::to_string(&msg).unwrap();
        let back: crate::types::ChatMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back.blocks.len(), 2);
        match &back.blocks[0] {
            crate::types::Block::Image { media_type, data, width, height } => {
                assert_eq!(media_type, "image/png");
                assert_eq!((*width, *height), (64, 32));
                assert!(rgba_from_b64(data).is_some(), "reloaded image must decode");
            }
            other => panic!("expected Image block first, got {other:?}"),
        }
        assert_eq!(back.text_content(), "[image]look at this");
    }

    #[test]
    fn non_image_bytes_are_rejected() {
        // pasting e.g. an HTML fragment must fail, not produce a broken block
        assert!(PendingImage::from_bytes(b"<html><body>hi</body></html>").is_err());
        assert!(PendingImage::from_bytes(b"").is_err());
    }

    #[test]
    fn oversized_images_are_downscaled() {
        // an image above the cap is resized so it can't blow up requests
        let big = image::RgbaImage::new(MAX_IMAGE_EDGE + 500, 100);
        let mut png = std::io::Cursor::new(Vec::new());
        big.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let img = PendingImage::from_bytes(&png.into_inner()).unwrap();
        // `image::resize` keeps the aspect ratio with integer rounding, so the
        // long edge lands at or just below the cap — never above it.
        assert!(img.width <= MAX_IMAGE_EDGE, "width {} exceeded cap", img.width);
        assert!(img.width > MAX_IMAGE_EDGE / 2, "unexpectedly shrunk to {}", img.width);
        assert!(img.height >= 1);
        assert!(img.height < 100, "aspect ratio not preserved: height {}", img.height);
    }

    #[test]
    fn message_blocks_text_only_and_image_only() {
        // text only -> single Text block (unchanged behaviour)
        let b = message_blocks("hello", vec![]);
        assert_eq!(b.len(), 1);
        assert!(matches!(b[0], crate::types::Block::Text { .. }));

        // image only -> no empty Text block appended
        let img = PendingImage { media_type: "image/png".into(), data: "AAAA".into(), width: 1, height: 1, tex: None };
        let b = message_blocks("   ", vec![img]);
        assert_eq!(b.len(), 1);
        assert!(matches!(b[0], crate::types::Block::Image { .. }));

        // both -> image first, then the text
        let img = PendingImage { media_type: "image/png".into(), data: "AAAA".into(), width: 1, height: 1, tex: None };
        let b = message_blocks("what is this?", vec![img]);
        assert_eq!(b.len(), 2);
        assert!(matches!(b[0], crate::types::Block::Image { .. }));
        assert!(matches!(b[1], crate::types::Block::Text { .. }));
    }

    #[test]
    fn image_file_extensions_recognised() {
        for p in ["a.png", "b.JPG", "c.jpeg", "d.webp", "e.gif", "f.bmp"] {
            assert!(is_image_path(std::path::Path::new(p)), "{p} should count as an image");
        }
        for p in ["a.txt", "b.rs", "c.md", "noext"] {
            assert!(!is_image_path(std::path::Path::new(p)), "{p} should not count as an image");
        }
    }

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
    fn split_tables_finds_tables_and_skips_fences() {
        // plain prose -> single markdown segment
        assert!(matches!(split_tables("just prose\n").as_slice(), [Seg::Md(_)]));
        // fenced code is not a table even when it looks like one
        assert!(matches!(split_tables("```\n| a | b |\n|---|---|\n```\n").as_slice(), [Seg::Md(_)]));
        // a pipe line without a ruler is not a table
        assert!(matches!(split_tables("| not a table |\n| still not |\n").as_slice(), [Seg::Md(_)]));
        let segs = split_tables("before\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\nafter\n");
        assert_eq!(segs.len(), 3);
        match &segs[1] {
            Seg::Table { headers, rows } => {
                assert_eq!(headers, &["a", "b"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["1", "2"]);
            }
            _ => panic!("middle segment should be a table"),
        }
        assert!(matches!(&segs[0], Seg::Md(t) if t.contains("before")));
        assert!(matches!(&segs[2], Seg::Md(t) if t.contains("after")));
    }

    #[test]
    fn inline_text_strips_marks_and_links() {
        assert_eq!(inline_text("plain"), "plain");
        assert_eq!(inline_text("**bold** and `code`"), "bold and code");
        assert_eq!(inline_text("[label](https://a.b/c) here"), "label here");
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

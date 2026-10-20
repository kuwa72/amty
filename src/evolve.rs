//! Self-evolution: amty improves itself between runs.
//!
//! - **Reflection** (`evolve.reflect`): when a run ends, a one-shot provider
//!   call distills durable lessons into `MEMORY.md` (injected into the
//!   system tail every turn) and reusable `SKILL.md` files (`evolve.skills`).
//!   The reflector can also tune a whitelist of config keys.
//! - **Approval learning** (`evolve.learn_approvals`): in `ask` mode a shell
//!   command approved verbatim several times stops prompting.
//! - **Self-rebuild** (`evolve.self_modify`, `evolve.auto_restart`): the
//!   `self_update` tool rebuilds the binary from its own source tree; a
//!   successful build respawns the app once all runs are idle.

use crate::app::App;
use crate::config;
use crate::types::EvKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn state_path() -> PathBuf {
    config::data_dir().join("evolve.json")
}

fn log_path() -> PathBuf {
    config::data_dir().join("evolve.log")
}

fn build_log_path() -> PathBuf {
    config::data_dir().join("self-update.log")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolStat {
    pub calls: u64,
    pub errors: u64,
    pub denied: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// tool name → counters
    pub tools: BTreeMap<String, ToolStat>,
    /// exact shell command line → times the user approved it
    pub approved_commands: BTreeMap<String, u64>,
    /// session id → message count at last reflection (watermark)
    pub reflected: BTreeMap<String, u64>,
    /// timestamps of reflection calls — used for the daily cap
    pub reflection_times: Vec<u64>,
    /// no auto-reflection before this timestamp (provider-failure cooldown)
    pub reflect_fail_until: u64,
    pub reflections: u64,
    pub builds_ok: u64,
    pub builds_fail: u64,
}

impl Stats {
    pub fn load() -> Self {
        std::fs::read_to_string(state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// Append a line to `evolve.log`, keeping the file under ~64 KB.
fn log_line(msg: &str) {
    use std::io::Write;
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 64 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {msg}", crate::session::now_secs());
    }
}

fn save_stats(app: &App) {
    app.stats.lock().unwrap().save();
}

// ---------- stats recording (called from the agent loop) ----------

pub fn record_tool(app: &App, name: &str, ok: bool) {
    let mut st = app.stats.lock().unwrap();
    let t = st.tools.entry(name.to_string()).or_default();
    t.calls += 1;
    if !ok {
        t.errors += 1;
    }
    drop(st);
    save_stats(app);
}

pub fn record_denied(app: &App, name: &str) {
    let mut st = app.stats.lock().unwrap();
    st.tools.entry(name.to_string()).or_default().denied += 1;
    drop(st);
    save_stats(app);
}

/// How many identical approvals before a command is auto-allowed.
const AUTO_APPROVE_AFTER: u64 = 3;
const MAX_APPROVED_COMMANDS: usize = 256;

/// Called when the user approves a tool call. Only `shell` commands are
/// learned — file paths are too varied for exact-match to be useful.
pub fn record_approved(app: &App, name: &str, input: &Value) {
    if name != "shell" {
        return;
    }
    let Some(cmd) = input["command"].as_str() else { return };
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }
    let mut st = app.stats.lock().unwrap();
    // bound the map: evict the least-approved entries when full
    if st.approved_commands.len() >= MAX_APPROVED_COMMANDS && !st.approved_commands.contains_key(cmd) {
        if let Some(min) = st.approved_commands.values().min().copied() {
            st.approved_commands.retain(|_, c| *c > min);
        }
    }
    *st.approved_commands.entry(cmd.to_string()).or_insert(0) += 1;
    drop(st);
    save_stats(app);
}

/// True when this exact call has been approved often enough to skip the
/// prompt. Exact-match only — no prefix inference, so `rm -rf a` never
/// legitimizes `rm -rf b`.
pub fn auto_approved(app: &App, name: &str, input: &Value) -> bool {
    if name != "shell" || !app.cfg.read().unwrap().evolve.learn_approvals {
        return false;
    }
    let Some(cmd) = input["command"].as_str() else { return false };
    app.stats
        .lock()
        .unwrap()
        .approved_commands
        .get(cmd.trim())
        .map(|c| *c >= AUTO_APPROVE_AFTER)
        .unwrap_or(false)
}

// ---------- memory ----------

/// MEMORY.md injected into the system tail (kept small; it rides every turn).
const MEMORY_INJECT: usize = 6000;
/// On-disk cap — the reflector rewrites the whole file each time.
const MEMORY_MAX: usize = 24 * 1024;

/// System-prompt fragment for the current memory contents; "" when empty so
/// the tail stays unchanged for fresh installs.
pub fn memory_prompt() -> String {
    let Ok(text) = std::fs::read_to_string(config::memory_path()) else {
        return String::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut body = text;
    if body.len() > MEMORY_INJECT {
        let mut end = MEMORY_INJECT;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body = &body[..end];
    }
    format!("\n\nMemory — durable notes learned from previous sessions:\n{body}")
}

// ---------- restart ----------

/// Schedule a process restart once all agent runs are idle, then try now.
/// `target` is the binary to install+launch (usually the rebuilt artifact).
pub fn request_restart(app: &Arc<App>, target: PathBuf) {
    *app.restart_pending.lock().unwrap() = Some(target);
    app.emit("", EvKind::Touched);
    maybe_restart(app);
}

/// If a restart is pending and nothing is running, hand off to a detached
/// helper process that replaces the exe and relaunches it, then exit.
/// Returns true when the handoff happened (i.e. never — it exits the process).
pub fn maybe_restart(app: &Arc<App>) -> bool {
    let install = app.restart_pending.lock().unwrap().clone();
    let Some(install) = install else { return false };
    if !app.runs.lock().unwrap().is_empty() {
        return false;
    }
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    if SPAWNED.swap(true, Ordering::SeqCst) {
        return false;
    }
    match spawn_swap_helper(&install) {
        Ok(()) => {
            eprintln!("amty: restarting (install {})", install.display());
            std::process::exit(0);
        }
        Err(e) => {
            log_line(&format!("restart failed: {e}"));
            *app.restart_pending.lock().unwrap() = None;
            false
        }
    }
}

/// Path of the throwaway helper copy — must differ from the target exe so
/// Windows lets the helper overwrite it.
fn helper_path() -> PathBuf {
    let name = if cfg!(windows) { "amty-helper.exe" } else { "amty-helper" };
    config::data_dir().join("self-swap").join(name)
}

/// Copy the running exe aside and spawn it as `amty self-swap …`: the helper
/// waits for this process to exit, installs `install` over the exe, and
/// relaunches it. Needed because a running exe can't be overwritten —
/// on Windows the linker can't even *produce* it, on unix writing to a
/// running text image is ETXTBSY.
fn spawn_swap_helper(install: &PathBuf) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = helper_path();
    if let Some(dir) = helper.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::copy(&exe, &helper).map_err(|e| format!("copy helper: {e}"))?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.args([
        "self-swap".into(),
        "--wait-pid".into(),
        std::process::id().to_string(),
        "--install".into(),
        install.to_string_lossy().into_owned(),
        "--target".into(),
        exe.to_string_lossy().into_owned(),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    crate::catalog::no_window(&mut cmd);
    cmd.spawn().map_err(|e| format!("spawn helper: {e}"))?;
    Ok(())
}

/// `amty self-swap` entry point — runs inside the throwaway helper copy.
/// Synchronous and std-only: no tokio, no App, no config load.
pub fn self_swap(wait_pid: u32, install: PathBuf, target: PathBuf) -> anyhow::Result<()> {
    log_line(&format!(
        "self-swap: pid {} → install {} over {}",
        wait_pid,
        install.display(),
        target.display()
    ));
    wait_for_exit(wait_pid);
    if install != target {
        install_binary(&install, &target).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    log_line(&format!("self-swap: relaunching {}", target.display()));
    match std::process::Command::new(&target).spawn() {
        Ok(_) => Ok(()),
        Err(e) => {
            // relaunch failed — roll the old binary back so the user isn't
            // left with a broken install
            let old = target.with_extension("old");
            if old.exists() {
                let _ = std::fs::rename(&old, &target);
            }
            log_line(&format!("self-swap: relaunch failed ({e}), rolled back"));
            Err(anyhow::anyhow!("relaunch {}: {e}", target.display()))
        }
    }
}

/// Poll until `pid` is gone (or ~120 s elapse — the install retry loop is the
/// real guarantee; this just avoids a double-instance window).
fn wait_for_exit(pid: u32) {
    for _ in 0..400 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // /proc covers Linux/WSL; kill -0 covers macOS
    if PathBuf::from(format!("/proc/{pid}")).exists() {
        return true;
    }
    std::process::Command::new("/bin/sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess};
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE;
        CloseHandle(h);
        alive
    }
}

/// Replace `target` with `install`: move the current exe aside (renaming a
/// running exe is legal even on Windows — only write/delete is locked) then
/// copy the new one in. Retries for ~60 s in case the file is still locked.
fn install_binary(install: &PathBuf, target: &PathBuf) -> Result<(), String> {
    let old = target.with_extension("old");
    let mut last = String::new();
    for _ in 0..200 {
        let _ = std::fs::rename(target, &old);
        match std::fs::copy(install, target) {
            Ok(_) => {
                let _ = std::fs::remove_file(&old);
                return Ok(());
            }
            Err(e) => {
                last = e.to_string();
                // put the old exe back if the slot is empty so a hard failure
                // never leaves the user without a binary
                if !target.exists() && old.exists() {
                    let _ = std::fs::rename(&old, target);
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    Err(format!("copy {} → {}: {last}", install.display(), target.display()))
}

// ---------- self rebuild ----------

/// The source tree this binary was built from (compile-time), if it still
/// exists on this machine. `self_update rebuild` is a no-op otherwise.
pub fn source_dir() -> Option<PathBuf> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.join("Cargo.toml").exists().then_some(d)
}

/// Rebuild the profile this binary was built with: running from
/// target/debug → `cargo build`; anything else → `cargo build --release`.
fn build_profile() -> &'static str {
    let debug = std::env::current_exe()
        .map(|p| p.components().any(|c| c.as_os_str() == "debug"))
        .unwrap_or(false);
    if debug { "debug" } else { "release" }
}

/// The cargo target dir for self-builds. If the normal artifact path IS the
/// running exe, the linker can't write it (Windows lock / unix ETXTBSY), so
/// the build is redirected to `target/self-update/` — a second target dir,
/// which means a full rebuild there but never a lock failure.
fn build_target_dir(profile: &str) -> Option<(PathBuf, PathBuf)> {
    let exe_name = if cfg!(windows) { "amty.exe" } else { "amty" };
    let src = source_dir()?;
    let normal_dir = src.join("target");
    let normal = normal_dir.join(profile).join(exe_name);
    let collides = std::env::current_exe()
        .map(|e| {
            // canonicalize both: same file through a symlinked path still locks
            std::fs::canonicalize(&normal).unwrap_or(normal.clone())
                == std::fs::canonicalize(&e).unwrap_or(e.clone())
        })
        .unwrap_or(false);
    let dir = if collides { normal_dir.join("self-update") } else { normal_dir };
    let artifact = dir.join(profile).join(exe_name);
    Some((dir, artifact))
}

static BUILDING: AtomicBool = AtomicBool::new(false);

/// Kick off `cargo build` in the source tree, detached with output going to
/// `self-update.log` (a cold build can exceed the 300 s tool cap, so it must
/// not run inside the tool call). On success the artifact becomes the restart
/// target; with `evolve.auto_restart` the app respawns as soon as it's idle.
pub fn start_rebuild(app: &Arc<App>) -> Result<String, String> {
    let src = source_dir().ok_or_else(|| {
        format!("source tree {} not found — this binary wasn't built from a checkout on this machine", env!("CARGO_MANIFEST_DIR"))
    })?;
    if BUILDING.swap(true, Ordering::SeqCst) {
        return Err("a build is already running".into());
    }
    let profile = build_profile();
    let Some((target_dir, artifact)) = build_target_dir(profile) else {
        return Err("could not resolve artifact path".into());
    };
    let log = build_log_path();
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::File::create(&log).map_err(|e| e.to_string())?;
    let file_err = file.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build")
        .arg("--target-dir")
        .arg(&target_dir);
    if profile == "release" {
        cmd.arg("--release");
    }
    crate::catalog::no_window(&mut cmd);
    let mut child = cmd
        .current_dir(&src)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(file_err)
        .spawn()
        .map_err(|e| {
            BUILDING.store(false, Ordering::SeqCst);
            format!("cargo spawn failed: {e}")
        })?;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        BUILDING.store(false, Ordering::SeqCst);
        {
            let mut st = app2.stats.lock().unwrap();
            if ok { st.builds_ok += 1 } else { st.builds_fail += 1 }
        }
        save_stats(&app2);
        if !ok {
            log_line("self-update build failed — see self-update.log");
            app2.emit("", EvKind::Touched);
            return;
        }
        log_line(&format!("self-update build ok: {}", artifact.display()));
        let target = artifact;
        *app2.built_binary.lock().unwrap() = Some(target.clone());
        if app2.cfg.read().unwrap().evolve.auto_restart {
            request_restart(&app2, target);
        } else {
            app2.emit("", EvKind::Touched);
        }
    });
    Ok(format!(
        "cargo build ({profile}) started in {}\nlog: {}\nOn success the new binary {} restart when idle.",
        src.display(),
        log.display(),
        if app.cfg.read().unwrap().evolve.auto_restart { "will" } else { "is ready; use self_update restart to" },
    ))
}

// ---------- reflection ----------

/// Minimum new messages since the last reflection before we spend a call.
const MIN_NEW_MESSAGES: u64 = 6;
/// Cap on `reflected` watermarks.
const MAX_REFLECTED: usize = 100;
/// Transcript budget for the reflection prompt.
const REFLECT_CHARS: usize = 30_000;
const MAX_SKILLS_PER_REFLECT: usize = 3;

const REFLECT_SYSTEM: &str = "You are the self-improvement pass of amty, a lightweight AI agent app. \
You review a session transcript, the agent's current persistent MEMORY, and tool usage stats, then \
decide what durable knowledge should carry over to future sessions.\n\
Rules:\n\
- Keep memory short (at most ~60 lines): durable facts about the user's environment, preferences, \
recurring pitfalls and their fixes, project-specific commands that worked, lessons from errors.\n\
- Remove memory items that turned out wrong, obsolete, or trivially task-specific.\n\
- Never store secrets, tokens, or private paths the user did not share intentionally.\n\
- Emit a skill only for clearly reusable know-how (a procedure, a workaround, a tool recipe) — \
at most a couple, each a complete SKILL.md whose first non-heading line is a one-line description.\n\
- Config suggestions are limited to the whitelisted keys shown below.\n\n\
Output ONLY fenced blocks in these forms — or the single word NONE when nothing is worth keeping:\n\n\
```memory\n<complete new contents of MEMORY.md>\n```\n\n\
```skill <kebab-case-name>\n<complete SKILL.md>\n```\n\n\
```config\n<key> = <value>\n```";

/// Config keys the reflector may change. Deliberately a safe subset: nothing
/// that redirects requests, swaps providers, or rewrites the system prompt.
const REFLECT_CONFIG_KEYS: &[&str] = &[
    "+allow_commands",
    "+allow_paths",
    "max_tokens",
    "search.max_results",
    "search.fetch_max_chars",
];

/// Erase a learned auto-approval (the exact command line as recorded).
pub fn forget_command(app: &App, command: &str) -> bool {
    let mut st = app.stats.lock().unwrap();
    let removed = st.approved_commands.remove(command.trim()).is_some();
    drop(st);
    if removed {
        save_stats(app);
    }
    removed
}

/// Cost control: reflection burns one provider call per trigger, so cap the
/// count per rolling day and back off briefly after a provider failure.
const MAX_REFLECTIONS_PER_DAY: u64 = 20;
const FAIL_COOLDOWN_SECS: u64 = 600;

fn auto_reflect_allowed(app: &App) -> bool {
    let now = crate::session::now_secs();
    let mut st = app.stats.lock().unwrap();
    if now < st.reflect_fail_until {
        return false;
    }
    let day_ago = now.saturating_sub(86_400);
    st.reflection_times.retain(|t| *t > day_ago);
    (st.reflection_times.len() as u64) < MAX_REFLECTIONS_PER_DAY
}

/// Called after every agent run finishes (`agent::start_run_blocks`). Order:
/// a pending restart wins over reflection — exiting kills the reflect task.
pub fn after_run(app: &Arc<App>, sid: &str) {
    if maybe_restart(app) {
        return; // unreachable — maybe_restart exits the process
    }
    if !app.cfg.read().unwrap().evolve.reflect {
        return;
    }
    let Some(session) = app.sessions.get(sid) else { return };
    let total = session.messages.len() as u64;
    {
        let mut st = app.stats.lock().unwrap();
        let last = st.reflected.get(sid).copied().unwrap_or(0);
        if total.saturating_sub(last) < MIN_NEW_MESSAGES {
            return;
        }
        // mark first so a slow/failed reflection can't double-fire
        st.reflected.insert(sid.to_string(), total);
        if st.reflected.len() > MAX_REFLECTED {
            if let Some(min) = st.reflected.values().min().copied() {
                st.reflected.retain(|_, v| *v > min);
            }
        }
    }
    save_stats(app);
    if !auto_reflect_allowed(app) {
        return;
    }
    let app2 = app.clone();
    let sid = sid.to_string();
    tokio::spawn(async move {
        match reflect(&app2, &sid).await {
            Ok(summary) => {
                log_line(&format!("reflect[{sid}]: {summary}"));
                app2.emit("", EvKind::Evolved { summary });
            }
            Err(e) => {
                log_line(&format!("reflect[{sid}] failed: {e}"));
                let mut st = app2.stats.lock().unwrap();
                st.reflect_fail_until =
                    crate::session::now_secs() + FAIL_COOLDOWN_SECS;
                drop(st);
                save_stats(&app2);
            }
        }
    });
}

/// Run the reflection pass for a session right now (the `self_update
/// reflect` tool path — the agent explicitly asked, so no message-count gate).
pub async fn reflect_now(app: &Arc<App>, sid: &str) -> Result<String, String> {
    let sid = app.sessions.resolve(sid).ok_or_else(|| format!("no such session '{sid}'"))?;
    {
        let mut st = app.stats.lock().unwrap();
        if let Some(s) = app.sessions.get(&sid) {
            st.reflected.insert(sid.clone(), s.messages.len() as u64);
        }
    }
    save_stats(app);
    let out = reflect(app, &sid).await;
    if let Ok(summary) = &out {
        app.emit("", EvKind::Evolved { summary: summary.clone() });
    }
    out
}

async fn reflect(app: &Arc<App>, sid: &str) -> Result<String, String> {
    let session = app.sessions.get(sid).ok_or_else(|| "session gone".to_string())?;
    let (prov_conf, model) = {
        let cfg = app.cfg.read().unwrap();
        let name = session.provider.clone().unwrap_or_else(|| cfg.provider.clone());
        let conf = cfg.providers.get(&name).cloned().ok_or_else(|| format!("provider '{name}' not defined"))?;
        let model = session.model.clone().unwrap_or_else(|| conf.model.clone());
        (conf, model)
    };
    let prov = crate::provider::build(&prov_conf);

    // tail of the transcript, bounded — reflections need the gist of what
    // happened most recently, not the whole scrollback
    let full = crate::compact::transcript(&session.messages);
    let tail = if full.len() > REFLECT_CHARS {
        let mut start = full.len() - REFLECT_CHARS;
        while !full.is_char_boundary(start) {
            start += 1;
        }
        // skip to the next line boundary so we don't start mid-block
        match full[start..].find('\n') {
            Some(n) => &full[start + n + 1..],
            None => &full[start..],
        }
    } else {
        full.as_str()
    };

    let memory = std::fs::read_to_string(config::memory_path()).unwrap_or_default();
    let stats = {
        let st = app.stats.lock().unwrap().clone();
        let tools: Vec<String> = st
            .tools
            .iter()
            .map(|(n, t)| format!("{n}: {} calls, {} errors, {} denied", t.calls, t.errors, t.denied))
            .collect();
        format!(
            "tool usage:\n{}\napproved shell commands (count): {}",
            if tools.is_empty() { "  (none)".into() } else { tools.join("\n") },
            st.approved_commands
                .iter()
                .map(|(c, n)| format!("{n}x {c}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    let prompt = format!(
        "SESSION TRANSCRIPT (tail):\n{tail}\n\n\
         CURRENT MEMORY.md:\n{}\n\n\
         STATS:\n{stats}\n\n\
         Writable config keys: {}",
        if memory.trim().is_empty() { "(empty)" } else { memory.trim() },
        REFLECT_CONFIG_KEYS.join(", "),
    );

    let out = crate::provider::stream_text(
        prov.as_ref(),
        &model,
        REFLECT_SYSTEM,
        prompt,
        4096,
        || false,
    )
    .await?;
    let applied = apply_reflection(app, &out);
    {
        let mut st = app.stats.lock().unwrap();
        st.reflections += 1;
        st.reflection_times.push(crate::session::now_secs());
        if st.reflection_times.len() > 200 {
            let drop_n = st.reflection_times.len() - 200;
            st.reflection_times.drain(..drop_n);
        }
    }
    save_stats(app);
    Ok(applied)
}

/// Parse the reflector's fenced blocks and persist them. Returns a short
/// human-readable summary of what changed.
fn apply_reflection(app: &Arc<App>, out: &str) -> String {
    let mut did: Vec<String> = vec![];
    for (kind, arg, body) in parse_blocks(out) {
        match kind.as_str() {
            "memory" => {
                let mut content = body;
                if content.len() > MEMORY_MAX {
                    let mut end = MEMORY_MAX;
                    while !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    content.truncate(end);
                }
                let path = config::memory_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // atomic write: a killed process (restart, crash) must not
                // leave a half-written memory file
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, &content).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                    did.push("memory".into());
                }
            }
            "skill" => {
                if !app.cfg.read().unwrap().evolve.skills || arg.is_empty() {
                    continue;
                }
                if did.iter().filter(|d| d.starts_with("skill:")).count() >= MAX_SKILLS_PER_REFLECT {
                    continue;
                }
                let name: String = arg
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if name.is_empty() || body.trim().is_empty() {
                    continue;
                }
                let dir = config::skills_dir().join(&name);
                let _ = std::fs::create_dir_all(&dir);
                if std::fs::write(dir.join("SKILL.md"), &body).is_ok() {
                    did.push(format!("skill:{name}"));
                }
            }
            "config" => {
                let mut changed = vec![];
                {
                    let mut cfg = app.cfg.write().unwrap();
                    for line in body.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let Some((k, v)) = line.split_once('=') else { continue };
                        let (k, v) = (k.trim(), v.trim());
                        if !REFLECT_CONFIG_KEYS.contains(&k) {
                            continue;
                        }
                        if cfg.apply_set(k, v).is_ok() {
                            changed.push(k.to_string());
                        }
                    }
                    if !changed.is_empty() {
                        let _ = cfg.save(&app.cfg_path);
                    }
                }
                if !changed.is_empty() {
                    did.push(format!("config:{}", changed.join(",")));
                }
            }
            _ => {}
        }
    }
    if did.is_empty() {
        "no changes".into()
    } else {
        format!("evolved: {}", did.join(" "))
    }
}

/// Extract ```<kind> [arg] fenced blocks: (kind, arg, body).
fn parse_blocks(out: &str) -> Vec<(String, String, String)> {
    let mut blocks = vec![];
    let mut rest = out;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(nl) = after.find('\n') else { break };
        let header = after[..nl].trim();
        let body_start = start + 3 + nl + 1;
        let Some(rel_end) = rest[body_start..].find("```") else { break };
        let body = rest[body_start..body_start + rel_end].trim_end_matches('\n').to_string();
        let mut it = header.splitn(2, ' ');
        let kind = it.next().unwrap_or("").to_string();
        let arg = it.next().unwrap_or("").trim().to_string();
        if !kind.is_empty() {
            blocks.push((kind, arg, body));
        }
        rest = &rest[body_start + rel_end + 3..];
    }
    blocks
}

// ---------- status ----------

/// Human-readable status for the `self_update status` tool.
pub fn status_text(app: &App) -> String {
    let v = status_json(app);
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

pub fn status_json(app: &App) -> Value {
    let st = app.stats.lock().unwrap().clone();
    let cfg = app.cfg.read().unwrap();
    let mem = config::memory_path();
    let built = app.built_binary.lock().unwrap().clone();
    let pending = app.restart_pending.lock().unwrap().clone();
    let build_log_tail = std::fs::read(&build_log_path())
        .ok()
        .map(|b| {
            let s = String::from_utf8_lossy(&b);
            s.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>()
        });
    json!({
        "config": {
            "reflect": cfg.evolve.reflect,
            "skills": cfg.evolve.skills,
            "self_modify": cfg.evolve.self_modify,
            "auto_restart": cfg.evolve.auto_restart,
            "learn_approvals": cfg.evolve.learn_approvals,
        },
        "memory_path": mem.display().to_string(),
        "memory_bytes": std::fs::metadata(&mem).map(|m| m.len()).unwrap_or(0),
        "source_dir": source_dir().map(|p| p.display().to_string()),
        "build_profile": build_profile(),
        "building": BUILDING.load(Ordering::SeqCst),
        "built_binary": built.map(|p| p.display().to_string()),
        "restart_pending": pending.map(|p| p.display().to_string()),
        "stats": st,
        "build_log_tail": build_log_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_blocks_extracts_kinds_and_args() {
        let out = "intro\n```memory\nfacts\n```\nmid\n```skill my-tool\nbody\n```\n```config\na = b\n```";
        let b = parse_blocks(out);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], ("memory".into(), "".into(), "facts".into()));
        assert_eq!(b[1], ("skill".into(), "my-tool".into(), "body".into()));
        assert_eq!(b[2], ("config".into(), "".into(), "a = b".into()));
    }

    #[test]
    fn parse_blocks_ignores_plain_text() {
        assert!(parse_blocks("just NONE").is_empty());
        // unclosed fence ignored
        assert!(parse_blocks("```memory\nnever closed").is_empty());
    }

    #[test]
    fn stats_roundtrip() {
        let dir = std::env::temp_dir().join(format!("amty-evolve-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = Stats::default();
        st.tools.entry("shell".into()).or_default().calls = 5;
        st.approved_commands.insert("cargo test".into(), 3);
        st.reflected.insert("abc".into(), 12);
        let text = serde_json::to_string(&st).unwrap();
        let back: Stats = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tools["shell"].calls, 5);
        assert_eq!(back.approved_commands["cargo test"], 3);
        std::fs::remove_dir_all(dir).ok();
    }
}

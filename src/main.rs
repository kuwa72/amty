// GUI binary: no console window on Windows by default. CLI subcommands still
// inherit the invoker's console (stdout works in cmd/PowerShell/WSL); pass
// --console or set AMTY_CONSOLE=1 to allocate a debug console for GUI mode.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod agent;
mod api;
mod app;
mod catalog;
mod cli;
mod compact;
mod config;
mod evolve;
mod gui;
mod i18n;
mod jobs;
mod mcp;
mod mcp_serve;
mod notes;
mod oauth;
mod provider;
mod session;
mod tools;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "amty", version, about = "lightweight scriptable AI chat & agent client")]
pub struct Cli {
    /// Windows only: allocate a console window for log output (GUI mode).
    /// AMTY_CONSOLE=1 does the same.
    #[arg(long, global = true)]
    console: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Launch the GUI (default).
    Gui,
    /// Show status of the running instance.
    Status,
    /// List sessions.
    Sessions,
    /// Search sessions by title, message text and tool calls.
    Search {
        /// query text (space-joined)
        query: Vec<String>,
        #[arg(short, long)] session: Option<String>,
        #[arg(long, default_value = "50")] limit: usize,
        /// print the raw API JSON instead of a table
        #[arg(long)] json: bool,
    },
    /// Show token usage (per session, or all sessions plus a total).
    Usage {
        #[arg(short, long)] session: Option<String>,
    },
    /// Create a session, print its id.
    New {
        #[arg(long)] provider: Option<String>,
        #[arg(long)] model: Option<String>,
    },
    /// Send a prompt. Defaults to the most recent session.
    Send {
        text: Vec<String>,
        #[arg(short, long)] session: Option<String>,
        #[arg(short = 'n', long)] new: bool,
        #[arg(long)] no_wait: bool,
        #[arg(long)] provider: Option<String>,
        #[arg(long)] model: Option<String>,
    },
    /// Print a session's messages.
    Show {
        session: Option<String>,
        #[arg(long, default_value = "50")] last: usize,
    },
    /// List files/folders created during a session (empty = most recent).
    Files {
        session: Option<String>,
    },
    /// Delete a session.
    Delete { session: String },
    /// Stream a session's events (SSE).
    Events {
        #[arg(short, long)] session: Option<String>,
    },
    /// Cancel the running turn.
    Cancel {
        #[arg(short, long)] session: Option<String>,
    },
    /// List pending approvals.
    Approvals,
    /// Approve a pending tool call.
    Approve { id: String },
    /// Deny a pending tool call.
    Deny { id: String, #[arg(short, long)] reason: Option<String> },
    /// Get/set configuration.
    Config {
        #[command(subcommand)] sub: ConfigCmd,
    },
    /// Show MCP server statuses (or `mcp remove <name>`).
    Mcp {
        #[command(subcommand)]
        sub: Option<McpCmd>,
    },
    /// List the installable MCP server catalog.
    Catalog,
    /// Install an MCP server from the catalog. `amty install notion NOTION_TOKEN=ntn_…`
    Install {
        id: String,
        /// KEY=VALUE env pairs for the server
        env: Vec<String>,
    },
    /// Run a catalog server's OAuth/auth subcommand (opens browser).
    Auth {
        id: String,
        /// print log tail instead of starting auth
        #[arg(long)] status: bool,
    },
    /// Copy a downloaded OAuth keys file to where the server expects it.
    Keys { id: String, src: String },
    /// List installed skills (or `skills remove <name>`).
    Skills {
        #[command(subcommand)]
        sub: Option<SkillCmd>,
    },
    /// Manage markdown notes (the notes dir — Obsidian-compatible).
    /// Bare `amty notes` lists all notes.
    Notes {
        #[command(subcommand)]
        sub: Option<NoteCmd>,
    },
    /// Manage scheduled jobs (prompts that fire on a timer).
    /// Bare `amty jobs` lists all jobs.
    Jobs {
        #[command(subcommand)]
        sub: Option<JobCmd>,
    },
    /// Show self-evolution state (memory, stats, pending restart) or manage it.
    Evolve {
        #[command(subcommand)]
        sub: Option<EvolveCmd>,
    },
    /// Expose the running instance as an MCP server (stdio) for external agents.
    McpServe,
    /// Import mcpServers from claude_desktop_config.json into config.toml.
    ImportClaude,
    /// Internal: swap helper process for self-update. Runs in a copy of this
    /// binary so it can overwrite the real exe after the original exits
    /// (a running exe can't be overwritten, and on Windows can't even be
    /// linked over by cargo).
    #[command(hide = true)]
    SelfSwap {
        #[arg(long)]
        wait_pid: u32,
        #[arg(long)]
        install: std::path::PathBuf,
        #[arg(long)]
        target: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    Get { key: Option<String> },
    Set { key: String, value: String },
    /// Delete a setting, e.g. `amty config unset mcp._probe_github`.
    Unset { key: String },
}

#[derive(Subcommand)]
pub enum McpCmd {
    /// Remove an MCP server from the config entirely (disconnects it).
    Remove { name: String },
}

#[derive(Subcommand)]
pub enum SkillCmd {
    /// Remove a skill (only ones managed inside amty's own skills dir).
    Remove { name: String },
}

#[derive(Subcommand)]
pub enum NoteCmd {
    /// Print a note's markdown content.
    Read { name: String },
    /// Create/overwrite a note. Content comes from the args, or stdin when
    /// none are given (`amty notes write x < file.md`).
    Write { name: String, content: Vec<String> },
    /// Delete a note permanently.
    #[command(alias = "rm")]
    Remove { name: String },
    /// Full-text search over note names and contents.
    Search {
        query: Vec<String>,
        #[arg(long, default_value = "50")] limit: usize,
        /// print the raw API JSON instead of a table
        #[arg(long)] json: bool,
    },
    /// Print the notes directory path.
    Dir,
}

#[derive(Subcommand)]
pub enum JobCmd {
    /// Register a scheduled job. Exactly one of --every / --at is required.
    Add {
        /// Job name (defaults to the prompt's first characters).
        #[arg(long)]
        name: Option<String>,
        /// Fire on an interval: `30s` `15m` `6h` `1d` (combining works: `1h30m`).
        #[arg(long)]
        every: Option<String>,
        /// Fire daily at local HH:MM.
        #[arg(long)]
        at: Option<String>,
        /// Reuse an existing session; default is a fresh `[job]` session per fire.
        #[arg(long)]
        session: Option<String>,
        /// The prompt to send when the job fires.
        prompt: Vec<String>,
    },
    /// Delete a job (id, unique prefix, or name).
    #[command(alias = "rm")]
    Remove { id: String },
    /// Fire a job immediately regardless of schedule.
    Run { id: String },
    /// Enable a disabled job.
    Enable { id: String },
    /// Disable a job without deleting it.
    Disable { id: String },
}

#[derive(Subcommand)]
pub enum EvolveCmd {
    /// Force a reflection pass on a session (empty = most recent).
    Reflect { session: Option<String> },
    /// Erase a learned auto-approval (the exact command line).
    Forget { command: String },
    /// Drop a scheduled self-swap/restart before it fires.
    Cancel,
}

/// Linux/WSL display fixups: point XDG_RUNTIME_DIR at WSLg's socket dir when
/// the Wayland socket isn't reachable, else drop WAYLAND_DISPLAY so winit
/// falls back to X11.
#[cfg(target_os = "linux")]
fn fix_display_env() {
    use std::path::Path;
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let reachable = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join(&wayland).exists())
        .unwrap_or(false);
    if reachable {
        return;
    }
    if Path::new("/mnt/wslg/runtime-dir").join(&wayland).exists() {
        std::env::set_var("XDG_RUNTIME_DIR", "/mnt/wslg/runtime-dir");
    } else if std::env::var("DISPLAY").is_ok() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }
}

#[cfg(not(target_os = "linux"))]
fn fix_display_env() {}

/// Windows: allocate a console only when asked (`--console` / AMTY_CONSOLE).
/// Skipped when a console is already attached (e.g. launched from cmd).
#[cfg(target_os = "windows")]
fn maybe_alloc_console(wants: bool) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
        fn AllocConsole() -> i32;
    }
    let wants = wants || std::env::var_os("AMTY_CONSOLE").is_some();
    if wants && unsafe { GetConsoleWindow() }.is_null() {
        unsafe { AllocConsole() };
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(target_os = "windows")]
    maybe_alloc_console(cli.console);
    match cli.cmd.unwrap_or(Cmd::Gui) {
        Cmd::Gui => {
            fix_display_env();
            let rt = tokio::runtime::Runtime::new()?;
            let app = rt.block_on(app::App::start())?;
            let r = gui::run(rt, app);
            // cleanup runtime file on exit — but only if it still points at
            // this instance (a respawned GUI may have written its own already)
            if std::fs::read_to_string(config::runtime_path())
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v["pid"].as_u64())
                == Some(std::process::id() as u64)
            {
                let _ = std::fs::remove_file(config::runtime_path());
            }
            r.map_err(|e| anyhow::anyhow!("gui: {e}"))
        }
        Cmd::McpServe => mcp_serve::run(),
        // no tokio, no App, no config — the helper must work even while the
        // runtime/config state is being shut down by the exiting parent
        Cmd::SelfSwap { wait_pid, install, target } => {
            evolve::self_swap(wait_pid, install, target)
        }
        Cmd::ImportClaude => {
            let (mut cfg, path) = config::Config::load()?;
            match config::import_claude(&mut cfg) {
                Ok(added) => {
                    cfg.save(&path)?;
                    println!("imported {} server(s): {}", added.len(), added.join(", "));
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        cmd => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cli::run(cmd))
        }
    }
}

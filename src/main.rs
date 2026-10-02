mod agent;
mod api;
mod app;
mod cli;
mod config;
mod gui;
mod mcp;
mod mcp_serve;
mod provider;
mod session;
mod tools;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "amty", version, about = "lightweight scriptable AI chat & agent client")]
pub struct Cli {
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
    /// Show MCP server statuses.
    Mcp,
    /// Expose the running instance as an MCP server (stdio) for external agents.
    McpServe,
    /// Import mcpServers from claude_desktop_config.json into config.toml.
    ImportClaude,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    Get { key: Option<String> },
    Set { key: String, value: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Gui) {
        Cmd::Gui => {
            let rt = tokio::runtime::Runtime::new()?;
            let app = rt.block_on(app::App::start())?;
            let r = gui::run(rt, app);
            // cleanup runtime file on exit
            let _ = std::fs::remove_file(config::runtime_path());
            r.map_err(|e| anyhow::anyhow!("gui: {e}"))
        }
        Cmd::McpServe => mcp_serve::run(),
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

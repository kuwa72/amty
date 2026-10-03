# amty

Lightweight, scriptable AI chat & agent client. Rust + egui (no webview, no Electron).
Main target: macOS. Development happens on WSL/Linux; the same code builds for macOS.

## Architecture

```
GUI (egui, src/gui.rs)          -- thin; renders session store + event bus
  |
App core (src/app.rs)           -- config, sessions, approvals, event broadcast
  |- agent loop (src/agent.rs)  -- provider stream -> tool calls -> results -> repeat
  |- providers (src/provider.rs)-- Anthropic native + OpenAI-compatible (hand-rolled SSE)
  |                             + Command Code (POST /alpha/generate, NDJSON stream,
  |                               key from ~/.commandcode/auth.json)
  |- builtin tools (src/tools.rs) -- fs_read/fs_list/fs_write/fs_edit/shell/config_*/mcp
  |- MCP client (src/mcp.rs)    -- stdio JSON-RPC, tools exposed as mcp__<srv>__<tool>
  |- MCP catalog (src/catalog.rs) -- curated `npx -y` servers (notion/slack/gmail/
  |                               gdrive/gdocs); GUI one-click install + env form
  |
Control API (src/api.rs)        -- axum on 127.0.0.1:<ephemeral>, Bearer token.
                                   port+token in $XDG_DATA_HOME/amty/runtime.json
  |- CLI (src/cli.rs)           -- `amty send/sessions/status/config/approve/...`
  `- MCP facade (src/mcp_serve.rs) -- `amty mcp-serve` exposes the app itself as an
                                    MCP server so external agents can drive it
```

Key idea: one control plane, three surfaces — GUI, CLI, MCP. Anything the agent can
do via `config_set`, a script can do via `POST /v1/config`, and vice versa.

## Files

- config: `$XDG_CONFIG_HOME/amty/config.toml` (auto-created; `mcp_servers` uses the
  same shape as `claude_desktop_config.json`'s `mcpServers`)
- sessions: `$XDG_DATA_HOME/amty/sessions/<id>.jsonl` (meta line + messages)
- runtime discovery: `$XDG_DATA_HOME/amty/runtime.json` (0600)

## Build / test / run

```sh
cargo check          # fast type check
cargo test           # unit tests (all offline)
cargo build          # debug binary at target/debug/amty
cargo build --release

./target/debug/amty              # GUI (needs a display; on WSL use WSLg:
                                 #   XDG_RUNTIME_DIR=/mnt/wslg/runtime-dir)
./target/debug/amty status       # requires a running instance
./target/debug/amty send "hi"    # most recent session; --new, -s <id>, --no-wait
./target/debug/amty mcp-serve    # stdio MCP server for external agents
./target/debug/amty catalog      # list installable MCP servers
./target/debug/amty install notion NOTION_TOKEN=ntn_…   # catalog install via API

# Windows native binary (cross-compiled from WSL; needs mingw-w64 + cmake):
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/amty.exe  (~18 MB, static, no DLLs)
# config on Windows: %APPDATA%\amty\config.toml
```

## Conventions

- Egui 0.36: `eframe::App` uses `fn ui(&mut self, ui: &mut Ui, frame)`; panels are
  `egui::Panel::{left,right,top,bottom}` and `CentralPanel`, all taking `&mut Ui`.
- reqwest 0.13: TLS feature is `rustls` (platform verifier), not `rustls-tls`.
- No MCP/LLM SDK deps on purpose — hand-rolled SSE + JSON-RPC keeps the dep tree
  and binary small.
- CJK fonts are loaded from OS font paths at startup (see `install_cjk_fonts`);
  egui's bundled fonts have no CJK glyphs.

## Known limits / TODO

- Command Code provider: provider name `commandcode`. Auth comes from
  `~/.commandcode/auth.json` (sign in with the `cmdc` CLI first), or set
  `providers.commandcode.api_key` / `api_key_env`. The server requires an
  `x-command-code-version` header — amty detects it via `cmdc --version`.
  Model ids are like `deepseek/deepseek-v4-flash` or `kimi-k2.5`
  (see `cmdc --list-models`).
- Japanese IME works on macOS/Windows native, NOT under WSLg (no
  `text_input_v3` in the WSLg compositor). On WSL use paste or `amty send`.
- MCP client supports stdio transport only (no streamable-HTTP servers yet).
- Catalog installs need Node.js. On Windows npx is resolved from PATH, falling
  back to `%ProgramFiles%\nodejs\npx.cmd` (fresh installs land there while the
  running process still has a stale PATH); when a fallback path is used its dir
  is injected into the child's PATH env so `node` resolves. MCP children get
  `current_dir = ~` since npm refuses to run under a UNC cwd. First-run npx
  downloads are slow — MCP handshake timeout is 120 s. Google-family servers
  still need a manual OAuth step (see each entry's note in src/catalog.rs); a
  failed start surfaces the server's stderr tail in the status.
- Anthropic provider needs `ANTHROPIC_API_KEY`; OAuth token heuristic is minimal.
- No token accounting, retries/backoff, or prompt caching yet.
- macOS packaging (.app bundle, signing) not set up; `cargo build` on macOS works.

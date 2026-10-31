# amty

Lightweight, scriptable AI chat & agent client. Rust + egui (no webview, no Electron).
Main target: macOS. Development happens on WSL/Linux; the same code builds for macOS.

## Architecture

```
GUI (egui, src/gui.rs)          -- thin; renders session store + event bus
  |
App core (src/app.rs)           -- config, sessions, approvals, event broadcast
  |- agent loop (src/agent.rs)  -- provider stream -> tool calls -> results -> repeat;
  |                             auto-compacts via src/compact.rs when the
  |                             estimated prompt nears the context window
  |                             (LLM summary stands in for the older span in
  |                             provider requests only — `Session::compact`,
  |                             so the log/GUI keep every message and stay
  |                             scrollable);
  |                             abnormal exits persist a `[amty: run ended —
  |                             <reason>]` user msg so the log records why
  |- providers (src/provider.rs)-- Anthropic native + OpenAI-compatible (hand-rolled SSE)
  |                             + Command Code (POST /alpha/generate, NDJSON stream,
  |                               key from ~/.commandcode/auth.json)
  |- builtin tools (src/tools.rs) -- fs_read/fs_list/fs_write/fs_edit/shell/config_*/mcp
  |                               /web_search/web_fetch
  |- token accounting (src/types.rs `Usage`) -- providers emit a per-call
  |                               StreamEvent::Usage including cache read/write;
  |                               folded into Session::usage (persisted) and
  |                               shown in the GUI header + `amty usage`
  |- MCP client (src/mcp.rs)    -- stdio JSON-RPC + streamable HTTP transports,
  |                               tools exposed as mcp__<srv>__<tool>
  |- notes (src/notes.rs)       -- plain-markdown knowledge store; a note's
  |                               name is its /-separated path under the root
  |                               minus .md. note_* tools / /v1/notes API /
  |                               `amty notes` CLI / amty_*_notes MCP tools /
  |                               GUI right panel + floating editor window
  |- OAuth client (src/oauth.rs) -- auth-code + PKCE + localhost:8571 callback,
  |                               RFC 9728/8414 discovery, dynamic client
  |                               registration, tokens in $XDG_DATA_HOME/amty/oauth/
  |- MCP catalog (src/catalog.rs) -- curated `npx -y` servers (notion/
  |                               slack) + a hosted OAuth server (notion-
  |                               hosted);
  |                               GUI one-click install + env/OAuth form
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
- skills: `$XDG_CONFIG_HOME/amty/skills/<name>/SKILL.md` — auto-indexed into the
  system prompt each turn (`config::skills_prompt`); the first non-heading line
  becomes the description. Agents write machine-specific operational notes here
  (e.g. `windows-env`, `wsl-env`, `gws`) instead of probing the environment.
- memory: `$XDG_CONFIG_HOME/amty/MEMORY.md` — written by the reflection pass,
  injected into the system tail each turn (`evolve::memory_prompt`, ~6 KB cap)
- sessions: `$XDG_DATA_HOME/amty/sessions/<id>.jsonl` (meta line + messages).
  `SessionStore::search` / `search_in` do a plain in-memory scan over titles,
  message text and tool calls (no index; the whole store is already loaded);
  exposed as `GET /v1/search?q=…[&session=…][&limit=N]`, `amty search <query>
  [-s <id>] [--limit N] [--json]`, the `amty_search_sessions` MCP tool, and the
  GUI sidebar box (typing filters titles, Enter runs the full-text search).
- notes: `$XDG_DATA_HOME/amty/notes/**/*.md`, or `notes.dir` in config
  (`~` expanded) — point it at an Obsidian vault to share the store with a
  human editor. Writes are atomic (tmp+rename); `note_write`/`note_delete`
  are Write-risk but implicitly allowlisted (they can only touch the notes
  root). Surfaced as `GET /v1/notes`, `GET /v1/notes-search?q=…`,
  `GET|PUT|DELETE /v1/notes/{*name}` (name keeps its `/`s), `amty notes
  [read|write|remove|search|dir]` (write takes args or stdin), and the
  GUI's right panel (list/filter) + floating viewer/editor window.
  `notes::prompt` appends the dir location to the system tail each turn.
- runtime discovery: `$XDG_DATA_HOME/amty/runtime.json` (0600)
- evolve state: `$XDG_DATA_HOME/amty/evolve.json` (tool stats, learned
  approvals, reflection watermarks), `evolve.log`, `self-update.log`

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
./target/debug/amty search rust -s <id>   # cross-session full-text search; --limit N --json
./target/debug/amty notes        # list notes; read/write/rm/search/dir subs
./target/debug/amty mcp-serve    # stdio MCP server for external agents
./target/debug/amty catalog      # list installable MCP servers
./target/debug/amty mcp remove <name>      # delete a server from config
./target/debug/amty config unset mcp.<name># generic delete (mcp./provider./search.)
./target/debug/amty install notion NOTION_TOKEN=ntn_…   # catalog install via API
./target/debug/amty auth gdrive [--status]   # run a catalog entry's OAuth flow
./target/debug/amty evolve            # self-evolution state (memory/stats/restart)
./target/debug/amty evolve <session>  # force a reflection pass on a session

# Windows native binary (cross-compiled from WSL; needs mingw-w64 incl. windres):
./scripts/build-windows.sh          # build + deploy to %USERPROFILE%\.local\bin\amty.exe
                                    # (refuses while amty.exe runs; --force kills it first)
# equivalent by hand:
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/amty.exe  (~24 MB, static, no DLLs)
# release uses thin LTO — fat LTO + codegen-units=1 spent ~5 min in rustc.
# config on Windows: %APPDATA%\amty\config.toml
# - exe icon: build.rs + embed-resource compiles assets/amty.rc (points at
#   assets/amty-icon.ico, regenerated by scripts/gen_icon.py)
# - windows_subsystem="windows": no console on double-click. CLI subcommands
#   still print into the invoking console, but cmd.exe does not wait for
#   GUI-subsystem exes (use PowerShell/WSL for scripting). `--console` or
#   AMTY_CONSOLE=1 AllocConsole()s a debug console for GUI mode.
# - link error `thread_pthread.o: undefined reference to 'nanosleep64'` means the
#   cached aws-lc-sys C objects for that profile are stale (built against an
#   older mingw header). Fix: delete
#   target/x86_64-pc-windows-gnu/<profile>/build/aws-lc-sys-* and
#   .../deps/libaws_lc_sys-*, then rebuild.
```

## Conventions

- Egui 0.36: `eframe::App` uses `fn ui(&mut self, ui: &mut Ui, frame)`; panels are
  `egui::Panel::{left,right,top,bottom}` and `CentralPanel`, all taking `&mut Ui`.
- reqwest 0.13: TLS feature is `rustls` (platform verifier), not `rustls-tls`.
- No MCP/LLM SDK deps on purpose — hand-rolled SSE + JSON-RPC keeps the dep tree
  and binary small.
- CJK fonts are loaded from OS font paths at startup (see `install_cjk_fonts`);
  egui's bundled fonts have no CJK glyphs.

## Self-evolution (src/evolve.rs)

amty continuously improves itself; toggles live under `evolve.*` config keys
(all default on):

- **Reflection** (`evolve.reflect`): after each run ends, if the session grew by
  ≥6 messages since the last pass, a one-shot provider call (`provider::stream_text`
  — shared with auto-compaction) reads the transcript tail + current MEMORY.md +
  tool stats and emits fenced blocks: ` ```memory ` (full MEMORY.md rewrite,
  atomic via tmp+rename), ` ```skill <name> ` (complete SKILL.md into the skills
  dir, ≤3/pass, `evolve.skills` gate), ` ```config ` (whitelisted keys only:
  `+allow_commands`, `+allow_paths`, `max_tokens`, `search.*` — never provider/
  model/system_prompt). Results surface as `EvKind::Evolved` → GUI toast.
  Cost control: ≤20 reflections/rolling day (`reflection_times`) + a 10 min
  cooldown after a provider failure (`reflect_fail_until`); the manual
  `reflect` action bypasses both.
- **Approval learning** (`evolve.learn_approvals`): in `ask`/`allowlist` mode,
  approving the exact same `shell` command line 3 times auto-allows it from
  then on (`evolve::auto_approved` in `approval_gate`). Exact match only.
  Unlearn via `POST /v1/evolve/forget`, `self_update forget <command>`, or
  `amty evolve forget <cmd>`.
- **Self-rebuild** (`evolve.self_modify`): the `self_update` tool —
  `status | reflect [session] | rebuild | install <path> | restart | forget`.
  `rebuild` runs `cargo build` detached in `CARGO_MANIFEST_DIR` (profile matches
  the running exe: target/debug → debug, else release) logging to
  `self-update.log`, since a cold build can exceed the 300 s tool cap. If the
  artifact path equals the running exe the build is redirected to
  `target/self-update/` — a running exe can't be linked over (Windows lock,
  unix ETXTBSY), so it builds out of the way and the swap handles installation.
- **Install a prebuilt binary** (`install`): for machines where `rebuild`
  cannot run — e.g. a Windows amty cross-built from WSL, whose
  `CARGO_MANIFEST_DIR` is a `/home/...` path (`source_dir: null`) and where no
  Windows cargo exists. `install` takes the artifact path, validates it
  (exists, is a file, `.exe` on Windows, not the running exe, >1 MiB, and
  `<artifact> --version` actually runs and prints `amty`), sets
  `built_binary`, and hands off to the same restart path. Smoke-testing
  *before* the swap matters: afterwards a rollback is the only recourse.
  Deploy recipe: `cargo build --release --target x86_64-pc-windows-gnu` in
  WSL → copy to a Windows path from the WSL side (`/mnt/c…`; `cmd /c copy
  \\wsl.localhost\…` silently copies 0 files) → `self_update install path=…`.
  Deploying *from* a binary older than the `install` action needs one manual
  rename-aside/copy (renaming a running exe is legal on Windows); after that,
  `install` works. Machine-specific notes live in the `amty-self-update` skill.
- **Trigger paths**: rebuild/install/restart are only reachable through the
  `self_update` agent tool — drive it by asking the agent (`amty send "call
  self_update rebuild"` or the GUI). There is no CLI subcommand or API
  endpoint that builds (`amty evolve` and `GET /v1/evolve` are read-only; the
  `amty_evolve` MCP facade exposes status|reflect|forget only). rebuild/
  install/restart count as `Risk::Exec` → they prompt for approval in `ask`
  mode. `self_update restart` with no freshly `built_binary` only re-launches
  the same exe (install == target → swap helper skips the copy); replacing the
  exe requires a `rebuild`- or `install`-produced artifact. `self_update
  status` shows `source_dir`/`build_profile`/`building`/`built_binary`/
  `restart_pending` — check it before claiming a rebuild is possible.
- **Restart/swap** (`evolve.auto_restart`): when all runs are idle (checked
  after `end_run`), `maybe_restart` copies the running exe to
  `$XDG_DATA_HOME/amty/self-swap/amty-helper` and spawns it as the hidden
  `amty self-swap --wait-pid <pid> --install <artifact> --target <exe>`
  subcommand, then `process::exit` — the helper waits for the parent pid,
  renames the old exe to `amty.old` (renaming a running exe is legal even on
  Windows), copies the new binary in (retry loop, rollback on relaunch
  failure), and respawns it. Same code path on all platforms.
- **Stats**: per-tool calls/errors/denied + approved-command counts persist
  to `evolve.json`; exposed via `self_update status`, `GET /v1/evolve`,
  `amty evolve`, the `amty_evolve` MCP facade tool (status|reflect|forget),
  and fed into the reflection prompt. The Settings GUI has an "evolve" tab
  with the five toggles plus a read-only MEMORY.md viewer.
- **Verified on Windows**: `self_update restart` from an agent session drove
  the full swap (helper copy → pid wait → exe replace → relaunch) including
  a real binary replacement (0.3.0 → 0.3.0-swaptest), and `self_update
  install` deployed a cross-built artifact the same way (2026-10-21: exe
  replaced, pid changed, new `amty search` working). A Windows binary whose
  CARGO_MANIFEST_DIR points into WSL reports `source_dir: null` — rebuild
  there needs a Windows-side checkout + cargo. So the deployed
  `%USERPROFILE%\.local\bin\amty.exe` (cross-built from WSL) can swap/restart
  itself and install a prebuilt artifact, but cannot self-*rebuild*;
  `./scripts/build-windows.sh` remains the build-and-deploy script for that
  machine (it still requires amty to be stopped, `--force` to kill it).

## Known limits / TODO

- Command Code provider: provider name `commandcode`. Auth comes from
  `~/.commandcode/auth.json` (sign in with the `cmdc` CLI first), or set
  `providers.commandcode.api_key` / `api_key_env`. The server requires an
  `x-command-code-version` header — amty detects it via `cmdc --version`.
  Model ids are like `deepseek/deepseek-v4-flash` or `kimi-k2.5`
  (see `cmdc --list-models`). On Windows `cmdc` is resolved via `where` plus
  npm/scoop/fnm fallback paths (npm installs a `cmdc.cmd` shim under
  `%APPDATA%\npm`); when cmdc is unavailable the model picker falls back to a
  static shortlist (`FALLBACK_CC_MODELS`) since the field is free-text anyway.
- Japanese IME works on macOS/Windows native, NOT under WSLg (no
  `text_input_v3` in the WSLg compositor). On WSL use paste or `amty send`.
- MCP client supports stdio + streamable HTTP. Hosted servers set `url` (and
  `oauth_client_id`/`oauth_client_secret`/`oauth_scopes`/`oauth_token_url`) in
  `mcp_servers` with `command = ""`. OAuth flow: `POST /v1/mcp-auth` → browser +
  `http://127.0.0.1:8571/oauth/callback` + PKCE; tokens go to
  `$XDG_DATA_HOME/amty/oauth/<id>.json` (mode 0600 on unix), refresh runs
  automatically via the stored `oauth_token_url`. Entries with fixed
  auth/token endpoints (Google) skip discovery; others use RFC 9728/8414
  discovery + dynamic client registration (verified against mcp.notion.com).
- npm-based catalog installs need Node.js (hosted entries don't). On Windows,
  bare command names that resolve to `.cmd`/`.bat` shims (npx, npm, uvx…)
  can't be spawned by CreateProcess — `catalog::shim_where` resolves them via
  `where` at MCP spawn time (`resolve_spawn_command` in `mcp.rs`), and
  `npx_exe` additionally falls back to `%ProgramFiles%\nodejs\npx.cmd` for
  stale-PATH first runs. `spawn_warnings` surfaces bad invocations (e.g.
  `powershell -File` pointed at a non-.ps1) in install/add responses. MCP children get
  `current_dir = ~` since npm refuses to run under a UNC cwd. First-run npx
  downloads are slow — MCP handshake timeout is 120 s. A failed start surfaces
  the server's stderr tail in the status. Catalog entries
  with `auth_cmd` get a GUI/CLI auth runner (`POST /v1/mcp-auth`) that spawns
  `npx -y <pkg> <cmd>` with the server's env, logs to
  `$XDG_DATA_HOME/amty/auth-<id>.log`, then reconnects the server.
- web_search backend is `search.backend` in config: `duckduckgo` (default, keyless,
  HTML scrape of html.duckduckgo.com — can rate-limit; falls back to
  lite.duckduckgo.com) | `brave` (BRAVE_API_KEY) |
  `tavily` (TAVILY_API_KEY) | `searxng` (search.base_url). Keys also settable via
  `search.api_key`/`search.api_key_env`; limits via `search.max_results` (default 5)
  and `search.fetch_max_chars` (default 20000). web_fetch rejects non-http(s)
  schemes and loopback/private/link-local targets (SSRF guard; redirects are
  re-checked per hop). SearXNG base_url is exempt since it's user-configured.
- Google Workspace access goes through the `gws` CLI (googleworkspace/cli) via
  the shell tool — the hosted Google MCP endpoints need Developer Preview
  enrollment, so no Google entries are shipped in the catalog. `gws` is in the
  default `allow_commands`; auth lives in `~/.config/gws/` (per-OS).
- Anthropic provider needs `ANTHROPIC_API_KEY`; OAuth token heuristic is minimal.
- **Speed budget (measured, 182 local sessions / 4,947 tool results).** Wall
  clock is dominated by *round trips*, not context size: one provider round trip
  is 3.0–3.6 s, a tool call adds its own execution time, and a 1.5 M-char
  session still answers a no-tool prompt in 3.0 s (same as an empty one, because
  93% of input is cache reads). So: reduce round trips, not tokens.
  - Known waste, and what fixed it: 315 tool calls were byte-identical repeats
    of an earlier call *in the same session* (`fs_read` 134, `shell` 113), each
    costing a full round trip. `App::tool_cache` now memoizes read-only calls
    (`agent::cacheable`: fs_read/fs_list/web_fetch/config_get + `mcp`
    list|tools) per session, and any non-cacheable call
    (`invalidate_tool_cache`) drops the memo so a read after a write is never
    stale. Failures are not memoized.
  - `tools::MAX_OUT` 48 KB → 16 KB: 4% of results exceeded 8 KB and carried 73%
    of all result bytes (`fs_read` alone 4.9 MB of 7.2 MB). Measured re-read
    totals: −25% (itunes), −9% (session-analysis), −1% (harness2) — a modest
    win, and the reason not to go lower: a smaller cap forces the model to
    re-read files in slices, which costs more round trips than it saves bytes.
  - `web_search` DDG failure: 19 of 43 historical calls errored. Measured
    cause — 8 rapid searches succeed, then every call returns HTTP 202 with an
    empty body for 20+ minutes regardless of User-Agent, so retrying is
    pointless in-session. `ddg_search` now falls through html → lite → Bing
    (`bing_search`, keyless, `parse_bing` decodes the `/ck/a?u=a1<base64url>`
    redirect) and notes the substitution in the result; `search.backend=bing`
    selects it directly.
  - Parallel tool execution was considered and rejected: 57% of multi-call
    steps mix a write or `shell` with other calls (so most are not safely
    parallel), and concurrency saves tool wall time while leaving the round-trip
    count — the dominant cost — untouched.
- Tool calls run inside a 200 ms cancel poll (drop aborts the request /
  kill_on_drop kills the shell child) with a 300 s hard cap in `run_tool`;
  web_fetch DNS lookups are separately capped at 10 s.
- Auto-compaction (src/compact.rs): each turn estimates input tokens
  (chars/4 ASCII, 1/char non-ASCII — no tokenizer dep) and, past 90% of
  (context_window - max_tokens), asks the provider to summarize the older
  messages and rewrites the session as [summary + recent tail]. The tail
  always opens on a plain user-text message so tool_result blocks are never
  orphaned. Context size: `provider.<name>.context_window` override >
  Command Code model list's `context_length` (floor 200K, its smallest
  model) > defaults (Anthropic 200K, OpenAI-compatible 128K). If the
  summary call fails, a plain "N messages omitted" marker is used instead.
  Token accounting is real now: providers emit `StreamEvent::Usage` per call
  (Command Code `finish-step`/`finish` → `cc_usage`; OpenAI `include_usage` →
  `openai_usage`; Anthropic `message_start`+`message_delta` → `anthropic_usage`,
  split by phase so input isn't counted twice), `retry_stream` forwards it
  *before* the terminal `Done` and only for the attempt that survives, the
  agent loop folds it into `Session::usage` (persisted in the meta line, so
  totals survive a restart), and it surfaces three ways: `EvKind::Usage` →
  header badge, `GET /v1/status` → `usage`, and `amty usage [-s <id>]`.
  `Usage::uncached_input()` is the number that matters: on a long agent turn
  ~97% of input is cache reads (a real JobBench-style rollout: input 141730 /
  output 44991 / cache_read 5476480), so the cache split is the only thing that
  explains the bill. Provider streams retry transient
  failures (network resets, decode errors, 5xx) up to 3 attempts with
  exponential backoff via `provider::retry_stream` — replays are deduplicated
  so already-forwarded text/tool_use events aren't doubled; permanent
  failures (4xx, missing keys) fail fast. Anthropic requests
  carry `cache_control: ephemeral` breakpoints on the tools tail, the static
  system block, and the last message — `ChatRequest.system` must stay
  byte-stable, volatile additions go in `system_tail` (the skills index, which
  is re-scanned every turn). `Mcp::specs()` sorts tool names for the same
  reason: keep the request prefix deterministic.
- Window/GPU resilience (src/gui.rs): `rescue_offscreen_window` polls on a 1 s
  heartbeat (Windows only) and recenters via Win32 `MonitorFromRect` when the
  window ends up fully off all monitors (eframe only clamps the restored
  position at startup). `guard_gpu_loss` installs wgpu `set_device_lost_callback`
  + `on_uncaptured_error` — a real device loss (`Unknown` reason; `Destroyed` is
  normal teardown) respawns the exe and closes cleanly, and `run_native` is
  wrapped in `catch_unwind` so panics escaping wgpu-hal get the same treatment.
  Non-device-loss wgpu errors still panic. `respawn_self` is deduped by a static
  AtomicBool; `main` only removes `runtime.json` when its pid still matches so a
  respawned instance's file isn't deleted.
- macOS packaging (.app bundle, signing) not set up; `cargo build` on macOS works.
<!-- lead-flow begin (managed by `lead enable`; do not edit) -->
## lead-flow（`/lead-flow <issue-number>`）

このプロジェクトでは `lead` を使った Issue 駆動開発フローを採用している。ユーザーが `/lead-flow <issue-number>` で作業を指示したら、以下を実行する。

1. `lead run <issue-number>` でブランチ・作業状態を作成する。
2. TDD：まず失敗するテストを追加し Red を確認してから実装し、Green を確認する。
3. `./test/run-tests.sh`（または `go test ./...`、プロジェクト固有のテストコマンド）をすべてパスさせる。
4. `git push -u origin $(git branch --show-current)` でブランチを push する。
5. `gh pr create --fill` で PR を作成する。
6. `bin/ci-wait <pr-number>`（または `gh pr checks`）で CI が pass するまで待つ。
7. `gh pr merge --squash`（または `lead finish <issue-number> --merge`）でマージする。
8. 関連 Issue が closed になったことを確認する。

ルール：
- `main` には直接 push しない。`issue/<number>-<slug>` ブランチで作業する。
- 1 Issue に原則 1 ブランチ/PR とし、Issue 側で明示がない限り分割しない。
- `AGENTS.md`、`.agents/`、`.claude/`、`.devin/` などのプロジェクト規約ファイルは、当該 Issue で変更が明示されていない限り編集しない。

<!-- lead-flow end -->

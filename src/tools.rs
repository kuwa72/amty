use crate::app::App;
use crate::config::{ApprovalMode, Config, SearchConf};
use crate::types::ToolSpec;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Safe,
    Write,
    Exec,
}

pub fn risk_of(name: &str) -> Risk {
    match name {
        "fs_read" | "fs_list" | "config_get" | "mcp" | "web_search" | "web_fetch" => Risk::Safe,
        "shell" => Risk::Exec,
        _ => {
            if name.starts_with("mcp__") {
                Risk::Write
            } else {
                Risk::Write
            }
        }
    }
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "fs_read".into(),
            description: "Read a text file. Returns content with 1-based line numbers.".into(),
            schema: schema(
                json!({
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "first line, 1-based"},
                    "limit": {"type": "integer", "description": "max lines"}
                }),
                &["path"],
            ),
        },
        ToolSpec {
            name: "fs_list".into(),
            description: "List directory entries. Directories end with '/'.".into(),
            schema: schema(json!({"path": {"type": "string"}}), &["path"]),
        },
        ToolSpec {
            name: "fs_write".into(),
            description: "Write (create or overwrite) a file. Creates parent directories.".into(),
            schema: schema(
                json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        },
        ToolSpec {
            name: "fs_edit".into(),
            description: "Exact string replacement in a file. Fails if `old` is missing or matches multiple times unless replace_all.".into(),
            schema: schema(
                json!({
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                }),
                &["path", "old", "new"],
            ),
        },
        ToolSpec {
            name: "shell".into(),
            description: "Run a shell command and capture stdout/stderr. Times out after `timeout` seconds (default 60, max 300).".into(),
            schema: schema(
                json!({
                    "command": {"type": "string"},
                    "workdir": {"type": "string"},
                    "timeout": {"type": "integer"}
                }),
                &["command"],
            ),
        },
        ToolSpec {
            name: "web_search".into(),
            description: "Search the web. Returns numbered results (title, url, snippet). Follow up with web_fetch to read a page. Backend and limits: search.* settings.".into(),
            schema: schema(
                json!({
                    "query": {"type": "string"},
                    "count": {"type": "integer", "description": "max results, 1-10"}
                }),
                &["query"],
            ),
        },
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch a URL and return readable text (HTML stripped, truncated).".into(),
            schema: schema(
                json!({
                    "url": {"type": "string"},
                    "max_chars": {"type": "integer", "description": "max characters to return"}
                }),
                &["url"],
            ),
        },
        ToolSpec {
            name: "config_get".into(),
            description: "Read an amty setting. Keys: provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, search, provider.<name>.<field>, search.<field>.".into(),
            schema: schema(json!({"key": {"type": "string", "description": "omit to list keys"}}), &[]),
        },
        ToolSpec {
            name: "config_set".into(),
            description: "Change an amty setting at runtime (persisted). Keys: provider, model, approval(ask|auto|allowlist), system_prompt, max_tokens, allow_commands, allow_paths, +allow_commands, +allow_paths, provider.<name>.{model,api_key,api_key_env,base_url,context_window}, mcp.<name>.enabled, search.{backend,api_key,api_key_env,base_url,max_results,fetch_max_chars}.".into(),
            schema: schema(
                json!({"key": {"type": "string"}, "value": {"type": "string"}}),
                &["key", "value"],
            ),
        },
        ToolSpec {
            name: "config_unset".into(),
            description: "Delete an amty setting (persisted) — the removal counterpart of config_set. Keys: mcp.<name> | provider.<name> | provider.<name>.{api_key,api_key_env,base_url,max_tokens,context_window} | search.{api_key,api_key_env,base_url}.".into(),
            schema: schema(
                json!({"key": {"type": "string", "description": "e.g. mcp._probe_github"}}),
                &["key"],
            ),
        },
        ToolSpec {
            name: "mcp".into(),
            description: "Manage MCP servers. actions: list | tools | enable <name> | disable <name> | reconnect <name> | remove <name>.".into(),
            schema: schema(
                json!({
                    "action": {"type": "string"},
                    "server": {"type": "string"}
                }),
                &["action"],
            ),
        },
        ToolSpec {
            name: "mcp_search".into(),
            description: "Search npm for MCP server packages. Returns id/package/description candidates.".into(),
            schema: schema(
                json!({
                    "query": {"type": "string", "description": "search keyword, e.g. github or postgres"},
                    "count": {"type": "integer", "description": "max results 1-50"}
                }),
                &["query"],
            ),
        },
        ToolSpec {
            name: "mcp_install".into(),
            description: "Add an MCP server to config and connect it. id=name in config, package=npm/uvx package, command=npx|uvx, args and env optional.".into(),
            schema: schema(
                json!({
                    "id": {"type": "string"},
                    "package": {"type": "string"},
                    "command": {"type": "string", "description": "npx (default) or uvx"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "env": {"type": "object", "description": "environment variables for the server"}
                }),
                &["id", "package"],
            ),
        },
    ]
}

/// Short human-readable summary for approval prompts.
pub fn summarize(name: &str, input: &Value) -> String {
    match name {
        "shell" => format!("shell: {}", input["command"].as_str().unwrap_or("")),
        "fs_write" | "fs_edit" => format!("{name}: {}", input["path"].as_str().unwrap_or("")),
        "fs_read" | "fs_list" => format!("{name}: {}", input["path"].as_str().unwrap_or("")),
        "web_search" => format!("web_search: {}", input["query"].as_str().unwrap_or("")),
        "web_fetch" => format!("web_fetch: {}", input["url"].as_str().unwrap_or("")),
        "config_set" => format!("config_set: {} = {}", input["key"].as_str().unwrap_or(""), input["value"].as_str().unwrap_or("")),
        "config_unset" => format!("config_unset: {}", input["key"].as_str().unwrap_or("")),
        "mcp" => format!("mcp: {} {}", input["action"].as_str().unwrap_or(""), input["server"].as_str().unwrap_or("")),
        "mcp_search" => format!("mcp_search: {}", input["query"].as_str().unwrap_or("")),
        "mcp_install" => format!("mcp_install: {} ({})", input["id"].as_str().unwrap_or(""), input["package"].as_str().unwrap_or("")),
        _ => {
            let s = input.to_string();
            format!("{name}: {}", &s[..s.len().min(300)])
        }
    }
}

/// Decide whether `input` needs interactive approval under `cfg`.
pub fn needs_approval(cfg: &Config, name: &str, input: &Value) -> bool {
    // `mcp` is Safe only for read actions; mutations count as Write.
    let mutating_mcp = name == "mcp"
        && !matches!(input["action"].as_str().unwrap_or("list"), "list" | "tools");
    match if mutating_mcp { Risk::Write } else { risk_of(name) } {
        Risk::Safe => false,
        _ => match cfg.approval {
            ApprovalMode::Auto => false,
            ApprovalMode::Ask => true,
            ApprovalMode::Allowlist => !allowlisted(cfg, name, input),
        },
    }
}

fn allowlisted(cfg: &Config, name: &str, input: &Value) -> bool {
    match name {
        "shell" => {
            let cmd = input["command"].as_str().unwrap_or("");
            cfg.allow_commands.iter().any(|p| cmd == p || cmd.starts_with(&format!("{p} ")))
        }
        "fs_write" | "fs_edit" => {
            let path = input["path"].as_str().unwrap_or("");
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| Path::new(path).to_path_buf());
            cfg.allow_paths.iter().any(|p| {
                let base = std::fs::canonicalize(p).unwrap_or_else(|_| Path::new(p).to_path_buf());
                canon.starts_with(&base)
            })
        }
        _ => false,
    }
}

const MAX_OUT: usize = 48_000;

fn truncate(s: String) -> String {
    if s.len() > MAX_OUT {
        format!("{}…[truncated {} bytes]", &s[..MAX_OUT], s.len() - MAX_OUT)
    } else {
        s
    }
}

pub async fn execute(app: &App, name: &str, input: &Value) -> Result<String, String> {
    match name {
        "fs_read" => {
            let path = input["path"].as_str().unwrap_or("");
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"].as_u64().map(|l| l as usize);
            let lines: Vec<&str> = text.lines().collect();
            let start = (offset - 1).min(lines.len());
            let end = limit.map(|l| (start + l).min(lines.len())).unwrap_or(lines.len());
            let out: String = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{}: {}", start + i + 1, l))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(truncate(out))
        }
        "fs_list" => {
            let path = input["path"].as_str().unwrap_or(".");
            let mut entries: Vec<String> = vec![];
            let rd = std::fs::read_dir(path).map_err(|e| e.to_string())?;
            for e in rd.flatten().take(2000) {
                let mut n = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    n.push('/');
                }
                entries.push(n);
            }
            entries.sort();
            Ok(entries.join("\n"))
        }
        "fs_write" => {
            let path = input["path"].as_str().ok_or("path required")?;
            let content = input["content"].as_str().unwrap_or("");
            if let Some(dir) = Path::new(path).parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, content).map_err(|e| e.to_string())?;
            Ok(format!("wrote {} bytes to {path}", content.len()))
        }
        "fs_edit" => {
            let path = input["path"].as_str().ok_or("path required")?;
            let old = input["old"].as_str().ok_or("old required")?;
            let new = input["new"].as_str().unwrap_or("");
            let all = input["replace_all"].as_bool().unwrap_or(false);
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let count = text.matches(old).count();
            if count == 0 {
                return Err("old string not found".into());
            }
            if count > 1 && !all {
                return Err(format!("old string matches {count} times; set replace_all or make it unique"));
            }
            let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
            std::fs::write(path, out).map_err(|e| e.to_string())?;
            Ok(format!("edited {path} ({count} replacement(s))"))
        }
        "shell" => {
            let command = input["command"].as_str().ok_or("command required")?;
            let timeout = input["timeout"].as_u64().unwrap_or(60).clamp(1, 300);
            let workdir = input["workdir"].as_str();
            run_shell(command, workdir, timeout).await
        }
        "config_get" => {
            let cfg = app.cfg.read().map_err(|e| e.to_string())?;
            match input["key"].as_str() {
                Some(k) => cfg.get(k).map_err(|e| e.to_string()),
                None => Ok("provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, search, provider.<name>.{model,base_url,api_key_env,api_key}, search.{backend,api_key,api_key_env,base_url,max_results,fetch_max_chars}".into()),
            }
        }
        "config_set" => {
            let key = input["key"].as_str().ok_or("key required")?.to_string();
            let value = input["value"].as_str().unwrap_or("").to_string();
            let msg = {
                let mut cfg = app.cfg.write().map_err(|e| e.to_string())?;
                let msg = cfg.apply_set(&key, &value).map_err(|e| e.to_string())?;
                cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
                msg
            };
            if key.starts_with("mcp.") {
                app.mcp.update_servers(app.mcp_servers_map());
                let mcp = app.mcp.clone();
                tokio::spawn(async move { mcp.reconcile().await; });
            }
            Ok(msg)
        }
        "config_unset" => {
            let key = input["key"].as_str().ok_or("key required")?.to_string();
            let msg = {
                let mut cfg = app.cfg.write().map_err(|e| e.to_string())?;
                let msg = cfg.apply_unset(&key).map_err(|e| e.to_string())?;
                cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
                msg
            };
            if key.starts_with("mcp") {
                app.mcp.update_servers(app.mcp_servers_map());
                let mcp = app.mcp.clone();
                tokio::spawn(async move { mcp.reconcile().await; });
            }
            Ok(msg)
        }
        "web_search" => {
            let conf = app.cfg.read().map_err(|e| e.to_string())?.search.clone();
            let query = input["query"].as_str().ok_or("query required")?;
            let count = input["count"].as_u64().unwrap_or(conf.max_results as u64).clamp(1, 10) as usize;
            web_search(&conf, query, count).await.map(truncate)
        }
        "web_fetch" => {
            let conf = app.cfg.read().map_err(|e| e.to_string())?.search.clone();
            let url = input["url"].as_str().ok_or("url required")?;
            let max = input["max_chars"].as_u64().map(|n| n as usize).unwrap_or(conf.fetch_max_chars);
            web_fetch(url, max.clamp(200, MAX_OUT)).await.map(truncate)
        }
        "mcp" => {
            let action = input["action"].as_str().unwrap_or("list");
            match action {
                "list" => Ok(app.mcp.status_text()),
                "tools" => Ok(app.mcp.tools_text()),
                "enable" | "disable" | "reconnect" => {
                    let server = input["server"].as_str().ok_or("server required")?.to_string();
                    {
                        let mut cfg = app.cfg.write().map_err(|e| e.to_string())?;
                        let conf = cfg.mcp_servers.get_mut(&server).ok_or_else(|| format!("unknown server '{server}'"))?;
                        match action {
                            "enable" => conf.enabled = true,
                            "disable" => conf.enabled = false,
                            _ => {}
                        }
                        cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
                    }
                    app.mcp.update_servers(app.mcp_servers_map());
                    app.mcp.reconcile().await;
                    Ok(format!("{action} {server}: done"))
                }
                "remove" => {
                    let server = input["server"].as_str().ok_or("server required")?.to_string();
                    {
                        let mut cfg = app.cfg.write().map_err(|e| e.to_string())?;
                        if cfg.mcp_servers.remove(&server).is_none() {
                            return Err(format!("unknown server '{server}'"));
                        }
                        cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
                    }
                    app.mcp.update_servers(app.mcp_servers_map());
                    app.mcp.reconcile().await;
                    Ok(format!("removed '{server}' from config and disconnected"))
                }
                _ => Err(format!("unknown action '{action}'")),
            }
        }
        "mcp_search" => {
            let query = input["query"].as_str().ok_or("query required")?;
            let count = input["count"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
            let results = crate::catalog::npm_search(query, count).await.map_err(|e| e.to_string())?;
            if results.is_empty() {
                return Ok("no packages found".into());
            }
            Ok(results
                .iter()
                .enumerate()
                .map(|(i, (name, desc))| format!("{}. {}\n   {}", i + 1, name, desc))
                .collect::<Vec<_>>()
                .join("\n\n"))
        }
        "mcp_install" => {
            let id = input["id"].as_str().ok_or("id required")?;
            let package = input["package"].as_str().ok_or("package required")?;
            let command = input["command"].as_str().unwrap_or("npx");
            let args = input["args"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
            let env = input["env"].as_object().map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect::<BTreeMap<_, _>>()
            }).unwrap_or_default();
            let (name, conf) = crate::catalog::build_dynamic(id, package, command, args, env).map_err(|e| e.to_string())?;
            let warnings = crate::catalog::spawn_warnings(&conf);
            {
                let mut cfg = app.cfg.write().map_err(|e| e.to_string())?;
                cfg.mcp_servers.insert(name.clone(), conf);
                cfg.save(&app.cfg_path).map_err(|e| e.to_string())?;
            }
            app.mcp.update_servers(app.mcp_servers_map());
            app.mcp.reconcile().await;
            let mut msg = format!("installed '{name}' and saved to config; use mcp enable/disable or config_set mcp.{name}.enabled to toggle");
            for w in warnings {
                msg.push_str(&format!("\nwarning: {w}"));
            }
            Ok(msg)
        }
        _ => Err(format!("unknown builtin tool '{name}'")),
    }
}

/// Returns (program, flag, wrapped command). On Windows, force UTF-8 output —
/// cmd.exe otherwise answers in the OEM codepage (cp932 on ja-JP), which
/// shows up as mojibake both in tool results and in the model's context.
#[cfg(unix)]
fn wrap_shell(command: &str) -> (&'static str, &'static str, String) {
    ("/bin/sh", "-c", command.to_string())
}

#[cfg(windows)]
fn wrap_shell(command: &str) -> (&'static str, &'static str, String) {
    ("cmd", "/C", format!("chcp 65001 >nul & {command}"))
}

#[cfg(windows)]
fn is_unc(p: &Path) -> bool {
    p.to_string_lossy().starts_with("\\\\")
}

#[cfg(not(windows))]
fn is_unc(_p: &Path) -> bool {
    false
}

/// Run a shell command, capturing stdout+stderr. stdio MUST be piped
/// explicitly: `wait_with_output` only captures what was piped at spawn —
/// the spawn default (inherit) silently returns empty output.
async fn run_shell(command: &str, workdir: Option<&str>, timeout: u64) -> Result<String, String> {
    let (prog, flag, command) = wrap_shell(command);
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args([flag, &command])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::catalog::no_window_async(&mut cmd);
    let wd = workdir
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    // cmd.exe cannot start in a UNC dir, so fall back to home
    let mut note = String::new();
    let wd = wd.map(|p| {
        if is_unc(&p) {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            note = format!(
                "[amty] cwd {} is unusable for cmd.exe; running in {}\n",
                p.display(),
                home.display()
            );
            home
        } else {
            p
        }
    });
    if let Some(wd) = wd {
        cmd.current_dir(wd);
    }
    // kill_on_drop: if the agent run is cancelled mid-tool, dropping
    // this future kills the child instead of leaving it running
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let run = child.wait_with_output();
    match tokio::time::timeout(Duration::from_secs(timeout), run).await {
        Ok(Ok(out)) => {
            let mut s = note;
            s.push_str(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                s.push_str("\n[stderr]\n");
                s.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            if !out.status.success() {
                s.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
            }
            Ok(truncate(s))
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {timeout}s")),
    }
}

// ---------- web_search / web_fetch ----------

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Shared client: no auto-redirects so each hop can be SSRF-checked.
fn http_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::Client::builder()
        .user_agent(concat!("amty/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    Ok(CLIENT.get_or_init(|| c))
}

async fn web_search(conf: &SearchConf, query: &str, count: usize) -> Result<String, String> {
    let results = match conf.backend.as_str() {
        "" | "duckduckgo" => ddg_search(query, count).await?,
        "brave" => brave_search(conf, query, count).await?,
        "tavily" => tavily_search(conf, query, count).await?,
        "searxng" => searxng_search(conf, query, count).await?,
        other => return Err(format!("unknown search backend '{other}'")),
    };
    if results.is_empty() {
        return Ok("no results".into());
    }
    Ok(results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// DuckDuckGo HTML endpoint — no API key, but rate-limits heavy clients.
/// Falls back to the lite endpoint (different markup) when the html
/// endpoint is unreachable, blocked, or returns an unparseable page.
async fn ddg_search(query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let html_err = match http_client()?
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query)])
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.map_err(|e| e.to_string())?;
            let mut out = parse_ddg(&body);
            out.truncate(count);
            if !out.is_empty() {
                return Ok(out);
            }
            "html endpoint: no results parsed".to_string()
        }
        Ok(r) => format!("html endpoint returned {}", r.status()),
        Err(e) => format!("html endpoint: {e}"),
    };
    let resp = http_client()?
        .get(format!("https://lite.duckduckgo.com/lite/?q={}", url_encode(query)))
        .send()
        .await
        .map_err(|e| format!("{html_err}; lite fallback: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{html_err}; lite returned {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let mut out = parse_ddg_lite(&body);
    out.truncate(count);
    if out.is_empty() {
        return Err(format!("{html_err}; lite parsed no results — possibly rate limited; set search.backend=brave"));
    }
    Ok(out)
}

async fn brave_search(conf: &SearchConf, query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let key = conf.resolved_key().ok_or("brave needs an API key: set search.api_key or BRAVE_API_KEY")?;
    let url = format!("https://api.search.brave.com/res/v1/web/search?q={}&count={count}", url_encode(query));
    let resp = http_client()?
        .get(url)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("brave returned {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v.pointer("/web/results")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["description"].as_str().unwrap_or("")),
        })
        .collect())
}

async fn tavily_search(conf: &SearchConf, query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let key = conf.resolved_key().ok_or("tavily needs an API key: set search.api_key or TAVILY_API_KEY")?;
    let resp = http_client()?
        .post("https://api.tavily.com/search")
        .json(&json!({"api_key": key, "query": query, "max_results": count, "search_depth": "basic"}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("tavily returned {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["content"].as_str().unwrap_or("")),
        })
        .collect())
}

/// SearXNG is self-hosted, so its base_url is trusted and exempt from the
/// SSRF check (a local instance is the point of this backend).
async fn searxng_search(conf: &SearchConf, query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let base = conf.base_url.as_deref().ok_or("searxng needs search.base_url (e.g. http://localhost:8080)")?;
    let url = format!("{}/search?q={}&format=json", base.trim_end_matches('/'), url_encode(query));
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("searxng returned {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["content"].as_str().unwrap_or("")),
        })
        .collect())
}

const FETCH_BODY_CAP: usize = 8 * 1024 * 1024;

/// Fetch `url`, following up to 5 redirects with an SSRF check on each hop.
async fn web_fetch(url: &str, max_chars: usize) -> Result<String, String> {
    let mut url = reqwest::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    for _ in 0..5 {
        check_url(&url).await?;
        let resp = http_client()?.get(url.clone()).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without location header")?;
            url = url.join(loc).map_err(|e| e.to_string())?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if resp.content_length().map_or(false, |l| l > FETCH_BODY_CAP as u64) {
            return Err(format!("body too large (>{} bytes)", FETCH_BODY_CAP));
        }
        let body = read_body_limited(resp, FETCH_BODY_CAP).await?;
        let mut text = if ctype.contains("html") || ctype.is_empty() {
            html_to_text(&body)
        } else {
            body
        };
        if text.len() > max_chars {
            let mut end = max_chars;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("…[truncated]");
        }
        return Ok(format!("{url}\n\n{text}"));
    }
    Err("too many redirects".into())
}

async fn read_body_limited(resp: reqwest::Response, cap: usize) -> Result<String, String> {
    let mut resp = resp;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len() + chunk.len() > cap {
            return Err(format!("body exceeds {cap} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Reject non-http(s) schemes and hosts resolving to loopback/private/
/// link-local addresses so web_fetch can't reach internal services.
async fn check_url(url: &reqwest::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme '{}'", url.scheme()));
    }
    let host = url.host_str().ok_or("url has no host")?;
    let h = host.trim_end_matches('.').trim_matches(['[', ']']).to_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") || h.ends_with(".internal") {
        return Err("refusing to fetch local host name".into());
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        check_ip(ip)?;
    } else {
        let port = url.port_or_known_default().unwrap_or(443);
        // resolver can stall for minutes on a dead network — bound it
        let addrs = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((h.as_str(), port)),
        )
        .await
        .map_err(|_| "dns lookup timed out".to_string())?
        .map_err(|e| format!("dns lookup failed: {e}"))?;
        for a in addrs {
            check_ip(a.ip())?;
        }
    }
    Ok(())
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    const ERR: &str = "refusing to fetch private/loopback address";
    let v4 = match ip {
        IpAddr::V4(v) => v,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4,
            None => {
                let s0 = v6.segments()[0];
                if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast()
                    || (s0 & 0xffc0) == 0xfe80  // link-local fe80::/10
                    || (s0 & 0xfe00) == 0xfc00  // unique-local fc00::/7
                {
                    return Err(ERR.into());
                }
                return Ok(());
            }
        },
    };
    let o = v4.octets();
    if v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        || v4.is_multicast() || v4.is_broadcast()
        || o[0] == 0 || o[0] >= 240                       // this-net / reserved
        || (o[0] == 100 && (o[1] & 0xc0) == 64)           // CGNAT 100.64/10
        || (o[0] == 198 && (o[1] & 0xfe) == 18)           // benchmarking 198.18/15
    {
        return Err(ERR.into());
    }
    Ok(())
}

// ---------- HTML → text / parsing helpers (pure, unit-tested) ----------

/// Crude HTML → text: drops script/style/etc blocks, turns block-level tags
/// into newlines, strips the rest, unescapes entities, collapses whitespace.
fn html_to_text(html: &str) -> String {
    const SKIP: &[&str] = &["script", "style", "noscript", "svg", "head", "iframe", "template", "select"];
    const BLOCK: &[&str] = &[
        "p", "div", "br", "hr", "li", "ul", "ol", "tr", "td", "th", "table", "section",
        "article", "header", "footer", "nav", "main", "aside", "blockquote", "pre",
        "h1", "h2", "h3", "h4", "h5", "h6", "form", "fieldset", "figure", "figcaption",
        "dl", "dt", "dd", "title",
    ];
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            let end = html[i..].find('<').map(|e| i + e).unwrap_or(b.len());
            out.push_str(&html[i..end]);
            i = end;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i += html[i + 4..].find("-->").map(|e| e + 7).unwrap_or(b.len() - i);
            continue;
        }
        let mut j = i + 1;
        let closing = j < b.len() && b[j] == b'/';
        if closing {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
            j += 1;
        }
        let name = html[name_start..j.min(b.len())].to_ascii_lowercase();
        let gt = html[j..].find('>').map(|e| j + e + 1).unwrap_or(b.len());
        if !closing && SKIP.contains(&name.as_str()) {
            // skip to the matching close tag (or EOF if absent)
            let close = format!("</{name}");
            match html[gt..].to_ascii_lowercase().find(&close) {
                Some(e) => i = gt + e,
                None => break,
            }
            continue;
        }
        if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        }
        i = gt;
    }
    collapse_ws(&unescape(&out))
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// unescape + strip tags + squeeze inner whitespace — for API/HTML fields.
fn clean(s: &str) -> String {
    unescape(&strip_tags(s)).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let ch = tail.find(';').filter(|&e| e >= 2 && e <= 12).and_then(|e| {
            let ent = &tail[1..e];
            let c = match ent {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" | "QUOT" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                "mdash" => '—',
                "ndash" => '–',
                "hellip" => '…',
                "copy" => '©',
                "reg" => '®',
                "trade" => '™',
                "middot" => '·',
                "bull" => '•',
                "rarr" | "arr" => '→',
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)?
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32)?,
                _ => return None,
            };
            Some((c, e))
        });
        match ch {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = false;
    for line in s.lines() {
        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            if blank {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&t);
    }
    out.trim().to_string()
}

/// Parse html.duckduckgo.com output: `result__a` anchors (organic results are
/// direct links, older markup uses `uddg=` redirect params) paired with
/// `result__snippet` anchors in order. Ads share the `result__a` class but
/// route through `/y.js` — dropped while keeping title/snippet indices aligned.
fn parse_ddg(html: &str) -> Vec<SearchResult> {
    let titles = anchors_with_class(html, "result__a\"");
    let snippets: Vec<(usize, String)> = anchors_with_class(html, "result__snippet\"")
        .into_iter()
        .map(|(p, _, t)| (p, t))
        .collect();
    pair_results(&titles, &snippets)
}

/// lite.duckduckgo.com markup: `result-link` anchors (single-quoted class)
/// paired with `result-snippet` <td>s.
fn parse_ddg_lite(html: &str) -> Vec<SearchResult> {
    let titles = anchors_with_class(html, "result-link'");
    let snippets = tds_with_class(html, "result-snippet'");
    pair_results(&titles, &snippets)
}

fn pair_results(titles: &[(usize, String, String)], snippets: &[(usize, String)]) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for (i, (pos, href, title)) in titles.iter().enumerate() {
        if is_ddg_ad(href) {
            continue;
        }
        // the snippet between this title and the next (if any)
        let next = titles.get(i + 1).map(|(p, ..)| *p).unwrap_or(usize::MAX);
        let snippet = snippets
            .iter()
            .find(|(p, ..)| p > pos && *p < next)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        out.push(SearchResult {
            title: title.clone(),
            url: decode_ddg_url(href),
            snippet,
        });
    }
    out
}

/// `<td ... class*marker*>text</td>` cells → (byte pos, cleaned text).
fn tds_with_class(html: &str, marker: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = html[pos..].find(marker) {
        let i = pos + i;
        let Some(gt) = html[i..].find('>') else { break };
        let rest = &html[i + gt + 1..];
        let end = rest.find("</td>").unwrap_or(rest.len());
        out.push((i, clean(&rest[..end])));
        pos = i + gt + 1 + end;
    }
    out
}

fn is_ddg_ad(href: &str) -> bool {
    href.contains("/y.js") || href.contains("ad_domain=") || href.contains("ad_provider=")
}

/// All `<a ... class*marker* ...>text</a>` anchors → (byte pos, href, cleaned text).
/// The marker sits mid-tag, so scan back to the '<' — attribute order
/// differs between endpoints (html: class then href; lite: href then class).
fn anchors_with_class(html: &str, marker: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = html[pos..].find(marker) {
        let i = pos + i;
        let Some(gt) = html[i..].find('>') else { break };
        let start = html[..i].rfind('<').unwrap_or(i);
        let tag = &html[start..i + gt];
        let rest = &html[i + gt + 1..];
        let end = rest.find("</a>").unwrap_or(rest.len());
        let href = attr(tag, "href").unwrap_or_default();
        out.push((i, href, clean(&rest[..end])));
        pos = i + gt + 1 + end;
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    for q in ['"', '\''] {
        let pat = format!("{name}={q}");
        if let Some(i) = tag.find(&pat) {
            let rest = &tag[i + pat.len()..];
            if let Some(e) = rest.find(q) {
                return Some(rest[..e].to_string());
            }
        }
    }
    None
}

/// `//duckduckgo.com/l/?uddg=<pct-encoded>&rut=...` → real URL.
fn decode_ddg_url(href: &str) -> String {
    if let Some(i) = href.find("uddg=") {
        let v = &href[i + 5..];
        let v = v.split('&').next().unwrap_or(v);
        if let Some(u) = percent_decode(v) {
            return u;
        }
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    href.to_string()
}

/// Percent-encode for query-string values (unreserved chars kept verbatim).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(hex(b[i + 1])? << 4 | hex(b[i + 2])?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: ApprovalMode) -> Config {
        Config { approval: mode, ..Config::default() }
    }

    #[test]
    fn approval_modes() {
        let ask = cfg(ApprovalMode::Ask);
        assert!(needs_approval(&ask, "shell", &json!({"command": "ls"})));
        assert!(needs_approval(&ask, "fs_write", &json!({"path": "/tmp/x"})));
        assert!(!needs_approval(&ask, "fs_read", &json!({"path": "/tmp/x"})));
        assert!(!needs_approval(&ask, "config_get", &json!({})));

        let auto = cfg(ApprovalMode::Auto);
        assert!(!needs_approval(&auto, "shell", &json!({"command": "rm -rf /"})));

        let mut al = cfg(ApprovalMode::Allowlist);
        al.allow_commands = vec!["ls".into()];
        assert!(!needs_approval(&al, "shell", &json!({"command": "ls -la"})));
        assert!(needs_approval(&al, "shell", &json!({"command": "rm x"})));
        assert!(needs_approval(&al, "shell", &json!({"command": "lsblk"}))); // prefix boundary
    }

    /// Regression: without Stdio::piped(), wait_with_output returns empty
    /// output on every platform — and on Windows the child's stdio lands on
    /// the hidden CREATE_NO_WINDOW console buffer instead.
    #[tokio::test]
    async fn shell_captures_stdout_and_stderr() {
        #[cfg(unix)]
        let cmd = "echo aaa-out; echo bbb-err 1>&2";
        #[cfg(windows)]
        let cmd = "echo aaa-out & echo bbb-err 1>&2";
        let out = run_shell(cmd, None, 30).await.unwrap();
        assert!(out.contains("aaa-out"), "stdout missing: {out:?}");
        assert!(out.contains("bbb-err"), "stderr missing: {out:?}");
    }

    #[test]
    fn summarize_includes_target() {
        assert!(summarize("shell", &json!({"command": "make"})).contains("make"));
        assert!(summarize("fs_write", &json!({"path": "/a"})).contains("/a"));
        assert!(summarize("web_search", &json!({"query": "q"})).contains("q"));
    }

    #[test]
    fn web_tools_are_safe() {
        let ask = cfg(ApprovalMode::Ask);
        assert!(!needs_approval(&ask, "web_search", &json!({"query": "x"})));
        assert!(!needs_approval(&ask, "web_fetch", &json!({"url": "https://ex.com"})));
    }

    #[test]
    fn html_to_text_strips_scripts_and_blocks() {
        let html = "<html><head><title>t</title><style>a{color:red}</style></head>\
            <body><script>track()</script><!-- hidden --><h1>Hello</h1>\
            <p>a <b>bold</b> &amp; co</p><ul><li>one</li><li>two</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(!text.contains("track"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("hidden"));
        assert!(text.contains("Hello"));
        assert!(text.contains("a bold & co"));
        assert!(text.contains("one") && text.contains("two"));
    }

    #[test]
    fn unescape_entities() {
        assert_eq!(unescape("a &amp; b &lt;x&gt; &#65; &#x42;"), "a & b <x> A B");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("bad &bogus; ok"), "bad &bogus; ok");
        assert_eq!(unescape("tail &"), "tail &");
    }

    #[test]
    fn parse_ddg_extracts_results() {
        let html = r#"
            <div class="result--ad"><a class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x">Ad</a>
            <a class="result__snippet">Ad snip</a></div>
            <div class="result"><h2 class="result__title">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example &lt;Title&gt;</a>
            </h2><a class="result__snippet" href="x">A <b>snippet</b> here.</a></div>
            <div class="result"><h2><a class="result__a" href="https://direct.example/x">Second</a></h2>
            <a class="result__snippet">Second snip</a></div>"#;
        let r = parse_ddg(html);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Example <Title>");
        assert_eq!(r[0].url, "https://example.com/page");
        assert_eq!(r[0].snippet, "A snippet here.");
        assert_eq!(r[1].url, "https://direct.example/x");
    }

    #[test]
    fn parse_ddg_lite_extracts_results() {
        let html = r#"
            <td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc" class='result-link'>Example &lt;Title&gt;</a></td>
            <td class='result-snippet'>A <b>snippet</b> here.</td>
            <td><a rel="nofollow" href="https://direct.example/x" class='result-link'>Second</a></td>
            <td class='result-snippet'>Second snip</td>"#;
        let r = parse_ddg_lite(html);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Example <Title>");
        assert_eq!(r[0].url, "https://example.com/page");
        assert_eq!(r[0].snippet, "A snippet here.");
        assert_eq!(r[1].url, "https://direct.example/x");
        assert_eq!(r[1].snippet, "Second snip");
    }

    #[test]
    fn private_ips_rejected() {
        for ip in [
            "127.0.0.1", "10.0.0.1", "172.16.5.4", "192.168.1.1", "169.254.1.1",
            "0.0.0.0", "100.64.0.1", "198.18.0.1", "::1", "fe80::1", "fc00::1", "::ffff:127.0.0.1",
        ] {
            assert!(check_ip(ip.parse().unwrap()).is_err(), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(check_ip(ip.parse().unwrap()).is_ok(), "{ip} should pass");
        }
    }

    #[test]
    fn percent_decode_works() {
        assert_eq!(percent_decode("https%3A%2F%2Fex.com%2Fa%20b").unwrap(), "https://ex.com/a b");
        assert_eq!(percent_decode("a+b").unwrap(), "a b");
        assert_eq!(percent_decode("%a").unwrap(), "%a");
        assert!(percent_decode("%zz").is_none());
    }
}

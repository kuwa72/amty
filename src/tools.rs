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
        "fs_read" | "fs_list" | "config_get" | "mcp" | "web_search" | "web_fetch"
        | "self_update" | "note_list" | "note_read" | "note_search" | "job_list" => Risk::Safe,
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
            description: "Read an amty setting. Keys: provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, search, evolve, provider.<name>.<field>, search.<field>, evolve.<field>.".into(),
            schema: schema(json!({"key": {"type": "string", "description": "omit to list keys"}}), &[]),
        },
        ToolSpec {
            name: "config_set".into(),
            description: "Change an amty setting at runtime (persisted). Keys: provider, model, approval(ask|auto|allowlist), system_prompt, max_tokens, allow_commands, allow_paths, +allow_commands, +allow_paths, provider.<name>.{model,api_key,api_key_env,base_url,context_window}, mcp.<name>.enabled, search.{backend,api_key,api_key_env,base_url,max_results,fetch_max_chars}, evolve.{reflect,skills,self_modify,auto_restart,learn_approvals}.".into(),
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
            name: "note_list".into(),
            description: "List notes in the markdown knowledge store (name, title, size, mtime). Note names are '/'-separated paths without .md, e.g. 'projects/amty/decisions'.".into(),
            schema: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "note_read".into(),
            description: "Read a note's markdown content.".into(),
            schema: schema(json!({"name": {"type": "string"}}), &["name"]),
        },
        ToolSpec {
            name: "note_write".into(),
            description: "Create or overwrite a note (full content replace). Writes only inside the notes dir; nested paths create directories.".into(),
            schema: schema(
                json!({"name": {"type": "string"}, "content": {"type": "string"}}),
                &["name", "content"],
            ),
        },
        ToolSpec {
            name: "note_delete".into(),
            description: "Delete a note permanently.".into(),
            schema: schema(json!({"name": {"type": "string"}}), &["name"]),
        },
        ToolSpec {
            name: "note_search".into(),
            description: "Full-text search over note names and contents (case-insensitive substring). Returns matching notes with a one-line excerpt.".into(),
            schema: schema(
                json!({
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "description": "max hits (default 20)"}
                }),
                &["query"],
            ),
        },
        ToolSpec {
            name: "job_list".into(),
            description: "List scheduled jobs (id, name, spec, enabled, next run, last status).".into(),
            schema: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "job_add".into(),
            description: "Schedule a prompt to run on a timer — use this to schedule yourself (e.g. 'check X every hour'). spec: 'every 30m|6h|1d' or 'at HH:MM' (local time). Empty session = a fresh [job] session each fire.".into(),
            schema: schema(
                json!({
                    "name": {"type": "string", "description": "job name (defaults to prompt prefix)"},
                    "spec": {"type": "string", "description": "'every 30m' or 'at 09:00'"},
                    "prompt": {"type": "string", "description": "the message amty sends itself when the job fires"},
                    "session": {"type": "string", "description": "reuse this session id instead of a fresh one"}
                }),
                &["spec", "prompt"],
            ),
        },
        ToolSpec {
            name: "job_remove".into(),
            description: "Delete a scheduled job (id, unique prefix, or name).".into(),
            schema: schema(json!({"id": {"type": "string"}}), &["id"]),
        },
        ToolSpec {
            name: "job_run".into(),
            description: "Fire a scheduled job immediately, regardless of its schedule.".into(),
            schema: schema(json!({"id": {"type": "string"}}), &["id"]),
        },
        ToolSpec {
            name: "job_set".into(),
            description: "Enable or disable a scheduled job without deleting it.".into(),
            schema: schema(
                json!({"id": {"type": "string"}, "enabled": {"type": "boolean"}}),
                &["id", "enabled"],
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
            name: "self_update".into(),
            description: "Self-evolution control. actions: status (memory/stats/build state) | reflect (force a memory-distillation pass on a session) | rebuild (cargo build amty from its own source tree, detached; on success the new binary is installed over the running exe and the app restarts when idle — a detached helper process does the swap, so it works even though a running exe can't be overwritten) | install (take an ALREADY BUILT binary — `path` — after checking it runs, and make it the next version; use this when rebuild can't work, e.g. a cross-built Windows exe on a machine without cargo) | restart (schedule that restart manually) | cancel (drop a scheduled swap/restart before it fires) | forget (erase a learned auto-approval; pass the exact command in `command`). Rebuild needs the checkout this binary was built from; install only needs the artifact.".into(),
            schema: schema(
                json!({
                    "action": {"type": "string"},
                    "session": {"type": "string", "description": "reflect action: session id or prefix; empty = most recent"},
                    "command": {"type": "string", "description": "forget action: exact command to unlearn"},
                    "path": {"type": "string", "description": "install action: absolute path of the built binary to install (e.g. target/x86_64-pc-windows-gnu/release/amty.exe)"}
                }),
                &["action"],
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
        "self_update" => format!("self_update: {}", input["action"].as_str().unwrap_or("status")),
        n if n.starts_with("note_") => {
            let key = n.strip_prefix("note_").unwrap_or(n);
            format!("{n}: {}", input["name"].as_str().or(input["query"].as_str()).unwrap_or(key))
        }
        n if n.starts_with("job_") => {
            format!("{n}: {}", input["name"].as_str().or(input["id"].as_str()).or(input["spec"].as_str()).unwrap_or(""))
        }
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
    // `self_update` read actions (status, reflect, cancel) are safe; rebuild/
    // install/restart count as Exec — they spawn a build or replace the exe.
    let mutating_su = name == "self_update"
        && !matches!(input["action"].as_str().unwrap_or("status"), "status" | "reflect" | "cancel");
    let risk = if mutating_mcp {
        Risk::Write
    } else if mutating_su {
        Risk::Exec
    } else {
        risk_of(name)
    };
    match risk {
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
        // note writes/deletes only ever touch the notes dir — an app-owned
        // sandbox — so there's no path for a user allowlist to match against.
        "note_write" | "note_delete" => true,
        // job calls only register/remove/fire prompts; the prompts' own tool
        // calls still go through normal approval when they run.
        "job_add" | "job_remove" | "job_run" | "job_set" => true,
        _ => false,
    }
}

/// Default ceiling for any single tool result that enters the model's context.
///
/// Builtin tools go through [`truncate`]; MCP results go through
/// [`crate::mcp::cap_result`], which uses this same constant so the two paths
/// behave identically (see issue #6: an uncapped MCP `Snapshot` put 107,816
/// chars / ~27k tokens into one turn).
///
/// Lowered 48 KB → 16 KB after measuring 182 sessions: of 4,947 tool results,
/// 222 (4%) exceeded 8 KB and those carried 73% of all result bytes (`fs_read`
/// alone was 4.9 MB of 7.2 MB). A result is re-sent on every later turn, so one
/// 40 KB read bills 40 KB × remaining turns. 16 KB still fits a 400-line source
/// file, and the re-read total only drops ~1% versus 48 KB (measured), so this
/// trims the tail without forcing the model to re-read in slices.
pub const MAX_OUT: usize = 16_000;

/// Clip `s` to `max` bytes without splitting a UTF-8 character, appending a
/// marker that tells the model (and the reader) how much was dropped.
pub fn truncate_to(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
}

fn truncate(s: String) -> String {
    truncate_to(s, MAX_OUT)
}

pub async fn execute(app: &std::sync::Arc<App>, name: &str, input: &Value) -> Result<String, String> {
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
                None => Ok("provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, search, evolve, provider.<name>.{model,base_url,api_key_env,api_key}, search.{backend,api_key,api_key_env,base_url,max_results,fetch_max_chars}, evolve.{reflect,skills,self_modify,auto_restart,learn_approvals}".into()),
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
        "self_update" => {
            let action = input["action"].as_str().unwrap_or("status");
            match action {
                "status" => Ok(crate::evolve::status_text(app)),
                "reflect" => {
                    let session = input["session"].as_str().unwrap_or("");
                    crate::evolve::reflect_now(app, session).await
                }
                "rebuild" => crate::evolve::start_rebuild(app),
                "install" => {
                    let p = input["path"]
                        .as_str()
                        .ok_or("install requires `path` (the built binary to install)")?;
                    crate::evolve::install_artifact(app, &PathBuf::from(p))
                }
                "restart" => {
                    let target = app
                        .built_binary
                        .lock()
                        .unwrap()
                        .clone()
                        .or_else(|| std::env::current_exe().ok())
                        .ok_or_else(|| "no binary to restart with".to_string())?;
                    crate::evolve::request_restart(app, target.clone());
                    Ok(format!("restart scheduled into {} — fires when all runs are idle (including this one when it ends)", target.display()))
                }
                "cancel" => Ok(match crate::evolve::cancel_restart(app) {
                    Some(p) => format!("cancelled the pending swap to {}", p.display()),
                    None => "no restart pending".into(),
                }),
                "forget" => {
                    let cmd = input["command"].as_str().ok_or("command required")?;
                    Ok(if crate::evolve::forget_command(app, cmd) {
                        format!("forgot learned approval: {cmd}")
                    } else {
                        format!("not learned: {cmd}")
                    })
                }
                _ => Err(format!("unknown action '{action}' (status|reflect|rebuild|install|restart|cancel|forget)")),
            }
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
        "note_list" | "note_read" | "note_write" | "note_delete" | "note_search" => {
            let dir = {
                let cfg = app.cfg.read().map_err(|e| e.to_string())?;
                crate::notes::dir(&cfg)
            };
            match name {
                "note_list" => {
                    let metas = crate::notes::list(&dir);
                    let mut out = format!("# notes dir: {}\n", dir.display());
                    if metas.is_empty() {
                        out.push_str("(no notes yet)");
                    }
                    for m in metas {
                        out.push_str(&format!(
                            "{}\t{} B\t{}\t{}\n",
                            m.name,
                            m.bytes,
                            crate::session::fmt_stamp(m.updated),
                            m.title,
                        ));
                    }
                    Ok(out.trim_end().to_string())
                }
                "note_read" => {
                    let n = input["name"].as_str().ok_or("name required")?;
                    crate::notes::read(&dir, n).map(truncate)
                }
                "note_write" => {
                    let n = input["name"].as_str().ok_or("name required")?;
                    let content = input["content"].as_str().unwrap_or("");
                    let bytes = crate::notes::write(&dir, n, content)?;
                    app.emit("", crate::types::EvKind::Touched);
                    Ok(format!("wrote {bytes} bytes to note '{n}'"))
                }
                "note_delete" => {
                    let n = input["name"].as_str().ok_or("name required")?;
                    crate::notes::delete(&dir, n)?;
                    app.emit("", crate::types::EvKind::Touched);
                    Ok(format!("deleted note '{n}'"))
                }
                _ => {
                    let q = input["query"].as_str().ok_or("query required")?;
                    let limit = input["limit"].as_u64().unwrap_or(20).clamp(1, 200) as usize;
                    let hits = crate::notes::search(&dir, q, limit);
                    if hits.is_empty() {
                        return Ok("no matching notes".into());
                    }
                    Ok(hits
                        .iter()
                        .map(|h| format!("{}\t{}\t{}", h.name, h.title, h.snippet))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }
        }
        "job_list" => {
            let jobs = app.jobs.lock().map_err(|e| e.to_string())?;
            if jobs.is_empty() {
                return Ok("(no jobs)".into());
            }
            Ok(jobs
                .iter()
                .map(|j| {
                    format!(
                        "{}\t{}\t{}\tnext={}\tlast={}\t{}",
                        j.id,
                        j.name,
                        j.spec,
                        crate::session::fmt_stamp(j.next_run),
                        if j.last_status.is_empty() { "-" } else { &j.last_status },
                        if j.enabled { "on" } else { "off" },
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "job_add" => {
            let spec = input["spec"].as_str().ok_or("spec required")?;
            let prompt = input["prompt"].as_str().ok_or("prompt required")?;
            let job = crate::jobs::add(
                app,
                input["name"].as_str().unwrap_or(""),
                spec,
                prompt,
                input["session"].as_str().unwrap_or(""),
            )?;
            Ok(format!("job {} '{}' scheduled: {} (next {})", job.id, job.name, job.spec, crate::session::fmt_stamp(job.next_run)))
        }
        "job_remove" => {
            let id = input["id"].as_str().ok_or("id required")?;
            crate::jobs::remove(app, id)?;
            Ok(format!("removed job '{id}'"))
        }
        "job_run" => {
            let id = input["id"].as_str().ok_or("id required")?;
            let st = crate::jobs::run_now(app, id)?;
            Ok(format!("job '{id}': {st}"))
        }
        "job_set" => {
            let id = input["id"].as_str().ok_or("id required")?;
            let on = input["enabled"].as_bool().ok_or("enabled: bool required")?;
            crate::jobs::set_enabled(app, id, on)?;
            Ok(format!("job '{id}' {}", if on { "enabled" } else { "disabled" }))
        }
        _ => Err(format!("unknown builtin tool '{name}'")),
    }
}

/// Returns (program, flag, wrapped command). On Windows, force UTF-8 output —
/// cmd.exe otherwise answers in the OEM codepage (cp932 on ja-JP), which
/// shows up as mojibake both in tool results and in the model's context.
///
/// DO NOT change "/C" to "/S /C" here without reproducing the *Rust* argv path.
/// A `/S /C` attempt (2026-10) broke every command: the shell died with
/// `'"chcp 65001 >nul & …' is not recognized as an internal or external
/// command`, i.e. the whole wrapped string was taken as one command name.
/// The earlier "verification" was invalid because it passed a verbatim command
/// *string* to Python (no argv quoting), while `Command::args` quotes `/S /C`
/// itself (it contains a space) and escapes the inner quotes as `\"`.
/// Reproduce with the LIST form instead:
///     subprocess.run(["cmd", "/S", "/C", "chcp 65001 >nul & dir /b <path>"])
/// The quote-mangling limitation described above was fixed by routing Windows
/// commands through a temp `.cmd` file instead — see `run_shell_via_cmd_file`.
/// The Windows `wrap_shell` below is now only exercised by tests; production
/// code no longer passes a wrapped command string to `cmd /C`.
#[cfg(unix)]
fn wrap_shell(command: &str) -> (&'static str, &'static str, String) {
    ("/bin/sh", "-c", command.to_string())
}

#[cfg(all(windows, test))]
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
    // Windows: `cmd /C "<command>"` cannot carry a command that itself contains
    // quotes. Rust escapes the inner quotes when building the cmd.exe argv, and
    // cmd.exe does not un-escape them the way a POSIX shell would, so the child
    // receives mangled arguments. Measured on this machine (5 shapes, all quoted):
    //   dir /b "C:\Program Files"        -> rc=1 "The filename, directory name,
    //                                            or volume label syntax is incorrect."
    //   wsl.exe ... bash -c "echo x"     -> rc=2 "unexpected EOF while looking
    //                                            for matching `\"'"
    //   git -C "\\wsl.localhost\..."     -> rc=128 "cannot change to '\"\\\\wsl...'"
    // 98% of the shell commands in the local sessions contain a quote, which is
    // why 573 hand-written scripts exist: the model gave up and wrote a file.
    // Fix: land the command in a temp .cmd (UTF-8 + `chcp 65001`, the only
    // encoding that round-trips Japanese *and* quotes — verified) and run that.
    #[cfg(windows)]
    {
        run_shell_via_cmd_file(command, workdir, timeout).await
    }
    #[cfg(not(windows))]
    {
        let (prog, flag, command) = wrap_shell(command);
        run_shell_spawn(prog, flag, &command, workdir, timeout).await
    }
}

/// Windows path: write `command` to a temp `.cmd` and execute that file.
///
/// The file is written by amty (not by the model), so the shell tool keeps
/// working exactly as documented while the quoting problem disappears. The
/// UTF-8 body is decoded correctly because `chcp 65001` is the first command in
/// the file; a BOM would be echoed as garbage by cmd.exe, so it is not written.
#[cfg(windows)]
async fn run_shell_via_cmd_file(
    command: &str,
    workdir: Option<&str>,
    timeout: u64,
) -> Result<String, String> {
    use std::sync::atomic::{AtomicU64, Ordering};

    let dir = std::env::temp_dir().join("amty-sh");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // unique per call: two concurrent shell calls must not collide
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("sh-{}-{}.cmd", std::process::id(), n));

    let mut body = String::from("@echo off\r\nchcp 65001 >nul\r\n");
    body.push_str(command);
    body.push_str("\r\n");
    std::fs::write(&path, body.as_bytes()).map_err(|e| e.to_string())?;

    let path_arg = path.to_string_lossy().to_string();
    let res = run_shell_spawn("cmd", "/C", &path_arg, workdir, timeout).await;

    let _ = std::fs::remove_file(&path);
    res
}

/// Spawn `prog flag arg`, capture stdio, apply the UNC cwd fallback.
async fn run_shell_spawn(
    prog: &str,
    flag: &str,
    arg: &str,
    workdir: Option<&str>,
    timeout: u64,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args([flag, arg])
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
    let (results, note) = match conf.backend.as_str() {
        "" | "duckduckgo" => ddg_search(query, count).await?,
        "brave" => brave_search(conf, query, count).await?,
        "tavily" => tavily_search(conf, query, count).await?,
        "searxng" => searxng_search(conf, query, count).await?,
        "bing" => (bing_search(query, count).await?, String::new()),
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
        .join("\n\n")
        + &note)
}

/// DuckDuckGo HTML endpoint — no API key, but rate-limits heavy clients.
/// Falls back to the lite endpoint (different markup) when the html
/// endpoint is unreachable, blocked, or returns an unparseable page.
async fn ddg_search(query: &str, count: usize) -> Result<(Vec<SearchResult>, String), String> {
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
                return Ok((out, String::new()));
            }
            "html endpoint: no results parsed".to_string()
        }
        Ok(r) => format!("html endpoint returned {}", r.status()),
        Err(e) => format!("html endpoint: {e}"),
    };
    // lite is a different host/path, so it is still worth one shot: the html
    // endpoint can be blocked while lite works.
    let lite = async {
        let resp = http_client()?
            .get(format!("https://lite.duckduckgo.com/lite/?q={}", url_encode(query)))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("lite returned {}", resp.status()));
        }
        let body = resp.text().await.map_err(|e| e.to_string())?;
        let mut out = parse_ddg_lite(&body);
        out.truncate(count);
        if out.is_empty() {
            return Err("lite parsed no results".to_string());
        }
        Ok(out)
    };
    match lite.await {
        Ok(out) => Ok((out, String::new())),
        Err(lite_err) => {
            // Both DDG endpoints are down or rate-limiting. Measured on this
            // machine: 8 rapid searches succeed, then every call returns HTTP
            // 202 with an empty body for at least 20 minutes — retrying DDG
            // costs a round trip and never recovers in-session, so switch
            // engines instead of failing the tool.
            let out = bing_search(query, count).await.map_err(|bing_err| {
                format!(
                    "{html_err}; {lite_err}; bing fallback: {bing_err}. \
                     Search is rate limited — set search.backend to brave/tavily \
                     with an API key, or retry later."
                )
            })?;
            Ok((
                out,
                format!(
                    "\n\n(note: search.backend=duckduckgo was rate limited; \
                     these results come from Bing fallback)"
                ),
            ))
        }
    }
}

/// Bing HTML search — keyless fallback used only when DuckDuckGo blocks us.
///
/// Results carry an obfuscated redirect (`/ck/a?…&u=a1<base64url>`); the `u=`
/// parameter decodes to the real URL, and when it is absent (some results) the
/// `<cite>` element shows the destination in readable form.
async fn bing_search(query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let url = format!(
        "https://www.bing.com/search?q={}&count={count}&setlang=en&cc=us&mkt=en-us",
        url_encode(query)
    );
    let resp = http_client()?
        .get(url)
        // Bing returns a Chinese-language shell to this client by default and
        // occasionally no `b_algo` blocks at all; a browser UA + mkt=en-us
        // makes the markup stable (measured: 10/10 relevant results).
        .header("User-Agent", BROWSER_UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("bing returned {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let out = parse_bing(&body);
    if out.is_empty() {
        return Err("bing parsed no results".into());
    }
    Ok(out.into_iter().take(count).collect())
}

/// A browser UA for engines that reject bot clients. DDG serves results to any
/// UA, but Bing returns a localized stub without this (measured).
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// `<li class="b_algo">` blocks → title/url/snippet. The snippet lives in a
/// `<p>` (class varies: `b_lineclamp*`), and the URL is either the decoded
/// `u=a1…` param or the `<cite>` text.
fn parse_bing(html: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for part in html.split("<li class=\"b_algo\"").skip(1) {
        let Some(h) = part.find("<h2") else { continue };
        let head = &part[h..];
        let Some(gt) = head.find('>') else { continue };
        // the <a> inside the <h2>
        let Some(a) = head[gt..].find("<a ") else { continue };
        let anchor = &head[gt + a..];
        let Some(agt) = anchor.find('>') else { continue };
        let tag = &anchor[..agt];
        let Some(href) = attr(tag, "href") else { continue };
        let rest = &anchor[agt + 1..];
        let title = clean(&rest[..rest.find("</a>").unwrap_or(rest.len())]);
        if title.is_empty() {
            continue;
        }
        let snippet = find_tag_text(part, "<p").unwrap_or_default();
        let url = decode_bing_url(&href)
            .or_else(|| find_tag_text(part, "<cite").map(|c| cite_to_url(&c)))
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        out.push(SearchResult { title, url, snippet });
    }
    out
}

/// First `<p…>text</p>` / `<cite…>text</cite>` inside `s`, cleaned.
fn find_tag_text(s: &str, open: &str) -> Option<String> {
    let i = s.find(open)?;
    let rest = &s[i..];
    let gt = rest.find('>')?;
    let body = &rest[gt + 1..];
    let end = body.find("</")?;
    Some(unescape(&strip_tags(&body[..end]).trim().to_string()))
}

/// `<cite>` shows `https://docs.rs › tokio › task` — fold the breadcrumb back
/// into a URL when the redirect param is missing. Bing emits the separator
/// both as a literal `›` and as `&rsaquo;`, and `clean()` leaves entities
/// alone, so strip tags and decode entities before joining.
fn cite_to_url(cite: &str) -> String {
    let text = unescape(&strip_tags(cite));
    let joined = text.replace(" \u{203a} ", "/").replace(' ', "");
    if joined.starts_with("http") {
        joined
    } else {
        format!("https://{joined}")
    }
}

/// `https://www.bing.com/ck/a?…&u=a1<base64url of the target>` → real URL.
fn decode_bing_url(href: &str) -> Option<String> {
    let i = href.find("u=a1")?;
    let v: String = href[i + 4..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let mut b64 = v.to_string();
    while b64.len() % 4 != 0 {
        b64.push('=');
    }
    let raw = base64_decode(&b64)?;
    let s = String::from_utf8_lossy(&raw).to_string();
    s.starts_with("http").then_some(s)
}

/// Minimal base64 decoder accepting both the standard and URL-safe alphabets
/// (`+`/`/` and `-`/`_`), with optional padding. Bing's `u=a1…` parameter is
/// URL-safe, and callers should not have to remember which.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

async fn brave_search(conf: &SearchConf, query: &str, count: usize) -> Result<(Vec<SearchResult>, String), String> {
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
    Ok((v.pointer("/web/results")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["description"].as_str().unwrap_or("")),
        })
        .collect(), String::new()))
}

async fn tavily_search(conf: &SearchConf, query: &str, count: usize) -> Result<(Vec<SearchResult>, String), String> {
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
    Ok((v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["content"].as_str().unwrap_or("")),
        })
        .collect(), String::new()))
}

/// SearXNG is self-hosted, so its base_url is trusted and exempt from the
/// SSRF check (a local instance is the point of this backend).
async fn searxng_search(conf: &SearchConf, query: &str, count: usize) -> Result<(Vec<SearchResult>, String), String> {
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
    Ok((v["results"]
        .as_array()
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| SearchResult {
            title: clean(r["title"].as_str().unwrap_or("")),
            url: r["url"].as_str().unwrap_or("").into(),
            snippet: clean(r["content"].as_str().unwrap_or("")),
        })
        .collect(), String::new()))
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
                "lsaquo" => '‹',
                "rsaquo" => '›',
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
    fn self_update_read_actions_are_safe() {
        let ask = cfg(ApprovalMode::Ask);
        assert!(!needs_approval(&ask, "self_update", &json!({"action": "status"})));
        assert!(!needs_approval(&ask, "self_update", &json!({"action": "reflect"})));
        assert!(!needs_approval(&ask, "self_update", &json!({"action": "cancel"})));
        assert!(needs_approval(&ask, "self_update", &json!({"action": "rebuild"})));
        assert!(needs_approval(&ask, "self_update", &json!({"action": "restart"})));
        // install replaces the running exe — never silently allowed
        assert!(needs_approval(&ask, "self_update", &json!({"action": "install", "path": "/tmp/amty"})));
        let auto = cfg(ApprovalMode::Auto);
        assert!(!needs_approval(&auto, "self_update", &json!({"action": "rebuild"})));
    }

    /// Regression guard for the 2026-10 shell outage: changing the flag to
    /// `/S /C` broke every command, because `Command::args` quotes `/S /C`
    /// itself (it has a space) and escapes the inner quotes as `\"`, so cmd
    /// received one long token and answered `'"chcp 65001 >nul & …' is not
    /// recognized as an internal or external command`.
    ///
    /// So: assert the flag and the command are passed as SEPARATE, unmutated
    /// arguments. Anything cleverer must first be verified on Windows through
    /// the argv path (`subprocess.run(["cmd", ...])`), not with a command
    /// string.
    #[test]
    #[cfg(windows)]
    fn shell_wrapper_passes_flag_and_command_separately() {
        let (prog, flag, wrapped) = wrap_shell(r#"dir /b "C:\Program Files""#);
        assert_eq!(prog, "cmd");
        assert_eq!(flag, "/C", "a bare /C is the verified-working form");
        assert!(flag.split_whitespace().count() == 1, "the flag must be one token");
        assert!(wrapped.starts_with("chcp 65001 >nul & "), "UTF-8 output must be forced");
        // the agent's command must be present verbatim, quotes included
        assert!(wrapped.ends_with(r#"dir /b "C:\Program Files""#));
    }

    /// The unix side must not gain the Windows flag or prefix.
    #[test]
    #[cfg(unix)]
    fn shell_wrapper_is_sh_c_on_unix() {
        let (prog, flag, wrapped) = wrap_shell(r#"ls -la "a b""#);
        assert_eq!((prog, flag), ("/bin/sh", "-c"));
        assert_eq!(wrapped, r#"ls -la "a b""#);
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

    /// The shell tool must survive quotes. `cmd /C "<command>"` mangled every
    /// quoted argument (measured: `dir /b "C:\Program Files"` rc=1,
    /// `wsl.exe ... bash -c "echo x"` rc=2, `git -C "\\wsl.localhost\..."` rc=128),
    /// which is why 98% of the shell commands in the local sessions had to be
    /// wrapped in hand-written scripts. Windows now routes through a temp .cmd.
    #[cfg(windows)]
    #[tokio::test]
    async fn shell_survives_quotes_and_japanese() {
        let out = run_shell(r#"echo "quoted arg" & echo 日本語テスト"#, None, 30)
            .await
            .expect("quoted command should run");
        assert!(out.contains("quoted arg"), "quotes mangled: {out}");
        assert!(out.contains("日本語テスト"), "japanese mangled: {out}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_still_reports_failures_and_multi_commands() {
        // multi-command with & , and an exit code that must survive
        let out = run_shell("echo A & echo B & exit 3", None, 30).await.expect("runs");
        assert!(out.contains("A") && out.contains("B"), "{out}");
        assert!(out.contains("[exit 3]"), "exit code lost: {out}");
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
    fn parse_bing_extracts_results_and_decodes_redirects() {
        // `u=a1<base64url>` where the payload is https://example.com/page
        let html = r#"<ol id="b_results">
            <li class="b_algo" data-id iid=SERP.1><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=abc&amp;u=a1aHR0cHM6Ly9leGFtcGxlLmNvbS9wYWdl&amp;ntb=1">Example &lt;Title&gt;</a></h2>
            <p class="b_lineclamp4">A <strong>snippet</strong> here.</p></li>
            <li class="b_algo"><h2><a href="https://direct.example/x">Second</a></h2>
            <cite>https://direct.example &rsaquo; x</cite></li></ol>"#;
        let r = parse_bing(html);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Example <Title>");
        assert_eq!(r[0].url, "https://example.com/page");
        assert!(r[0].snippet.contains("snippet"));
        // no u= param → fall back to the <cite> breadcrumb
        assert_eq!(r[1].url, "https://direct.example/x");
    }

    #[test]
    fn base64_decode_handles_url_safe_and_padding() {
        // "Hi" -> SGk= ; url-safe alphabet and missing padding must work
        assert_eq!(base64_decode("SGk=").unwrap(), b"Hi");
        assert_eq!(base64_decode("SGk").unwrap(), b"Hi");
        // url-safe chars map back onto the standard alphabet
        assert_eq!(base64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        // invalid input is rejected rather than silently truncated
        assert_eq!(base64_decode("!!"), None);
    }

    #[test]
    fn decode_bing_url_rejects_non_http() {
        assert_eq!(decode_bing_url("https://example.com/x"), None);
        // u=a1(base64 of "/images/search?q=x") — an internal path, not a result
        assert_eq!(decode_bing_url("https://www.bing.com/ck/a?u=a1L2ltYWdlcy9zZWFyY2g"), None);
    }

    #[test]
    fn truncate_to_respects_char_boundaries() {
        // multi-byte char split by the limit must not panic or split
        let s = "あ".repeat(40); // 120 bytes
        let out = truncate_to(s, 20);
        assert!(out.starts_with("あああ"));
        assert!(out.contains("[truncated"));
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

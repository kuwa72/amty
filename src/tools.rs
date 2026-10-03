use crate::app::App;
use crate::config::{ApprovalMode, Config};
use crate::types::ToolSpec;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Safe,
    Write,
    Exec,
}

pub fn risk_of(name: &str) -> Risk {
    match name {
        "fs_read" | "fs_list" | "config_get" | "mcp" => Risk::Safe,
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
            name: "config_get".into(),
            description: "Read an amty setting. Keys: provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, provider.<name>.<field>.".into(),
            schema: schema(json!({"key": {"type": "string", "description": "omit to list keys"}}), &[]),
        },
        ToolSpec {
            name: "config_set".into(),
            description: "Change an amty setting at runtime (persisted). Keys: provider, model, approval(ask|auto|allowlist), system_prompt, max_tokens, allow_commands, allow_paths, +allow_commands, +allow_paths, provider.<name>.{model,api_key,api_key_env,base_url}, mcp.<name>.enabled.".into(),
            schema: schema(
                json!({"key": {"type": "string"}, "value": {"type": "string"}}),
                &["key", "value"],
            ),
        },
        ToolSpec {
            name: "mcp".into(),
            description: "Manage MCP servers. actions: list | tools | enable <name> | disable <name> | reconnect <name>.".into(),
            schema: schema(
                json!({
                    "action": {"type": "string"},
                    "server": {"type": "string"}
                }),
                &["action"],
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
        "config_set" => format!("config_set: {} = {}", input["key"].as_str().unwrap_or(""), input["value"].as_str().unwrap_or("")),
        "mcp" => format!("mcp: {} {}", input["action"].as_str().unwrap_or(""), input["server"].as_str().unwrap_or("")),
        _ => {
            let s = input.to_string();
            format!("{name}: {}", &s[..s.len().min(300)])
        }
    }
}

/// Decide whether `input` needs interactive approval under `cfg`.
pub fn needs_approval(cfg: &Config, name: &str, input: &Value) -> bool {
    match risk_of(name) {
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
            let (prog, flag, command) = wrap_shell(command);
            let mut cmd = tokio::process::Command::new(prog);
            cmd.args([flag, &command]);
            if let Some(wd) = input["workdir"].as_str() {
                cmd.current_dir(wd);
            }
            let run = cmd.output();
            match tokio::time::timeout(Duration::from_secs(timeout), run).await {
                Ok(Ok(out)) => {
                    let mut s = String::new();
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
        "config_get" => {
            let cfg = app.cfg.read().map_err(|e| e.to_string())?;
            match input["key"].as_str() {
                Some(k) => cfg.get(k).map_err(|e| e.to_string()),
                None => Ok("provider, model, approval, system_prompt, allow_commands, allow_paths, providers, mcp_servers, provider.<name>.{model,base_url,api_key_env,api_key}".into()),
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
                _ => Err(format!("unknown action '{action}'")),
            }
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

    #[test]
    fn summarize_includes_target() {
        assert!(summarize("shell", &json!({"command": "make"})).contains("make"));
        assert!(summarize("fs_write", &json!({"path": "/a"})).contains("/a"));
    }
}

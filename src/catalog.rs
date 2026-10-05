//! One-click MCP server catalog — well-known servers installable via `npx -y`.
use crate::config::McpServerConf;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub struct EnvSpec {
    pub key: &'static str,
    /// English hint shown next to the input field.
    pub hint: &'static str,
    pub hint_ja: &'static str,
    /// mask input (password field)
    pub secret: bool,
    pub required: bool,
}

pub struct Entry {
    /// used as the mcp server name in config.toml
    pub id: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub desc_ja: &'static str,
    /// npm package run via `npx -y <pkg>`
    pub package: &'static str,
    pub env: &'static [EnvSpec],
    /// extra setup the user must do by hand (OAuth keys file etc.)
    pub note: &'static str,
    pub note_ja: &'static str,
    /// OAuth/auth subcommand runnable as `npx -y <pkg> <cmd>` from the GUI
    pub auth_cmd: Option<&'static str>,
    /// credentials file the server needs before auth can run
    pub keys: Option<KeysSpec>,
    /// hosted (streamable HTTP) endpoint — no local command needed
    pub url: Option<&'static str>,
    /// OAuth parameters for hosted entries
    pub oauth: Option<OauthSpec>,
}

#[derive(Clone, Copy)]
pub struct OauthSpec {
    /// space-separated scope list ("" = server default)
    pub scopes: &'static str,
    /// fixed endpoints (Google); None = RFC 8414/9728 discovery
    pub auth_url: Option<&'static str>,
    pub token_url: Option<&'static str>,
    /// request offline access (refresh token) — Google needs these params
    pub offline: bool,
    /// client id/secret must be provided by the user (no DCR available)
    pub needs_client: bool,
}

#[derive(Clone, Copy)]
pub struct KeysSpec {
    /// expected file path relative to the home dir
    pub path: &'static str,
}

pub const CATALOG: &[Entry] = &[
    Entry {
        id: "notion",
        label: "Notion",
        desc: "Official Notion MCP — read/write pages, databases, search.",
        desc_ja: "Notion公式MCP — ページ/DBの読み書き・検索。",
        package: "@notionhq/notion-mcp-server",
        env: &[EnvSpec {
            key: "NOTION_TOKEN",
            hint: "integration token (ntn_…) from notion.so/profile/integrations",
            hint_ja: "インテグレーショントークン (ntn_…) — notion.so/profile/integrations で発行",
            secret: true,
            required: true,
        }],
        note: "Create an internal integration at notion.so/profile/integrations, then share target pages with it.",
        note_ja: "notion.so/profile/integrations でインテグレーションを作成し、対象ページに「コネクト」で共有してください。",
        auth_cmd: None,
        keys: None,
        url: None,
        oauth: None,
    },
    Entry {
        id: "slack",
        label: "Slack",
        desc: "Slack workspace — channels, posting, search.",
        desc_ja: "Slackワークスペース — チャンネル/投稿/検索。",
        package: "@modelcontextprotocol/server-slack",
        env: &[
            EnvSpec {
                key: "SLACK_BOT_TOKEN",
                hint: "xoxb-… bot token",
                hint_ja: "xoxb-… のBotトークン",
                secret: true,
                required: true,
            },
            EnvSpec {
                key: "SLACK_TEAM_ID",
                hint: "workspace team id (T0…)",
                hint_ja: "ワークスペースのチームID (T0…)",
                secret: false,
                required: true,
            },
        ],
        note: "Create a Slack app at api.slack.com/apps, add bot scopes (channels:history, chat:write, …), install it, copy the Bot User OAuth Token.",
        note_ja: "api.slack.com/apps でアプリを作成 → Botスコープ (channels:history, chat:write など) を付与 → インストールしてBotトークンをコピー。",
        auth_cmd: None,
        keys: None,
        url: None,
        oauth: None,
    },
    // ---- hosted (streamable HTTP) servers: OAuth, no local process ----
    Entry {
        id: "notion-hosted",
        label: "Notion (hosted)",
        desc: "Official hosted Notion MCP — standard OAuth, nothing to install.",
        desc_ja: "Notion公式ホスト型MCP — 標準OAuth認証、インストール不要。",
        package: "",
        env: &[],
        note: "Standard OAuth — press auth, sign in to Notion in the browser, done.",
        note_ja: "標準OAuth — 「認証」ボタンでブラウザが開き、Notionにログインするだけで完了します。",
        auth_cmd: None,
        keys: None,
        url: Some("https://mcp.notion.com/mcp"),
        oauth: Some(OauthSpec {
            scopes: "",
            auth_url: None,
            token_url: None,
            offline: false,
            needs_client: false,
        }),
    },
];

pub fn find(id: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// Absolute path of the credentials file a catalog entry expects.
pub fn keys_path(id: &str) -> Option<std::path::PathBuf> {
    let entry = find(id)?;
    let spec = entry.keys?;
    Some(dirs::home_dir()?.join(spec.path))
}

/// Copy a downloaded OAuth keys file into the location the server expects.
pub fn place_keys(id: &str, src: &std::path::Path) -> Result<std::path::PathBuf> {
    let dst = keys_path(id).context("this server takes no keys file")?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(src, &dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(dst)
}

/// A windows-subsystem GUI app has no console, so every console-subsystem
/// child (cmd, powershell, where, npx.cmd…) would briefly pop a console
/// window. `CREATE_NO_WINDOW` suppresses it — apply to every Command spawned.
#[cfg(target_os = "windows")]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Set CREATE_NO_WINDOW on a std Command (no-op off Windows).
pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Same for a tokio Command (creation_flags is an inherent method there).
pub fn no_window_async(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// `where` lookup for a bare command name on Windows. CreateProcess only
/// auto-appends ".exe", so PATHEXT shims (`npx.cmd`, `npm.cmd`, …) must be
/// resolved explicitly — Rust then spawns .cmd/.bat via cmd.exe internally.
/// Prefers .exe over .cmd/.bat; skips extensionless scripts and .ps1.
#[cfg(target_os = "windows")]
pub fn shim_where(name: &str) -> Option<String> {
    let mut c = std::process::Command::new("where");
    c.arg(name);
    no_window(&mut c);
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    hits.iter()
        .find(|p| p.to_lowercase().ends_with(".exe"))
        .or_else(|| {
            hits.iter().find(|p| {
                let l = p.to_lowercase();
                l.ends_with(".cmd") || l.ends_with(".bat")
            })
        })
        .cloned()
}

/// Resolve a bare command name to a spawnable path for MCP stdio servers.
/// Only needed on Windows; elsewhere the name goes through unchanged.
pub fn resolve_spawn_command(command: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        let bare = !command.contains(['\\', '/']) && !command.contains('.');
        if bare {
            if let Some(p) = shim_where(command) {
                return p;
            }
        }
    }
    command.to_string()
}

/// Warnings about how this server def will actually spawn (Windows shim
/// quirks). Surfaced in mcp_install/mcp_add results so bad invocations are
/// caught at install time instead of failing silently at connect.
pub fn spawn_warnings(conf: &McpServerConf) -> Vec<String> {
    let mut w: Vec<String> = vec![];
    #[cfg(target_os = "windows")]
    {
        let cmd = conf.command.to_lowercase();
        if cmd.contains("powershell") || cmd.contains("pwsh") {
            if let Some(i) = conf.args.iter().position(|a| a == "-File") {
                match conf.args.get(i + 1) {
                    Some(f) if f.to_lowercase().ends_with(".ps1") => {}
                    Some(f) => w.push(format!("powershell -File only accepts .ps1 scripts — '{f}' will fail")),
                    None => w.push("powershell -File needs a script path argument".into()),
                }
            }
        }
        let bare = !conf.command.contains(['\\', '/']) && !conf.command.contains('.');
        if bare && !conf.command.is_empty() && conf.url.is_none() {
            match shim_where(&conf.command) {
                Some(p) => w.push(format!("'{}' resolves to '{p}'", conf.command)),
                None => w.push(format!("'{}' not found on PATH — spawn will fail", conf.command)),
            }
        }
    }
    let _ = &conf;
    let _ = &mut w; // unix: nothing pushed
    w
}

/// Resolve the npx executable. On Windows `npx` may be freshly installed but
/// missing from this process's (stale) PATH — fall back to the standard
/// installer location. Rust spawns .cmd shims via cmd.exe internally.
fn npx_exe() -> Option<String> {
    #[cfg(not(target_os = "windows"))]
    {
        Some("npx".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(p) = shim_where("npx") {
            return Some(p);
        }
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        for p in [
            format!(r"{pf}\nodejs\npx.cmd"),
            format!(r"{}\fnm\aliases\default\npx.cmd", std::env::var("LOCALAPPDATA").unwrap_or_default()),
            format!(r"{}\npx.cmd", std::env::var("APPDATA").unwrap_or_default()),
        ] {
            if std::path::Path::new(&p).exists() {
                return Some(p);
            }
        }
        None
    }
}

/// Is `npx` runnable on this machine?
pub fn node_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        npx_exe()
            .and_then(|npx| {
                let mut c = std::process::Command::new(npx);
                c.arg("--version");
                no_window(&mut c);
                c.output().ok()
            })
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

#[derive(Serialize)]
pub struct CatalogRow {
    pub id: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub desc_ja: &'static str,
    pub package: &'static str,
    pub env: Vec<serde_json::Value>,
    pub note: &'static str,
    pub note_ja: &'static str,
    pub auth_cmd: Option<&'static str>,
    pub url: Option<&'static str>,
    /// oauth-capable hosted entry; needs_client => user must supply id/secret
    pub oauth: Option<&'static str>, // "discovery" | "client"
    pub installed: bool,
}

pub fn rows(installed: &BTreeMap<String, McpServerConf>) -> Vec<CatalogRow> {
    CATALOG
        .iter()
        .map(|e| CatalogRow {
            id: e.id,
            label: e.label,
            desc: e.desc,
            desc_ja: e.desc_ja,
            package: e.package,
            env: e
                .env
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "key": s.key, "hint": s.hint, "hint_ja": s.hint_ja,
                        "secret": s.secret, "required": s.required,
                    })
                })
                .collect(),
            note: e.note,
            note_ja: e.note_ja,
            auth_cmd: e.auth_cmd,
            url: e.url,
            oauth: e.oauth.map(|o| if o.needs_client { "client" } else { "discovery" }),
            installed: installed.contains_key(e.id),
        })
        .collect()
}

/// Build the McpServerConf for a catalog entry. npx is a .cmd shim on
/// Windows and can't be spawned directly, so it goes through `cmd /c`.
/// Env keys OAUTH_CLIENT_ID / OAUTH_CLIENT_SECRET map to the conf's OAuth
/// fields for hosted entries.
pub fn build_conf(id: &str, env: BTreeMap<String, String>) -> Result<(String, McpServerConf)> {
    let entry = find(id).context("unknown catalog id")?;
    if let Some(url) = entry.url {
        let oauth = entry.oauth.context("hosted entry without oauth spec")?;
        let cid = env.get("OAUTH_CLIENT_ID").map(|s| s.trim().to_string()).unwrap_or_default();
        let csec = env.get("OAUTH_CLIENT_SECRET").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if oauth.needs_client && cid.is_empty() {
            bail!("OAUTH_CLIENT_ID is required (create an OAuth client in Google Cloud first)");
        }
        return Ok((
            entry.id.to_string(),
            McpServerConf {
                url: Some(url.into()),
                oauth_client_id: if cid.is_empty() { None } else { Some(cid) },
                oauth_client_secret: csec,
                oauth_scopes: Some(oauth.scopes.into()),
                oauth_token_url: oauth.token_url.map(String::from),
                ..Default::default()
            },
        ));
    }
    if !node_available() {
        bail!("Node.js (npx) not found on PATH — install Node.js first");
    }
    let mut final_env = BTreeMap::new();
    for spec in entry.env {
        let v = env.get(spec.key).map(|s| s.trim().to_string()).unwrap_or_default();
        if spec.required && v.is_empty() {
            bail!("{} is required", spec.key);
        }
        if !v.is_empty() {
            final_env.insert(spec.key.into(), v);
        }
    }
    let command = npx_exe().context("npx disappeared")?;
    inject_node_dir(&command, &mut final_env);
    let args = vec!["-y".to_string(), entry.package.into()];
    Ok((entry.id.to_string(), McpServerConf { command, args, env: final_env, ..Default::default() }))
}

/// When npx was found via a fallback path (this process's PATH predates the
/// Node install), the .cmd shim still needs `node` on PATH — inject its dir.
fn inject_node_dir(command: &str, env: &mut BTreeMap<String, String>) {
    if command != "npx" && !env.contains_key("PATH") {
        if let Some(dir) = std::path::Path::new(command).parent() {
            let cur = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".into(), format!("{};{cur}", dir.display()));
        }
    }
}

/// Command+env for `npx -y <pkg> <auth_cmd>` — used by GUI/API auth flows.
/// `base_env` is the installed server's env (OAuth flows often need the
/// same client id/secret as the server itself).
pub fn auth_spawn(id: &str, base_env: &BTreeMap<String, String>) -> Result<(String, Vec<String>, BTreeMap<String, String>)> {
    let entry = find(id).context("unknown catalog id")?;
    let auth = entry.auth_cmd.context("no auth command for this entry")?;
    let command = npx_exe().context("npx disappeared")?;
    let mut env = base_env.clone();
    inject_node_dir(&command, &mut env);
    Ok((command, vec!["-y".into(), entry.package.into(), auth.into()], env))
}

// ---------- dynamic catalog (web search / npm registry) ----------

#[derive(Debug, Clone, Deserialize)]
struct NpmSearchResponse {
    objects: Vec<NpmObject>,
}

#[derive(Debug, Clone, Deserialize)]
struct NpmObject {
    package: NpmPackage,
}

#[derive(Debug, Clone, Deserialize)]
struct NpmPackage {
    name: String,
    #[serde(default)]
    description: String,
}

/// Search npm for packages matching a keyword. Prefer official
/// `@modelcontextprotocol/server-*` packages, but fall back to any match.
pub async fn npm_search(query: &str, count: usize) -> Result<Vec<(String, String)>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    // npm registry search text query
    let q = if query.is_empty() { "modelcontextprotocol server" } else { query };
    let url = format!(
        "https://registry.npmjs.org/-/v1/search?text={}&size={}",
        q.split_whitespace().collect::<Vec<_>>().join("+"),
        count.max(1).min(50)
    );
    let res: NpmSearchResponse = client.get(&url).send().await?.error_for_status()?.json().await?;
    Ok(res
        .objects
        .into_iter()
        .map(|o| (o.package.name, o.package.description))
        .collect())
}

/// Build an McpServerConf from a dynamically-discovered package.
/// `command` is usually "npx" or "uvx"; `args` defaults to sensible values.
pub fn build_dynamic(
    id: &str,
    package: &str,
    command: &str,
    args: Option<Vec<String>>,
    env: BTreeMap<String, String>,
) -> Result<(String, McpServerConf)> {
    let command = command.to_string();
    let args = args.unwrap_or_else(|| match command.as_str() {
        "npx" => vec!["-y".into(), package.into()],
        "uvx" => vec![package.into()],
        _ => vec![package.into()],
    });
    if command == "npx" && !node_available() {
        bail!("Node.js (npx) not found on PATH — install Node.js first");
    }
    let mut final_env = env;
    if command == "npx" {
        if let Some(npx) = npx_exe() {
            inject_node_dir(&npx, &mut final_env);
        }
    }
    Ok((
        id.to_string(),
        McpServerConf {
            command,
            args,
            env: final_env,
            enabled: true,
            ..Default::default()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_dcr_entry_needs_no_client() {
        let (_, conf) = build_conf("notion-hosted", Default::default()).unwrap();
        assert_eq!(conf.url.as_deref(), Some("https://mcp.notion.com/mcp"));
        assert!(conf.oauth_client_id.is_none());
    }

    #[test]
    fn unknown_id_rejected() {
        assert!(find("nope").is_none());
    }

    #[test]
    fn required_env_enforced() {
        if !node_available() {
            return; // no node in this environment
        }
        assert!(build_conf("notion", Default::default()).is_err());
        let mut env = BTreeMap::new();
        env.insert("NOTION_TOKEN".to_string(), "ntn_x".to_string());
        let (name, conf) = build_conf("notion", env).unwrap();
        assert_eq!(name, "notion");
        assert_eq!(conf.env["NOTION_TOKEN"], "ntn_x");
        assert!(conf.args.iter().any(|a| a.contains("notion-mcp")));
        assert!(conf.command.contains("npx"));
    }

    #[test]
    fn dynamic_builds_npx_and_uvx_defaults() {
        let (_name, conf) = build_dynamic("github", "@modelcontextprotocol/server-github", "npx", None, BTreeMap::new()).unwrap();
        assert!(conf.enabled);
        assert_eq!(conf.command, "npx");
        assert_eq!(conf.args, vec!["-y", "@modelcontextprotocol/server-github"]);

        let (_name, conf) = build_dynamic("git", "mcp-server-git", "uvx", None, BTreeMap::new()).unwrap();
        assert_eq!(conf.command, "uvx");
        assert_eq!(conf.args, vec!["mcp-server-git"]);

        let mut env = BTreeMap::new();
        env.insert("GITHUB_PERSONAL_ACCESS_TOKEN".into(), "ghp_x".into());
        let (_name, conf) = build_dynamic("github", "@modelcontextprotocol/server-github", "npx", None, env).unwrap();
        assert_eq!(conf.env["GITHUB_PERSONAL_ACCESS_TOKEN"], "ghp_x");
    }
}

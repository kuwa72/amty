//! One-click MCP server catalog — well-known servers installable via `npx -y`.
use crate::config::McpServerConf;
use anyhow::{bail, Context, Result};
use serde::Serialize;
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
    },
    Entry {
        id: "gmail",
        label: "Gmail",
        desc: "Gmail — read/search/send, labels, attachments. Community (gongrzhe).",
        desc_ja: "Gmail — 読み取り/検索/送信、ラベル、添付。コミュニティ製(gongrzhe)。",
        package: "@gongrzhe/server-gmail-autoauth-mcp",
        env: &[],
        note: "Requires Google Cloud OAuth keys: place gcp-oauth.keys.json in ~/.gmail-mcp/, then run auth (button below or `npx @gongrzhe/server-gmail-autoauth-mcp auth`).",
        note_ja: "Google CloudのOAuthキーが必要: gcp-oauth.keys.json を ~/.gmail-mcp/ に置き、下の「認証」ボタン(または `npx @gongrzhe/server-gmail-autoauth-mcp auth`)を実行。",
        auth_cmd: Some("auth"),
    },
    Entry {
        id: "gdrive",
        label: "Google Drive",
        desc: "Drive + Docs/Sheets/Slides — files, folders, docs content. Community (piotr-agier).",
        desc_ja: "Drive + Docs/Sheets/Slides — ファイル/フォルダ/ドキュメント操作。コミュニティ製(piotr-agier)。",
        package: "@piotr-agier/google-drive-mcp",
        env: &[EnvSpec {
            key: "GOOGLE_DRIVE_OAUTH_CREDENTIALS",
            hint: "path to gcp-oauth.keys.json",
            hint_ja: "gcp-oauth.keys.json のパス",
            secret: false,
            required: false,
        }],
        note: "Place Google Cloud OAuth keys as gcp-oauth.keys.json in ~/.config/google-drive-mcp/ (or set the env var), enable Drive/Docs/Sheets/Slides APIs. Browser auth runs on first launch.",
        note_ja: "Google CloudのOAuthキーを gcp-oauth.keys.json として ~/.config/google-drive-mcp/ に配置(または環境変数で指定)し、Drive/Docs/Sheets/Slides APIを有効化。下の「認証」ボタンでブラウザ認証(初回は自動でも開きます)。",
        auth_cmd: Some("auth"),
    },
    Entry {
        id: "gdocs",
        label: "Google Docs",
        desc: "Docs/Sheets/Drive/Calendar — document create/read/edit. Community (a-bonus).",
        desc_ja: "Docs/Sheets/Drive/Calendar — ドキュメントの作成/読み取り/編集。コミュニティ製(a-bonus)。",
        package: "@a-bonus/google-docs-mcp",
        env: &[
            EnvSpec {
                key: "GOOGLE_CLIENT_ID",
                hint: "OAuth client id",
                hint_ja: "OAuthクライアントID",
                secret: false,
                required: true,
            },
            EnvSpec {
                key: "GOOGLE_CLIENT_SECRET",
                hint: "OAuth client secret",
                hint_ja: "OAuthクライアントシークレット",
                secret: true,
                required: true,
            },
        ],
        note: "Enable Docs/Sheets/Drive APIs in Google Cloud, create a Desktop-type OAuth client, then run `npx -y @a-bonus/google-docs-mcp auth` once.",
        note_ja: "Google CloudでDocs/Sheets/Drive APIを有効化し、デスクトップ型OAuthクライアントを作成。envにID/SECRETを入れて導入後、「認証」ボタンを実行。",
        auth_cmd: Some("auth"),
    },
];

pub fn find(id: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.id == id)
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
        let on_path = std::process::Command::new("where")
            .arg("npx")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if on_path {
            return Some("npx".to_string());
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
            .and_then(|npx| std::process::Command::new(npx).arg("--version").output().ok())
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
            installed: installed.contains_key(e.id),
        })
        .collect()
}

/// Build the McpServerConf for a catalog entry. npx is a .cmd shim on
/// Windows and can't be spawned directly, so it goes through `cmd /c`.
pub fn build_conf(id: &str, env: BTreeMap<String, String>) -> Result<(String, McpServerConf)> {
    let entry = find(id).context("unknown catalog id")?;
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
    Ok((entry.id.to_string(), McpServerConf { command, args, env: final_env, enabled: true }))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

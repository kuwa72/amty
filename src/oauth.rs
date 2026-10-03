//! Minimal OAuth 2.0 authorization-code + PKCE client for hosted (streamable
//! HTTP) MCP servers. Supports fixed endpoints (Google) and MCP-style
//! discovery + dynamic client registration (Notion et al.).
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

/// Fixed localhost port for the OAuth callback. Google "Web application"
/// clients require an exact-match redirect URI, so users register
/// `http://127.0.0.1:8571/oauth/callback` once.
pub const OAUTH_PORT: u16 = 8571;
pub const CALLBACK_PATH: &str = "/oauth/callback";

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// unix seconds when the access token expires
    #[serde(default)]
    pub expires_at: Option<u64>,
}

fn store_path(name: &str) -> PathBuf {
    crate::config::data_dir().join("oauth").join(format!("{name}.json"))
}

pub fn load(name: &str) -> Option<Tokens> {
    let data = std::fs::read(store_path(name)).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn save(name: &str, t: &Tokens) -> Result<()> {
    let path = store_path(name);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(&f, t)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn forget(name: &str) {
    let _ = std::fs::remove_file(store_path(name));
}

/// A usable access token: returns the stored one, refreshes when expired.
/// Ok(None) = no token stored → user must run the auth flow.
pub async fn access_token(name: &str, conf: &crate::config::McpServerConf) -> Result<Option<String>> {
    let Some(mut t) = load(name) else { return Ok(None) };
    let now = crate::session::now_secs();
    let fresh = t.expires_at.map(|e| now + 60 < e).unwrap_or(true);
    if fresh && !t.access_token.is_empty() {
        return Ok(Some(t.access_token));
    }
    let Some(rt) = t.refresh_token.clone() else { return Ok(None) };
    let token_url = conf.oauth_token_url.clone().ok_or_else(|| anyhow!("no token endpoint stored"))?;
    let client_id = conf.oauth_client_id.clone().unwrap_or_default();
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", rt),
        ("client_id", client_id),
    ];
    if let Some(s) = &conf.oauth_client_secret {
        form.push(("client_secret", s.clone()));
    }
    let http = reqwest::Client::new();
    let resp = http.post(&token_url).form(&form).send().await?;
    if !resp.status().is_success() {
        // refresh rejected (revoked/expired) — force re-auth
        forget(name);
        bail!("refresh failed: HTTP {}", resp.status());
    }
    let v: Value = resp.json().await?;
    t.access_token = v["access_token"].as_str().unwrap_or_default().into();
    if let Some(rt) = v["refresh_token"].as_str() {
        t.refresh_token = Some(rt.into());
    }
    if let Some(secs) = v["expires_in"].as_u64() {
        t.expires_at = Some(now + secs);
    }
    save(name, &t)?;
    Ok(Some(t.access_token))
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_verifier() -> String {
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    raw[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    b64url(&raw)
}

fn open_browser(url: &str) -> Result<()> {
    // `cmd /c start` would split the URL at every unquoted '&' (query params
    // get eaten as command separators) — rundll32 opens the default browser
    // without any shell parsing.
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    r.map(|_| ()).context("failed to open browser")
}

/// RFC 9728 + RFC 8414 discovery: server URL → authorization/token/(register) endpoints.
async fn discover_endpoints(server_url: &str) -> Result<(String, String, Option<String>)> {
    let u = reqwest::Url::parse(server_url)?;
    let base = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default());
    let http = reqwest::Client::new();
    // 1. protected-resource metadata → authorization server issuer
    let mut issuers: Vec<String> = vec![];
    for wk in [
        format!("{base}/.well-known/oauth-protected-resource"),
        format!("{base}/.well-known/oauth-protected-resource{}", u.path()),
    ] {
        if let Ok(r) = http.get(&wk).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(a) = v["authorization_servers"].as_array() {
                    issuers.extend(a.iter().filter_map(|x| x.as_str().map(String::from)));
                }
                if !issuers.is_empty() {
                    break;
                }
            }
        }
    }
    if issuers.is_empty() {
        issuers.push(base.clone());
    }
    // 2. authorization-server metadata
    for issuer in issuers {
        for wk in [
            format!("{}/.well-known/oauth-authorization-server", issuer.trim_end_matches('/')),
            format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')),
        ] {
            if let Ok(r) = http.get(&wk).send().await {
                if !r.status().is_success() {
                    continue;
                }
                if let Ok(v) = r.json::<Value>().await {
                    let auth = v["authorization_endpoint"].as_str().map(String::from);
                    let tok = v["token_endpoint"].as_str().map(String::from);
                    if let (Some(a), Some(t)) = (auth, tok) {
                        let reg = v["registration_endpoint"].as_str().map(String::from);
                        return Ok((a, t, reg));
                    }
                }
            }
        }
    }
    bail!("oauth discovery failed for {server_url}")
}

/// Result of a successful auth flow — caller persists these into the config.
pub struct Outcome {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub token_url: String,
}

/// Full authorization flow. Blocks until the browser callback arrives (or
/// times out). Writes progress into `log`. On success persists tokens and
/// returns the resolved client registration.
pub async fn authorize(
    name: &str,
    conf: &crate::config::McpServerConf,
    spec: &crate::catalog::OauthSpec,
    log: &PathBuf,
) -> Result<Outcome> {
    let logf = |msg: String| {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(log) {
            let _ = writeln!(f, "{msg}");
        }
    };

    let url = conf.url.clone().unwrap_or_default();
    let (auth_url, token_url, reg_url) = if let (Some(a), Some(t)) = (spec.auth_url, spec.token_url) {
        (a.to_string(), t.to_string(), None)
    } else {
        logf(format!("discovering oauth endpoints for {url}"));
        discover_endpoints(&url).await?
    };
    // client credentials: configured, or dynamic registration
    let mut client_id = conf.oauth_client_id.clone().unwrap_or_default();
    let mut client_secret = conf.oauth_client_secret.clone();
    if client_id.is_empty() {
        let Some(reg) = reg_url.clone() else {
            bail!("no OAuth client id configured and this server has no dynamic registration — \
                   set 'oauth client id' in Settings → MCP");
        };
        logf(format!("registering oauth client at {reg}"));
        let http = reqwest::Client::new();
        let resp = http
            .post(&reg)
            .json(&json!({
                "client_name": "amty",
                "redirect_uris": [format!("http://127.0.0.1:{OAUTH_PORT}{CALLBACK_PATH}")],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": if spec.needs_client { "client_secret_basic" } else { "none" },
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("dynamic registration failed: HTTP {} — set client id/secret instead", resp.status());
        }
        let v: Value = resp.json().await?;
        client_id = v["client_id"].as_str().context("registration: no client_id")?.to_string();
        client_secret = v["client_secret"].as_str().map(String::from);
        logf("registered oauth client".into());
    }

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{OAUTH_PORT}"))
        .await
        .context(format!("oauth callback port {OAUTH_PORT} busy"))?;
    let redirect = format!("http://127.0.0.1:{OAUTH_PORT}{CALLBACK_PATH}");

    let verifier = random_verifier();
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().simple().to_string();
    let mut q: Vec<(String, String)> = vec![
        ("response_type".into(), "code".into()),
        ("client_id".into(), client_id.clone()),
        ("redirect_uri".into(), redirect.clone()),
        ("state".into(), state.clone()),
        ("code_challenge".into(), challenge),
        ("code_challenge_method".into(), "S256".into()),
    ];
    if !spec.scopes.is_empty() {
        q.push(("scope".into(), spec.scopes.to_string()));
    }
    if spec.offline {
        q.push(("access_type".into(), "offline".into()));
        q.push(("prompt".into(), "consent".into()));
    }
    let qs: String = q.iter().map(|(k, v)| format!("{k}={}", urlenc(v))).collect::<Vec<_>>().join("&");
    let sep = if auth_url.contains('?') { "&" } else { "?" };
    let full = format!("{auth_url}{sep}{qs}");

    logf(format!("listening on {redirect}"));
    if open_browser(&full).is_ok() {
        logf("opened browser for authorization".into());
    } else {
        logf(format!("open this URL in your browser:\n{full}"));
    }

    // wait for the redirect
    let code = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or_default().to_string();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            if !path.starts_with(CALLBACK_PATH) {
                let _ = sock.try_write(b"HTTP/1.1 404 Not Found\r\ncontent-length:0\r\n\r\n");
                continue;
            }
            let _ = sock.try_write(b"HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\r\n<h2>amty: auth complete. You can close this tab.</h2>");
            let query = path.split_once('?').map(|x| x.1).unwrap_or_default();
            let mut code = String::new();
            let mut st = String::new();
            for kv in query.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    match k {
                        "code" => code = urldec(v),
                        "state" => st = v.to_string(),
                        "error" => return Err(anyhow!("oauth error: {}", urldec(v))),
                        _ => {}
                    }
                }
            }
            if st != state {
                return Err(anyhow!("state mismatch"));
            }
            if code.is_empty() {
                return Err(anyhow!("no code in callback"));
            }
            return Ok(code);
        }
    })
    .await
    .context("auth timed out (5min)")??;
    drop(listener);
    logf("got authorization code, exchanging…".into());

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(s) = &client_secret {
        form.push(("client_secret", s.clone()));
    }
    let http = reqwest::Client::new();
    let resp = http.post(&token_url).form(&form).send().await?;
    if !resp.status().is_success() {
        bail!("token exchange failed: HTTP {} {}", resp.status(), resp.text().await.unwrap_or_default());
    }
    let v: Value = resp.json().await?;
    let now = crate::session::now_secs();
    let t = Tokens {
        access_token: v["access_token"].as_str().context("no access_token")?.into(),
        refresh_token: v["refresh_token"].as_str().map(String::from),
        expires_at: v["expires_in"].as_u64().map(|e| now + e),
    };
    save(name, &t)?;
    logf("token stored".into());
    Ok(Outcome { client_id, client_secret, token_url })
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn urldec(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?');
                out.push(h);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_challenge() {
        // verifier → S256 challenge must be base64url(sha256(verifier))
        let v = random_verifier();
        assert!(v.len() >= 43);
        let c = b64url(&Sha256::digest(v.as_bytes()));
        assert!(!c.contains('+') && !c.contains('/') && !c.contains('='));
        assert_eq!(c.len(), 43);
    }

    #[test]
    fn urlencode_roundtrip() {
        let s = "a+b c/d=e&f~日本語";
        assert_eq!(urldec(&urlenc(s)), s);
    }

    #[test]
    fn tokens_serde_roundtrip() {
        let t = Tokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(123),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Tokens = serde_json::from_str(&s).unwrap();
        assert_eq!(back.access_token, "at");
        assert_eq!(back.refresh_token.as_deref(), Some("rt"));
        assert_eq!(back.expires_at, Some(123));
        // minimal json also parses
        let bare: Tokens = serde_json::from_str(r#"{"access_token":"x"}"#).unwrap();
        assert!(bare.refresh_token.is_none());
    }
}

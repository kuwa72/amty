use crate::config::Config;
use crate::mcp::Manager as McpManager;
use crate::session::SessionStore;
use crate::types::{AppEvent, EvKind};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, oneshot};

pub struct PendingApproval {
    pub id: String,
    pub session: String,
    pub tool: String,
    pub detail: String,
    tx: oneshot::Sender<bool>,
}

#[derive(Default)]
pub struct ApprovalQueue {
    inner: Mutex<HashMap<String, PendingApproval>>,
}

impl ApprovalQueue {
    pub fn request(&self, session: &str, tool: &str, detail: &str) -> (String, oneshot::Receiver<bool>) {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().insert(
            id.clone(),
            PendingApproval {
                id: id.clone(),
                session: session.into(),
                tool: tool.into(),
                detail: detail.into(),
                tx,
            },
        );
        (id, rx)
    }

    pub fn resolve(&self, id: &str, allow: bool) -> bool {
        if let Some(p) = self.inner.lock().unwrap().remove(id) {
            let _ = p.tx.send(allow);
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<serde_json::Value> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .map(|p| serde_json::json!({
                "id": p.id, "session": p.session, "tool": p.tool, "detail": p.detail,
            }))
            .collect()
    }
}

pub struct AuthRun {
    pub running: bool,
    pub log: std::path::PathBuf,
}

pub struct App {
    pub cfg: RwLock<Config>,
    pub cfg_path: std::path::PathBuf,
    pub sessions: SessionStore,
    pub mcp: Arc<McpManager>,
    pub approvals: ApprovalQueue,
    pub events: broadcast::Sender<AppEvent>,
    /// session id → cancel flag
    pub runs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// catalog id → auth subprocess state
    pub auth: Mutex<HashMap<String, AuthRun>>,
    pub token: String,
    pub started: u64,
    pub rt: tokio::runtime::Handle,
}

#[derive(Serialize)]
struct RuntimeFile {
    port: u16,
    token: String,
    pid: u32,
}

impl App {
    /// Build shared state and start the control API listener.
    /// Call from inside the tokio runtime.
    pub async fn start() -> Result<Arc<Self>> {
        let (cfg, cfg_path) = Config::load()?;
        let sessions = SessionStore::load(crate::config::data_dir().join("sessions"));
        let mcp = McpManager::new(
            cfg.mcp_servers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        );
        let (events, _) = broadcast::channel(2048);
        let token = uuid::Uuid::new_v4().simple().to_string();

        // bind first so we know the port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        let app = Arc::new(App {
            cfg: RwLock::new(cfg),
            cfg_path,
            sessions,
            mcp,
            approvals: ApprovalQueue::default(),
            events,
            runs: Mutex::new(HashMap::new()),
            auth: Mutex::new(HashMap::new()),
            token,
            started: crate::session::now_secs(),
            rt: tokio::runtime::Handle::current(),
        });

        let router = crate::api::router(app.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // write runtime.json (0600) for CLI/mcp-serve discovery
        let rt_path = crate::config::runtime_path();
        if let Some(dir) = rt_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = serde_json::to_string(&RuntimeFile {
            port,
            token: app.token.clone(),
            pid: std::process::id(),
        })?;
        std::fs::write(&rt_path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&rt_path, std::fs::Permissions::from_mode(0o600));
        }

        // connect MCP servers in the background
        let mcp = app.mcp.clone();
        let ev = app.events.clone();
        tokio::spawn(async move {
            mcp.ensure_connected().await;
            let _ = ev.send(AppEvent { session: String::new(), kind: EvKind::Touched });
        });

        Ok(app)
    }

    pub fn emit(&self, session: &str, kind: EvKind) {
        let _ = self.events.send(AppEvent { session: session.into(), kind });
    }

    pub fn begin_run(&self, session: &str) -> Option<Arc<AtomicBool>> {
        let mut runs = self.runs.lock().unwrap();
        if runs.get(session).map(|f| !f.load(Ordering::SeqCst)).unwrap_or(false) {
            return None; // already running
        }
        let flag = Arc::new(AtomicBool::new(false));
        runs.insert(session.into(), flag.clone());
        self.emit(session, EvKind::Running { running: true });
        Some(flag)
    }

    pub fn end_run(&self, session: &str) {
        self.runs.lock().unwrap().remove(session);
        self.emit(session, EvKind::Running { running: false });
    }

    pub fn cancel(&self, session: &str) -> bool {
        if let Some(f) = self.runs.lock().unwrap().get(session) {
            f.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn is_running(&self, session: &str) -> bool {
        self.runs.lock().unwrap().contains_key(session)
    }

    /// Run a catalog entry's auth flow: `npx -y <pkg> auth` for package-based
    /// servers, or the built-in OAuth client for hosted (streamable HTTP)
    /// entries. Progress goes to `<data_dir>/auth-<id>.log`. On completion
    /// the MCP manager reconnects so the server picks up fresh credentials.
    pub fn start_auth(self: &Arc<Self>, id: &str) -> Result<std::path::PathBuf> {
        let entry = crate::catalog::find(id).ok_or_else(|| anyhow::anyhow!("unknown catalog id '{id}'"))?;
        if entry.oauth.is_some() {
            return self.start_oauth(id);
        }
        if entry.auth_cmd.is_none() {
            anyhow::bail!("{id} has no auth flow");
        }
        let base_env = self
            .cfg
            .read()
            .unwrap()
            .mcp_servers
            .get(id)
            .map(|s| s.env.clone())
            .unwrap_or_default();
        let (cmd, args, env) = crate::catalog::auth_spawn(id, &base_env)?;
        let log = crate::config::data_dir().join(format!("auth-{id}.log"));
        let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log)?;
        let file_err = file.try_clone()?;
        let mut child = std::process::Command::new(&cmd)
            .args(&args)
            .envs(&env)
            .current_dir(dirs::home_dir().unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .stdout(file)
            .stderr(file_err)
            .spawn()
            .map_err(|e| anyhow::anyhow!("auth spawn '{cmd}': {e}"))?;
        self.auth.lock().unwrap().insert(
            id.to_string(),
            AuthRun { running: true, log: log.clone() },
        );
        // monitor: mark finished, append exit code, reconnect the server
        let app = self.clone();
        let id_s = id.to_string();
        let log_c = log.clone();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.to_string()).unwrap_or_else(|_| "killed".into());
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_c) {
                use std::io::Write;
                let _ = writeln!(f, "\n[amty] auth exited: {code}");
            }
            if let Ok(mut m) = app.auth.lock() {
                if let Some(a) = m.get_mut(&id_s) {
                    a.running = false;
                }
            }
            let app2 = app.clone();
            app.rt.spawn(async move {
                app2.mcp.update_servers(app2.mcp_servers_map());
                app2.mcp.reconcile().await;
                app2.emit("", EvKind::Touched);
            });
        });
        Ok(log)
    }

    /// Hosted OAuth flow: browser + localhost callback + PKCE + token store.
    /// If the server isn't in the config yet, auth doubles as install.
    fn start_oauth(self: &Arc<Self>, id: &str) -> Result<std::path::PathBuf> {
        let entry = crate::catalog::find(id).ok_or_else(|| anyhow::anyhow!("unknown catalog id"))?;
        let spec = entry.oauth.ok_or_else(|| anyhow::anyhow!("{id} is not an oauth entry"))?;
        let log = crate::config::data_dir().join(format!("auth-{id}.log"));
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log)?;
        let conf = {
            let cfg = self.cfg.read().unwrap();
            cfg.mcp_servers.get(id).cloned().unwrap_or_else(|| {
                crate::config::McpServerConf {
                    url: entry.url.map(String::from),
                    oauth_scopes: Some(spec.scopes.into()),
                    oauth_token_url: spec.token_url.map(String::from),
                    ..Default::default()
                }
            })
        };
        if spec.needs_client && conf.oauth_client_id.as_deref().unwrap_or("").is_empty() {
            anyhow::bail!(
                "{id}: no OAuth client id — install the catalog entry first (fill OAUTH_CLIENT_ID/SECRET), \
                 or set 'oauth client id/secret' in Settings → MCP"
            );
        }
        self.auth.lock().unwrap().insert(id.to_string(), AuthRun { running: true, log: log.clone() });
        let app = self.clone();
        let id_s = id.to_string();
        let log_t = log.clone();
        let rt = app.rt.clone();
        rt.spawn(async move {
            let res = crate::oauth::authorize(&id_s, &conf, &spec, &log_t).await;
            match res {
                Ok(out) => {
                    // persist client registration + ensure the server exists
                    {
                        let mut cfg = app.cfg.write().unwrap();
                        let s = cfg.mcp_servers.entry(id_s.clone()).or_insert_with(|| {
                            crate::config::McpServerConf {
                                url: entry.url.map(String::from),
                                oauth_scopes: Some(spec.scopes.into()),
                                ..Default::default()
                            }
                        });
                        s.oauth_client_id = Some(out.client_id);
                        s.oauth_client_secret = out.client_secret;
                        s.oauth_token_url = Some(out.token_url);
                        let _ = cfg.save(&app.cfg_path);
                    }
                    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_t) {
                        use std::io::Write;
                        let _ = writeln!(f, "[amty] auth complete");
                    }
                }
                Err(e) => {
                    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_t) {
                        use std::io::Write;
                        let _ = writeln!(f, "[amty] auth failed: {e}");
                    }
                }
            }
            if let Ok(mut m) = app.auth.lock() {
                if let Some(a) = m.get_mut(&id_s) {
                    a.running = false;
                }
            }
            app.mcp.update_servers(app.mcp_servers_map());
            app.mcp.reconcile().await;
            app.emit("", EvKind::Touched);
        });
        Ok(log)
    }

    /// Tail of a catalog auth log, if any.
    pub fn auth_log_tail(&self, id: &str, bytes: usize) -> Option<String> {
        let path = self.auth.lock().unwrap().get(id).map(|a| a.log.clone())
            .unwrap_or_else(|| crate::config::data_dir().join(format!("auth-{id}.log")));
        let data = std::fs::read(path).ok()?;
        let s = String::from_utf8_lossy(&data);
        Some(s.chars().rev().take(bytes).collect::<String>().chars().rev().collect())
    }

    /// Snapshot of the configured MCP servers (for `Manager::update_servers`).
    pub fn mcp_servers_map(&self) -> HashMap<String, crate::config::McpServerConf> {
        self.cfg
            .read()
            .unwrap()
            .mcp_servers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

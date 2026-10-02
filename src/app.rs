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

pub struct App {
    pub cfg: RwLock<Config>,
    pub cfg_path: std::path::PathBuf,
    pub sessions: SessionStore,
    pub mcp: Arc<McpManager>,
    pub approvals: ApprovalQueue,
    pub events: broadcast::Sender<AppEvent>,
    /// session id → cancel flag
    pub runs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub token: String,
    pub started: u64,
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
            token,
            started: crate::session::now_secs(),
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

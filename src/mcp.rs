//! Minimal MCP client (stdio JSON-RPC, newline-delimited).
use crate::config::McpServerConf;
use crate::types::ToolSpec;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long a dying server gets to finish flushing stderr before the failure
/// reason is frozen. Servers that are actually exiting close the pipe well
/// inside this; a server that merely dropped stdin is not waited on for long.
const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// Clip an MCP tool result to [`crate::tools::MAX_OUT`].
///
/// MCP servers return screen dumps and document bodies, so this is the path that
/// most needs a ceiling (issue #6: an uncapped windows-mcp `Snapshot` put 107,816
/// chars / ~27k tokens into a single turn). Shares the helper with builtin tools
/// so the truncation marker is identical and no UTF-8 character is split.
pub fn cap_result(text: String) -> String {
    crate::tools::truncate_to(text, crate::tools::MAX_OUT)
}

struct Rpc {
    /// server id from config — used in every error message
    name: String,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next: AtomicU64,
    tx: mpsc::UnboundedSender<Value>,
    child: Mutex<Child>,
    /// last bytes of the server's stderr — included in failure status
    stderr_tail: Arc<StderrState>,
    /// Why the child's stdout closed, once it has (EOF or read error).
    ///
    /// Set *before* the pending map is drained, so a call that registers after
    /// the drain still sees it. Without this a crashed server leaves every
    /// `call()` waiting for its full timeout (issue #15: two
    /// `create_pull_request` calls silently blocked for 120 s each).
    dead: Arc<Mutex<Option<String>>>,
}

/// Bytes a dead stdio server has said on stderr so far.
///
/// The reader task fills `text` in 4 KB chunks and sets `eof` when the pipe
/// closes. Death is usually detected on the stdin/stdout side, which races the
/// stderr reader: a server that dies immediately has written its complaint but
/// the tail is still empty when the reason is composed
/// (`dead_server_is_reported_with_its_stderr` failed on CI exactly this way).
/// `settle` closes that gap by waiting for `eof` before the reason is frozen.
#[derive(Default)]
struct StderrState {
    text: Mutex<String>,
    eof: std::sync::atomic::AtomicBool,
}

impl StderrState {
    /// Wait up to `budget` for the stderr pipe to close, so the tail is complete.
    ///
    /// Polls the flag instead of waiting on a `Notify`: the reader can reach EOF
    /// *before* the failing task gets here, and `notify_waiters` would then be
    /// lost, costing a full `budget` on every death. Costs nothing when the pipe
    /// has already closed (`echo boom; exit` — the common case).
    async fn settle(&self, budget: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + budget;
        while !self.eof.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}

/// Human-readable "why did the transport die" text for a dead stdio server.
///
/// Includes the server's stderr tail when it said anything — a crash usually
/// explains itself there, and that is exactly what the caller cannot see when
/// the tool call just hangs.
fn dead_reason(why: &str, stderr: &StderrState) -> String {
    let tail = stderr.text.lock().map(|t| t.trim().to_string()).unwrap_or_default();
    if tail.is_empty() {
        why.to_string()
    } else {
        let tail: String = tail.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{why}; stderr: {tail}")
    }
}

/// Mark the transport dead and fail every waiting call with `reason`.
///
/// The dead flag is published first: `Rpc::call` re-checks it after inserting
/// into `pending`, so an insert that races this drain cannot be left stranded.
///
/// `stderr` is settled first (bounded) so the message carries the server's last
/// words even when death was detected before the stderr reader caught up.
async fn fail_pending(
    name: &str,
    pending: &Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    dead: &Mutex<Option<String>>,
    why: &str,
    stderr: &StderrState,
) {
    stderr.settle(STDERR_SETTLE).await;
    let reason = {
        let mut d = dead.lock().unwrap();
        if d.is_none() {
            *d = Some(dead_reason(why, stderr));
        }
        d.clone().unwrap_or_default()
    };
    let msg = format!("mcp server '{name}' is not running: {reason}");
    let waiting: Vec<_> = pending.lock().unwrap().drain().collect();
    for (_, tx) in waiting {
        let _ = tx.send(Err(msg.clone()));
    }
}

impl Rpc {
    fn spawn(conf: McpServerConf, name: &str) -> Result<Arc<Self>> {
        // Windows: bare names like `npx` resolve to .cmd shims CreateProcess
        // can't launch directly — resolve to a concrete path first.
        let prog = crate::catalog::resolve_spawn_command(&conf.command);
        let mut cmd = Command::new(&prog);
        crate::catalog::no_window_async(&mut cmd);
        cmd.args(&conf.args)
            .envs(&conf.env)
            // servers expect a sane cwd; amty may have been launched from a
            // UNC path (cmd/npm refuse to run with a UNC current dir)
            .current_dir(dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context(format!("spawn '{prog}'"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr_tail: Arc<StderrState> = Arc::new(StderrState::default());
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut rdr = BufReader::new(stderr);
                let mut buf = [0u8; 4096];
                while let Ok(n) = rdr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.text.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > 4096 {
                        let cut = t.len() - 4096;
                        t.drain(..cut);
                    }
                }
                // pipe closed: everything the server ever said is now in `text`
                tail.eof.store(true, Ordering::SeqCst);
            });
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let dead: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // writer task
        {
            let dead_w = dead.clone();
            let pending_w = pending.clone();
            let tail = stderr_tail.clone();
            let name_w = name.to_string();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(msg) = rx.recv().await {
                    let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        fail_pending(&name_w, &pending_w, &dead_w, "stdin closed", &tail).await;
                        break;
                    }
                }
            });
        }

        // reader task — also answers server→client requests
        let pending_r = pending.clone();
        let tx_r = tx.clone();
        let dead_r = dead.clone();
        let tail_r = stderr_tail.clone();
        let name_r = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        // EOF: the server exited (crash, kill, or a clean
                        // shutdown) — nothing will ever answer the calls that
                        // are still in flight, so fail them now instead of
                        // letting each one burn its full timeout.
                        fail_pending(&name_r, &pending_r, &dead_r, "server exited", &tail_r).await;
                        break;
                    }
                    Err(e) => {
                        fail_pending(&name_r, &pending_r, &dead_r, &e.to_string(), &tail_r).await;
                        break;
                    }
                };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let id = v.get("id").and_then(|i| i.as_u64());
                if v.get("method").is_some() {
                    // server→client request or notification
                    if let Some(id) = id {
                        let method = v["method"].as_str().unwrap_or("");
                        let result = match method {
                            "ping" => json!({}),
                            "roots/list" => json!({"roots": []}),
                            _ => {
                                let _ = tx_r.send(json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "error": {"code": -32601, "message": "not supported"},
                                }));
                                continue;
                            }
                        };
                        let _ = tx_r.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                } else if let Some(id) = id {
                    if let Some(tx) = pending_r.lock().unwrap().remove(&id) {
                        if let Some(err) = v.get("error") {
                            let _ = tx.send(Err(err["message"].as_str().unwrap_or("rpc error").into()));
                        } else {
                            let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            name: name.to_string(),
            pending,
            next: AtomicU64::new(1),
            tx,
            child: Mutex::new(child),
            stderr_tail,
            dead,
        }))
    }

    async fn call(&self, method: &str, params: Value, timeout_s: u64) -> Result<Value> {
        // fail fast on a server that already closed its pipe (issue #15)
        if let Some(why) = self.dead_reason() {
            bail!("mcp server '{}' is not running: {why}", self.name);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // the reader may have drained `pending` between the check above and the
        // insert — re-check so this call can't be stranded either
        if let Some(why) = self.dead_reason() {
            self.pending.lock().unwrap().remove(&id);
            bail!("mcp server '{}' is not running: {why}", self.name);
        }
        self.tx
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .map_err(|_| anyhow!("mcp server '{}' closed", self.name))?;
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("mcp server '{}' dropped the call", self.name),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("mcp '{method}' on server '{}' timed out after {timeout_s}s", self.name)
            }
        }
    }

    /// `Some(reason)` once the child's stdout closed (EOF or read error).
    fn dead_reason(&self) -> Option<String> {
        self.dead.lock().unwrap().clone()
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.start_kill();
        }
    }
}

/// Streamable-HTTP MCP transport (hosted servers). POST-per-request; the
/// response may be plain JSON or an SSE stream carrying the JSON-RPC reply.
struct HttpRpc {
    http: reqwest::Client,
    url: String,
    name: String,
    conf: McpServerConf,
    session: Mutex<Option<String>>,
}

fn sse_response(body: &str, want_id: u64) -> Option<Value> {
    for chunk in body.split("\n\n") {
        let mut data = String::new();
        for line in chunk.lines() {
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim());
            }
        }
        if data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
                return Some(v);
            }
        }
    }
    None
}

impl HttpRpc {
    fn new(name: String, conf: McpServerConf) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: conf.url.clone().unwrap_or_default(),
            name,
            conf,
            session: Mutex::new(None),
        }
    }

    async fn call(&self, method: &str, params: Value, timeout_s: u64) -> Result<Value> {
        use std::sync::atomic::Ordering as Ord2;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ord2::SeqCst);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.post(body, timeout_s).await?;
        let v = match resp {
            HttpResp::Json(v) => v,
            HttpResp::Sse(text) => sse_response(&text, id).context("no matching response in SSE stream")?,
        };
        if let Some(err) = v.get("error") {
            bail!("{}", err["message"].as_str().unwrap_or("rpc error"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str) {
        let _ = self
            .post(json!({"jsonrpc": "2.0", "method": method, "params": {}}), 10)
            .await;
    }

    async fn post(&self, body: Value, timeout_s: u64) -> Result<HttpResp> {
        let token = if self.conf.oauth_client_id.is_some() || self.conf.oauth_token_url.is_some() || crate::oauth::load(&self.name).is_some() {
            crate::oauth::access_token(&self.name, &self.conf).await?
        } else {
            None
        };
        let mut req = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .json(&body);
        if let Some(sid) = self.session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", sid);
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let fut = req.send();
        let resp = match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), fut).await {
            Ok(r) => r?,
            Err(_) => bail!("mcp '{method}' timed out", method = body["method"].as_str().unwrap_or("")),
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("HTTP 401 — authentication required (run auth for '{n}')", n = self.name);
        }
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::ACCEPTED {
            bail!("HTTP {}", resp.status());
        }
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(sid.to_string());
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if ctype.contains("text/event-stream") {
            Ok(HttpResp::Sse(resp.text().await?))
        } else if ctype.contains("json") {
            Ok(HttpResp::Json(resp.json::<Value>().await?))
        } else {
            Ok(HttpResp::Json(Value::Null))
        }
    }
}

enum HttpResp {
    Json(Value),
    Sse(String),
}

enum Transport {
    Stdio(Arc<Rpc>),
    Http(Arc<HttpRpc>),
}

impl Transport {
    async fn call(&self, method: &str, params: Value, timeout_s: u64) -> Result<Value> {
        match self {
            Transport::Stdio(r) => r.call(method, params, timeout_s).await,
            Transport::Http(r) => r.call(method, params, timeout_s).await,
        }
    }

    /// `Some(reason)` when the underlying child process is gone. Only stdio
    /// has a process; hosted HTTP servers recover on their own.
    fn dead_reason(&self) -> Option<String> {
        match self {
            Transport::Stdio(r) => r.dead_reason(),
            Transport::Http(_) => None,
        }
    }
}

struct Client {
    rpc: Transport,
    tools: Vec<(String, ToolSpec)>, // (original_name, spec with mcp__ prefix)
}

#[derive(Clone, Debug)]
pub enum Status {
    Disabled,
    Connecting,
    Connected(usize),
    Failed(String),
}

pub struct Manager {
    servers: Mutex<HashMap<String, McpServerConf>>,
    clients: Mutex<HashMap<String, Arc<Client>>>,
    status: Mutex<HashMap<String, Status>>,
}

impl Manager {
    pub fn new(servers: HashMap<String, McpServerConf>) -> Arc<Self> {
        Arc::new(Self {
            servers: Mutex::new(servers),
            clients: Mutex::new(HashMap::new()),
            status: Mutex::new(HashMap::new()),
        })
    }

    /// Sync server definitions from config; call after config changes.
    pub async fn reconcile(self: &Arc<Self>) {
        // caller updates `servers` via update_servers() first
        let names: Vec<String> = self.servers.lock().unwrap().keys().cloned().collect();
        for name in names {
            let enabled = self.servers.lock().unwrap().get(&name).map(|s| s.enabled).unwrap_or(false);
            let connected = self.clients.lock().unwrap().contains_key(&name);
            if enabled && !connected {
                self.connect(&name).await;
            } else if !enabled && connected {
                self.clients.lock().unwrap().remove(&name);
                self.status.lock().unwrap().insert(name, Status::Disabled);
            }
        }
    }

    pub fn update_servers(&self, servers: HashMap<String, McpServerConf>) {
        // drop clients whose conf changed or was removed/disabled
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|name, _| {
            servers.get(name).map(|s| s.enabled).unwrap_or(false)
        });
        // re-check conf equality: force reconnect when the def changed
        let defs = self.servers.lock().unwrap();
        let changed: Vec<String> = defs
            .iter()
            .filter(|(n, old)| servers.get(*n).map(|new| new != *old).unwrap_or(false))
            .map(|(n, _)| n.clone())
            .collect();
        drop(defs);
        for n in changed {
            clients.remove(&n);
        }
        *self.servers.lock().unwrap() = servers;
    }

    /// Connect all enabled servers (call at startup and lazily).
    pub async fn ensure_connected(self: &Arc<Self>) {
        let names: Vec<String> = {
            let servers = self.servers.lock().unwrap();
            let clients = self.clients.lock().unwrap();
            servers
                .iter()
                .filter(|(n, s)| s.enabled && !clients.contains_key(*n))
                .map(|(n, _)| n.clone())
                .collect()
        };
        let mut set = tokio::task::JoinSet::new();
        for name in names {
            let me = self.clone();
            set.spawn(async move { me.connect(&name).await; });
        }
        while set.join_next().await.is_some() {}
    }

    async fn connect(self: &Arc<Self>, name: &str) {
        let conf = match self.servers.lock().unwrap().get(name).cloned() {
            Some(c) if c.enabled => c,
            _ => return,
        };
        self.status.lock().unwrap().insert(name.into(), Status::Connecting);
        match Self::handshake(conf, name).await {
            Ok(client) => {
                let n = client.tools.len();
                self.clients.lock().unwrap().insert(name.into(), Arc::new(client));
                self.status.lock().unwrap().insert(name.into(), Status::Connected(n));
            }
            Err(e) => {
                self.status.lock().unwrap().insert(name.into(), Status::Failed(e.to_string()));
            }
        }
    }

    async fn handshake(conf: McpServerConf, name: &str) -> Result<Client> {
        // npx -y downloads the package on first run — allow ample time
        const INIT_TIMEOUT: u64 = 120;
        let rpc: Transport = if conf.url.is_some() {
            Transport::Http(Arc::new(HttpRpc::new(name.to_string(), conf.clone())))
        } else {
            Transport::Stdio(Rpc::spawn(conf.clone(), name)?)
        };
        // attach the server's stderr tail so config/setup errors are visible
        let with_err = |e: anyhow::Error| -> anyhow::Error {
            let tail = match &rpc {
                Transport::Stdio(r) => r.stderr_tail.text.lock().unwrap().trim().to_string(),
                _ => String::new(),
            };
            if tail.is_empty() {
                e
            } else {
                e.context(format!("stderr: {}", tail.chars().take(600).collect::<String>()))
            }
        };
        rpc.call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "amty", "version": env!("CARGO_PKG_VERSION")},
            }),
            INIT_TIMEOUT,
        )
        .await
        .map_err(with_err)?;
        match &rpc {
            Transport::Stdio(r) => {
                let _ = r.tx.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
            }
            Transport::Http(r) => r.notify("notifications/initialized").await,
        }
        let mut tools = vec![];
        let mut cursor = Value::Null;
        loop {
            let mut params = json!({});
            if !cursor.is_null() {
                params["cursor"] = cursor.clone();
            }
            let res = rpc.call("tools/list", params, 30).await.map_err(with_err)?;
            if let Some(list) = res["tools"].as_array() {
                for t in list {
                    let name = t["name"].as_str().unwrap_or("").to_string();
                    if name.is_empty() {
                        continue;
                    }
                    tools.push((
                        name.clone(),
                        ToolSpec {
                            name,
                            description: t["description"].as_str().unwrap_or("").to_string(),
                            schema: t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                        },
                    ));
                }
            }
            match res.get("nextCursor").and_then(|c| c.as_str()) {
                Some(c) => cursor = json!(c),
                None => break,
            }
        }
        Ok(Client { rpc, tools })
    }

    /// All MCP tool specs, namespaced as `mcp__<server>__<tool>`, sorted for a
    /// stable order — hash-map iteration would otherwise shuffle the tools
    /// prefix and defeat prompt caching.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut out = vec![];
        for (srv, client) in self.clients.lock().unwrap().iter() {
            for (orig, spec) in &client.tools {
                out.push(ToolSpec {
                    name: format!("mcp__{srv}__{}", spec.name),
                    description: format!("[mcp:{srv}] {}", spec.description),
                    schema: spec.schema.clone(),
                });
                let _ = orig;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Drop clients whose stdio child has died, recording why in `status`.
    ///
    /// Keeps the dead entry out of `clients` so the next `ensure_connected`
    /// (start of every run) reconnects, and makes the failure visible in
    /// `amty status` / the GUI instead of a stale `connected (n tools)`.
    fn prune_dead(&self) -> Vec<(String, String)> {
        let mut dead: Vec<(String, String)> = vec![];
        let mut clients = self.clients.lock().unwrap();
        let mut gone: Vec<String> = vec![];
        for (name, client) in clients.iter() {
            if let Some(why) = client.rpc.dead_reason() {
                dead.push((name.clone(), why));
                gone.push(name.clone());
            }
        }
        for name in &gone {
            clients.remove(name);
        }
        drop(clients);
        for (name, why) in &dead {
            self.status.lock().unwrap().insert(
                name.clone(),
                Status::Failed(format!("{why} (reconnecting on the next turn)")),
            );
        }
        dead
    }

    pub async fn call_tool(&self, namespaced: &str, input: Value) -> Result<String, String> {
        let rest = namespaced.strip_prefix("mcp__").ok_or("bad tool name")?;
        let (srv, tool) = rest.split_once("__").ok_or("bad tool name")?;
        // a crashed server must not silently cost the caller 120 s (issue #15)
        let dead = self.prune_dead();
        let client = self.clients.lock().unwrap().get(srv).cloned();
        let Some(client) = client else {
            return Err(match dead.iter().find(|(n, _)| n == srv) {
                Some((_, why)) => format!("mcp server '{srv}' is not running: {why}"),
                None => format!("mcp server '{srv}' not connected"),
            });
        };
        let orig = client
            .tools
            .iter()
            .find(|(_, s)| s.name == tool)
            .map(|(o, _)| o.clone())
            .unwrap_or_else(|| tool.to_string());
        let res = client
            .rpc
            .call("tools/call", json!({"name": orig, "arguments": input}), 120)
            .await
            .map_err(|e| e.to_string());
        // the call itself may have killed/revealed a dead child — prune now so
        // the status surface reflects it immediately
        self.prune_dead();
        let res = res?;

        let is_err = res["isError"].as_bool().unwrap_or(false);
        let text: String = res["content"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|b| match b["type"].as_str() {
                        Some("text") => Some(b["text"].as_str().unwrap_or("").to_string()),
                        Some("resource") => Some(b["resource"]["text"].as_str().unwrap_or("").to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| res.to_string());
        // Same ceiling builtin tools get (issue #6): without this an MCP server
        // can push an arbitrary amount into the context and the session log — a
        // windows-mcp Snapshot was measured at 107,816 chars (~27k tokens).
        let text = cap_result(text);
        if is_err {
            Err(text)
        } else {
            Ok(text)
        }
    }

    pub fn statuses(&self) -> Vec<(String, Status, bool)> {
        let servers = self.servers.lock().unwrap();
        let status = self.status.lock().unwrap();
        servers
            .iter()
            .map(|(n, c)| {
                (
                    n.clone(),
                    status.get(n).cloned().unwrap_or(if c.enabled { Status::Connecting } else { Status::Disabled }),
                    c.enabled,
                )
            })
            .collect()
    }

    pub fn status_text(&self) -> String {
        self.statuses()
            .into_iter()
            .map(|(n, s, en)| {
                let st = match s {
                    Status::Disabled => "disabled".into(),
                    Status::Connecting => "connecting".into(),
                    Status::Connected(n) => format!("connected ({n} tools)"),
                    Status::Failed(e) => format!("failed: {e}"),
                };
                format!("{n}: {st}{}", if en { "" } else { " [off]" })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tools_text(&self) -> String {
        let specs = self.specs();
        if specs.is_empty() {
            return "no mcp tools".into();
        }
        specs.iter().map(|s| format!("{}: {}", s.name, s.description)).collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_results_are_capped_like_builtin_tools() {
        // small results are untouched
        assert_eq!(cap_result("ok".to_string()), "ok");
        // oversized results are clipped, marked, and keep their head
        let big = "x".repeat(crate::tools::MAX_OUT + 5_000);
        let out = cap_result(big.clone());
        assert!(out.len() < big.len());
        assert!(out.contains("[truncated"));
        assert!(out.starts_with("xxx"), "keeps the head where the summary lives");
        // a multi-byte character must never be sliced in half
        let jp = "あ".repeat((crate::tools::MAX_OUT / 3) + 100);
        let out = cap_result(jp);
        assert!(out.contains("[truncated"));
        assert!(out.is_char_boundary(out.find('…').unwrap()));
    }

    #[test]
    fn sse_response_finds_matching_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[1,2]}}\n\n";
        let v = sse_response(body, 7).unwrap();
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 2);
        assert!(sse_response(body, 8).is_none());
        // multi-line data + unrelated events
        let body2 = "event: ping\n\n: comment\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\ndata: {\"id\":2}\n\n";
        assert!(sse_response(body2, 1).is_some());
        assert!(sse_response(body2, 2).is_some());
        assert!(sse_response(body2, 3).is_none());
    }

    fn stdio_conf(script: &str) -> McpServerConf {
        McpServerConf {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        }
    }

    /// Issue #15: a stdio server that exits immediately used to leave every
    /// `call()` blocked until its own timeout (120 s for `tools/call`). The
    /// reader task must fail the pending calls instead.
    #[tokio::test]
    async fn call_fails_fast_when_the_server_exits() {
        // `sh -c exit 0` — a child that closes stdout straight away, like a
        // crashed MCP server. The assertion below is the regression: it used to
        // wait the full 120 s.
        let rpc = Rpc::spawn(stdio_conf("exit 0"), "stub").expect("spawn");
        let started = std::time::Instant::now();
        let err = rpc
            .call("tools/call", json!({}), 15)
            .await
            .expect_err("a dead server must not answer");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(10),
            "call waited {waited:?} — the dead-transport handshake is missing"
        );
        let msg = err.to_string();
        assert!(msg.contains("stub"), "error should name the server: {msg}");
        assert!(
            msg.contains("not running") || msg.contains("exited") || msg.contains("closed"),
            "error should say why: {msg}"
        );
        // and it stays fast on every later call (no re-wait)
        let t2 = std::time::Instant::now();
        assert!(rpc.call("tools/call", json!({}), 120).await.is_err());
        assert!(t2.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn dead_server_is_reported_with_its_stderr() {
        // a server that complains on stderr and exits: the tail must reach the
        // caller, since otherwise the failure is invisible
        let rpc = Rpc::spawn(stdio_conf("echo 'boom: bad token' >&2; exit 1"), "noisy").expect("spawn");
        let err = rpc
            .call("tools/call", json!({}), 30)
            .await
            .expect_err("dead server");
        let msg = err.to_string();
        assert!(msg.contains("noisy"), "{msg}");
        assert!(msg.contains("boom: bad token"), "stderr tail missing: {msg}");
    }
}

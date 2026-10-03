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

struct Rpc {
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next: AtomicU64,
    tx: mpsc::UnboundedSender<Value>,
    child: Mutex<Child>,
    /// last bytes of the server's stderr — included in failure status
    stderr_tail: Arc<Mutex<String>>,
}

impl Rpc {
    fn spawn(conf: McpServerConf) -> Result<Arc<Self>> {
        let mut cmd = Command::new(&conf.command);
        cmd.args(&conf.args)
            .envs(&conf.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context(format!("spawn '{}'", conf.command))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
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
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > 4096 {
                        let cut = t.len() - 4096;
                        t.drain(..cut);
                    }
                }
            });
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // writer task
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = rx.recv().await {
                let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // reader task — also answers server→client requests
        let pending_r = pending.clone();
        let tx_r = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
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

        Ok(Arc::new(Self { pending, next: AtomicU64::new(1), tx, child: Mutex::new(child), stderr_tail }))
    }

    async fn call(&self, method: &str, params: Value, timeout_s: u64) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.tx
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .map_err(|_| anyhow!("mcp server closed"))?;
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("mcp server dropped the call"),
            Err(_) => bail!("mcp '{method}' timed out"),
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.start_kill();
        }
    }
}

struct Client {
    rpc: Arc<Rpc>,
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
        match Self::handshake(conf).await {
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

    async fn handshake(conf: McpServerConf) -> Result<Client> {
        // npx -y downloads the package on first run — allow ample time
        const INIT_TIMEOUT: u64 = 120;
        let rpc = Rpc::spawn(conf)?;
        // attach the server's stderr tail so config/setup errors are visible
        let with_err = |e: anyhow::Error| -> anyhow::Error {
            let tail = rpc.stderr_tail.lock().unwrap().trim().to_string();
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
        let _ = rpc.tx.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
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

    /// All MCP tool specs, namespaced as `mcp__<server>__<tool>`.
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
        out
    }

    pub async fn call_tool(&self, namespaced: &str, input: Value) -> Result<String, String> {
        let rest = namespaced.strip_prefix("mcp__").ok_or("bad tool name")?;
        let (srv, tool) = rest.split_once("__").ok_or("bad tool name")?;
        let client = self.clients.lock().unwrap().get(srv).cloned();
        let Some(client) = client else {
            return Err(format!("mcp server '{srv}' not connected"));
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
            .map_err(|e| e.to_string())?;
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

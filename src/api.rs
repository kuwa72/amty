//! Local control API — the same surface the CLI and `amty mcp-serve` use.
use crate::agent;
use crate::app::App;
use crate::types::{AppEvent, EvKind};
use axum::extract::{Path, State};
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/sessions", get(list_sessions).post(new_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/messages", post(send_message))
        .route("/v1/sessions/{id}/cancel", post(cancel))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/config", get(get_config).post(set_config))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}", post(resolve_approval))
        .route("/v1/mcp", get(mcp_status).post(mcp_add))
        .route("/v1/mcp/{name}", post(mcp_toggle))
        .route("/v1/mcp-catalog", get(mcp_catalog))
        .route("/v1/mcp-install", post(mcp_install))
        .route("/v1/mcp-auth", post(mcp_auth))
        .route("/v1/mcp-auth/{id}", get(mcp_auth_status))
        .route("/v1/mcp-keys", post(mcp_keys))
        .route("/v1/skills", get(skills_list))
        .route("/v1/skills/{name}", delete(skill_remove))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}

async fn auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {}", app.token))
        .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg.read().unwrap();
    Json(json!({
        "name": "amty",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": crate::session::now_secs() - app.started,
        "provider": cfg.provider,
        "model": cfg.active().map(|p| p.model.clone()).unwrap_or_default(),
        "approval": format!("{:?}", cfg.approval).to_lowercase(),
        "providers": cfg.providers.keys().cloned().collect::<Vec<_>>(),
        "sessions": app.sessions.list().len(),
        "mcp": app.mcp.statuses().iter().map(|(n, s, en)| {
            let st = match s {
                crate::mcp::Status::Disabled => "disabled".to_string(),
                crate::mcp::Status::Connecting => "connecting".to_string(),
                crate::mcp::Status::Connected(n) => format!("connected:{n}"),
                crate::mcp::Status::Failed(e) => format!("failed:{e}"),
            };
            json!({"name": n, "status": st, "enabled": en})
        }).collect::<Vec<_>>(),
    }))
}

async fn list_sessions(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(app.sessions.list()))
}

#[derive(Deserialize)]
struct NewSession {
    provider: Option<String>,
    model: Option<String>,
    title: Option<String>,
}

async fn new_session(State(app): State<Arc<App>>, body: Option<Json<NewSession>>) -> Result<Json<Value>, (StatusCode, String)> {
    let b = body.map(|x| x.0).unwrap_or(NewSession { provider: None, model: None, title: None });
    let id = app.sessions.create(b.provider, b.model);
    if let Some(t) = b.title {
        let mut map = app.sessions.map.write().unwrap();
        if let Some(s) = map.get_mut(&id) {
            s.title = t;
        }
    }
    app.emit(&id, EvKind::Touched);
    Ok(Json(json!({"id": id})))
}

async fn get_session(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = app.sessions.resolve(&id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?;
    let s = app.sessions.get(&sid).ok_or((StatusCode::NOT_FOUND, "gone".into()))?;
    Ok(Json(json!(s)))
}

async fn delete_session(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = app.sessions.resolve(&id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?;
    app.sessions.delete(&sid);
    app.emit(&sid, EvKind::Touched);
    Ok(Json(json!({"deleted": sid})))
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
    #[serde(default)]
    wait: Option<bool>,
}

async fn send_message(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<SendBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = app.sessions.resolve(&id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?;
    let wait = body.wait.unwrap_or(false);

    if wait {
        let mut rx = app.events.subscribe();
        agent::start_run(&app, &sid, body.text).map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
        let mut text = String::new();
        loop {
            match rx.recv().await {
                Ok(AppEvent { session, kind }) if session == sid => match kind {
                    EvKind::Text { text: t } => text.push_str(&t),
                    EvKind::Done => return Ok(Json(json!({"session": sid, "text": text}))),
                    EvKind::Error { message } => {
                        return Ok(Json(json!({"session": sid, "text": text, "error": message})))
                    }
                    _ => {}
                },
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Err((StatusCode::INTERNAL_SERVER_ERROR, "event bus closed".into())),
            }
        }
    } else {
        agent::start_run(&app, &sid, body.text).map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
        Ok(Json(json!({"session": sid, "accepted": true})))
    }
}

async fn cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> Json<Value> {
    let sid = app.sessions.resolve(&id).unwrap_or(id);
    Json(json!({"cancelled": app.cancel(&sid)}))
}

async fn events(State(app): State<Arc<App>>, Path(id): Path<String>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let sid = app.sessions.resolve(&id).unwrap_or(id);
    let rx = app.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |r| match r {
        Ok(ev) if ev.session == sid || ev.session.is_empty() => {
            Some(Ok(Event::default().json_data(&ev).unwrap_or_else(|_| Event::default().data("{}"))))
        }
        _ => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg.read().unwrap().clone();
    let mut v = serde_json::to_value(&cfg).unwrap_or(json!({}));
    // mask api keys
    if let Some(providers) = v.get_mut("providers").and_then(|p| p.as_object_mut()) {
        for (_, p) in providers.iter_mut() {
            if p.get("api_key").map(|k| !k.is_null()).unwrap_or(false) {
                p["api_key"] = json!("***");
            }
        }
    }
    Json(v)
}

#[derive(Deserialize)]
struct SetConfig {
    key: String,
    value: String,
}

async fn set_config(State(app): State<Arc<App>>, Json(b): Json<SetConfig>) -> Result<Json<Value>, (StatusCode, String)> {
    let msg = {
        let mut cfg = app.cfg.write().unwrap();
        let msg = cfg.apply_set(&b.key, &b.value).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        cfg.save(&app.cfg_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        msg
    };
    if b.key.starts_with("mcp.") {
        app.mcp.update_servers(app.mcp_servers_map());
        app.mcp.reconcile().await;
    }
    app.emit("", EvKind::Touched);
    Ok(Json(json!({"ok": msg})))
}

async fn list_approvals(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(app.approvals.list()))
}

#[derive(Deserialize)]
struct ResolveBody {
    allow: bool,
}

async fn resolve_approval(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<ResolveBody>,
) -> Json<Value> {
    let ok = app.approvals.resolve(&id, b.allow);
    Json(json!({"resolved": ok}))
}

async fn mcp_status(State(app): State<Arc<App>>) -> Json<Value> {
    let rows: Vec<Value> = app
        .mcp
        .statuses()
        .iter()
        .map(|(n, s, en)| {
            let st = match s {
                crate::mcp::Status::Disabled => "disabled".to_string(),
                crate::mcp::Status::Connecting => "connecting".to_string(),
                crate::mcp::Status::Connected(n) => format!("connected:{n}"),
                crate::mcp::Status::Failed(e) => format!("failed:{e}"),
            };
            json!({"name": n, "status": st, "enabled": en})
        })
        .collect();
    Json(json!(rows))
}

#[derive(Deserialize)]
struct AddMcp {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

/// Add (or replace) an MCP server definition — same fields as
/// claude_desktop_config.json entries.
async fn mcp_add(
    State(app): State<Arc<App>>,
    Json(b): Json<AddMcp>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if b.name.is_empty() || b.command.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name and command required".into()));
    }
    {
        let mut cfg = app.cfg.write().unwrap();
        cfg.mcp_servers.insert(
            b.name.clone(),
            crate::config::McpServerConf {
                command: b.command,
                args: b.args,
                env: b.env,
                enabled: true,
                ..Default::default()
            },
        );
        cfg.save(&app.cfg_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    app.mcp.update_servers(app.mcp_servers_map());
    app.mcp.reconcile().await;
    Ok(Json(json!({"added": b.name})))
}

#[derive(Deserialize)]
struct ToggleBody {
    enabled: bool,
}

async fn mcp_catalog(State(app): State<Arc<App>>) -> Json<Value> {
    let installed = app.cfg.read().unwrap().mcp_servers.clone();
    Json(json!({
        "node_available": crate::catalog::node_available(),
        "entries": crate::catalog::rows(&installed),
    }))
}

#[derive(Deserialize)]
struct InstallBody {
    id: String,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    /// override the server name in config (default: catalog id)
    name: Option<String>,
}

async fn mcp_install(
    State(app): State<Arc<App>>,
    Json(b): Json<InstallBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (def_name, conf) = crate::catalog::build_conf(&b.id, b.env)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let name = b.name.unwrap_or(def_name);
    {
        let mut cfg = app.cfg.write().unwrap();
        cfg.mcp_servers.insert(name.clone(), conf);
        cfg.save(&app.cfg_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    app.mcp.update_servers(app.mcp_servers_map());
    app.mcp.reconcile().await;
    let status = app
        .mcp
        .statuses()
        .into_iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, s, _)| format!("{s:?}"))
        .unwrap_or_default();
    app.emit("", EvKind::Touched);
    Ok(Json(json!({"installed": name, "status": status})))
}

#[derive(Deserialize)]
struct AuthBody {
    id: String,
}

/// Kick off a catalog entry's OAuth/auth subcommand (browser may open).
async fn mcp_auth(
    State(app): State<Arc<App>>,
    Json(b): Json<AuthBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let log = app.start_auth(&b.id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({"started": b.id, "log": log.to_string_lossy()})))
}

async fn mcp_auth_status(State(app): State<Arc<App>>, Path(id): Path<String>) -> Json<Value> {
    let running = app.auth.lock().unwrap().get(&id).map(|a| a.running).unwrap_or(false);
    let tail = app.auth_log_tail(&id, 2000).unwrap_or_default();
    Json(json!({"id": id, "running": running, "log_tail": tail}))
}

#[derive(Deserialize)]
struct KeysBody {
    id: String,
    src: String,
}

/// Copy a downloaded OAuth keys file to where the catalog server expects it.
async fn mcp_keys(
    State(app): State<Arc<App>>,
    Json(b): Json<KeysBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let dst = crate::catalog::place_keys(&b.id, std::path::Path::new(&b.src))
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    app.mcp.update_servers(app.mcp_servers_map());
    app.mcp.reconcile().await;
    Ok(Json(json!({"placed": dst.to_string_lossy()})))
}

/// List skills from amty's own dir plus shared skill dirs (claude/agents/devin).
async fn skills_list() -> Json<Value> {
    let skills: Vec<Value> = crate::config::skills()
        .iter()
        .map(|s| json!({"name": s.name, "desc": s.desc, "path": s.path, "managed": s.managed}))
        .collect();
    Json(json!({"skills": skills}))
}

/// Delete a skill from amty's own skills dir only; shared dirs are read-only.
async fn skill_remove(Path(name): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid skill name".into()));
    }
    let dir = crate::config::skills_dir().join(&name);
    if !dir.is_dir() {
        return Err((StatusCode::NOT_FOUND, "skill not found in amty's own dir (shared dirs are read-only)".into()));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"removed": name})))
}

async fn mcp_toggle(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<ToggleBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    {
        let mut cfg = app.cfg.write().unwrap();
        let s = cfg.mcp_servers.get_mut(&name).ok_or((StatusCode::NOT_FOUND, "no such server".into()))?;
        s.enabled = b.enabled;
        cfg.save(&app.cfg_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    app.mcp.update_servers(app.mcp_servers_map());
    app.mcp.reconcile().await;
    Ok(Json(json!({"ok": true})))
}

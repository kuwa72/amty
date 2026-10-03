//! `amty <subcommand>` — talks to the running instance over the local API.
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};

pub fn runtime_info() -> Result<(String, String)> {
    let path = crate::config::runtime_path();
    let text = std::fs::read_to_string(&path).context("amty is not running (no runtime.json)")?;
    let v: Value = serde_json::from_str(&text)?;
    let port = v["port"].as_u64().ok_or_else(|| anyhow!("bad runtime.json"))?;
    let token = v["token"].as_str().unwrap_or("").to_string();
    Ok((format!("http://127.0.0.1:{port}"), token))
}

pub fn client(token: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            h
        })
        .build()
        .unwrap()
}

async fn err_body(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // error bodies may be plain text (axum tuple errors) or json
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => bail!("HTTP {status}: {v}"),
            Err(_) => bail!("HTTP {status}: {text}"),
        }
    }
    Ok(serde_json::from_str(&text).unwrap_or(json!(null)))
}

pub async fn get(base: &str, token: &str, path: &str) -> Result<Value> {
    let resp = client(token).get(format!("{base}{path}")).send().await?;
    err_body(resp).await
}

pub async fn post(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    let resp = client(token).post(format!("{base}{path}")).json(&body).send().await?;
    err_body(resp).await
}

pub async fn delete(base: &str, token: &str, path: &str) -> Result<Value> {
    let resp = client(token).delete(format!("{base}{path}")).send().await?;
    Ok(resp.json().await.unwrap_or(json!(null)))
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

pub async fn run(cmd: crate::Cmd) -> Result<()> {
    use crate::Cmd::*;
    match cmd {
        Gui | McpServe | ImportClaude => unreachable!(),
        Status => {
            let (base, tok) = runtime_info()?;
            print_json(&get(&base, &tok, "/v1/status").await?);
        }
        Sessions => {
            let (base, tok) = runtime_info()?;
            let v = get(&base, &tok, "/v1/sessions").await?;
            if let Some(arr) = v.as_array() {
                for s in arr {
                    println!(
                        "{}\t{}\t{} msgs\t{}",
                        s["id"].as_str().unwrap_or(""),
                        s["title"].as_str().unwrap_or(""),
                        s["n_messages"],
                        s["provider"].as_str().unwrap_or("default"),
                    );
                }
            }
        }
        New { provider, model } => {
            let (base, tok) = runtime_info()?;
            let v = post(&base, &tok, "/v1/sessions", json!({"provider": provider, "model": model})).await?;
            println!("{}", v["id"].as_str().unwrap_or(""));
        }
        Send { text, session, new, no_wait, provider, model } => {
            let (base, tok) = runtime_info()?;
            let sid = if new || provider.is_some() || model.is_some() {
                let v = post(&base, &tok, "/v1/sessions", json!({"provider": provider, "model": model})).await?;
                v["id"].as_str().unwrap_or("").to_string()
            } else if let Some(s) = session {
                s
            } else {
                let v = get(&base, &tok, "/v1/sessions").await?;
                match v[0]["id"].as_str() {
                    Some(id) => id.to_string(),
                    None => {
                        let v = post(&base, &tok, "/v1/sessions", json!({})).await?;
                        v["id"].as_str().unwrap_or("").to_string()
                    }
                }
            };
            send_and_print(&base, &tok, &sid, text.join(" "), !no_wait).await?;
        }
        Show { session, last } => {
            let (base, tok) = runtime_info()?;
            let v = get(&base, &tok, &format!("/v1/sessions/{}", session.unwrap_or_default())).await?;
            let msgs = v["messages"].as_array().cloned().unwrap_or_default();
            for m in msgs.iter().rev().take(last).rev() {
                let role = m["role"].as_str().unwrap_or("?");
                for b in m["blocks"].as_array().cloned().unwrap_or_default() {
                    match b["type"].as_str() {
                        Some("text") => println!("[{role}] {}", b["text"].as_str().unwrap_or("")),
                        Some("tool_use") => println!("[{role}] >> {} {}", b["name"].as_str().unwrap_or(""), b["input"]),
                        Some("tool_result") => println!("[tool] {}", preview(b["content"].as_str().unwrap_or(""))),
                        _ => {}
                    }
                }
            }
        }
        Delete { session } => {
            let (base, tok) = runtime_info()?;
            print_json(&delete(&base, &tok, &format!("/v1/sessions/{session}")).await?);
        }
        Events { session } => {
            let (base, tok) = runtime_info()?;
            let resp = client(&tok)
                .get(format!("{base}/v1/sessions/{}/events", session.unwrap_or_default()))
                .send()
                .await?;
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                buf.push_str(&String::from_utf8_lossy(&chunk?));
                while let Some(pos) = buf.find('\n') {
                    let line: String = buf.drain(..=pos).collect();
                    let line = line.trim();
                    if let Some(data) = line.strip_prefix("data:") {
                        println!("{}", data.trim());
                    }
                }
            }
        }
        Cancel { session } => {
            let (base, tok) = runtime_info()?;
            print_json(&post(&base, &tok, &format!("/v1/sessions/{}/cancel", session.unwrap_or_default()), json!({})).await?);
        }
        Approvals => {
            let (base, tok) = runtime_info()?;
            print_json(&get(&base, &tok, "/v1/approvals").await?);
        }
        Approve { id } => {
            let (base, tok) = runtime_info()?;
            print_json(&post(&base, &tok, &format!("/v1/approvals/{id}"), json!({"allow": true})).await?);
        }
        Deny { id, reason } => {
            let (base, tok) = runtime_info()?;
            let _ = reason;
            print_json(&post(&base, &tok, &format!("/v1/approvals/{id}"), json!({"allow": false})).await?);
        }
        Config { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                crate::ConfigCmd::Get { key: Some(k) } => {
                    let v = get(&base, &tok, "/v1/config").await?;
                    let mut cur = v.clone();
                    for p in k.split('.') {
                        cur = cur.get(p).cloned().unwrap_or(Value::Null);
                    }
                    print_json(&cur);
                }
                crate::ConfigCmd::Get { key: None } => {
                    print_json(&get(&base, &tok, "/v1/config").await?);
                }
                crate::ConfigCmd::Set { key, value } => {
                    print_json(&post(&base, &tok, "/v1/config", json!({"key": key, "value": value})).await?);
                }
            }
        }
        Mcp => {
            let (base, tok) = runtime_info()?;
            print_json(&get(&base, &tok, "/v1/mcp").await?);
        }
        Catalog => {
            let (base, tok) = runtime_info()?;
            let v = get(&base, &tok, "/v1/mcp-catalog").await?;
            if !v["node_available"].as_bool().unwrap_or(false) {
                eprintln!("note: Node.js (npx) not found on PATH");
            }
            if let Some(arr) = v["entries"].as_array() {
                for e in arr {
                    let mark = if e["installed"].as_bool().unwrap_or(false) { "✓" } else { " " };
                    println!("{mark} {:10} {}\n    pkg: npx -y {}", e["id"].as_str().unwrap_or(""), e["label"].as_str().unwrap_or(""), e["package"].as_str().unwrap_or(""));
                    if let Some(envs) = e["env"].as_array() {
                        for ev in envs {
                            let req = if ev["required"].as_bool().unwrap_or(false) { " (required)" } else { "" };
                            println!("    env: {}{}", ev["key"].as_str().unwrap_or(""), req);
                        }
                    }
                }
            }
        }
        Install { id, env } => {
            let (base, tok) = runtime_info()?;
            let env_map: serde_json::Map<String, Value> = env
                .iter()
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            print_json(&post(&base, &tok, "/v1/mcp-install", json!({"id": id, "env": env_map})).await?);
        }
    }
    Ok(())
}

async fn send_and_print(base: &str, tok: &str, sid: &str, text: String, wait: bool) -> Result<()> {
    let v = post(base, tok, &format!("/v1/sessions/{sid}/messages"), json!({"text": text, "wait": wait})).await?;
    if wait {
        if let Some(e) = v["error"].as_str() {
            eprintln!("error: {e}");
        }
        println!("{}", v["text"].as_str().unwrap_or(""));
    } else {
        print_json(&v);
    }
    Ok(())
}

fn preview(s: &str) -> String {
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s.to_string()
    }
}

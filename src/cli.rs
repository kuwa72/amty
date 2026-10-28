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

pub async fn put(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    let resp = client(token).put(format!("{base}{path}")).json(&body).send().await?;
    err_body(resp).await
}

pub async fn patch(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    let resp = client(token).patch(format!("{base}{path}")).json(&body).send().await?;
    err_body(resp).await
}

/// Percent-encode a note name for a URL path, keeping `/` separators intact.
fn note_url(name: &str) -> String {
    name.split('/')
        .map(url_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// Percent-encode a query-string value (unreserved characters kept verbatim).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub async fn run(cmd: crate::Cmd) -> Result<()> {
    use crate::Cmd::*;
    match cmd {
        Gui | McpServe | ImportClaude | SelfSwap { .. } => unreachable!(),
        Status => {
            let (base, tok) = runtime_info()?;
            print_json(&get(&base, &tok, "/v1/status").await?);
        }
        Sessions => {
            let (base, tok) = runtime_info()?;
            let v = get(&base, &tok, "/v1/sessions").await?;
            if let Some(arr) = v.as_array() {
                for s in arr {
                    let u = &s["usage"];
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let toks = n("input") + n("output") + n("cache_read") + n("cache_write");
                    println!(
                        "{}\t{}\t{} msgs\t{}\t{} tok\t(in {} / out {} / cache {})",
                        s["id"].as_str().unwrap_or(""),
                        s["title"].as_str().unwrap_or(""),
                        s["n_messages"],
                        s["provider"].as_str().unwrap_or("default"),
                        toks,
                        n("input"),
                        n("output"),
                        n("cache_read"),
                    );
                }
            }
        }
        Search { query, session, limit, json: as_json } => {
            let (base, tok) = runtime_info()?;
            let q = query.join(" ");
            if q.trim().is_empty() {
                bail!("usage: amty search <query> [-s <session>] [--limit N]");
            }
            let mut path = format!("/v1/search?q={}&limit={limit}", url_encode(&q));
            if let Some(sid) = session.as_deref().filter(|s| !s.is_empty()) {
                path.push_str(&format!("&session={}", url_encode(sid)));
            }
            let v = get(&base, &tok, &path).await?;
            if as_json {
                print_json(&v);
            } else {
                let hits = v["hits"].as_array().cloned().unwrap_or_default();
                for h in &hits {
                    let idx = h["msg_index"].as_u64().map(|i| format!("#{i}")).unwrap_or_default();
                    println!(
                        "{}\t{}\t{:<9}{}\t{}",
                        h["id"].as_str().unwrap_or(""),
                        crate::session::fmt_stamp(h["updated"].as_u64().unwrap_or(0)),
                        h["role"].as_str().unwrap_or(""),
                        idx,
                        h["snippet"].as_str().unwrap_or(""),
                    );
                }
                let n = v["n"].as_u64().unwrap_or(0);
                eprintln!("\n{n} hit(s) for \"{q}\"");
            }
        }
        Usage { session } => {
            let (base, tok) = runtime_info()?;
            match session {
                // per-session breakdown when one is named
                Some(sid) => {
                    let v = get(&base, &tok, "/v1/sessions").await?;
                    let found = v.as_array().and_then(|a| {
                        a.iter().find(|s| {
                            s["id"].as_str().map(|i| i.starts_with(&sid)).unwrap_or(false)
                        }).cloned()
                    });
                    match found {
                        Some(s) => {
                            let u = &s["usage"];
                            let g = |k: &str| u[k].as_u64().unwrap_or(0);
                            let (i, o, cr, cw, rs) =
                                (g("input"), g("output"), g("cache_read"), g("cache_write"), g("reasoning"));
                            println!("session {}  {}", s["id"].as_str().unwrap_or(""), s["title"].as_str().unwrap_or(""));
                            println!("messages           {}", s["n_messages"]);
                            println!("input              {i}");
                            println!("  of which cached  {cr}");
                            println!("  uncached         {}", i.saturating_sub(cr));
                            println!("output             {o}");
                            println!("reasoning          {rs}");
                            println!("cache_write        {cw}");
                            if i > 0 {
                                println!("cache hit rate     {:.1}%", 100.0 * cr as f64 / i as f64);
                            }
                        }
                        None => bail!("no session matching '{sid}'"),
                    }
                }
                // otherwise every session, then the grand total
                None => {
                    let v = get(&base, &tok, "/v1/sessions").await?;
                    let arr = v.as_array().cloned().unwrap_or_default();
                    let mut ti = 0u64; let mut to = 0u64; let mut tcr = 0u64;
                    let mut tcw = 0u64; let mut tr = 0u64;
                    let mut n_with = 0usize;
                    for s in &arr {
                        let u = &s["usage"];
                        let g = |k: &str| u[k].as_u64().unwrap_or(0);
                        let (i, o, cr, cw, rs) =
                            (g("input"), g("output"), g("cache_read"), g("cache_write"), g("reasoning"));
                        if i + o + cr + cw == 0 {
                            continue;
                        }
                        n_with += 1;
                        ti += i; to += o; tcr += cr; tcw += cw; tr += rs;
                        println!(
                            "{}\t{}\tin {}\tout {}\tcache_read {}\treasoning {}",
                            s["id"].as_str().unwrap_or(""),
                            s["title"].as_str().unwrap_or(""),
                            i, o, cr, rs,
                        );
                    }
                    println!("\n{n_with} of {} session(s) have usage", arr.len());
                    println!("total input        {}", ti);
                    println!("  of which cached  {}", tcr);
                    println!("  uncached         {}", ti.saturating_sub(tcr));
                    println!("total output       {}", to);
                    println!("total reasoning    {}", tr);
                    println!("total cache_write  {}", tcw);
                    println!("total tokens       {}", ti + to + tcw);
                    if ti > 0 {
                        // cache_read is a *subset* of input (both include the
                        // cached prefix), so the rate is against input only.
                        println!("cache hit rate     {:.1}%", 100.0 * tcr as f64 / ti as f64);
                    }
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
        Files { session } => {
            let (base, tok) = runtime_info()?;
            // empty id = most recent session (the API route needs a real
            // segment, so resolve it client-side)
            let sid = match session.filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => {
                    let v = get(&base, &tok, "/v1/sessions").await?;
                    v[0]["id"].as_str().unwrap_or_default().to_string()
                }
            };
            let v = get(&base, &tok, &format!("/v1/sessions/{sid}/files")).await?;
            for f in v["files"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}\t{:<8}\t{}",
                    crate::session::fmt_stamp(f["ts"].as_u64().unwrap_or(0)),
                    f["tool"].as_str().unwrap_or(""),
                    f["path"].as_str().unwrap_or(""),
                );
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
                    // `apply_set` accepts the documented `provider.<name>.<field>`
                    // alias, but the serialized struct calls the map `providers`.
                    // Translate here so `set` and `get` accept the same spelling
                    // (previously `amty config get provider.x.model` returned
                    // null while the value was set — a silent lie).
                    let k = match k.strip_prefix("provider.") {
                        Some(rest) => format!("providers.{rest}"),
                        None => k,
                    };
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
                crate::ConfigCmd::Unset { key } => {
                    print_json(&delete(&base, &tok, &format!("/v1/config/{key}")).await?);
                }
            }
        }
        Mcp { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                None => print_json(&get(&base, &tok, "/v1/mcp").await?),
                Some(crate::McpCmd::Remove { name }) => {
                    print_json(&delete(&base, &tok, &format!("/v1/mcp/{name}")).await?)
                }
            }
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
        Keys { id, src } => {
            let (base, tok) = runtime_info()?;
            print_json(&post(&base, &tok, "/v1/mcp-keys", json!({"id": id, "src": src})).await?);
        }
        Skills { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                None => {
                    let v = get(&base, &tok, "/v1/skills").await?;
                    if let Some(arr) = v["skills"].as_array() {
                        for s in arr {
                            let name = s["name"].as_str().unwrap_or("");
                            let managed = s["managed"].as_bool().unwrap_or(false);
                            println!("{} {:24} {}", if managed { "*" } else { " " }, name, s["desc"].as_str().unwrap_or(""));
                            println!("      {}", s["path"].as_str().unwrap_or(""));
                        }
                        println!("(* = managed under {}; others are shared/read-only)", crate::config::skills_dir().display());
                    }
                }
                Some(crate::SkillCmd::Remove { name }) => {
                    print_json(&delete(&base, &tok, &format!("/v1/skills/{name}")).await?)
                }
            }
        }
        Notes { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                None => {
                    let v = get(&base, &tok, "/v1/notes").await?;
                    println!("{}", v["dir"].as_str().unwrap_or(""));
                    if let Some(arr) = v["notes"].as_array() {
                        for n in arr {
                            println!(
                                "{}\t{} B\t{}\t{}",
                                n["name"].as_str().unwrap_or(""),
                                n["bytes"].as_u64().unwrap_or(0),
                                crate::session::fmt_stamp(n["updated"].as_u64().unwrap_or(0)),
                                n["title"].as_str().unwrap_or(""),
                            );
                        }
                    }
                }
                Some(crate::NoteCmd::Read { name }) => {
                    let v = get(&base, &tok, &format!("/v1/notes/{}", note_url(&name))).await?;
                    print!("{}", v["content"].as_str().unwrap_or(""));
                }
                Some(crate::NoteCmd::Write { name, content }) => {
                    let text = if content.is_empty() {
                        // no inline content → take the note from stdin
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf)?;
                        buf
                    } else {
                        content.join(" ")
                    };
                    print_json(&put(&base, &tok, &format!("/v1/notes/{}", note_url(&name)), json!({"content": text})).await?);
                }
                Some(crate::NoteCmd::Remove { name }) => {
                    print_json(&delete(&base, &tok, &format!("/v1/notes/{}", note_url(&name))).await?);
                }
                Some(crate::NoteCmd::Search { query, limit, json: as_json }) => {
                    let q = query.join(" ");
                    if q.trim().is_empty() {
                        bail!("usage: amty notes search <query> [--limit N]");
                    }
                    let v = get(&base, &tok, &format!("/v1/notes-search?q={}&limit={limit}", url_encode(&q))).await?;
                    if as_json {
                        print_json(&v);
                    } else {
                        for h in v["hits"].as_array().cloned().unwrap_or_default() {
                            println!(
                                "{}\t{}\t{}",
                                h["name"].as_str().unwrap_or(""),
                                crate::session::fmt_stamp(h["updated"].as_u64().unwrap_or(0)),
                                h["snippet"].as_str().unwrap_or(""),
                            );
                        }
                        eprintln!("\n{} hit(s) for \"{q}\"", v["n"].as_u64().unwrap_or(0));
                    }
                }
                Some(crate::NoteCmd::Dir) => {
                    let v = get(&base, &tok, "/v1/notes").await?;
                    println!("{}", v["dir"].as_str().unwrap_or(""));
                }
            }
        }
        Jobs { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                None => {
                    let v = get(&base, &tok, "/v1/jobs").await?;
                    for j in v["jobs"].as_array().cloned().unwrap_or_default() {
                        println!(
                            "{}\t{}\t{}\t{}\t{}{}",
                            j["id"].as_str().unwrap_or(""),
                            j["spec"].as_str().unwrap_or(""),
                            if j["enabled"].as_bool().unwrap_or(true) { "on " } else { "off" },
                            crate::session::fmt_stamp(j["next_run"].as_u64().unwrap_or(0)),
                            j["name"].as_str().unwrap_or(""),
                            match j["last_status"].as_str() {
                                Some(s) if !s.is_empty() => format!("\tlast: {s}"),
                                _ => String::new(),
                            },
                        );
                    }
                }
                Some(crate::JobCmd::Add { name, every, at, session, prompt }) => {
                    let spec = match (every, at) {
                        (Some(e), None) => format!("every {e}"),
                        (None, Some(a)) => format!("at {a}"),
                        _ => bail!("exactly one of --every / --at is required"),
                    };
                    let text = prompt.join(" ");
                    print_json(&post(
                        &base, &tok, "/v1/jobs",
                        json!({
                            "name": name.unwrap_or_default(),
                            "spec": spec,
                            "prompt": text,
                            "session": session.unwrap_or_default(),
                        }),
                    ).await?);
                }
                Some(crate::JobCmd::Remove { id }) => {
                    print_json(&delete(&base, &tok, &format!("/v1/jobs/{id}")).await?)
                }
                Some(crate::JobCmd::Run { id }) => {
                    print_json(&post(&base, &tok, &format!("/v1/jobs/{id}/run"), json!({})).await?)
                }
                Some(crate::JobCmd::Enable { id }) => {
                    print_json(&patch(&base, &tok, &format!("/v1/jobs/{id}"), json!({"enabled": true})).await?)
                }
                Some(crate::JobCmd::Disable { id }) => {
                    print_json(&patch(&base, &tok, &format!("/v1/jobs/{id}"), json!({"enabled": false})).await?)
                }
            }
        }
        Evolve { sub } => {
            let (base, tok) = runtime_info()?;
            match sub {
                Some(crate::EvolveCmd::Reflect { session }) => {
                    print_json(&post(&base, &tok, "/v1/evolve/reflect", json!({"session": session.unwrap_or_default()})).await?)
                }
                Some(crate::EvolveCmd::Forget { command }) => {
                    print_json(&post(&base, &tok, "/v1/evolve/forget", json!({"command": command})).await?)
                }
                None => print_json(&get(&base, &tok, "/v1/evolve").await?),
            }
        }
        Auth { id, status } => {
            let (base, tok) = runtime_info()?;
            if status {
                print_json(&get(&base, &tok, &format!("/v1/mcp-auth/{id}")).await?);
            } else {
                print_json(&post(&base, &tok, "/v1/mcp-auth", json!({"id": id})).await?);
            }
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
    crate::types::truncate_preview(s, 200)
}

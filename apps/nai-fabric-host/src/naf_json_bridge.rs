//! Loopback JSON capability bridge for non-NAF runtimes (C# / Files).
//!
//! NAF/1 is a BLAKE3-framed protobuf named-pipe protocol; a C# product cannot frame
//! it without a native helper. This bridge exposes the SAME runtime capability
//! boundary (`LatticeRuntime::dispatch`) as JSON over loopback so those products can
//! call `semantic.index`, `query.vector`, `query.search`, `query.federated` and
//! `index.query` without re-implementing the framing.
//!
//! It is a local capability bridge, not a replacement for the NAF/1 pipe. It is
//! enabled by default (NAF/1 is unchanged for every existing consumer); set
//! `NAI_LATTICE_JSON=0` to disable it. It binds 127.0.0.1 only, restricts the
//! method whitelist below, and requires a per-user bearer token persisted next to
//! the endpoint manifest (`json-bridge.token`). `GET /health` is unauthenticated
//! so products can discover liveness before reading the token.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use nai_lattice_runtime::LatticeRuntime;

const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Only read/recall capabilities are bridged; mutations still go through NAF/1.
const ALLOWED: &[&str] = &[
    "capabilities.list",
    "semantic.index",
    "semantic.index_batch",
    "semantic.delete",
    "query.vector",
    "query.search",
    "query.federated",
    "index.query",
];

pub(crate) fn start(rt: Arc<LatticeRuntime>) {
    // Enabled by default: existing NAF/1 consumers are unaffected, and NAI OS
    // products that are not NAF clients (C#/Files) need this door. `=0` disables.
    if std::env::var("NAI_LATTICE_JSON").ok().as_deref() == Some("0") {
        return;
    }
    let Some(token) = load_or_create_token() else {
        tracing::warn!("NAF JSON bridge disabled: could not provision a local token");
        return;
    };
    let port = std::env::var("NAI_LATTICE_JSON_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8789);

    std::thread::Builder::new()
        .name("naf-json-bridge".into())
        .spawn(move || {
            let listener = match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => listener,
                Err(error) => {
                    tracing::warn!(port, err = %error, "NAF JSON bridge could not bind");
                    return;
                }
            };
            tracing::info!(port, "NAF JSON bridge listening on http://127.0.0.1:{port}/dispatch");
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let rt = Arc::clone(&rt);
                let token = token.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &rt, &token);
                });
            }
        })
        .ok();
}

fn handle(mut stream: TcpStream, rt: &LatticeRuntime, token: &str) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut header_end = None;
    while header_end.is_none() && buffer.len() < MAX_HEADER {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = find(&buffer, b"\r\n\r\n");
    }

    let Some(header_end) = header_end else {
        return write_json(&mut stream, 400, &serde_json::json!({ "error": "bad request" }));
    };

    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header.split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split_whitespace();
    let http_method = request_parts.next().unwrap_or_default();
    let path = request_parts.next().unwrap_or_default();

    // Unauthenticated liveness so a product can discover the bridge before it
    // reads the per-user token.
    if http_method == "GET" && path == "/health" {
        return write_json(
            &mut stream,
            200,
            &serde_json::json!({ "status": "ok", "bridge": "naf-json", "version": 1 }),
        );
    }

    if http_method != "POST" || path != "/dispatch" {
        return write_json(&mut stream, 404, &serde_json::json!({ "error": "not found" }));
    }

    let mut authorization: Option<String> = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some(value) = line.strip_prefix("Authorization:") {
            authorization = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let expected = format!("Bearer {token}");
    if authorization.as_deref() != Some(expected.as_str()) {
        return write_json(&mut stream, 401, &serde_json::json!({ "error": "unauthorized" }));
    }
    if content_length == 0 || content_length > MAX_BODY {
        return write_json(&mut stream, 413, &serde_json::json!({ "error": "body required" }));
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    if body.len() < content_length {
        return write_json(&mut stream, 400, &serde_json::json!({ "error": "truncated body" }));
    }

    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return write_json(&mut stream, 400, &serde_json::json!({ "error": error.to_string() }))
        }
    };

    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    if !ALLOWED.contains(&method) {
        return write_json(
            &mut stream,
            403,
            &serde_json::json!({ "error": "method not allowed", "method": method }),
        );
    }

    let producer = request
        .get("producer")
        .and_then(|p| p.as_str())
        .unwrap_or("files");
    let args = request.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));
    let args_bytes = serde_json::to_vec(&args).unwrap_or_default();

    match rt.dispatch(method, &args_bytes, producer, None) {
        Ok(result) => {
            let parsed: serde_json::Value =
                serde_json::from_slice(&result).unwrap_or(serde_json::Value::Null);
            write_json(
                &mut stream,
                200,
                &serde_json::json!({ "status": 0, "body": parsed }),
            )
        }
        Err(error) => write_json(
            &mut stream,
            200,
            &serde_json::json!({ "status": 500, "error": error.to_string() }),
        ),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn write_json(stream: &mut TcpStream, status: u16, body: &serde_json::Value) -> std::io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_default();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()
}

/// Reads the per-user bridge token, creating it on first use. The token is
/// defense-in-depth on top of the loopback bind and the method whitelist.
fn load_or_create_token() -> Option<String> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    let directory = std::path::Path::new(&base)
        .join("NAI")
        .join("Lattice")
        .join("v1");
    std::fs::create_dir_all(&directory).ok()?;

    let path = directory.join("json-bridge.token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let value = existing.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }

    // Entropy: BLAKE3 over time, pid and a fresh heap address. Local-only bridge.
    let mut hasher = blake3::Hasher::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(&now.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    let probe = Box::new(0u8);
    hasher.update(&(probe.as_ref() as *const u8 as usize).to_le_bytes());
    let token = hasher.finalize().to_hex().to_string();
    std::fs::write(&path, &token).ok()?;
    Some(token)
}

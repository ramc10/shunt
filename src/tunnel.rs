//! Client-side persistent tunnel — `shunt live`.
//!
//! Opens a WebSocket to the relay server, registers a subdomain, then forwards
//! HTTP requests from the relay to the local shunt proxy. Reconnects with
//! exponential backoff on disconnect.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Protocol frames (must match live_relay.rs)
// ---------------------------------------------------------------------------

/// Relay → Client
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RelayFrame {
    Ack  { subdomain: String },
    Deny { reason: String },
    Req  {
        id:      String,
        method:  String,
        path:    String,
        headers: HashMap<String, String>,
        body:    String, // base64
    },
}

/// Client → Relay
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame<'a> {
    Register  { subdomain: &'a str, token: &'a str },
    ResHead   { id: &'a str, status: u16, headers: &'a HashMap<String, String> },
    ResBody   { id: &'a str, data: &'a str },
    ResEnd    { id: &'a str },
    ResErr    { id: &'a str, message: &'a str },
}

// ---------------------------------------------------------------------------
// Entry point — reconnect loop
// ---------------------------------------------------------------------------

pub async fn run_live(
    relay_ws_url: &str,
    subdomain: &str,
    token: &str,
    local_url: &str,
) -> Result<()> {
    let mut backoff = Duration::from_secs(2);
    loop {
        match connect_and_serve(relay_ws_url, subdomain, token, local_url).await {
            Ok(()) => {
                println!("  · Tunnel closed.");
                break;
            }
            Err(e) => {
                let secs = backoff.as_secs();
                println!("  · Tunnel disconnected ({e}), reconnecting in {secs}s…");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single connection session
// ---------------------------------------------------------------------------

async fn connect_and_serve(
    relay_ws_url: &str,
    subdomain: &str,
    token: &str,
    local_url: &str,
) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(relay_ws_url)
        .await
        .context("Failed to connect to relay")?;

    let (sink, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink));

    // ── Register ────────────────────────────────────────────────────────────
    {
        let frame = ClientFrame::Register { subdomain, token };
        let text = serde_json::to_string(&frame)?;
        sink.lock().await.send(Message::Text(text)).await?;
    }

    // Wait for Ack/Deny
    match stream.next().await {
        Some(Ok(Message::Text(text))) => {
            let frame: RelayFrame = serde_json::from_str(&text)
                .context("Invalid relay response")?;
            match frame {
                RelayFrame::Ack { subdomain: s } => {
                    println!("  ✓ Tunnel connected: {s}");
                }
                RelayFrame::Deny { reason } => {
                    anyhow::bail!("Relay denied registration: {reason}");
                }
                _ => anyhow::bail!("Unexpected relay response"),
            }
        }
        _ => anyhow::bail!("No response from relay during registration"),
    }

    // ── Handle incoming requests ────────────────────────────────────────────
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            Message::Ping(d) => {
                let _ = sink.lock().await.send(Message::Pong(d)).await;
                continue;
            }
            _ => continue,
        };

        let frame: RelayFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(_) => continue,
        };

        if let RelayFrame::Req { id, method, path, headers, body } = frame {
            let sink = sink.clone();
            let http = http.clone();
            let local = local_url.to_owned();
            tokio::spawn(async move {
                handle_request(id, method, path, headers, body, &local, &http, &sink).await;
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Request handler — forward to local shunt, stream response back
// ---------------------------------------------------------------------------

type WsSink = Arc<Mutex<futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>
    >,
    Message,
>>>;

async fn handle_request(
    id:      String,
    method:  String,
    path:    String,
    headers: HashMap<String, String>,
    body_b64: String,
    local_url: &str,
    http:    &reqwest::Client,
    sink:    &WsSink,
) {
    let send_err = |msg: &str| {
        let frame = serde_json::to_string(&ClientFrame::ResErr { id: &id, message: msg }).unwrap();
        async move {
            let _ = sink.lock().await.send(Message::Text(frame)).await;
        }
    };

    // Decode body
    let body_bytes = match B64.decode(&body_b64) {
        Ok(b) => b,
        Err(e) => { send_err(&format!("base64 decode: {e}")).await; return; }
    };

    // Build local request
    let url = format!("{local_url}{path}");
    let method = match method.parse::<reqwest::Method>() {
        Ok(m) => m,
        Err(e) => { send_err(&format!("bad method: {e}")).await; return; }
    };

    let mut req = http.request(method, &url);
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    req = req.body(body_bytes);

    // Send to local shunt
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => { send_err(&format!("local request failed: {e}")).await; return; }
    };

    // Send response head
    let status = resp.status().as_u16();
    let resp_headers: HashMap<String, String> = resp.headers().iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned())))
        .collect();

    {
        let frame = serde_json::to_string(&ClientFrame::ResHead {
            id: &id, status, headers: &resp_headers,
        }).unwrap();
        if sink.lock().await.send(Message::Text(frame)).await.is_err() { return; }
    }

    // Stream body chunks
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let data = B64.encode(&bytes);
                let frame = serde_json::to_string(&ClientFrame::ResBody {
                    id: &id, data: &data,
                }).unwrap();
                if sink.lock().await.send(Message::Text(frame)).await.is_err() { return; }
            }
            Err(e) => {
                send_err(&format!("body stream error: {e}")).await;
                return;
            }
        }
    }

    // Send end
    let frame = serde_json::to_string(&ClientFrame::ResEnd { id: &id }).unwrap();
    let _ = sink.lock().await.send(Message::Text(frame)).await;
}

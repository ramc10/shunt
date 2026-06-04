//! Persistent tunnel relay server — `shunt relay serve`.
//!
//! Runs on the VPS. Two roles:
//!   1. Accepts WebSocket tunnel connections from shunt instances (`GET /tunnel`).
//!   2. Proxies HTTP requests to the right tunnel based on the `Host` header.
//!
//! Multi-tenant from day one: all state is keyed by subdomain. Adding users later
//! is a token-registry change (env var → SQLite), not a protocol change.

use anyhow::Result;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// ---------------------------------------------------------------------------
// Protocol frames (JSON over WebSocket text messages)
// ---------------------------------------------------------------------------

/// Client → Relay
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Register { subdomain: String, token: String },
    ResHead { id: String, status: u16, headers: HashMap<String, String> },
    ResBody { id: String, data: String }, // base64
    ResEnd  { id: String },
    ResErr  { id: String, message: String },
}

/// Relay → Client
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RelayFrame<'a> {
    Ack  { subdomain: &'a str },
    Deny { reason: &'a str },
    Req  {
        id:      &'a str,
        method:  &'a str,
        path:    &'a str,
        headers: &'a HashMap<String, String>,
        body:    &'a str, // base64
    },
}

// ---------------------------------------------------------------------------
// Relay state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RelayState {
    tunnels:       Arc<RwLock<HashMap<String, TunnelHandle>>>,
    allowed_token: Arc<String>,
    // Future: replace allowed_token with Arc<RwLock<HashMap<token, subdomain>>>
    //         loaded from SQLite for multi-user.
}

#[derive(Clone)]
struct TunnelHandle {
    tx: mpsc::Sender<TunnelRequest>,
}

struct TunnelRequest {
    id:      String,
    method:  String,
    path:    String,
    headers: HashMap<String, String>,
    body:    Bytes,
    res_tx:  mpsc::Sender<ResponseChunk>,
}

#[derive(Debug)]
enum ResponseChunk {
    Head   { status: u16, headers: HashMap<String, String> },
    Body   (Bytes),
    End,
    Err    (String),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_relay_server(port: u16, token: String) -> Result<()> {
    let state = RelayState {
        tunnels:       Arc::new(RwLock::new(HashMap::new())),
        allowed_token: Arc::new(token),
    };

    let app = Router::new()
        .route("/tunnel", get(ws_handler))
        .fallback(proxy_handler)
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    println!("  ◆ shunt relay  listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket tunnel handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_tunnel(socket, state))
}

async fn handle_tunnel(socket: WebSocket, state: RelayState) {
    let (mut sink, mut stream) = socket.split();

    // ── Step 1: expect a Register frame ─────────────────────────────────────
    let subdomain = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<ClientFrame>(&text) {
                    Ok(ClientFrame::Register { subdomain, token }) => {
                        if token != *state.allowed_token {
                            let _ = sink.send(Message::Text(
                                serde_json::to_string(&RelayFrame::Deny { reason: "invalid token" }).unwrap()
                            )).await;
                            return;
                        }
                        let _ = sink.send(Message::Text(
                            serde_json::to_string(&RelayFrame::Ack { subdomain: &subdomain }).unwrap()
                        )).await;
                        break subdomain;
                    }
                    _ => { return; } // unexpected frame before registration
                }
            }
            _ => return,
        }
    };

    // ── Step 2: register tunnel ──────────────────────────────────────────────
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<TunnelRequest>(16);
    state.tunnels.write().insert(subdomain.clone(), TunnelHandle { tx: tunnel_tx });
    println!("  ◆ tunnel registered: {subdomain}");

    // ── Step 3: sender task (relay → tunnel) ─────────────────────────────────
    // Pending requests: id → response channel
    let pending: Arc<RwLock<HashMap<String, mpsc::Sender<ResponseChunk>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Channel for outbound WS messages (from both sender task and request handlers)
    let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(64);

    // Flush outbound messages to the WS sink
    let ws_tx_clone = ws_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if sink.send(msg).await.is_err() { break; }
        }
    });

    // Forward incoming TunnelRequests to the WS
    let ws_tx2 = ws_tx_clone.clone();
    let pending2 = pending.clone();
    tokio::spawn(async move {
        while let Some(req) = tunnel_rx.recv().await {
            pending2.write().insert(req.id.clone(), req.res_tx);
            let body_b64 = B64.encode(&req.body);
            let frame = RelayFrame::Req {
                id:      &req.id,
                method:  &req.method,
                path:    &req.path,
                headers: &req.headers,
                body:    &body_b64,
            };
            let text = serde_json::to_string(&frame).unwrap();
            if ws_tx2.send(Message::Text(text)).await.is_err() { break; }
        }
    });

    // ── Step 4: reader loop (tunnel → relay) ─────────────────────────────────
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let frame = match serde_json::from_str::<ClientFrame>(&text) {
            Ok(f) => f,
            Err(_) => continue,
        };
        match frame {
            ClientFrame::ResHead { id, status, headers } => {
                // Clone tx before awaiting — parking_lot guard must not cross await points
                let tx = pending.read().get(&id).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(ResponseChunk::Head { status, headers }).await;
                }
            }
            ClientFrame::ResBody { id, data } => {
                let tx = pending.read().get(&id).cloned();
                if let Some(tx) = tx {
                    if let Ok(bytes) = B64.decode(&data) {
                        let _ = tx.send(ResponseChunk::Body(Bytes::from(bytes))).await;
                    }
                }
            }
            ClientFrame::ResEnd { id } => {
                let tx = pending.write().remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.send(ResponseChunk::End).await;
                }
            }
            ClientFrame::ResErr { id, message } => {
                let tx = pending.write().remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.send(ResponseChunk::Err(message)).await;
                }
            }
            ClientFrame::Register { .. } => {} // ignore duplicate registration
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    state.tunnels.write().remove(&subdomain);
    println!("  · tunnel disconnected: {subdomain}");
}

// ---------------------------------------------------------------------------
// HTTP proxy handler
// ---------------------------------------------------------------------------

async fn proxy_handler(
    State(state): State<RelayState>,
    req: Request<Body>,
) -> Response {
    // Extract subdomain from Host header: "shunt.ramcharan.shop" → "shunt"
    let subdomain = match extract_subdomain(req.headers()) {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "missing Host header").into_response(),
    };

    // Find tunnel
    let handle = state.tunnels.read().get(&subdomain).cloned();
    let handle = match handle {
        Some(h) => h,
        None => return (
            StatusCode::BAD_GATEWAY,
            format!("no tunnel connected for '{subdomain}'"),
        ).into_response(),
    };

    // Build request fields to send through tunnel
    let id = uuid::Uuid::new_v4().to_string();
    let method = req.method().to_string();
    let path = req.uri().path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let headers: HashMap<String, String> = req.headers().iter()
        .filter_map(|(k, v)| {
            let key = k.as_str().to_lowercase();
            // Don't forward hop-by-hop headers
            if matches!(key.as_str(), "host" | "connection" | "transfer-encoding" | "upgrade") {
                return None;
            }
            v.to_str().ok().map(|v| (key, v.to_owned()))
        })
        .collect();
    let body = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "failed to read body").into_response(),
    };

    // Send request to tunnel
    let (res_tx, res_rx) = mpsc::channel::<ResponseChunk>(32);
    let tunnel_req = TunnelRequest { id, method, path, headers, body, res_tx };
    if handle.tx.send(tunnel_req).await.is_err() {
        return (StatusCode::BAD_GATEWAY, "tunnel send failed").into_response();
    }

    // Wait for response head
    let mut rx = res_rx;
    let (status, res_headers) = match rx.recv().await {
        Some(ResponseChunk::Head { status, headers }) => (status, headers),
        Some(ResponseChunk::Err(e)) => return (StatusCode::BAD_GATEWAY, e).into_response(),
        _ => return (StatusCode::BAD_GATEWAY, "no response from tunnel").into_response(),
    };

    // Build streaming body from remaining chunks
    let stream = ReceiverStream::new(rx).filter_map(|chunk| async move {
        match chunk {
            ResponseChunk::Body(b) => Some(Ok::<_, std::convert::Infallible>(b)),
            ResponseChunk::End | ResponseChunk::Head { .. } | ResponseChunk::Err(_) => None,
        }
    });

    let mut builder = Response::builder()
        .status(status);
    for (k, v) in &res_headers {
        builder = builder.header(k, v);
    }
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response()
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_subdomain(headers: &axum::http::HeaderMap) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?;
    // "shunt.ramcharan.shop" → "shunt"
    // "shunt.ramcharan.shop:8085" → "shunt"
    let host = host.split(':').next()?;
    let subdomain = host.split('.').next()?;
    if subdomain.is_empty() { return None; }
    Some(subdomain.to_owned())
}

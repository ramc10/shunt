/// Usage extraction from upstream responses.
///
/// - Non-streaming (application/json): parse `usage` from buffered body bytes.
/// - Streaming (text/event-stream): wrap the body with an SSE scanner that
///   extracts token counts from `message_start`/`message_delta` events and
///   calls a callback on stream end — zero added latency.
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, BodyDataStream};
use axum::http::Response;
use bytes::{Bytes, BytesMut};
use futures_util::Stream;

// ---------------------------------------------------------------------------
// Non-streaming usage extraction
// ---------------------------------------------------------------------------

/// Extract `(input_tokens, output_tokens)` from a JSON response body.
/// Returns `(0, 0)` if the body is not parseable or has no usage field.
pub fn extract_usage_from_json(body: &[u8]) -> (u64, u64) {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (0, 0),
    };
    let input  = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
    (input, output)
}

// ---------------------------------------------------------------------------
// Streaming detection
// ---------------------------------------------------------------------------

pub fn is_streaming_response(resp: &Response<Body>) -> bool {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// SSE scanner stream adapter
// ---------------------------------------------------------------------------

/// Wraps a `Body` stream, scanning SSE events for token usage.
/// Every byte is forwarded immediately; `on_complete(input, output)` is called
/// once when the stream ends.
pub fn wrap_streaming_body(
    body: Body,
    on_complete: Arc<dyn Fn(u64, u64) + Send + Sync + 'static>,
) -> Body {
    Body::from_stream(SseScanner::new(body.into_data_stream(), on_complete))
}

struct SseScanner {
    inner: BodyDataStream,
    line_buf: BytesMut,
    input_tokens: u64,
    output_tokens: u64,
    last_event: LastEvent,
    on_complete: Arc<dyn Fn(u64, u64) + Send + Sync + 'static>,
    done: bool,
}

#[derive(Default)]
enum LastEvent {
    #[default]
    None,
    MessageStart,
    MessageDelta,
}

impl SseScanner {
    fn new(inner: BodyDataStream, on_complete: Arc<dyn Fn(u64, u64) + Send + Sync + 'static>) -> Self {
        Self {
            inner,
            line_buf: BytesMut::new(),
            input_tokens: 0,
            output_tokens: 0,
            last_event: LastEvent::None,
            on_complete,
            done: false,
        }
    }

    /// Process complete lines in `line_buf`, extracting token counts from SSE events.
    fn scan_lines(&mut self) {
        loop {
            let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let raw = self.line_buf.split_to(pos + 1);
            let line = raw
                .strip_suffix(b"\r\n")
                .or_else(|| raw.strip_suffix(b"\n"))
                .unwrap_or(&raw);

            if line.starts_with(b"event: message_start") {
                self.last_event = LastEvent::MessageStart;
            } else if line.starts_with(b"event: message_delta") {
                self.last_event = LastEvent::MessageDelta;
            } else if let Some(json_bytes) = line.strip_prefix(b"data: ") {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(json_bytes) {
                    match self.last_event {
                        LastEvent::MessageStart => {
                            self.input_tokens += v["message"]["usage"]["input_tokens"]
                                .as_u64()
                                .unwrap_or(0);
                        }
                        LastEvent::MessageDelta => {
                            self.output_tokens += v["usage"]["output_tokens"]
                                .as_u64()
                                .unwrap_or(0);
                        }
                        LastEvent::None => {}
                    }
                }
                self.last_event = LastEvent::None;
            }
        }
    }
}

impl Stream for SseScanner {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                self.line_buf.extend_from_slice(&chunk);
                self.scan_lines();
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                self.done = true;
                (self.on_complete)(self.input_tokens, self.output_tokens);
                Poll::Ready(None)
            }
        }
    }
}

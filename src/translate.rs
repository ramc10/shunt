use axum::body::Body;
use axum::http::header;
use axum::http::HeaderValue;
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::quota;

/// Map OpenAI model names → Claude model names.
/// Claude model names are passed through unchanged; only OpenAI aliases are remapped.
pub fn map_model(openai_model: &str) -> String {
    if openai_model.starts_with("claude-") {
        return openai_model.to_owned();
    }
    match openai_model {
        "gpt-4o" | "gpt-4.5" | "o1" | "o1-pro" | "o3" | "o3-pro" | "gpt-5" | "gpt-5.5" => {
            "claude-opus-4-6"
        }
        "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" | "o1-mini" | "o3-mini" => {
            "claude-haiku-4-5-20251001"
        }
        _ => "claude-sonnet-4-6",
    }.to_owned()
}

/// Translate an OpenAI Chat Completions request body to an Anthropic Messages body.
pub fn translate_to_anthropic(body: Value) -> Value {
    let model = body["model"].as_str().unwrap_or("gpt-4o");
    let claude_model = map_model(model);

    // Extract system message from messages array.
    let mut system: Option<String> = None;
    let mut messages = Vec::new();
    if let Some(arr) = body["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("");
            if role == "system" {
                // system can be a string or array of content parts
                let content = msg["content"].as_str()
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| serde_json::to_string(&msg["content"]).unwrap_or_default());
                system = Some(content);
            } else if role == "tool" {
                // OpenAI tool result → Anthropic tool_result content block
                let tool_use_id = msg["tool_call_id"].as_str().unwrap_or("").to_owned();
                let content = msg["content"].as_str().unwrap_or("").to_owned();
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]
                }));
            } else {
                // Check for tool_calls in assistant messages
                if let Some(tool_calls) = msg["tool_calls"].as_array() {
                    let mut content_blocks: Vec<Value> = Vec::new();
                    if let Some(text) = msg["content"].as_str().filter(|s| !s.is_empty()) {
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                    for tc in tool_calls {
                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": tc["id"].as_str().unwrap_or(""),
                            "name": tc["function"]["name"].as_str().unwrap_or(""),
                            "input": serde_json::from_str::<Value>(
                                tc["function"]["arguments"].as_str().unwrap_or("{}")
                            ).unwrap_or(json!({})),
                        }));
                    }
                    messages.push(json!({"role": "assistant", "content": content_blocks}));
                } else {
                    let content = msg["content"].as_str().unwrap_or("").to_owned();
                    messages.push(json!({ "role": role, "content": content }));
                }
            }
        }
    }

    let max_tokens = body["max_tokens"].as_u64().unwrap_or(8096);
    let stream = body["stream"].as_bool().unwrap_or(false);

    let mut req = json!({
        "model": claude_model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    if let Some(sys) = system {
        req["system"] = json!(sys);
    }
    if let Some(temp) = body.get("temperature") {
        req["temperature"] = temp.clone();
    }
    if let Some(sp) = body.get("stop") {
        req["stop_sequences"] = sp.clone();
    }

    // Translate OpenAI tools → Anthropic tools format
    if let Some(tools) = body["tools"].as_array() {
        let claude_tools: Vec<Value> = tools.iter().filter_map(|t| {
            let func = &t["function"];
            Some(json!({
                "name": func["name"].as_str()?,
                "description": func["description"].as_str().unwrap_or(""),
                "input_schema": func.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
            }))
        }).collect();
        if !claude_tools.is_empty() {
            req["tools"] = json!(claude_tools);
        }
    }

    req
}

/// Translate a complete (non-streaming) Anthropic Messages response to OpenAI format.
pub fn translate_from_anthropic(body: Value) -> Value {
    let id = format!("chatcmpl-{}", &uuid_v4()[..8]);
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-6").to_owned();

    // Extract text content and tool_use blocks.
    let mut text_content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = body["content"].as_array() {
        for (idx, block) in blocks.iter().enumerate() {
            match block["type"].as_str() {
                Some("text") => {
                    text_content.push_str(block["text"].as_str().unwrap_or(""));
                }
                Some("tool_use") => {
                    let args = match &block["input"] {
                        Value::String(s) => s.clone(),
                        v => serde_json::to_string(v).unwrap_or_default(),
                    };
                    tool_calls.push(json!({
                        "id": block["id"].as_str().unwrap_or(""),
                        "type": "function",
                        "index": idx,
                        "function": {
                            "name": block["name"].as_str().unwrap_or(""),
                            "arguments": args,
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let stop_reason = body["stop_reason"].as_str().unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "end_turn"   => "stop",
        "tool_use"   => "tool_calls",
        "max_tokens" => "length",
        other        => other,
    };

    let input_tokens = body["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);

    let mut message = json!({"role": "assistant", "content": text_content});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

pub fn uuid_v4() -> String {
    use crate::oauth::rand_bytes;
    let b: [u8; 16] = rand_bytes();
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(b[0..4].try_into().unwrap()),
        u16::from_be_bytes(b[4..6].try_into().unwrap()),
        u16::from_be_bytes(b[6..8].try_into().unwrap()),
        u16::from_be_bytes(b[8..10].try_into().unwrap()),
        {
            let mut v = 0u64;
            for &x in &b[10..16] { v = (v << 8) | x as u64; }
            v
        }
    )
}

/// Translate Anthropic SSE events to OpenAI SSE format, yielding raw bytes.
/// Handles text content, tool_use blocks, and finish reasons.
pub fn translate_anthropic_stream(
    resp: reqwest::Response,
    chat_id: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    let id = chat_id;
    let byte_stream = resp.bytes_stream();

    async_stream::stream! {
        let mut buf = String::new();
        // Per-block state: block_index -> (tool_call_oai_index, tool_id, tool_name)
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        futures_util::pin_mut!(byte_stream);

        // Send initial role chunk
        let init = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": id,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
            })).unwrap()
        );
        yield Ok(Bytes::from(init));

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => break,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();

                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }

                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                let event_type = event["type"].as_str().unwrap_or("");

                let maybe_chunk = match event_type {
                    "content_block_start" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let cb = &event["content_block"];
                        if cb["type"].as_str() == Some("tool_use") {
                            let tool_id = cb["id"].as_str().unwrap_or("").to_owned();
                            let tool_name = cb["name"].as_str().unwrap_or("").to_owned();
                            let oai_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(block_idx, (oai_idx, tool_id.clone(), tool_name.clone()));
                            Some(json!({
                                "id": id,
                                "object": "chat.completion.chunk",
                                "choices": [{"index": 0, "delta": {
                                    "tool_calls": [{
                                        "index": oai_idx,
                                        "id": tool_id,
                                        "type": "function",
                                        "function": {"name": tool_name, "arguments": ""}
                                    }]
                                }, "finish_reason": null}]
                            }))
                        } else {
                            None
                        }
                    }
                    "content_block_delta" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                let text = delta["text"].as_str().unwrap_or("");
                                if text.is_empty() { continue; }
                                Some(json!({
                                    "id": id,
                                    "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                                }))
                            }
                            Some("input_json_delta") => {
                                let args = delta["partial_json"].as_str().unwrap_or("");
                                if let Some((oai_idx, _, _)) = tool_blocks.get(&block_idx) {
                                    Some(json!({
                                        "id": id,
                                        "object": "chat.completion.chunk",
                                        "choices": [{"index": 0, "delta": {
                                            "tool_calls": [{"index": oai_idx, "function": {"arguments": args}}]
                                        }, "finish_reason": null}]
                                    }))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                    "message_delta" => {
                        let stop_reason = event["delta"]["stop_reason"].as_str().unwrap_or("stop");
                        let finish = match stop_reason {
                            "end_turn"  => "stop",
                            "tool_use"  => "tool_calls",
                            "max_tokens" => "length",
                            other       => other,
                        };
                        Some(json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
                        }))
                    }
                    _ => None,
                };

                if let Some(c) = maybe_chunk {
                    let out = format!("data: {}\n\n", serde_json::to_string(&c).unwrap());
                    yield Ok(Bytes::from(out));
                }
            }
        }

        yield Ok(Bytes::from("data: [DONE]\n\n"));
    }
}

/// Map Claude model names → chatgpt.com model names.
pub fn map_model_to_chatgpt(model: &str) -> &str {
    if model.contains("opus") {
        "gpt-4o"
    } else if model.contains("haiku") {
        "gpt-4o-mini"
    } else {
        "gpt-4o"
    }
}

/// Extract flat text from an Anthropic content value (string or content block array).
/// Tool-use blocks are rendered as `[Tool: name(args)]`; tool_result blocks are flattened.
pub fn extract_text_from_anthropic_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    if let Some(arr) = content.as_array() {
        let mut text = String::new();
        for block in arr {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("tool");
                    let args = serde_json::to_string(&block["input"]).unwrap_or_default();
                    text.push_str(&format!("[Tool: {}({})]", name, args));
                }
                Some("tool_result") => {
                    let result = block["content"].as_str()
                        .map(|s| s.to_owned())
                        .unwrap_or_else(|| serde_json::to_string(&block["content"]).unwrap_or_default());
                    text.push_str(&result);
                }
                _ => {}
            }
        }
        return text;
    }
    String::new()
}

/// Translate an Anthropic `/v1/messages` request body to chatgpt.com `/backend-api/conversation` format.
/// Tools are stripped — chatgpt.com's backend API does not support tool use.
pub fn translate_anthropic_req_to_chatgpt(body: &Value) -> Value {
    let claude_model = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let model = map_model_to_chatgpt(claude_model);
    let system_prompt = body["system"].as_str().unwrap_or("").to_owned();

    let mut messages: Vec<Value> = Vec::new();
    if let Some(arr) = body["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("user");
            let text = extract_text_from_anthropic_content(&msg["content"]);
            messages.push(json!({
                "id": uuid_v4(),
                "author": {"role": role},
                "content": {"content_type": "text", "parts": [text]},
                "metadata": {}
            }));
        }
    }

    json!({
        "action": "next",
        "messages": messages,
        "model": model,
        "parent_message_id": uuid_v4(),
        "system_prompt": system_prompt,
        "history_and_training_disabled": true,
        "supports_modapi": false,
    })
}

/// Translate a chatgpt.com non-streaming response to Anthropic format.
pub fn translate_chatgpt_resp_to_anthropic(body: Value, model: &str) -> Value {
    let id = format!("msg_{}", &uuid_v4()[..8]);
    let text = body["message"]["content"]["parts"][0]
        .as_str()
        .unwrap_or("")
        .to_owned();
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    })
}

/// Translate the response back from chatgpt.com format to Anthropic format.
/// Handles both streaming and non-streaming responses.
pub async fn translate_response_chatgpt_to_anthropic(resp: Response, model: &str) -> Response {
    let msg_id = format!("msg_{}", &uuid_v4()[..8]);
    let model = model.to_owned();

    if quota::is_streaming_response(&resp) {
        let (mut parts, body) = resp.into_parts();
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let stream = translate_chatgpt_stream_to_anthropic(body, model, msg_id);
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        let (mut parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();
        let chatgpt_val: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let anthropic_val = translate_chatgpt_resp_to_anthropic(chatgpt_val, &model);
        let out = serde_json::to_vec(&anthropic_val).unwrap_or_default();
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Response::from_parts(parts, Body::from(out))
    }
}

/// Stream-translate a chatgpt.com SSE response body into Anthropic SSE events.
///
/// chatgpt.com sends the **full accumulated text** in each chunk (not a delta),
/// so we track `prev_len` and compute deltas ourselves.
pub fn translate_chatgpt_stream_to_anthropic(
    body: Body,
    model: String,
    msg_id: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::stream! {
        let start_evt = format!(
            "event: message_start\ndata: {}\n\nevent: ping\ndata: {{\"type\":\"ping\"}}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": msg_id, "type": "message", "role": "assistant",
                    "content": [], "model": model, "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            })).unwrap()
        );
        yield Ok(Bytes::from(start_evt));

        let mut buf = String::new();
        let mut content_block_open = false;
        let mut prev_len: usize = 0;
        let byte_stream = body.into_data_stream();
        futures_util::pin_mut!(byte_stream);

        'outer: while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(_) => break };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { break 'outer; }
                let Ok(val) = serde_json::from_str::<Value>(data) else { continue };

                let text = match val["message"]["content"]["parts"][0].as_str() {
                    Some(t) => t.to_owned(),
                    None => continue,
                };

                let delta = text[prev_len..].to_owned();
                if !delta.is_empty() {
                    if !content_block_open {
                        content_block_open = true;
                        yield Ok(Bytes::from(format!(
                            "event: content_block_start\ndata: {}\n\n",
                            serde_json::to_string(&json!({
                                "type": "content_block_start", "index": 0,
                                "content_block": {"type": "text", "text": ""}
                            })).unwrap()
                        )));
                    }
                    yield Ok(Bytes::from(format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        serde_json::to_string(&json!({
                            "type": "content_block_delta", "index": 0,
                            "delta": {"type": "text_delta", "text": delta}
                        })).unwrap()
                    )));
                    prev_len = text.len();
                }

                if val["message"]["end_turn"].as_bool() == Some(true) {
                    break 'outer;
                }
            }
        }

        if content_block_open {
            yield Ok(Bytes::from(format!(
                "event: content_block_stop\ndata: {}\n\n",
                serde_json::to_string(&json!({"type": "content_block_stop", "index": 0})).unwrap()
            )));
        }
        yield Ok(Bytes::from(format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 0}
            })).unwrap()
        )));
        yield Ok(Bytes::from(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ));
    }
}

/// Translate an Anthropic `/v1/messages` request body to OpenAI `/v1/chat/completions` format.
/// Used when routing an Anthropic-protocol request to an OpenAI-compat account.
/// `target_model` is the already-resolved provider model name (e.g. "llama-3.3-70b-versatile").
pub fn translate_anthropic_req_to_openai(body: Value, target_model: &str) -> Value {
    let model = target_model;
    let stream = body["stream"].as_bool().unwrap_or(false);
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(8096);

    let mut messages: Vec<Value> = Vec::new();

    // Prepend system prompt if present.
    if let Some(sys) = body["system"].as_str().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": sys}));
    }

    if let Some(arr) = body["messages"].as_array() {
        for msg in arr {
            let role = msg["role"].as_str().unwrap_or("user");

            if let Some(blocks) = msg["content"].as_array() {
                // Check for tool_result blocks (user turn carrying tool results).
                let has_tool_result = blocks.iter().any(|b| b["type"] == "tool_result");
                if has_tool_result {
                    for b in blocks {
                        if b["type"] == "tool_result" {
                            let content = b["content"].as_str()
                                .map(|s| s.to_owned())
                                .unwrap_or_else(|| serde_json::to_string(&b["content"]).unwrap_or_default());
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": b["tool_use_id"].as_str().unwrap_or(""),
                                "content": content,
                            }));
                        }
                    }
                    continue;
                }

                // Regular content blocks — may include text and tool_use.
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                        Some("tool_use") => {
                            let args = match &b["input"] {
                                Value::String(s) => s.clone(),
                                v => serde_json::to_string(v).unwrap_or_default(),
                            };
                            tool_calls.push(json!({
                                "id": b["id"].as_str().unwrap_or(""),
                                "type": "function",
                                "function": {"name": b["name"].as_str().unwrap_or(""), "arguments": args},
                            }));
                        }
                        _ => {}
                    }
                }
                let mut m = json!({"role": role, "content": text});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = json!(tool_calls);
                }
                messages.push(m);
            } else if let Some(s) = msg["content"].as_str() {
                messages.push(json!({"role": role, "content": s}));
            }
        }
    }

    let mut req = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    // Request usage data in stream final chunk.
    if stream {
        req["stream_options"] = json!({"include_usage": true});
    }
    if let Some(t) = body.get("temperature") { req["temperature"] = t.clone(); }
    if let Some(sp) = body.get("stop_sequences") { req["stop"] = sp.clone(); }

    // Anthropic tools → OpenAI tools.
    if let Some(tools) = body["tools"].as_array() {
        let oai: Vec<Value> = tools.iter().map(|t| json!({
            "type": "function",
            "function": {
                "name": t["name"].as_str().unwrap_or(""),
                "description": t["description"].as_str().unwrap_or(""),
                "parameters": t.get("input_schema").cloned()
                    .unwrap_or(json!({"type": "object", "properties": {}})),
            }
        })).collect();
        if !oai.is_empty() { req["tools"] = json!(oai); }
    }

    if let Some(tc) = body.get("tool_choice") {
        req["tool_choice"] = match tc["type"].as_str() {
            Some("any")  => json!({"type": "required"}),
            Some("tool") => json!({"type": "function", "function": {"name": tc["name"]}}),
            _            => json!("auto"),
        };
    }

    req
}

/// Translate an OpenAI `/v1/chat/completions` non-streaming response to Anthropic format.
pub fn translate_openai_resp_to_anthropic(body: Value, model: &str) -> Value {
    let id = format!("msg_{}", &uuid_v4()[..8]);
    let choice = &body["choices"][0];
    let msg = &choice["message"];

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = msg["content"].as_str().filter(|s| !s.is_empty()) {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            content.push(json!({
                "type": "tool_use",
                "id": tc["id"].as_str().unwrap_or(""),
                "name": tc["function"]["name"].as_str().unwrap_or(""),
                "input": serde_json::from_str::<Value>(
                    tc["function"]["arguments"].as_str().unwrap_or("{}")
                ).unwrap_or(json!({})),
            }));
        }
    }

    let stop_reason = match choice["finish_reason"].as_str().unwrap_or("stop") {
        "stop"       => "end_turn",
        "tool_calls" => "tool_use",
        "length"     => "max_tokens",
        other        => other,
    };

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens":  body["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": body["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        }
    })
}

/// Translate the response back from OpenAI format to Anthropic format.
/// Handles both streaming and non-streaming responses.
pub async fn translate_response_openai_to_anthropic(resp: Response, model: &str) -> Response {
    let msg_id = format!("msg_{}", &uuid_v4()[..8]);
    let model = model.to_owned();

    if quota::is_streaming_response(&resp) {
        let (mut parts, body) = resp.into_parts();
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let stream = translate_openai_stream_to_anthropic(body, model, msg_id);
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        let (mut parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();
        let openai_val: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let anthropic_val = translate_openai_resp_to_anthropic(openai_val, &model);
        let out = serde_json::to_vec(&anthropic_val).unwrap_or_default();
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Response::from_parts(parts, Body::from(out))
    }
}

/// Translate the response back from Anthropic format to OpenAI format.
pub async fn translate_response_anthropic_to_openai(resp: Response) -> Response {
    let chat_id = format!("chatcmpl-{}", &uuid_v4()[..8]);

    if quota::is_streaming_response(&resp) {
        let (parts, body) = resp.into_parts();
        let stream = translate_body_anthropic_to_openai(body, chat_id);
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        let (mut parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap_or_default();
        let anthropic_val: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let openai_val = translate_from_anthropic(anthropic_val);
        let out = serde_json::to_vec(&openai_val).unwrap_or_default();
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Response::from_parts(parts, Body::from(out))
    }
}

/// Stream-translate an OpenAI SSE response body into Anthropic SSE events.
///
/// Emits: `message_start` → `content_block_start` → N×`content_block_delta`
///       → `content_block_stop` → `message_delta` → `message_stop`
pub fn translate_openai_stream_to_anthropic(
    body: Body,
    model: String,
    msg_id: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::stream! {
        // Send message_start immediately (input_tokens unknown yet, use 0).
        let start_evt = format!(
            "event: message_start\ndata: {}\n\nevent: ping\ndata: {{\"type\":\"ping\"}}\n\n",
            serde_json::to_string(&json!({
                "type": "message_start",
                "message": {
                    "id": msg_id, "type": "message", "role": "assistant",
                    "content": [], "model": model, "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            })).unwrap()
        );
        yield Ok(Bytes::from(start_evt));

        let mut buf = String::new();
        let mut content_block_open = false;
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        let mut output_tokens: u64 = 0;
        let mut input_tokens: u64 = 0;
        let byte_stream = body.into_data_stream();
        futures_util::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(_) => break };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }
                let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };

                // Collect usage from final chunk (stream_options.include_usage).
                if let Some(u) = ev.get("usage") {
                    input_tokens  = u["prompt_tokens"].as_u64().unwrap_or(input_tokens);
                    output_tokens = u["completion_tokens"].as_u64().unwrap_or(output_tokens);
                }

                let choice = &ev["choices"][0];
                let delta = &choice["delta"];
                let finish = choice["finish_reason"].as_str();

                // Text delta.
                if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                    if !content_block_open {
                        content_block_open = true;
                        let cb = format!(
                            "event: content_block_start\ndata: {}\n\n",
                            serde_json::to_string(&json!({
                                "type": "content_block_start", "index": 0,
                                "content_block": {"type": "text", "text": ""}
                            })).unwrap()
                        );
                        yield Ok(Bytes::from(cb));
                    }
                    let d = format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        serde_json::to_string(&json!({
                            "type": "content_block_delta", "index": 0,
                            "delta": {"type": "text_delta", "text": text}
                        })).unwrap()
                    );
                    yield Ok(Bytes::from(d));
                }

                // Tool call deltas.
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let oai_idx = tc["index"].as_u64().unwrap_or(0);
                        // New tool call: emit content_block_start for tool_use.
                        if let Some(id) = tc["id"].as_str() {
                            let name = tc["function"]["name"].as_str().unwrap_or("").to_owned();
                            let my_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(oai_idx, (my_idx, id.to_owned(), name.clone()));
                            let cb = format!(
                                "event: content_block_start\ndata: {}\n\n",
                                serde_json::to_string(&json!({
                                    "type": "content_block_start",
                                    "index": my_idx + 1, // +1: text block at 0
                                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                                })).unwrap()
                            );
                            yield Ok(Bytes::from(cb));
                        }
                        // Streaming arguments.
                        if let Some(args_chunk) = tc["function"]["arguments"].as_str() {
                            if let Some(&(my_idx, _, _)) = tool_blocks.get(&oai_idx) {
                                let d = format!(
                                    "event: content_block_delta\ndata: {}\n\n",
                                    serde_json::to_string(&json!({
                                        "type": "content_block_delta",
                                        "index": my_idx + 1,
                                        "delta": {"type": "input_json_delta", "partial_json": args_chunk}
                                    })).unwrap()
                                );
                                yield Ok(Bytes::from(d));
                            }
                        }
                    }
                }

                // Finish reason → close blocks + message_delta + message_stop.
                if let Some(fr) = finish {
                    let stop_reason = match fr {
                        "stop"       => "end_turn",
                        "tool_calls" => "tool_use",
                        "length"     => "max_tokens",
                        other        => other,
                    };

                    // Close open content/tool blocks.
                    if content_block_open {
                        yield Ok(Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            serde_json::to_string(&json!({"type":"content_block_stop","index":0})).unwrap()
                        )));
                    }
                    for (_, (my_idx, _, _)) in &tool_blocks {
                        yield Ok(Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            serde_json::to_string(&json!({"type":"content_block_stop","index": my_idx + 1})).unwrap()
                        )));
                    }

                    yield Ok(Bytes::from(format!(
                        "event: message_delta\ndata: {}\n\n",
                        serde_json::to_string(&json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": {"output_tokens": output_tokens}
                        })).unwrap()
                    )));
                    yield Ok(Bytes::from(
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    ));
                }
            }
        }
    }
}

/// Stream-translate an Anthropic SSE response body (from axum `Body`) into OpenAI SSE format.
/// Equivalent to `translate_anthropic_stream` but consumes an axum `Body` instead of a
/// `reqwest::Response`, so it can be used after the forwarder returns.
pub fn translate_body_anthropic_to_openai(
    body: Body,
    chat_id: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::stream! {
        let id = chat_id;

        // Initial role chunk.
        let init = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": id, "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
            })).unwrap()
        );
        yield Ok(Bytes::from(init));

        let mut buf = String::new();
        let mut tool_blocks: std::collections::HashMap<u64, (usize, String, String)> = std::collections::HashMap::new();
        let mut tool_call_count: usize = 0;
        let byte_stream = body.into_data_stream();
        futures_util::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(_) => break };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_owned();
                buf = buf[nl + 1..].to_owned();
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { continue; }
                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                let event_type = event["type"].as_str().unwrap_or("");

                let maybe_chunk = match event_type {
                    "content_block_start" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let cb = &event["content_block"];
                        if cb["type"].as_str() == Some("tool_use") {
                            let tool_id = cb["id"].as_str().unwrap_or("").to_owned();
                            let tool_name = cb["name"].as_str().unwrap_or("").to_owned();
                            let oai_idx = tool_call_count;
                            tool_call_count += 1;
                            tool_blocks.insert(block_idx, (oai_idx, tool_id.clone(), tool_name.clone()));
                            Some(json!({
                                "id": id, "object": "chat.completion.chunk",
                                "choices": [{"index": 0, "delta": {
                                    "tool_calls": [{"index": oai_idx, "id": tool_id, "type": "function",
                                        "function": {"name": tool_name, "arguments": ""}}]
                                }, "finish_reason": null}]
                            }))
                        } else { None }
                    }
                    "content_block_delta" => {
                        let block_idx = event["index"].as_u64().unwrap_or(0);
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                let text = delta["text"].as_str().unwrap_or("");
                                if text.is_empty() { continue; }
                                Some(json!({
                                    "id": id, "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                                }))
                            }
                            Some("input_json_delta") => {
                                let args = delta["partial_json"].as_str().unwrap_or("");
                                tool_blocks.get(&block_idx).map(|(oai_idx, _, _)| json!({
                                    "id": id, "object": "chat.completion.chunk",
                                    "choices": [{"index": 0, "delta": {
                                        "tool_calls": [{"index": oai_idx, "function": {"arguments": args}}]
                                    }, "finish_reason": null}]
                                }))
                            }
                            _ => None,
                        }
                    }
                    "message_delta" => {
                        let stop_reason = event["delta"]["stop_reason"].as_str().unwrap_or("stop");
                        let finish = match stop_reason {
                            "end_turn"   => "stop",
                            "tool_use"   => "tool_calls",
                            "max_tokens" => "length",
                            other        => other,
                        };
                        Some(json!({
                            "id": id, "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
                        }))
                    }
                    _ => None,
                };

                if let Some(c) = maybe_chunk {
                    let out = format!("data: {}\n\n", serde_json::to_string(&c).unwrap());
                    yield Ok(Bytes::from(out));
                }
            }
        }
        yield Ok(Bytes::from("data: [DONE]\n\n"));
    }
}

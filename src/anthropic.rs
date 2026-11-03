//! Anthropic `/v1/messages` <-> OpenAI `/v1/chat/completions` translation.
//!
//! The engines behind CART only speak chat-completions, so a `/v1/messages`
//! request is rewritten before routing (which then sees the same body shape as
//! a native chat request) and the response — JSON or SSE — is rewritten back.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use rand::Rng;
use serde_json::{json, Map, Value};
use std::convert::Infallible;

/// Claude Code prepends an attribution block to `system`
/// (`x-anthropic-billing-header: cc_version=…; cc_entrypoint=…;`). It means
/// nothing to the model, and since it sits at the very front and differs by
/// client version and entrypoint, it would split the engine's prefix cache
/// between otherwise identical prompts — so it is dropped.
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Tool-use ids we hand out are `toolu_<nonce>_<hex(upstream id)>`.
/// Engines reuse ids across turns (`get_weather:0` on every turn), which the
/// client would see as duplicates; the nonce makes them unique and the hex
/// tail lets us give the engine its original id back on the next request.
const TOOL_ID_PREFIX: &str = "toolu_";

fn str_field<'v>(v: &'v Value, key: &str) -> &'v str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn block_type(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn encode_tool_id(upstream: &str) -> String {
    let nonce: u64 = rand::rng().random();
    let mut id = format!("{TOOL_ID_PREFIX}{nonce:016x}_");
    for b in upstream.as_bytes() {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

fn decode_tool_id(id: &str) -> String {
    id.strip_prefix(TOOL_ID_PREFIX)
        .and_then(|rest| rest.split_once('_'))
        .and_then(|(_, hex)| decode_hex(hex))
        .unwrap_or_else(|| id.to_string())
}

fn decode_hex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

// ------------------------------------------------------------------
// Request: Anthropic -> OpenAI
// ------------------------------------------------------------------

/// The request's `stop_sequences`, needed to report which one matched.
pub fn stop_sequences(req: &Value) -> Vec<String> {
    req.get("stop_sequences")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a `/v1/messages` body into a `/v1/chat/completions` body.
/// Returns an error message for content chat-completions cannot carry.
pub fn request_to_openai(req: &Value) -> Result<Value, String> {
    let obj = req
        .as_object()
        .ok_or("request body must be a JSON object")?;
    let mut out = Map::new();
    out.insert(
        "model".into(),
        obj.get("model").cloned().unwrap_or_else(|| json!("")),
    );
    // top_k is not OpenAI, but both SGLang and vLLM accept it.
    for key in ["max_tokens", "temperature", "top_p", "top_k"] {
        if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    let stops = stop_sequences(req);
    if !stops.is_empty() {
        out.insert("stop".into(), json!(stops));
    }
    if obj.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("stream".into(), json!(true));
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }

    let tools = convert_tools(obj.get("tools"));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = obj.get("tool_choice") {
            convert_tool_choice(tc, &mut out);
        }
    }

    let mut messages = Vec::new();
    if let Some(system) = convert_system(obj.get("system")) {
        messages.push(json!({"role": "system", "content": system}));
    }
    let input = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages must be an array")?;
    for msg in input {
        convert_message(msg, &mut messages)?;
    }
    out.insert("messages".into(), Value::Array(messages));
    Ok(Value::Object(out))
}

/// Client tools only: server tools (`web_search_*`, `bash_*`, …) have no
/// chat-completions equivalent and are dropped.
fn convert_tools(tools: Option<&Value>) -> Vec<Value> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter(|t| matches!(block_type(t), None | Some("custom")))
        .map(|t| {
            let mut function = Map::new();
            function.insert("name".into(), json!(str_field(t, "name")));
            if let Some(d) = t.get("description") {
                function.insert("description".into(), d.clone());
            }
            function.insert(
                "parameters".into(),
                t.get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            json!({"type": "function", "function": function})
        })
        .collect()
}

fn convert_tool_choice(tc: &Value, out: &mut Map<String, Value>) {
    let choice = match block_type(tc) {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("none") => json!("none"),
        Some("tool") => json!({"type": "function", "function": {"name": str_field(tc, "name")}}),
        _ => return,
    };
    out.insert("tool_choice".into(), choice);
    if tc.get("disable_parallel_tool_use").and_then(Value::as_bool) == Some(true) {
        out.insert("parallel_tool_calls".into(), json!(false));
    }
}

fn convert_system(system: Option<&Value>) -> Option<String> {
    let text = match system? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.starts_with(BILLING_HEADER_PREFIX))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn convert_message(msg: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
    let blocks = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": role, "content": s}));
            return Ok(());
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            return Err(format!(
                "{role} message content must be a string or an array"
            ))
        }
    };
    if role == "assistant" {
        out.push(convert_assistant(blocks));
        Ok(())
    } else {
        convert_user(blocks, out)
    }
}

fn convert_assistant(blocks: &[Value]) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        match block_type(b) {
            Some("text") => text.push_str(str_field(b, "text")),
            Some("thinking") => reasoning.push_str(str_field(b, "thinking")),
            Some("tool_use") => tool_calls.push(json!({
                "id": decode_tool_id(str_field(b, "id")),
                "type": "function",
                "function": {
                    "name": str_field(b, "name"),
                    "arguments": b.get("input").map_or_else(|| "{}".to_string(), Value::to_string),
                },
            })),
            _ => {}
        }
    }
    let mut m = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        m["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        m["tool_calls"] = Value::Array(tool_calls);
    }
    m
}

/// A user turn becomes its `tool` messages first (chat-completions requires
/// them right after the assistant's `tool_calls`), then one `user` message
/// with everything else — text alongside tool results is common in Claude
/// Code (e.g. reminders appended after a tool result).
fn convert_user(blocks: &[Value], out: &mut Vec<Value>) -> Result<(), String> {
    let mut parts = Vec::new();
    for b in blocks {
        match block_type(b) {
            Some("text") => parts.push(json!({"type": "text", "text": str_field(b, "text")})),
            Some("image") => parts.push(convert_image(b)?),
            Some("tool_result") => {
                let (content, images) = tool_result_content(b)?;
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": decode_tool_id(str_field(b, "tool_use_id")),
                    "content": content,
                }));
                // Tool messages are text-only; images ride in the user message.
                parts.extend(images);
            }
            Some("document") => return Err("document content blocks are not supported".into()),
            _ => {}
        }
    }
    if !parts.is_empty() {
        out.push(json!({"role": "user", "content": collapse_text_parts(parts)}));
    }
    Ok(())
}

/// Plain string content when there are only text parts: every chat template
/// handles a string, not every one handles a part list.
fn collapse_text_parts(parts: Vec<Value>) -> Value {
    if parts.iter().all(|p| block_type(p) == Some("text")) {
        let texts: Vec<&str> = parts.iter().map(|p| str_field(p, "text")).collect();
        json!(texts.join("\n"))
    } else {
        Value::Array(parts)
    }
}

fn convert_image(b: &Value) -> Result<Value, String> {
    let src = b.get("source").ok_or("image block without source")?;
    let url = match block_type(src) {
        Some("base64") => format!(
            "data:{};base64,{}",
            str_field(src, "media_type"),
            str_field(src, "data")
        ),
        Some("url") => str_field(src, "url").to_string(),
        other => {
            return Err(format!(
                "unsupported image source type: {}",
                other.unwrap_or("none")
            ))
        }
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn tool_result_content(b: &Value) -> Result<(String, Vec<Value>), String> {
    let mut text = String::new();
    let mut images = Vec::new();
    match b.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => text.push_str(s),
        Some(Value::Array(items)) => {
            for item in items {
                match block_type(item) {
                    Some("text") => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(str_field(item, "text"));
                    }
                    Some("image") => images.push(convert_image(item)?),
                    Some("document") => {
                        return Err("document content blocks are not supported".into())
                    }
                    _ => {}
                }
            }
        }
        Some(other) => text.push_str(&other.to_string()),
    }
    if b.get("is_error").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    Ok((text, images))
}

// ------------------------------------------------------------------
// Response: OpenAI -> Anthropic
// ------------------------------------------------------------------

fn stop_reason(
    finish: Option<&str>,
    matched: Option<&str>,
    stop_sequences: &[String],
    saw_tool: bool,
) -> (&'static str, Value) {
    match finish {
        Some("length") => ("max_tokens", Value::Null),
        Some("content_filter") => ("refusal", Value::Null),
        Some("tool_calls") => ("tool_use", Value::Null),
        _ if saw_tool => ("tool_use", Value::Null),
        _ => match matched.filter(|m| stop_sequences.iter().any(|s| s == m)) {
            Some(m) => ("stop_sequence", json!(m)),
            None => ("end_turn", Value::Null),
        },
    }
}

/// SGLang reports the matched stop string as `matched_stop`, vLLM as `stop_reason`.
fn matched_stop(choice: &Value) -> Option<String> {
    choice
        .get("matched_stop")
        .or_else(|| choice.get("stop_reason"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Anthropic's `input_tokens` excludes cache reads; clients add the two back
/// up to get the context size.
fn convert_usage(usage: Option<&Value>) -> Value {
    let get = |ptr: &str| {
        usage
            .and_then(|u| u.pointer(ptr))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let prompt = get("/prompt_tokens");
    let cached = get("/prompt_tokens_details/cached_tokens").min(prompt);
    json!({
        "input_tokens": prompt - cached,
        "output_tokens": get("/completion_tokens"),
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": cached,
    })
}

fn tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({"_raw": arguments}))
}

pub fn response_to_anthropic(resp: &Value, model: &str, stop_sequences: &[String]) -> Value {
    let choice = resp.pointer("/choices/0").unwrap_or(&Value::Null);
    let msg = &choice["message"];
    let mut content = Vec::new();
    let reasoning = str_field(msg, "reasoning_content");
    if !reasoning.is_empty() {
        content.push(json!({"type": "thinking", "thinking": reasoning, "signature": ""}));
    }
    let text = str_field(msg, "content");
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    let mut saw_tool = false;
    for tc in msg["tool_calls"].as_array().into_iter().flatten() {
        saw_tool = true;
        let f = &tc["function"];
        content.push(json!({
            "type": "tool_use",
            "id": encode_tool_id(str_field(tc, "id")),
            "name": str_field(f, "name"),
            "input": tool_input(str_field(f, "arguments")),
        }));
    }
    let (reason, seq) = stop_reason(
        choice.get("finish_reason").and_then(Value::as_str),
        matched_stop(choice).as_deref(),
        stop_sequences,
        saw_tool,
    );
    json!({
        "id": format!("msg_{}", str_field(resp, "id")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": reason,
        "stop_sequence": seq,
        "usage": convert_usage(resp.get("usage")),
    })
}

// ------------------------------------------------------------------
// Streaming: OpenAI SSE -> Anthropic SSE
// ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Block {
    Thinking,
    Text,
    Tool(u64),
}

/// Stateful converter from `chat.completion.chunk` SSE to Anthropic events.
/// Exactly one content block is open at a time, as Anthropic's stream requires.
pub struct StreamConverter {
    model: String,
    stop_sequences: Vec<String>,
    buf: Vec<u8>,
    started: bool,
    finished: bool,
    open: Option<Block>,
    next_index: usize,
    saw_tool: bool,
    finish_reason: Option<String>,
    matched_stop: Option<String>,
    usage: Option<Value>,
}

fn push_event(out: &mut String, name: &str, data: Value) {
    out.push_str("event: ");
    out.push_str(name);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

impl StreamConverter {
    pub fn new(model: &str, stop_sequences: Vec<String>) -> Self {
        Self {
            model: model.to_string(),
            stop_sequences,
            buf: Vec::new(),
            started: false,
            finished: false,
            open: None,
            next_index: 0,
            saw_tool: false,
            finish_reason: None,
            matched_stop: None,
            usage: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Feed raw upstream bytes; returns the Anthropic SSE text to send.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim_end().strip_prefix("data:") {
                self.handle_data(data.trim_start(), &mut out);
            }
        }
        out
    }

    /// Upstream closed. A stream that ended without `[DONE]` but with a
    /// finish reason is complete; one without either was cut off.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.finished {
        } else if self.finish_reason.is_some() {
            self.finish_into(&mut out);
        } else {
            self.error_into("api_error", "upstream stream ended unexpectedly", &mut out);
        }
        out
    }

    pub fn fail(&mut self, message: &str) -> String {
        let mut out = String::new();
        if !self.finished {
            self.error_into("api_error", message, &mut out);
        }
        out
    }

    fn handle_data(&mut self, data: &str, out: &mut String) {
        if self.finished {
            return;
        }
        if data == "[DONE]" {
            self.finish_into(out);
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            self.error_into("api_error", &msg, out);
            return;
        }
        self.start_into(str_field(&chunk, "id"), out);
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return;
        };
        let delta = &choice["delta"];

        let reasoning = str_field(delta, "reasoning_content");
        if !reasoning.is_empty() {
            if self.open != Some(Block::Thinking) {
                self.open_block(
                    Block::Thinking,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                    out,
                );
            }
            self.delta_into(
                json!({"type": "thinking_delta", "thinking": reasoning}),
                out,
            );
        }

        let text = str_field(delta, "content");
        if !text.is_empty() {
            if self.open != Some(Block::Text) {
                self.open_block(Block::Text, json!({"type": "text", "text": ""}), out);
            }
            self.delta_into(json!({"type": "text_delta", "text": text}), out);
        }

        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let f = &call["function"];
            if self.open != Some(Block::Tool(idx)) {
                self.saw_tool = true;
                let block = json!({
                    "type": "tool_use",
                    "id": encode_tool_id(str_field(call, "id")),
                    "name": str_field(f, "name"),
                    "input": {},
                });
                self.open_block(Block::Tool(idx), block, out);
            }
            let args = str_field(f, "arguments");
            if !args.is_empty() {
                self.delta_into(
                    json!({"type": "input_json_delta", "partial_json": args}),
                    out,
                );
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
            self.matched_stop = matched_stop(choice);
        }
    }

    fn start_into(&mut self, upstream_id: &str, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        push_event(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{upstream_id}"),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        );
    }

    fn open_block(&mut self, kind: Block, block: Value, out: &mut String) {
        self.close_into(out);
        push_event(
            out,
            "content_block_start",
            json!({"type": "content_block_start", "index": self.next_index, "content_block": block}),
        );
        self.open = Some(kind);
        self.next_index += 1;
    }

    fn delta_into(&self, delta: Value, out: &mut String) {
        push_event(
            out,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": self.next_index - 1, "delta": delta}),
        );
    }

    fn close_into(&mut self, out: &mut String) {
        if self.open.take().is_some() {
            push_event(
                out,
                "content_block_stop",
                json!({"type": "content_block_stop", "index": self.next_index - 1}),
            );
        }
    }

    fn finish_into(&mut self, out: &mut String) {
        self.start_into("", out);
        self.close_into(out);
        let (reason, seq) = stop_reason(
            self.finish_reason.as_deref(),
            self.matched_stop.as_deref(),
            &self.stop_sequences,
            self.saw_tool,
        );
        push_event(
            out,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": seq},
                "usage": convert_usage(self.usage.as_ref()),
            }),
        );
        push_event(out, "message_stop", json!({"type": "message_stop"}));
        self.finished = true;
    }

    fn error_into(&mut self, etype: &str, message: &str, out: &mut String) {
        push_event(
            out,
            "error",
            json!({"type": "error", "error": {"type": etype, "message": message}}),
        );
        self.finished = true;
    }
}

fn convert_stream(body: Body, conv: StreamConverter) -> Body {
    let upstream = body.into_data_stream();
    // Dropping `upstream` (when this stream ends or the client goes away)
    // drops the proxy's load-tracking wrapper with it.
    let s = stream::unfold(Some((upstream, conv)), |state| async move {
        let (mut up, mut conv) = state?;
        loop {
            let (out, done) = match up.next().await {
                Some(Ok(bytes)) => {
                    let out = conv.feed(&bytes);
                    let done = conv.is_finished();
                    (out, done)
                }
                Some(Err(e)) => (conv.fail(&format!("upstream stream error: {e}")), true),
                None => (conv.finish(), true),
            };
            if done {
                return (!out.is_empty()).then(|| (Ok::<_, Infallible>(Bytes::from(out)), None));
            }
            if !out.is_empty() {
                return Some((Ok(Bytes::from(out)), Some((up, conv))));
            }
        }
    });
    Body::from_stream(s)
}

// ------------------------------------------------------------------
// Errors
// ------------------------------------------------------------------

fn error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

pub fn error_response(status: StatusCode, message: &str) -> Response {
    let body = json!({"type": "error", "error": {"type": error_type(status), "message": message}});
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Pull a message out of an OpenAI / FastAPI / plain-text error body.
fn upstream_error_message(status: StatusCode, body: &[u8]) -> String {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let from_json = parsed.as_ref().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .or_else(|| v.get("error"))
            .and_then(Value::as_str)
            .map(String::from)
    });
    let msg = from_json.unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    let msg = if msg.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string()
    } else {
        msg
    };
    // Clients recognise an over-long prompt by this phrase.
    let lower = msg.to_ascii_lowercase();
    if status == StatusCode::BAD_REQUEST && lower.contains("context length") {
        format!("prompt is too long: {msg}")
    } else {
        msg
    }
}

/// Rewrite the proxied chat-completions response as a `/v1/messages` one.
pub async fn translate_response(
    resp: Response,
    model: &str,
    stop_sequences: Vec<String>,
) -> Response {
    let (parts, body) = resp.into_parts();
    let routed_peer = parts.headers.get("x-routed-peer").cloned();
    let is_sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    let mut out = if !parts.status.is_success() {
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap_or_default();
        error_response(parts.status, &upstream_error_message(parts.status, &bytes))
    } else if is_sse {
        let mut r = Response::new(convert_stream(
            body,
            StreamConverter::new(model, stop_sequences),
        ));
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        r
    } else {
        match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    response_to_anthropic(&v, model, &stop_sequences).to_string(),
                )
                    .into_response(),
                Err(e) => error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream returned invalid JSON: {e}"),
                ),
            },
            Err(e) => error_response(
                StatusCode::BAD_GATEWAY,
                &format!("failed to read upstream response: {e}"),
            ),
        }
    };
    if let Some(peer) = routed_peer {
        out.headers_mut().insert("x-routed-peer", peer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(req: Value) -> Value {
        request_to_openai(&req).expect("conversion should succeed")
    }

    /// Parse emitted SSE into (event name, data) pairs.
    fn events(sse: &str) -> Vec<(String, Value)> {
        sse.split("\n\n")
            .filter(|e| !e.is_empty())
            .map(|e| {
                let mut lines = e.lines();
                let name = lines.next().unwrap().strip_prefix("event: ").unwrap();
                let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
                (name.to_string(), serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    fn names(evs: &[(String, Value)]) -> Vec<&str> {
        evs.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn test_tool_id_roundtrip() {
        let id = encode_tool_id("get_weather:0");
        assert!(id.starts_with("toolu_"));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert_eq!(decode_tool_id(&id), "get_weather:0");
        assert_ne!(id, encode_tool_id("get_weather:0"), "ids must be unique");
    }

    #[test]
    fn test_foreign_tool_id_passes_through() {
        assert_eq!(decode_tool_id("toolu_01AbCdEf"), "toolu_01AbCdEf");
        assert_eq!(decode_tool_id("toolu_x_zz"), "toolu_x_zz");
        assert_eq!(decode_tool_id("call_123"), "call_123");
    }

    #[test]
    fn test_scalars_and_stream() {
        let out = convert(json!({
            "model": "kimi-k3", "max_tokens": 100, "temperature": 0.5, "top_p": 0.9, "top_k": 20,
            "stop_sequences": ["END"], "stream": true, "metadata": {"user_id": "u"},
            "thinking": {"type": "enabled", "budget_tokens": 1000},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            out,
            json!({
                "model": "kimi-k3", "max_tokens": 100, "temperature": 0.5, "top_p": 0.9, "top_k": 20,
                "stop": ["END"], "stream": true, "stream_options": {"include_usage": true},
                "messages": [{"role": "user", "content": "hi"}],
            })
        );
    }

    #[test]
    fn test_system_blocks_drop_billing_header() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1; cch=abc;"},
                {"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Be brief."},
            ],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            out["messages"][0],
            json!({"role": "system", "content": "You are helpful.\nBe brief."})
        );
    }

    #[test]
    fn test_tools_and_tool_choice() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "tools": [
                {"name": "Read", "description": "read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}, "cache_control": {"type": "ephemeral"}},
                {"type": "web_search_20250305", "name": "web_search"},
                {"type": "custom", "name": "NoSchema"},
            ],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            out["tools"],
            json!([
                {"type": "function", "function": {"name": "Read", "description": "read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}},
                {"type": "function", "function": {"name": "NoSchema", "parameters": {"type": "object", "properties": {}}}},
            ])
        );
        assert_eq!(out["tool_choice"], json!("required"));
        assert_eq!(out["parallel_tool_calls"], json!(false));

        let named = convert(json!({
            "model": "m", "max_tokens": 1,
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "Read"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            named["tool_choice"],
            json!({"type": "function", "function": {"name": "Read"}})
        );
        assert!(named.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn test_tool_choice_dropped_without_client_tools() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "any"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn test_tool_turn_roundtrip() {
        let tool_id = encode_tool_id("get_weather:0");
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "weather?"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "need tool", "signature": ""},
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": tool_id, "name": "get_weather", "input": {"city": "Paris"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": tool_id, "content": [{"type": "text", "text": "sunny"}, {"type": "text", "text": "20C"}]},
                    {"type": "text", "text": "<system-reminder>be nice</system-reminder>"},
                ]},
            ],
        }));
        assert_eq!(
            out["messages"],
            json!([
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": "Checking.", "reasoning_content": "need tool", "tool_calls": [
                    {"id": "get_weather:0", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "get_weather:0", "content": "sunny\n20C"},
                {"role": "user", "content": "<system-reminder>be nice</system-reminder>"},
            ])
        );
    }

    #[test]
    fn test_tool_result_error_and_image() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": "file not found"},
                {"type": "tool_result", "tool_use_id": "call_2", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ]},
            ]}],
        }));
        assert_eq!(
            out["messages"],
            json!([
                {"role": "tool", "tool_call_id": "call_1", "content": "Error: file not found"},
                {"role": "tool", "tool_call_id": "call_2", "content": ""},
                {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
            ])
        );
    }

    #[test]
    fn test_images_keep_part_list() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
            ]}],
        }));
        assert_eq!(
            out["messages"][0]["content"],
            json!([
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            ])
        );
    }

    #[test]
    fn test_remote_image_is_caught_by_media_guard_after_translation() {
        let out = convert(json!({
            "model": "m", "max_tokens": 1,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "http://evil.example.com/a.png"}},
            ]}],
        }));
        assert!(
            crate::proxy::validate_remote_media(Some(&out), "/v1/chat/completions", 400).is_err()
        );
    }

    #[test]
    fn test_rejects_unsupported_content() {
        let doc = json!({"model": "m", "max_tokens": 1, "messages": [{"role": "user", "content": [
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "x"}},
        ]}]});
        assert!(request_to_openai(&doc).is_err());
        assert!(request_to_openai(&json!({"model": "m"})).is_err());
        assert!(request_to_openai(&json!([1])).is_err());
    }

    // Captured from SGLang serving kimi-k3 with tool-call and reasoning parsers.
    const SGLANG_JSON: &str = r#"{"id":"40cd65afe1014334bdd50a73a54f2960","object":"chat.completion","created":1793712870,"model":"kimi-k3","choices":[{"index":0,"message":{"role":"assistant","content":"I'll check the weather in Paris for you.","reasoning_content":"The user wants Paris weather.","tool_calls":[{"id":"get_weather:0","index":0,"type":"function","function":{"name":"get_weather","arguments":"{\"city\": \"Paris\"}"}}]},"logprobs":null,"finish_reason":"tool_calls","matched_stop":null}],"usage":{"prompt_tokens":170,"total_tokens":257,"completion_tokens":87,"prompt_tokens_details":null,"reasoning_tokens":35}}"#;

    #[test]
    fn test_non_streaming_response() {
        let resp: Value = serde_json::from_str(SGLANG_JSON).unwrap();
        let mut out = response_to_anthropic(&resp, "claude-sonnet", &[]);
        let id = out["content"][2]["id"].take();
        assert_eq!(decode_tool_id(id.as_str().unwrap()), "get_weather:0");
        assert_eq!(
            out,
            json!({
                "id": "msg_40cd65afe1014334bdd50a73a54f2960",
                "type": "message", "role": "assistant", "model": "claude-sonnet",
                "content": [
                    {"type": "thinking", "thinking": "The user wants Paris weather.", "signature": ""},
                    {"type": "text", "text": "I'll check the weather in Paris for you."},
                    {"type": "tool_use", "id": null, "name": "get_weather", "input": {"city": "Paris"}},
                ],
                "stop_reason": "tool_use", "stop_sequence": null,
                "usage": {"input_tokens": 170, "output_tokens": 87, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            })
        );
    }

    #[test]
    fn test_stop_reasons_and_cached_usage() {
        let resp = json!({"id": "x", "choices": [{"message": {"content": "a"}, "finish_reason": "stop", "matched_stop": "END"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 64}}});
        let out = response_to_anthropic(&resp, "m", &["END".to_string()]);
        assert_eq!(out["stop_reason"], "stop_sequence");
        assert_eq!(out["stop_sequence"], "END");
        assert_eq!(out["usage"]["input_tokens"], 36);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 64);

        // EOS token reported as matched_stop (an integer) is a normal end.
        let eos = json!({"choices": [{"message": {"content": "a"}, "finish_reason": "stop", "matched_stop": 163586}]});
        assert_eq!(
            response_to_anthropic(&eos, "m", &[])["stop_reason"],
            "end_turn"
        );
        let len = json!({"choices": [{"message": {"content": "a"}, "finish_reason": "length"}]});
        assert_eq!(
            response_to_anthropic(&len, "m", &[])["stop_reason"],
            "max_tokens"
        );
    }

    // Captured from SGLang (kimi-k3): reasoning, text, two parallel tool calls, usage chunk.
    const SGLANG_STREAM: &str = concat!(
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"reasoning_content":null,"role":"assistant","content":""},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"reasoning_content":"The user wants"},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"reasoning_content":" two calls."},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"role":null,"content":"I'll fetch both 天气.","reasoning_content":null,"tool_calls":null},"finish_reason":null}],"usage":null}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"id":"get_weather:0","index":0,"type":"function","function":{"name":"get_weather","arguments":"{\"city\": "}}]},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"id":"get_weather:1","index":1,"type":"function","function":{"name":"get_weather","arguments":"{\"city\": \"Tokyo\"}"}}]},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[{"index":0,"delta":{"reasoning_content":null},"finish_reason":"tool_calls","matched_stop":"<|close|>tools<|sep|>"}]}"#,
        "\n\n",
        r#"data: {"id":"8b8f","choices":[],"usage":{"prompt_tokens":173,"total_tokens":297,"completion_tokens":124,"prompt_tokens_details":{"cached_tokens":128}}}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );

    fn run_stream(input: &[u8], chunk_size: usize) -> String {
        let mut conv = StreamConverter::new("claude-sonnet", vec![]);
        let mut out = String::new();
        for piece in input.chunks(chunk_size) {
            out.push_str(&conv.feed(piece));
        }
        out.push_str(&conv.finish());
        out
    }

    #[test]
    fn test_streaming_conversion() {
        let evs = events(&run_stream(SGLANG_STREAM.as_bytes(), usize::MAX));
        assert_eq!(
            names(&evs),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(evs[0].1["message"]["id"], "msg_8b8f");
        assert_eq!(evs[0].1["message"]["model"], "claude-sonnet");
        assert_eq!(evs[1].1["content_block"]["type"], "thinking");
        assert_eq!(evs[3].1["delta"]["thinking"], " two calls.");
        assert_eq!(
            evs[6].1["delta"],
            json!({"type": "text_delta", "text": "I'll fetch both 天气."})
        );

        let tool0 = &evs[8].1;
        assert_eq!(tool0["index"], 2);
        assert_eq!(tool0["content_block"]["name"], "get_weather");
        assert_eq!(
            decode_tool_id(tool0["content_block"]["id"].as_str().unwrap()),
            "get_weather:0"
        );
        let args: String = [&evs[9].1, &evs[10].1]
            .iter()
            .map(|e| e["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(args, r#"{"city": "Paris"}"#);
        assert_eq!(evs[11].1["index"], 2);

        let tool1 = &evs[12].1;
        assert_eq!(tool1["index"], 3);
        assert_eq!(
            decode_tool_id(tool1["content_block"]["id"].as_str().unwrap()),
            "get_weather:1"
        );
        assert_eq!(evs[14].1["index"], 3);

        assert_eq!(
            evs[15].1,
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"input_tokens": 45, "output_tokens": 124, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 128}})
        );
    }

    #[test]
    fn test_streaming_survives_arbitrary_splits() {
        // Splitting inside multi-byte UTF-8 and mid-line must not change the output
        // (tool ids carry a random nonce, so compare with ids masked).
        let mask = |s: String| {
            events(&s)
                .into_iter()
                .map(|(n, mut d)| {
                    if let Some(id) = d.pointer_mut("/content_block/id") {
                        *id = json!("masked");
                    }
                    (n, d)
                })
                .collect::<Vec<_>>()
        };
        let whole = mask(run_stream(SGLANG_STREAM.as_bytes(), usize::MAX));
        for size in [1, 3, 7, 64] {
            assert_eq!(
                mask(run_stream(SGLANG_STREAM.as_bytes(), size)),
                whole,
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn test_stream_cut_off_emits_error() {
        let mut conv = StreamConverter::new("m", vec![]);
        let mut out = conv.feed(
            br#"data: {"id":"a","choices":[{"delta":{"content":"hel"},"finish_reason":null}]}
"#,
        );
        out.push_str(&conv.finish());
        let evs = events(&out);
        assert_eq!(
            names(&evs),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "error"
            ]
        );
        assert_eq!(conv.finish(), "");
    }

    #[test]
    fn test_stream_without_done_but_finished_is_complete() {
        let mut conv = StreamConverter::new("m", vec![]);
        let mut out = conv.feed(
            br#"data: {"id":"a","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}
"#,
        );
        out.push_str(&conv.finish());
        let evs = events(&out);
        assert_eq!(names(&evs).last(), Some(&"message_stop"));
        assert_eq!(evs[evs.len() - 2].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn test_stream_upstream_error_chunk() {
        let mut conv = StreamConverter::new("m", vec![]);
        let out = conv.feed(b"data: {\"error\":{\"message\":\"boom\"}}\n\ndata: [DONE]\n\n");
        let evs = events(&out);
        assert_eq!(names(&evs), ["error"]);
        assert_eq!(evs[0].1["error"]["message"], "boom");
    }

    #[test]
    fn test_upstream_error_message() {
        let ctx = br#"{"object":"error","message":"The input (2000000 tokens) is longer than the model's context length (1048576 tokens).","type":"BadRequestError","code":400}"#;
        assert!(upstream_error_message(StatusCode::BAD_REQUEST, ctx)
            .starts_with("prompt is too long: The input"));
        let oai = br#"{"error":{"message":"Remote image/video URLs are not permitted.","type":"invalid_request_error"}}"#;
        assert_eq!(
            upstream_error_message(StatusCode::BAD_REQUEST, oai),
            "Remote image/video URLs are not permitted."
        );
        assert_eq!(
            upstream_error_message(StatusCode::SERVICE_UNAVAILABLE, b"No available workers"),
            "No available workers"
        );
        assert_eq!(
            upstream_error_message(StatusCode::BAD_GATEWAY, b""),
            "Bad Gateway"
        );
        assert_eq!(
            error_type(StatusCode::SERVICE_UNAVAILABLE),
            "overloaded_error"
        );
    }

    #[tokio::test]
    async fn test_translate_error_response_keeps_status() {
        let upstream = (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
        let resp = translate_response(upstream, "m", vec![]).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        );
    }

    #[tokio::test]
    async fn test_translate_streaming_response() {
        let mut upstream = Response::new(Body::from(SGLANG_STREAM));
        upstream.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        upstream
            .headers_mut()
            .insert("x-routed-peer", HeaderValue::from_static("http://w1"));
        let resp = translate_response(upstream, "m", vec![]).await;
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        assert_eq!(resp.headers()["x-routed-peer"], "http://w1");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let evs = events(std::str::from_utf8(&body).unwrap());
        assert_eq!(names(&evs).last(), Some(&"message_stop"));
    }
}

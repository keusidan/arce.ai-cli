//! The conversation and the wire: messages as the CLI keeps them, and a streaming client for
//! OpenAI-compatible chat-completions endpoints.

use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::provider::ProviderConfig;
use crate::segmenter::{Piece, Segmenter};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// the arguments as the model wrote them (JSON text)
    pub arguments: String,
    /// provider data that must be sent back with the call (a thought signature)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

impl ToolCall {
    pub fn args(&self) -> Result<Map<String, Value>, String> {
        let text = self.arguments.trim();
        if text.is_empty() {
            return Ok(Map::new());
        }
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err("引数が JSON のオブジェクトではありません".into()),
            Err(e) => Err(format!("引数の JSON が読めません: {e}")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        name: String,
        content: String,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message::User { content: text.into() }
    }
    pub fn text(&self) -> &str {
        match self {
            Message::User { content } | Message::Assistant { content, .. } | Message::Tool { content, .. } => content,
        }
    }
    /// Everything that is sent of it, for counting.
    pub fn wire_len(&self) -> usize {
        match self {
            Message::Assistant { content, tool_calls, .. } => {
                content.len() + tool_calls.iter().map(|c| c.name.len() + c.arguments.len() + 16).sum::<usize>()
            }
            other => other.text().len(),
        }
    }
}

/// A tool as the model is told about it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn to_openai(&self) -> Value {
        json!({ "type": "function", "function": { "name": self.name, "description": self.description, "parameters": self.parameters } })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// None: the provider did not say (not the same as 0)
    pub cached: Option<u64>,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        if let Some(c) = o.cached {
            self.cached = Some(self.cached.unwrap_or(0) + c);
        }
    }
}

#[derive(Clone, Debug)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// a tool call started being written (its name when known)
    ToolCallStarted(String),
}

#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// tool calls written in the text form that could not be read
    pub unreadable_calls: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProviderError {
    pub status: Option<u16>,
    pub body: String,
    pub message: String,
    /// nothing of the reply had arrived yet (a fallback may still switch)
    pub before_output: bool,
    pub aborted: bool,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    fn new(status: Option<u16>, body: String, message: String) -> ProviderError {
        ProviderError { status, body, message, before_output: true, aborted: false }
    }
    pub fn aborted() -> ProviderError {
        ProviderError { status: None, body: String::new(), message: "止めました".into(), before_output: true, aborted: true }
    }
}

pub struct ChatRequest<'a> {
    pub provider: &'a ProviderConfig,
    pub model: &'a str,
    pub api_key: Option<String>,
    pub system: String,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    /// generated fields (thinking) laid under the provider's own `extra_body`
    pub extra: Map<String, Value>,
    pub cache_key: Option<String>,
    pub pass_reasoning: bool,
}

/// The system prompt's tool section for the text form.
pub fn text_tool_section(tools: &[ToolSpec]) -> String {
    let mut b = String::from(
        "\n\n# Tools\nYou may call one or more tools. Write each call on its own as\n<tool_call>\n{\"name\": \"<tool>\", \"arguments\": {…}}\n</tool_call>\nThe result comes back as <tool_response>…</tool_response>. The tools:\n",
    );
    for t in tools {
        b.push_str(&json!({ "name": t.name, "description": t.description, "parameters": t.parameters }).to_string());
        b.push('\n');
    }
    b
}

/// The messages for the wire. With native tools, calls and results keep their roles; in the text form
/// they become tags inside assistant and user messages, and user messages in a row are joined.
pub fn wire_messages(system: &str, messages: &[Message], native: bool, pass_reasoning: bool) -> Vec<Value> {
    let mut out: Vec<Value> = vec![json!({ "role": "system", "content": system })];
    // results that were given, so that a call without one is sent as text
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|m| if let Message::Tool { tool_call_id, .. } = m { Some(tool_call_id.as_str()) } else { None })
        .collect();
    let push_user = |out: &mut Vec<Value>, text: String| {
        if let Some(last) = out.last_mut()
            && last["role"] == "user"
            && let Some(prev) = last["content"].as_str()
        {
            last["content"] = Value::String(format!("{prev}\n\n{text}"));
            return;
        }
        out.push(json!({ "role": "user", "content": text }));
    };
    for m in messages {
        match m {
            Message::User { content } => push_user(&mut out, content.clone()),
            Message::Assistant { content, reasoning, tool_calls } => {
                let native_calls: Vec<&ToolCall> =
                    if native { tool_calls.iter().filter(|c| answered.contains(c.id.as_str())).collect() } else { vec![] };
                let mut text = content.clone();
                for c in tool_calls {
                    if !native_calls.iter().any(|n| n.id == c.id) {
                        let args: Value = serde_json::from_str(&c.arguments).unwrap_or(Value::String(c.arguments.clone()));
                        text.push_str(&format!("\n<tool_call>\n{}\n</tool_call>", json!({ "name": c.name, "arguments": args })));
                    }
                }
                let mut msg = json!({ "role": "assistant", "content": text });
                if !native_calls.is_empty() {
                    if text.is_empty() {
                        msg["content"] = Value::Null;
                    }
                    msg["tool_calls"] = Value::Array(
                        native_calls
                            .iter()
                            .map(|c| {
                                let mut v = json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": if c.arguments.trim().is_empty() { "{}" } else { c.arguments.as_str() } } });
                                if let Some(extra) = &c.extra {
                                    v["extra_content"] = extra.clone();
                                }
                                v
                            })
                            .collect(),
                    );
                }
                if pass_reasoning && let Some(r) = reasoning.as_ref().filter(|r| !r.is_empty()) {
                    msg["reasoning_content"] = Value::String(r.clone());
                }
                out.push(msg);
            }
            Message::Tool { tool_call_id, content, .. } => {
                if native {
                    out.push(json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content }));
                } else {
                    push_user(&mut out, format!("<tool_response>\n{content}\n</tool_response>"));
                }
            }
        }
    }
    out
}

pub fn request_body(req: &ChatRequest, native: bool) -> Value {
    let mut system = req.system.clone();
    if !native && !req.tools.is_empty() {
        system.push_str(&text_tool_section(req.tools));
    }
    let mut body = json!({
        "model": req.model,
        "messages": wire_messages(&system, req.messages, native, req.pass_reasoning),
        "stream": true,
    });
    if req.provider.stream_usage {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if native && !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(ToolSpec::to_openai).collect());
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = json!(p);
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if let Some(key) = &req.cache_key {
        body[req.provider.prompt_cache_field.as_str()] = json!(key);
    }
    for (k, v) in &req.extra {
        body[k.as_str()] = v.clone();
    }
    for (k, v) in &req.provider.extra_body {
        body[k.as_str()] = v.clone();
    }
    body
}

/// A tool call being put together from pieces.
#[derive(Default, Debug)]
struct Partial {
    index: i64,
    id: String,
    name: String,
    args: String,
    extra: Option<Value>,
}

#[derive(Default)]
pub struct CallAssembler {
    calls: Vec<Partial>,
}

impl CallAssembler {
    /// One element of `delta.tool_calls`. A piece with the index of an earlier call but another id is a
    /// new call (one provider numbers every call of a reply 0 and tells them apart by id only).
    pub fn push(&mut self, piece: &Value) -> Option<String> {
        let index = piece.get("index").and_then(Value::as_i64).unwrap_or(0);
        let id = piece.get("id").and_then(Value::as_str).unwrap_or("");
        let f = piece.get("function");
        let name = f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("");
        let args = f.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("");
        let extra = piece.get("extra_content").cloned();
        let pos = self.calls.iter().rposition(|c| c.index == index);
        let starts_new = match pos {
            None => true,
            Some(p) => !id.is_empty() && !self.calls[p].id.is_empty() && self.calls[p].id != id,
        };
        let mut started = None;
        let at = if starts_new {
            self.calls.push(Partial { index, ..Default::default() });
            if !name.is_empty() {
                started = Some(name.to_string());
            }
            self.calls.len() - 1
        } else {
            pos.unwrap_or(0)
        };
        let call = &mut self.calls[at];
        if !id.is_empty() && call.id.is_empty() {
            call.id = id.to_string();
        }
        if !name.is_empty() && call.name.is_empty() {
            call.name = name.to_string();
            if started.is_none() {
                started = Some(name.to_string());
            }
        }
        call.args.push_str(args);
        if extra.is_some() {
            call.extra = extra;
        }
        started
    }

    pub fn finish(self) -> Vec<ToolCall> {
        let mut out = vec![];
        for (n, c) in self.calls.into_iter().enumerate() {
            let objects = split_objects(&c.args);
            let pieces = if objects.len() > 1 { objects } else { vec![c.args.clone()] };
            let many = pieces.len() > 1;
            for (k, args) in pieces.into_iter().enumerate() {
                let id = if c.id.is_empty() || many { format!("call_{n}_{k}") } else { c.id.clone() };
                out.push(ToolCall { id, name: c.name.clone(), arguments: args, extra: c.extra.clone() });
            }
        }
        out
    }
}

/// `{…}{…}` (calls sent back to back as one) split into objects; strings are not counted.
pub fn split_objects(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0i32;
    let mut start = None;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start.take()
                {
                    out.push(text[s..=i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// A tool call written in the text form: `{"name": …, "arguments": {…}}`.
pub fn parse_text_call(json_text: &str, n: usize) -> Option<ToolCall> {
    let v: Value = serde_json::from_str(json_text.trim()).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let args = v.get("arguments").or_else(|| v.get("parameters")).cloned().unwrap_or(json!({}));
    let arguments = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };
    Some(ToolCall { id: format!("call_t{n}"), name, arguments, extra: None })
}

fn usage_of(v: &Value) -> Option<Usage> {
    let u = v.get("usage").filter(|u| u.is_object())?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| n("prompt_cache_hit_tokens"));
    Some(Usage { input: n("prompt_tokens").unwrap_or(0), output: n("completion_tokens").unwrap_or(0), cached })
}

pub fn http_client(connect_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .user_agent(concat!("arce-ai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

fn chat_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = b.strip_suffix("/chat/completions").unwrap_or(b);
    format!("{b}/chat/completions")
}

/// Sends [req] and streams the answer to [on_event]. Thinking (`reasoning_content`, `reasoning` or
/// `<think>`), text and tool calls are told apart; the usage is read when the provider sends it.
pub async fn stream_chat(
    client: &reqwest::Client,
    req: &ChatRequest<'_>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
    cancel: &CancellationToken,
    idle_timeout: Duration,
) -> Result<Completion, ProviderError> {
    if req.provider.id == "demo" {
        return demo_reply(req, on_event, cancel).await;
    }
    let native = req.provider.uses_native_tools();
    let body = request_body(req, native);
    let mut builder = client.post(chat_url(&req.provider.base_url)).json(&body);
    if let Some(key) = &req.api_key {
        builder = builder.bearer_auth(key);
    }
    for (k, v) in &req.provider.headers {
        builder = builder.header(k, v);
    }
    let send = builder.send();
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(ProviderError::aborted()),
        r = tokio::time::timeout(idle_timeout, send) => match r {
            Err(_) => return Err(ProviderError::new(None, String::new(), "応答がありませんでした（時間切れ）".into())),
            Ok(Err(e)) => return Err(ProviderError::new(None, String::new(), format!("接続できませんでした: {}", without_url(&e)))),
            Ok(Ok(r)) => r,
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let body: String = text.chars().take(4000).collect();
        let (_, reason) = crate::provider::classify(Some(status.as_u16()), &body);
        let said = crate::provider::provider_message(&body, 300).map(|m| format!("。提供元の答え: {m}")).unwrap_or_else(|| {
            if body.trim().is_empty() { String::new() } else { format!("。本文: {}", body.chars().take(120).collect::<String>()) }
        });
        return Err(ProviderError::new(Some(status.as_u16()), body, format!("{reason}（HTTP {}）{said}", status.as_u16())));
    }
    let is_json =
        resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("application/json"));

    let mut seg = Segmenter::new(!native);
    let mut out = Completion::default();
    let mut calls = CallAssembler::default();
    let mut text_calls: Vec<String> = vec![];
    let mut any_output = false;

    let mut handle =
        |v: &Value, out: &mut Completion, any: &mut bool, on_event: &mut (dyn FnMut(StreamEvent) + Send)| -> Result<(), ProviderError> {
            if let Some(err) = v.get("error") {
                let msg = err.get("message").and_then(Value::as_str).unwrap_or("提供元がエラーを返しました").to_string();
                let mut e = ProviderError::new(err.get("code").and_then(Value::as_u64).map(|c| c as u16), v.to_string(), msg);
                e.before_output = !*any;
                return Err(e);
            }
            if let Some(u) = usage_of(v) {
                out.usage = Some(u);
            }
            let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else { return Ok(()) };
            let delta = choice.get("delta").or_else(|| choice.get("message")).cloned().unwrap_or(Value::Null);
            for key in ["reasoning_content", "reasoning"] {
                if let Some(r) = delta.get(key).and_then(Value::as_str).filter(|r| !r.is_empty()) {
                    *any = true;
                    out.reasoning.push_str(r);
                    on_event(StreamEvent::Reasoning(r.to_string()));
                    break;
                }
            }
            if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                *any = true;
                for piece in seg.push(t) {
                    match piece {
                        Piece::Text(s) => {
                            out.text.push_str(&s);
                            on_event(StreamEvent::Text(s));
                        }
                        Piece::Thinking(s) => {
                            out.reasoning.push_str(&s);
                            on_event(StreamEvent::Reasoning(s));
                        }
                        Piece::ToolCall(j) => text_calls.push(j),
                    }
                }
            }
            if let Some(Value::Array(tc)) = delta.get("tool_calls") {
                *any = true;
                for piece in tc {
                    if let Some(name) = calls.push(piece) {
                        on_event(StreamEvent::ToolCallStarted(name));
                    }
                }
            }
            if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
                out.finish_reason = Some(r.to_string());
            }
            Ok(())
        };

    if is_json {
        let text =
            resp.text().await.map_err(|e| ProviderError::new(None, String::new(), format!("応答を読めません: {}", without_url(&e))))?;
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            ProviderError::new(None, text.clone(), format!("応答が JSON ではありません: {}", text.chars().take(120).collect::<String>()))
        })?;
        handle(&v, &mut out, &mut any_output, on_event)?;
    } else {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = vec![];
        'read: loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    let mut e = ProviderError::aborted();
                    e.before_output = !any_output;
                    return Err(e);
                }
                n = tokio::time::timeout(idle_timeout, stream.next()) => n,
            };
            let chunk = match next {
                Err(_) => {
                    let mut e = ProviderError::new(None, String::new(), "提供元からの応答が途中で止まりました（時間切れ）".into());
                    e.before_output = !any_output;
                    return Err(e);
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    let mut e = ProviderError::new(None, String::new(), format!("通信が切れました: {}", without_url(&e)));
                    e.before_output = !any_output;
                    return Err(e);
                }
                Ok(Some(Ok(c))) => c,
            };
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'read;
                }
                if data.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(data) {
                    Ok(v) => handle(&v, &mut out, &mut any_output, on_event)?,
                    Err(_) => continue,
                }
            }
        }
    }
    for piece in seg.finish() {
        match piece {
            Piece::Text(s) => {
                out.text.push_str(&s);
                on_event(StreamEvent::Text(s));
            }
            Piece::Thinking(s) => {
                out.reasoning.push_str(&s);
                on_event(StreamEvent::Reasoning(s));
            }
            Piece::ToolCall(j) => text_calls.push(j),
        }
    }
    out.tool_calls = calls.finish();
    for (n, j) in text_calls.into_iter().enumerate() {
        match parse_text_call(&j, n) {
            Some(c) => out.tool_calls.push(c),
            None => out.unreadable_calls.push(j),
        }
    }
    Ok(out)
}

/// The error without the request's URL (it may carry a query the user put there).
fn without_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(u) = e.url() {
        s = s.replace(u.as_str(), "(URL)");
    }
    s
}

/// The demo model: answers without a network so the CLI can be tried before a provider is set up.
async fn demo_reply(
    req: &ChatRequest<'_>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
    cancel: &CancellationToken,
) -> Result<Completion, ProviderError> {
    let last =
        req.messages.iter().rev().find_map(|m| if let Message::User { content } = m { Some(content.as_str()) } else { None }).unwrap_or("");
    let thinking = "デモ応答です。実際のモデルにはつながっていません。";
    let text = format!(
        "これは **デモ応答** です（ネットワークを使いません）。\n\n受け取った依頼: `{}`\n\n実際のモデルを使うには:\n\n1. `arce providers add --preset openrouter`（など）でプロバイダを足す\n2. `arce secrets set openrouter` で API キーを入れる\n3. `arce config set model \"openrouter|<モデル名>\"`\n\n| コマンド | 内容 |\n|---|---|\n| `/help` | コマンドの一覧 |\n| `/model` | モデルを替える |\n| `/mode` | 承認のモード |\n",
        last.chars().take(80).collect::<String>().replace('`', "'")
    );
    let mut out = Completion::default();
    out.reasoning.push_str(thinking);
    on_event(StreamEvent::Reasoning(thinking.to_string()));
    for chunk in text.split_inclusive('\n') {
        if cancel.is_cancelled() {
            return Err(ProviderError::aborted());
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
        out.text.push_str(chunk);
        on_event(StreamEvent::Text(chunk.to_string()));
    }
    out.finish_reason = Some("stop".into());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembler_joins_pieces_and_splits_same_index_ids() {
        let mut a = CallAssembler::default();
        a.push(&json!({"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"pa"}}));
        a.push(&json!({"index":0,"function":{"arguments":"th\":\"x\"}"}}));
        a.push(&json!({"index":0,"id":"b","function":{"name":"glob","arguments":"{}"}}));
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, "{\"path\":\"x\"}");
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[1].name, "glob");
    }

    #[test]
    fn back_to_back_objects_become_calls() {
        let mut a = CallAssembler::default();
        a.push(&json!({"index":0,"function":{"name":"grep","arguments":"{\"p\":\"}{\"}{\"p\":2}"}}));
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, "{\"p\":\"}{\"}");
    }

    #[test]
    fn text_form_puts_calls_in_tags_and_joins_users() {
        let msgs = vec![
            Message::user("hi"),
            Message::Assistant {
                content: "ok".into(),
                reasoning: None,
                tool_calls: vec![ToolCall { id: "1".into(), name: "x".into(), arguments: "{\"a\":1}".into(), extra: None }],
            },
            Message::Tool { tool_call_id: "1".into(), name: "x".into(), content: "done".into() },
            Message::user("next"),
        ];
        let w = wire_messages("sys", &msgs, false, false);
        assert_eq!(w.len(), 4);
        assert!(w[2]["content"].as_str().unwrap().contains("<tool_call>"));
        assert_eq!(w[3]["role"], "user");
        assert!(w[3]["content"].as_str().unwrap().contains("<tool_response>\ndone"));
        assert!(w[3]["content"].as_str().unwrap().ends_with("next"));
    }

    #[test]
    fn native_form_keeps_roles_and_unanswered_calls_become_text() {
        let msgs = vec![
            Message::user("hi"),
            Message::Assistant {
                content: String::new(),
                reasoning: Some("r".into()),
                tool_calls: vec![
                    ToolCall { id: "1".into(), name: "x".into(), arguments: "".into(), extra: Some(json!({"g":1})) },
                    ToolCall { id: "2".into(), name: "y".into(), arguments: "{}".into(), extra: None },
                ],
            },
            Message::Tool { tool_call_id: "1".into(), name: "x".into(), content: "done".into() },
        ];
        let w = wire_messages("sys", &msgs, true, true);
        assert_eq!(w[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(w[2]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(w[2]["tool_calls"][0]["extra_content"]["g"], 1);
        assert!(w[2]["content"].as_str().unwrap().contains("\"y\""));
        assert_eq!(w[2]["reasoning_content"], "r");
        assert_eq!(w[3]["role"], "tool");
    }

    #[test]
    fn body_layers_extra_fields_and_cache_key() {
        let mut p = ProviderConfig { id: "p".into(), base_url: "https://x.test/v1".into(), prompt_cache: true, ..Default::default() };
        p.extra_body.insert("temperature".into(), json!(0.1));
        let mut extra = Map::new();
        extra.insert("reasoning_effort".into(), json!("high"));
        let req = ChatRequest {
            provider: &p,
            model: "m",
            api_key: None,
            system: "s".into(),
            messages: &[],
            tools: &[],
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(100),
            extra,
            cache_key: Some("arce-abc".into()),
            pass_reasoning: false,
        };
        let b = request_body(&req, true);
        assert_eq!(b["temperature"], 0.1);
        assert_eq!(b["reasoning_effort"], "high");
        assert_eq!(b["prompt_cache_key"], "arce-abc");
        assert_eq!(b["max_tokens"], 100);
    }

    #[test]
    fn chat_url_accepts_a_pasted_full_path() {
        assert_eq!(chat_url("https://a.test/v1/"), "https://a.test/v1/chat/completions");
        assert_eq!(chat_url("https://a.test/v1/chat/completions"), "https://a.test/v1/chat/completions");
    }

    #[test]
    fn usage_reads_both_cache_fields() {
        let u = usage_of(&json!({"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}})).unwrap();
        assert_eq!(u.cached, Some(8));
        let u = usage_of(&json!({"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":4}})).unwrap();
        assert_eq!(u.cached, Some(4));
        let u = usage_of(&json!({"usage":{"prompt_tokens":10,"completion_tokens":2}})).unwrap();
        assert_eq!(u.cached, None);
    }
}

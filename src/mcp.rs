//! MCP servers over stdio (JSON-RPC, one message per line).
//!
//! A server's tools are offered as `mcp_<server>__<tool>`. When the servers together have more tools
//! than `mcp.lazy_threshold`, the model gets three small tools instead (`mcp_find`, `mcp_describe`,
//! `mcp_call`) and reads a definition only when it needs it: long tool lists cost tokens on every
//! request and slow the first word of a reply.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

use crate::llm::ToolSpec;
use crate::settings::McpServer;

const PROTOCOL_VERSION: &str = "2025-06-18";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct Client {
    pub id: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next: AtomicU64,
    _child: Child,
    pub tools: Vec<McpTool>,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl McpTool {
    pub fn full_name(&self) -> String {
        format!("mcp_{}__{}", self.server, self.name)
    }
}

impl Client {
    /// Starts [server] and reads its tools. [fill] puts secrets into the environment values.
    pub async fn start(server: &McpServer, fill: &dyn Fn(&str) -> String, timeout: Duration) -> Result<Client> {
        let mut cmd = tokio::process::Command::new(&server.command);
        cmd.args(&server.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        for (k, v) in &server.env {
            cmd.env(k, fill(v));
        }
        let mut child = cmd.spawn().with_context(|| format!("MCP サーバー {} を始められません（{}）", server.id, server.command))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("stdin がありません"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("stdout がありません"))?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = v.get("id").and_then(Value::as_u64) else { continue };
                if v.get("method").is_some() {
                    continue; // a request from the server (sampling, roots): not offered
                }
                if let Some(tx) = reader_pending.lock().await.remove(&id) {
                    let r = match v.get("error") {
                        Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("MCP のエラー").to_string()),
                        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(r);
                }
            }
            // the server ended: everyone waiting gets an error
            for (_, tx) in reader_pending.lock().await.drain() {
                let _ = tx.send(Err("MCP サーバーが終了しました".into()));
            }
        });
        let mut client = Client {
            id: server.id.clone(),
            stdin: Mutex::new(stdin),
            pending,
            next: AtomicU64::new(1),
            _child: child,
            tools: vec![],
            timeout,
        };
        client
            .request("initialize", json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "arce-ai-cli", "version": env!("CARGO_PKG_VERSION") } }))
            .await
            .with_context(|| format!("{} の初期化に失敗しました", server.id))?;
        client.notify("notifications/initialized", json!({})).await?;
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = client.request("tools/list", params).await?;
            for t in r["tools"].as_array().into_iter().flatten() {
                let Some(name) = t["name"].as_str() else { continue };
                client.tools.push(McpTool {
                    server: server.id.clone(),
                    name: name.to_string(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    schema: t.get("inputSchema").cloned().unwrap_or(json!({ "type": "object" })),
                });
            }
            match r["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(client)
    }

    async fn send(&self, msg: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{msg}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        match tokio::time::timeout(self.timeout, rx).await {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP サーバー {} が {} 秒以内に答えませんでした", self.id, self.timeout.as_secs())
            }
            Ok(Err(_)) => bail!("MCP サーバー {} との接続が切れました", self.id),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Ok(Ok(v))) => Ok(v),
        }
    }

    /// Calls [tool]; the text of the result, and whether the server said it failed.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<(String, bool)> {
        let r = self.request("tools/call", json!({ "name": tool, "arguments": arguments })).await?;
        Ok(result_text(&r))
    }
}

/// The text parts of a `tools/call` result (other parts are named), and `isError`.
pub fn result_text(r: &Value) -> (String, bool) {
    let mut out = vec![];
    for part in r["content"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => out.push(part["text"].as_str().unwrap_or("").to_string()),
            Some("resource") => out.push(
                part["resource"]["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("[resource {}]", part["resource"]["uri"].as_str().unwrap_or(""))),
            ),
            Some(other) => out.push(format!("[{other}]")),
            None => {}
        }
    }
    if out.is_empty()
        && let Some(s) = r.get("structuredContent")
    {
        out.push(s.to_string());
    }
    (out.join("\n"), r["isError"].as_bool().unwrap_or(false))
}

/// Every running server.
#[derive(Default)]
pub struct Manager {
    pub clients: Vec<Client>,
    pub lazy: bool,
}

impl Manager {
    pub async fn start(
        servers: &[McpServer],
        lazy_threshold: usize,
        fill: &dyn Fn(&str) -> String,
        timeout: Duration,
    ) -> (Manager, Vec<String>) {
        let mut m = Manager::default();
        let mut errors = vec![];
        for s in servers.iter().filter(|s| s.enabled && !s.command.is_empty()) {
            if !crate::secrets::valid_id(&s.id) {
                errors.push(format!("MCP サーバーの id「{}」は使えません", s.id));
                continue;
            }
            match Client::start(s, fill, timeout).await {
                Ok(c) => m.clients.push(c),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        m.lazy = m.tool_count() > lazy_threshold;
        (m, errors)
    }

    pub fn tool_count(&self) -> usize {
        self.clients.iter().map(|c| c.tools.len()).sum()
    }

    pub fn all_tools(&self) -> impl Iterator<Item = &McpTool> {
        self.clients.iter().flat_map(|c| c.tools.iter())
    }

    pub fn find_tool(&self, full: &str) -> Option<(&Client, &McpTool)> {
        self.clients.iter().find_map(|c| c.tools.iter().find(|t| t.full_name() == full).map(|t| (c, t)))
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        if self.clients.is_empty() {
            return vec![];
        }
        if self.lazy {
            return vec![
                ToolSpec {
                    name: "mcp_find".into(),
                    description: "Find tools of the MCP servers by words; returns names and one-line descriptions.".into(),
                    parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
                },
                ToolSpec {
                    name: "mcp_describe".into(),
                    description: "The full definition (parameters) of one MCP tool.".into(),
                    parameters: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
                },
                ToolSpec {
                    name: "mcp_call".into(),
                    description: "Call an MCP tool by its full name with its arguments.".into(),
                    parameters: json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name"]}),
                },
            ];
        }
        self.all_tools()
            .map(|t| ToolSpec {
                name: t.full_name(),
                description: format!("[{}] {}", t.server, t.description),
                parameters: t.schema.clone(),
            })
            .collect()
    }

    /// `mcp_find`: tools whose name or description holds every word.
    pub fn find(&self, query: &str) -> String {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let hits: Vec<String> = self
            .all_tools()
            .filter(|t| {
                let hay = format!("{} {}", t.full_name(), t.description).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .take(30)
            .map(|t| format!("{} — {}", t.full_name(), t.description.lines().next().unwrap_or("")))
            .collect();
        if hits.is_empty() { "（見つかりません。別の語で探してください）".into() } else { hits.join("\n") }
    }

    pub fn describe(&self, name: &str) -> String {
        match self.find_tool(name) {
            Some((_, t)) => json!({ "name": t.full_name(), "description": t.description, "parameters": t.schema }).to_string(),
            None => format!("{name} という MCP の道具はありません（mcp_find で探せます）"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_text_reads_parts() {
        let r = json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"x"},{"type":"resource","resource":{"uri":"file:///x"}}],"isError":true});
        assert_eq!(result_text(&r), ("a\n[image]\n[resource file:///x]".into(), true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn talks_to_a_stdio_server() {
        // a tiny server in sh: answers initialize, tools/list and one tools/call
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"capabilities":{}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done
"#;
        let server =
            McpServer { id: "t".into(), command: "sh".into(), args: vec!["-c".into(), script.into()], enabled: true, ..Default::default() };
        let (m, errors) = Manager::start(&[server], 0, &|v| v.to_string(), Duration::from_secs(5)).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(m.lazy);
        assert_eq!(m.find("echo"), "mcp_t__echo — Echo text");
        let (c, t) = m.find_tool("mcp_t__echo").unwrap();
        assert_eq!(c.call(&t.name, json!({})).await.unwrap(), ("pong".into(), false));
    }
}

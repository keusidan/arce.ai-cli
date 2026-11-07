//! One conversation: a message in, a reply out, with the tools in between.
//!
//! A reply is a loop: build the request (the agent, the prompt, the tools), make room when the
//! conversation would not fit, send it (switching to a fallback model only before the first word),
//! stream the answer, and when the model calls tools, check and run them and go round again. A tool
//! call that cannot be used is not an error: the model is told why and writes it again. The same
//! call with the same arguments too many times in a row stops the reply.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agents::{self, Agent};
use crate::compaction::{self, FEEDBACK_PREFIX};
use crate::delegation;
use crate::hooks;
use crate::llm::{self, ChatRequest, Completion, Message, ProviderError, StreamEvent, ToolCall, ToolSpec};
use crate::mcp;
use crate::permissions::{Decision, PermissionState, ToolKind};
use crate::prompt;
use crate::provider::{self, ModelConfig, ProviderConfig, ThinkingFormat, parse_ref};
use crate::secrets::SecretStore;
use crate::session::{self, Session};
use crate::settings::{self, Paths, Settings};
use crate::tools::{self, Book, BookEntry, Env, Prepared};
use crate::ui::{Approval, ApprovalRequest, Level, PlanItem, RunMeter, Ui};

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

pub struct Controller {
    pub settings: Settings,
    pub paths: Paths,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub is_git: bool,
    pub store: SecretStore,
    pub client: reqwest::Client,
    pub session: Session,
    pub perms: PermissionState,
    pub mcp: mcp::Manager,
    pub ui: Arc<dyn Ui>,
    pub last_agent: Option<&'static Agent>,
    pub delegated: delegation::Store,
    /// providers whose cache key was refused this session
    pub cache_refused: HashSet<String>,
    pub last_meter: Option<RunMeter>,
    pub instructions: Vec<(PathBuf, String)>,
}

/// The built-in demo provider (no network), used until a provider is set up.
pub fn demo_provider() -> ProviderConfig {
    ProviderConfig {
        id: "demo".into(),
        name: "デモ（ネットワークなし）".into(),
        base_url: "http://127.0.0.1/".into(),
        models: vec![ModelConfig { id: "arce-demo".into(), name: "ARCE demo".into(), ..Default::default() }],
        ..Default::default()
    }
}

fn canonical(v: &Value) -> String {
    // serde_json's map is ordered by key, so the text is the same for the same arguments
    v.to_string()
}

impl Controller {
    pub fn providers(&self) -> Vec<ProviderConfig> {
        let mut out = self.settings.providers.clone();
        if !out.iter().any(|p| p.id == "demo") {
            out.push(demo_provider());
        }
        out
    }

    /// Every `provider|model` that can be chosen.
    pub fn model_refs(&self) -> Vec<String> {
        self.providers().iter().flat_map(|p| p.models.iter().map(move |m| format!("{}|{}", p.id, m.id))).collect()
    }

    pub fn resolve(&self, reference: &str) -> Result<(ProviderConfig, ModelConfig), String> {
        let (pid, mid) = parse_ref(reference).ok_or_else(|| format!("モデル「{reference}」は provider|model の形ではありません"))?;
        let p = self
            .providers()
            .into_iter()
            .find(|p| p.id == pid)
            .ok_or_else(|| format!("プロバイダ {pid} がありません（arce providers add で足せます）"))?;
        // a model that is not in the list is still used: the list is only for settings
        let m = p.model(mid).cloned().unwrap_or_else(|| ModelConfig { id: mid.to_string(), ..Default::default() });
        Ok((p, m))
    }

    pub fn api_key(&self, p: &ProviderConfig) -> Option<String> {
        if !p.api_key_env.is_empty()
            && let Ok(v) = std::env::var(&p.api_key_env)
            && !v.is_empty()
        {
            return Some(v);
        }
        self.store.value(&p.id)
    }

    /// The secrets the tools may use, with the hosts each may go to.
    pub fn book(&self) -> Book {
        let mut book = Book::default();
        let off: HashSet<&str> = self.settings.secrets_off_for_model.iter().map(String::as_str).collect();
        let mut extra = BTreeMap::new();
        for p in &self.settings.providers {
            if let Some(v) = self.api_key(p) {
                extra.insert(p.id.clone(), v.clone());
                let host = reqwest::Url::parse(&p.base_url).ok().and_then(|u| u.host_str().map(str::to_string)).into_iter().collect();
                if !off.contains(p.id.as_str()) {
                    book.entries.insert(p.id.clone(), BookEntry { value: v, hosts: host, kind: "provider" });
                }
            }
        }
        for (id, s) in &self.store.items {
            let Some(v) = self.store.value(id) else { continue };
            if book.entries.contains_key(id) || off.contains(id.as_str()) {
                continue;
            }
            let mut hosts = s.hosts.clone();
            hosts.extend(self.settings.secret_hosts.get(id).cloned().unwrap_or_default());
            book.entries.insert(id.clone(), BookEntry { value: v, hosts, kind: "user" });
        }
        book.all_values = self.store.all_values(&extra);
        book
    }

    pub fn reload_instructions(&mut self) {
        let files = prompt::instruction_files(&self.paths.global_instructions(), &self.root, &self.cwd, &self.settings.instruction_files);
        let max = self.settings.limits.attach_max_chars;
        self.instructions =
            files.into_iter().filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, tools::clip_middle(&t, max)))).collect();
    }

    pub fn coding_ready(&self) -> bool {
        self.settings.coding.is_ready()
    }

    pub fn system_prompt(&self, agent: &Agent, model: &ModelConfig) -> String {
        let ids = self.book().ids();
        prompt::build(&prompt::PromptParts {
            settings: &self.settings,
            agent,
            mode: self.perms.mode,
            cwd: &self.cwd,
            is_git: self.is_git,
            instructions: &self.instructions,
            model_prompt: &model.prompt,
            secret_ids: &ids,
            coding_ready: self.coding_ready() && agent.id == agents::CODING.id,
            mcp_lazy: self.mcp.lazy,
        })
    }

    pub fn tool_specs(&self, agent: &Agent) -> Vec<ToolSpec> {
        let mode = self.perms.mode;
        let mut out: Vec<ToolSpec> = tools::specs()
            .into_iter()
            .filter(|t| agent.allows(&t.name))
            .filter(|t| !(mode == settings::PermissionMode::Plan && matches!(t.name.as_str(), "write_file" | "edit_file" | "delegate")))
            .filter(|t| t.name != "delegate" || (self.coding_ready() && agent.id == agents::CODING.id))
            .filter(|t| t.name != "web_search" || self.settings.search.enabled)
            .collect();
        if agent.allows("mcp_x__y") {
            out.extend(self.mcp.specs());
        }
        out
    }

    fn approve(&self, req: ApprovalRequest) -> Approval {
        let ui = self.ui.clone();
        tokio::task::block_in_place(move || ui.approve(&req))
    }

    /// Asks when the rules say so; Err is what the model is told.
    fn authorize(&mut self, tool: &str, prep: &Prepared) -> Result<(), String> {
        match self.perms.decide(tool, prep.kind, &prep.subjects, prep.outside, prep.secret) {
            Decision::Allow => Ok(()),
            Decision::Deny(why) => Err(why),
            Decision::Ask(reason) => {
                let can_remember = prep.kind != ToolKind::Command && prep.kind != ToolKind::Settings;
                match self.approve(ApprovalRequest {
                    tool: tool.to_string(),
                    title: prep.title.clone(),
                    detail: prep.detail.clone(),
                    reason,
                    can_remember,
                }) {
                    Approval::Yes => Ok(()),
                    Approval::Always => {
                        self.perms.approved.insert(tool.to_string());
                        Ok(())
                    }
                    Approval::No(why) => Err(match why {
                        Some(w) => format!("ユーザーが断りました。理由: {w}"),
                        None => "ユーザーが断りました。別の方法を考えるか、ユーザーに聞いてください".into(),
                    }),
                }
            }
        }
    }

    fn hook_timeout(&self) -> Duration {
        Duration::from_secs(self.settings.limits.hook_timeout_secs)
    }

    /// The sampling for one request.
    fn sampling(&self, p: &ProviderConfig, m: &ModelConfig, used: u64) -> (Option<f64>, Option<f64>, Option<u32>) {
        let o = &self.settings.online;
        let temperature = m.temperature.or(o.temperature);
        let top_p = m.top_p.or(o.top_p);
        if p.id == "demo" {
            return (temperature, top_p, None);
        }
        let mut max = m
            .max_tokens
            .or(o.max_tokens)
            .or_else(|| (o.use_model_max && m.max_output_tokens > 0).then_some(m.max_output_tokens))
            .unwrap_or(self.settings.limits.default_max_tokens);
        if m.max_output_tokens > 0 {
            max = max.min(m.max_output_tokens);
        }
        if m.context_tokens > 0 {
            let room = (m.context_tokens as u64).saturating_sub(used + 512).max(256) as u32;
            max = max.min(room);
        }
        (temperature, top_p, Some(max))
    }

    /// One request to [reference]; the events go to the screen.
    async fn request_once(
        &mut self,
        reference: &str,
        system: &str,
        tools: &[ToolSpec],
        messages: &[Message],
        cancel: &CancellationToken,
        partial: &mut String,
    ) -> Result<Completion, ProviderError> {
        let (p, m) = self.resolve(reference).map_err(|e| ProviderError {
            status: None,
            body: String::new(),
            message: e,
            before_output: true,
            aborted: false,
        })?;
        let key = self.api_key(&p);
        let local = reqwest::Url::parse(&p.base_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| crate::secrets::hosts::is_private(h.trim_matches(['[', ']']))))
            .unwrap_or(false);
        if key.is_none() && p.id != "demo" && !local {
            return Err(ProviderError {
                status: None,
                body: String::new(),
                message: format!(
                    "{} の API キーがありません（`arce secrets set {}` で入れるか、環境変数 {} に入れてください）",
                    p.label(),
                    p.id,
                    if p.api_key_env.is_empty() { crate::secrets::env_name(&p.id) } else { p.api_key_env.clone() }
                ),
                before_output: true,
                aborted: false,
            });
        }
        let used = compaction::estimate_tokens(system)
            + compaction::estimate_tokens(&serde_json::to_string(tools).unwrap_or_default())
            + compaction::estimate_messages(messages);
        let (temperature, top_p, max_tokens) = self.sampling(&p, &m, used);
        let extra = m.thinking.body_fields(self.settings.thinking, self.settings.effort);
        let pass_reasoning = matches!(m.thinking.format, ThinkingFormat::Deepseek | ThinkingFormat::Zai) && p.uses_native_tools();
        let use_cache = p.prompt_cache && !self.cache_refused.contains(&p.id);
        let ui = self.ui.clone();
        let idle = Duration::from_secs(self.settings.limits.idle_timeout_secs);

        let mut attempt_cache = use_cache;
        loop {
            let req = ChatRequest {
                provider: &p,
                model: &m.id,
                api_key: key.clone(),
                system: system.to_string(),
                messages,
                tools,
                temperature,
                top_p,
                max_tokens,
                extra: extra.clone(),
                cache_key: attempt_cache.then(|| self.session.cache_key.clone()),
                pass_reasoning,
            };
            // the waiting line until the first piece arrives
            let first = Arc::new(AtomicBool::new(false));
            let ticker = {
                let first = first.clone();
                let ui = ui.clone();
                let big = used >= 3000 && local;
                tokio::spawn(async move {
                    let started = Instant::now();
                    loop {
                        if first.load(Ordering::Relaxed) {
                            break;
                        }
                        let secs = started.elapsed().as_secs();
                        let note = if big {
                            "（この端末のモデルは、長い指示を読むのに時間がかかります）"
                        } else {
                            ""
                        };
                        ui.status(Some(&format!("モデルに送信しています（返信を待っています） {secs}秒{note}")));
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                })
            };
            let mut thinking_since: Option<Instant> = None;
            let mut thinking_done = false;
            let mut on_event = |e: StreamEvent| {
                if !first.swap(true, Ordering::Relaxed) {
                    ui.status(None);
                }
                match e {
                    StreamEvent::Reasoning(s) => {
                        thinking_since.get_or_insert_with(Instant::now);
                        ui.reasoning(&s);
                    }
                    StreamEvent::Text(s) => {
                        if let (Some(t), false) = (thinking_since, thinking_done) {
                            ui.reasoning_end(t.elapsed().as_secs_f64());
                            thinking_done = true;
                        }
                        partial.push_str(&s);
                        ui.text(&s);
                    }
                    StreamEvent::ToolCallStarted(_) => {
                        if let (Some(t), false) = (thinking_since, thinking_done) {
                            ui.reasoning_end(t.elapsed().as_secs_f64());
                            thinking_done = true;
                        }
                    }
                }
            };
            let r = llm::stream_chat(&self.client, &req, &mut on_event, cancel, idle).await;
            ticker.abort();
            ui.status(None);
            if let (Some(t), false) = (thinking_since, thinking_done) {
                ui.reasoning_end(t.elapsed().as_secs_f64());
            }
            match r {
                Err(e)
                    if attempt_cache
                        && matches!(e.status, Some(400) | Some(422))
                        && e.body.contains(&p.prompt_cache_field)
                        && !p.extra_body.contains_key(&p.prompt_cache_field) =>
                {
                    // the server refused the cache key by name: once more without it
                    self.cache_refused.insert(p.id.clone());
                    attempt_cache = false;
                    self.ui.notice(
                        Level::Warn,
                        &format!("{} は {} を受け付けなかったので、この会話では送りません", p.label(), p.prompt_cache_field),
                    );
                }
                other => return other,
            }
        }
    }

    /// Sends with fallback: a model that fails before writing anything is replaced by the rules' targets.
    async fn request(
        &mut self,
        agent: &Agent,
        messages: &[Message],
        cancel: &CancellationToken,
        partial: &mut String,
    ) -> Result<(Completion, String), ProviderError> {
        let mut current = self.settings.model.clone();
        if agent.id == agents::CODING.id && self.coding_ready() && !self.settings.coding.planner_follows_model {
            current = self.settings.coding.planner.clone();
        }
        let mut tried = vec![current.clone()];
        loop {
            let (_, m) = self.resolve(&current).map_err(|e| ProviderError {
                status: None,
                body: String::new(),
                message: e,
                before_output: true,
                aborted: false,
            })?;
            let system = self.system_prompt(agent, &m);
            let tools = self.tool_specs(agent);
            match self.request_once(&current, &system, &tools, messages, cancel, partial).await {
                Ok(c) => return Ok((c, current)),
                Err(e) if e.aborted || !e.before_output || !self.settings.fallback_enabled => return Err(e),
                Err(e) => {
                    let (p, mm) = parse_ref(&current).unwrap_or(("", ""));
                    let next =
                        provider::fallback_targets(&self.settings.fallback, p, mm, e.status).into_iter().find(|t| !tried.contains(t));
                    match next {
                        Some(n) => {
                            let why = e.status.map_or("接続できない".to_string(), |s| format!("HTTP {s}"));
                            self.ui.notice(Level::Warn, &format!("{current} が {why} のため {n} に切り替えました"));
                            tried.push(n.clone());
                            current = n;
                        }
                        None => return Err(e),
                    }
                }
            }
        }
    }

    /// `@path` in a message: the file (or folder listing) is attached.
    fn attachments(&self, text: &str) -> (String, bool) {
        let re = regex::Regex::new(r"(?:^|\s)@([^\s@]+)").unwrap();
        let mut extra = String::new();
        let mut any = false;
        let max = self.settings.limits.attach_max_chars;
        for c in re.captures_iter(text) {
            let raw = c[1].trim_end_matches([',', '.', '、', '。', ')', '）']);
            let p = crate::permissions::resolve(&self.cwd, raw);
            if p.is_file() {
                match std::fs::read(&p) {
                    Ok(bytes) if !bytes.iter().take(8192).any(|b| *b == 0) => {
                        let t = String::from_utf8_lossy(&bytes);
                        extra.push_str(&format!(
                            "\n\n<file path=\"{}\">\n{}\n</file>",
                            tools::shown_path(&self.cwd, &p),
                            tools::clip_middle(&t, max)
                        ));
                        any = true;
                    }
                    _ => self.ui.notice(Level::Warn, &format!("@{raw} はテキストとして読めませんでした")),
                }
            } else if p.is_dir() {
                let names: Vec<String> = std::fs::read_dir(&p)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned() + if e.path().is_dir() { "/" } else { "" })
                            .collect()
                    })
                    .unwrap_or_default();
                extra.push_str(&format!("\n\n<folder path=\"{}\">\n{}\n</folder>", tools::shown_path(&self.cwd, &p), names.join("\n")));
                any = true;
            }
        }
        (extra, any)
    }

    /// Sends [input] and runs the reply to its end.
    pub async fn send(&mut self, input: &str, cancel: CancellationToken) -> Outcome {
        let mut meter = RunMeter::default();
        self.ui.reply_start();
        let outcome = self.send_inner(input, &cancel, &mut meter).await;
        match &outcome {
            Outcome::Failed(msg) => self.ui.notice(Level::Error, msg),
            Outcome::Cancelled => self.ui.notice(Level::Warn, "止めました"),
            Outcome::Done => {}
        }
        let stop = hooks::run(
            &self.settings.hooks,
            "stop",
            "",
            &json!({"event": "stop", "session_id": self.session.id, "cwd": self.cwd}),
            &self.cwd,
            self.hook_timeout(),
        )
        .await;
        for e in stop.errors {
            self.ui.notice(Level::Warn, &e);
        }
        self.ui.reply_end(&meter, outcome == Outcome::Done);
        self.last_meter = Some(meter);
        outcome
    }

    async fn send_inner(&mut self, input: &str, cancel: &CancellationToken, meter: &mut RunMeter) -> Outcome {
        let mut text = input.to_string();
        let hook = hooks::run(
            &self.settings.hooks,
            "prompt",
            "",
            &json!({"event": "prompt", "prompt": input, "session_id": self.session.id, "cwd": self.cwd}),
            &self.cwd,
            self.hook_timeout(),
        )
        .await;
        for e in &hook.errors {
            self.ui.notice(Level::Warn, e);
        }
        if let Some(why) = hook.blocked {
            return Outcome::Failed(format!("フックが送信を止めました: {why}"));
        }
        if !hook.output.is_empty() {
            text.push_str(&format!("\n\n[from a hook]\n{}", hook.output));
        }
        let (files, has_attachments) = self.attachments(input);
        text.push_str(&files);
        let agent = agents::select(input, has_attachments, &self.settings.agent, self.last_agent);
        self.last_agent = Some(agent);
        if let Err(e) = self.session.next_turn() {
            return Outcome::Failed(format!("セッションを保存できません: {e}"));
        }
        if self.session.title.is_empty() {
            let _ = self.session.set_title(&session::title_from(input, self.settings.title_max_chars));
        }
        if let Err(e) = self.session.push(Message::user(text)) {
            return Outcome::Failed(format!("セッションを保存できません: {e}"));
        }
        self.perms.external_seen = false;
        let mut rounds = 0u32;
        let mut retries = 0u32;
        let mut last_sig = String::new();
        let mut repeats = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Outcome::Cancelled;
            }
            if let Err(e) = self.make_room(agent, false, cancel, meter).await {
                return Outcome::Failed(e);
            }
            if let Err(e) = self.precheck(agent) {
                return Outcome::Failed(e);
            }
            let history = self.session.history.clone();
            let mut partial = String::new();
            meter.requests += 1;
            let (c, used_model) = match self.request(agent, &history, cancel, &mut partial).await {
                Ok(x) => x,
                Err(e) if e.aborted => {
                    if !partial.trim().is_empty() {
                        let _ = self.session.push(Message::Assistant {
                            content: format!("{partial}\n\n（ここで止められました）"),
                            reasoning: None,
                            tool_calls: vec![],
                        });
                    }
                    return Outcome::Cancelled;
                }
                Err(e) => return Outcome::Failed(e.message),
            };
            if let Some(u) = &c.usage {
                meter.add_usage(u);
            }
            if c.text.trim().is_empty() && c.tool_calls.is_empty() && c.unreadable_calls.is_empty() {
                if c.finish_reason.as_deref() == Some("length") {
                    return Outcome::Failed("思考だけで出力の上限に達しました。最大トークンを増やすか、思考を弱めてください".into());
                }
                retries += 1;
                if retries > self.settings.limits.retry_limit {
                    return Outcome::Failed(format!("{used_model} の返信が空でした（{retries} 回）"));
                }
                self.ui.notice(Level::Warn, "返信が空だったので、もう一度頼みます");
                continue;
            }
            let reasoning = (!c.reasoning.is_empty()).then(|| c.reasoning.clone());
            if !c.unreadable_calls.is_empty() {
                let _ = self.session.push(Message::Assistant { content: c.text.clone(), reasoning, tool_calls: vec![] });
                retries += 1;
                if retries > self.settings.limits.retry_limit {
                    return Outcome::Failed("道具の呼び出しが読めませんでした（JSON が壊れています）".into());
                }
                self.ui.notice(Level::Warn, "道具の呼び出しが読めませんでした（JSON が壊れています）。書き直してもらいます");
                let _ = self.session.push(Message::user(format!(
                    "{FEEDBACK_PREFIX} The tool call could not be read (the JSON is broken). Write it again as <tool_call>{{\"name\": \"…\", \"arguments\": {{…}}}}</tool_call> with valid JSON."
                )));
                continue;
            }
            let calls = c.tool_calls.clone();
            if let Err(e) = self.session.push(Message::Assistant { content: c.text.clone(), reasoning, tool_calls: calls.clone() }) {
                return Outcome::Failed(format!("セッションを保存できません: {e}"));
            }
            if calls.is_empty() {
                return Outcome::Done;
            }
            // the same call with the same arguments, again and again
            let sig = calls
                .iter()
                .map(|c| format!("{}:{}", c.name, c.args().map(|a| canonical(&Value::Object(a))).unwrap_or_else(|_| c.arguments.clone())))
                .collect::<Vec<_>>()
                .join("|");
            if sig == last_sig {
                repeats += 1;
            } else {
                last_sig = sig;
                repeats = 1;
            }
            if repeats >= self.settings.limits.repeat_limit.max(2) {
                for call in &calls {
                    let _ = self.session.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: "Error: stopped (the same call was repeated)".into(),
                    });
                }
                return Outcome::Failed(format!("同じ道具を同じ引数で {repeats} 回続けて呼んだので止めました"));
            }
            for call in &calls {
                if cancel.is_cancelled() {
                    let _ = self.session.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: "Error: stopped by the user".into(),
                    });
                    continue;
                }
                meter.tools += 1;
                let (content, _ok) = self.run_tool(agent, call, cancel, meter).await;
                if let Err(e) = self.session.push(Message::Tool { tool_call_id: call.id.clone(), name: call.name.clone(), content }) {
                    return Outcome::Failed(format!("セッションを保存できません: {e}"));
                }
            }
            if cancel.is_cancelled() {
                return Outcome::Cancelled;
            }
            rounds += 1;
            if rounds >= self.settings.limits.max_tool_rounds {
                return Outcome::Failed(format!("道具を呼ぶ回数の上限（{rounds} 回）に達しました。続けるには、もう一度送ってください"));
            }
        }
    }

    /// Runs one call: checks, asks, runs, and returns what the model is told.
    async fn run_tool(&mut self, agent: &Agent, call: &ToolCall, cancel: &CancellationToken, meter: &mut RunMeter) -> (String, bool) {
        let name = call.name.as_str();
        let args = match call.args() {
            Ok(a) => a,
            Err(e) => {
                self.ui.tool_start(name, "");
                self.ui.tool_end(name, false, &e);
                return (format!("Error: {e}. Call the tool again with a valid JSON object."), false);
            }
        };
        let offered = self.tool_specs(agent).iter().any(|t| t.name == name) || (name == "mcp_call" && self.mcp.lazy);
        if !offered {
            let why = if self.perms.mode == settings::PermissionMode::Plan && matches!(name, "write_file" | "edit_file" | "delegate") {
                "計画モードでは使えません".to_string()
            } else {
                format!("{name} はいまは使えない道具です")
            };
            self.ui.tool_start(name, "");
            self.ui.tool_end(name, false, &why);
            return (format!("Error: {why}. Use one of the tools you were given."), false);
        }
        let (content, ok, external) = match name {
            "ask_user" => {
                let q = args.get("question").and_then(Value::as_str).unwrap_or("").to_string();
                let options: Vec<String> = args
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                let ui = self.ui.clone();
                let answer = tokio::task::block_in_place(move || ui.ask(&q, &options));
                match answer {
                    Some(a) => (format!("The user answered: {a}"), true, false),
                    None => (
                        "The user cannot answer now (non-interactive). Decide with the most reasonable choice and say which.".into(),
                        false,
                        false,
                    ),
                }
            }
            "update_plan" => {
                let items: Vec<PlanItem> = args
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|i| PlanItem {
                                step: i["step"].as_str().unwrap_or("").to_string(),
                                status: i["status"].as_str().unwrap_or("pending").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.ui.plan(&items);
                ("Plan updated.".into(), true, false)
            }
            "get_settings" => {
                self.ui.tool_start(name, "");
                let v = self.settings_view();
                self.ui.tool_end(name, true, "設定を読みました");
                (v, true, false)
            }
            "update_settings" => {
                let (c, ok) = self.update_settings(&args);
                (c, ok, false)
            }
            "delegate" => {
                let (c, ok) = self.delegate(&args, cancel, meter).await;
                (c, ok, false)
            }
            "mcp_find" => (self.mcp.find(args.get("query").and_then(Value::as_str).unwrap_or("")), true, false),
            "mcp_describe" => (self.mcp.describe(args.get("name").and_then(Value::as_str).unwrap_or("")), true, false),
            n if n == "mcp_call" || n.starts_with("mcp_") => {
                let (full, a) = if n == "mcp_call" {
                    (
                        args.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        args.get("arguments").cloned().unwrap_or(json!({})),
                    )
                } else {
                    (n.to_string(), Value::Object(args.clone()))
                };
                self.call_mcp(&full, a).await
            }
            _ => self.run_builtin(name, &args, cancel).await,
        };
        if external {
            self.perms.external_seen = true;
        }
        (content, ok)
    }

    async fn call_mcp(&mut self, full: &str, a: Value) -> (String, bool, bool) {
        let Some((client, tool)) = self.mcp.find_tool(full) else {
            return (format!("Error: {full} という MCP の道具はありません（mcp_find で探せます）"), false, false);
        };
        let tool_name = tool.name.clone();
        let server = client.id.clone();
        let mut prep = Prepared {
            kind: ToolKind::Mcp,
            subjects: vec![],
            outside: false,
            secret: false,
            summary: format!("{server} · {tool_name}"),
            title: format!("{server} · {tool_name}"),
            detail: serde_json::to_string_pretty(&a).unwrap_or_default(),
            writes: vec![],
        };
        prep.detail = tools::clip_middle(&prep.detail, 4000);
        self.ui.tool_start(full, &prep.summary);
        if let Err(why) = self.authorize(full, &prep) {
            self.ui.tool_end(full, false, &why);
            return (format!("Error: {why}"), false, false);
        }
        let pre = hooks::run(
            &self.settings.hooks,
            "pre_tool",
            full,
            &json!({"event":"pre_tool","tool":full,"input":a,"session_id":self.session.id}),
            &self.cwd,
            self.hook_timeout(),
        )
        .await;
        if let Some(why) = pre.blocked {
            self.ui.tool_end(full, false, &why);
            return (format!("Error: a hook refused this call: {why}"), false, false);
        }
        let (client, _) = self.mcp.find_tool(full).unwrap();
        let r = client.call(&tool_name, a).await;
        let book = self.book();
        let (text, ok) = match r {
            Ok((t, is_err)) => (book.redact(&tools::clip_middle(&t, self.settings.limits.command_max_output_chars)), !is_err),
            Err(e) => (format!("Error: {e:#}"), false),
        };
        let lines = text.lines().count();
        self.ui.tool_end(full, ok, &format!("{lines} 行"));
        let _ = hooks::run(
            &self.settings.hooks,
            "post_tool",
            full,
            &json!({"event":"post_tool","tool":full,"ok":ok,"output":tools::clip_middle(&text, 2000),"session_id":self.session.id}),
            &self.cwd,
            self.hook_timeout(),
        )
        .await;
        (text, ok, true)
    }

    async fn run_builtin(&mut self, name: &str, args: &Map<String, Value>, cancel: &CancellationToken) -> (String, bool, bool) {
        let book = self.book();
        let roots = self.perms.roots.clone();
        let prep = {
            let env = Env { cwd: &self.cwd, roots: &roots, settings: &self.settings, book: &book, client: &self.client, cancel };
            tools::prepare(name, args, &env)
        };
        let prep = match prep {
            Ok(p) => p,
            Err(e) => {
                self.ui.tool_start(name, "");
                self.ui.tool_end(name, false, &e);
                return (format!("Error: {e}"), false, false);
            }
        };
        self.ui.tool_start(name, &prep.summary);
        if let Err(why) = self.authorize(name, &prep) {
            self.ui.tool_end(name, false, &why);
            return (format!("Error: {why}"), false, false);
        }
        let pre = hooks::run(
            &self.settings.hooks,
            "pre_tool",
            name,
            &json!({"event":"pre_tool","tool":name,"input":args,"session_id":self.session.id,"cwd":self.cwd}),
            &self.cwd,
            self.hook_timeout(),
        )
        .await;
        for e in &pre.errors {
            self.ui.notice(Level::Warn, e);
        }
        if let Some(why) = pre.blocked {
            self.ui.tool_end(name, false, &why);
            return (format!("Error: a hook refused this call: {why}"), false, false);
        }
        for p in &prep.writes {
            if let Err(e) = self.session.checkpoint(p) {
                self.ui.notice(Level::Warn, &format!("元に戻すための控えを作れませんでした: {e}"));
            }
        }
        let out = {
            let env = Env { cwd: &self.cwd, roots: &roots, settings: &self.settings, book: &book, client: &self.client, cancel };
            tools::run(name, args, &env).await
        };
        self.ui.tool_end(name, out.ok, &out.summary);
        let post = hooks::run(&self.settings.hooks, "post_tool", name, &json!({"event":"post_tool","tool":name,"input":args,"ok":out.ok,"output":tools::clip_middle(&out.content, 2000),"session_id":self.session.id,"cwd":self.cwd}), &self.cwd, self.hook_timeout()).await;
        for e in &post.errors {
            self.ui.notice(Level::Warn, e);
        }
        let mut content = out.content;
        if let Some(why) = post.blocked {
            content.push_str(&format!("\n[a hook says] {why}"));
        }
        (content, out.ok, out.external)
    }

    /// `get_settings`: what the model may read (never a key).
    pub fn settings_view(&self) -> String {
        let items: Vec<Value> = settings::CHAT_CHANGEABLE
            .iter()
            .map(|(k, label)| json!({ "key": k, "label": label, "value": settings::get_value(&self.settings, k) }))
            .collect();
        let providers: Vec<Value> = self
            .providers()
            .iter()
            .map(|p| {
                json!({
                    "id": p.id, "name": p.label(),
                    "host": reqwest::Url::parse(&p.base_url).ok().and_then(|u| u.host_str().map(str::to_string)),
                    "has_key": self.api_key(p).is_some(),
                    "models": p.models.iter().map(|m| json!({"id": m.id, "context_tokens": m.context_tokens, "max_output_tokens": m.max_output_tokens})).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "model": self.settings.model,
            "mode": self.perms.mode.as_str(),
            "settings": items,
            "providers": providers,
            "mcp_servers": self.mcp.clients.iter().map(|c| json!({"id": c.id, "tools": c.tools.len()})).collect::<Vec<_>>(),
            "secret_ids": self.book().ids(),
            "note": "Keys and tokens are never shown. Providers, permissions, hooks, MCP servers and secrets can only be changed by the user in the settings file."
        })
        .to_string()
    }

    fn update_settings(&mut self, args: &Map<String, Value>) -> (String, bool) {
        let changes = args.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
        self.ui.tool_start("update_settings", &format!("{} 件", changes.len()));
        if changes.is_empty() || changes.len() > 6 {
            let e = "changes は 1〜6 件にしてください";
            self.ui.tool_end("update_settings", false, e);
            return (format!("Error: {e}"), false);
        }
        let mut next = self.settings.clone();
        let mut lines = vec![];
        let mut applied = vec![];
        for c in &changes {
            let key = c.get("key").and_then(Value::as_str).unwrap_or("").to_string();
            let value = c.get("value").cloned().unwrap_or(Value::Null);
            if let Some(why) = settings::blocked_reason(&key) {
                let e = format!("{key}: {why}");
                self.ui.tool_end("update_settings", false, &e);
                return (format!("Error: {e}. Changeable keys are listed by get_settings."), false);
            }
            match settings::with_json(&next, &key, value.clone()) {
                Ok(s) => {
                    let old = settings::get_value(&next, &key).map(|v| v.to_string()).unwrap_or_default();
                    let label = settings::CHAT_CHANGEABLE.iter().find(|(k, _)| *k == key).map_or("", |(_, l)| l);
                    lines.push(format!("{key}（{label}）: {old} → {value}"));
                    applied.push((key, value));
                    next = s;
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    self.ui.tool_end("update_settings", false, &e);
                    return (format!("Error: {e}"), false);
                }
            }
        }
        let prep = Prepared {
            kind: ToolKind::Settings,
            subjects: vec![],
            outside: false,
            secret: false,
            summary: String::new(),
            title: "設定を変えます".into(),
            detail: lines.join("\n"),
            writes: vec![],
        };
        if let Err(why) = self.authorize("update_settings", &prep) {
            self.ui.tool_end("update_settings", false, &why);
            return (format!("Error: {why}"), false);
        }
        self.settings = next;
        let file = self.paths.settings_file();
        for (k, v) in &applied {
            if let Err(e) = settings::persist(&file, k, v.clone()) {
                self.ui.notice(Level::Warn, &format!("{k} をファイルに保存できませんでした: {e:#}"));
            }
        }
        let now: Vec<String> = applied.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.ui.tool_end("update_settings", true, &now.join(", "));
        (format!("Applied (from the next request): {}", now.join(", ")), true)
    }

    fn developers(&self) -> (Vec<delegation::Developer>, Vec<String>) {
        let mut devs = vec![];
        let mut problems = vec![];
        let c = &self.settings.coding;
        for r in c.developer_refs() {
            match self.resolve(&r) {
                Ok((p, m)) => {
                    let max = if c.developer_max_tokens > 0 {
                        Some(c.developer_max_tokens)
                    } else {
                        m.max_tokens
                            .or_else(|| (m.max_output_tokens > 0).then_some(m.max_output_tokens))
                            .or(Some(self.settings.limits.default_max_tokens))
                    };
                    devs.push(delegation::Developer {
                        reference: r.clone(),
                        api_key: self.api_key(&p),
                        model: m.id.clone(),
                        temperature: m.temperature.or(Some(c.developer_temperature)),
                        max_tokens: if p.id == "demo" { None } else { max },
                        provider: p,
                    });
                }
                Err(e) => problems.push(e),
            }
        }
        (devs, problems)
    }

    async fn delegate(&mut self, args: &Map<String, Value>, cancel: &CancellationToken, meter: &mut RunMeter) -> (String, bool) {
        self.ui.tool_start("delegate", "開発モデルに渡します");
        let parsed = match delegation::parse(args, self.settings.coding.max_tasks) {
            Ok(a) => a,
            Err(e) => {
                self.ui.tool_end("delegate", false, &e);
                return (format!("Error: {e}. Write the delegate call again."), false);
            }
        };
        let (devs, problems) = self.developers();
        for p in &problems {
            self.ui.notice(Level::Warn, p);
        }
        if devs.is_empty() {
            let e = "使える開発モデルがありません（coding.developers を設定してください）";
            self.ui.tool_end("delegate", false, e);
            return (format!("Error: {e}. Do the work yourself."), false);
        }
        if parsed.revise.is_none() {
            let list: String = parsed
                .tasks
                .iter()
                .map(|t| {
                    format!(
                        "- **{}** {}{}\n",
                        t.id,
                        t.title,
                        if t.depends_on.is_empty() { String::new() } else { format!("（{} のあと）", t.depends_on.join("・")) }
                    )
                })
                .collect();
            self.ui.shown(&format!("```mermaid\n{}\n```\n{list}", parsed.plan.trim()));
        }
        struct P {
            ui: Arc<dyn Ui>,
            requests: AtomicU32,
        }
        impl delegation::Progress for P {
            fn status(&self, done: usize, total: usize, working: &[String]) {
                let now = working.first().map(|w| format!(" · {w}")).unwrap_or_default();
                self.ui.status(Some(&format!("開発モデルが作業しています（{done}/{total} 件完了）{now}")));
            }
            fn finished(&self, r: &delegation::JobResult) {
                self.ui.status(None);
                let model = if r.model.is_empty() { String::new() } else { format!(" · {}", r.model) };
                self.ui.tool_end(&r.id, r.ok, &format!("{} {}{model}", r.id, r.task.title));
            }
            fn request(&self) {
                self.requests.fetch_add(1, Ordering::Relaxed);
            }
        }
        let progress = P { ui: self.ui.clone(), requests: AtomicU32::new(0) };
        let limits = delegation::Limits {
            parallel: self.settings.coding.parallel,
            idle_timeout: Duration::from_secs(self.settings.limits.idle_timeout_secs),
        };
        let client = self.client.clone();
        let results = delegation::run(&parsed, &devs, &mut self.delegated, &client, cancel, &limits, &progress).await;
        meter.developer_requests += progress.requests.load(Ordering::Relaxed);
        let mut written: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if self.settings.coding.apply_files {
            written = self.apply_developer_files(&results);
        }
        let ok = results.iter().any(|r| r.ok);
        (delegation::report(&results, self.settings.coding.result_chars, &written), ok)
    }

    /// Writes the files the developers returned, through the usual permission (one card for all).
    fn apply_developer_files(&mut self, results: &[delegation::JobResult]) -> BTreeMap<String, Vec<String>> {
        let mut planned: Vec<(String, PathBuf, String, Prepared)> = vec![];
        let book = self.book();
        let roots = self.perms.roots.clone();
        let cancel = CancellationToken::new();
        for r in results.iter().filter(|r| r.ok) {
            for (path, content) in delegation::files_in(&r.answer) {
                let mut a = Map::new();
                a.insert("path".into(), json!(path));
                a.insert("content".into(), json!(content));
                let env =
                    Env { cwd: &self.cwd, roots: &roots, settings: &self.settings, book: &book, client: &self.client, cancel: &cancel };
                match tools::prepare("write_file", &a, &env) {
                    Ok(prep) => planned.push((r.id.clone(), prep.writes[0].clone(), content, prep)),
                    Err(e) => self.ui.notice(Level::Warn, &format!("{}: {path} を書けません: {e}", r.id)),
                }
            }
        }
        let mut written: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if planned.is_empty() {
            return written;
        }
        let mut ask = vec![];
        let mut allowed = vec![];
        for (i, (_, _, _, prep)) in planned.iter().enumerate() {
            match self.perms.decide("write_file", ToolKind::Write, &prep.subjects, prep.outside, false) {
                Decision::Allow => allowed.push(i),
                Decision::Ask(_) => ask.push(i),
                Decision::Deny(why) => self.ui.notice(Level::Warn, &format!("{}: {why}", prep.summary)),
            }
        }
        if !ask.is_empty() {
            let detail = ask.iter().map(|i| format!("{}\n{}", planned[*i].3.title, planned[*i].3.detail)).collect::<Vec<_>>().join("\n");
            let req = ApprovalRequest {
                tool: "delegate".into(),
                title: format!("開発モデルのファイルを {} 個書きます", ask.len()),
                detail,
                reason: String::new(),
                can_remember: false,
            };
            if matches!(self.approve(req), Approval::Yes | Approval::Always) {
                allowed.extend(ask);
            }
        }
        allowed.sort();
        for i in allowed {
            let (rid, path, content, _) = &planned[i];
            let _ = self.session.checkpoint(path);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(path, content) {
                Ok(()) => {
                    let shown = tools::shown_path(&self.cwd, path);
                    self.ui.tool_end("write_file", true, &format!("{rid} → {shown}"));
                    written.entry(rid.clone()).or_default().push(shown);
                }
                Err(e) => self.ui.notice(Level::Warn, &format!("{} に書けません: {e}", path.display())),
            }
        }
        written
    }

    /// Refuses a request that cannot fit, before anything is sent.
    fn precheck(&self, agent: &Agent) -> Result<(), String> {
        if !self.settings.token_precheck.enabled {
            return Ok(());
        }
        let (_, m) = self.resolve(&self.settings.model)?;
        if m.context_tokens == 0 {
            return Ok(());
        }
        let ctx = m.context_tokens as u64;
        let system = self.system_prompt(agent, &m);
        let tools = self.tool_specs(agent);
        let tool_tokens = compaction::estimate_tokens(&serde_json::to_string(&tools).unwrap_or_default());
        let overhead = compaction::estimate_tokens(&system) + tool_tokens;
        if overhead * 100 > ctx * self.settings.token_precheck.overhead_percent as u64 {
            let mcp_part = compaction::estimate_tokens(&serde_json::to_string(&self.mcp.specs()).unwrap_or_default());
            return Err(format!(
                "指示と道具の定義だけで約 {} トークンあり、文脈 {} の {}% を超えます（MCP の道具が約 {} トークン）。MCP サーバーを減らすか、mcp.lazy_threshold を下げてください",
                crate::ui::thousands(overhead),
                crate::ui::thousands(ctx),
                self.settings.token_precheck.overhead_percent,
                crate::ui::thousands(mcp_part)
            ));
        }
        let total = overhead + compaction::estimate_messages(&self.session.history);
        if total + 256 > ctx {
            return Err(format!(
                "会話が文脈（{}）に入りません（約 {}）。/compact で要約するか、/new で始め直してください",
                crate::ui::thousands(ctx),
                crate::ui::thousands(total)
            ));
        }
        Ok(())
    }

    /// `約 12,340 トークン（会話 8,000・指示 1,500・道具 2,500）／文脈 131,072（9%）`
    pub fn context_line(&self, agent: &Agent, input: &str) -> String {
        let Ok((_, m)) = self.resolve(&self.settings.model) else { return String::new() };
        let system = compaction::estimate_tokens(&self.system_prompt(agent, &m));
        let tools = compaction::estimate_tokens(&serde_json::to_string(&self.tool_specs(agent)).unwrap_or_default());
        let convo = compaction::estimate_messages(&self.session.history);
        let inp = compaction::estimate_tokens(input);
        let total = system + tools + convo + inp;
        let mut line = format!(
            "約 {} トークン（会話 {}・入力 {}・指示 {}・道具 {}）",
            crate::ui::thousands(total),
            crate::ui::thousands(convo),
            crate::ui::thousands(inp),
            crate::ui::thousands(system),
            crate::ui::thousands(tools)
        );
        if m.context_tokens > 0 {
            line.push_str(&format!(
                "／文脈 {}（{}%）",
                crate::ui::thousands(m.context_tokens as u64),
                total * 100 / m.context_tokens as u64
            ));
        }
        line
    }

    /// Makes room when the next request would pass `compaction.at_percent` of the context (or always,
    /// with [force]). Errors are only for a failed summary that was asked for by hand.
    pub async fn make_room(&mut self, agent: &Agent, force: bool, cancel: &CancellationToken, meter: &mut RunMeter) -> Result<(), String> {
        let c = self.settings.compaction.clone();
        if !c.enabled && !force {
            return Ok(());
        }
        let (_, m) = self.resolve(&self.settings.model)?;
        let ctx = m.context_tokens as u64;
        if ctx == 0 && !force {
            return Ok(());
        }
        let fixed = compaction::estimate_tokens(&self.system_prompt(agent, &m))
            + compaction::estimate_tokens(&serde_json::to_string(&self.tool_specs(agent)).unwrap_or_default());
        let fits = |h: &[Message]| ctx > 0 && (fixed + compaction::estimate_messages(h)) * 100 <= ctx * c.at_percent as u64;
        let before = fixed + compaction::estimate_messages(&self.session.history);
        if !force && fits(&self.session.history) {
            return Ok(());
        }
        let cut = compaction::cut_index(&self.session.history, c.keep_turns);
        // 1. old tool answers first
        if let Some(clipped) =
            compaction::clip_tool_results(&self.session.history, cut.unwrap_or(self.session.history.len()), c.tool_result_chars)
        {
            let _ = self.session.replace(clipped);
            if !force && fits(&self.session.history) {
                let after = fixed + compaction::estimate_messages(&self.session.history);
                self.ui.notice(
                    Level::Info,
                    &format!(
                        "古い道具の結果を切り詰めました（約 {} → {} トークン）",
                        crate::ui::thousands(before),
                        crate::ui::thousands(after)
                    ),
                );
                return Ok(());
            }
        }
        // 2. a summary of everything before the kept turns
        let Some(cut) = compaction::cut_index(&self.session.history, c.keep_turns) else {
            return if force { Err("要約できるほど前の会話がありません".into()) } else { Ok(()) };
        };
        let lines = compaction::lines(&self.session.history[..cut], 6000, c.tool_result_chars);
        let group_chars = if ctx > 0 { (ctx as usize * 4 / 10).max(4000) } else { 24_000 };
        let mut summary = String::new();
        self.ui.status(Some("会話の前半を要約しています…"));
        for group in compaction::chunks(&lines, group_chars) {
            let req_text = compaction::request(&group, &summary, c.summary_chars);
            meter.requests += 1;
            match self.plain_request(compaction::INSTRUCTION, &req_text, cancel).await {
                Ok(t) => match compaction::clean(&t) {
                    Some(s) => summary = s,
                    None => {
                        self.ui.status(None);
                        self.ui.notice(Level::Warn, "要約が空だったので、会話はそのまま送ります");
                        return if force { Err("要約が空でした".into()) } else { Ok(()) };
                    }
                },
                Err(e) => {
                    self.ui.status(None);
                    self.ui.notice(Level::Warn, &format!("要約できませんでした（{e}）。会話はそのまま送ります"));
                    return if force { Err(e) } else { Ok(()) };
                }
            }
        }
        self.ui.status(None);
        let n = cut;
        let replaced = compaction::replace(&self.session.history, cut, &summary);
        let _ = self.session.replace(replaced);
        let after = fixed + compaction::estimate_messages(&self.session.history);
        self.ui.notice(
            Level::Info,
            &format!(
                "会話の前半（{n} 件）を要約しました（約 {} → {} トークン）",
                crate::ui::thousands(before),
                crate::ui::thousands(after)
            ),
        );
        Ok(())
    }

    /// A request without tools or history, its text only (summaries).
    async fn plain_request(&mut self, system: &str, text: &str, cancel: &CancellationToken) -> Result<String, String> {
        let (p, m) = self.resolve(&self.settings.model)?;
        let key = self.api_key(&p);
        let messages = vec![Message::user(text)];
        let mut extra = Map::new();
        // a summary needs no long thinking
        extra.extend(m.thinking.body_fields(false, 0.0));
        let req = ChatRequest {
            provider: &p,
            model: &m.id,
            api_key: key,
            system: system.to_string(),
            messages: &messages,
            tools: &[],
            temperature: m.temperature.or(self.settings.online.temperature),
            top_p: None,
            max_tokens: Some(((self.settings.compaction.summary_chars / 2) as u32).max(1024)),
            extra,
            cache_key: None,
            pass_reasoning: false,
        };
        let mut ignore = |_e: StreamEvent| {};
        llm::stream_chat(&self.client, &req, &mut ignore, cancel, Duration::from_secs(self.settings.limits.idle_timeout_secs))
            .await
            .map(|c| c.text)
            .map_err(|e| e.message)
    }

    pub async fn compact_now(&mut self, cancel: &CancellationToken) -> Result<(), String> {
        let agent = self.last_agent.unwrap_or(&agents::CODING);
        let mut meter = RunMeter::default();
        self.make_room(agent, true, cancel, &mut meter).await
    }

    /// Puts back the files of the last turn that changed any, and tells the model.
    pub fn undo(&mut self) -> Result<Option<(u32, Vec<PathBuf>)>> {
        let r = self.session.undo()?;
        if let Some((turn, paths)) = &r {
            let shown: Vec<String> = paths.iter().map(|p| tools::shown_path(&self.cwd, p)).collect();
            self.session.push(Message::user(format!(
                "{FEEDBACK_PREFIX} The user undid the file changes of turn {turn}: {} are back as they were before it. Read them again before changing them.",
                shown.join(", ")
            )))?;
        }
        Ok(r)
    }

    pub fn set_mode(&mut self, mode: settings::PermissionMode) {
        self.perms.mode = mode;
    }

    pub fn add_dir(&mut self, dir: &Path) -> Result<PathBuf> {
        let p = dir.canonicalize()?;
        if !self.perms.roots.contains(&p) {
            self.perms.roots.push(p.clone());
        }
        Ok(p)
    }
}

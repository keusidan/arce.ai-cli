//! What the user sees and answers: the terminal (REPL and `-p` text) and JSON lines for scripts.
//!
//! The controller only talks to [Ui]; how a reply looks is decided here. In the terminal a reply has
//! no name, icon or frame: thinking (folded to a line or streamed faintly), tool lines, the text, and
//! a faint summary line at the end.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::llm::Usage;
use crate::markdown::Renderer;
use crate::settings::ShowThinking;
use crate::theme::{self, Style, Token, display_width, paint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// yes, and do not ask about this tool again in this session
    Always,
    No(Option<String>),
}

pub struct ApprovalRequest {
    pub tool: String,
    /// one line: `$ cargo test`, `src/main.rs を書き換えます`
    pub title: String,
    /// the rest of the card (a diff, the arguments)
    pub detail: String,
    /// why it asks, when it is not the usual reason
    pub reason: String,
    /// whether "do not ask again" is offered (never for commands)
    pub can_remember: bool,
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    pub step: String,
    pub status: String,
}

/// Counts of one reply: requests to models, tools run, time, tokens.
#[derive(Clone, Debug)]
pub struct RunMeter {
    pub started: Instant,
    pub requests: u32,
    pub developer_requests: u32,
    pub tools: u32,
    pub usage: Usage,
    pub has_usage: bool,
}

impl Default for RunMeter {
    fn default() -> Self {
        RunMeter { started: Instant::now(), requests: 0, developer_requests: 0, tools: 0, usage: Usage::default(), has_usage: false }
    }
}

pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl RunMeter {
    pub fn add_usage(&mut self, u: &Usage) {
        self.usage.add(u);
        self.has_usage = true;
    }

    /// `通信 5 回（プランナー 3・開発 2） · ツール 1 回 · 所要 1:24 · 入力 12,340 トークン（キャッシュ 9,800）· 出力 512`
    pub fn line(&self) -> String {
        let mut parts = vec![];
        let total = self.requests + self.developer_requests;
        if self.developer_requests > 0 {
            parts.push(format!("通信 {total} 回（プランナー {}・開発 {}）", self.requests, self.developer_requests));
        } else {
            parts.push(format!("通信 {total} 回"));
        }
        if self.tools > 0 {
            parts.push(format!("ツール {} 回", self.tools));
        }
        parts.push(format!("所要 {}", clock(self.started.elapsed())));
        if self.has_usage {
            let cache = self.usage.cached.map(|c| format!("（キャッシュ {}）", thousands(c))).unwrap_or_default();
            parts.push(format!("入力 {} トークン{cache}", thousands(self.usage.input)));
            parts.push(format!("出力 {}", thousands(self.usage.output)));
        }
        parts.join(" · ")
    }
}

/// The last headline a model wrote in its thinking (`**…**`, `## …`) or its last line.
pub fn thinking_headline(text: &str) -> String {
    for line in text.lines().rev() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("**").and_then(|r| r.strip_suffix("**")) {
            return h.trim().to_string();
        }
        if t.starts_with('#') {
            return t.trim_start_matches('#').trim().to_string();
        }
    }
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

pub trait Ui: Send + Sync {
    fn interactive(&self) -> bool;
    /// a reply begins
    fn reply_start(&self) {}
    fn text(&self, s: &str);
    fn reasoning(&self, s: &str);
    fn reasoning_end(&self, secs: f64);
    fn tool_start(&self, name: &str, summary: &str);
    fn tool_end(&self, name: &str, ok: bool, summary: &str);
    fn notice(&self, level: Level, msg: &str);
    /// a passing line ("waiting for the model 12秒"); None clears it
    fn status(&self, msg: Option<&str>);
    fn approve(&self, req: &ApprovalRequest) -> Approval;
    fn ask(&self, question: &str, options: &[String]) -> Option<String>;
    fn plan(&self, items: &[PlanItem]);
    /// Markdown the user should see (a delegated plan, a file a developer wrote)
    fn shown(&self, markdown: &str);
    fn reply_end(&self, meter: &RunMeter, ok: bool);
}

struct TermState {
    md: Renderer,
    in_thinking: bool,
    thinking_buf: String,
    thinking_started: Option<Instant>,
    status_shown: bool,
    text_started: bool,
    /// the text so far ends with a new line
    at_line_start: bool,
}

/// The terminal: Markdown when stdout is a terminal, plain text otherwise.
pub struct TerminalUi {
    state: Mutex<TermState>,
    interactive: bool,
    markdown: bool,
    show_thinking: ShowThinking,
    run_summary: bool,
    /// progress goes to stderr when stdout is for the answer only (`-p`)
    progress_to_stderr: bool,
    stderr_tty: bool,
}

impl TerminalUi {
    pub fn new(
        interactive: bool,
        markdown: bool,
        show_thinking: ShowThinking,
        run_summary: bool,
        fold: usize,
        progress_to_stderr: bool,
    ) -> TerminalUi {
        let stdout_tty = std::io::stdout().is_terminal();
        TerminalUi {
            state: Mutex::new(TermState {
                md: Renderer::new(theme::term_width(), fold),
                in_thinking: false,
                thinking_buf: String::new(),
                thinking_started: None,
                status_shown: false,
                text_started: false,
                at_line_start: true,
            }),
            interactive,
            markdown: markdown && stdout_tty,
            show_thinking,
            run_summary,
            progress_to_stderr,
            stderr_tty: std::io::stderr().is_terminal(),
        }
    }

    /// Progress lines (tools, thinking, notices) go here.
    fn side(&self, s: &str) {
        if self.progress_to_stderr {
            if self.stderr_tty {
                eprint!("{s}");
            } else {
                eprint!("{}", theme::strip_ansi(s));
            }
            let _ = std::io::stderr().flush();
        } else {
            print!("{s}");
            let _ = std::io::stdout().flush();
        }
    }

    fn clear_status(&self, st: &mut TermState) {
        if st.status_shown {
            eprint!("\r\x1b[2K");
            st.status_shown = false;
        }
    }

    fn end_thinking(&self, st: &mut TermState) {
        if st.in_thinking {
            st.in_thinking = false;
            if self.show_thinking == ShowThinking::Full && !st.thinking_buf.ends_with('\n') {
                self.side("\n");
            }
        }
    }

    fn break_text_line(&self, st: &mut TermState) {
        // a tool line or notice after text that did not end its line
        if st.text_started && !st.at_line_start {
            let rest = if self.markdown { st.md.finish() } else { "\n".into() };
            print!("{rest}");
            let _ = std::io::stdout().flush();
            st.at_line_start = true;
        }
    }
}

impl Ui for TerminalUi {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn reply_start(&self) {
        let mut st = self.state.lock().unwrap();
        st.text_started = false;
        st.at_line_start = true;
        st.thinking_buf.clear();
        st.md = Renderer::new(theme::term_width(), st.md.fold);
    }

    fn text(&self, s: &str) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.end_thinking(&mut st);
        if !st.text_started {
            st.text_started = true;
        }
        let out = if self.markdown { st.md.push(s) } else { s.to_string() };
        if !out.is_empty() {
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
        if !s.is_empty() {
            st.at_line_start = s.ends_with('\n');
        }
    }

    fn reasoning(&self, s: &str) {
        let mut st = self.state.lock().unwrap();
        if self.show_thinking == ShowThinking::Hidden {
            return;
        }
        if !st.in_thinking {
            st.in_thinking = true;
            st.thinking_started.get_or_insert_with(Instant::now);
            if self.show_thinking == ShowThinking::Full {
                self.clear_status(&mut st);
                self.break_text_line(&mut st);
                self.side(&format!("{}\n", paint("◆ 考えています…", Style::fg(Token::Accent2))));
            }
        }
        st.thinking_buf.push_str(s);
        match self.show_thinking {
            ShowThinking::Full => self.side(&paint(s, Style::fg(Token::Faint))),
            ShowThinking::Summary => {
                let secs = st.thinking_started.map_or(0, |t| t.elapsed().as_secs());
                let head = thinking_headline(&st.thinking_buf);
                let width = theme::term_width().saturating_sub(24);
                let head: String = head
                    .chars()
                    .scan(0, |w, c| {
                        *w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                        (*w <= width).then_some(c)
                    })
                    .collect();
                if self.stderr_tty {
                    eprint!(
                        "\r\x1b[2K{} {}",
                        paint(&format!("◆ 考えています… {secs}秒"), Style::fg(Token::Accent2)),
                        paint(&head, Style::fg(Token::Faint))
                    );
                    st.status_shown = true;
                }
            }
            ShowThinking::Hidden => {}
        }
    }

    fn reasoning_end(&self, secs: f64) {
        let mut st = self.state.lock().unwrap();
        if self.show_thinking == ShowThinking::Hidden || st.thinking_buf.is_empty() {
            st.in_thinking = false;
            return;
        }
        self.clear_status(&mut st);
        self.end_thinking(&mut st);
        let chars = st.thinking_buf.chars().count();
        self.side(&format!("{}\n", paint(&format!("◇ 思考（{secs:.1}秒 · {} 文字）", thousands(chars as u64)), Style::fg(Token::Faint))));
        st.thinking_buf.clear();
        st.thinking_started = None;
    }

    fn tool_start(&self, name: &str, summary: &str) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.end_thinking(&mut st);
        self.break_text_line(&mut st);
        self.side(&format!(
            "{} {} {}\n",
            paint("●", Style::fg(Token::Accent)),
            paint(name, Style::fg(Token::Ink).bold()),
            paint(summary, Style::fg(Token::Muted))
        ));
    }

    fn tool_end(&self, _name: &str, ok: bool, summary: &str) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        let mark = if ok { paint("⎿", Style::fg(Token::Faint)) } else { paint("⎿", Style::fg(Token::Red)) };
        let text = if ok { paint(summary, Style::fg(Token::Faint)) } else { paint(summary, Style::fg(Token::Red)) };
        self.side(&format!("  {mark} {text}\n"));
    }

    fn notice(&self, level: Level, msg: &str) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.end_thinking(&mut st);
        self.break_text_line(&mut st);
        let (mark, style) = match level {
            Level::Info => ("ℹ", Style::fg(Token::Muted)),
            Level::Warn => ("▲", Style::fg(Token::Amber)),
            Level::Error => ("✕", Style::fg(Token::Red)),
        };
        let line = format!("{} {}\n", paint(mark, style), paint(msg, style));
        if level == Level::Info {
            self.side(&line);
        } else {
            eprint!("{}", if self.stderr_tty { line } else { theme::strip_ansi(&line) });
        }
    }

    fn status(&self, msg: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        if !self.stderr_tty {
            return;
        }
        match msg {
            Some(m) => {
                eprint!("\r\x1b[2K{}", paint(m, Style::fg(Token::Muted)));
                st.status_shown = true;
            }
            None => self.clear_status(&mut st),
        }
        let _ = std::io::stderr().flush();
    }

    fn approve(&self, req: &ApprovalRequest) -> Approval {
        if !self.interactive {
            return Approval::No(Some("非対話モードでは承認できません（--mode か permissions.allow で許可してください）".into()));
        }
        {
            let mut st = self.state.lock().unwrap();
            self.clear_status(&mut st);
            self.end_thinking(&mut st);
            self.break_text_line(&mut st);
        }
        let edge = |s: &str| paint(s, Style::fg(Token::Amber));
        let mut card = format!("{} {}\n", edge("╭ 確認"), paint(&req.tool, Style::fg(Token::Ink).bold()));
        card.push_str(&format!("{} {}\n", edge("│"), req.title));
        for line in req.detail.lines().take(60) {
            card.push_str(&format!("{} {line}\n", edge("│")));
        }
        if req.detail.lines().count() > 60 {
            card.push_str(&format!(
                "{} {}\n",
                edge("│"),
                paint(&format!("…ほか {} 行", req.detail.lines().count() - 60), Style::fg(Token::Faint))
            ));
        }
        if !req.reason.is_empty() {
            card.push_str(&format!("{} {}\n", edge("│"), paint(&format!("理由: {}", req.reason), Style::fg(Token::Amber))));
        }
        let mut choices = format!("{} 実行する", paint("[y]", Style::fg(Token::Green).bold()));
        if req.can_remember {
            choices.push_str(&format!("  {} この道具は以後聞かない", paint("[a]", Style::fg(Token::Accent).bold())));
        }
        choices.push_str(&format!("  {} やめる（続けて理由を書けます）", paint("[n]", Style::fg(Token::Red).bold())));
        card.push_str(&format!("{} {choices}\n", edge("╰")));
        eprint!("{card}{} ", paint("›", Style::fg(Token::Accent)));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Approval::No(None);
        }
        let t = line.trim();
        let lower = t.to_lowercase();
        if lower == "y" || lower == "yes" || t == "はい" {
            Approval::Yes
        } else if (lower == "a" || lower == "always") && req.can_remember {
            Approval::Always
        } else {
            let reason = t.trim_start_matches(['n', 'N']).trim_start_matches([':', '：', ' ']).trim();
            Approval::No((!reason.is_empty()).then(|| reason.to_string()))
        }
    }

    fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        if !self.interactive {
            return None;
        }
        {
            let mut st = self.state.lock().unwrap();
            self.clear_status(&mut st);
            self.end_thinking(&mut st);
            self.break_text_line(&mut st);
        }
        let mut card = format!("{} {}\n", paint("╭ 質問", Style::fg(Token::Accent)), paint(question, Style::fg(Token::Ink).bold()));
        for (i, o) in options.iter().enumerate() {
            card.push_str(&format!(
                "{} {} {o}\n",
                paint("│", Style::fg(Token::Accent)),
                paint(&format!("{}.", i + 1), Style::fg(Token::Accent))
            ));
        }
        let hint = if options.is_empty() { "答えを書いてください" } else { "番号か、自由な答え" };
        card.push_str(&format!("{} {}\n", paint("╰", Style::fg(Token::Accent)), paint(hint, Style::fg(Token::Faint))));
        eprint!("{card}{} ", paint("›", Style::fg(Token::Accent)));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let t = line.trim();
        if let Ok(n) = t.parse::<usize>()
            && n >= 1
            && n <= options.len()
        {
            return Some(options[n - 1].clone());
        }
        (!t.is_empty()).then(|| t.to_string())
    }

    fn plan(&self, items: &[PlanItem]) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.break_text_line(&mut st);
        let mut s = String::new();
        for it in items {
            let (mark, style) = match it.status.as_str() {
                "completed" => ("☑", Style::fg(Token::Green)),
                "in_progress" => ("◐", Style::fg(Token::Accent)),
                _ => ("☐", Style::fg(Token::Muted)),
            };
            let text = if it.status == "completed" {
                paint(&it.step, Style::fg(Token::Faint).strike())
            } else {
                paint(&it.step, Style::fg(Token::Ink))
            };
            s.push_str(&format!("  {} {text}\n", paint(mark, style)));
        }
        self.side(&s);
    }

    fn shown(&self, markdown: &str) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.break_text_line(&mut st);
        let out = if self.markdown {
            let mut r = Renderer::new(theme::term_width(), st.md.fold);
            let mut o = r.push(markdown);
            o.push_str(&r.push("\n"));
            o.push_str(&r.finish());
            o
        } else {
            format!("{markdown}\n")
        };
        self.side(&out);
    }

    fn reply_end(&self, meter: &RunMeter, ok: bool) {
        let mut st = self.state.lock().unwrap();
        self.clear_status(&mut st);
        self.end_thinking(&mut st);
        let rest = if self.markdown { st.md.finish() } else { String::new() };
        if !rest.is_empty() {
            print!("{rest}");
        } else if st.text_started && !st.at_line_start {
            println!();
        }
        let _ = std::io::stdout().flush();
        st.text_started = false;
        st.at_line_start = true;
        if self.run_summary && (self.interactive || self.stderr_tty) {
            let line = paint(&meter.line(), Style::fg(Token::Faint));
            let mark = if ok { "" } else { "✕ " };
            if self.progress_to_stderr {
                eprintln!("{mark}{line}");
            } else {
                println!("{mark}{line}");
            }
        }
    }
}

/// JSON for scripts: one object at the end (`json`), or one line per event (`stream-json`).
pub struct JsonUi {
    pub stream: bool,
    pub text: Mutex<String>,
    pub events: Mutex<Vec<Value>>,
}

impl JsonUi {
    pub fn new(stream: bool) -> JsonUi {
        JsonUi { stream, text: Mutex::new(String::new()), events: Mutex::new(vec![]) }
    }

    fn emit(&self, v: Value) {
        if self.stream {
            println!("{v}");
            let _ = std::io::stdout().flush();
        } else {
            self.events.lock().unwrap().push(v);
        }
    }
}

impl Ui for JsonUi {
    fn interactive(&self) -> bool {
        false
    }
    fn text(&self, s: &str) {
        self.text.lock().unwrap().push_str(s);
        if self.stream {
            self.emit(json!({ "type": "text", "text": s }));
        }
    }
    fn reasoning(&self, s: &str) {
        if self.stream {
            self.emit(json!({ "type": "reasoning", "text": s }));
        }
    }
    fn reasoning_end(&self, secs: f64) {
        if self.stream {
            self.emit(json!({ "type": "reasoning_end", "seconds": secs }));
        }
    }
    fn tool_start(&self, name: &str, summary: &str) {
        self.emit(json!({ "type": "tool_call", "name": name, "summary": summary }));
    }
    fn tool_end(&self, name: &str, ok: bool, summary: &str) {
        self.emit(json!({ "type": "tool_result", "name": name, "ok": ok, "summary": summary }));
    }
    fn notice(&self, level: Level, msg: &str) {
        let l = match level {
            Level::Info => "info",
            Level::Warn => "warning",
            Level::Error => "error",
        };
        self.emit(json!({ "type": "notice", "level": l, "message": msg }));
    }
    fn status(&self, _msg: Option<&str>) {}
    fn approve(&self, req: &ApprovalRequest) -> Approval {
        self.emit(json!({ "type": "approval_refused", "tool": req.tool, "title": req.title }));
        Approval::No(Some("非対話モードでは承認できません（--mode か permissions.allow で許可してください）".into()))
    }
    fn ask(&self, question: &str, _options: &[String]) -> Option<String> {
        self.emit(json!({ "type": "question_unanswered", "question": question }));
        None
    }
    fn plan(&self, items: &[PlanItem]) {
        self.emit(
            json!({ "type": "plan", "items": items.iter().map(|i| json!({"step": i.step, "status": i.status})).collect::<Vec<_>>() }),
        );
    }
    fn shown(&self, markdown: &str) {
        self.emit(json!({ "type": "shown", "markdown": markdown }));
    }
    fn reply_end(&self, meter: &RunMeter, ok: bool) {
        if self.stream {
            self.emit(
                json!({ "type": "reply_end", "ok": ok, "requests": meter.requests + meter.developer_requests, "tools": meter.tools }),
            );
        }
    }
}

/// Pads [text] to [width] columns.
pub fn pad(text: &str, width: usize) -> String {
    let w = display_width(text);
    if w >= width { text.to_string() } else { format!("{text}{}", " ".repeat(width - w)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_line_reads_like_the_app() {
        let mut m = RunMeter { requests: 3, developer_requests: 2, tools: 1, ..Default::default() };
        m.add_usage(&Usage { input: 12340, output: 512, cached: Some(9800) });
        let line = m.line();
        assert!(line.starts_with("通信 5 回（プランナー 3・開発 2） · ツール 1 回 · 所要 0:00"), "{line}");
        assert!(line.ends_with("入力 12,340 トークン（キャッシュ 9,800） · 出力 512"), "{line}");
    }

    #[test]
    fn headline_prefers_bold_and_headings() {
        assert_eq!(thinking_headline("**Plan**\nstep one\n"), "Plan");
        assert_eq!(thinking_headline("a\n## Check tests\nb"), "Check tests");
        assert_eq!(thinking_headline("first\nlast\n"), "last");
    }

    #[test]
    fn clock_and_thousands() {
        assert_eq!(clock(Duration::from_secs(84)), "1:24");
        assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }
}

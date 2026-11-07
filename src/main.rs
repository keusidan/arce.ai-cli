//! `arce`: ARCE.AI in the terminal.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use arce::app::{self, Options};
use arce::controller::Outcome;
use arce::provider::{self, ModelConfig, ProviderConfig};
use arce::secrets::SecretStore;
use arce::session;
use arce::settings::{self, Paths};
use arce::theme::{accent, faint, green, muted, red};
use arce::ui::{JsonUi, TerminalUi, Ui, pad};

#[derive(Parser)]
#[command(name = "arce", version, about = "ARCE.AI — ターミナルで使うコーディング／チャットのアシスタント", long_about = None)]
struct Cli {
    /// 最初に送るメッセージ（-p のときは、送って終わる）
    prompt: Vec<String>,
    /// 非対話：送って、答えを出して終わる（標準入力も読む）
    #[arg(short = 'p', long)]
    print: bool,
    /// -p の出力の形
    #[arg(long, value_enum, default_value = "text")]
    output_format: OutputFormat,
    /// このフォルダの最後のセッションの続きから
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// セッションを id で開く
    #[arg(short = 'r', long)]
    resume: Option<String>,
    /// 使うモデル（provider|model）
    #[arg(short = 'm', long)]
    model: Option<String>,
    /// 承認のモード: ask / auto-edit / plan / yolo
    #[arg(long)]
    mode: Option<String>,
    /// エージェントを固定: auto / general / coding / research / artifact / file-analysis
    #[arg(long)]
    agent: Option<String>,
    /// 作業フォルダに足す（何度でも）
    #[arg(long = "add-dir")]
    add_dir: Vec<PathBuf>,
    /// この実行だけの許可の規則（例: "run_command(cargo test*)"）
    #[arg(long)]
    allow: Vec<String>,
    /// この実行だけの禁止の規則
    #[arg(long)]
    deny: Vec<String>,
    /// セッションを保存しない
    #[arg(long)]
    no_session: bool,
    /// 作業フォルダ（既定は今のフォルダ）
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Subcommand)]
enum Command {
    /// 設定を読む・変える（settings.toml）
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// プロバイダ（OpenAI 互換の接続先）
    Providers {
        #[command(subcommand)]
        action: ProvidersCmd,
    },
    /// API キーとトークン（secrets.json、所有者だけが読める）
    Secrets {
        #[command(subcommand)]
        action: SecretsCmd,
    },
    /// 保存したセッション
    Sessions {
        #[command(subcommand)]
        action: SessionsCmd,
    },
    /// このフォルダ（プロジェクト）を信頼する。信頼すると .arce/settings.toml の送り先・フック・権限も読む
    Trust {
        path: Option<PathBuf>,
        /// 信頼をやめる
        #[arg(long)]
        off: bool,
    },
    /// 設定・キー・接続を確かめる
    Doctor {
        /// 各プロバイダの /models に問い合わせる
        #[arg(long)]
        ping: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 値を出す（キーは a.b の形）
    Get { key: String },
    /// 値を変える（JSON として読めればその型、読めなければ文字列）
    Set {
        key: String,
        value: String,
        /// プロジェクトの .arce/settings.toml に書く
        #[arg(long)]
        project: bool,
    },
    /// すべての設定（合わせたあと）
    List,
    /// 設定ファイルの場所
    Path,
}

#[derive(Subcommand)]
enum ProvidersCmd {
    List,
    /// 足す（--preset か、--id と --url）
    Add {
        #[arg(long)]
        preset: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// function calling がないサーバー
        #[arg(long)]
        text_tools: bool,
    },
    /// 消す
    Remove {
        id: String,
    },
    /// 提供元の /models を読んで並べる
    Models {
        id: String,
    },
    /// モデルを足す（上限が分かれば一緒に）
    AddModel {
        provider: String,
        model: String,
        #[arg(long)]
        context: Option<u32>,
        #[arg(long)]
        max_output: Option<u32>,
        /// 思考の送り方: none / deepseek / zai / chat_template / openai / openrouter
        #[arg(long)]
        thinking: Option<String>,
    },
    /// 用意されたプロバイダの一覧
    Presets,
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// 値を入れる（画面に出さずに読む。標準入力からも可）
    Set {
        id: String,
        /// この秘密を送ってよいホスト（何度でも）。プロバイダのキーは自動で、その base_url のホスト
        #[arg(long)]
        host: Vec<String>,
    },
    /// 名前・ホスト・値があるかだけを出す（値は出さない）
    List,
    Rm {
        id: String,
    },
}

#[derive(Subcommand)]
enum SessionsCmd {
    List,
    Search { query: Vec<String> },
    Show { id: String },
    Rm { id: String },
}

fn options(cli: &Cli, interactive: bool) -> Options {
    Options {
        cwd: cli.cwd.clone(),
        model: cli.model.clone(),
        mode: cli.mode.clone(),
        agent: cli.agent.clone(),
        add_dirs: cli.add_dir.clone(),
        allow: cli.allow.clone(),
        deny: cli.deny.clone(),
        resume: cli.resume.clone(),
        continue_last: cli.continue_last,
        no_session: cli.no_session,
        interactive,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", red("✕"));
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    if let Some(cmd) = &cli.command {
        subcommand(cmd, &cli).await?;
        return Ok(0);
    }
    let mut prompt = cli.prompt.join(" ");
    if cli.print {
        if !std::io::stdin().is_terminal() {
            let mut piped = String::new();
            std::io::stdin().read_to_string(&mut piped)?;
            if !piped.trim().is_empty() {
                prompt = if prompt.is_empty() { piped } else { format!("{prompt}\n\n{piped}") };
            }
        }
        if prompt.trim().is_empty() {
            bail!("-p には、送るメッセージが要ります（引数か標準入力）");
        }
        return headless(&cli, &prompt).await;
    }
    let opt = options(&cli, true);
    let c = app::build(&opt, |s| -> Arc<dyn Ui> {
        Arc::new(TerminalUi::new(true, s.ui.markdown, s.ui.show_thinking, s.ui.run_summary, s.ui.code_fold_lines, false))
    })
    .await?;
    arce::repl::run(c, (!prompt.trim().is_empty()).then_some(prompt)).await?;
    Ok(0)
}

async fn headless(cli: &Cli, prompt: &str) -> Result<i32> {
    let opt = options(cli, false);
    let json_ui = Arc::new(JsonUi::new(cli.output_format == OutputFormat::StreamJson));
    let format = cli.output_format;
    let ui_for_build = json_ui.clone();
    let mut c = app::build(&opt, move |s| -> Arc<dyn Ui> {
        match format {
            OutputFormat::Text => {
                Arc::new(TerminalUi::new(false, s.ui.markdown, s.ui.show_thinking, s.ui.run_summary, s.ui.code_fold_lines, true))
            }
            _ => ui_for_build,
        }
    })
    .await?;
    let started = std::time::Instant::now();
    let outcome = arce::repl::send(&mut c, prompt).await;
    let meter = c.last_meter.clone().unwrap_or_default();
    let code = match &outcome {
        Outcome::Done => 0,
        Outcome::Cancelled => 130,
        Outcome::Failed(_) => 1,
    };
    if format == OutputFormat::Json || format == OutputFormat::StreamJson {
        let result = json_ui.text.lock().unwrap().clone();
        let (status, error) = match &outcome {
            Outcome::Done => ("done", Value::Null),
            Outcome::Cancelled => ("cancelled", Value::Null),
            Outcome::Failed(e) => ("failed", json!(e)),
        };
        let mut v = json!({
            "type": "result",
            "status": status,
            "error": error,
            "result": result,
            "session_id": c.session.id,
            "model": c.settings.model,
            "requests": meter.requests + meter.developer_requests,
            "tools": meter.tools,
            "duration_ms": started.elapsed().as_millis() as u64,
        });
        if meter.has_usage {
            v["usage"] =
                json!({ "input_tokens": meter.usage.input, "output_tokens": meter.usage.output, "cached_tokens": meter.usage.cached });
        }
        if format == OutputFormat::Json {
            v["events"] = Value::Array(json_ui.events.lock().unwrap().clone());
        }
        println!("{v}");
    }
    Ok(code)
}

fn user_file_value(paths: &Paths) -> Result<Value> {
    Ok(settings::read_toml(&paths.settings_file())?.unwrap_or(json!({})))
}

fn user_providers(paths: &Paths) -> Result<Vec<ProviderConfig>> {
    let v = user_file_value(paths)?;
    Ok(match v.get("providers") {
        Some(p) => serde_json::from_value(p.clone()).context("settings.toml の providers が読めません")?,
        None => vec![],
    })
}

fn save_providers(paths: &Paths, list: &[ProviderConfig]) -> Result<()> {
    settings::persist(&paths.settings_file(), "providers", serde_json::to_value(list)?)
}

async fn subcommand(cmd: &Command, cli: &Cli) -> Result<()> {
    let paths = Paths::detect();
    let cwd = app::working_dir(&options(cli, false))?;
    match cmd {
        Command::Config { action } => match action {
            ConfigCmd::Path => {
                println!("{}", paths.settings_file().display());
                println!("{}", settings::project_root(&cwd).join(".arce/settings.toml").display());
            }
            ConfigCmd::List => {
                let loaded = settings::load(&paths, &cwd)?;
                let t: toml::Value = serde_json::from_value(strip_null(serde_json::to_value(&loaded.settings)?))?;
                print!("{}", toml::to_string_pretty(&t)?);
                for w in loaded.warnings {
                    eprintln!("{} {w}", arce::theme::amber("▲"));
                }
            }
            ConfigCmd::Get { key } => {
                let loaded = settings::load(&paths, &cwd)?;
                match settings::get_value(&loaded.settings, key) {
                    Some(v) => println!("{}", if let Value::String(s) = &v { s.clone() } else { v.to_string() }),
                    None => bail!("{key} は設定の名前ではありません"),
                }
            }
            ConfigCmd::Set { key, value, project } => {
                let parsed: Value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone()));
                settings::with_json(&settings::Settings::default(), key, parsed.clone())?;
                let file = if *project { settings::project_root(&cwd).join(".arce/settings.toml") } else { paths.settings_file() };
                settings::persist(&file, key, parsed)?;
                println!("{} {key} = {value}  {}", green("✓"), faint(&file.display().to_string()));
            }
        },
        Command::Providers { action } => providers(action, &paths).await?,
        Command::Secrets { action } => secrets_cmd(action, &paths)?,
        Command::Sessions { action } => {
            let dir = paths.sessions_dir();
            match action {
                SessionsCmd::List => {
                    for i in session::list(&dir) {
                        println!(
                            "{} {} {}",
                            accent(&i.id),
                            pad(&i.title, 40),
                            faint(&format!("{} 件 · {} · {}", i.messages, i.model, i.cwd))
                        );
                    }
                }
                SessionsCmd::Search { query } => {
                    for (i, hit) in session::search(&dir, &query.join(" ")) {
                        println!("{} {}\n    {}", accent(&i.id), i.title, faint(&hit));
                    }
                }
                SessionsCmd::Show { id } => {
                    let s = session::Session::open(&dir, id)?;
                    print!("{}", session::to_markdown(&s.title, &s.history));
                }
                SessionsCmd::Rm { id } => {
                    if !arce::secrets::valid_id(id) {
                        bail!("id が正しくありません");
                    }
                    std::fs::remove_file(dir.join(format!("{id}.jsonl"))).with_context(|| format!("セッション {id} がありません"))?;
                    let sub = dir.join(id);
                    if sub.is_dir() {
                        std::fs::remove_dir_all(sub)?;
                    }
                    println!("{} 消しました", green("✓"));
                }
            }
        }
        Command::Trust { path, off } => {
            let root = settings::project_root(&path.clone().map_or(cwd.clone(), |p| cwd.join(p)));
            settings::trust(&paths, &root, !off)?;
            println!("{} {} を{}", green("✓"), root.display(), if *off { "信頼しないことにしました" } else { "信頼しました" });
        }
        Command::Doctor { ping } => doctor(&paths, &cwd, *ping).await?,
    }
    Ok(())
}

fn strip_null(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_null(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_null).collect()),
        o => o,
    }
}

async fn providers(action: &ProvidersCmd, paths: &Paths) -> Result<()> {
    match action {
        ProvidersCmd::Presets => {
            for p in provider::presets() {
                println!("{} {} {}", accent(&pad(&p.id, 12)), pad(&p.name, 28), faint(&p.base_url));
            }
        }
        ProvidersCmd::List => {
            let store = SecretStore::load(paths.secrets_file());
            let list = user_providers(paths)?;
            if list.is_empty() {
                println!("{}", muted("プロバイダはありません（arce providers add --preset openrouter など）"));
            }
            for p in list {
                let key = if !p.api_key_env.is_empty() && std::env::var(&p.api_key_env).is_ok() || store.value(&p.id).is_some() {
                    green("キーあり")
                } else {
                    red("キーなし")
                };
                println!("{} {} {} {}", accent(&pad(&p.id, 12)), pad(p.label(), 24), key, faint(&p.base_url));
                for m in &p.models {
                    let limits = match (m.context_tokens, m.max_output_tokens) {
                        (0, 0) => String::new(),
                        (c, o) => format!("文脈 {c} · 出力 {o}"),
                    };
                    println!("    {} {}", m.id, faint(&limits));
                }
            }
        }
        ProvidersCmd::Add { preset, id, url, name, text_tools } => {
            let mut p = match preset {
                Some(pid) => provider::presets()
                    .into_iter()
                    .find(|p| p.id == *pid)
                    .with_context(|| format!("プリセット {pid} はありません（arce providers presets）"))?,
                None => ProviderConfig {
                    id: id.clone().context("--id が要ります")?,
                    base_url: url.clone().context("--url が要ります")?,
                    ..Default::default()
                },
            };
            if let Some(i) = id {
                p.id = i.clone();
            }
            if let Some(u) = url {
                p.base_url = u.clone();
            }
            if let Some(n) = name {
                p.name = n.clone();
            }
            if *text_tools {
                p.tool_mode = provider::ToolMode::Text;
            }
            let problems = p.problems();
            if !problems.is_empty() {
                bail!("{}", problems.join("\n"));
            }
            let mut list = user_providers(paths)?;
            if list.iter().any(|x| x.id == p.id) {
                bail!("{} はもうあります（消すには arce providers remove {}）", p.id, p.id);
            }
            if provider::sends_secrets_in_the_clear(&p.base_url) {
                eprintln!("{} このアドレスは暗号化されません（キーが読まれるおそれがあります）", arce::theme::amber("▲"));
            }
            let id = p.id.clone();
            list.push(p);
            save_providers(paths, &list)?;
            println!("{} {id} を足しました", green("✓"));
            println!(
                "{}",
                faint(&format!(
                    "次に: arce secrets set {id}  →  arce providers models {id}  →  arce providers add-model {id} <モデル>  →  arce config set model \"{id}|<モデル>\""
                ))
            );
        }
        ProvidersCmd::Remove { id } => {
            let mut list = user_providers(paths)?;
            let before = list.len();
            list.retain(|p| p.id != *id);
            if list.len() == before {
                bail!("{id} はありません");
            }
            save_providers(paths, &list)?;
            println!("{} {id} を消しました（キーは arce secrets rm {id} で消せます）", green("✓"));
        }
        ProvidersCmd::AddModel { provider: pid, model, context, max_output, thinking } => {
            let mut list = user_providers(paths)?;
            let p = list.iter_mut().find(|p| p.id == *pid).with_context(|| format!("{pid} はありません"))?;
            let mut m =
                p.models.iter().find(|m| m.id == *model).cloned().unwrap_or(ModelConfig { id: model.clone(), ..Default::default() });
            if let Some(c) = context {
                m.context_tokens = *c;
            }
            if let Some(o) = max_output {
                m.max_output_tokens = *o;
            }
            if let Some(t) = thinking {
                m.thinking.format = serde_json::from_value(json!(t)).with_context(|| format!("思考の送り方「{t}」はありません"))?;
            }
            p.models.retain(|x| x.id != *model);
            p.models.push(m);
            save_providers(paths, &list)?;
            println!("{} {pid}|{model} を足しました", green("✓"));
        }
        ProvidersCmd::Models { id } => {
            let list = user_providers(paths)?;
            let p = list.iter().find(|p| p.id == *id).with_context(|| format!("{id} はありません"))?;
            let store = SecretStore::load(paths.secrets_file());
            let key = if p.api_key_env.is_empty() {
                store.value(&p.id)
            } else {
                std::env::var(&p.api_key_env).ok().or_else(|| store.value(&p.id))
            };
            let models = fetch_models(p, key).await?;
            for (id, ctx, out) in models {
                let limits = match (ctx, out) {
                    (None, None) => String::new(),
                    (c, o) => {
                        format!("文脈 {} · 出力 {}", c.map_or("?".into(), |x| x.to_string()), o.map_or("?".into(), |x| x.to_string()))
                    }
                };
                println!("{id} {}", faint(&limits));
            }
        }
    }
    Ok(())
}

/// The provider's `/models`: id, context, largest reply when it says them.
async fn fetch_models(p: &ProviderConfig, key: Option<String>) -> Result<Vec<(String, Option<u64>, Option<u64>)>> {
    let client = arce::llm::http_client(Duration::from_secs(15));
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let mut req = client.get(&url).timeout(Duration::from_secs(30));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.context("接続できませんでした")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let (_, why) = provider::classify(Some(status.as_u16()), &text);
        bail!("{why}（HTTP {}）", status.as_u16());
    }
    let v: Value = serde_json::from_str(&text).context("一覧が JSON ではありません")?;
    let items = v.get("data").or_else(|| v.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
    let n = |m: &Value, keys: &[&str]| keys.iter().find_map(|k| m.get(*k).and_then(Value::as_u64));
    let mut out: Vec<(String, Option<u64>, Option<u64>)> = items
        .iter()
        .filter_map(|m| {
            let id = m.get("id").or_else(|| m.get("name")).and_then(Value::as_str)?.trim_start_matches("models/").to_string();
            let ctx = n(m, &["context_length", "context_window", "max_context_length", "max_model_len", "inputTokenLimit"]);
            let out = m
                .get("top_provider")
                .and_then(|t| t.get("max_completion_tokens"))
                .and_then(Value::as_u64)
                .or_else(|| n(m, &["max_completion_tokens", "outputTokenLimit"]));
            Some((id, ctx, out))
        })
        .collect();
    out.sort();
    Ok(out)
}

fn secrets_cmd(action: &SecretsCmd, paths: &Paths) -> Result<()> {
    let mut store = SecretStore::load(paths.secrets_file());
    match action {
        SecretsCmd::Set { id, host } => {
            let id = id.to_lowercase();
            if !arce::secrets::valid_id(&id) {
                bail!("id は小文字の英数字と - _ . にしてください");
            }
            let value = if std::io::stdin().is_terminal() {
                rpassword::prompt_password(format!("{id} の値（画面には出ません）: "))?
            } else {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            };
            let value = value.trim().to_string();
            if value.is_empty() {
                bail!("値が空です");
            }
            let mut entry = store.items.remove(&id).unwrap_or_default();
            entry.value = value;
            for h in host {
                if !entry.hosts.contains(h) {
                    entry.hosts.push(h.clone());
                }
            }
            store.items.insert(id.clone(), entry);
            store.save()?;
            println!("{} {id} を保存しました（{}）", green("✓"), faint(&paths.secrets_file().display().to_string()));
        }
        SecretsCmd::List => {
            if store.items.is_empty() {
                println!("{}", muted("秘密はありません"));
            }
            for (id, s) in &store.items {
                let hosts = if s.hosts.is_empty() {
                    "（ホスト未指定：プロバイダのキーならその接続先、それ以外は http_request で使えません）".to_string()
                } else {
                    s.hosts.join(", ")
                };
                println!("{} {} {}", accent(&pad(id, 16)), if s.value.is_empty() { red("値なし") } else { green("値あり") }, faint(&hosts));
            }
        }
        SecretsCmd::Rm { id } => {
            if store.items.remove(id).is_none() {
                bail!("{id} はありません");
            }
            store.save()?;
            println!("{} {id} を消しました", green("✓"));
        }
    }
    Ok(())
}

async fn doctor(paths: &Paths, cwd: &std::path::Path, ping: bool) -> Result<()> {
    let ok = |s: &str| println!("{} {s}", green("✓"));
    let warn = |s: &str| println!("{} {s}", arce::theme::amber("▲"));
    println!("{} {}", muted("設定"), paths.settings_file().display());
    let loaded = match settings::load(paths, cwd) {
        Ok(l) => {
            ok("設定を読めました");
            l
        }
        Err(e) => {
            println!("{} {e:#}", red("✕"));
            return Ok(());
        }
    };
    for w in &loaded.warnings {
        warn(w);
    }
    let root = settings::project_root(cwd);
    println!("{} {}（{}）", muted("プロジェクト"), root.display(), if loaded.trusted { "信頼済み" } else { "未信頼" });
    let store = SecretStore::load(paths.secrets_file());
    #[cfg(unix)]
    if let Ok(md) = std::fs::metadata(paths.secrets_file()) {
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o077 != 0 {
            warn("secrets.json が所有者以外にも読めます（chmod 600 にしてください）");
        } else {
            ok("secrets.json は所有者だけが読めます");
        }
    }
    println!("{} {}", muted("モデル"), loaded.settings.model);
    for p in &loaded.settings.providers {
        let key = if !p.api_key_env.is_empty() { std::env::var(&p.api_key_env).ok() } else { None }.or_else(|| store.value(&p.id));
        if key.is_some() {
            ok(&format!("{}: キーあり", p.id))
        } else {
            warn(&format!("{}: キーなし（arce secrets set {}）", p.id, p.id))
        }
        if ping {
            match fetch_models(p, key).await {
                Ok(list) => ok(&format!("{}: /models に {} 件", p.id, list.len())),
                Err(e) => warn(&format!("{}: {e:#}", p.id)),
            }
        }
    }
    for s in &loaded.settings.mcp.servers {
        println!("{} MCP {}: {} {}", muted("·"), s.id, s.command, s.args.join(" "));
    }
    let sessions = session::list(&paths.sessions_dir());
    println!("{} {} 件（{}）", muted("セッション"), sessions.len(), paths.sessions_dir().display());
    Ok(())
}

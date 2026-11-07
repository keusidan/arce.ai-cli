//! Settings: everything except secrets is a setting.
//!
//! The user's file is `~/.config/arce/settings.toml`; a project may add `.arce/settings.toml`. A
//! project's file can change only what cannot send anything anywhere until the folder is trusted
//! (`arce trust`): providers, fallback rules, permissions, hooks, MCP servers, search and HTTP settings
//! of an untrusted folder are ignored, because a cloned repository is not the user.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::{FallbackRule, ProviderConfig};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// read freely; ask before writing files, running commands, sending requests
    #[default]
    Ask,
    /// write files in the workspace without asking; commands still ask
    AutoEdit,
    /// read only: nothing is written or run (allow-rules still apply to commands)
    Plan,
    /// nothing asks (deny rules still hold)
    Yolo,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<PermissionMode> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "ask" | "default" => Some(Self::Ask),
            "auto-edit" | "autoedit" | "edit" => Some(Self::AutoEdit),
            "plan" => Some(Self::Plan),
            "yolo" | "auto" | "full" => Some(Self::Yolo),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoEdit => "auto-edit",
            Self::Plan => "plan",
            Self::Yolo => "yolo",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "確認する",
            Self::AutoEdit => "編集は自動",
            Self::Plan => "計画だけ（読み取り専用）",
            Self::Yolo => "すべて許可",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowThinking {
    #[default]
    Full,
    /// a one-line headline and the time only
    Summary,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpConfirm {
    /// every request asks
    #[default]
    Always,
    /// only requests that carry a secret ask
    WithSecrets,
    Never,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Limits {
    pub max_tool_rounds: u32,
    /// the same tool with the same arguments this many times in a row stops the reply
    pub repeat_limit: u32,
    /// unusable tool calls and empty replies are retried this many times
    pub retry_limit: u32,
    pub connect_timeout_secs: u64,
    /// no byte from the provider for this long ends the request
    pub idle_timeout_secs: u64,
    pub command_timeout_secs: u64,
    pub command_max_timeout_secs: u64,
    pub command_max_output_chars: usize,
    pub read_max_chars: usize,
    pub read_default_lines: usize,
    pub grep_max_results: usize,
    pub glob_max_results: usize,
    pub attach_max_chars: usize,
    pub mcp_timeout_secs: u64,
    pub hook_timeout_secs: u64,
    pub default_max_tokens: u32,
    /// how many tool calls of one reply run at the same time when all of them only read
    pub parallel_reads: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_tool_rounds: 30,
            repeat_limit: 3,
            retry_limit: 2,
            connect_timeout_secs: 30,
            idle_timeout_secs: 300,
            command_timeout_secs: 120,
            command_max_timeout_secs: 1800,
            command_max_output_chars: 30_000,
            read_max_chars: 100_000,
            read_default_lines: 2000,
            grep_max_results: 200,
            glob_max_results: 500,
            attach_max_chars: 200_000,
            mcp_timeout_secs: 60,
            hook_timeout_secs: 30,
            default_max_tokens: 8192,
            parallel_reads: 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// None: the model's own setting, then its largest reply (when `use_model_max`), then the default
    pub max_tokens: Option<u32>,
    pub use_model_max: bool,
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling { temperature: None, top_p: None, max_tokens: None, use_model_max: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub at_percent: u32,
    pub keep_turns: usize,
    pub summary_chars: usize,
    pub tool_result_chars: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings { enabled: true, at_percent: 80, keep_turns: 4, summary_chars: 3000, tool_result_chars: 1500 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TokenPrecheck {
    pub enabled: bool,
    /// instructions and tool definitions may take at most this share of the context (percent)
    pub overhead_percent: u32,
}

impl Default for TokenPrecheck {
    fn default() -> Self {
        TokenPrecheck { enabled: true, overhead_percent: 80 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CodingSettings {
    pub enabled: bool,
    /// the planner is the model in use; off: `planner`
    pub planner_follows_model: bool,
    pub planner: String,
    pub developers: Vec<String>,
    pub max_tasks: usize,
    pub parallel: usize,
    pub review_rounds: u32,
    /// 0: the model's own setting
    pub developer_max_tokens: u32,
    pub developer_temperature: f64,
    pub result_chars: usize,
    /// write the files the developers return into the workspace (through the usual permission)
    pub apply_files: bool,
}

impl Default for CodingSettings {
    fn default() -> Self {
        CodingSettings {
            enabled: false,
            planner_follows_model: true,
            planner: String::new(),
            developers: vec![],
            max_tasks: 8,
            parallel: 4,
            review_rounds: 1,
            developer_max_tokens: 0,
            developer_temperature: 0.2,
            result_chars: 12_000,
            apply_files: true,
        }
    }
}

impl CodingSettings {
    pub fn developer_refs(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for d in &self.developers {
            let d = d.trim().to_string();
            if !d.is_empty() && !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }
    pub fn is_ready(&self) -> bool {
        self.enabled && !self.developer_refs().is_empty() && (self.planner_follows_model || !self.planner.is_empty())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Permissions {
    /// `tool` or `tool(pattern)`; the pattern is a glob over the command, the path or the host
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    /// after a reply read something from outside (web, HTTP, MCP), commands ask even when allowed
    pub confirm_after_external: bool,
    pub http_confirm: HttpConfirm,
    pub http_allow_insecure: bool,
    pub http_max_redirects: u32,
    pub http_timeout_secs: u64,
    pub http_max_response_chars: usize,
    /// folders besides the working directory that tools may use without asking
    pub additional_dirs: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions {
            allow: vec![],
            ask: vec![],
            deny: vec![],
            confirm_after_external: true,
            http_confirm: HttpConfirm::Always,
            http_allow_insecure: false,
            http_max_redirects: 5,
            http_timeout_secs: 30,
            http_max_response_chars: 20_000,
            additional_dirs: vec![],
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hook {
    /// `pre_tool`, `post_tool`, `prompt`, `stop`
    pub event: String,
    /// a glob over the tool name (empty: every tool)
    pub matcher: String,
    pub command: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServer {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    /// values may use `{{secret:ID}}`
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    /// tools that never ask (names without the `mcp_<id>__` prefix)
    pub trusted_tools: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpSettings {
    pub servers: Vec<McpServer>,
    /// with more tools than this, the model gets `mcp_find` / `mcp_describe` / `mcp_call` instead
    pub lazy_threshold: usize,
}

impl Default for McpSettings {
    fn default() -> Self {
        McpSettings { servers: vec![], lazy_threshold: 40 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SearchSettings {
    pub enabled: bool,
    /// a SearXNG instance (`https://…`); empty: the instant answers of DuckDuckGo
    pub searxng_url: String,
    pub max_results: usize,
}

impl Default for SearchSettings {
    fn default() -> Self {
        SearchSettings { enabled: true, searxng_url: String::new(), max_results: 6 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiSettings {
    /// `auto`, `dark`, `light`, `none`
    pub theme: String,
    pub markdown: bool,
    pub show_thinking: ShowThinking,
    /// the line with requests, tools, time and tokens after each reply
    pub run_summary: bool,
    /// code blocks longer than this many lines show only the head (0: always whole)
    pub code_fold_lines: usize,
    /// the token line before sending (`/context` shows it at any time)
    pub token_line: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            theme: "auto".into(),
            markdown: true,
            show_thinking: ShowThinking::Full,
            run_summary: true,
            code_fold_lines: 0,
            token_line: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// `provider|model` in use
    pub model: String,
    pub mode: PermissionMode,
    /// `auto` (chosen per message) or an agent id
    pub agent: String,
    pub thinking: bool,
    /// speed (0) to accuracy (1)
    pub effort: f64,
    pub tone: String,
    pub extra_prompt: String,
    /// project instruction files read from the repository root down to the working directory
    pub instruction_files: Vec<String>,
    pub limits: Limits,
    pub online: Sampling,
    pub compaction: CompactionSettings,
    pub token_precheck: TokenPrecheck,
    pub coding: CodingSettings,
    pub fallback_enabled: bool,
    pub fallback: Vec<FallbackRule>,
    pub permissions: Permissions,
    pub hooks: Vec<Hook>,
    pub mcp: McpSettings,
    pub search: SearchSettings,
    pub ui: UiSettings,
    pub providers: Vec<ProviderConfig>,
    /// hosts each stored secret may be sent to, by id (values are in the secrets file)
    pub secret_hosts: BTreeMap<String, Vec<String>>,
    /// secrets the model may not use (by id)
    pub secrets_off_for_model: Vec<String>,
    /// the title of a session: the first words of its first message, at most this many characters
    pub title_max_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model: "demo|arce-demo".into(),
            mode: PermissionMode::Ask,
            agent: "auto".into(),
            thinking: true,
            effort: 0.6,
            tone: String::new(),
            extra_prompt: String::new(),
            instruction_files: vec!["AGENTS.md".into(), "ARCE.md".into()],
            limits: Limits::default(),
            online: Sampling::default(),
            compaction: CompactionSettings::default(),
            token_precheck: TokenPrecheck::default(),
            coding: CodingSettings::default(),
            fallback_enabled: true,
            fallback: vec![],
            permissions: Permissions::default(),
            hooks: vec![],
            mcp: McpSettings::default(),
            search: SearchSettings::default(),
            ui: UiSettings::default(),
            providers: vec![],
            secret_hosts: BTreeMap::new(),
            secrets_off_for_model: vec![],
            title_max_chars: 40,
        }
    }
}

/// Top-level keys a project's file may not set until its folder is trusted.
pub const UNTRUSTED_BLOCKED: [&str; 9] =
    ["providers", "fallback", "fallback_enabled", "permissions", "hooks", "mcp", "search", "secret_hosts", "secrets_off_for_model"];

/// Where the CLI keeps its files. `ARCE_HOME` puts everything under one folder (tests, portable use).
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Paths {
        if let Some(home) = std::env::var_os("ARCE_HOME").filter(|v| !v.is_empty()) {
            let home = PathBuf::from(home);
            return Paths { config_dir: home.join("config"), data_dir: home.join("data") };
        }
        let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("arce");
        let data = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("arce");
        Paths { config_dir: config, data_dir: data }
    }
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.data_dir.join("secrets.json")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    pub fn history_file(&self) -> PathBuf {
        self.data_dir.join("history.txt")
    }
    pub fn trusted_file(&self) -> PathBuf {
        self.data_dir.join("trusted.json")
    }
    pub fn global_instructions(&self) -> PathBuf {
        self.config_dir.join("AGENTS.md")
    }
    pub fn commands_dir(&self) -> PathBuf {
        self.config_dir.join("commands")
    }
}

/// The settings as loaded, with what was refused from the project's file.
pub struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
    pub project_file: Option<PathBuf>,
    pub trusted: bool,
}

pub fn read_toml(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let t: toml::Value = toml::from_str(&text).with_context(|| format!("{} を読めません", path.display()))?;
            Ok(Some(serde_json::to_value(t)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{} を読めません", path.display())),
    }
}

/// [over] laid on [base]: objects are merged key by key, anything else is replaced.
pub fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// The project folder: the nearest folder upwards with `.arce/` or `.git`, else [cwd].
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".arce").is_dir() || d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

pub fn trusted_dirs(paths: &Paths) -> Vec<PathBuf> {
    std::fs::read_to_string(paths.trusted_file()).ok().and_then(|t| serde_json::from_str::<Vec<PathBuf>>(&t).ok()).unwrap_or_default()
}

pub fn is_trusted(paths: &Paths, root: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    trusted_dirs(paths).iter().any(|t| root.starts_with(t))
}

pub fn trust(paths: &Paths, root: &Path, on: bool) -> Result<()> {
    let root = root.canonicalize()?;
    let mut list = trusted_dirs(paths);
    list.retain(|p| p != &root);
    if on {
        list.push(root);
    }
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::write(paths.trusted_file(), serde_json::to_string_pretty(&list)?)?;
    Ok(())
}

pub fn load(paths: &Paths, cwd: &Path) -> Result<Loaded> {
    let mut value = serde_json::to_value(Settings::default())?;
    let mut warnings = vec![];
    if let Some(user) = read_toml(&paths.settings_file())? {
        merge(&mut value, user);
    }
    let root = project_root(cwd);
    let trusted = is_trusted(paths, &root);
    let project_path = root.join(".arce").join("settings.toml");
    let mut project_file = None;
    if let Some(mut project) = read_toml(&project_path)? {
        project_file = Some(project_path.clone());
        if !trusted && let Value::Object(map) = &mut project {
            let refused: Vec<String> = UNTRUSTED_BLOCKED.iter().filter(|k| map.contains_key(**k)).map(|k| k.to_string()).collect();
            for k in &refused {
                map.remove(k);
            }
            if !refused.is_empty() {
                warnings.push(format!(
                    "このフォルダは信頼されていないため、{} の {} は読みませんでした（`arce trust` で信頼できます）",
                    project_path.display(),
                    refused.join("・")
                ));
            }
        }
        merge(&mut value, project);
    }
    let mut settings: Settings = serde_json::from_value(value).context("設定の値の型が合いません")?;
    for p in &settings.providers {
        warnings.extend(p.problems());
    }
    settings.providers.retain(|p| p.problems().is_empty());
    for p in &settings.providers {
        if crate::provider::sends_secrets_in_the_clear(&p.base_url) {
            warnings.push(format!("{}: このアドレスは暗号化されません（キーが読まれるおそれがあります）", p.id));
        }
    }
    Ok(Loaded { settings, warnings, project_file, trusted })
}

/// [settings] with [key] (dotted: `limits.max_tool_rounds`) set to [raw]: JSON when it reads as JSON,
/// else a string. The settings type decides whether the value fits.
pub fn with_value(settings: &Settings, key: &str, raw: &str) -> Result<Settings> {
    let parsed: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    with_json(settings, key, parsed)
}

pub fn with_json(settings: &Settings, key: &str, value: Value) -> Result<Settings> {
    let mut root = serde_json::to_value(settings)?;
    let parts: Vec<&str> = key.split('.').collect();
    let mut slot = &mut root;
    for (i, part) in parts.iter().enumerate() {
        let Value::Object(map) = slot else { bail!("{key} は設定の名前ではありません") };
        if !map.contains_key(*part) {
            bail!("{key} は設定の名前ではありません");
        }
        if i == parts.len() - 1 {
            map.insert(part.to_string(), value.clone());
            break;
        }
        slot = map.get_mut(*part).unwrap();
    }
    serde_json::from_value(root).with_context(|| format!("{key} の値の形が合いません"))
}

pub fn get_value(settings: &Settings, key: &str) -> Option<Value> {
    let root = serde_json::to_value(settings).ok()?;
    let mut slot = &root;
    for part in key.split('.') {
        slot = slot.get(part)?;
    }
    Some(slot.clone())
}

/// Writes one key into the user's file (or the project's) without touching the rest of it.
pub fn persist(path: &Path, key: &str, value: Value) -> Result<()> {
    let mut doc: Value = read_toml(path)?.unwrap_or(Value::Object(Default::default()));
    let parts: Vec<&str> = key.split('.').collect();
    let mut slot = &mut doc;
    for (i, part) in parts.iter().enumerate() {
        if !slot.is_object() {
            *slot = Value::Object(Default::default());
        }
        let map = slot.as_object_mut().unwrap();
        if i == parts.len() - 1 {
            map.insert(part.to_string(), value.clone());
            break;
        }
        slot = map.entry(part.to_string()).or_insert_with(|| Value::Object(Default::default()));
    }
    // check that the whole file still reads
    let mut check = serde_json::to_value(Settings::default())?;
    merge(&mut check, doc.clone());
    serde_json::from_value::<Settings>(check).with_context(|| format!("{key} の値の形が合いません"))?;
    let toml_value: toml::Value = serde_json::from_value(strip_nulls(doc))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, toml::to_string_pretty(&toml_value)?)?;
    Ok(())
}

/// TOML has no null: an unset optional value is left out.
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().filter(|v| !v.is_null()).map(strip_nulls).collect()),
        other => other,
    }
}

/// The settings a model may propose to change with `update_settings` (always shown and confirmed
/// first), with a label in Japanese.
pub const CHAT_CHANGEABLE: &[(&str, &str)] = &[
    ("model", "使うモデル（provider|model）"),
    ("agent", "エージェント（auto か id）"),
    ("thinking", "思考する"),
    ("effort", "速度⇔精度（0〜1）"),
    ("tone", "口調"),
    ("extra_prompt", "追加の指示"),
    ("limits.max_tool_rounds", "ツールを呼ぶ回数の上限"),
    ("limits.repeat_limit", "同じ呼び出しの繰り返しで止める回数"),
    ("limits.retry_limit", "書き直しの回数"),
    ("limits.command_timeout_secs", "コマンドの既定の待ち時間（秒）"),
    ("limits.command_max_output_chars", "コマンドの出力の最大文字数"),
    ("limits.read_max_chars", "ファイルを読む最大文字数"),
    ("limits.grep_max_results", "検索の最大件数"),
    ("limits.default_max_tokens", "返信の長さの既定（トークン）"),
    ("online.temperature", "temperature"),
    ("online.top_p", "top_p"),
    ("online.max_tokens", "返信の最大トークン"),
    ("online.use_model_max", "モデルの最大出力を使う"),
    ("compaction.enabled", "会話の自動要約"),
    ("compaction.at_percent", "要約を始める文脈の割合（%）"),
    ("compaction.keep_turns", "要約せずに残す発言の数"),
    ("compaction.summary_chars", "要約の長さ（文字）"),
    ("compaction.tool_result_chars", "古い道具の結果を切り詰める長さ"),
    ("token_precheck.enabled", "送る前のトークンの確認"),
    ("token_precheck.overhead_percent", "指示と道具が文脈に占めてよい割合（%）"),
    ("coding.enabled", "コーディングモード（分担）"),
    ("coding.max_tasks", "1 回に渡すタスクの最大数"),
    ("coding.parallel", "1 つの開発モデルが同時に走らせる数"),
    ("coding.review_rounds", "書き直しを頼める回数"),
    ("coding.result_chars", "プランナーが読む結果の長さ"),
    ("search.enabled", "Web 検索"),
    ("search.max_results", "検索結果の件数"),
    ("ui.show_thinking", "思考の表示（full / summary / hidden）"),
    ("ui.markdown", "Markdown を整えて表示"),
    ("ui.run_summary", "返信のあとの集計の行"),
    ("ui.code_fold_lines", "長いコードを畳む行数"),
    ("ui.token_line", "送る前のトークン数の行"),
    ("title_max_chars", "セッションの題の長さ"),
];

/// Why a key cannot be changed from the chat; None when it can.
pub fn blocked_reason(key: &str) -> Option<&'static str> {
    if CHAT_CHANGEABLE.iter().any(|(k, _)| *k == key) {
        return None;
    }
    let top = key.split('.').next().unwrap_or(key);
    Some(match top {
        "providers" | "fallback" | "fallback_enabled" => {
            "送り先を変える設定は、会話からは変えられません（会話の内容やキーが別の場所へ送られるため）"
        }
        "permissions" | "mode" => "承認と権限は、会話からは変えられません（入り込んだ指示で確認を外されないため）",
        "hooks" | "mcp" => "外部のコマンドを動かす設定は、会話からは変えられません",
        "secret_hosts" | "secrets_off_for_model" => "秘密の扱いは、会話からは変えられません",
        "coding" => "開発モデルの選び方は、会話からは変えられません（送り先が変わるため）",
        "search" => "検索の送り先は、会話からは変えられません",
        _ => "この設定は、会話からは変えられません",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_value_types_through_the_settings() {
        let s = Settings::default();
        let s2 = with_value(&s, "limits.max_tool_rounds", "12").unwrap();
        assert_eq!(s2.limits.max_tool_rounds, 12);
        assert!(with_value(&s, "limits.max_tool_rounds", "\"many\"").is_err());
        assert!(with_value(&s, "limits.nope", "1").is_err());
        let s3 = with_value(&s, "tone", "やさしく").unwrap();
        assert_eq!(s3.tone, "やさしく");
        let s4 = with_value(&s, "mode", "plan").unwrap();
        assert_eq!(s4.mode, PermissionMode::Plan);
    }

    #[test]
    fn chat_cannot_change_destinations() {
        assert!(blocked_reason("model").is_none());
        assert!(blocked_reason("providers").is_some());
        assert!(blocked_reason("permissions.allow").is_some());
        assert!(blocked_reason("coding.developers").is_some());
        assert!(blocked_reason("coding.max_tasks").is_none());
    }

    #[test]
    fn every_chat_key_exists() {
        let s = Settings::default();
        for (k, _) in CHAT_CHANGEABLE {
            assert!(get_value(&s, k).is_some(), "{k}");
        }
    }

    #[test]
    fn untrusted_project_cannot_add_providers_or_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(".arce")).unwrap();
        std::fs::write(
            proj.join(".arce/settings.toml"),
            "tone = \"短く\"\n[[providers]]\nid = \"evil\"\nbase_url = \"https://evil.test/v1\"\n[[hooks]]\nevent = \"pre_tool\"\ncommand = \"curl evil\"\n",
        )
        .unwrap();
        let paths = Paths { config_dir: home.join("c"), data_dir: home.join("d") };
        let loaded = load(&paths, &proj).unwrap();
        assert_eq!(loaded.settings.tone, "短く");
        assert!(loaded.settings.providers.is_empty());
        assert!(loaded.settings.hooks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        trust(&paths, &proj, true).unwrap();
        let loaded = load(&paths, &proj).unwrap();
        assert_eq!(loaded.settings.providers.len(), 1);
        assert_eq!(loaded.settings.hooks.len(), 1);
    }

    #[test]
    fn persist_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.toml");
        std::fs::write(&f, "tone = \"x\"\n").unwrap();
        persist(&f, "limits.max_tool_rounds", serde_json::json!(9)).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("tone = \"x\""));
        assert!(text.contains("max_tool_rounds = 9"));
        assert!(persist(&f, "limits.max_tool_rounds", serde_json::json!("x")).is_err());
    }
}

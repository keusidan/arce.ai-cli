//! Providers (OpenAI-compatible endpoints), their models, how thinking is asked for, which failures
//! switch to another model, and what a failure means for the user.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// `provider|model`.
pub fn parse_ref(r: &str) -> Option<(&str, &str)> {
    let (p, m) = r.split_once('|')?;
    (!p.is_empty()).then_some((p, m))
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    /// function calling when the provider supports it (the default), else the text form
    #[default]
    Auto,
    Native,
    /// `<tool_call>{json}</tool_call>` in the text; for servers without function calling
    Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// The environment variable that holds the key; empty: `ARCE_SECRET_<ID>` or the secrets file.
    pub api_key_env: String,
    pub models: Vec<ModelConfig>,
    /// Extra headers (never a key: names that look like credentials are refused on load).
    pub headers: std::collections::BTreeMap<String, String>,
    /// Extra JSON fields merged into every request body (they win over generated ones).
    pub extra_body: Map<String, Value>,
    pub tool_mode: ToolMode,
    /// Whether the provider takes `tools` at all (false: the text form is used).
    pub supports_tools: bool,
    /// Send the same per-session cache key with every request of a conversation.
    pub prompt_cache: bool,
    pub prompt_cache_field: String,
    /// Ask for the usage at the end of the stream (`stream_options.include_usage`).
    pub stream_usage: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            models: vec![],
            headers: Default::default(),
            extra_body: Map::new(),
            tool_mode: ToolMode::Auto,
            supports_tools: true,
            prompt_cache: false,
            prompt_cache_field: "prompt_cache_key".into(),
            stream_usage: true,
        }
    }
}

impl ProviderConfig {
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }

    pub fn model(&self, id: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn uses_native_tools(&self) -> bool {
        match self.tool_mode {
            ToolMode::Native => true,
            ToolMode::Text => false,
            ToolMode::Auto => self.supports_tools,
        }
    }

    /// The header names that could carry a credential; they are not accepted in `headers`.
    pub fn sensitive_header(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        n == "authorization"
            || n == "proxy-authorization"
            || n == "cookie"
            || n.contains("api-key")
            || n.contains("api_key")
            || n.contains("apikey")
            || n.contains("token")
            || n.contains("secret")
    }

    /// Problems of the configuration that make it unusable or unsafe, in Japanese.
    pub fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        if self.id.is_empty() || !crate::secrets::valid_id(&self.id) {
            out.push(format!("プロバイダの id「{}」は使えません（小文字の英数字と - _ .）", self.id));
        }
        if reqwest::Url::parse(&self.base_url).is_err() {
            out.push(format!("{}: base_url が URL ではありません", self.id));
        }
        if let Ok(u) = reqwest::Url::parse(&self.base_url)
            && (!u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some())
        {
            out.push(format!("{}: base_url にユーザー名・?・# を書かないでください（キーは秘密として別に入れます）", self.id));
        }
        for name in self.headers.keys() {
            if Self::sensitive_header(name) {
                out.push(format!("{}: ヘッダー {name} には秘密が入りうるので、headers には書けません", self.id));
            }
        }
        const RESERVED: [&str; 4] = ["model", "messages", "tools", "stream"];
        if self.prompt_cache
            && (RESERVED.contains(&self.prompt_cache_field.as_str())
                || !self.prompt_cache_field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            out.push(format!("{}: prompt_cache_field「{}」は使えません", self.id, self.prompt_cache_field));
        }
        out
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    /// Context window in tokens (0: unknown).
    pub context_tokens: u32,
    /// Longest reply the model can write (0: unknown).
    pub max_output_tokens: u32,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub prompt: String,
    pub thinking: ThinkingConfig,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    /// nothing is sent; the provider's default decides
    #[default]
    None,
    Deepseek,
    Zai,
    ChatTemplate,
    Openai,
    Openrouter,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSwitch {
    /// follows the session's thinking switch and effort
    #[default]
    Auto,
    On,
    Off,
}

/// How thinking is asked of one model.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ThinkingConfig {
    pub format: ThinkingFormat,
    pub switch: ThinkingSwitch,
    /// `low`, `medium`, `high`, `max`, or empty (from the effort slider).
    pub effort: String,
    /// GLM's `clear_thinking`.
    pub clear_thinking: Option<bool>,
}

impl ThinkingConfig {
    /// The effort word for [level] (0..1) when none was written: below .5 low, below .85 high, else max.
    pub fn effort_for(&self, level: f64) -> String {
        if !self.effort.trim().is_empty() {
            return self.effort.trim().to_lowercase();
        }
        if level < 0.5 {
            "low".into()
        } else if level < 0.85 {
            "high".into()
        } else {
            "max".into()
        }
    }

    /// Whether thinking is on for a request, given the session switch.
    pub fn is_on(&self, session_on: bool, level: f64) -> bool {
        match self.switch {
            ThinkingSwitch::On => true,
            ThinkingSwitch::Off => false,
            ThinkingSwitch::Auto => session_on && level >= 0.25,
        }
    }

    /// The fields to add to the request body.
    pub fn body_fields(&self, session_on: bool, level: f64) -> Map<String, Value> {
        let on = self.is_on(session_on, level);
        let effort = self.effort_for(level);
        let mut m = Map::new();
        let kind = if on { "enabled" } else { "disabled" };
        match self.format {
            ThinkingFormat::None => {}
            ThinkingFormat::Deepseek => {
                m.insert("thinking".into(), json!({ "type": kind }));
                if on {
                    m.insert("reasoning_effort".into(), json!(effort));
                }
            }
            ThinkingFormat::Zai => {
                let mut t = Map::new();
                t.insert("type".into(), json!(kind));
                if let Some(c) = self.clear_thinking {
                    t.insert("clear_thinking".into(), json!(c));
                }
                m.insert("thinking".into(), Value::Object(t));
                if on {
                    m.insert("reasoning_effort".into(), json!(effort));
                }
            }
            ThinkingFormat::ChatTemplate => {
                let mut k = Map::new();
                k.insert("thinking".into(), json!(on));
                k.insert("enable_thinking".into(), json!(on));
                if on {
                    k.insert("reasoning_effort".into(), json!(effort));
                }
                if let Some(c) = self.clear_thinking {
                    k.insert("clear_thinking".into(), json!(c));
                }
                m.insert("chat_template_kwargs".into(), Value::Object(k));
            }
            ThinkingFormat::Openai => {
                if on {
                    let e = if effort == "max" { "high".to_string() } else { effort };
                    m.insert("reasoning_effort".into(), json!(e));
                }
            }
            ThinkingFormat::Openrouter => {
                if on {
                    let e = if effort == "max" { "high".to_string() } else { effort };
                    m.insert("reasoning".into(), json!({ "effort": e }));
                } else {
                    m.insert("reasoning".into(), json!({ "enabled": false }));
                }
            }
        }
        m
    }
}

/// "When this model cannot be used, use these instead."
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FallbackRule {
    /// `provider|model`, `provider|*` or `*`.
    pub from: String,
    pub targets: Vec<String>,
    /// HTTP statuses that start the fallback; empty: the default set.
    pub statuses: Vec<u16>,
}

impl FallbackRule {
    pub fn specificity(&self) -> u8 {
        if self.from == "*" {
            0
        } else if self.from.ends_with("|*") {
            1
        } else {
            2
        }
    }

    pub fn covers(&self, provider: &str, model: &str) -> bool {
        if self.from == "*" {
            return true;
        }
        match parse_ref(&self.from) {
            Some((p, m)) => p == provider && (m == "*" || m == model),
            None => false,
        }
    }

    /// Whether a failure with [status] (None: no connection) starts this rule.
    pub fn triggers(&self, status: Option<u16>) -> bool {
        match status {
            None => true,
            Some(s) if self.statuses.is_empty() => DEFAULT_FALLBACK_STATUSES.contains(&s) || s >= 500,
            Some(s) => self.statuses.contains(&s),
        }
    }
}

pub const DEFAULT_FALLBACK_STATUSES: [u16; 5] = [401, 403, 404, 408, 429];

/// The models to try after [provider|model] failed with [status], in order, most specific rule first.
pub fn fallback_targets(rules: &[FallbackRule], provider: &str, model: &str, status: Option<u16>) -> Vec<String> {
    let mut matching: Vec<&FallbackRule> = rules.iter().filter(|r| r.covers(provider, model) && r.triggers(status)).collect();
    matching.sort_by_key(|r| std::cmp::Reverse(r.specificity()));
    let me = format!("{provider}|{model}");
    let mut out: Vec<String> = vec![];
    for r in matching {
        for t in &r.targets {
            if parse_ref(t).is_some() && *t != me && !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Usable,
    Blocked,
    Temporary,
    KeyRejected,
    Unknown,
}

/// What a failed request says about the model, with a short reason in Japanese.
pub fn classify(status: Option<u16>, body: &str) -> (Availability, String) {
    use Availability::*;
    let Some(status) = status else { return (Temporary, "接続できませんでした".into()) };
    if (200..300).contains(&status) {
        return (Usable, "使えます".into());
    }
    let text = body.to_lowercase();
    let has = |p: &str| text.contains(p);
    let re = |p: &str| regex::Regex::new(p).map(|r| r.is_match(&text)).unwrap_or(false);
    let key_words = re(
        r"api[ _-]?key (is )?(not valid|invalid|missing|expired)|api_key_invalid|invalid[ _-]?api[ _-]?key|incorrect api key|invalid_api_key|authentication|unauthenticated|unauthorized|invalid (bearer )?token|no auth",
    );
    if status == 401 || ((status == 400 || status == 403) && key_words) {
        return (KeyRejected, "API キーが受け付けられませんでした".into());
    }
    let free_tier = has("free tier") || has("free_tier") || has("free plan");
    let plan_words = re(
        r"billing|paid plan|paid tier|upgrade|payment|credit|insufficient[ _]funds|insufficient_quota|not enabled|isn.t enabled|requires? (a )?(paid|pro|premium)",
    );
    let model_words = re(r"model|models/");
    match status {
        402 => (Blocked, "残高（クレジット）が足りません".into()),
        429 => {
            if re(r#"limit:\s*0\b|"limit":\s*0\b|limit of 0\b"#) {
                (Blocked, "このキー（無料枠）では、このモデルの利用枠が 0 です".into())
            } else if has("insufficient_quota") || re(r"insufficient (credits|balance|funds)") {
                (Blocked, "利用枠（クレジット）を使い切っています。プランや請求を確認してください".into())
            } else if free_tier && re(r"not (available|supported|allowed)|isn.t available|only available|requires? (a )?paid") {
                (Blocked, "無料枠では使えないモデルです".into())
            } else if re(r"per[ _-]?day|daily") {
                (Temporary, "今日の分の回数制限に達しました（日が変わると戻ります）".into())
            } else {
                (Temporary, "回数制限です（少し待てば使えます）".into())
            }
        }
        403 => {
            if plan_words || free_tier || re(r"permission|not allowed|forbidden|access|not authorized|denied|region|country|territory") {
                let why = if free_tier {
                    "無料枠では使えないモデルです"
                } else if plan_words {
                    "このキー（プラン）では使えません（請求や有効化が必要です）"
                } else {
                    "このキーには、このモデルを使う権限がありません"
                };
                (Blocked, why.into())
            } else {
                (Unknown, "拒否されました（HTTP 403）".into())
            }
        }
        404 => {
            if model_words || has("no endpoints") {
                (Blocked, "このモデルは、この提供元・このキーにはありません（またはもう使えません）".into())
            } else {
                (Unknown, "接続先が見つかりません（URL を確認してください）".into())
            }
        }
        400 | 422 => {
            if re(
                r"(model|models/[\w.\-:]+)[^.]{0,80}(not found|does not exist|not supported|unsupported|is not available|isn.t available|no longer available|deprecated|decommissioned|not enabled|not a valid model)",
            ) || re(r"(invalid|unknown|unsupported) model")
            {
                (Blocked, "このモデルは使えません（提供元が受け付けませんでした）".into())
            } else if free_tier && re(r"not (available|supported|allowed)") {
                (Blocked, "無料枠では使えないモデルです".into())
            } else {
                (Unknown, format!("リクエストの形が、このモデルに合いませんでした（HTTP {status}）"))
            }
        }
        408 => (Temporary, "時間切れです".into()),
        s if s >= 500 => (Temporary, format!("提供元の側で失敗しました（HTTP {s}）。あとで試してください")),
        s => (Unknown, format!("失敗しました（HTTP {s}）")),
    }
}

/// The provider's own words from an error body (`error.message`, also inside a list), cut to [max].
pub fn provider_message(body: &str, max: usize) -> Option<String> {
    let mut v: Value = serde_json::from_str(body).ok()?;
    if let Value::Array(a) = &v
        && let Some(first) = a.first()
    {
        v = first.clone();
    }
    let text = match v.get("error") {
        Some(Value::Object(e)) => e.get("message").and_then(Value::as_str),
        Some(Value::String(s)) => Some(s.as_str()),
        _ => v.get("message").and_then(Value::as_str),
    }?;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    Some(if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) })
}

/// Ready-made providers for `arce providers add --preset <id>`.
pub fn presets() -> Vec<ProviderConfig> {
    let p =
        |id: &str, name: &str, url: &str| ProviderConfig { id: id.into(), name: name.into(), base_url: url.into(), ..Default::default() };
    vec![
        p("openai", "OpenAI", "https://api.openai.com/v1"),
        p("openrouter", "OpenRouter", "https://openrouter.ai/api/v1"),
        p("deepseek", "DeepSeek", "https://api.deepseek.com/v1"),
        p("gemini", "Gemini", "https://generativelanguage.googleapis.com/v1beta/openai"),
        p("zai", "Z.ai", "https://api.z.ai/api/paas/v4"),
        p("groq", "Groq", "https://api.groq.com/openai/v1"),
        p("mistral", "Mistral", "https://api.mistral.ai/v1"),
        p("ollama", "Ollama（この端末）", "http://127.0.0.1:11434/v1"),
        p("llamacpp", "llama.cpp server（この端末）", "http://127.0.0.1:8080/v1"),
    ]
}

/// Whether a key sent to [url] would travel in the clear (http to a host outside the house).
pub fn sends_secrets_in_the_clear(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url.trim()) else { return false };
    if u.scheme() != "http" {
        return false;
    }
    let Some(host) = u.host_str() else { return false };
    let h = host.trim_start_matches('[').trim_end_matches(']').to_lowercase();
    if h == "localhost" || h.ends_with(".local") || h.ends_with(".lan") || h.ends_with(".home.arpa") {
        return false;
    }
    !crate::secrets::hosts::is_private(&h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_prefers_specific_rules_and_skips_itself() {
        let rules = vec![
            FallbackRule { from: "*".into(), targets: vec!["c|z".into()], statuses: vec![] },
            FallbackRule { from: "a|*".into(), targets: vec!["b|y".into(), "a|x".into()], statuses: vec![] },
            FallbackRule { from: "a|x".into(), targets: vec!["d|w".into()], statuses: vec![429] },
        ];
        assert_eq!(fallback_targets(&rules, "a", "x", Some(429)), vec!["d|w", "b|y", "c|z"]);
        assert_eq!(fallback_targets(&rules, "a", "x", Some(403)), vec!["b|y", "c|z"]);
        assert!(fallback_targets(&rules, "a", "x", Some(400)).is_empty());
        assert_eq!(fallback_targets(&rules, "a", "x", None).len(), 3);
        assert_eq!(fallback_targets(&rules, "q", "x", Some(503)), vec!["c|z"]);
    }

    #[test]
    fn classify_tells_plan_from_rate_limit() {
        assert_eq!(classify(Some(429), r#"{"error":{"message":"Quota exceeded ... limit: 0"}}"#).0, Availability::Blocked);
        assert_eq!(classify(Some(429), "You exceeded your current quota, please retry").0, Availability::Temporary);
        assert_eq!(classify(Some(429), r#"{"error":{"code":"insufficient_quota"}}"#).0, Availability::Blocked);
        assert_eq!(classify(Some(401), "").0, Availability::KeyRejected);
        assert_eq!(classify(Some(404), "model foo not found").0, Availability::Blocked);
        assert_eq!(classify(Some(404), "").0, Availability::Unknown);
        assert_eq!(classify(Some(503), "").0, Availability::Temporary);
        assert_eq!(classify(None, "").0, Availability::Temporary);
    }

    #[test]
    fn provider_message_reads_lists_and_objects() {
        assert_eq!(provider_message(r#"[{"error":{"message":"bad  thing"}}]"#, 50).unwrap(), "bad thing");
        assert_eq!(provider_message(r#"{"error":"nope"}"#, 50).unwrap(), "nope");
        assert!(provider_message("not json", 50).is_none());
    }

    #[test]
    fn thinking_fields_follow_the_format() {
        let c = ThinkingConfig { format: ThinkingFormat::Deepseek, ..Default::default() };
        let on = c.body_fields(true, 0.9);
        assert_eq!(on["thinking"]["type"], "enabled");
        assert_eq!(on["reasoning_effort"], "max");
        let off = c.body_fields(false, 0.9);
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(!off.contains_key("reasoning_effort"));
        let o = ThinkingConfig { format: ThinkingFormat::Openai, ..Default::default() }.body_fields(true, 0.9);
        assert_eq!(o["reasoning_effort"], "high");
        let r = ThinkingConfig { format: ThinkingFormat::Openrouter, ..Default::default() }.body_fields(false, 0.5);
        assert_eq!(r["reasoning"]["enabled"], false);
        assert!(ThinkingConfig::default().body_fields(true, 1.0).is_empty());
    }

    #[test]
    fn provider_problems_catch_keys_in_the_wrong_place() {
        let mut p = ProviderConfig { id: "x".into(), base_url: "https://u:p@api.test/v1".into(), ..Default::default() };
        p.headers.insert("X-Api-Key".into(), "v".into());
        let probs = p.problems();
        assert_eq!(probs.len(), 2, "{probs:?}");
    }

    #[test]
    fn clear_text_warning_skips_the_house() {
        assert!(sends_secrets_in_the_clear("http://api.example.com/v1"));
        assert!(!sends_secrets_in_the_clear("http://127.0.0.1:8080/v1"));
        assert!(!sends_secrets_in_the_clear("http://nas.local/v1"));
        assert!(!sends_secrets_in_the_clear("https://api.example.com/v1"));
    }
}

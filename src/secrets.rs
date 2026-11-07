//! Keys and tokens: where they live, how a model points at one without seeing it, where one may be
//! sent, and how a value is taken out of anything that goes back to the model or the screen.
//!
//! The values live only in the secrets file (`secrets.json`, mode 0600, never in the settings) or in
//! environment variables. A model writes `{{secret:ID}}`; the CLI puts the value in at the moment of use,
//! only in the arguments of `http_request` and `run_command`, and only towards hosts bound to that
//! secret.

use std::collections::BTreeMap;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};

static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*secret\s*:\s*([A-Za-z0-9][A-Za-z0-9_.\-]{0,63})\s*\}\}").unwrap());

/// `{{secret:NAME}}` in a text a model wrote.
pub mod placeholders {
    use super::*;

    pub fn has_any(text: &str) -> bool {
        PLACEHOLDER.is_match(text)
    }

    /// The ids in [text], lower-case, each once, in the order they appear.
    pub fn ids_in(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in PLACEHOLDER.captures_iter(text) {
            let id = c[1].to_lowercase();
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// [text] with each placeholder replaced by what [value_of] says; an unknown id is left as written.
    pub fn substitute(text: &str, value_of: impl Fn(&str) -> Option<String>) -> String {
        PLACEHOLDER.replace_all(text, |c: &regex::Captures| value_of(&c[1].to_lowercase()).unwrap_or_else(|| c[0].to_string())).into_owned()
    }
}

/// Ids made from names (`Gemini (無料枠)` -> `gemini`).
pub fn slug(name: &str) -> String {
    let half: String = name
        .chars()
        .map(|c| match c as u32 {
            0xff01..=0xff5e => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
            0x3000 => ' ',
            _ => c,
        })
        .collect();
    let lower = half.to_lowercase();
    let mut out = String::new();
    let mut dash = false;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

pub fn valid_id(id: &str) -> bool {
    static VALID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_.\-]{0,63}$").unwrap());
    VALID.is_match(id)
}

/// Where a secret may be sent. Every rule refuses the way an attacker would write the address.
pub mod hosts {
    use super::*;

    /// Why [url] may not carry a secret to [allowed] hosts, or None when it may.
    pub fn problem(url: &str, allowed: &[String], allow_insecure: bool) -> Option<String> {
        let text = url.trim();
        if text.is_empty() {
            return Some("アドレスが空です".into());
        }
        if text.chars().any(|c| c.is_whitespace() || c == '\\' || c.is_control()) {
            return Some("アドレスに空白・バックスラッシュ・制御文字があります".into());
        }
        let Some(after_scheme) = text.split_once("://").map(|(_, rest)| rest) else {
            return Some("完全な URL（https://…）ではありません".into());
        };
        let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('%') {
            return Some("ホストに符号化された文字（%…）があります".into());
        }
        let Ok(parsed) = reqwest::Url::parse(text) else {
            return Some("URL として読めません".into());
        };
        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "https" && !(allow_insecure && scheme == "http") {
            return Some(if scheme == "http" {
                "秘密を http（暗号化なし）で送ることはできません".into()
            } else {
                "秘密を送れるのは https のアドレスだけです".into()
            });
        }
        if authority.contains('@') || !parsed.username().is_empty() || parsed.password().is_some() {
            return Some("アドレスにユーザー名（user@host）があります。見かけと本当の送り先が違います".into());
        }
        let raw_host = match parsed.host() {
            Some(url::Host::Domain(d)) => d.to_string(),
            Some(url::Host::Ipv4(a)) => a.to_string(),
            Some(url::Host::Ipv6(a)) => a.to_string(),
            None => String::new(),
        };
        // the URL parser turns `0x7f.1` into 127.0.0.1; read what was written as well
        let written = written_host(authority);
        let Some(host) = normalize(&raw_host) else {
            return Some("ホストが空か、受け付けない書き方です".into());
        };
        if let Some(w) = normalize(written)
            && is_private(&w)
        {
            return Some(format!("ホスト（{w}）は、この端末かその内側のネットワークです"));
        }
        if is_private(&host) {
            return Some(format!("ホスト（{host}）は、この端末かその内側のネットワークです"));
        }
        if !allows(&host, allowed) {
            let list = if allowed.is_empty() { "なし".to_string() } else { allowed.join(", ") };
            return Some(format!("この秘密は {host} には送れません（送ってよいホスト: {list}）"));
        }
        None
    }

    /// The host as written in an authority (`host:port`, `[v6]:port`), without the port.
    fn written_host(authority: &str) -> &str {
        if let Some(rest) = authority.strip_prefix('[') {
            return rest.split(']').next().unwrap_or("");
        }
        match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => authority,
        }
    }

    /// The host in lower case without a trailing dot; None for an empty one or one with characters other
    /// than letters, digits, dots and hyphens (and the colons of an IPv6 literal).
    pub fn normalize(host: &str) -> Option<String> {
        let h = host.trim().to_lowercase();
        let h = h.trim_end_matches('.');
        if h.is_empty() {
            return None;
        }
        if h.contains(':') {
            return h.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.').then(|| h.to_string());
        }
        static NAME: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^[a-z0-9]([a-z0-9\-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9\-]*[a-z0-9])?)*$").unwrap());
        NAME.is_match(h).then(|| h.to_string())
    }

    /// Whether [host] is one of [allowed]: the same name, `*.example.com` for names under it (not the
    /// name itself), or `*` for any. An entry may be written as a URL.
    pub fn allows(host: &str, allowed: &[String]) -> bool {
        for raw in allowed {
            let mut entry = raw.trim().to_lowercase();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                return true;
            }
            if entry.contains("://") {
                entry = reqwest::Url::parse(&entry).ok().and_then(|u| u.host_str().and_then(normalize)).unwrap_or_default();
            }
            if entry.is_empty() {
                continue;
            }
            if let Some(suffix) = entry.strip_prefix('*') {
                if host.ends_with(suffix) && host.len() > suffix.len() {
                    return true;
                }
            } else if normalize(&entry).as_deref() == Some(host) {
                return true;
            }
        }
        false
    }

    /// Inside the device or the local network, or written so that it cannot be told.
    pub fn is_private(host: &str) -> bool {
        let h = host.to_lowercase();
        if h == "localhost" || h.ends_with(".localhost") {
            return true;
        }
        if h.contains(':') {
            return private_v6(&h);
        }
        let numeric_looking = h.chars().all(|c| c.is_ascii_hexdigit() || c == 'x' || c == '.')
            && h.starts_with(|c: char| c.is_ascii_digit())
            && !h.chars().all(|c| ('a'..='f').contains(&c));
        if numeric_looking {
            let parts: Vec<&str> = h.split('.').collect();
            let plain = parts.len() == 4
                && parts.iter().all(|p| {
                    !p.is_empty()
                        && p.len() <= 3
                        && p.chars().all(|c| c.is_ascii_digit())
                        && (p.len() == 1 || !p.starts_with('0'))
                        && p.parse::<u16>().is_ok_and(|n| n <= 255)
                });
            if !plain {
                return true;
            }
            let o: Vec<u8> = parts.iter().map(|p| p.parse().unwrap()).collect();
            return private_v4(&o);
        }
        if !h.contains('.') {
            return true;
        }
        [".local", ".internal", ".lan", ".home", ".corp", ".intranet", ".private", ".home.arpa"].iter().any(|s| h.ends_with(s))
    }

    fn private_v4(o: &[u8]) -> bool {
        let (a, b) = (o[0], o[1]);
        a == 0
            || a == 10
            || a == 127
            || (a == 100 && (64..=127).contains(&b))
            || (a == 169 && b == 254)
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && b == 0 && o[2] == 0)
            || (a == 192 && b == 168)
            || (a == 198 && (b == 18 || b == 19))
            || a >= 224
    }

    fn private_v6(h: &str) -> bool {
        let Ok(addr) = h.parse::<Ipv6Addr>() else {
            return true;
        };
        let b = addr.octets();
        if b.iter().all(|x| *x == 0) {
            return true;
        }
        if b[..15].iter().all(|x| *x == 0) && b[15] == 1 {
            return true;
        }
        if (b[0] & 0xfe) == 0xfc || (b[0] == 0xfe && (b[1] & 0xc0) == 0x80) || b[0] == 0xff {
            return true;
        }
        let mapped = b[..10].iter().all(|x| *x == 0) && b[10] == 0xff && b[11] == 0xff;
        let compatible = b[..12].iter().all(|x| *x == 0);
        if mapped || compatible {
            return private_v4(&b[12..16]);
        }
        false
    }

    /// The hosts of `scheme://…` addresses written in [text], each once.
    pub fn in_text(text: &str) -> Vec<String> {
        static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s'"`<>)\]}|;,]+"#).unwrap());
        let mut out: Vec<String> = Vec::new();
        for m in URL.find_iter(text) {
            let raw = m.as_str();
            let host = reqwest::Url::parse(raw)
                .ok()
                .and_then(|u| u.host_str().map(|h| normalize(h).unwrap_or_else(|| h.to_string())))
                .unwrap_or_else(|| raw.to_string());
            if !out.contains(&host) {
                out.push(host);
            }
        }
        out
    }
}

/// Takes the values of secrets out of a text, in the forms they turn up in: as they are, URL-encoded,
/// escaped for JSON, as `\uXXXX`, as HTML entities, in hex and in base64 (both alphabets, with and
/// without padding, and at the three alignments a value may have inside a longer base64 text).
pub mod redaction {
    use super::*;

    pub const MIN_LENGTH: usize = 4;
    pub const MIN_ALIGNED_LENGTH: usize = 8;

    fn percent(value: &str, plus_for_space: bool) -> String {
        let mut out = String::new();
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                b' ' if plus_for_space => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    fn aligned(engine: &base64::engine::GeneralPurpose, bytes: &[u8], offset: usize) -> String {
        let mut padded = vec![0u8; offset];
        padded.extend_from_slice(bytes);
        let text = engine.encode(&padded).replace('=', "");
        let head = [0, 2, 3][offset];
        let tail = if (bytes.len() + offset) % 3 == 0 { 0 } else { 1 };
        if text.len() <= head + tail {
            return String::new();
        }
        text[head..text.len() - tail].to_string()
    }

    /// The forms of [value] that are looked for, longest first.
    pub fn variants(value: &str) -> Vec<String> {
        if value.chars().count() < MIN_LENGTH {
            return vec![];
        }
        let bytes = value.as_bytes();
        let std = base64::engine::general_purpose::STANDARD.encode(bytes);
        let url = base64::engine::general_purpose::URL_SAFE.encode(bytes);
        let json = serde_json::to_string(value).unwrap();
        let escaped = json[1..json.len() - 1].to_string();
        let mut forms: Vec<String> = vec![
            value.to_string(),
            percent(value, false),
            percent(value, true),
            escaped.replace('/', "\\/"),
            escaped,
            std.replace('=', ""),
            std,
            url.replace('=', ""),
            url,
            value.encode_utf16().map(|u| format!("\\u{u:04x}")).collect(),
            bytes.iter().map(|b| format!("{b:02x}")).collect(),
            bytes.iter().map(|b| format!("{b:02X}")).collect(),
            value.chars().map(|c| format!("&#{};", c as u32)).collect(),
            value.chars().map(|c| format!("&#x{:x};", c as u32)).collect(),
        ];
        for engine in [&base64::engine::general_purpose::STANDARD, &base64::engine::general_purpose::URL_SAFE] {
            for offset in 0..3 {
                let piece = aligned(engine, bytes, offset);
                if piece.len() >= MIN_ALIGNED_LENGTH {
                    forms.push(piece);
                }
            }
        }
        forms.retain(|f| f.chars().count() >= MIN_LENGTH);
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.dedup();
        forms
    }

    /// [text] with every form of every value replaced by `[secret:<id>]`. With [trim_tail], a tail that is
    /// the start of a form (a value still being written) is removed too.
    pub fn redact(text: &str, id_to_value: &BTreeMap<String, String>, trim_tail: bool) -> String {
        let mut all: Vec<(String, &str)> = Vec::new();
        for (id, value) in id_to_value {
            for form in variants(value) {
                all.push((form, id.as_str()));
            }
        }
        all.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
        let mut out = text.to_string();
        for (form, id) in &all {
            if out.contains(form.as_str()) {
                out = out.replace(form.as_str(), &format!("[secret:{id}]"));
            }
        }
        if !trim_tail {
            return out;
        }
        let mut cut = 0usize;
        for (form, _) in &all {
            let most = (form.len() - 1).min(out.len());
            let mut k = most;
            while k >= MIN_LENGTH && k > cut {
                if form.is_char_boundary(k) && out.ends_with(&form[..k]) {
                    cut = k;
                    break;
                }
                k -= 1;
            }
        }
        if cut == 0 { out } else { out[..out.len() - cut].to_string() }
    }
}

/// One stored secret: its value and the hosts it may be sent to.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredSecret {
    pub value: String,
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// The secrets file. Read once; written with owner-only permissions.
#[derive(Clone, Debug, Default)]
pub struct SecretStore {
    pub path: Option<PathBuf>,
    pub items: BTreeMap<String, StoredSecret>,
}

impl SecretStore {
    pub fn load(path: PathBuf) -> SecretStore {
        let items = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        SecretStore { path: Some(path), items }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&self.items)?;
        let tmp = path.with_extension("tmp");
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The value of [id]: `ARCE_SECRET_<ID>` in the environment first, then the file.
    pub fn value(&self, id: &str) -> Option<String> {
        if let Ok(v) = std::env::var(env_name(id))
            && !v.is_empty()
        {
            return Some(v);
        }
        self.items.get(id).map(|s| s.value.clone()).filter(|v| !v.is_empty())
    }

    /// Every known value, for redaction (environment ones included).
    pub fn all_values(&self, extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for id in self.items.keys() {
            if let Some(v) = self.value(id) {
                out.insert(id.clone(), v);
            }
        }
        for (k, v) in extra {
            if !v.is_empty() {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }
}

/// `ARCE_SECRET_<ID>` with the id upper-cased and anything but letters and digits turned into `_`.
pub fn env_name(id: &str) -> String {
    let body: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("ARCE_SECRET_{body}")
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn placeholders_are_found_once_and_lowercased() {
        let t = "a {{secret:Gem}} b {{ secret : gem }} {{secret:other.key}}";
        assert_eq!(placeholders::ids_in(t), vec!["gem", "other.key"]);
        let out = placeholders::substitute(t, |id| (id == "gem").then(|| "V".to_string()));
        assert_eq!(out, "a V b V {{secret:other.key}}");
    }

    #[test]
    fn slug_makes_ids_from_names() {
        assert_eq!(slug("Gemini (無料枠)"), "gemini");
        assert_eq!(slug("ＯｐｅｎＡＩ　Key"), "openai-key");
        assert_eq!(slug("日本語"), "");
    }

    #[test]
    fn hosts_accept_only_bound_https_hosts() {
        let a = allowed(&["api.example.com"]);
        assert!(hosts::problem("https://api.example.com/v1?key=x", &a, false).is_none());
        assert!(hosts::problem("http://api.example.com/", &a, false).is_some());
        assert!(hosts::problem("http://api.example.com/", &a, true).is_none());
        assert!(hosts::problem("https://evil.example/", &a, false).is_some());
        assert!(hosts::problem("https://api.example.com@evil.example/", &a, false).is_some());
        assert!(hosts::problem("https://api%2eexample.com/", &a, false).is_some());
        assert!(hosts::problem("https://api.example.com /", &a, false).is_some());
    }

    #[test]
    fn wildcard_covers_subdomains_only() {
        let a = allowed(&["*.example.com"]);
        assert!(hosts::allows("api.example.com", &a));
        assert!(!hosts::allows("example.com", &a));
        assert!(!hosts::allows("evil-example.com", &a));
        assert!(hosts::allows("anything.test", &allowed(&["*"])));
    }

    #[test]
    fn private_addresses_are_refused_in_every_spelling() {
        let any = allowed(&["*"]);
        for url in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://10.1.2.3/",
            "https://192.168.0.1/",
            "https://172.16.0.1/",
            "https://169.254.169.254/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[fd00::1]/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://0177.0.0.1/",
            "https://printer/",
            "https://nas.local/",
            "https://metadata.internal/",
        ] {
            assert!(hosts::problem(url, &any, false).is_some(), "{url} should be refused");
        }
        assert!(hosts::problem("https://8.8.8.8/", &any, false).is_none());
    }

    #[test]
    fn hosts_in_text_finds_written_addresses() {
        let h = hosts::in_text("curl -s https://API.example.com/x && wget 'http://other.test/y'");
        assert_eq!(h, vec!["api.example.com", "other.test"]);
    }

    #[test]
    fn redaction_covers_encodings() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), "s3cr3t/value+x".to_string());
        let b64 = base64::engine::general_purpose::STANDARD.encode("s3cr3t/value+x");
        let text = format!(
            "raw s3cr3t/value+x url s3cr3t%2Fvalue%2Bx json s3cr3t\\/value+x b64 {b64} hex {}",
            "s3cr3t/value+x".bytes().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let out = redaction::redact(&text, &m, false);
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(!out.contains(&b64));
        assert_eq!(out.matches("[secret:k]").count(), 5);
    }

    #[test]
    fn redaction_finds_values_inside_longer_base64() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), "token-123456".to_string());
        for prefix in ["", "a", "ab"] {
            let whole = base64::engine::general_purpose::STANDARD.encode(format!("{prefix}token-123456 and more"));
            let out = redaction::redact(&whole, &m, false);
            assert!(out.contains("[secret:k]"), "prefix {prefix:?}: {out}");
        }
    }

    #[test]
    fn redaction_trims_a_cut_tail() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), "abcdefghij".to_string());
        assert_eq!(redaction::redact("value: abcdef", &m, true), "value: ");
        assert_eq!(redaction::redact("value: abc", &m, true), "value: abc");
    }

    #[test]
    fn short_values_are_not_redacted() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), "abc".to_string());
        assert_eq!(redaction::redact("abc abc", &m, false), "abc abc");
    }

    #[test]
    fn env_names_are_upper_snake() {
        assert_eq!(env_name("open-router.key"), "ARCE_SECRET_OPEN_ROUTER_KEY");
    }
}

//! Markdown for the terminal, as it streams.
//!
//! A line is drawn when it is complete; a table when its last row has arrived. What is drawn: headings,
//! **bold**, *italic*, ~~strike~~, `code`, links (`text (url)`; only http(s)), lists and task lists,
//! quotes, rules, tables and fenced code blocks. HTML, images and footnotes stay as text. With
//! colours off the text is the same with the marks taken out.

use std::sync::LazyLock;

use regex::Regex;

use crate::theme::{Style, Token, display_width, paint};

static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.*?)\s*#*\s*$").unwrap());
static RULE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}(?:(?:-\s*){3,}|(?:\*\s*){3,}|(?:_\s*){3,})$").unwrap());
static LIST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*)([-*+]|\d{1,9}[.)])\s+(\[[ xX]\]\s+)?(.*)$").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}(`{3,}|~{3,})\s*([^`\s]*)").unwrap());
static TABLE_SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$").unwrap());

struct Code {
    fence: String,
    lang: String,
    lines: usize,
    hidden: usize,
}

pub struct Renderer {
    pending: String,
    code: Option<Code>,
    table: Vec<String>,
    pub width: usize,
    /// code blocks show at most this many lines (0: all)
    pub fold: usize,
}

impl Renderer {
    pub fn new(width: usize, fold: usize) -> Renderer {
        Renderer { pending: String::new(), code: None, table: vec![], width, fold }
    }

    /// Whether a code block is open (the text so far ends inside one).
    pub fn in_code(&self) -> bool {
        self.code.is_some()
    }

    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let mut out = String::new();
        while let Some(nl) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=nl).collect();
            out.push_str(&self.line(line.trim_end_matches(['\n', '\r'])));
        }
        out
    }

    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            out.push_str(&self.line(&rest));
        }
        out.push_str(&self.flush_table());
        if let Some(code) = self.code.take() {
            out.push_str(&code_footer(&code));
        }
        out
    }

    fn line(&mut self, line: &str) -> String {
        if let Some(code) = &mut self.code {
            let t = line.trim_start();
            if t.starts_with(&code.fence) && t.trim_end().chars().all(|c| c == code.fence.chars().next().unwrap()) {
                let code = self.code.take().unwrap();
                return code_footer(&code);
            }
            code.lines += 1;
            if self.fold > 0 && code.lines > self.fold {
                code.hidden += 1;
                return String::new();
            }
            return format!("{} {}\n", paint("│", Style::fg(Token::Line)), paint(line, Style::fg(Token::Ink)));
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('|') || (!self.table.is_empty() && trimmed.contains('|') && !trimmed.is_empty()) {
            self.table.push(line.to_string());
            return String::new();
        }
        let mut out = self.flush_table();
        if let Some(c) = FENCE.captures(line) {
            let fence = c[1].to_string();
            let lang = c.get(2).map_or("", |m| m.as_str()).to_string();
            let head = if lang.is_empty() { "code".to_string() } else { lang.clone() };
            out.push_str(&format!("{}\n", paint(&format!("╭─ {head} "), Style::fg(Token::Faint))));
            self.code = Some(Code { fence, lang, lines: 0, hidden: 0 });
            return out;
        }
        out.push_str(&block_line(line, self.width));
        out
    }

    fn flush_table(&mut self) -> String {
        if self.table.is_empty() {
            return String::new();
        }
        let rows = std::mem::take(&mut self.table);
        if rows.len() < 2 || !TABLE_SEP.is_match(&rows[1]) {
            return rows.iter().map(|r| block_line(r, self.width)).collect();
        }
        render_table(&rows, self.width)
    }
}

fn code_footer(code: &Code) -> String {
    let mut s = String::new();
    if code.hidden > 0 {
        s.push_str(&format!(
            "{} {}\n",
            paint("│", Style::fg(Token::Line)),
            paint(&format!("… {} 行を省略", code.hidden), Style::fg(Token::Faint))
        ));
    }
    let what = if code.lang.is_empty() { format!("{} 行", code.lines) } else { format!("{} · {} 行", code.lang, code.lines) };
    s.push_str(&format!("{}\n", paint(&format!("╰─ {what}"), Style::fg(Token::Faint))));
    s
}

fn block_line(line: &str, width: usize) -> String {
    if line.trim().is_empty() {
        return "\n".into();
    }
    if let Some(c) = HEADING.captures(line) {
        let level = c[1].len();
        let text = inline(&c[2]);
        let style = if level <= 2 { Style::fg(Token::Accent).bold() } else { Style::fg(Token::Ink).bold() };
        let mut s = format!("{}\n", paint(&crate::theme::strip_ansi(&text), style));
        if level == 1 {
            s.push_str(&format!("{}\n", paint(&"─".repeat(display_width(&text).min(width)), Style::fg(Token::Line))));
        }
        return s;
    }
    if RULE.is_match(line) {
        return format!("{}\n", paint(&"─".repeat(width.min(72)), Style::fg(Token::Line)));
    }
    if let Some(rest) = line.trim_start().strip_prefix('>') {
        let text = rest.strip_prefix(' ').unwrap_or(rest);
        return format!(
            "{} {}\n",
            paint("▎", Style::fg(Token::Accent2)),
            paint(&crate::theme::strip_ansi(&inline(text)), Style::fg(Token::Muted).italic())
        );
    }
    if let Some(c) = LIST.captures(line) {
        let indent = c[1].replace('\t', "    ").len();
        let marker = &c[2];
        let bullet = if marker.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            paint(marker, Style::fg(Token::Accent))
        } else {
            paint(["•", "◦", "▪"][(indent / 2).min(2)], Style::fg(Token::Accent))
        };
        let check = match c.get(3).map(|m| m.as_str().trim()) {
            Some("[ ]") => format!("{} ", paint("☐", Style::fg(Token::Muted))),
            Some(_) => format!("{} ", paint("☑", Style::fg(Token::Green))),
            None => String::new(),
        };
        return format!("{}{} {}{}\n", " ".repeat(indent), bullet, check, inline(&c[4]));
    }
    format!("{}\n", inline(line))
}

fn split_row(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = vec![];
    let mut cur = String::new();
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '`' => {
                code = !code;
                cur.push(c);
            }
            '|' if !code => cells.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    cells.push(cur);
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

fn render_table(rows: &[String], width: usize) -> String {
    let aligns: Vec<char> = split_row(&rows[1])
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => 'c',
            (false, true) => 'r',
            _ => 'l',
        })
        .collect();
    let body: Vec<Vec<String>> =
        std::iter::once(&rows[0]).chain(rows[2..].iter()).map(|r| split_row(r).into_iter().map(|c| inline(&c)).collect()).collect();
    let cols = body.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in &body {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(display_width(c));
        }
    }
    // shrink the widest columns until the table fits
    let total = |w: &[usize]| w.iter().sum::<usize>() + 3 * w.len() + 1;
    while total(&widths) > width && widths.iter().any(|w| *w > 8) {
        let i = (0..cols).max_by_key(|i| widths[*i]).unwrap();
        widths[i] -= 1;
    }
    let line = |l: &str, m: &str, r: &str| -> String {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        paint(&format!("{l}{}{r}", parts.join(m)), Style::fg(Token::Line))
    };
    let bar = paint("│", Style::fg(Token::Line));
    let mut out = format!("{}\n", line("┌", "┬", "┐"));
    for (n, r) in body.iter().enumerate() {
        let mut s = bar.clone();
        for (i, width) in widths.iter().enumerate() {
            let cell = r.get(i).cloned().unwrap_or_default();
            let cell = fit(&cell, *width);
            let pad = width.saturating_sub(display_width(&cell));
            let (left, right) = match aligns.get(i).copied().unwrap_or('l') {
                'r' => (pad, 0),
                'c' => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            let cell = if n == 0 { paint(&crate::theme::strip_ansi(&cell), Style::fg(Token::Ink).bold()) } else { cell };
            s.push_str(&format!(" {}{}{} {bar}", " ".repeat(left), cell, " ".repeat(right)));
        }
        out.push_str(&s);
        out.push('\n');
        if n == 0 {
            out.push_str(&format!("{}\n", line("├", "┼", "┤")));
        }
    }
    out.push_str(&format!("{}\n", line("└", "┴", "┘")));
    out
}

/// [text] cut to [width] columns with `…` (styles are dropped when cut).
fn fit(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let plain = crate::theme::strip_ansi(text);
    let mut out = String::new();
    let mut w = 0;
    for c in plain.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Inline marks: `code`, **bold**, *italic*, ~~strike~~, [text](url).
pub fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&k| chars[k..k + pat.len()] == *pat)
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && "\\`*_~[]()#|".contains(chars[i + 1]) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '`' {
            let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
            let pat: Vec<char> = vec!['`'; ticks];
            if let Some(end) = find(i + ticks, &pat) {
                let code: String = chars[i + ticks..end].iter().collect();
                out.push_str(&paint(code.trim(), Style::fg(Token::Accent2)));
                i = end + ticks;
                continue;
            }
        }
        if c == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find(i + 2, &['*', '*'])
            && end > i + 2
        {
            let inner: String = chars[i + 2..end].iter().collect();
            out.push_str(&paint(&crate::theme::strip_ansi(&inline(&inner)), Style::fg(Token::Ink).bold()));
            i = end + 2;
            continue;
        }
        if c == '~'
            && i + 1 < chars.len()
            && chars[i + 1] == '~'
            && let Some(end) = find(i + 2, &['~', '~'])
            && end > i + 2
        {
            let inner: String = chars[i + 2..end].iter().collect();
            out.push_str(&paint(&inner, Style::fg(Token::Muted).strike()));
            i = end + 2;
            continue;
        }
        if c == '*'
            && i + 1 < chars.len()
            && !chars[i + 1].is_whitespace()
            && let Some(end) = find(i + 1, &['*'])
            && end > i + 1
            && !chars[end - 1].is_whitespace()
        {
            let inner: String = chars[i + 1..end].iter().collect();
            out.push_str(&paint(&inner, Style::fg(Token::Ink).italic()));
            i = end + 1;
            continue;
        }
        if c == '['
            && let Some(close) = find(i + 1, &[']'])
            && close + 1 < chars.len()
            && chars[close + 1] == '('
            && let Some(end) = find(close + 2, &[')'])
        {
            let label: String = chars[i + 1..close].iter().collect();
            let url: String = chars[close + 2..end].iter().collect();
            match safe_web_url(&url) {
                Some(u) => {
                    let shown = inline(&label);
                    out.push_str(&paint(&crate::theme::strip_ansi(&shown), Style::fg(Token::Accent).underline()));
                    if crate::theme::strip_ansi(&shown) != u {
                        out.push_str(&paint(&format!(" ({u})"), Style::fg(Token::Faint)));
                    }
                }
                None => out.push_str(&inline(&label)),
            }
            i = end + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The address to show for a link that came from a model: only http(s), without a user name (a way
/// to make a bad host look like a good one).
pub fn safe_web_url(raw: &str) -> Option<String> {
    let text: String = raw.chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect();
    if text.is_empty() || text.len() > 4096 {
        return None;
    }
    let u = reqwest::Url::parse(&text).ok()?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none_or(str::is_empty)
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return None;
    }
    Some(u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::strip_ansi;

    fn render(text: &str) -> String {
        let mut r = Renderer::new(80, 0);
        let mut out = r.push(text);
        out.push_str(&r.finish());
        strip_ansi(&out)
    }

    #[test]
    fn inline_marks_are_taken_out() {
        assert_eq!(strip_ansi(&inline("a **b** *c* `d` ~~e~~ snake_case_name")), "a b c d e snake_case_name");
    }

    #[test]
    fn links_show_only_web_addresses() {
        assert_eq!(strip_ansi(&inline("[x](https://a.test/)")), "x (https://a.test/)");
        assert_eq!(strip_ansi(&inline("[x](javascript:alert(1))")), "x)");
        assert!(safe_web_url("https://good.test@evil.test/").is_none());
        assert!(safe_web_url("file:///etc/passwd").is_none());
    }

    #[test]
    fn code_blocks_are_framed_and_counted() {
        let out = render("```rust\nfn main() {}\n```\n");
        assert_eq!(out, "╭─ rust \n│ fn main() {}\n╰─ rust · 1 行\n");
    }

    #[test]
    fn code_blocks_fold() {
        let mut r = Renderer::new(80, 2);
        let mut out = r.push("```\n1\n2\n3\n4\n```\n");
        out.push_str(&r.finish());
        assert!(strip_ansi(&out).contains("… 2 行を省略"));
    }

    #[test]
    fn tables_are_aligned_with_wide_characters() {
        let out = render("| 名前 | n |\n|:--|--:|\n| あ | 10 |\n| abc | 2 |\nafter\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "┌──────┬────┐");
        assert_eq!(lines[1], "│ 名前 │  n │");
        assert_eq!(lines[3], "│ あ   │ 10 │");
        assert_eq!(lines[4], "│ abc  │  2 │");
        assert_eq!(lines[6], "after");
    }

    #[test]
    fn lists_quotes_headings_and_rules() {
        let out = render("# Title\n- a\n  - b\n1. one\n- [x] done\n> quoted\n---\n");
        assert!(out.starts_with("Title\n─────\n• a\n  ◦ b\n1. one\n• ☑ done\n▎ quoted\n"));
    }

    #[test]
    fn lines_arrive_in_pieces() {
        let mut r = Renderer::new(80, 0);
        assert_eq!(r.push("**bo"), "");
        assert_eq!(strip_ansi(&r.push("ld**\n")), "bold\n");
    }
}

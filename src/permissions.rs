//! Whether a tool call runs, asks, or is refused.
//!
//! Deny rules always win. Then the mode sets the frame (ask / auto-edit / plan / yolo), rules
//! (`tool` or `tool(glob)`) refine it, and a few things always ask: changing settings, writing outside
//! the working folders, and commands after the reply has read something from outside (a web page, an
//! HTTP answer, an MCP result may carry instructions that are not the user's).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobMatcher};

use crate::settings::{HttpConfirm, PermissionMode, Permissions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// reads the workspace
    Read,
    /// changes files
    Write,
    /// runs a program
    Command,
    /// sends a request out
    Network,
    /// asks the user, keeps a plan, reads settings, searches: nothing changes
    Interaction,
    /// changes the CLI's own settings
    Settings,
    Mcp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// ask, with the reason the card shows (empty: the usual one)
    Ask(String),
    Deny(String),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub text: String,
    tool: GlobMatcher,
    subject: Option<GlobMatcher>,
}

impl Rule {
    /// `read_file`, `mcp_github__*`, `run_command(git status*)`, `write_file(src/**)`.
    pub fn parse(text: &str) -> Option<Rule> {
        let t = text.trim();
        let (tool, subject) = match t.find('(') {
            Some(open) if t.ends_with(')') => (&t[..open], Some(&t[open + 1..t.len() - 1])),
            _ => (t, None),
        };
        let tool = Glob::new(tool.trim()).ok()?.compile_matcher();
        let subject = match subject {
            Some(s) => Some(Glob::new(s.trim()).ok()?.compile_matcher()),
            None => None,
        };
        Some(Rule { text: t.to_string(), tool, subject })
    }

    pub fn matches(&self, tool: &str, subject: &str) -> bool {
        self.tool.is_match(tool) && self.subject.as_ref().is_none_or(|s| s.is_match(subject))
    }
}

/// The simple commands of a shell line (split at `&&`, `||`, `;`, `|`, `&` and new lines), or None
/// when the line does more than run commands one after another: substitutions (`$(…)`, backticks),
/// redirections, or quotes that are not closed. A rule allows a line only when it allows every part.
pub fn split_shell(command: &str) -> Option<Vec<String>> {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut single = false;
    let mut double = false;
    while let Some(c) = chars.next() {
        if single {
            cur.push(c);
            if c == '\'' {
                single = false;
            }
            continue;
        }
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !double => {
                single = true;
                cur.push(c);
            }
            '"' => {
                double = !double;
                cur.push(c);
            }
            '`' => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            '>' | '<' if !double => return None,
            '&' | '|' | ';' | '\n' if !double => {
                if matches!(chars.peek(), Some('&') | Some('|')) {
                    chars.next();
                }
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if single || double {
        return None;
    }
    parts.push(cur);
    let parts: Vec<String> = parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then_some(parts)
}

/// `a/./b/../c` -> `a/c` without touching the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// [path] as an absolute path (from [cwd]; `~` is the home folder), with symbolic links of the part
/// that exists resolved, so a link inside the workspace cannot point a write outside it.
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = if let Some(rest) = path.strip_prefix("~/") { dirs::home_dir().unwrap_or_default().join(rest) } else { PathBuf::from(path) };
    let abs = normalize(&if p.is_absolute() { p } else { cwd.join(p) });
    // canonicalize the deepest part that exists
    let mut existing = abs.clone();
    let mut rest = vec![];
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return abs,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

pub struct PermissionState {
    pub mode: PermissionMode,
    pub allow: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub deny: Vec<Rule>,
    pub confirm_after_external: bool,
    pub http_confirm: HttpConfirm,
    /// folders the tools may use without asking (the working folder first)
    pub roots: Vec<PathBuf>,
    /// tools the user said not to ask about again in this session (never commands)
    pub approved: HashSet<String>,
    /// MCP tools the settings trust
    pub trusted_mcp: HashSet<String>,
    /// whether this reply has read something from outside
    pub external_seen: bool,
    /// whether a person can answer a question (false in `-p`)
    pub interactive: bool,
}

impl PermissionState {
    pub fn new(mode: PermissionMode, p: &Permissions, roots: Vec<PathBuf>, interactive: bool) -> (PermissionState, Vec<String>) {
        let mut bad = vec![];
        let mut rules = |list: &[String]| -> Vec<Rule> {
            list.iter()
                .filter_map(|t| {
                    let r = Rule::parse(t);
                    if r.is_none() {
                        bad.push(format!("権限の規則「{t}」を読めません"));
                    }
                    r
                })
                .collect()
        };
        let state = PermissionState {
            mode,
            allow: rules(&p.allow),
            ask: rules(&p.ask),
            deny: rules(&p.deny),
            confirm_after_external: p.confirm_after_external,
            http_confirm: p.http_confirm,
            roots,
            approved: HashSet::new(),
            trusted_mcp: HashSet::new(),
            external_seen: false,
            interactive,
        };
        (state, bad)
    }

    pub fn inside_roots(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }

    /// The decision for [tool] of [kind]. [subjects]: what rules are matched against (the parts of a
    /// command, a path, a URL); [outside]: a path outside the working folders; [secret]: the call
    /// carries a secret.
    pub fn decide(&self, tool: &str, kind: ToolKind, subjects: &[String], outside: bool, secret: bool) -> Decision {
        // a command that is more than plain commands one after another is matched whole by deny and ask
        // rules, and never allowed by a rule
        let (parts, plain) = if kind == ToolKind::Command {
            match subjects.first().and_then(|c| split_shell(c)) {
                Some(p) if subjects.len() == 1 => (p, true),
                _ => (subjects.to_vec(), subjects.len() > 1),
            }
        } else {
            (subjects.to_vec(), true)
        };
        let subjects = &parts;
        let any = |rules: &[Rule]| -> Option<String> {
            rules
                .iter()
                .find(|r| subjects.iter().any(|s| r.matches(tool, s)) || (subjects.is_empty() && r.matches(tool, "")))
                .map(|r| r.text.clone())
        };
        let all = |rules: &[Rule]| -> bool {
            plain
                && (!subjects.is_empty() && subjects.iter().all(|s| rules.iter().any(|r| r.matches(tool, s)))
                    || subjects.is_empty() && rules.iter().any(|r| r.matches(tool, "")))
        };
        if let Some(r) = any(&self.deny) {
            return Decision::Deny(format!("設定の規則 {r} で禁止されています"));
        }
        let yolo = self.mode == PermissionMode::Yolo;
        let after_external = self.confirm_after_external && self.external_seen;
        if kind == ToolKind::Settings {
            return Decision::Ask(String::new());
        }
        if self.mode == PermissionMode::Plan {
            match kind {
                ToolKind::Write => return Decision::Deny("計画モードでは、ファイルを変更できません（/mode で切り替えられます）".into()),
                ToolKind::Command if !all(&self.allow) => {
                    return Decision::Deny("計画モードでは、許可の規則にないコマンドは動かせません".into());
                }
                _ => {}
            }
        }
        if let Some(r) = any(&self.ask) {
            return Decision::Ask(format!("設定の規則 {r} で、確認することになっています"));
        }
        if kind == ToolKind::Command && secret && !yolo {
            return Decision::Ask("秘密を環境変数かコマンドに入れます。コマンドは値をどこへでも送れます".into());
        }
        if kind == ToolKind::Command && after_external && !yolo {
            return Decision::Ask("外部の内容（Web・HTTP・MCP）を読んだあとのコマンドです".into());
        }
        if outside && !yolo && kind != ToolKind::Network {
            return Decision::Ask("作業フォルダの外のパスです".into());
        }
        if all(&self.allow) {
            return Decision::Allow;
        }
        if kind != ToolKind::Command && self.approved.contains(tool) {
            return Decision::Allow;
        }
        match kind {
            ToolKind::Read | ToolKind::Interaction => Decision::Allow,
            ToolKind::Write => match self.mode {
                PermissionMode::Yolo | PermissionMode::AutoEdit => Decision::Allow,
                _ => Decision::Ask(String::new()),
            },
            ToolKind::Command => {
                if yolo {
                    Decision::Allow
                } else {
                    Decision::Ask(String::new())
                }
            }
            ToolKind::Network => match self.http_confirm {
                _ if yolo => Decision::Allow,
                HttpConfirm::Never => Decision::Allow,
                HttpConfirm::WithSecrets if !secret => Decision::Allow,
                _ => Decision::Ask(if secret { "秘密を送ります".into() } else { String::new() }),
            },
            ToolKind::Mcp => {
                if yolo || self.trusted_mcp.contains(tool) {
                    Decision::Allow
                } else {
                    Decision::Ask(String::new())
                }
            }
            ToolKind::Settings => Decision::Ask(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(mode: PermissionMode, allow: &[&str], deny: &[&str]) -> PermissionState {
        let p = Permissions {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Permissions::default()
        };
        PermissionState::new(mode, &p, vec![PathBuf::from("/work")], true).0
    }

    fn cmd(c: &str) -> Vec<String> {
        vec![c.to_string()]
    }

    #[test]
    fn shell_lines_split_and_tricks_are_refused() {
        assert_eq!(split_shell("git status && cargo test | tail -5").unwrap(), vec!["git status", "cargo test", "tail -5"]);
        assert_eq!(split_shell("echo 'a && b'").unwrap(), vec!["echo 'a && b'"]);
        assert!(split_shell("echo $(rm -rf /)").is_none());
        assert!(split_shell("echo `id`").is_none());
        assert!(split_shell("cat x > /etc/passwd").is_none());
        assert!(split_shell("echo 'open").is_none());
    }

    #[test]
    fn allow_rules_must_cover_every_part() {
        let s = state(PermissionMode::Ask, &["run_command(git status*)", "run_command(cargo test*)"], &[]);
        assert_eq!(s.decide("run_command", ToolKind::Command, &cmd("git status -s"), false, false), Decision::Allow);
        assert_eq!(s.decide("run_command", ToolKind::Command, &cmd("git status && cargo test"), false, false), Decision::Allow);
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("git status && rm -rf x"), false, false), Decision::Ask(_)));
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("git status $(rm x)"), false, false), Decision::Ask(_)));
    }

    #[test]
    fn deny_wins_over_everything() {
        let s = state(PermissionMode::Yolo, &["run_command"], &["run_command(rm *)"]);
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("rm -rf x"), false, false), Decision::Deny(_)));
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("ls && rm -rf x"), false, false), Decision::Deny(_)));
    }

    #[test]
    fn modes_frame_the_defaults() {
        let ask = state(PermissionMode::Ask, &[], &[]);
        let edit = state(PermissionMode::AutoEdit, &[], &[]);
        let plan = state(PermissionMode::Plan, &[], &[]);
        let p = vec!["/work/a.rs".to_string()];
        assert_eq!(ask.decide("read_file", ToolKind::Read, &p, false, false), Decision::Allow);
        assert!(matches!(ask.decide("write_file", ToolKind::Write, &p, false, false), Decision::Ask(_)));
        assert_eq!(edit.decide("write_file", ToolKind::Write, &p, false, false), Decision::Allow);
        assert!(matches!(edit.decide("write_file", ToolKind::Write, &p, true, false), Decision::Ask(_)));
        assert!(matches!(plan.decide("write_file", ToolKind::Write, &p, false, false), Decision::Deny(_)));
        assert!(matches!(plan.decide("run_command", ToolKind::Command, &cmd("ls"), false, false), Decision::Deny(_)));
        assert!(matches!(
            state(PermissionMode::Yolo, &[], &[]).decide("update_settings", ToolKind::Settings, &[], false, false),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn commands_after_external_content_ask_even_when_allowed() {
        let mut s = state(PermissionMode::Ask, &["run_command(ls*)"], &[]);
        s.external_seen = true;
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("ls"), false, false), Decision::Ask(_)));
    }

    #[test]
    fn remembered_tools_never_include_commands() {
        let mut s = state(PermissionMode::Ask, &[], &[]);
        s.approved.insert("run_command".into());
        s.approved.insert("mcp_x__y".into());
        assert!(matches!(s.decide("run_command", ToolKind::Command, &cmd("ls"), false, false), Decision::Ask(_)));
        assert_eq!(s.decide("mcp_x__y", ToolKind::Mcp, &[], false, false), Decision::Allow);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_normalizes_and_stays_absolute() {
        let r = resolve(Path::new("/nonexistent/work"), "a/../b/./c.txt");
        assert_eq!(r, PathBuf::from("/nonexistent/work/b/c.txt"));
    }
}

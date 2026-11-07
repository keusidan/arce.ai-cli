//! Putting a controller together from the settings and the command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::controller::Controller;
use crate::llm;
use crate::mcp;
use crate::permissions::PermissionState;
use crate::secrets::{SecretStore, placeholders};
use crate::session::Session;
use crate::settings::{self, Paths, PermissionMode};
use crate::theme;
use crate::ui::{Level, Ui};

#[derive(Default, Clone)]
pub struct Options {
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub agent: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub resume: Option<String>,
    pub continue_last: bool,
    pub no_session: bool,
    pub interactive: bool,
}

pub fn working_dir(opt: &Options) -> Result<PathBuf> {
    let cwd = match &opt.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    cwd.canonicalize().with_context(|| format!("{} を開けません", cwd.display()))
}

pub async fn build(opt: &Options, make_ui: impl FnOnce(&settings::Settings) -> Arc<dyn Ui>) -> Result<Controller> {
    let paths = Paths::detect();
    let cwd = working_dir(opt)?;
    let loaded = settings::load(&paths, &cwd)?;
    let mut s = loaded.settings;
    if let Some(m) = &opt.model {
        s.model = m.clone();
    }
    if let Some(m) = &opt.mode {
        s.mode = PermissionMode::parse(m).with_context(|| format!("モード「{m}」はありません（ask / auto-edit / plan / yolo）"))?;
    }
    if let Some(a) = &opt.agent {
        if a != "auto" && crate::agents::by_id(a).is_none() {
            bail!("エージェント「{a}」はありません（auto / general / coding / research / artifact / file-analysis）");
        }
        s.agent = a.clone();
    }
    s.permissions.allow.extend(opt.allow.iter().cloned());
    s.permissions.deny.extend(opt.deny.iter().cloned());
    theme::init(&s.ui.theme);
    let ui = make_ui(&s);
    for w in &loaded.warnings {
        ui.notice(Level::Warn, w);
    }
    let store = SecretStore::load(paths.secrets_file());
    let root = settings::project_root(&cwd);
    let is_git = root.join(".git").exists();
    let mut roots = vec![cwd.clone()];
    for d in s.permissions.additional_dirs.iter().map(PathBuf::from).chain(opt.add_dirs.iter().cloned()) {
        let d = if d.is_absolute() { d } else { cwd.join(d) };
        match d.canonicalize() {
            Ok(p) if !roots.contains(&p) => roots.push(p),
            Ok(_) => {}
            Err(_) => ui.notice(Level::Warn, &format!("{} がないので、作業フォルダに足しませんでした", d.display())),
        }
    }
    let (mut perms, bad) = PermissionState::new(s.mode, &s.permissions, roots, opt.interactive);
    for b in bad {
        ui.notice(Level::Warn, &b);
    }
    let fill = |v: &str| placeholders::substitute(v, |id| store.value(id));
    let (mcp, errors) =
        mcp::Manager::start(&s.mcp.servers, s.mcp.lazy_threshold, &fill, Duration::from_secs(s.limits.mcp_timeout_secs)).await;
    for e in errors {
        ui.notice(Level::Warn, &e);
    }
    for server in &s.mcp.servers {
        for t in &server.trusted_tools {
            perms.trusted_mcp.insert(format!("mcp_{}__{}", server.id, t));
        }
    }
    let sessions = paths.sessions_dir();
    let session = if let Some(id) = &opt.resume {
        Session::open(&sessions, id)?
    } else if opt.continue_last {
        match Session::latest(&sessions, &cwd) {
            Some(id) => Session::open(&sessions, &id)?,
            None => {
                ui.notice(Level::Info, "このフォルダの前のセッションがないので、新しく始めます");
                Session::create(&sessions, &cwd, &s.model, opt.no_session)?
            }
        }
    } else {
        Session::create(&sessions, &cwd, &s.model, opt.no_session)?
    };
    let client = llm::http_client(Duration::from_secs(s.limits.connect_timeout_secs));
    let mut c = Controller {
        settings: s,
        paths,
        cwd,
        root,
        is_git,
        store,
        client,
        session,
        perms,
        mcp,
        ui,
        last_agent: None,
        delegated: Default::default(),
        cache_refused: Default::default(),
        last_meter: None,
        instructions: vec![],
    };
    c.reload_instructions();
    Ok(c)
}

/// Custom commands: `.arce/commands/<name>.md` of the project, then the user's.
pub fn custom_commands(paths: &Paths, root: &Path) -> Vec<(String, PathBuf, String)> {
    let mut out: Vec<(String, PathBuf, String)> = vec![];
    for dir in [root.join(".arce").join("commands"), paths.commands_dir()] {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        files.sort();
        for p in files {
            let name = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
            if out.iter().any(|(n, _, _)| *n == name) {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let (desc, _) = split_frontmatter(&text);
            out.push((name, p, desc));
        }
    }
    out
}

/// (description, body) of a command file with an optional `---` front matter.
pub fn split_frontmatter(text: &str) -> (String, String) {
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let head = &rest[..end];
        let body = rest[end + 4..].trim_start_matches(['-', '\n']).to_string();
        let desc =
            head.lines().find_map(|l| l.strip_prefix("description:")).map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
        return (desc, body);
    }
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_start_matches('#').trim().to_string();
    (first, text.to_string())
}

/// The message a custom command sends: `$ARGUMENTS` replaced, or the arguments added at the end.
pub fn expand_command(body: &str, args: &str) -> String {
    if body.contains("$ARGUMENTS") {
        body.replace("$ARGUMENTS", args)
    } else if args.trim().is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{args}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_and_arguments() {
        let (d, b) = split_frontmatter("---\ndescription: \"レビュー\"\n---\nReview $ARGUMENTS please\n");
        assert_eq!(d, "レビュー");
        assert_eq!(expand_command(&b, "src/a.rs"), "Review src/a.rs please\n");
        let (d, b) = split_frontmatter("# Tests\nRun the tests");
        assert_eq!(d, "Tests");
        assert_eq!(expand_command(&b, "fast"), "# Tests\nRun the tests\n\nfast");
    }
}

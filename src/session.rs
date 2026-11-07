//! Sessions: the conversation on disk, and the files as they were before a turn changed them.
//!
//! A session is one JSONL file (`sessions/<id>.jsonl`), appended to as the conversation goes; a
//! compaction appends the new history whole, so nothing earlier is rewritten. Before a tool changes a
//! file, its earlier content is copied to `sessions/<id>/checkpoints/<turn>/`, once per turn, so
//! `/undo` can put the files of the last turn back.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::llm::Message;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Meta {
        id: String,
        created: String,
        cwd: String,
        model: String,
    },
    Msg {
        m: Message,
    },
    /// the whole history after a compaction or an undo
    Replace {
        history: Vec<Message>,
    },
    Title {
        title: String,
    },
    Turn {
        n: u32,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub title: String,
    pub created: String,
    pub cwd: String,
    pub model: String,
    pub messages: usize,
    pub modified: Option<std::time::SystemTime>,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub file: PathBuf,
    pub history: Vec<Message>,
    pub title: String,
    pub turn: u32,
    /// sent as the provider's prompt cache key when the provider wants one
    pub cache_key: String,
    /// writes are skipped (tests, `--no-session`)
    pub ephemeral: bool,
}

fn new_id() -> String {
    let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let tail: u32 = rand::random::<u32>() % 0x10000;
    format!("{now}-{tail:04x}")
}

fn random_key() -> String {
    let n: u64 = rand::random();
    format!("arce-{n:016x}")
}

impl Session {
    pub fn create(sessions_dir: &Path, cwd: &Path, model: &str, ephemeral: bool) -> Result<Session> {
        let id = new_id();
        let s = Session {
            dir: sessions_dir.join(&id),
            file: sessions_dir.join(format!("{id}.jsonl")),
            id: id.clone(),
            history: vec![],
            title: String::new(),
            turn: 0,
            cache_key: random_key(),
            ephemeral,
        };
        if !ephemeral {
            fs::create_dir_all(sessions_dir)?;
            s.append(&Record::Meta {
                id,
                created: chrono::Local::now().to_rfc3339(),
                cwd: cwd.display().to_string(),
                model: model.to_string(),
            })?;
        }
        Ok(s)
    }

    pub fn open(sessions_dir: &Path, id: &str) -> Result<Session> {
        let file = sessions_dir.join(format!("{id}.jsonl"));
        let text = fs::read_to_string(&file).with_context(|| format!("セッション {id} が見つかりません"))?;
        let mut s = Session {
            dir: sessions_dir.join(id),
            file,
            id: id.to_string(),
            history: vec![],
            title: String::new(),
            turn: 0,
            cache_key: format!("arce-{id}"),
            ephemeral: false,
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(rec) = serde_json::from_str::<Record>(line) else { continue };
            match rec {
                Record::Msg { m } => s.history.push(m),
                Record::Replace { history } => s.history = history,
                Record::Title { title } => s.title = title,
                Record::Turn { n } => s.turn = n,
                Record::Meta { .. } => {}
            }
        }
        // a reply cut off in the middle may have left calls without results: drop the dangling tail
        s.history = without_dangling_calls(std::mem::take(&mut s.history));
        Ok(s)
    }

    /// The newest session of [cwd] (for `--continue`).
    pub fn latest(sessions_dir: &Path, cwd: &Path) -> Option<String> {
        let cwd = cwd.display().to_string();
        list(sessions_dir).into_iter().find(|i| i.cwd == cwd).map(|i| i.id)
    }

    fn append(&self, rec: &Record) -> Result<()> {
        if self.ephemeral {
            return Ok(());
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&self.file)?;
        writeln!(f, "{}", serde_json::to_string(rec)?)?;
        Ok(())
    }

    pub fn push(&mut self, m: Message) -> Result<()> {
        self.append(&Record::Msg { m: m.clone() })?;
        self.history.push(m);
        Ok(())
    }

    pub fn replace(&mut self, history: Vec<Message>) -> Result<()> {
        self.append(&Record::Replace { history: history.clone() })?;
        self.history = history;
        Ok(())
    }

    pub fn set_title(&mut self, title: &str) -> Result<()> {
        self.title = title.to_string();
        self.append(&Record::Title { title: title.to_string() })
    }

    pub fn next_turn(&mut self) -> Result<u32> {
        self.turn += 1;
        self.append(&Record::Turn { n: self.turn })?;
        Ok(self.turn)
    }

    fn checkpoint_dir(&self, turn: u32) -> PathBuf {
        self.dir.join("checkpoints").join(format!("{turn:06}"))
    }

    /// Keeps [path] as it is now, once per turn (the first change of a turn is the one to undo).
    pub fn checkpoint(&self, path: &Path) -> Result<()> {
        let dir = self.checkpoint_dir(self.turn);
        let manifest_path = dir.join("manifest.json");
        let mut manifest: Vec<Saved> =
            fs::read_to_string(&manifest_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        if manifest.iter().any(|s| s.path == path) {
            return Ok(());
        }
        fs::create_dir_all(&dir)?;
        let existed = path.is_file();
        let backup = format!("{}.bak", manifest.len());
        if existed {
            fs::copy(path, dir.join(&backup)).with_context(|| format!("{} を控えられません", path.display()))?;
        }
        manifest.push(Saved { path: path.to_path_buf(), existed, backup });
        fs::write(manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        Ok(())
    }

    /// Puts back the files of the newest turn that changed any; returns the turn and the paths.
    pub fn undo(&self) -> Result<Option<(u32, Vec<PathBuf>)>> {
        let root = self.dir.join("checkpoints");
        let Ok(entries) = fs::read_dir(&root) else { return Ok(None) };
        let mut turns: Vec<(u32, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()).map(|n| (n, e.path())))
            .collect();
        turns.sort();
        let Some((turn, dir)) = turns.pop() else { return Ok(None) };
        let manifest: Vec<Saved> = serde_json::from_str(&fs::read_to_string(dir.join("manifest.json"))?)?;
        let mut restored = vec![];
        for s in manifest.iter().rev() {
            if s.existed {
                if let Some(parent) = s.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(dir.join(&s.backup), &s.path)?;
            } else if s.path.is_file() {
                fs::remove_file(&s.path)?;
            }
            restored.push(s.path.clone());
        }
        fs::remove_dir_all(&dir)?;
        Ok(Some((turn, restored)))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    path: PathBuf,
    existed: bool,
    backup: String,
}

/// [history] without assistant tool calls whose results never came (and anything after them).
pub fn without_dangling_calls(mut history: Vec<Message>) -> Vec<Message> {
    loop {
        let Some(pos) = history.iter().rposition(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty())) else {
            return history;
        };
        let Message::Assistant { tool_calls, .. } = &history[pos] else { unreachable!() };
        let answered = tool_calls
            .iter()
            .all(|c| history[pos + 1..].iter().any(|m| matches!(m, Message::Tool { tool_call_id, .. } if *tool_call_id == c.id)));
        if answered {
            return history;
        }
        history.truncate(pos);
    }
}

/// Every session, newest first.
pub fn list(sessions_dir: &Path) -> Vec<Info> {
    let Ok(entries) = fs::read_dir(sessions_dir) else { return vec![] };
    let mut out: Vec<Info> = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let mut info = Info { modified: e.metadata().and_then(|m| m.modified()).ok(), ..Default::default() };
        for line in text.lines() {
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Meta { id, created, cwd, model }) => {
                    info.id = id;
                    info.created = created;
                    info.cwd = cwd;
                    info.model = model;
                }
                Ok(Record::Title { title }) => info.title = title,
                Ok(Record::Msg { m }) => {
                    info.messages += 1;
                    if info.title.is_empty()
                        && let Message::User { content } = &m
                    {
                        info.title = content.chars().take(40).collect();
                    }
                }
                Ok(Record::Replace { history }) => info.messages = history.len(),
                _ => {}
            }
        }
        if !info.id.is_empty() {
            out.push(info);
        }
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.modified));
    out
}

/// Sessions whose title or text holds every word of [query] (case does not matter).
pub fn search(sessions_dir: &Path, query: &str) -> Vec<(Info, String)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return vec![];
    }
    let mut out = vec![];
    for info in list(sessions_dir) {
        let Ok(text) = fs::read_to_string(sessions_dir.join(format!("{}.jsonl", info.id))) else { continue };
        let lower = text.to_lowercase();
        if !words.iter().all(|w| lower.contains(w.as_str()) || info.title.to_lowercase().contains(w.as_str())) {
            continue;
        }
        // the first message that holds the first word, as the hit to show
        let hit = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Record>(l).ok())
            .filter_map(|r| if let Record::Msg { m } = r { Some(m.text().to_string()) } else { None })
            .find(|t| t.to_lowercase().contains(words[0].as_str()))
            .map(|t| {
                let flat = t.split_whitespace().collect::<Vec<_>>().join(" ");
                flat.chars().take(80).collect()
            })
            .unwrap_or_default();
        out.push((info, hit));
    }
    out
}

/// A title from the first message: its first line, cut to [max] characters.
pub fn title_from(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() <= max { line.to_string() } else { format!("{}…", line.chars().take(max).collect::<String>()) }
}

/// The conversation as Markdown (for `/export`).
pub fn to_markdown(title: &str, history: &[Message]) -> String {
    let mut b = format!("# {}\n\n", if title.is_empty() { "ARCE.AI" } else { title });
    for m in history {
        match m {
            Message::User { content } => b.push_str(&format!("## ユーザー\n\n{content}\n\n")),
            Message::Assistant { content, tool_calls, .. } => {
                if !content.trim().is_empty() {
                    b.push_str(&format!("## ARCE.AI\n\n{content}\n\n"));
                }
                for c in tool_calls {
                    b.push_str(&format!("> 道具 `{}` {}\n\n", c.name, c.arguments.chars().take(200).collect::<String>()));
                }
            }
            Message::Tool { name, content, .. } => {
                let head: String = content.chars().take(600).collect();
                b.push_str(&format!("<details><summary>{name} の結果</summary>\n\n```\n{head}\n```\n\n</details>\n\n"));
            }
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    #[test]
    fn a_session_reads_back_with_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path(), Path::new("/w"), "demo|m", false).unwrap();
        s.push(Message::user("hello")).unwrap();
        s.push(Message::Assistant { content: "hi".into(), reasoning: None, tool_calls: vec![] }).unwrap();
        s.replace(vec![Message::user("summary")]).unwrap();
        s.push(Message::user("again")).unwrap();
        s.set_title("題").unwrap();
        let back = Session::open(dir.path(), &s.id).unwrap();
        assert_eq!(back.history, vec![Message::user("summary"), Message::user("again")]);
        assert_eq!(back.title, "題");
        assert_eq!(Session::latest(dir.path(), Path::new("/w")), Some(s.id.clone()));
        assert_eq!(list(dir.path())[0].title, "題");
        assert_eq!(search(dir.path(), "AGAIN").len(), 1);
        assert!(search(dir.path(), "again nothing").is_empty());
    }

    #[test]
    fn undo_puts_back_changed_and_removes_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("w");
        fs::create_dir_all(&work).unwrap();
        let old = work.join("old.txt");
        let new = work.join("sub/new.txt");
        fs::write(&old, "before").unwrap();
        let mut s = Session::create(&dir.path().join("s"), &work, "m", false).unwrap();
        s.next_turn().unwrap();
        s.checkpoint(&old).unwrap();
        s.checkpoint(&new).unwrap();
        fs::write(&old, "after").unwrap();
        s.checkpoint(&old).unwrap(); // a second change in the same turn keeps the first copy
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&new, "x").unwrap();
        let (turn, paths) = s.undo().unwrap().unwrap();
        assert_eq!(turn, 1);
        assert_eq!(paths.len(), 2);
        assert_eq!(fs::read_to_string(&old).unwrap(), "before");
        assert!(!new.exists());
        assert!(s.undo().unwrap().is_none());
    }

    #[test]
    fn dangling_calls_are_dropped() {
        let call = |id: &str| ToolCall { id: id.into(), name: "x".into(), arguments: "{}".into(), extra: None };
        let h = vec![
            Message::user("a"),
            Message::Assistant { content: String::new(), reasoning: None, tool_calls: vec![call("1")] },
            Message::Tool { tool_call_id: "1".into(), name: "x".into(), content: "r".into() },
            Message::Assistant { content: String::new(), reasoning: None, tool_calls: vec![call("2"), call("3")] },
            Message::Tool { tool_call_id: "2".into(), name: "x".into(), content: "r".into() },
        ];
        assert_eq!(without_dangling_calls(h).len(), 3);
    }

    #[test]
    fn titles_are_the_first_line() {
        assert_eq!(title_from("\n  直して\nほか", 10), "直して");
        assert_eq!(title_from("abcdefghijk", 3), "abc…");
    }
}

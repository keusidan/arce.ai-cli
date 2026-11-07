//! The split of a job as a graph: what has to be done (nodes) and what has to come first (arrows).
//!
//! The planner writes it as a Mermaid flowchart: a form a model writes well, a person reads at a
//! glance, and other tools take as it is. Only the part that says "this, then that" is read:
//! `flowchart`/`graph`, nodes (`a["title"]`, `a[title]`, `a(title)`, `a{title}`, `a`) and arrows
//! (`-->`, `---`, `==>`, `-.->`, with or without a label, `a --> b & c`, `a --> b --> c`). Styling,
//! subgraphs and the rest are skipped. An arrow `a --> b` means b starts after a.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Debug, PartialEq)]
pub struct PlanGraph {
    /// node id -> its title ('' when none), in the order first seen
    pub labels: Vec<(String, String)>,
    /// (from, to): `to` starts after `from`
    pub edges: Vec<(String, String)>,
}

static ARROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:-{2,}>|={2,}>|-\.+->|-{3,}|--[xo]|~~~)").unwrap());
static LABELLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"--\s+[^>\-][^>]*?\s+-->").unwrap());
static PIPE_LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|[^|]*\|").unwrap());
static SKIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(classdef|class|style|linkstyle|click|direction)\b").unwrap());
static NODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^([A-Za-z0-9_]+)\s*(?:\[\s*"?(.*?)"?\s*\]|\(\s*"?(.*?)"?\s*\)|\{\s*"?(.*?)"?\s*\}|>\s*"?(.*?)"?\s*\])?\s*(?::::\w+)?$"#)
        .unwrap()
});

impl PlanGraph {
    /// Reads [text]; None when no node is found in it.
    pub fn parse(text: &str) -> Option<PlanGraph> {
        let mut labels: Vec<(String, String)> = vec![];
        let mut edges: Vec<(String, String)> = vec![];
        let mut seen = BTreeSet::new();
        let set_label = |labels: &mut Vec<(String, String)>, id: &str, label: &str| match labels.iter_mut().find(|(i, _)| i == id) {
            Some(entry) => {
                if !label.is_empty() {
                    entry.1 = label.to_string();
                }
            }
            None => labels.push((id.to_string(), label.to_string())),
        };
        for raw in text.split(['\n', ';']) {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("%%") || line.starts_with("```") {
                continue;
            }
            let lower = line.to_lowercase();
            if lower.starts_with("flowchart") || lower.starts_with("graph ") || lower == "graph" {
                continue;
            }
            if lower.starts_with("subgraph") || lower == "end" || SKIP.is_match(line) {
                continue;
            }
            let line = LABELLED.replace_all(line, "-->");
            let line = PIPE_LABEL.replace_all(&line, " ");
            let mut previous: Option<Vec<String>> = None;
            for part in ARROW.split(&line) {
                let mut ids = vec![];
                for piece in split_and(part) {
                    if let Some((id, label)) = node(piece.trim()) {
                        set_label(&mut labels, &id, &label);
                        ids.push(id);
                    }
                }
                if let Some(prev) = &previous {
                    for from in prev {
                        for to in &ids {
                            if from != to && seen.insert(format!("{from}>{to}")) {
                                edges.push((from.clone(), to.clone()));
                            }
                        }
                    }
                }
                if !ids.is_empty() {
                    previous = Some(ids);
                }
            }
        }
        (!labels.is_empty()).then_some(PlanGraph { labels, edges })
    }

    pub fn ids(&self) -> Vec<&str> {
        self.labels.iter().map(|(i, _)| i.as_str()).collect()
    }

    pub fn label(&self, id: &str) -> &str {
        self.labels.iter().find(|(i, _)| i == id).map_or("", |(_, l)| l.as_str())
    }

    /// The ids a node waits for.
    pub fn waits_for(&self, id: &str) -> Vec<String> {
        self.edges.iter().filter(|(_, t)| t == id).map(|(f, _)| f.clone()).collect()
    }

    /// A cycle when there is one (the ids on it), else None.
    pub fn cycle(&self) -> Option<Vec<String>> {
        let ids = self.ids();
        let mut indegree: BTreeMap<&str, usize> = ids.iter().map(|i| (*i, 0)).collect();
        for (_, t) in &self.edges {
            *indegree.entry(t.as_str()).or_default() += 1;
        }
        let mut queue: VecDeque<&str> = indegree.iter().filter(|(_, d)| **d == 0).map(|(i, _)| *i).collect();
        let mut done = 0;
        while let Some(n) = queue.pop_front() {
            done += 1;
            for (f, t) in &self.edges {
                if f == n {
                    let d = indegree.get_mut(t.as_str()).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(t.as_str());
                    }
                }
            }
        }
        if done == indegree.len() {
            return None;
        }
        Some(indegree.into_iter().filter(|(_, d)| *d > 0).map(|(i, _)| i.to_string()).collect())
    }

    /// A Mermaid flowchart of tasks and their dependencies (when the planner did not draw one).
    pub fn from_tasks(tasks: &[(String, String, Vec<String>)]) -> String {
        let mut b = String::from("flowchart TD\n");
        for (id, title, _) in tasks {
            b.push_str(&format!("    {id}[\"{}\"]\n", title.replace('"', "'")));
        }
        for (id, _, deps) in tasks {
            for d in deps {
                b.push_str(&format!("    {d} --> {id}\n"));
            }
        }
        b
    }
}

/// `b & c` -> `b`, `c`, without splitting inside quotes or brackets.
fn split_and(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0i32;
    let mut quoted = false;
    let mut cur = String::new();
    for ch in text.chars() {
        match ch {
            '"' => quoted = !quoted,
            '[' | '(' | '{' if !quoted => depth += 1,
            ']' | ')' | '}' if !quoted => depth -= 1,
            '&' if !quoted && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}

fn node(text: &str) -> Option<(String, String)> {
    if text.is_empty() {
        return None;
    }
    let c = NODE.captures(text)?;
    let id = c.get(1)?.as_str().to_string();
    let label = (2..=5).find_map(|i| c.get(i)).map_or("", |m| m.as_str()).trim().to_string();
    Some((id, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nodes_arrows_chains_and_groups() {
        let g = PlanGraph::parse(
            "```mermaid\nflowchart TD\n  a[\"Model\"] --> d[\"Screen\"]\n  b[API] & c(Settings) --> d\n  d -->|then| e{Tests}\n  style a fill:#fff\n```",
        )
        .unwrap();
        assert_eq!(g.ids(), vec!["a", "d", "b", "c", "e"]);
        assert_eq!(g.label("c"), "Settings");
        assert_eq!(g.label("e"), "Tests");
        assert_eq!(g.waits_for("d"), vec!["a", "b", "c"]);
        assert_eq!(g.waits_for("e"), vec!["d"]);
        assert!(g.cycle().is_none());
    }

    #[test]
    fn labelled_arrows_and_chains() {
        let g = PlanGraph::parse("graph LR\na -- uses --> b --> c\nc -.-> d\nd ==> e").unwrap();
        assert_eq!(g.edges.len(), 4);
        assert_eq!(g.waits_for("e"), vec!["d"]);
    }

    #[test]
    fn finds_cycles() {
        let g = PlanGraph::parse("flowchart TD\na --> b\nb --> c\nc --> a\nx").unwrap();
        let mut c = g.cycle().unwrap();
        c.sort();
        assert_eq!(c, vec!["a", "b", "c"]);
    }

    #[test]
    fn nothing_readable_is_none() {
        assert!(PlanGraph::parse("flowchart TD\n%% nothing").is_none());
    }

    #[test]
    fn draws_tasks_back_into_mermaid() {
        let text = PlanGraph::from_tasks(&[("a".into(), "A".into(), vec![]), ("b".into(), "B \"x\"".into(), vec!["a".into()])]);
        let g = PlanGraph::parse(&text).unwrap();
        assert_eq!(g.waits_for("b"), vec!["a"]);
        assert_eq!(g.label("b"), "B 'x'");
    }
}

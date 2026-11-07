//! Agents are roles, not models: extra instructions, the tools they may call, and the words that
//! suggest them. The CLI picks one per message from the words (attachments steer towards file
//! analysis); the user can always pin one.

#[derive(Debug)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub instructions: &'static str,
    /// the tools it may use; `*` for every tool
    pub tools: &'static [&'static str],
    pub keywords: &'static [&'static str],
}

const READ_TOOLS: &[&str] = &["read_file", "list_dir", "glob", "grep", "ask_user", "update_plan", "get_settings", "update_settings"];

pub static GENERAL: Agent = Agent {
    id: "general",
    name: "General Chat",
    instructions: "Answer conversationally. Use tools only when they are clearly needed.",
    tools: &[
        "read_file",
        "list_dir",
        "glob",
        "grep",
        "ask_user",
        "web_search",
        "http_request",
        "get_settings",
        "update_settings",
        "update_plan",
    ],
    keywords: &[],
};

pub static CODING: Agent = Agent {
    id: "coding",
    name: "Coding",
    instructions: "You are working in the user's repository. Read the code before changing it, keep changes small and in the style of the surrounding code, and prefer edit_file over rewriting whole files. Run the project's own build and tests with run_command when they exist and report what passed. Ask with ask_user when a key choice (language, platform, scope) is missing instead of guessing.",
    tools: &["*"],
    keywords: &[
        "作って",
        "実装",
        "コード",
        "プログラム",
        "関数",
        "バグ",
        "エラー",
        "直して",
        "修正",
        "テスト",
        "ビルド",
        "リファクタ",
        "code",
        "implement",
        "function",
        "bug",
        "fix",
        "test",
        "build",
        "refactor",
        "compile",
        "kotlin",
        "python",
        "java",
        "javascript",
        "typescript",
        "rust",
        "c++",
        "go ",
        "script",
        "スクリプト",
        "cargo",
        "npm",
        "make",
    ],
};

pub static RESEARCH: Agent = Agent {
    id: "research",
    name: "Research / Search",
    instructions: "Research the question. Search first, compare sources, and cite the URLs you rely on.",
    tools: &[
        "web_search",
        "http_request",
        "read_file",
        "list_dir",
        "glob",
        "grep",
        "ask_user",
        "get_settings",
        "update_settings",
        "update_plan",
    ],
    keywords: &["調べ", "検索", "最新", "ニュース", "比較", "ソース", "出典", "search", "latest", "news", "compare", "source"],
};

pub static ARTIFACT: Agent = Agent {
    id: "artifact",
    name: "Artifact",
    instructions: "Build a self-contained web page. Put all HTML, CSS and JavaScript in ONE file (write it with write_file when the user wants a file, else one ```html block); do not load external scripts. Add a short usage note.",
    tools: &["write_file", "read_file", "list_dir", "ask_user", "get_settings", "update_settings", "update_plan"],
    keywords: &[
        "html",
        "css",
        "webページ",
        "ウェブページ",
        "サイト",
        "ゲーム",
        "プレビュー",
        "artifact",
        "アーティファクト",
        "page",
        "game",
    ],
};

pub static FILE_ANALYSIS: Agent = Agent {
    id: "file-analysis",
    name: "File Analysis",
    instructions: "The user attached or pointed at files. Base the answer on their contents and quote the relevant parts.",
    tools: READ_TOOLS,
    keywords: &["ファイル", "添付", "要約", "解析", "読んで", "file", "attached", "summarize", "analyze"],
};

pub fn all() -> [&'static Agent; 5] {
    [&GENERAL, &CODING, &RESEARCH, &ARTIFACT, &FILE_ANALYSIS]
}

pub fn by_id(id: &str) -> Option<&'static Agent> {
    all().into_iter().find(|a| a.id == id)
}

impl Agent {
    pub fn allows(&self, tool: &str) -> bool {
        self.tools.contains(&"*")
            || self.tools.contains(&tool)
            || (tool.starts_with("mcp_") && self.tools.iter().any(|t| *t == "*" || *t == "http_request"))
    }
}

/// Agents with at least one hit, best first; ties keep the order of [all].
pub fn rank(request: &str, has_attachments: bool) -> Vec<(&'static Agent, usize)> {
    let text = request.to_lowercase();
    let mut scored: Vec<(usize, &'static Agent, usize)> = all()
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            let hits =
                a.keywords.iter().filter(|k| text.contains(*k)).count() + if has_attachments && a.id == FILE_ANALYSIS.id { 2 } else { 0 };
            (i, a, hits)
        })
        .filter(|(_, _, h)| *h > 0)
        .collect();
    scored.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(_, a, h)| (a, h)).collect()
}

/// The agent for [request]: the pinned one, else the best by words, else [fallback] (the agent of the
/// session so far, so a short follow-up such as "続けて" keeps the role).
pub fn select(request: &str, has_attachments: bool, pinned: &str, fallback: Option<&'static Agent>) -> &'static Agent {
    if pinned != "auto"
        && let Some(a) = by_id(pinned)
    {
        return a;
    }
    match rank(request, has_attachments).first() {
        Some((a, _)) => a,
        None => fallback.unwrap_or(&CODING),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_pick_the_agent() {
        assert_eq!(select("このバグを直して", false, "auto", None).id, "coding");
        assert_eq!(select("最新のニュースを調べて", false, "auto", None).id, "research");
        assert_eq!(select("これを読んで", true, "auto", None).id, "file-analysis");
        assert_eq!(select("こんにちは", false, "auto", Some(&GENERAL)).id, "general");
    }

    #[test]
    fn a_pinned_agent_wins() {
        assert_eq!(select("バグ", false, "research", None).id, "research");
    }

    #[test]
    fn ties_keep_the_listed_order() {
        // one coding word, one research word: coding is listed first
        assert_eq!(select("code search", false, "auto", None).id, "coding");
    }

    #[test]
    fn coding_allows_everything_and_research_cannot_write() {
        assert!(CODING.allows("run_command"));
        assert!(!RESEARCH.allows("write_file"));
        assert!(RESEARCH.allows("mcp_x__y"));
        assert!(!FILE_ANALYSIS.allows("mcp_x__y"));
    }
}

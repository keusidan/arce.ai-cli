//! Makes room in a long conversation: first the old tool answers are cut short, then everything
//! before the last few messages of the user is replaced by a summary the model writes.
//!
//! The pure part is here (where to cut, what to ask, how to put the summary back); asking the model
//! is the controller's. The summary goes into the first message that is kept, so the history still
//! alternates and a provider never sees two user messages in a row because of it. The cut is always
//! at a message of the user, so a tool call is never separated from its answer.

use crate::llm::Message;

/// Notes the CLI writes into the history for the model start with this.
pub const FEEDBACK_PREFIX: &str = "[from the app]";

pub const SUMMARY_HEADER: &str = "[Summary of the earlier conversation. The app wrote it so the conversation fits the context window; treat it as what was said before.]";
pub const CURRENT_MESSAGE_HEADER: &str = "[The user's message that follows the summary]";

pub const INSTRUCTION: &str = "You summarize the earlier part of a conversation so that it can continue without it. Write in the language the user wrote in. Keep what the conversation still needs: what the user wants, decisions made and why, names of files, functions, settings and commands, numbers, errors that were found and how they were fixed, and what is still to be done. Leave out greetings and anything that was only a step on the way. Reply with the summary only.";

/// A rough count of the tokens in [text]: a third of its UTF-8 bytes.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(|m| (m.wire_len() as u64).div_ceil(3) + 4).sum()
}

fn is_user_message(m: &Message) -> bool {
    matches!(m, Message::User { content } if !content.starts_with(FEEDBACK_PREFIX))
}

/// Where the kept part starts: the [keep_turns]-th last message of the user. None when there is
/// nothing before it to summarize.
pub fn cut_index(history: &[Message], keep_turns: usize) -> Option<usize> {
    let keep = keep_turns.max(1);
    let mut seen = 0;
    for i in (0..history.len()).rev() {
        if !is_user_message(&history[i]) {
            continue;
        }
        seen += 1;
        if seen == keep {
            return (i > 0).then_some(i);
        }
    }
    None
}

/// [history] with the tool answers before [up_to] cut to [max_chars] each. None when nothing was long
/// enough to cut.
pub fn clip_tool_results(history: &[Message], up_to: usize, max_chars: usize) -> Option<Vec<Message>> {
    if max_chars == 0 {
        return None;
    }
    let mut changed = false;
    let out = history
        .iter()
        .enumerate()
        .map(|(i, m)| match m {
            Message::Tool { tool_call_id, name, content } if i < up_to && content.chars().count() > max_chars + 80 => {
                changed = true;
                let total = content.chars().count();
                let head: String = content.chars().take(max_chars).collect();
                Message::Tool {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    content: format!("{head}\n[… {} characters left out to save room]", total - max_chars),
                }
            }
            other => other.clone(),
        })
        .collect();
    changed.then_some(out)
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { format!("{} […]", text.chars().take(max).collect::<String>()) }
}

/// The turns as plain text for the model that summarizes them.
pub fn lines(turns: &[Message], turn_chars: usize, tool_chars: usize) -> Vec<String> {
    let mut out = vec![];
    for t in turns {
        match t {
            Message::User { content } => out.push(format!("User: {}", cut(content.trim(), turn_chars))),
            Message::Assistant { content, tool_calls, .. } => {
                let mut parts = vec![];
                if !content.trim().is_empty() {
                    parts.push(cut(content.trim(), turn_chars));
                }
                for c in tool_calls {
                    let flat = format!("{} {}", c.name, c.arguments.split_whitespace().collect::<Vec<_>>().join(" "));
                    parts.push(format!("[called a tool: {}]", cut(&flat, 240)));
                }
                if !parts.is_empty() {
                    out.push(format!("Assistant: {}", parts.join(" ")));
                }
            }
            Message::Tool { content, .. } => out.push(format!("Tool result: {}", cut(content.trim(), tool_chars))),
        }
    }
    out
}

/// [lines] in groups of at most [max_chars] characters.
pub fn chunks(lines: &[String], max_chars: usize) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for line in lines {
        if !cur.is_empty() && cur.chars().count() + line.chars().count() + 1 > max_chars {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// What the summarizing model is asked for one group; [before] is the summary so far.
pub fn request(group: &str, before: &str, max_chars: usize) -> String {
    let mut parts = vec![];
    if !before.trim().is_empty() {
        parts.push(format!("Summary so far:\n{}\n", before.trim()));
    }
    parts.push(format!("Conversation to add to the summary:\n{group}\n"));
    parts.push(format!("Write the whole summary again, now including the conversation above. At most {max_chars} characters."));
    parts.join("\n")
}

/// [history] with everything before [cut] replaced by [summary], written into the first kept message.
pub fn replace(history: &[Message], cut: usize, summary: &str) -> Vec<Message> {
    let text = history[cut].text();
    let mut out = vec![Message::user(format!("{SUMMARY_HEADER}\n{}\n\n{CURRENT_MESSAGE_HEADER}\n{text}", summary.trim()))];
    out.extend_from_slice(&history[cut + 1..]);
    out
}

/// A model's answer as the summary: thinking and blanks removed; None when nothing is left.
pub fn clean(answer: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?s)<think>.*?</think>").unwrap();
    let t = re.replace_all(answer, "").trim().to_string();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    fn convo() -> Vec<Message> {
        vec![
            Message::user("one"),
            Message::Assistant {
                content: "a".into(),
                reasoning: None,
                tool_calls: vec![ToolCall { id: "1".into(), name: "grep".into(), arguments: "{}".into(), extra: None }],
            },
            Message::Tool { tool_call_id: "1".into(), name: "grep".into(), content: "x".repeat(500) },
            Message::user(format!("{FEEDBACK_PREFIX} note")),
            Message::user("two"),
            Message::Assistant { content: "b".into(), reasoning: None, tool_calls: vec![] },
            Message::user("three"),
        ]
    }

    #[test]
    fn cut_is_at_a_real_user_message() {
        let h = convo();
        assert_eq!(cut_index(&h, 2), Some(4));
        assert_eq!(cut_index(&h, 3), None);
        assert_eq!(cut_index(&h, 1), Some(6));
    }

    #[test]
    fn clips_only_old_long_tool_results() {
        let h = convo();
        let c = clip_tool_results(&h, 4, 100).unwrap();
        assert!(c[2].text().starts_with(&"x".repeat(100)));
        assert!(c[2].text().contains("400 characters left out"));
        assert!(clip_tool_results(&h, 2, 100).is_none());
    }

    #[test]
    fn summary_goes_into_the_first_kept_message() {
        let h = convo();
        let r = replace(&h, 4, "sum");
        assert_eq!(r.len(), 3);
        assert!(r[0].text().starts_with(SUMMARY_HEADER));
        assert!(r[0].text().ends_with("two"));
    }

    #[test]
    fn lines_and_chunks() {
        let l = lines(&convo(), 100, 10);
        assert!(l[1].contains("[called a tool: grep {}]"));
        assert!(l[2].ends_with("[…]"));
        let c = chunks(&["aaaa".into(), "bbbb".into(), "cccc".into()], 9);
        assert_eq!(c, vec!["aaaa\nbbbb", "cccc"]);
    }

    #[test]
    fn clean_drops_thinking() {
        assert_eq!(clean("<think>x</think>\n sum "), Some("sum".into()));
        assert_eq!(clean("<think>x</think>"), None);
    }

    #[test]
    fn estimate_is_a_third_of_bytes() {
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("日本"), 2);
    }
}

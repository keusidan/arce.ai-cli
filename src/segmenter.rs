//! Splits streamed model text into thinking, plain text and tool calls written as tags.
//!
//! Servers without function calling get the text form (`<tool_call>{json}</tool_call>`), and some
//! servers put the thinking inline (`<think>…</think>`). A tag may be cut between two pieces of the
//! stream, so the tail of a piece that could be the start of a tag is held until the next one.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Thinking(String),
    /// the whole JSON of one call
    ToolCall(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Text,
    Think,
    Tool,
}

pub struct Segmenter {
    state: State,
    held: String,
    tool: String,
    /// whether `<tool_call>` is read as a tag (text tool mode)
    tools_as_text: bool,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";
const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";

impl Segmenter {
    pub fn new(tools_as_text: bool) -> Segmenter {
        Segmenter { state: State::Text, held: String::new(), tool: String::new(), tools_as_text }
    }

    fn tags(&self) -> Vec<&'static str> {
        match self.state {
            State::Text => {
                if self.tools_as_text {
                    vec![THINK_OPEN, TOOL_OPEN]
                } else {
                    vec![THINK_OPEN]
                }
            }
            State::Think => vec![THINK_CLOSE],
            State::Tool => vec![TOOL_CLOSE],
        }
    }

    fn emit(&mut self, text: &str, out: &mut Vec<Piece>) {
        if text.is_empty() {
            return;
        }
        match self.state {
            State::Text => push(out, Piece::Text(text.to_string())),
            State::Think => push(out, Piece::Thinking(text.to_string())),
            State::Tool => self.tool.push_str(text),
        }
    }

    pub fn push(&mut self, chunk: &str) -> Vec<Piece> {
        let mut out = vec![];
        let mut buf = std::mem::take(&mut self.held);
        buf.push_str(chunk);
        loop {
            let tags = self.tags();
            let found = tags.iter().filter_map(|t| buf.find(t).map(|i| (i, *t))).min_by_key(|(i, _)| *i);
            match found {
                Some((i, tag)) => {
                    let before = buf[..i].to_string();
                    self.emit(&before, &mut out);
                    buf = buf[i + tag.len()..].to_string();
                    match tag {
                        THINK_OPEN => self.state = State::Think,
                        TOOL_OPEN => {
                            self.state = State::Tool;
                            self.tool.clear();
                        }
                        THINK_CLOSE => self.state = State::Text,
                        TOOL_CLOSE => {
                            let call = self.tool.trim().to_string();
                            self.tool.clear();
                            self.state = State::Text;
                            if !call.is_empty() {
                                out.push(Piece::ToolCall(call));
                            }
                        }
                        _ => {}
                    }
                }
                None => {
                    // hold a tail that may be the start of a tag
                    let mut keep = 0;
                    for tag in &tags {
                        for k in (1..tag.len()).rev() {
                            if k <= buf.len() && buf.is_char_boundary(buf.len() - k) && buf.ends_with(&tag[..k]) {
                                keep = keep.max(k);
                                break;
                            }
                        }
                    }
                    let split = buf.len() - keep;
                    let now = buf[..split].to_string();
                    self.held = buf[split..].to_string();
                    self.emit(&now, &mut out);
                    break;
                }
            }
        }
        out
    }

    /// The rest at the end of the stream; a tool call that was never closed is returned as a call (the
    /// reader decides whether its JSON is whole).
    pub fn finish(&mut self) -> Vec<Piece> {
        let mut out = vec![];
        let held = std::mem::take(&mut self.held);
        self.emit(&held, &mut out);
        if self.state == State::Tool && !self.tool.trim().is_empty() {
            out.push(Piece::ToolCall(self.tool.trim().to_string()));
            self.tool.clear();
        }
        self.state = State::Text;
        out
    }

    pub fn in_tool_call(&self) -> bool {
        self.state == State::Tool
    }
}

fn push(out: &mut Vec<Piece>, piece: Piece) {
    match (out.last_mut(), &piece) {
        (Some(Piece::Text(a)), Piece::Text(b)) => a.push_str(b),
        (Some(Piece::Thinking(a)), Piece::Thinking(b)) => a.push_str(b),
        _ => out.push(piece),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str], tools: bool) -> Vec<Piece> {
        let mut s = Segmenter::new(tools);
        let mut all = vec![];
        for c in chunks {
            for p in s.push(c) {
                push(&mut all, p);
            }
        }
        for p in s.finish() {
            push(&mut all, p);
        }
        all
    }

    #[test]
    fn splits_thinking_text_and_calls_across_chunks() {
        let out = run(&["<thi", "nk>hmm</th", "ink>Hello <tool", "_call>{\"name\":\"x\"}</tool_c", "all> bye"], true);
        assert_eq!(
            out,
            vec![
                Piece::Thinking("hmm".into()),
                Piece::Text("Hello ".into()),
                Piece::ToolCall("{\"name\":\"x\"}".into()),
                Piece::Text(" bye".into()),
            ]
        );
    }

    #[test]
    fn tool_tags_are_text_when_tools_are_native() {
        let out = run(&["a <tool_call>b</tool_call>"], false);
        assert_eq!(out, vec![Piece::Text("a <tool_call>b</tool_call>".into())]);
    }

    #[test]
    fn a_lone_angle_bracket_is_released_at_the_end() {
        assert_eq!(run(&["x <"], true), vec![Piece::Text("x <".into())]);
    }

    #[test]
    fn unclosed_call_is_returned_at_the_end() {
        assert_eq!(run(&["<tool_call>{\"a\":1}"], true), vec![Piece::ToolCall("{\"a\":1}".into())]);
    }

    #[test]
    fn multibyte_text_is_not_split_inside_a_character() {
        assert_eq!(
            run(&["日本語<", "think>考</think>です"], false),
            vec![Piece::Text("日本語".into()), Piece::Thinking("考".into()), Piece::Text("です".into())]
        );
    }
}

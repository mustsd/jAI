//! Turns a stream of [`ChatEvent`]s into a finished assistant turn.
//!
//! Providers split text and tool-call arguments across SSE frames at arbitrary
//! byte boundaries, including in the middle of a multi-byte UTF-8 character.
//! This module therefore never slices `str` by byte offset without checking
//! char boundaries — it appends decoded fragments only.

use crate::message::{ChatEvent, TokenUsage, ToolCall, normalize_tool_name};

/// Final result of one assistant response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Incremental accumulator; feed it every event, then call
/// [`Accumulator::into_turn`].
#[derive(Debug, Default)]
pub struct Accumulator {
    text: String,
    calls: Vec<PartialCall>,
    usage: TokenUsage,
    finished: bool,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one provider event.
    pub fn apply(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::Delta(text) => self.push_text(text),
            ChatEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => self.push_tool_delta(
                *index,
                id.as_deref(),
                name.as_deref(),
                arguments_delta.as_deref(),
            ),
            ChatEvent::Usage(usage) => self.usage.add(usage),
            ChatEvent::Finished => self.finished = true,
        }
    }

    pub fn push_text(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Merge one tool-call fragment.
    ///
    /// `name` and `arguments` are appended because providers may split them;
    /// `id` is taken from the first fragment that carries one.
    pub fn push_tool_delta(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) {
        while self.calls.len() <= index {
            self.calls.push(PartialCall::default());
        }
        let call = &mut self.calls[index];
        if let Some(id) = id {
            if !id.is_empty() && call.id.is_none() {
                call.id = Some(id.to_string());
            }
        }
        if let Some(name) = name {
            call.name.push_str(name);
        }
        if let Some(arguments) = arguments {
            call.arguments.push_str(arguments);
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// True when the provider produced nothing at all.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.calls.iter().all(|call| call.name.is_empty())
    }

    /// Finalise the turn.
    pub fn into_turn(self) -> AssistantTurn {
        let mut tool_calls = Vec::new();
        for (index, call) in self.calls.into_iter().enumerate() {
            let raw = call.name.trim();
            if raw.is_empty() {
                continue;
            }
            // Models sometimes echo the legacy `filesystem.read` spelling (or
            // invent a name); normalise so the registry and the wire agree.
            let name = normalize_tool_name(raw);
            let id = match call.id {
                Some(id) if !id.is_empty() => id,
                // Providers always send an id, but never trust that blindly:
                // the tool result must reference something.
                _ => format!("call_{index}"),
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments: call.arguments,
            });
        }
        AssistantTurn {
            text: self.text,
            tool_calls,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ChatEvent {
        ChatEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            arguments_delta: args.map(str::to_string),
        }
    }

    #[test]
    fn text_deltas_accumulate_in_order() {
        let mut acc = Accumulator::new();
        acc.apply(&ChatEvent::Delta("Hel".to_string()));
        acc.apply(&ChatEvent::Delta("lo ".to_string()));
        acc.apply(&ChatEvent::Delta("世界".to_string()));
        let turn = acc.into_turn();
        assert_eq!(turn.text, "Hello 世界");
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn multibyte_fragments_survive_the_round_trip() {
        // A provider may split a 3-byte character across two deltas; because we
        // only ever append decoded strings, the result is correct.
        let mut acc = Accumulator::new();
        acc.push_text("实现");
        acc.push_text("一个 agent");
        assert_eq!(acc.text(), "实现一个 agent");
    }

    #[test]
    fn tool_call_fragments_merge_by_index() {
        let mut acc = Accumulator::new();
        acc.apply(&delta(0, Some("call_1"), Some("filesystem."), None));
        acc.apply(&delta(0, None, Some("read"), Some("{\"path\":")));
        acc.apply(&delta(0, None, None, Some("\"Cargo.toml\"}")));
        acc.apply(&delta(
            1,
            Some("call_2"),
            Some("shell.execute"),
            Some("{\"command\":\"ls\"}"),
        ));
        let turn = acc.into_turn();
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        // The fragments arrived as the legacy `filesystem.` + `read`.
        assert_eq!(turn.tool_calls[0].name, "filesystem_read");
        assert_eq!(turn.tool_calls[0].arguments, "{\"path\":\"Cargo.toml\"}");
        assert_eq!(turn.tool_calls[1].name, "shell_execute");
        assert_eq!(
            turn.tool_calls[0].arguments_value().unwrap()["path"],
            "Cargo.toml"
        );
    }

    #[test]
    fn missing_ids_are_synthesised_and_nameless_calls_dropped() {
        let mut acc = Accumulator::new();
        acc.apply(&delta(0, None, Some("filesystem.list"), Some("{}")));
        acc.apply(&delta(1, None, None, Some("{}")));
        let turn = acc.into_turn();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_0");
    }

    #[test]
    fn usage_and_finish_flags_are_tracked() {
        let mut acc = Accumulator::new();
        acc.apply(&ChatEvent::Usage(TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        }));
        acc.apply(&ChatEvent::Usage(TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
        }));
        assert!(!acc.is_finished());
        acc.apply(&ChatEvent::Finished);
        assert!(acc.is_finished());
        assert_eq!(acc.usage().total_tokens, 5);
    }

    #[test]
    fn empty_turns_are_reported() {
        assert!(Accumulator::new().is_empty());
        let mut acc = Accumulator::new();
        acc.push_text("x");
        assert!(!acc.is_empty());
    }

    #[test]
    fn text_and_tool_calls_can_arrive_together() {
        let mut acc = Accumulator::new();
        acc.push_text("Let me look.");
        acc.apply(&delta(
            0,
            Some("call_9"),
            Some("filesystem.read"),
            Some("{}"),
        ));
        let turn = acc.into_turn();
        assert_eq!(turn.text, "Let me look.");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "filesystem_read");
    }

    #[test]
    fn unwireable_tool_names_are_sanitised() {
        let mut acc = Accumulator::new();
        acc.apply(&delta(0, Some("call_1"), Some("网络 请求"), Some("{}")));
        assert!(!acc.is_empty());
        let turn = acc.into_turn();
        // The name is unusable as-is, so it is mapped to the safe pattern.
        assert_eq!(turn.tool_calls[0].name, "_____");
    }
}

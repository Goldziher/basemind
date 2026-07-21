//! Conversation history and token accounting.
//!
//! Holds the ordered message list plus an optional system prompt, and accumulates token usage
//! across turns. On-disk persistence lives in [`persist`]; compaction lands in a later slice. For
//! now this is the minimal store the turn-loop needs to build requests and report cumulative usage.

mod persist;

pub use persist::{SessionMeta, SessionStore};

use liter_llm::{Message, SystemMessage, Usage, UserContent, UserMessage};

/// The running conversation for one session.
pub struct History {
    system: Option<String>,
    messages: Vec<Message>,
    input_tokens: u64,
    output_tokens: u64,
}

impl History {
    /// A new history with an optional system prompt.
    pub fn new(system: Option<String>) -> Self {
        Self {
            system,
            messages: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Append a message verbatim (assistant/tool messages from the turn-loop).
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// The conversation messages (excluding the system prompt), in order.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Restore a persisted conversation: append `messages` and set the cumulative token totals.
    ///
    /// Used on resume; the system prompt supplied to [`History::new`] is preserved.
    pub fn restore(&mut self, messages: Vec<Message>, input_tokens: u64, output_tokens: u64) {
        self.messages.extend(messages);
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
    }

    /// Append a user message.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            name: None,
        }));
    }

    /// The full message list to send to the model: the system prompt (if any) prepended to the
    /// conversation.
    pub fn to_messages(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if let Some(system) = &self.system {
            out.push(Message::System(SystemMessage {
                content: UserContent::Text(system.clone()),
                name: None,
            }));
        }
        out.extend(self.messages.iter().cloned());
        out
    }

    /// Fold a usage delta into the running totals; returns cumulative `(input, output)` tokens.
    pub fn add_usage(&mut self, usage: &Usage) -> (u64, u64) {
        self.input_tokens = self.input_tokens.saturating_add(usage.prompt_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.completion_tokens);
        self.totals()
    }

    /// Cumulative `(input, output)` token totals for the session.
    pub fn totals(&self) -> (u64, u64) {
        (self.input_tokens, self.output_tokens)
    }

    /// Number of conversation messages (excluding the system prompt).
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the conversation is empty (excluding the system prompt).
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(message: &Message) -> Option<&str> {
        match message {
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn to_messages_prepends_the_system_prompt() {
        let mut history = History::new(Some("you are a coding agent".into()));
        history.push_user("hello");
        let messages = history.to_messages();
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::System(_)));
        assert_eq!(user_text(&messages[1]), Some("hello"));
    }

    #[test]
    fn without_a_system_prompt_only_conversation_messages_are_sent() {
        let mut history = History::new(None);
        history.push_user("hi");
        let messages = history.to_messages();
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], Message::User(_)));
    }

    #[test]
    fn restore_sets_messages_and_totals() {
        let mut history = History::new(Some("system".into()));
        history.restore(
            vec![Message::User(UserMessage {
                content: UserContent::Text("resumed".into()),
                name: None,
            })],
            512,
            128,
        );
        assert_eq!(history.len(), 1);
        assert_eq!(history.totals(), (512, 128));
        assert_eq!(user_text(&history.messages()[0]), Some("resumed"));
        // The system prompt is still prepended for the model request. ~keep
        assert!(matches!(history.to_messages()[0], Message::System(_)));
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut history = History::new(None);
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_tokens_details: None,
        };
        assert_eq!(history.add_usage(&usage), (100, 20));
        assert_eq!(history.add_usage(&usage), (200, 40));
        assert_eq!(history.totals(), (200, 40));
    }
}

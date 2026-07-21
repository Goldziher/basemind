//! The testable core of the TUI: the pure reducer [`App::apply`] and input handler
//! [`App::on_key`].
//!
//! Neither function performs any terminal or IO work — they only fold an [`AgentEvent`] into
//! state or translate a key press into an optional [`AgentCommand`]. That keeps the interesting
//! logic unit-testable without a terminal, and leaves rendering ([`crate::ui`]) and the async
//! event loop ([`crate::run`]) as thin, manually-reviewed shells around this module.

use basemind_agent::{AgentCommand, AgentEvent, PermissionDecision, StopReason};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How far a Page Up / Page Down moves the transcript viewport.
const PAGE_SCROLL: u16 = 10;

/// One rendered line of the conversation transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptEntry {
    /// A message the user submitted.
    User(String),
    /// Streaming assistant text; deltas accumulate into the trailing entry of this kind.
    Assistant(String),
    /// A tool invocation and (once finished) its `(ok, summary)` result.
    Tool {
        /// Provider-assigned tool-call id, used to pair a later result with this entry.
        call_id: String,
        /// The namespaced tool name (e.g. `code:outline`).
        name: String,
        /// The arguments rendered as compact JSON.
        args: String,
        /// `Some((ok, summary))` once the tool finishes.
        result: Option<(bool, String)>,
    },
    /// An out-of-band notice (errors, compaction, ...).
    Notice(String),
}

/// A permission request awaiting the user's decision.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionPrompt {
    /// Correlates with the [`AgentCommand::PermissionDecision`] reply.
    pub req_id: u64,
    /// The tool asking for approval.
    pub tool: String,
    /// The action being requested (e.g. `write`, `exec`).
    pub action: String,
    /// The target of the action (path, command, host).
    pub target: String,
}

/// The status bar model: what the header line summarizes.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// The active model name.
    pub model: String,
    /// Cumulative input tokens for the session.
    pub input_tokens: u64,
    /// Cumulative output tokens for the session.
    pub output_tokens: u64,
    /// Whether a turn is currently in flight.
    pub in_flight: bool,
    /// Why the last turn stopped, once idle.
    pub last_reason: Option<StopReason>,
}

/// The full UI state. Owned by the event loop; mutated only through [`App::apply`] and
/// [`App::on_key`].
#[derive(Clone, Debug)]
pub struct App {
    /// The conversation transcript, oldest first.
    pub transcript: Vec<TranscriptEntry>,
    /// The current (unsent) input line.
    pub input: String,
    /// The status bar model.
    pub status: Status,
    /// A pending permission request, if the engine is blocked on one.
    pub pending_permission: Option<PermissionPrompt>,
    /// Set once the user asks to quit; the run loop tears down on the next iteration.
    pub should_quit: bool,
    /// Transcript scroll offset in lines.
    pub scroll: u16,
    /// Set whenever state changes; the run loop redraws only when set, so idle ticks are skipped.
    pub dirty: bool,
}

impl App {
    /// Create a fresh app bound to a model name (shown in the status bar).
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            transcript: Vec::new(),
            input: String::new(),
            status: Status {
                model: model.into(),
                input_tokens: 0,
                output_tokens: 0,
                in_flight: false,
                last_reason: None,
            },
            pending_permission: None,
            should_quit: false,
            scroll: 0,
            dirty: true,
        }
    }

    /// Fold one engine event into state. Pure: no terminal or IO side effects.
    pub fn apply(&mut self, event: AgentEvent) {
        self.dirty = true;
        match event {
            AgentEvent::TurnStarted { .. } => {
                self.status.in_flight = true;
                self.status.last_reason = None;
                // Open a fresh assistant entry that subsequent deltas append into. ~keep
                self.transcript.push(TranscriptEntry::Assistant(String::new()));
            }
            AgentEvent::TextDelta { text, .. } => self.append_assistant(&text),
            AgentEvent::ToolStarted {
                call_id, name, args, ..
            } => {
                self.transcript.push(TranscriptEntry::Tool {
                    call_id,
                    name,
                    args: args.to_string(),
                    result: None,
                });
            }
            AgentEvent::ToolProgress { .. } => {}
            AgentEvent::ToolResult { call_id, ok, summary } => self.fill_tool_result(&call_id, ok, summary),
            AgentEvent::PermissionRequested {
                req_id,
                tool,
                action,
                target,
                ..
            } => {
                self.pending_permission = Some(PermissionPrompt {
                    req_id,
                    tool,
                    action,
                    target,
                });
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                // The event carries running session totals (documented cumulative), so assign ~keep
                // rather than add — adding would double-count across successive turns. ~keep
                self.status.input_tokens = input_tokens;
                self.status.output_tokens = output_tokens;
            }
            AgentEvent::Compacted {
                removed_messages,
                summary_tokens,
            } => {
                self.transcript.push(TranscriptEntry::Notice(format!(
                    "compacted {removed_messages} messages into ~{summary_tokens} tokens"
                )));
            }
            AgentEvent::TurnFinished { reason, .. } => {
                self.status.in_flight = false;
                self.status.last_reason = Some(reason);
                // The turn may have ended (cancel / shutdown / error) with a prompt still ~keep
                // outstanding; drop the now-meaningless overlay so a late answer is not sent into ~keep
                // a finished turn. ~keep
                self.pending_permission = None;
            }
            AgentEvent::Error { message, fatal, .. } => {
                let prefix = if fatal { "fatal error" } else { "error" };
                self.transcript
                    .push(TranscriptEntry::Notice(format!("{prefix}: {message}")));
                if fatal {
                    self.status.in_flight = false;
                    self.pending_permission = None;
                }
            }
        }
    }

    /// Translate a key press into an optional command. Pure: no terminal or IO side effects.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<AgentCommand> {
        self.dirty = true;

        // Ctrl-C is the unconditional exit — checked before the permission capture below so it ~keep
        // still works while a prompt is up. ~keep
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.pending_permission = None;
            self.should_quit = true;
            return Some(AgentCommand::Shutdown);
        }

        // A pending permission prompt captures the rest of the keyboard until answered. ~keep
        if let Some(prompt) = &self.pending_permission {
            return self.answer_permission(prompt.req_id, key.code);
        }

        match key.code {
            // Esc cancels a running turn and stays in the app (the payoff of mid-turn cancel); ~keep
            // when idle there is nothing to cancel, so it quits. ~keep
            KeyCode::Esc => {
                if self.status.in_flight {
                    Some(AgentCommand::Cancel)
                } else {
                    self.should_quit = true;
                    Some(AgentCommand::Shutdown)
                }
            }
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    return None;
                }
                // The engine does not queue messages mid-turn, so submitting now would silently ~keep
                // drop the input. Hold it in the box until the turn ends (Esc cancels a runaway one). ~keep
                if self.status.in_flight {
                    return None;
                }
                let text = std::mem::take(&mut self.input);
                self.transcript.push(TranscriptEntry::User(text.clone()));
                Some(AgentCommand::UserMessage { text })
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(PAGE_SCROLL);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(PAGE_SCROLL);
                None
            }
            _ => None,
        }
    }

    /// Map a key to a permission decision: `y` allow once, `a` allow for the session, `n`/`d` deny,
    /// `Esc` cancel the whole turn. Any of these clears the prompt; other keys are ignored while it
    /// is up.
    fn answer_permission(&mut self, req_id: u64, code: KeyCode) -> Option<AgentCommand> {
        if code == KeyCode::Esc {
            self.pending_permission = None;
            return Some(AgentCommand::Cancel);
        }
        let decision = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => PermissionDecision::Allow,
            KeyCode::Char('a') | KeyCode::Char('A') => PermissionDecision::AllowForSession,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('d') | KeyCode::Char('D') => {
                PermissionDecision::Deny
            }
            _ => return None,
        };
        self.pending_permission = None;
        Some(AgentCommand::PermissionDecision { req_id, decision })
    }

    /// Append streaming text to the trailing assistant entry, opening one if none is current.
    fn append_assistant(&mut self, text: &str) {
        if let Some(TranscriptEntry::Assistant(buffer)) = self.transcript.last_mut() {
            buffer.push_str(text);
        } else {
            self.transcript.push(TranscriptEntry::Assistant(text.to_string()));
        }
    }

    /// Record a tool result: prefer the entry with a matching `call_id`, else the most recent
    /// tool still awaiting a result.
    fn fill_tool_result(&mut self, call_id: &str, ok: bool, summary: String) {
        // Prefer the newest unfilled tool whose call_id matches; fall back to the newest unfilled ~keep
        // tool of any id (results can arrive with a call_id the UI never saw a start for). ~keep
        let by_id = self.transcript.iter().rposition(|entry| {
            matches!(entry, TranscriptEntry::Tool { call_id: id, result, .. } if id == call_id && result.is_none())
        });
        let index = by_id.or_else(|| {
            self.transcript
                .iter()
                .rposition(|entry| matches!(entry, TranscriptEntry::Tool { result: None, .. }))
        });
        if let Some(TranscriptEntry::Tool { result, .. }) = index.map(|i| &mut self.transcript[i]) {
            *result = Some((ok, summary));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn text_deltas_accumulate_into_one_assistant_entry() {
        let mut app = App::new("test-model");
        app.apply(AgentEvent::TurnStarted { turn: 1 });
        app.apply(AgentEvent::TextDelta {
            turn: 1,
            seq: 0,
            text: "Hello, ".into(),
        });
        app.apply(AgentEvent::TextDelta {
            turn: 1,
            seq: 1,
            text: "world".into(),
        });

        let assistants: Vec<_> = app
            .transcript
            .iter()
            .filter(|e| matches!(e, TranscriptEntry::Assistant(_)))
            .collect();
        assert_eq!(assistants.len(), 1, "deltas must fold into a single assistant entry");
        assert_eq!(&app.transcript[0], &TranscriptEntry::Assistant("Hello, world".into()));
        assert!(app.status.in_flight, "TurnStarted marks the turn in flight");
    }

    #[test]
    fn permission_request_sets_prompt_and_allow_for_session_answers_it() {
        let mut app = App::new("test-model");
        app.apply(AgentEvent::PermissionRequested {
            turn: 3,
            req_id: 42,
            call_id: "c1".into(),
            tool: "shell:exec".into(),
            action: "exec".into(),
            target: "ls".into(),
        });
        assert_eq!(
            app.pending_permission,
            Some(PermissionPrompt {
                req_id: 42,
                tool: "shell:exec".into(),
                action: "exec".into(),
                target: "ls".into(),
            })
        );

        let command = app.on_key(key(KeyCode::Char('a')));
        assert_eq!(
            command,
            Some(AgentCommand::PermissionDecision {
                req_id: 42,
                decision: PermissionDecision::AllowForSession,
            })
        );
        assert!(app.pending_permission.is_none(), "answering clears the prompt");
    }

    #[test]
    fn deny_key_maps_to_deny_decision() {
        let mut app = App::new("m");
        app.apply(AgentEvent::PermissionRequested {
            turn: 1,
            req_id: 7,
            call_id: "c".into(),
            tool: "shell:exec".into(),
            action: "exec".into(),
            target: "rm -rf /".into(),
        });
        assert_eq!(
            app.on_key(key(KeyCode::Char('n'))),
            Some(AgentCommand::PermissionDecision {
                req_id: 7,
                decision: PermissionDecision::Deny,
            })
        );
    }

    #[test]
    fn typing_then_enter_emits_user_message_and_clears_input() {
        let mut app = App::new("test-model");
        for c in "hi there".chars() {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(app.input, "hi there");

        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            command,
            Some(AgentCommand::UserMessage {
                text: "hi there".into()
            })
        );
        assert_eq!(app.input, "", "input clears on submit");
        assert_eq!(app.transcript.last(), Some(&TranscriptEntry::User("hi there".into())));
    }

    #[test]
    fn enter_on_blank_input_is_a_no_op() {
        let mut app = App::new("m");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn usage_event_sets_cumulative_token_counters() {
        let mut app = App::new("m");
        app.apply(AgentEvent::Usage {
            turn: 1,
            input_tokens: 128,
            output_tokens: 64,
            cost_usd: Some(0.01),
        });
        assert_eq!(app.status.input_tokens, 128);
        assert_eq!(app.status.output_tokens, 64);
    }

    #[test]
    fn turn_finished_clears_in_flight_and_records_reason() {
        let mut app = App::new("m");
        app.apply(AgentEvent::TurnStarted { turn: 1 });
        assert!(app.status.in_flight);
        app.apply(AgentEvent::TurnFinished {
            turn: 1,
            reason: StopReason::Stop,
            steps: 3,
        });
        assert!(!app.status.in_flight);
        assert_eq!(app.status.last_reason, Some(StopReason::Stop));
    }

    #[test]
    fn tool_result_fills_matching_started_entry() {
        let mut app = App::new("m");
        app.apply(AgentEvent::ToolStarted {
            turn: 1,
            call_id: "call-1".into(),
            name: "code:outline".into(),
            args: serde_json::json!({ "path": "src/lib.rs" }),
        });
        app.apply(AgentEvent::ToolResult {
            call_id: "call-1".into(),
            ok: true,
            summary: "12 symbols".into(),
        });
        match app.transcript.last() {
            Some(TranscriptEntry::Tool { result, name, .. }) => {
                assert_eq!(name, "code:outline");
                assert_eq!(result, &Some((true, "12 symbols".to_string())));
            }
            other => panic!("expected a tool entry, got {other:?}"),
        }
    }

    #[test]
    fn esc_while_a_turn_is_in_flight_cancels_without_quitting() {
        let mut app = App::new("m");
        app.apply(AgentEvent::TurnStarted { turn: 1 });
        assert!(app.status.in_flight);

        let command = app.on_key(key(KeyCode::Esc));
        assert_eq!(command, Some(AgentCommand::Cancel));
        assert!(!app.should_quit, "Esc during a turn cancels but keeps the session open");
    }

    #[test]
    fn esc_while_idle_quits() {
        let mut app = App::new("m");
        assert!(!app.status.in_flight);

        let command = app.on_key(key(KeyCode::Esc));
        assert_eq!(command, Some(AgentCommand::Shutdown));
        assert!(app.should_quit, "Esc with no turn running quits");
    }

    #[test]
    fn ctrl_c_during_a_permission_prompt_still_shuts_down() {
        let mut app = App::new("m");
        app.apply(AgentEvent::PermissionRequested {
            turn: 1,
            req_id: 3,
            call_id: "c".into(),
            tool: "shell:exec".into(),
            action: "exec".into(),
            target: "rm -rf /".into(),
        });
        let command = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            command,
            Some(AgentCommand::Shutdown),
            "Ctrl-C must escape even a prompt"
        );
        assert!(app.should_quit);
        assert!(app.pending_permission.is_none(), "the prompt is cleared on exit");
    }

    #[test]
    fn enter_while_a_turn_is_in_flight_is_held_not_sent() {
        let mut app = App::new("m");
        app.apply(AgentEvent::TurnStarted { turn: 1 });
        for c in "wait".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        // Enter mid-turn must not submit (the engine would drop it) nor clear the box. ~keep
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert_eq!(app.input, "wait", "the message is held, not lost");
        assert!(
            !app.transcript.iter().any(|e| matches!(e, TranscriptEntry::User(_))),
            "nothing is shown as sent"
        );
    }

    #[test]
    fn turn_finished_clears_a_stale_permission_prompt() {
        let mut app = App::new("m");
        app.apply(AgentEvent::PermissionRequested {
            turn: 1,
            req_id: 5,
            call_id: "c".into(),
            tool: "shell:exec".into(),
            action: "exec".into(),
            target: "ls".into(),
        });
        assert!(app.pending_permission.is_some());
        app.apply(AgentEvent::TurnFinished {
            turn: 1,
            reason: StopReason::Cancelled,
            steps: 1,
        });
        assert!(
            app.pending_permission.is_none(),
            "a turn ending without an answer must drop the overlay"
        );
    }

    #[test]
    fn esc_during_a_permission_prompt_cancels_the_turn() {
        let mut app = App::new("m");
        app.apply(AgentEvent::PermissionRequested {
            turn: 1,
            req_id: 9,
            call_id: "c".into(),
            tool: "shell:exec".into(),
            action: "exec".into(),
            target: "rm -rf /".into(),
        });
        let command = app.on_key(key(KeyCode::Esc));
        assert_eq!(command, Some(AgentCommand::Cancel));
        assert!(app.pending_permission.is_none(), "Esc clears the prompt");
        assert!(!app.should_quit, "cancelling a prompt does not quit the app");
    }

    #[test]
    fn ctrl_c_requests_shutdown_and_quit() {
        let mut app = App::new("m");
        let command = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(command, Some(AgentCommand::Shutdown));
        assert!(app.should_quit);
    }
}

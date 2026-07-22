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

/// Longest `/post subject: body` subject accepted: a longer or multi-token lead stays part of the body.
const MAX_SUBJECT_LEN: usize = 32;

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
    /// A multi-agent room message: a peer's (or your own echoed) post to the shared room.
    Room {
        /// The posting agent's identity (`"you"` for a locally echoed self-post).
        from: String,
        /// The optional subject line; empty when the post carried none.
        subject: String,
        /// The message body.
        body: String,
    },
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
    /// Transcript scroll offset in lines. Reconciled against the real viewport before each draw.
    pub scroll: u16,
    /// When set, the transcript auto-scrolls to the newest content each frame. A manual scroll-up
    /// clears it; paging back down to the bottom restores it.
    pub follow: bool,
    /// Set whenever state changes; the run loop redraws only when set, so idle ticks are skipped.
    pub dirty: bool,
    /// The current multi-agent room roster (peers sharing this room), newest snapshot from the engine.
    pub roster: Vec<basemind_agent::RoomPeer>,
    /// Count of room messages / peer deltas that landed while scrolled up (not following the newest
    /// line). Surfaced as an unread cue in the status bar and cleared on return to the bottom.
    pub unread: u32,
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
            follow: true,
            dirty: true,
            roster: Vec::new(),
            unread: 0,
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
            AgentEvent::RoomRoster { peers } => {
                self.roster = peers;
            }
            AgentEvent::RoomMessage(message) => {
                self.transcript.push(TranscriptEntry::Room {
                    from: message.from,
                    subject: message.subject,
                    body: message.body,
                });
                self.note_unread();
            }
            AgentEvent::RoomPeerJoined { peer } => {
                // A refreshed RoomRoster may already list this peer; do not double-announce it. ~keep
                if self.roster.iter().any(|existing| existing.id == peer.id) {
                    return;
                }
                self.transcript
                    .push(TranscriptEntry::Notice(format!("{} joined the room", peer.display)));
                self.roster.push(peer);
                self.note_unread();
            }
            AgentEvent::RoomPeerLeft { id } => {
                // Prefer the departing peer's display name; fall back to the raw id if unknown. ~keep
                let display = self
                    .roster
                    .iter()
                    .position(|existing| existing.id == id)
                    .map(|pos| self.roster.remove(pos).display)
                    .unwrap_or_else(|| id.clone());
                self.transcript
                    .push(TranscriptEntry::Notice(format!("{display} left the room")));
                self.note_unread();
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
            KeyCode::Enter => self.on_enter(),
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            // Scrolling up detaches from the newest content (stops following); the render-time ~keep
            // reconcile re-engages follow once the user pages back down to the bottom. ~keep
            KeyCode::Up => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.follow = false;
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

    /// Handle Enter: dispatch the local room slash-commands (`/roster`, `/room`, `/leave`, `/post`)
    /// first, else submit the input as a user message. Pure: no terminal or IO side effects.
    fn on_enter(&mut self) -> Option<AgentCommand> {
        // `/roster` (alias `/room`) and `/leave` resolve locally — no engine round-trip for the ~keep
        // roster dump, a single RoomLeave for the departure. ~keep
        match self.input.trim() {
            "/roster" | "/room" => {
                let notice = self.roster_notice();
                self.input.clear();
                self.transcript.push(TranscriptEntry::Notice(notice));
                return None;
            }
            "/leave" => {
                self.input.clear();
                self.transcript.push(TranscriptEntry::Notice("leaving the room".into()));
                return Some(AgentCommand::RoomLeave);
            }
            _ => {}
        }
        if self.input.starts_with("/post ") {
            return self.on_post();
        }
        if self.input.trim().is_empty() {
            return None;
        }
        // The engine does not queue messages mid-turn, so submitting now would silently drop the ~keep
        // input. Hold it in the box until the turn ends (Esc cancels a runaway one). ~keep
        if self.status.in_flight {
            return None;
        }
        let text = std::mem::take(&mut self.input);
        self.push_user(text.clone());
        Some(AgentCommand::UserMessage { text })
    }

    /// Handle a `/post ` submission: parse an optional `subject: ` lead, echo the post locally, and
    /// emit the [`AgentCommand::RoomPost`]. Held (not sent, not cleared) while a turn is in flight.
    fn on_post(&mut self) -> Option<AgentCommand> {
        let arg = self.input["/post ".len()..].to_string();
        if arg.trim().is_empty() {
            return None;
        }
        // The engine drops room posts issued mid-turn — its idle command loop is not polled during ~keep
        // a turn — so hold the input like a user message until the turn ends. ~keep
        if self.status.in_flight {
            return None;
        }
        self.input.clear();
        let (subject, body) = split_post(&arg);
        // The broker excludes self-posts from your own inbox, so echo it locally here. ~keep
        self.transcript.push(TranscriptEntry::Room {
            from: "you".into(),
            subject: subject.clone().unwrap_or_default(),
            body: body.clone(),
        });
        Some(AgentCommand::RoomPost { subject, text: body })
    }

    /// A one-line notice summarizing the current roster: the comma-joined display names, or a
    /// "no peers" line when empty. Backs the `/roster` command.
    fn roster_notice(&self) -> String {
        if self.roster.is_empty() {
            return "room: no peers".into();
        }
        let names = self
            .roster
            .iter()
            .map(|peer| peer.display.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("room peers: {names}")
    }

    /// Bump the unread cue when a room event lands while the transcript is scrolled up (not
    /// following the newest line); a following transcript already shows it, so it stays read.
    fn note_unread(&mut self) {
        if !self.follow {
            self.unread = self.unread.saturating_add(1);
        }
    }

    /// Record a user message in the transcript. Used by the Enter handler and to mirror the
    /// auto-sent opening prompt (sent straight to the engine, bypassing `on_key`) so it shows a
    /// `you:` line like any typed message.
    pub fn push_user(&mut self, text: String) {
        self.transcript.push(TranscriptEntry::User(text));
        self.dirty = true;
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

/// Split a `/post` argument into an optional subject and the body. A leading `subject: ` (split on
/// the first `": "`) is lifted into the subject only when the candidate is a single whitespace-free
/// token no longer than [`MAX_SUBJECT_LEN`]; otherwise the whole argument is the body (subject `None`).
fn split_post(arg: &str) -> (Option<String>, String) {
    if let Some((head, rest)) = arg.split_once(": ")
        && !head.is_empty()
        && head.chars().count() <= MAX_SUBJECT_LEN
        && !head.chars().any(char::is_whitespace)
    {
        return (Some(head.to_string()), rest.to_string());
    }
    (None, arg.to_string())
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
    fn room_roster_event_populates_roster() {
        let mut app = App::new("m");
        let peers = vec![
            basemind_agent::RoomPeer {
                id: "a".into(),
                display: "alice".into(),
            },
            basemind_agent::RoomPeer {
                id: "b".into(),
                display: "bob".into(),
            },
        ];
        app.apply(AgentEvent::RoomRoster { peers: peers.clone() });
        assert_eq!(app.roster, peers);
    }

    #[test]
    fn room_message_folds_into_room_entry() {
        let mut app = App::new("m");
        app.apply(AgentEvent::RoomMessage(basemind_agent::RoomMessage {
            from: "alice".into(),
            subject: "hi".into(),
            body: "hello there".into(),
        }));
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Room {
                from: "alice".into(),
                subject: "hi".into(),
                body: "hello there".into(),
            })
        );
    }

    #[test]
    fn slash_post_emits_room_post_and_clears_input() {
        let mut app = App::new("m");
        for c in "/post hello team".chars() {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), None);
        }
        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            command,
            Some(AgentCommand::RoomPost {
                subject: None,
                text: "hello team".into(),
            })
        );
        assert_eq!(app.input, "", "input clears on a room post");
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Room {
                from: "you".into(),
                subject: String::new(),
                body: "hello team".into(),
            }),
            "the post is locally echoed"
        );
    }

    #[test]
    fn slash_post_mid_turn_is_held() {
        let mut app = App::new("m");
        app.apply(AgentEvent::TurnStarted { turn: 1 });
        for c in "/post hi".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert_eq!(app.input, "/post hi", "the post is held, not cleared");
        assert!(
            !app.transcript.iter().any(|e| matches!(e, TranscriptEntry::Room { .. })),
            "nothing is echoed while held"
        );
    }

    #[test]
    fn ctrl_c_requests_shutdown_and_quit() {
        let mut app = App::new("m");
        let command = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(command, Some(AgentCommand::Shutdown));
        assert!(app.should_quit);
    }

    fn type_line(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn slash_post_with_a_subject_lifts_it_into_the_post_and_echo() {
        let mut app = App::new("m");
        type_line(&mut app, "/post fix: the parser drops trailing commas");
        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            command,
            Some(AgentCommand::RoomPost {
                subject: Some("fix".into()),
                text: "the parser drops trailing commas".into(),
            })
        );
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Room {
                from: "you".into(),
                subject: "fix".into(),
                body: "the parser drops trailing commas".into(),
            }),
            "the echo carries the parsed subject"
        );
        assert_eq!(app.input, "", "input clears on a room post");
    }

    #[test]
    fn slash_post_without_a_subject_stays_subjectless() {
        let mut app = App::new("m");
        type_line(&mut app, "/post ship it");
        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            command,
            Some(AgentCommand::RoomPost {
                subject: None,
                text: "ship it".into(),
            })
        );
    }

    #[test]
    fn split_post_treats_a_multi_token_lead_as_body() {
        // A whitespace-bearing lead before `": "` is not a subject — the whole thing is the body. ~keep
        assert_eq!(
            split_post("I think: we should merge"),
            (None, "I think: we should merge".to_string())
        );
        assert_eq!(split_post("sync: done"), (Some("sync".to_string()), "done".to_string()));
        assert_eq!(split_post("no marker here"), (None, "no marker here".to_string()));
    }

    #[test]
    fn slash_roster_lists_peers_as_a_notice_and_sends_nothing() {
        let mut app = App::new("m");
        app.apply(AgentEvent::RoomRoster {
            peers: vec![
                basemind_agent::RoomPeer {
                    id: "a".into(),
                    display: "alice".into(),
                },
                basemind_agent::RoomPeer {
                    id: "b".into(),
                    display: "bob".into(),
                },
            ],
        });
        type_line(&mut app, "/roster");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None, "/roster is a local command");
        assert_eq!(app.input, "", "input clears");
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("room peers: alice, bob".into()))
        );
    }

    #[test]
    fn slash_room_alias_with_an_empty_roster_says_no_peers() {
        let mut app = App::new("m");
        type_line(&mut app, "/room");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("room: no peers".into()))
        );
    }

    #[test]
    fn slash_leave_emits_room_leave_and_clears_input() {
        let mut app = App::new("m");
        type_line(&mut app, "/leave");
        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(command, Some(AgentCommand::RoomLeave));
        assert_eq!(app.input, "", "input clears on leave");
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("leaving the room".into()))
        );
    }

    #[test]
    fn peer_joined_pushes_a_notice_and_extends_the_roster() {
        let mut app = App::new("m");
        app.apply(AgentEvent::RoomPeerJoined {
            peer: basemind_agent::RoomPeer {
                id: "c".into(),
                display: "carol".into(),
            },
        });
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("carol joined the room".into()))
        );
        assert_eq!(app.roster.len(), 1, "the joiner is reflected in the roster");
    }

    #[test]
    fn peer_joined_does_not_double_announce_a_known_peer() {
        let mut app = App::new("m");
        let peer = basemind_agent::RoomPeer {
            id: "c".into(),
            display: "carol".into(),
        };
        app.apply(AgentEvent::RoomRoster {
            peers: vec![peer.clone()],
        });
        app.apply(AgentEvent::RoomPeerJoined { peer });
        assert!(
            !app.transcript.iter().any(|e| matches!(e, TranscriptEntry::Notice(_))),
            "a peer already on the roster must not be announced again"
        );
        assert_eq!(app.roster.len(), 1, "no duplicate roster entry");
    }

    #[test]
    fn peer_left_uses_the_display_name_and_prunes_the_roster() {
        let mut app = App::new("m");
        app.apply(AgentEvent::RoomRoster {
            peers: vec![basemind_agent::RoomPeer {
                id: "c".into(),
                display: "carol".into(),
            }],
        });
        app.apply(AgentEvent::RoomPeerLeft { id: "c".into() });
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("carol left the room".into()))
        );
        assert!(app.roster.is_empty(), "the departed peer is pruned");
    }

    #[test]
    fn peer_left_falls_back_to_the_id_when_unknown() {
        let mut app = App::new("m");
        app.apply(AgentEvent::RoomPeerLeft { id: "ghost".into() });
        assert_eq!(
            app.transcript.last(),
            Some(&TranscriptEntry::Notice("ghost left the room".into()))
        );
    }

    #[test]
    fn unread_counts_room_events_only_while_scrolled_up() {
        let mut app = App::new("m");
        // Following (at the bottom): a message is seen, so it stays read. ~keep
        app.apply(AgentEvent::RoomMessage(basemind_agent::RoomMessage {
            from: "alice".into(),
            subject: String::new(),
            body: "one".into(),
        }));
        assert_eq!(app.unread, 0, "a message read at the bottom does not count");

        // Detach (scroll up): subsequent room events accumulate as unread. ~keep
        app.follow = false;
        app.apply(AgentEvent::RoomMessage(basemind_agent::RoomMessage {
            from: "alice".into(),
            subject: String::new(),
            body: "two".into(),
        }));
        app.apply(AgentEvent::RoomPeerJoined {
            peer: basemind_agent::RoomPeer {
                id: "c".into(),
                display: "carol".into(),
            },
        });
        assert_eq!(app.unread, 2, "a message and a join both count while detached");
    }
}

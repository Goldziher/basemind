//! `[shells]` configuration sub-tree.
//!
//! Governs how basemind presents an agent-spawned shell session to the user.
//! The whole tree is feature-independent at the type level (it derives the same
//! `schemars` schema whether or not the `shells` cargo feature is compiled in)
//! so the published config schema is stable across feature matrices; the visual
//! launcher in [`crate::shells::launcher`] only consumes these values when built
//! with `--features shells`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default terminal width in columns for a freshly spawned visual session.
const DEFAULT_COLS: u16 = 200;
/// Default terminal height in rows for a freshly spawned visual session.
const DEFAULT_ROWS: u16 = 50;

/// `[shells]` config sub-tree. See module docs for context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShellsConfig {
    /// Master switch. Only meaningful when the `shells` cargo feature is compiled in; when
    /// `false` the shell MCP / CLI tools are wired but no session is ever spawned.
    pub enabled: bool,
    /// How a visual session is surfaced. Defaults to [`VisualMode::Current`] — a new tab in the
    /// terminal that is already open.
    pub visual: VisualMode,
    /// Which terminal emulator to drive. [`TerminalChoice::Auto`] (the default) detects the
    /// running terminal from the environment and falls back gracefully.
    pub terminal: TerminalChoice,
    /// Initial column count for the session's pseudo-terminal.
    #[schemars(range(min = 1))]
    pub default_cols: u16,
    /// Initial row count for the session's pseudo-terminal.
    #[schemars(range(min = 1))]
    pub default_rows: u16,
    /// Keep the session alive after the visual surface is closed. When `false` the session is
    /// torn down once its presentation exits.
    pub keep_on_exit: bool,
}

impl Default for ShellsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            visual: VisualMode::default(),
            terminal: TerminalChoice::default(),
            default_cols: DEFAULT_COLS,
            default_rows: DEFAULT_ROWS,
            keep_on_exit: true,
        }
    }
}

/// How a visual shell session is presented to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum VisualMode {
    /// Open the session as a new tab in the terminal that is already open. The default — it keeps
    /// the user's window layout intact.
    #[default]
    Current,
    /// Open the session in a brand-new terminal window.
    Window,
    /// Surface the session over a web frontend (wired by the lead; command-building is a no-op).
    Web,
    /// Do not present the session at all; it runs headless.
    Headless,
}

/// Which terminal emulator the visual launcher should drive.
///
/// [`TerminalChoice::Auto`] inspects the environment (`TERM_PROGRAM`, `$TERMINAL`, and platform
/// defaults) to pick a concrete emulator. The explicit variants force a specific emulator
/// regardless of detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalChoice {
    /// Detect the running terminal from the environment and fall back gracefully.
    #[default]
    Auto,
    /// iTerm2 (macOS).
    Iterm2,
    /// Terminal.app (macOS).
    TerminalApp,
    /// Windows Terminal (`wt.exe`).
    WindowsTerminal,
    /// GNOME Terminal (Linux).
    GnomeTerminal,
    /// Konsole (Linux / KDE).
    Konsole,
    /// WezTerm (cross-platform).
    Wezterm,
    /// Alacritty (cross-platform).
    Alacritty,
    /// kitty (cross-platform).
    Kitty,
    /// xterm (Linux / X11 fallback).
    Xterm,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_current_tab_in_open_terminal() {
        let cfg = ShellsConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.visual, VisualMode::Current);
        assert_eq!(cfg.terminal, TerminalChoice::Auto);
        assert_eq!(cfg.default_cols, 200);
        assert_eq!(cfg.default_rows, 50);
        assert!(cfg.keep_on_exit);
    }

    #[test]
    fn should_roundtrip_visual_mode_as_snake_case() {
        let json = serde_json::to_string(&VisualMode::Headless).expect("serialize");
        assert_eq!(json, "\"headless\"");
        let back: VisualMode = serde_json::from_str("\"current\"").expect("deserialize");
        assert_eq!(back, VisualMode::Current);
    }

    #[test]
    fn should_roundtrip_terminal_choice_as_snake_case() {
        let json = serde_json::to_string(&TerminalChoice::WindowsTerminal).expect("serialize");
        assert_eq!(json, "\"windows_terminal\"");
        let back: TerminalChoice = serde_json::from_str("\"gnome_terminal\"").expect("deserialize");
        assert_eq!(back, TerminalChoice::GnomeTerminal);
    }

    #[test]
    fn should_reject_unknown_fields() {
        let err = serde_json::from_str::<ShellsConfig>(r#"{"bogus": true}"#);
        assert!(err.is_err());
    }
}

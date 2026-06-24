//! Visual presentation of a spawned shell session.
//!
//! Headless sessions need no presentation; visual sessions are surfaced to the
//! user per the `[shells] visual` config (default: a new tab in the already-open
//! terminal). This module holds the per-OS launchers.
//!
//! The presentation is "attach to an rmux session inside a terminal surface": we
//! build the shell command `rmux -S <socket> attach-session -t <name>` and ask
//! the host terminal (or a fresh one) to run it as a new tab or window.
//!
//! [`build_present_command`] is pure and unit-tested — it returns the argv that
//! *would* drive the terminal without executing anything. [`present`] spawns it.
//!
//! The whole module is gated on `feature = "shells"`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{TerminalChoice, VisualMode};

/// How to attach to a live rmux session: its name, the daemon socket it lives on,
/// the initial terminal geometry, and the basemind executable to re-exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTarget {
    /// rmux session name (the basemind-minted `bmsh-*` id).
    pub session_name: String,
    /// Path to the rmux daemon's control socket.
    pub socket_path: PathBuf,
    /// Initial terminal width handed to the attach driver.
    pub cols: u16,
    /// Initial terminal height handed to the attach driver.
    pub rows: u16,
    /// The basemind executable to re-exec as the visual attach driver
    /// (`current_exe()`); there is no external `rmux` binary.
    pub exe: PathBuf,
}

impl AttachTarget {
    /// Build the shell command string that, when run inside a terminal, attaches to this session.
    ///
    /// Shape: `<exe> --__internal-attach <session_name> --socket <socket_path> --size <cols>x<rows>`.
    /// basemind ships no external `rmux` binary, so the attach re-execs basemind itself with the
    /// hidden `--__internal-attach` flag. Each path / name argument is quoted so values with spaces
    /// survive the host shell; `cols`/`rows` are `u16` and need no quoting.
    ///
    /// Quoting is OS-aware: POSIX single-quotes on unix, double-quotes on Windows (where `cmd.exe`
    /// and `wt.exe` do not honour single quotes). The Windows launcher itself passes the components
    /// as SEPARATE process arguments (see [`Self::attach_argv`]) and never parses this string, so
    /// this form is only handed back to a human via [`Presentation::AttachCommand`].
    #[must_use]
    pub fn attach_command(&self) -> String {
        format!(
            "{} {} {} --socket {} --size {}x{}",
            shell_quote(&self.exe.to_string_lossy()),
            crate::shells::attach::INTERNAL_ATTACH_FLAG,
            shell_quote(&self.session_name),
            shell_quote(&self.socket_path.to_string_lossy()),
            self.cols,
            self.rows,
        )
    }

    /// The attach re-exec as a raw argument vector: the basemind exe followed by the hidden
    /// `--__internal-attach` flag and its operands, each as a SEPARATE element.
    ///
    /// This is the quoting-free form a launcher passes to a terminal that forwards an argv (e.g.
    /// `wt.exe ... -- <argv>`), so paths with spaces survive without any shell-quoting round-trip.
    #[must_use]
    pub fn attach_argv(&self) -> Vec<String> {
        vec![
            self.exe.to_string_lossy().into_owned(),
            crate::shells::attach::INTERNAL_ATTACH_FLAG.to_string(),
            self.session_name.clone(),
            "--socket".to_string(),
            self.socket_path.to_string_lossy().into_owned(),
            "--size".to_string(),
            format!("{}x{}", self.cols, self.rows),
        ]
    }
}

/// The argv basemind would execute to present a session: a program plus its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentCommand {
    /// Program to spawn (e.g. `osascript`, `gnome-terminal`, `wt.exe`).
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
}

/// Outcome of presenting a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presentation {
    /// A terminal surface was spawned for the session.
    Spawned,
    /// Headless mode — nothing was presented.
    Headless,
    /// No terminal could be driven; the caller should run this attach command themselves.
    AttachCommand(String),
}

/// Quote a string for the host shell, OS-aware.
///
/// POSIX shells (unix) use single quotes with the classic `'\''` escape; `cmd.exe` / `wt.exe`
/// (Windows) do not honour single quotes, so the Windows form double-quotes and escapes embedded
/// double quotes by doubling them. This string is only ever shown to a human on Windows — the
/// launcher passes a raw argv (see [`AttachTarget::attach_argv`]) — so the quoting need only be
/// "pasteable", not a hardened argv encoder.
fn shell_quote(value: &str) -> String {
    #[cfg(windows)]
    {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    }
    #[cfg(not(windows))]
    {
        let escaped = value.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

/// Build the OS command that would present `target` per `mode` + `terminal`, WITHOUT executing.
///
/// Pure and unit-testable. Returns the argv to run, or `None` when nothing is spawned (Headless,
/// or Web — whose surface the lead wires separately).
#[must_use]
pub fn build_present_command(
    mode: VisualMode,
    terminal: TerminalChoice,
    target: &AttachTarget,
) -> Option<PresentCommand> {
    match mode {
        VisualMode::Headless | VisualMode::Web => None,
        VisualMode::Current => build_for_surface(terminal, target, Surface::Tab),
        VisualMode::Window => build_for_surface(terminal, target, Surface::Window),
    }
}

/// Whether the session opens as a tab in the running terminal or in a fresh window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Tab,
    Window,
}

#[cfg(target_os = "macos")]
fn build_for_surface(
    terminal: TerminalChoice,
    target: &AttachTarget,
    surface: Surface,
) -> Option<PresentCommand> {
    let attach = target.attach_command();
    let use_iterm = match terminal {
        TerminalChoice::Iterm2 => true,
        TerminalChoice::TerminalApp => false,
        TerminalChoice::Auto => detected_macos_is_iterm(),
        // Cross-platform emulators: honour them if the user forced one, else they have their own
        // CLIs; for macOS we fall back to Terminal.app driving when we cannot.
        _ => false,
    };
    let script = if use_iterm {
        macos_iterm_script(&attach, surface)
    } else {
        macos_terminal_app_script(&attach, surface)
    };
    Some(PresentCommand {
        program: "osascript".to_string(),
        args: vec!["-e".to_string(), script],
    })
}

#[cfg(target_os = "macos")]
fn detected_macos_is_iterm() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|v| v == "iTerm.app")
        .unwrap_or(false)
}

/// AppleScript driving iTerm2: a new tab in the current window, or a brand-new window.
#[cfg(target_os = "macos")]
fn macos_iterm_script(attach: &str, surface: Surface) -> String {
    let escaped = applescript_quote(attach);
    match surface {
        Surface::Tab => format!(
            "tell application \"iTerm2\"\n\
             tell current window to create tab with default profile\n\
             tell current session of current window to write text {escaped}\n\
             end tell"
        ),
        Surface::Window => format!(
            "tell application \"iTerm2\"\n\
             set newWindow to (create window with default profile)\n\
             tell current session of newWindow to write text {escaped}\n\
             end tell"
        ),
    }
}

/// AppleScript driving Terminal.app. `do script` opens a new window; reusing the front window's
/// tab is done via `do script ... in front window` for the tab surface.
#[cfg(target_os = "macos")]
fn macos_terminal_app_script(attach: &str, surface: Surface) -> String {
    let escaped = applescript_quote(attach);
    match surface {
        Surface::Tab => format!(
            "tell application \"Terminal\"\n\
             activate\n\
             do script {escaped} in front window\n\
             end tell"
        ),
        Surface::Window => format!(
            "tell application \"Terminal\"\n\
             activate\n\
             do script {escaped}\n\
             end tell"
        ),
    }
}

/// Quote a string for embedding inside an AppleScript string literal.
///
/// Escapes `\` and `"` (the literal delimiters), then `\n` / `\r` / `\t` to their
/// AppleScript escape sequences. Any remaining control character (`< 0x20`,
/// including a bare NUL) is replaced with a space so a newline / NUL embedded in
/// the attach command can never break out of the string literal and inject extra
/// AppleScript statements.
#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            // Any other control char (including NUL) is dropped to a space so it
            // cannot terminate the literal or smuggle a newline-delimited command.
            c if (c as u32) < 0x20 => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(target_os = "windows")]
fn build_for_surface(
    terminal: TerminalChoice,
    target: &AttachTarget,
    surface: Surface,
) -> Option<PresentCommand> {
    // The only Windows terminal the launcher drives is Windows Terminal (`wt.exe`). Honour an
    // explicit `WindowsTerminal` choice and treat `Auto` the same (it is the platform default on
    // Windows 11). Any non-Windows emulator choice is meaningless here, so fall through to the
    // attach-command hand-back rather than spawning the wrong terminal.
    match terminal {
        TerminalChoice::WindowsTerminal | TerminalChoice::Auto => {}
        _ => return None,
    }
    Some(windows_terminal_command(target, surface))
}

/// Build the `wt.exe` argv that opens the attach in a Windows Terminal tab or window.
///
/// `wt.exe` parses everything after `--` as the literal command argv (no shell, no `;`-as-tab
/// hazard), so we forward [`AttachTarget::attach_argv`] verbatim — each element a separate process
/// argument. The tab surface targets the current window (`-w 0 new-tab`); the window surface forces
/// a fresh window (`-w new new-tab`).
#[cfg(target_os = "windows")]
fn windows_terminal_command(target: &AttachTarget, surface: Surface) -> PresentCommand {
    let window = match surface {
        // `-w 0` reuses the current Windows Terminal window; a new tab is added there.
        Surface::Tab => "0",
        // `-w new` always spins up a fresh window.
        Surface::Window => "new",
    };
    let mut args = vec![
        "-w".to_string(),
        window.to_string(),
        "new-tab".to_string(),
        "--".to_string(),
    ];
    args.extend(target.attach_argv());
    PresentCommand {
        program: "wt.exe".to_string(),
        args,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn build_for_surface(
    terminal: TerminalChoice,
    target: &AttachTarget,
    surface: Surface,
) -> Option<PresentCommand> {
    let attach = target.attach_command();
    let emulator = resolve_linux_emulator(terminal);
    Some(linux_command(emulator, &attach, surface))
}

/// A concrete Linux terminal emulator the launcher knows how to drive.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxEmulator {
    GnomeTerminal,
    Konsole,
    Wezterm,
    Alacritty,
    Kitty,
    Xterm,
}

/// Map the configured choice to a concrete Linux emulator, detecting from the environment when
/// `Auto`. Falls back to `xterm` (the lowest common denominator on X11).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn resolve_linux_emulator(terminal: TerminalChoice) -> LinuxEmulator {
    match terminal {
        TerminalChoice::GnomeTerminal => LinuxEmulator::GnomeTerminal,
        TerminalChoice::Konsole => LinuxEmulator::Konsole,
        TerminalChoice::Wezterm => LinuxEmulator::Wezterm,
        TerminalChoice::Alacritty => LinuxEmulator::Alacritty,
        TerminalChoice::Kitty => LinuxEmulator::Kitty,
        TerminalChoice::Xterm => LinuxEmulator::Xterm,
        // macOS / Windows choices are meaningless on Linux; treat as Auto.
        TerminalChoice::Auto
        | TerminalChoice::Iterm2
        | TerminalChoice::TerminalApp
        | TerminalChoice::WindowsTerminal => detect_linux_emulator(),
    }
}

/// Detect the running Linux emulator from the environment, defaulting to xterm.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn detect_linux_emulator() -> LinuxEmulator {
    if let Ok(term) = std::env::var("TERMINAL")
        && let Some(found) = match_emulator_name(&term)
    {
        return found;
    }
    if let Ok(term) = std::env::var("TERM_PROGRAM")
        && let Some(found) = match_emulator_name(&term)
    {
        return found;
    }
    LinuxEmulator::Xterm
}

/// Best-effort name match against a known emulator. Substring-tolerant so a full path like
/// `/usr/bin/gnome-terminal` still resolves.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn match_emulator_name(name: &str) -> Option<LinuxEmulator> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("gnome-terminal") {
        Some(LinuxEmulator::GnomeTerminal)
    } else if lower.contains("konsole") {
        Some(LinuxEmulator::Konsole)
    } else if lower.contains("wezterm") {
        Some(LinuxEmulator::Wezterm)
    } else if lower.contains("alacritty") {
        Some(LinuxEmulator::Alacritty)
    } else if lower.contains("kitty") {
        Some(LinuxEmulator::Kitty)
    } else if lower.contains("xterm") {
        Some(LinuxEmulator::Xterm)
    } else {
        None
    }
}

/// Build the argv for a Linux emulator. Tab-capable emulators (gnome-terminal, konsole, wezterm,
/// kitty) open a tab in the running instance for [`Surface::Tab`]; alacritty and xterm have no tab
/// concept, so they fall back to a new window in both surfaces.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn linux_command(emulator: LinuxEmulator, attach: &str, surface: Surface) -> PresentCommand {
    let want_tab = surface == Surface::Tab;
    match emulator {
        LinuxEmulator::GnomeTerminal => {
            let mut args = vec![if want_tab { "--tab" } else { "--window" }.to_string()];
            args.push("--".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(attach.to_string());
            PresentCommand {
                program: "gnome-terminal".to_string(),
                args,
            }
        }
        LinuxEmulator::Konsole => {
            let flag = if want_tab { "--new-tab" } else { "" };
            let mut args = Vec::new();
            if !flag.is_empty() {
                args.push(flag.to_string());
            }
            args.push("-e".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(attach.to_string());
            PresentCommand {
                program: "konsole".to_string(),
                args,
            }
        }
        LinuxEmulator::Wezterm => {
            // `wezterm cli spawn` opens a new tab in the running GUI; a fresh window uses `start`.
            let args = if want_tab {
                vec![
                    "cli".to_string(),
                    "spawn".to_string(),
                    "--".to_string(),
                    "bash".to_string(),
                    "-lc".to_string(),
                    attach.to_string(),
                ]
            } else {
                vec![
                    "start".to_string(),
                    "--".to_string(),
                    "bash".to_string(),
                    "-lc".to_string(),
                    attach.to_string(),
                ]
            };
            PresentCommand {
                program: "wezterm".to_string(),
                args,
            }
        }
        LinuxEmulator::Kitty => {
            // kitty drives tabs via remote control; fall back to a new OS window otherwise.
            let args = if want_tab {
                vec![
                    "@".to_string(),
                    "launch".to_string(),
                    "--type=tab".to_string(),
                    "bash".to_string(),
                    "-lc".to_string(),
                    attach.to_string(),
                ]
            } else {
                vec!["bash".to_string(), "-lc".to_string(), attach.to_string()]
            };
            PresentCommand {
                program: "kitty".to_string(),
                args,
            }
        }
        // Alacritty and xterm have no tab model — always a new window.
        LinuxEmulator::Alacritty => PresentCommand {
            program: "alacritty".to_string(),
            args: vec![
                "-e".to_string(),
                "bash".to_string(),
                "-lc".to_string(),
                attach.to_string(),
            ],
        },
        // xterm has no tab model — always a new window running the attach directly.
        LinuxEmulator::Xterm => PresentCommand {
            program: "xterm".to_string(),
            args: vec![
                "-e".to_string(),
                "bash".to_string(),
                "-lc".to_string(),
                attach.to_string(),
            ],
        },
    }
}

/// Execute [`build_present_command`] — spawn the terminal surface. Best-effort.
///
/// Returns [`Presentation::Headless`] for headless mode, [`Presentation::AttachCommand`] when no
/// command could be built (Web mode), and [`Presentation::Spawned`] once the terminal is launched.
pub fn present(
    mode: VisualMode,
    terminal: TerminalChoice,
    target: &AttachTarget,
) -> Result<Presentation> {
    if mode == VisualMode::Headless {
        return Ok(Presentation::Headless);
    }
    let Some(command) = build_present_command(mode, terminal, target) else {
        // Web (or any mode that yields no command) hands the attach string back to the caller.
        return Ok(Presentation::AttachCommand(target.attach_command()));
    };
    std::process::Command::new(&command.program)
        .args(&command.args)
        .spawn()
        .with_context(|| format!("spawning terminal `{}` to present session", command.program))?;
    Ok(Presentation::Spawned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> AttachTarget {
        AttachTarget {
            session_name: "bmsh-1-2".to_string(),
            socket_path: PathBuf::from("/tmp/rmux.sock"),
            cols: 200,
            rows: 50,
            // Pinned to a fixed path so the asserted attach string is deterministic.
            exe: PathBuf::from("/usr/local/bin/basemind"),
        }
    }

    /// The attach command the per-OS launchers embed; pinned by `target()`. POSIX
    /// single-quoted form — used by the unix + Linux-emulator assertions. Windows quotes
    /// differently (see `windows_attach_command_uses_double_quotes`).
    #[cfg(not(windows))]
    const EXPECTED_ATTACH: &str = "'/usr/local/bin/basemind' --__internal-attach 'bmsh-1-2' --socket '/tmp/rmux.sock' \
         --size 200x50";

    #[cfg(not(windows))]
    #[test]
    fn attach_command_has_expected_shape() {
        let cmd = target().attach_command();
        assert_eq!(cmd, EXPECTED_ATTACH);
    }

    #[test]
    fn headless_builds_no_command() {
        assert!(
            build_present_command(VisualMode::Headless, TerminalChoice::Auto, &target()).is_none()
        );
    }

    #[test]
    fn web_builds_no_command() {
        assert!(build_present_command(VisualMode::Web, TerminalChoice::Auto, &target()).is_none());
    }

    #[test]
    fn present_headless_is_headless() {
        let out = present(VisualMode::Headless, TerminalChoice::Auto, &target()).expect("ok");
        assert_eq!(out, Presentation::Headless);
    }

    #[test]
    fn present_web_returns_attach_command() {
        let out = present(VisualMode::Web, TerminalChoice::Auto, &target()).expect("ok");
        assert_eq!(out, Presentation::AttachCommand(target().attach_command()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_iterm_tab_drives_osascript() {
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Iterm2, &target())
            .expect("command");
        assert_eq!(cmd.program, "osascript");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0], "-e");
        let script = &cmd.args[1];
        assert!(script.contains("iTerm2"), "script: {script}");
        assert!(script.contains("create tab"), "script: {script}");
        assert!(
            script.contains("--__internal-attach 'bmsh-1-2'"),
            "script: {script}"
        );
        assert!(
            script.contains("--socket '/tmp/rmux.sock'"),
            "script: {script}"
        );
        assert!(script.contains("--size 200x50"), "script: {script}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quote_neutralizes_control_chars() {
        // A newline must become the `\n` escape, not a raw line break that would
        // start a second AppleScript statement; a NUL collapses to a space.
        let quoted = applescript_quote("attach\nrm -rf /\0");
        assert!(quoted.starts_with('"') && quoted.ends_with('"'), "{quoted}");
        assert!(quoted.contains("\\n"), "newline must be escaped: {quoted}");
        assert!(
            !quoted[1..quoted.len() - 1].contains('\n'),
            "no raw newline inside the literal: {quoted}"
        );
        assert!(!quoted.contains('\0'), "NUL must be stripped: {quoted}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_iterm_window_creates_window() {
        let cmd = build_present_command(VisualMode::Window, TerminalChoice::Iterm2, &target())
            .expect("command");
        assert_eq!(cmd.program, "osascript");
        assert!(cmd.args[1].contains("create window"), "{}", cmd.args[1]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_app_tab_uses_front_window() {
        let cmd =
            build_present_command(VisualMode::Current, TerminalChoice::TerminalApp, &target())
                .expect("command");
        assert_eq!(cmd.program, "osascript");
        let script = &cmd.args[1];
        assert!(script.contains("Terminal"), "{script}");
        assert!(script.contains("do script"), "{script}");
        assert!(script.contains("in front window"), "{script}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_app_window_omits_front_window() {
        let cmd = build_present_command(VisualMode::Window, TerminalChoice::TerminalApp, &target())
            .expect("command");
        let script = &cmd.args[1];
        assert!(script.contains("do script"), "{script}");
        assert!(!script.contains("in front window"), "{script}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_current_builds_wt_new_tab_in_current_window() {
        let cmd = build_present_command(
            VisualMode::Current,
            TerminalChoice::WindowsTerminal,
            &target(),
        )
        .expect("wt.exe command");
        assert_eq!(cmd.program, "wt.exe");
        // `-w 0 new-tab --` then the attach argv, each a separate process argument.
        assert_eq!(cmd.args[0], "-w");
        assert_eq!(cmd.args[1], "0");
        assert_eq!(cmd.args[2], "new-tab");
        assert_eq!(cmd.args[3], "--");
        assert_eq!(cmd.args[4], "/usr/local/bin/basemind");
        assert_eq!(cmd.args[5], "--__internal-attach");
        assert_eq!(cmd.args[6], "bmsh-1-2");
        assert_eq!(cmd.args[7], "--socket");
        assert_eq!(cmd.args[8], "/tmp/rmux.sock");
        assert_eq!(cmd.args[9], "--size");
        assert_eq!(cmd.args[10], "200x50");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_window_forces_a_fresh_wt_window() {
        let cmd = build_present_command(
            VisualMode::Window,
            TerminalChoice::WindowsTerminal,
            &target(),
        )
        .expect("wt.exe command");
        assert_eq!(cmd.program, "wt.exe");
        assert_eq!(cmd.args[0], "-w");
        assert_eq!(cmd.args[1], "new");
        assert_eq!(cmd.args[2], "new-tab");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_auto_uses_wt() {
        // `Auto` is Windows Terminal on Windows; a non-Windows emulator choice hands the
        // attach command back instead of spawning the wrong terminal.
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Auto, &target())
            .expect("auto -> wt.exe");
        assert_eq!(cmd.program, "wt.exe");
        assert!(
            build_present_command(VisualMode::Current, TerminalChoice::Xterm, &target()).is_none()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_attach_command_uses_double_quotes() {
        // On Windows the human-facing attach string must double-quote (cmd/wt ignore single
        // quotes); the raw argv the launcher actually spawns is unquoted.
        let cmd = target().attach_command();
        assert!(cmd.contains("\"/usr/local/bin/basemind\""), "{cmd}");
        assert!(cmd.contains("\"bmsh-1-2\""), "{cmd}");
        assert!(!cmd.contains('\''), "no single quotes on Windows: {cmd}");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_gnome_terminal_tab() {
        let cmd = build_present_command(
            VisualMode::Current,
            TerminalChoice::GnomeTerminal,
            &target(),
        )
        .expect("command");
        assert_eq!(cmd.program, "gnome-terminal");
        assert_eq!(cmd.args[0], "--tab");
        assert_eq!(cmd.args[1], "--");
        assert_eq!(cmd.args[2], "bash");
        assert_eq!(cmd.args[3], "-lc");
        assert_eq!(cmd.args[4], EXPECTED_ATTACH);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_gnome_terminal_window() {
        let cmd =
            build_present_command(VisualMode::Window, TerminalChoice::GnomeTerminal, &target())
                .expect("command");
        assert_eq!(cmd.args[0], "--window");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_konsole_new_tab() {
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Konsole, &target())
            .expect("command");
        assert_eq!(cmd.program, "konsole");
        assert_eq!(cmd.args[0], "--new-tab");
        assert_eq!(cmd.args[1], "-e");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_wezterm_tab_spawns_cli() {
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Wezterm, &target())
            .expect("command");
        assert_eq!(cmd.program, "wezterm");
        assert_eq!(cmd.args[0], "cli");
        assert_eq!(cmd.args[1], "spawn");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_alacritty_falls_back_to_window_for_tab() {
        // Alacritty has no tab model — both surfaces yield the same new-window argv.
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Alacritty, &target())
            .expect("command");
        assert_eq!(cmd.program, "alacritty");
        assert_eq!(cmd.args[0], "-e");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn linux_xterm_is_the_fallback() {
        let cmd = build_present_command(VisualMode::Current, TerminalChoice::Xterm, &target())
            .expect("command");
        assert_eq!(cmd.program, "xterm");
        assert_eq!(cmd.args[0], "-e");
    }
}

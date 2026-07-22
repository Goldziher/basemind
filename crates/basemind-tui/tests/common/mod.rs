//! Shared PTY end-to-end harness for the `basemind-tui` replay tests. It spawns the real binary under
//! a pseudo-terminal, feeds its live ANSI output into a `vt100` emulator, injects keystrokes, and
//! exposes poll-until screen assertions. Included via `mod common;` from each `tests/pty_*.rs` file
//! (all `#![cfg(all(feature = "replay", unix))]`), so this module only ever compiles under that gate.
//!
//! Timing contract: the UI repaints on a ~33 ms dirty-tick, so never assert immediately after an
//! action — always go through [`PtySession::wait_for`] / [`expect_screen`](PtySession::expect_screen),
//! which poll the emulated screen until the expectation holds or a deadline elapses.

// Each pty_* test is its own binary that compiles this module and uses only a subset of the helpers, ~keep
// so clippy — which sees one binary at a time — reports the helpers exercised only by sibling test ~keep
// files as dead code. They are not: this is shared test infrastructure, never production code. ~keep
#![allow(dead_code)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Poll interval while waiting on screen state.
const POLL: Duration = Duration::from_millis(40);

/// How long to wait for the child to exit after Ctrl-C before force-killing it on teardown.
const REAP_BUDGET: Duration = Duration::from_secs(3);

/// Default drive budget for the `expect_*` helpers. Generous so a loaded CI box does not flake.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// One cell of the emulated screen, copied out of the parser so callers never hold the mutex across a
/// borrow (`vt100` hands back a `&Cell` tied to the `Screen`, which is tied to the lock).
#[derive(Clone, Debug)]
pub struct OwnedCell {
    pub ch: String,
    pub bold: bool,
    pub fg: vt100::Color,
    pub bg: vt100::Color,
}

/// A running `basemind-tui --replay` process wired to a PTY and a `vt100` screen emulator. Dropping it
/// quits the app (Ctrl-C), reaps the child, and joins the reader thread.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    // The scenario file and root dir must outlive the child; hold them so they drop only on teardown. ~keep
    _root: tempfile::TempDir,
    _scenario_file: tempfile::NamedTempFile,
}

impl PtySession {
    /// Spawn a replay run in a default 80x24 terminal.
    pub fn spawn(scenario_json: &str) -> PtySession {
        Self::spawn_full(scenario_json, 24, 80, &[])
    }

    /// Spawn a replay run in a terminal of the given size (for resize / layout tests).
    pub fn spawn_sized(scenario_json: &str, rows: u16, cols: u16) -> PtySession {
        Self::spawn_full(scenario_json, rows, cols, &[])
    }

    /// Spawn a replay run with extra CLI args appended after `--root`/`--replay` (e.g. a positional
    /// initial prompt).
    pub fn spawn_with_args(scenario_json: &str, extra_args: &[&str]) -> PtySession {
        Self::spawn_full(scenario_json, 24, 80, extra_args)
    }

    fn spawn_full(scenario_json: &str, rows: u16, cols: u16, extra_args: &[&str]) -> PtySession {
        let root = tempfile::tempdir().expect("create temp root");
        let mut scenario_file = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("create scenario file");
        scenario_file
            .write_all(scenario_json.as_bytes())
            .expect("write scenario");
        scenario_file.flush().expect("flush scenario");

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_basemind-tui"));
        command.arg("--root");
        command.arg(root.path());
        command.arg("--replay");
        command.arg(scenario_file.path());
        for arg in extra_args {
            command.arg(arg);
        }
        // A sane terminal type so crossterm's raw-mode setup is happy. ~keep
        command.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(command).expect("spawn binary");
        // Drop the slave so the master sees EOF once the child exits. ~keep
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer = pair.master.take_writer().expect("take pty writer");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let reader_parser = Arc::clone(&parser);
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => reader_parser.lock().expect("parser lock").process(&buf[..n]),
                }
            }
        });

        PtySession {
            master: pair.master,
            writer,
            child,
            parser,
            reader_thread: Some(reader_thread),
            _root: root,
            _scenario_file: scenario_file,
        }
    }

    /// The emulator's visible screen flattened to text (rows joined by newlines).
    pub fn screen(&self) -> String {
        self.parser.lock().expect("parser lock").screen().contents()
    }

    /// A single cell's contents + key styles, copied out so no lock is held across the return.
    pub fn cell(&self, row: u16, col: u16) -> Option<OwnedCell> {
        let guard = self.parser.lock().expect("parser lock");
        let screen = guard.screen();
        screen.cell(row, col).map(|cell| OwnedCell {
            ch: cell.contents().to_string(),
            bold: cell.bold(),
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
        })
    }

    /// The emulator's cursor position as `(row, col)`.
    pub fn cursor(&self) -> (u16, u16) {
        self.parser.lock().expect("parser lock").screen().cursor_position()
    }

    /// The screen with trailing blanks trimmed per row — a stable form for golden comparisons.
    pub fn snapshot(&self) -> String {
        self.screen()
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Poll until the screen contains `needle`, or return the last screen on timeout.
    pub fn wait_for(&self, needle: &str, timeout: Duration) -> Result<(), String> {
        self.wait_for_all(&[needle], timeout)
    }

    /// Poll until the screen contains every `needle`, or return the last screen on timeout.
    pub fn wait_for_all(&self, needles: &[&str], timeout: Duration) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if needles.iter().all(|needle| screen.contains(needle)) {
                return Ok(());
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "timed out after {timeout:?} waiting for all of {needles:?}\n\
                     --- last screen ---\n{screen}\n--- end screen ---"
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Assert `needle` stays ABSENT for the whole `dwell` window — polling throughout so a late frame
    /// that surfaces it still fails the check. Use for "no second prompt" / "marker stripped" cases.
    pub fn refute_after_settle(&self, needle: &str, dwell: Duration) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if screen.contains(needle) {
                return Err(format!(
                    "expected {needle:?} to stay absent but it appeared\n\
                     --- screen ---\n{screen}\n--- end screen ---"
                ));
            }
            if start.elapsed() >= dwell {
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    /// [`wait_for`](Self::wait_for) with the default timeout, panicking with the last screen on failure.
    pub fn expect_screen(&self, needle: &str) {
        if let Err(error) = self.wait_for(needle, DEFAULT_TIMEOUT) {
            panic!("{error}");
        }
    }

    /// [`wait_for_all`](Self::wait_for_all) with the default timeout, panicking on failure.
    pub fn expect_all(&self, needles: &[&str]) {
        if let Err(error) = self.wait_for_all(needles, DEFAULT_TIMEOUT) {
            panic!("{error}");
        }
    }

    /// [`refute_after_settle`](Self::refute_after_settle), panicking if `needle` appears within `dwell`.
    pub fn expect_absent(&self, needle: &str, dwell: Duration) {
        if let Err(error) = self.refute_after_settle(needle, dwell) {
            panic!("{error}");
        }
    }

    /// Write raw bytes to the terminal and flush.
    pub fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// Type a UTF-8 string as if entered at the keyboard.
    pub fn type_str(&mut self, text: &str) {
        self.send(text.as_bytes());
    }

    /// Type a single character.
    pub fn char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.send(ch.encode_utf8(&mut buf).as_bytes());
    }

    /// Press Enter (`\r`, which crossterm decodes as Enter in raw mode).
    pub fn enter(&mut self) {
        self.send(b"\r");
    }

    /// Press Escape. A lone `\x1b` can be ambiguous with an escape sequence, so callers should always
    /// follow this with a poll-until check rather than an immediate assertion.
    pub fn esc(&mut self) {
        self.send(b"\x1b");
    }

    /// Press Backspace (`\x7f`, the DEL crossterm maps to Backspace).
    pub fn backspace(&mut self) {
        self.send(b"\x7f");
    }

    /// Send Ctrl-C (`\x03`).
    pub fn ctrl_c(&mut self) {
        self.send(b"\x03");
    }

    /// Press the Up arrow.
    pub fn up(&mut self) {
        self.send(b"\x1b[A");
    }

    /// Press the Down arrow.
    pub fn down(&mut self) {
        self.send(b"\x1b[B");
    }

    /// Press Page Up.
    pub fn page_up(&mut self) {
        self.send(b"\x1b[5~");
    }

    /// Press Page Down.
    pub fn page_down(&mut self) {
        self.send(b"\x1b[6~");
    }

    /// Answer a permission prompt: allow once (`y`).
    pub fn allow_once(&mut self) {
        self.send(b"y");
    }

    /// Answer a permission prompt: allow for the session (`a`).
    pub fn allow_session(&mut self) {
        self.send(b"a");
    }

    /// Answer a permission prompt: deny (`n`).
    pub fn deny(&mut self) {
        self.send(b"n");
    }

    /// Resize the terminal. On unix this delivers SIGWINCH to the child, driving `run.rs`'s resize arm,
    /// and re-sizes the emulator so subsequent screen reads reflect the new geometry.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.parser
            .lock()
            .expect("parser lock")
            .screen_mut()
            .set_size(rows, cols);
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort quit, then reap so we never leak the child; force-kill if it lingers. ~keep
        let _ = self.writer.write_all(b"\x03");
        let _ = self.writer.flush();

        let reap_start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if reap_start.elapsed() < REAP_BUDGET => std::thread::sleep(POLL),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }

        // The reader hits EOF once the child is gone; join so no thread leaks. ~keep
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

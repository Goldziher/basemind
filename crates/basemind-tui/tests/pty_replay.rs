#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end test for the `basemind-tui` binary — the "Playwright for TTY"
//! layer that the in-memory `TestBackend` e2e tests cannot reach.
//!
//! `TestBackend` renders the pure `App`/`ui` half in memory, but it never exercises `run.rs`, which
//! enters crossterm raw mode + the alternate screen and needs a real terminal. This test closes that
//! gap: it spawns the actual binary under a pseudo-terminal, parses its live ANSI output with a
//! `vt100` emulator into an 80x24 screen grid, injects keystrokes, and asserts on the rendered
//! screen — proving the whole loop works over a genuine TTY.
//!
//! What it proves, driving a scripted (`--replay`) run with no network / no API key:
//! 1. The binary comes up in raw mode + alternate screen and renders the transcript.
//! 2. A permission-gated `shell:exec` raises the centered " permission required " overlay.
//! 3. Injecting `a` (allow-for-session) over the PTY approves it, the command runs, and its stdout
//!    (a unique marker) surfaces as a tool result — so the exec truly ran, not just echoed args.
//! 4. The scenario's second, tool-free turn stops the run (status bar reaches " idle (Stop) ").
//! 5. Ctrl-C over the PTY quits cleanly and the child reaps without leaking a process.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Unique stdout marker the scripted `shell:exec` emits; distinctive enough to never collide with
/// incidental screen text.
const MARKER: &str = "PTY-MARKER-7F3A9";

/// Total budget for the whole drive (spawn → approve → marker → idle). Generous so a loaded CI box
/// does not flake.
const DEADLINE: Duration = Duration::from_secs(15);

/// How long to wait for the child to exit after Ctrl-C before force-killing it.
const REAP_BUDGET: Duration = Duration::from_secs(3);

/// Poll interval while waiting on screen state.
const POLL: Duration = Duration::from_millis(40);

/// Snapshot the emulator's visible 80x24 screen as text for `contains` checks.
fn screen_text(parser: &Arc<Mutex<vt100::Parser>>) -> String {
    parser.lock().expect("parser lock").screen().contents()
}

#[test]
fn a_pty_replay_run_approves_a_shell_exec_and_renders_its_output() {
    // A hermetic, shell-only scenario: turn 1 runs `echo <marker>` behind the permission gate; turn ~keep
    // 2 emits a closing line with no tools, which stops the run. ~keep
    let scenario = format!(
        r#"{{
            "user": "run the marker",
            "turns": [
                {{
                    "text": "Running the marker now.",
                    "tools": [
                        {{ "id": "c1", "name": "shell:exec", "args": {{ "command": "echo {MARKER}" }} }}
                    ]
                }},
                {{ "text": "All done." }}
            ]
        }}"#
    );

    // A fresh empty tempdir as `--root` keeps the run hermetic (no code map — shell still works). ~keep
    let root = tempfile::tempdir().expect("create temp root");
    let mut scenario_file = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("create scenario file");
    scenario_file.write_all(scenario.as_bytes()).expect("write scenario");
    scenario_file.flush().expect("flush scenario");

    // Open an 80x24 PTY and spawn the binary Cargo built for this integration test. ~keep
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_basemind-tui"));
    command.arg("--root");
    command.arg(root.path());
    command.arg("--replay");
    command.arg(scenario_file.path());
    // A sane terminal type so crossterm's raw-mode setup is happy. ~keep
    command.env("TERM", "xterm-256color");

    let mut child = pty.slave.spawn_command(command).expect("spawn binary");
    // Drop the slave so the master sees EOF once the child exits. ~keep
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pty.master.take_writer().expect("take pty writer");

    // Feed all PTY output into a vt100 emulator from a background thread; the main thread polls the ~keep
    // resulting screen grid. ~keep
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
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

    // Drive the screen: approve the permission overlay once it appears, then wait for the marker to ~keep
    // surface as a tool result AND the run to reach idle (which only happens if the exec succeeded ~keep
    // and turn 2 stopped) — so a stray marker in the echoed args cannot pass the test on its own. ~keep
    let start = Instant::now();
    let mut approved = false;
    let mut done = false;
    while start.elapsed() < DEADLINE {
        let screen = screen_text(&parser);
        if !approved && screen.contains("permission required") {
            writer.write_all(b"a").expect("send approval key");
            writer.flush().expect("flush approval");
            approved = true;
        }
        if approved && screen.contains(MARKER) && screen.contains("idle (Stop)") {
            done = true;
            break;
        }
        std::thread::sleep(POLL);
    }

    if !done {
        let last = screen_text(&parser);
        // Best-effort cleanup before failing so we never leak the child. ~keep
        let _ = writer.write_all(&[0x03]);
        let _ = child.kill();
        panic!(
            "PTY replay did not complete (approved={approved}) within {:?}.\n\
             Expected screen to contain {MARKER:?} and \"idle (Stop)\".\n\
             --- last screen ---\n{last}\n--- end screen ---",
            DEADLINE
        );
    }

    let final_screen = screen_text(&parser);
    assert!(
        final_screen.contains(MARKER),
        "final screen should contain the exec output marker\n{final_screen}"
    );
    assert!(
        final_screen.contains("idle (Stop)"),
        "final screen should show the run reached idle after the stopping turn\n{final_screen}"
    );

    // Quit with Ctrl-C and reap the child on a short budget; force-kill if it lingers. ~keep
    writer.write_all(&[0x03]).expect("send ctrl-c");
    writer.flush().expect("flush ctrl-c");

    let reap_start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if reap_start.elapsed() < REAP_BUDGET => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }

    // The master's reader hits EOF once the child is gone; join so no thread leaks. ~keep
    let _ = reader_thread.join();
}

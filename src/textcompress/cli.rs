//! CLI surface for the behavioral output compressor (`basemind compress-output`).
//!
//! Kept out of `main.rs` so the binary entry point stays under the 1000-line cap;
//! the clap args and the stdin→compress→stdout runner live next to the module
//! they drive.

use std::io::{Read, Write};

use anyhow::{Context, Result};

use super::compress_output;
use super::delta::delta;

/// Arguments for `basemind compress-output`.
#[derive(clap::Args, Debug)]
pub struct CompressOutputArgs {
    /// Command family to compress for. When omitted, the family is detected
    /// from the output shape. Accepted: git_status, git_log, git_diff,
    /// npm_install, cargo_build, pytest, ls, grep, logs (aliases allowed).
    #[arg(long)]
    pub family: Option<String>,
}

/// Read all of stdin, run the behavioral output compressor, write the (possibly
/// compressed) result to stdout, and a one-line savings note to stderr. Reads
/// raw bytes lossily so non-UTF-8 tool output never aborts the pipe — the
/// compressor operates on the lossy text and the fail-open path returns it
/// unchanged when nothing matches.
pub fn run(args: &CompressOutputArgs) -> Result<()> {
    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .context("read stdin")?;
    let text = String::from_utf8_lossy(&raw);

    let outcome = compress_output(text.as_ref(), args.family.as_deref());

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(outcome.output.as_bytes())
        .context("write compressed output")?;
    stdout.flush().context("flush stdout")?;

    let saved = outcome
        .original_bytes
        .saturating_sub(outcome.compressed_bytes);
    let pct = if outcome.original_bytes > 0 {
        (saved as f64 / outcome.original_bytes as f64) * 100.0
    } else {
        0.0
    };
    eprintln!(
        "compress-output: family={} compressed={} {} -> {} bytes ({:.0}% saved)",
        outcome.family_detected,
        outcome.compressed,
        outcome.original_bytes,
        outcome.compressed_bytes,
        pct,
    );
    Ok(())
}

/// Arguments for `basemind delta`.
///
/// The NEW content is read from stdin; the OLD content is read from the
/// `--old` file path. The stateless [`delta`] primitive emits a compact
/// `+N/-M` line-diff (or a full-content bail marker on oversize input).
#[derive(clap::Args, Debug)]
pub struct DeltaArgs {
    /// Path to the OLD (previously seen) content. The NEW content is read from
    /// stdin.
    #[arg(long)]
    pub old: std::path::PathBuf,
}

/// Read the OLD content from `--old` and the NEW content from stdin, run the
/// stateless delta primitive, write the diff (or bail marker + full content) to
/// stdout, and a one-line stat to stderr. Both sides are read lossily so
/// non-UTF-8 content never aborts the pipe.
pub fn run_delta(args: &DeltaArgs) -> Result<()> {
    let old_raw = std::fs::read(&args.old)
        .with_context(|| format!("read old content from {}", args.old.display()))?;
    let old = String::from_utf8_lossy(&old_raw);

    let mut new_raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut new_raw)
        .context("read new content from stdin")?;
    let new = String::from_utf8_lossy(&new_raw);

    let outcome = delta(old.as_ref(), new.as_ref());

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(outcome.output.as_bytes())
        .context("write delta output")?;
    stdout.write_all(b"\n").context("write trailing newline")?;
    stdout.flush().context("flush stdout")?;

    eprintln!(
        "delta: changed={} bailed={} old_lines={} new_lines={} +{}/-{}",
        outcome.changed,
        outcome.bailed,
        outcome.old_lines,
        outcome.new_lines,
        outcome.added,
        outcome.removed,
    );
    Ok(())
}

//! CLI surface for the behavioral output compressor (`basemind compress-output`).
//!
//! Kept out of `main.rs` so the binary entry point stays under the 1000-line cap;
//! the clap args and the stdin→compress→stdout runner live next to the module
//! they drive.

use std::io::{Read, Write};

use anyhow::{Context, Result};

use super::compress_output;

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

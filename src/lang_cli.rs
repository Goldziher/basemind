//! `basemind lang` subcommand bodies (list / install / clean). Split out of `main.rs`
//! to keep that file under the 1000-line module cap; invoked from the `Cmd::Lang` dispatch.

use anyhow::Result;

use basemind::render::{self, Verbosity};

pub(crate) fn cmd_lang_list(no_color: bool) -> Result<()> {
    use anstyle::{AnsiColor, Color, Reset, Style};
    use std::io::Write;
    let mut out = render::stdout(no_color);
    let installed = basemind::lang::downloaded_languages();
    let supported: std::collections::HashSet<&str> = basemind::lang::SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .collect();
    let installed_set: std::collections::HashSet<&str> =
        installed.iter().map(String::as_str).collect();

    let ok = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let warn = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    let dim = Style::new().dimmed();

    let _ = writeln!(out, "supported by basemind (queries shipped):");
    for &name in basemind::lang::SUPPORTED_LANGUAGES {
        let (sym, label, style) = if installed_set.contains(name) {
            ('✓', "ready", ok)
        } else {
            ('·', "missing", warn)
        };
        let _ = writeln!(
            out,
            "  {s}{sym} {label:<7}{r} {name}",
            s = style.render(),
            r = Reset.render(),
            sym = sym,
            label = label,
            name = name,
        );
    }

    let extras: Vec<&str> = installed
        .iter()
        .map(String::as_str)
        .filter(|n| !supported.contains(n))
        .collect();
    if !extras.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{d}also cached (no basemind queries, parse-only):{r}",
            d = dim.render(),
            r = Reset.render(),
        );
        for n in extras {
            let _ = writeln!(
                out,
                "  {d}· {n}{r}",
                d = dim.render(),
                r = Reset.render(),
                n = n,
            );
        }
    }

    if let Some(dir) = basemind::lang::grammar_cache_dir() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{d}cache: {dir}{r}",
            d = dim.render(),
            r = Reset.render(),
            dir = dir.display(),
        );
    }
    Ok(())
}

pub(crate) fn cmd_lang_install(verbosity: Verbosity, no_color: bool) -> Result<()> {
    crate::bootstrap_grammars(verbosity, no_color)?;
    if verbosity != Verbosity::Quiet {
        let summary = basemind::lang::ensure_grammars().map_err(|e| anyhow::anyhow!("{e}"))?;
        if !summary.did_download() {
            println!(
                "all {} supported grammars already cached",
                summary.already_cached.len()
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_lang_clean() -> Result<()> {
    basemind::lang::clean_grammar_cache().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("grammar cache cleared");
    Ok(())
}

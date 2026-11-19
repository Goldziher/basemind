//! `basemind manual [topic]`: the offline manual (issue #133).
//!
//! The pages are `docs/manual/<topic>.md`, compiled in, so the command needs no index, daemon or
//! network. They are also the single source of the skill sections between
//! `<!-- manual:<topic> -->` markers: `scripts/sync_public_skills.py` writes [`spliced`] there, and
//! `tests/plugin_manifest_smoke.rs` fails when a copy differs.

use std::io::Write;

use anyhow::Result;
use clap::Args;
use clap::builder::PossibleValuesParser;

/// One manual page.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    /// The name `basemind manual <topic>` takes; the page's file stem.
    pub topic: &'static str,
    /// The page's Markdown, its first line a `# ` title.
    pub text: &'static str,
}

macro_rules! pages {
    ($($topic:literal),+ $(,)?) => {
        /// Every topic page, in the order the index lists them.
        pub const PAGES: &[Page] = &[$(Page {
            topic: $topic,
            text: include_str!(concat!("../../docs/manual/", $topic, ".md")),
        }),+];
    };
}

pages!(
    "code",
    "resolution",
    "git",
    "documents",
    "memory",
    "agents",
    "indexing",
    "paging",
    "setup",
    "exit-codes",
    "environment",
    "troubleshooting",
);

/// The page `basemind manual` prints with no topic: the overview and the topic list.
pub const INDEX: &str = include_str!("../../docs/manual/index.md");

#[derive(Args, Debug)]
pub struct ManualArgs {
    /// The page to print; omit for the overview and the list of topics.
    #[arg(value_parser = PossibleValuesParser::new(PAGES.iter().map(|page| page.topic)))]
    pub topic: Option<String>,
}

/// The page of `topic`, if there is one.
pub fn page(topic: &str) -> Option<&'static Page> {
    PAGES.iter().find(|page| page.topic == topic)
}

/// `topic`'s page as a skill section carries it ([`splice_body`]).
pub fn spliced(topic: &str) -> Option<String> {
    page(topic).map(|page| splice_body(page.text))
}

/// A page without its `# ` title line and the blank line after it, every heading outside a code
/// fence one level deeper, so the page's sections nest under the skill's own heading.
fn splice_body(text: &str) -> String {
    let body = text
        .strip_prefix("# ")
        .and_then(|rest| rest.split_once('\n'))
        // A Windows checkout may hold the page with CRLF line endings.
        .map_or(text, |(_, rest)| {
            rest.strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .unwrap_or(rest)
        });
    let mut out = String::with_capacity(body.len() + 16);
    let mut fenced = false;
    for line in body.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Print the index, or `args.topic`'s page (clap has already refused an unknown topic).
pub fn run(args: &ManualArgs) -> Result<()> {
    let text = args.topic.as_deref().and_then(page).map_or(INDEX, |page| page.text);
    match std::io::stdout().lock().write_all(text.as_bytes()) {
        // A reader that stopped early (`| head`) has what it wanted.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/manual")
    }

    #[test]
    fn every_page_file_is_a_topic_and_every_topic_a_page_file() {
        let mut files: Vec<String> = std::fs::read_dir(manual_dir())
            .expect("read docs/manual")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .filter_map(|name| name.strip_suffix(".md").map(str::to_string))
            .filter(|stem| stem != "index")
            .collect();
        files.sort();
        let mut topics: Vec<String> = PAGES.iter().map(|page| page.topic.to_string()).collect();
        topics.sort();
        assert!(topics.len() >= 10, "the manual lost its pages: {topics:?}");
        assert_eq!(
            files, topics,
            "docs/manual/*.md and PAGES disagree; list a new page in `pages!`"
        );
    }

    #[test]
    fn every_page_has_a_title_and_the_index_lists_it_once() {
        assert!(INDEX.starts_with("# "), "the index needs a `# ` title");
        for page in PAGES {
            assert!(page.text.starts_with("# "), "{} needs a `# ` title line", page.topic);
            let entry = format!("`basemind manual {}`", page.topic);
            assert_eq!(INDEX.matches(&entry).count(), 1, "index.md must list {entry} once");
        }
    }

    #[test]
    fn spliced_drops_the_title_and_demotes_headings_outside_fences() {
        let body = spliced("code").expect("code page");
        assert!(!body.contains("# Code navigation"), "title kept:\n{body}");
        assert!(body.contains("\n### Contracts\n"), "heading not demoted:\n{body}");
        assert!(body.starts_with("The code tools"), "leading blank line kept:\n{body}");
        assert!(spliced("no-such-topic").is_none());
    }

    #[test]
    fn splice_body_leaves_fenced_lines_alone() {
        assert_eq!(
            splice_body("# Title\n\nText\n```sh\n# comment\n```\n## Head\n"),
            "Text\n```sh\n# comment\n```\n### Head\n"
        );
    }

    #[test]
    fn splice_body_reads_crlf_pages() {
        assert_eq!(splice_body("# Title\r\n\r\nText\r\n## Head\r\n"), "Text\n### Head\n");
    }
}

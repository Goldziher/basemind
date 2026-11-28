//! What the text of a tool error may carry out of the process: no absolute host path.
//!
//! Tool errors get pasted into public issues, and a data-home or home-directory path names the user.
//! A data-home path is written `$BASEMIND_DATA_HOME/…`, a repository path repository-relative, and
//! any other path under the home directory `~/…`. The unredacted text goes to the log.

use std::path::Path;

/// How a data-home path is written in a tool error.
pub(crate) const DATA_HOME_PLACEHOLDER: &str = "$BASEMIND_DATA_HOME";

/// One directory a tool error must not spell out: every spelling of it, what a path under it starts
/// with instead, and what the directory itself is written as.
struct Root {
    spellings: Vec<String>,
    under: &'static str,
    itself: &'static str,
}

impl Root {
    fn new(path: &Path, under: &'static str, itself: &'static str) -> Self {
        let mut spellings = vec![path.display().to_string()];
        if let Ok(canonical) = path.canonicalize() {
            spellings.push(canonical.display().to_string());
        }
        spellings.retain(|spelling| spelling.len() > 1);
        spellings.dedup();
        Self {
            spellings,
            under,
            itself,
        }
    }
}

/// The directories one workspace's tool errors are redacted against.
pub struct Redaction {
    roots: Vec<Root>,
}

impl Redaction {
    /// Redact against the repository `root`, the data home `data_home` and the home directory `home`.
    pub fn new(root: &Path, data_home: &Path, home: Option<&Path>) -> Self {
        let mut roots = vec![
            Root::new(data_home, "$BASEMIND_DATA_HOME/", DATA_HOME_PLACEHOLDER),
            Root::new(root, "", "."),
        ];
        if let Some(home) = home {
            roots.push(Root::new(home, "~/", "~"));
        }
        Self { roots }
    }

    /// `text` with every spelling of a root rewritten, the longest first, so a path is written
    /// against the most specific root that holds it.
    pub fn apply(&self, text: &str) -> String {
        let mut spellings: Vec<(&str, &Root)> = self
            .roots
            .iter()
            .flat_map(|root| root.spellings.iter().map(move |spelling| (spelling.as_str(), root)))
            .collect();
        spellings.sort_by_key(|(spelling, _)| std::cmp::Reverse(spelling.len()));
        spellings.into_iter().fold(text.to_string(), |text, (spelling, root)| {
            rewrite(&text, spelling, root)
        })
    }
}

/// `text` with `spelling` rewritten wherever it is a whole path or a path's leading directories, and
/// left alone where it only begins a longer name (`/repo` in `/repo2`).
fn rewrite(text: &str, spelling: &str, root: &Root) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(spelling) {
        let consumed = text.len() - rest.len() + at;
        let before = &text[..consumed];
        let scheme_boundary = before.ends_with("://");
        let mid_path = !scheme_boundary
            && before
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_alphanumeric() || matches!(prev, '_' | '-' | '.' | '~' | '/' | '\\'));
        if mid_path {
            let first = spelling.chars().next().map_or(1, char::len_utf8);
            out.push_str(&rest[..at + first]);
            rest = &rest[at + first..];
            continue;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at + spelling.len()..];
        match after.chars().next() {
            Some(separator @ ('/' | '\\')) => {
                out.push_str(root.under);
                rest = &after[separator.len_utf8()..];
            }
            Some(next) if next.is_alphanumeric() || matches!(next, '_' | '-') => {
                out.push_str(spelling);
                rest = after;
            }
            _ => {
                out.push_str(root.itself);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redaction() -> Redaction {
        Redaction::new(
            Path::new("/home/someone/code/repo"),
            Path::new("/home/someone/.local/share/basemind"),
            Some(Path::new("/home/someone")),
        )
    }

    #[test]
    fn each_root_is_written_as_its_placeholder() {
        let cases = [
            (
                "io error on /home/someone/.local/share/basemind/cache/blobs/v34/ab/cd: denied",
                "io error on $BASEMIND_DATA_HOME/cache/blobs/v34/ab/cd: denied",
            ),
            (
                "not inside a git repository (starting from /home/someone/code/repo)",
                "not inside a git repository (starting from .)",
            ),
            (
                "read /home/someone/code/repo/src/lib.rs failed",
                "read src/lib.rs failed",
            ),
            ("config at /home/someone/.config/x.toml", "config at ~/.config/x.toml"),
            ("home is /home/someone.", "home is ~."),
            (
                "data home /home/someone/.local/share/basemind",
                "data home $BASEMIND_DATA_HOME",
            ),
        ];
        for (text, redacted) in cases {
            assert_eq!(redaction().apply(text), redacted, "{text}");
        }
    }

    #[test]
    fn a_longer_name_that_starts_like_a_root_is_left_alone() {
        assert_eq!(
            redaction().apply("/home/someone/code/repo2/a.rs and /home/someonelse/b"),
            "~/code/repo2/a.rs and /home/someonelse/b"
        );
    }

    #[test]
    fn a_root_inside_a_longer_path_is_left_alone() {
        assert_eq!(
            redaction().apply("/mnt/home/someone/x and /home/someone/y"),
            "/mnt/home/someone/x and ~/y"
        );
    }

    #[test]
    fn a_root_after_a_url_scheme_is_redacted() {
        assert_eq!(
            redaction().apply("file:///home/someone/x and sqlite:///home/someone/code/repo/db"),
            "file://~/x and sqlite://db"
        );
    }

    #[test]
    fn a_root_after_a_tilde_or_dot_is_left_alone() {
        let text = "~/home/someone/x and ./home/someone/y";
        assert_eq!(redaction().apply(text), text);
    }

    #[test]
    fn a_message_without_host_paths_is_unchanged() {
        let text = "`code_outline`: no file `src/x.rs` in this repository";
        assert_eq!(redaction().apply(text), text);
    }

    /// The text of a real temporary repository, which `canonicalize` may spell differently (macOS
    /// resolves `/var` to `/private/var`), is repository-relative under either spelling.
    #[test]
    fn both_spellings_of_a_directory_are_redacted() {
        let repo = tempfile::tempdir().expect("tempdir");
        let canonical = repo.path().canonicalize().expect("canonical");
        let redaction = Redaction::new(repo.path(), Path::new("/nonexistent/data"), None);
        let text = format!(
            "{} and {}",
            repo.path().join("a.rs").display(),
            canonical.join("b.rs").display()
        );
        assert_eq!(redaction.apply(&text), "a.rs and b.rs");
    }
}

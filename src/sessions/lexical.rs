//! Paths as transcripts record them, compared as text rather than through `std::path`: a transcript
//! may name `C:\repo\src\a.rs`, `C:/repo/src/a.rs` or `/repo/src/a.rs` whatever platform reads it,
//! and a canonical Windows checkout carries the verbatim `\\?\` prefix its transcripts do not. Both
//! separators split a path, `.` is dropped and `..` is resolved without touching the filesystem.

/// A path split into its root and components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexical {
    /// `/`, a drive (`c:`, lower case) or a UNC share (`//server/share`, lower case); `None` for a
    /// relative path.
    root: Option<String>,
    parts: Vec<String>,
}

impl Lexical {
    pub fn parse(raw: &str) -> Self {
        let text = raw.replace('\\', "/");
        let text = match text.strip_prefix("//?/") {
            Some(rest) => match rest.strip_prefix("UNC/") {
                Some(share) => format!("//{share}"),
                None => rest.to_owned(),
            },
            None => text,
        };
        let bytes = text.as_bytes();
        let (root, rest) = if let Some(unc) = text.strip_prefix("//") {
            let mut words = unc.splitn(3, '/');
            let server = words.next().unwrap_or_default();
            let share = words.next().unwrap_or_default();
            (
                Some(format!("//{server}/{share}").to_ascii_lowercase()),
                words.next().unwrap_or_default(),
            )
        } else if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            (Some(text[..2].to_ascii_lowercase()), &text[2..])
        } else if let Some(rest) = text.strip_prefix('/') {
            (Some("/".to_owned()), rest)
        } else {
            (None, text.as_str())
        };
        let mut parts: Vec<String> = Vec::new();
        for part in rest.split('/') {
            match part {
                "" | "." => {}
                ".." if root.is_some() => {
                    parts.pop();
                }
                _ => parts.push(part.to_owned()),
            }
        }
        Self { root, parts }
    }

    pub fn is_absolute(&self) -> bool {
        self.root.is_some()
    }

    /// The components of this path below `base`, or `None` when it is not at or below it. Under a
    /// drive or a share the comparison ignores ASCII case, as Windows does.
    pub fn strip(&self, base: &Lexical) -> Option<Vec<String>> {
        let root = self.root.as_deref()?;
        if base.root.as_deref() != Some(root) || base.parts.len() > self.parts.len() {
            return None;
        }
        let fold = root != "/";
        let same = |a: &String, b: &String| if fold { a.eq_ignore_ascii_case(b) } else { a == b };
        base.parts
            .iter()
            .zip(&self.parts)
            .all(|(b, a)| same(a, b))
            .then(|| self.parts[base.parts.len()..].to_vec())
    }

    /// The components of a relative path, `..` kept.
    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The path written with `/`.
    pub fn display(&self) -> String {
        let parts = self.parts.join("/");
        match self.root.as_deref() {
            Some("/") => format!("/{parts}"),
            Some(root) => format!("{root}/{parts}"),
            None => parts,
        }
    }
}

/// `path` taken from the directory `base`, both components below one checkout, with `..` resolved;
/// `None` when it climbs above the checkout.
pub fn resolve(base: &[String], path: &[String]) -> Option<Vec<String>> {
    let mut parts: Vec<String> = base.to_vec();
    for part in path {
        if part == ".." {
            parts.pop()?;
        } else {
            parts.push(part.clone());
        }
    }
    Some(parts)
}

/// Components as a repository-relative path: joined with `/`, `.` for the checkout itself.
pub fn join(parts: &[String]) -> String {
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn below(path: &str, base: &str) -> Option<String> {
        Lexical::parse(path)
            .strip(&Lexical::parse(base))
            .map(|parts| join(&parts))
    }

    #[test]
    fn windows_and_unix_shapes_are_read_on_any_platform() {
        let repo = r"\\?\C:\Users\a\repo";
        assert_eq!(below(r"C:\Users\a\repo\src\a.rs", repo).as_deref(), Some("src/a.rs"));
        assert_eq!(below("c:/users/a/repo/src/a.rs", repo).as_deref(), Some("src/a.rs"));
        assert_eq!(below(r"C:\Users\a\repo", repo).as_deref(), Some("."));
        assert_eq!(below(r"C:\Users\a\repo-old\a.rs", repo), None);
        assert_eq!(below(r"D:\Users\a\repo\a.rs", repo), None);
        assert_eq!(
            below(r"\\?\UNC\Server\Share\repo\a.rs", r"\\server\share\repo").as_deref(),
            Some("a.rs")
        );
        assert_eq!(below("/repo/src/../lib/a.rs", "/repo").as_deref(), Some("lib/a.rs"));
        assert_eq!(below("/Repo/a.rs", "/repo"), None, "case matters outside a drive");
        assert_eq!(below("src/a.rs", "/repo"), None, "a relative path is below nothing");
        assert_eq!(Lexical::parse(r"..\core\.\x.rs").parts(), ["..", "core", "x.rs"]);
    }
}

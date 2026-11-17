//! End-of-line keys read the way git reads them.
//!
//! Git compares the words of `core.eol`, `core.autocrlf` and `core.safecrlf` with `strcasecmp`, and
//! leaves `core.eol` unset when its value is none of them. gix 0.90 matches the words
//! case-sensitively: an unrecognized `core.eol` fails every filter pipeline (blame, diff), and
//! `Input` / `WARN` fall back to other modes. Git for Windows ships `eol = LF` in its system config
//! (GH #137). The values git would accept are rewritten into the spelling gix accepts, as overrides
//! at CLI precedence so they win over every config scope.

/// Each key with the words git matches without regard to case; booleans gix already folds.
const WORD_KEYS: [(&str, &[&str]); 3] = [
    ("core.eol", &["lf", "crlf", "native"]),
    ("core.autocrlf", &["input"]),
    ("core.safecrlf", &["warn"]),
];

/// The `key=value` overrides that make gix read each end-of-line key as git does, given `value`,
/// the effective value of a key. Empty when every key already parses the same in both.
pub(super) fn overrides(value: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    for (key, words) in WORD_KEYS {
        let Some(found) = value(key) else {
            continue;
        };
        if words.contains(&found.as_str()) {
            continue;
        }
        if let Some(word) = words.iter().find(|word| found.eq_ignore_ascii_case(word)) {
            out.push(format!("{key}={word}"));
        } else if key == "core.eol" {
            // Git leaves an unknown `core.eol` unset, which behaves as `native`.
            out.push(format!("{key}=native"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::overrides;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .rev()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn upper_case_words_are_folded() {
        let got = overrides(lookup(&[
            ("core.eol", "LF"),
            ("core.autocrlf", "Input"),
            ("core.safecrlf", "WARN"),
        ]));
        assert_eq!(got, ["core.eol=lf", "core.autocrlf=input", "core.safecrlf=warn"]);
    }

    #[test]
    fn values_gix_already_reads_are_left_alone() {
        let got = overrides(lookup(&[
            ("core.eol", "crlf"),
            ("core.autocrlf", "TRUE"),
            ("core.safecrlf", "false"),
        ]));
        assert!(got.is_empty(), "{got:?}");
        assert!(overrides(lookup(&[])).is_empty());
    }

    #[test]
    fn an_unknown_eol_is_native_as_in_git() {
        assert_eq!(overrides(lookup(&[("core.eol", "auto")])), ["core.eol=native"]);
    }
}

//! `[languages.<name>]` overrides on top of tree-sitter-language-pack detection.
//!
//! [`LangRules`] is compiled once from the config and consulted for every path the scanner, the
//! watcher and the git-history tools classify. Precedence, highest first: an exact `filenames`
//! match, the longest matching `extensions` suffix, then the built-in [`lang::detect`]. A language
//! with `enabled = false` turns every path that resolves to it (by override or built-in) into
//! [`Detection::Disabled`].

use std::collections::BTreeMap;
use std::path::Path;

use ahash::{AHashMap, AHashSet};

use crate::config::LanguageConfig;
use crate::lang::{self, LangId};

/// Outcome of classifying one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// Parse as this grammar.
    Lang(LangId),
    /// Resolved to a grammar the config disabled; handled like an unrecognised file.
    Disabled,
    /// No grammar claims the path.
    Unknown,
}

impl Detection {
    /// The grammar to parse with, or `None` for [`Detection::Disabled`] / [`Detection::Unknown`].
    pub fn lang(self) -> Option<LangId> {
        match self {
            Detection::Lang(l) => Some(l),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LangRules {
    by_filename: AHashMap<String, LangId>,
    /// Lowercase suffixes with a leading dot, longest first so `.html.erb` beats `.erb`.
    by_suffix: Vec<(String, LangId)>,
    disabled: AHashSet<LangId>,
}

impl LangRules {
    /// Compile the `[languages]` table. Errors name the offending table and, for an unknown
    /// grammar, the closest valid names.
    pub fn from_config(languages: &BTreeMap<String, LanguageConfig>) -> Result<Self, String> {
        let mut rules = Self::default();
        let mut suffix_owner: AHashMap<String, &str> = AHashMap::new();
        let mut name_owner: AHashMap<&str, &str> = AHashMap::new();
        for (key, cfg) in languages {
            let key: &str = key;
            let Some(id) = lang::intern(key) else {
                return Err(format!(
                    "[languages.{key}] is not a known tree-sitter grammar{}",
                    nearby_hint(key)
                ));
            };
            if !cfg.enabled {
                rules.disabled.insert(id);
            }
            for ext in &cfg.extensions {
                let suffix = normalize_suffix(ext);
                if suffix.contains('/') {
                    return Err(format!(
                        "[languages.{key}] extensions entry {ext:?} contains '/', so it could never match; \
                         extensions are matched against the file name only"
                    ));
                }
                if suffix.len() < 2 {
                    return Err(format!("[languages.{key}] extensions entry {ext:?} is empty"));
                }
                if let Some(prev) = suffix_owner.insert(suffix.clone(), key)
                    && prev != key
                {
                    return Err(format!(
                        "extension {suffix} is mapped by both [languages.{prev}] and [languages.{key}]"
                    ));
                }
                rules.by_suffix.push((suffix, id));
            }
            for name in &cfg.filenames {
                if name.is_empty() || name.contains('/') {
                    return Err(format!(
                        "[languages.{key}] filenames entry {name:?} must be a bare file name"
                    ));
                }
                if let Some(prev) = name_owner.insert(name.as_str(), key)
                    && prev != key
                {
                    return Err(format!(
                        "filename {name:?} is mapped by both [languages.{prev}] and [languages.{key}]"
                    ));
                }
                rules.by_filename.insert(name.clone(), id);
            }
        }
        rules
            .by_suffix
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(rules)
    }

    /// True when no override or disable is configured, so [`Self::detect`] is just [`lang::detect`].
    pub fn is_empty(&self) -> bool {
        self.by_filename.is_empty() && self.by_suffix.is_empty() && self.disabled.is_empty()
    }

    pub fn detect(&self, path: &Path) -> Detection {
        let resolved = self.resolve(path);
        match resolved {
            Some(id) if self.disabled.contains(id) => Detection::Disabled,
            Some(id) => Detection::Lang(id),
            None => Detection::Unknown,
        }
    }

    fn resolve(&self, path: &Path) -> Option<LangId> {
        if !self.by_filename.is_empty() || !self.by_suffix.is_empty() {
            let name = path.file_name().and_then(|n| n.to_str());
            if let Some(name) = name {
                if let Some(&id) = self.by_filename.get(name) {
                    return Some(id);
                }
                // Strictly longer than the suffix: a file named exactly `.mako` has no stem, so it is
                // matched through `filenames`, not `extensions`.
                if !self.by_suffix.is_empty() {
                    let lower = name.to_ascii_lowercase();
                    if let Some((_, id)) = self
                        .by_suffix
                        .iter()
                        .find(|(s, _)| lower.len() > s.len() && lower.ends_with(s))
                    {
                        return Some(id);
                    }
                }
            }
        }
        lang::detect(path)
    }

    /// Grammar names configured with `preload = true`, resolved and deduplicated.
    pub fn preload_names(languages: &BTreeMap<String, LanguageConfig>) -> Vec<LangId> {
        languages
            .iter()
            .filter(|(_, c)| c.preload && c.enabled)
            .filter_map(|(k, _)| lang::intern(k))
            .collect()
    }
}

/// `".Mako"` / `"mako"` -> `".mako"`.
fn normalize_suffix(ext: &str) -> String {
    let t = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    if t.is_empty() { String::new() } else { format!(".{t}") }
}

fn nearby_hint(key: &str) -> String {
    let needle = key.to_ascii_lowercase();
    let mut scored: Vec<(usize, String)> = tree_sitter_language_pack::available_languages()
        .into_iter()
        .filter_map(|name| {
            let d = edit_distance(&needle, &name.to_ascii_lowercase());
            let close = d <= 2 || name.contains(&needle) || needle.contains(name.as_str());
            close.then_some((d, name))
        })
        .collect();
    scored.sort();
    scored.truncate(5);
    if scored.is_empty() {
        " (run `basemind lang list` to see the available grammars)".to_string()
    } else {
        let names: Vec<String> = scored.into_iter().map(|(_, n)| n).collect();
        format!("; did you mean: {}?", names.join(", "))
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(&str, LanguageConfig)]) -> BTreeMap<String, LanguageConfig> {
        entries.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    fn cfg(extensions: &[&str], filenames: &[&str], enabled: bool) -> LanguageConfig {
        LanguageConfig {
            enabled,
            extensions: extensions.iter().map(|s| (*s).to_string()).collect(),
            filenames: filenames.iter().map(|s| (*s).to_string()).collect(),
            preload: false,
        }
    }

    #[test]
    fn empty_rules_defer_to_builtin_detection() {
        let rules = LangRules::from_config(&BTreeMap::new()).expect("empty table compiles");
        assert!(rules.is_empty());
        assert_eq!(rules.detect(Path::new("src/main.rs")), Detection::Lang("rust"));
        assert_eq!(rules.detect(Path::new("notes.unknownext")), Detection::Unknown);
    }

    #[test]
    fn extension_and_filename_overrides_beat_builtin_detection() {
        let rules = LangRules::from_config(&table(&[
            ("python", cfg(&[".MAKO", "tpl"], &["BUILD.in"], true)),
            ("javascript", cfg(&[".py"], &[], true)),
        ]))
        .expect("compiles");
        assert_eq!(rules.detect(Path::new("t/page.mako")), Detection::Lang("python"));
        assert_eq!(rules.detect(Path::new("t/PAGE.TPL")), Detection::Lang("python"));
        assert_eq!(rules.detect(Path::new("pkg/BUILD.in")), Detection::Lang("python"));
        assert_eq!(
            rules.detect(Path::new("a/script.py")),
            Detection::Lang("javascript"),
            "an override beats the built-in mapping"
        );
    }

    #[test]
    fn longest_suffix_wins() {
        let rules = LangRules::from_config(&table(&[
            ("html", cfg(&[".erb"], &[], true)),
            ("ruby", cfg(&[".html.erb"], &[], true)),
        ]))
        .expect("compiles");
        assert_eq!(rules.detect(Path::new("v/show.html.erb")), Detection::Lang("ruby"));
        assert_eq!(rules.detect(Path::new("v/show.erb")), Detection::Lang("html"));
    }

    #[test]
    fn disabled_language_is_reported_for_builtin_and_overridden_paths() {
        let probe = tree_sitter_language_pack::detect_language("notes.txt").expect("txt is detected by the pack");
        let rules = LangRules::from_config(&table(&[(probe, cfg(&[".weird"], &[], false))])).expect("compiles");
        assert_eq!(rules.detect(Path::new("notes.txt")), Detection::Disabled);
        assert_eq!(rules.detect(Path::new("a.weird")), Detection::Disabled);
        assert_eq!(rules.detect(Path::new("src/main.rs")), Detection::Lang("rust"));
        assert_eq!(rules.detect(Path::new("notes.txt")).lang(), None);
    }

    #[test]
    fn unknown_grammar_error_lists_nearby_names() {
        let err = LangRules::from_config(&table(&[("pythn", cfg(&[], &[], true))])).expect_err("typo rejected");
        assert!(err.contains("[languages.pythn]"), "{err}");
        assert!(err.contains("python"), "suggests the near miss: {err}");
    }

    #[test]
    fn conflicting_claims_are_rejected() {
        let err = LangRules::from_config(&table(&[
            ("python", cfg(&[".x"], &[], true)),
            ("ruby", cfg(&["x"], &[], true)),
        ]))
        .expect_err("same extension on two grammars");
        assert!(err.contains(".x"), "{err}");
        let err = LangRules::from_config(&table(&[("python", cfg(&[""], &[], true))])).expect_err("empty extension");
        assert!(err.contains("empty"), "{err}");
        let err = LangRules::from_config(&table(&[("python", cfg(&[], &["a/b"], true))])).expect_err("path filename");
        assert!(err.contains("bare file name"), "{err}");
        let err = LangRules::from_config(&table(&[("python", cfg(&[".a/b"], &[], true))])).expect_err("path extension");
        assert!(err.contains("could never match"), "{err}");
    }

    #[test]
    fn preload_names_skips_disabled_and_unknown() {
        let mut t = table(&[("python", cfg(&[], &[], true)), ("ruby", cfg(&[], &[], false))]);
        t.get_mut("python").unwrap().preload = true;
        t.get_mut("ruby").unwrap().preload = true;
        assert_eq!(LangRules::preload_names(&t), vec!["python"]);
    }
}

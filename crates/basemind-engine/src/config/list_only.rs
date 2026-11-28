//! The list-only tier: files the scan records as present (path, size, modification time, kind) and
//! never opens. Lockfiles, archives, packet captures, fonts, columnar data and other binaries are
//! known to exist, so an agent can ask where they are, but no extractor, chunker or embedder sees
//! their bytes.
//!
//! `[scan] list_only` is an ordered glob list compiled by [`compile_rules`], so a `!pattern` entry
//! re-includes a file an earlier entry listed (a lockfile worth indexing). Patterns match file
//! names and paths, never extensions alone: `pnpm-lock.yaml` and `.terraform.lock.hcl` are YAML and
//! HCL by extension, and are still lockfiles.

use serde::{Deserialize, Serialize};

use super::rules::{RuleSet, compile_rules};

/// Named lockfiles: machine-written, large, and of no use to a symbol or text search.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "pdm.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "mix.lock",
    "pubspec.lock",
    "Podfile.lock",
    "Package.resolved",
    "gradle.lockfile",
    "flake.lock",
    "packages.lock.json",
    "Chart.lock",
    "MODULE.bazel.lock",
    ".terraform.lock.hcl",
];

/// Archives and compressed bundles.
const ARCHIVES: &[&str] = &[
    "*.zip",
    "*.tar",
    "*.tar.gz",
    "*.tgz",
    "*.tar.bz2",
    "*.tar.xz",
    "*.tar.zst",
    "*.gz",
    "*.bz2",
    "*.xz",
    "*.zst",
    "*.7z",
    "*.rar",
    "*.jar",
    "*.war",
    "*.whl",
    "*.iso",
    "*.dmg",
];

/// Packet captures.
const CAPTURES: &[&str] = &["*.pcap", "*.pcapng", "*.cap"];

/// Font files.
const FONTS: &[&str] = &["*.ttf", "*.otf", "*.woff", "*.woff2", "*.eot"];

/// Columnar and database data files.
const DATA: &[&str] = &[
    "*.parquet",
    "*.avro",
    "*.orc",
    "*.feather",
    "*.arrow",
    "*.sqlite",
    "*.sqlite3",
    "*.npy",
    "*.npz",
];

/// Compiled objects and large media.
const BINARIES: &[&str] = &[
    "*.exe", "*.dll", "*.so", "*.dylib", "*.a", "*.o", "*.class", "*.wasm", "*.mp4", "*.mov", "*.mkv", "*.avi",
    "*.webm", "*.mp3", "*.wav", "*.flac", "*.ogg",
];

/// The default `[scan] list_only` list, in the order `basemind init` writes it. The last group is
/// the test data of [`super::documents::TEST_DATA_GLOBS`], so the scan lists fixtures and recorded
/// responses instead of extracting them.
pub const LIST_ONLY_GLOBS: &[&[&str]] = &[
    LOCKFILES,
    ARCHIVES,
    CAPTURES,
    FONTS,
    DATA,
    BINARIES,
    super::documents::TEST_DATA_GLOBS,
];

/// [`LIST_ONLY_GLOBS`] flattened into the owned list a config holds.
pub fn default_list_only() -> Vec<String> {
    LIST_ONLY_GLOBS
        .iter()
        .flat_map(|group| group.iter())
        .map(|glob| (*glob).to_string())
        .collect()
}

/// What a list-only file is, for `find_files` and `index_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListKind {
    /// A dependency lockfile.
    Lockfile,
    /// An archive or compressed bundle.
    Archive,
    /// A font, compiled object or media file.
    Binary,
    /// A packet capture or a columnar, database or compressed data file.
    Data,
}

impl ListKind {
    /// The lowercase name the wire and `index_status` use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lockfile => "lockfile",
            Self::Archive => "archive",
            Self::Binary => "binary",
            Self::Data => "data",
        }
    }
}

/// A list-only file as the file map records it: stat data only, never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedEntry {
    pub kind: ListKind,
    pub size_bytes: u64,
    /// Modification time in nanoseconds since the epoch; `0` when unknown.
    pub mtime: i64,
}

const DATA_SUFFIXES: &[&str] = &[
    ".csv.gz",
    ".json.gz",
    ".jsonl.gz",
    ".ndjson.gz",
    ".parquet",
    ".avro",
    ".orc",
    ".feather",
    ".arrow",
    ".sqlite",
    ".sqlite3",
    ".npy",
    ".npz",
    ".pcap",
    ".pcapng",
    ".cap",
];

const ARCHIVE_SUFFIXES: &[&str] = &[
    ".zip", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".zst", ".7z", ".rar", ".jar", ".war", ".whl", ".iso", ".dmg",
];

const BINARY_SUFFIXES: &[&str] = &[
    ".ttf", ".otf", ".woff", ".woff2", ".eot", ".exe", ".dll", ".so", ".dylib", ".a", ".o", ".class", ".wasm", ".mp4",
    ".mov", ".mkv", ".avi", ".webm", ".mp3", ".wav", ".flac", ".ogg",
];

/// Classify `rel` by its file name. A user pattern may list any path, so a name that matches no
/// known shape is [`ListKind::Data`].
pub fn kind_of(rel: &str) -> ListKind {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    let ends = |suffixes: &[&str]| suffixes.iter().any(|suffix| name.ends_with(suffix));
    if LOCKFILES.iter().any(|lock| lock.eq_ignore_ascii_case(&name))
        || ends(&[
            ".lock",
            ".lockb",
            ".lockfile",
            ".lock.json",
            ".lock.hcl",
            ".lock.yaml",
            ".sum",
        ])
    {
        ListKind::Lockfile
    } else if ends(DATA_SUFFIXES) {
        ListKind::Data
    } else if ends(ARCHIVE_SUFFIXES) {
        ListKind::Archive
    } else if ends(BINARY_SUFFIXES) {
        ListKind::Binary
    } else {
        ListKind::Data
    }
}

/// The compiled `[scan] list_only` list.
#[derive(Debug, Clone, Default)]
pub struct ListOnly(RuleSet);

impl ListOnly {
    /// Compile `patterns`; an invalid glob is a config error.
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        compile_rules(patterns.iter().map(String::as_str)).map(Self)
    }

    /// Does the list select the repo-relative, forward-slash `rel`?
    pub fn is_match(&self, rel: &str) -> bool {
        self.0.is_match(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(patterns: &[&str]) -> ListOnly {
        ListOnly::new(&patterns.iter().map(|p| (*p).to_string()).collect::<Vec<_>>()).expect("valid globs")
    }

    #[test]
    fn default_list_matches_by_name_not_extension() {
        let defaults = ListOnly::new(&default_list_only()).expect("defaults compile");
        for listed in [
            "infra/prod/.terraform.lock.hcl",
            "pnpm-lock.yaml",
            "web/pnpm-lock.yaml",
            "package-lock.json",
            "Cargo.lock",
            "go.sum",
            "capture/session.pcap",
            "a/b/old.cap",
            "dump.tar.gz",
            "assets/font.woff2",
            "warehouse/part-0.parquet",
            "data/rows.csv.gz",
        ] {
            assert!(defaults.is_match(listed), "{listed} must be list-only");
        }
        for indexed in [
            "src/main.rs",
            "config.yaml",
            "package.json",
            "docs/lock.md",
            "terraform/main.tf",
            "capture.rs",
        ] {
            assert!(!defaults.is_match(indexed), "{indexed} must stay indexed");
        }
    }

    #[test]
    fn negation_re_includes_and_a_directory_can_be_listed() {
        let rules = list(&["Cargo.lock", "!crates/app/Cargo.lock", "generated/"]);
        assert!(rules.is_match("Cargo.lock"));
        assert!(rules.is_match("crates/lib/Cargo.lock"));
        assert!(!rules.is_match("crates/app/Cargo.lock"));
        assert!(rules.is_match("generated/api/model.rs"));
        assert!(!rules.is_match("src/model.rs"));
    }

    #[test]
    fn an_empty_list_matches_nothing() {
        assert!(!list(&[]).is_match("Cargo.lock"));
    }

    #[test]
    fn a_bad_glob_is_an_error() {
        assert!(ListOnly::new(&["[".to_string()]).is_err());
    }

    #[test]
    fn kinds_follow_the_file_name() {
        for (path, kind) in [
            ("a/.terraform.lock.hcl", ListKind::Lockfile),
            ("a/pnpm-lock.yaml", ListKind::Lockfile),
            ("deps.sum", ListKind::Lockfile),
            ("x/y/Gemfile.lock", ListKind::Lockfile),
            ("a.zip", ListKind::Archive),
            ("a.tar.gz", ListKind::Archive),
            ("a.csv.gz", ListKind::Data),
            ("cap.pcap", ListKind::Data),
            ("t.parquet", ListKind::Data),
            ("f.woff2", ListKind::Binary),
            ("lib.so", ListKind::Binary),
            ("unknown.xyz", ListKind::Data),
        ] {
            assert_eq!(kind_of(path), kind, "{path}");
        }
    }

    #[test]
    fn every_default_glob_has_a_non_fallback_kind() {
        // Test data is data by design: its globs name directories and formats, not a known suffix.
        let named = LIST_ONLY_GLOBS
            .iter()
            .filter(|group| **group != super::super::documents::TEST_DATA_GLOBS);
        for glob in named.flat_map(|group| group.iter()) {
            let sample = glob.replace('*', "sample");
            let kind = kind_of(&sample);
            if kind == ListKind::Data {
                assert!(
                    DATA_SUFFIXES.iter().any(|suffix| sample.ends_with(suffix)),
                    "{glob} falls through to the data fallback"
                );
            }
        }
    }
}

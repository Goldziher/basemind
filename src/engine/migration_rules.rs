//! Migration rule files (#130): `.basemind/migrations/<name>.toml`, authored in the repository and
//! read by `basemind code migration check`.
//!
//! A rule file is repository content, so it is read as untrusted input: every table refuses an
//! unknown key, every string, list and the file itself are bounded, a regular expression is
//! compiled under a size limit, a glob may not leave the repository, and a rule file reached
//! through a symlink, or a migrations directory that resolves outside the root, is refused.
//!
//! ```toml
//! description = "Move logging from old_log to tracing"
//!
//! [[rule]]
//! name = "log-calls"
//! guidance = "Call tracing::info! instead"
//! include = ["src/**"]
//! exclude = ["src/legacy/**"]
//! match = [{ call = "old_log::info" }, { text = 'old_log!\(', regex = true }]
//! expect = [{ call = "tracing::info" }]
//! ```
//!
//! A pattern names exactly one of `call` (a call site's callee), `import` (an import's module),
//! `identifier` (a definition's name or a callee) or `text` (a line of the file); it is literal
//! unless `regex = true`.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;

use super::error::EngineError;
use crate::engine::refs::matching::Matching;

/// Where rule files live, relative to the repository root.
pub(crate) const MIGRATIONS_DIR: &str = ".basemind/migrations";
/// The largest rule file read.
const FILE_BYTES_MAX: u64 = 64 * 1024;
/// Rule files `check` with no name reads.
const FILES_MAX: usize = 64;
/// Rules one file may declare.
const RULES_MAX: usize = 100;
/// Patterns one `match` or `expect` list may hold, and globs one `include` or `exclude` list.
const LIST_MAX: usize = 32;
/// The longest pattern or glob, in bytes.
const PATTERN_BYTES_MAX: usize = 512;
/// The longest `description` or `guidance`, in bytes.
const TEXT_BYTES_MAX: usize = 1024;
/// The compiled size a regular expression may reach.
const REGEX_SIZE_MAX: usize = 1 << 20;
/// The longest migration or rule name.
const NAME_MAX: usize = 64;

/// A rule file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    #[serde(default)]
    guidance: Option<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(rename = "match")]
    matches: Vec<RawPattern>,
    #[serde(default)]
    expect: Vec<RawPattern>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPattern {
    #[serde(default)]
    call: Option<String>,
    #[serde(default)]
    import: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    regex: bool,
}

/// A checked migration: its rule file's rules, compiled.
#[derive(Debug)]
pub(crate) struct Migration {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) rules: Vec<Rule>,
}

/// One compiled rule.
#[derive(Debug)]
pub(crate) struct Rule {
    pub(crate) name: String,
    pub(crate) guidance: Option<String>,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    pub(crate) matches: Vec<Pattern>,
    pub(crate) expect: Vec<Pattern>,
}

impl Rule {
    /// Whether the rule looks at `path`: some `include` glob matches it (or there is none) and no
    /// `exclude` glob does.
    pub(crate) fn selects(&self, path: &str) -> bool {
        self.include.as_ref().is_none_or(|set| set.is_match(path))
            && self.exclude.as_ref().is_none_or(|set| !set.is_match(path))
    }

    /// Whether any pattern of the rule matches against `target`.
    pub(crate) fn uses(&self, target: Target) -> bool {
        self.matches.iter().chain(&self.expect).any(|p| p.target == target)
    }

    /// Whether any pattern of the rule reads the file's text: a `text` or `import` pattern, or a
    /// call pattern that checks the call expression ([`Pattern::reads_call_source`]).
    pub(crate) fn reads_text(&self) -> bool {
        self.matches.iter().chain(&self.expect).any(|p| match p.target {
            Target::Text | Target::Import => true,
            Target::Call | Target::Identifier => p.reads_call_source(),
        })
    }
}

/// What a pattern is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Call,
    Import,
    Identifier,
    Text,
}

/// One compiled pattern.
#[derive(Debug)]
pub(crate) struct Pattern {
    pub(crate) target: Target,
    matcher: Matcher,
    /// A `text` pattern's line regex, compiled once: the literal escaped, `^` / `$` at each line.
    line: Option<Regex>,
}

#[derive(Debug)]
enum Matcher {
    Literal(String),
    Regex(Regex),
}

impl Pattern {
    /// Whether a call site is this pattern. `callee` is the name the index stored and `source` the
    /// file's text from the call's first byte, when it was read.
    ///
    /// A literal is the callee, or a qualified path ending in it (`code_refs` exact matching). An
    /// index that stores only a call's last segment (Rust stores `info` for `old_log::info(..)`)
    /// cannot tell `old_log::info` from `tracing::info`, so a qualified literal whose last segment
    /// is the callee is checked against the call expression in the source: the text before its
    /// `(`, `!` or `<` must end in the literal. A regex matches the stored callee or that
    /// expression.
    pub(crate) fn matches_call(&self, callee: &str, source: Option<&str>) -> bool {
        let expression = || source.map(call_expression);
        match &self.matcher {
            Matcher::Literal(name) => {
                Matching::Exact.accepts_callee(callee, name)
                    || (last_segment(name) != name.as_str()
                        && Matching::Exact.accepts_callee(callee, last_segment(name))
                        && expression().is_some_and(|expression| Matching::Exact.accepts_callee(expression, name)))
            }
            Matcher::Regex(re) => re.is_match(callee) || expression().is_some_and(|expression| re.is_match(expression)),
        }
    }

    /// Whether [`Self::matches_call`] reads the source: a regex, or a qualified literal.
    pub(crate) fn reads_call_source(&self) -> bool {
        match &self.matcher {
            Matcher::Literal(name) => last_segment(name) != name.as_str(),
            Matcher::Regex(_) => true,
        }
    }

    /// Whether a definition's name is this pattern: equal to the literal, or matched by the regex.
    pub(crate) fn matches_name(&self, name: &str) -> bool {
        match &self.matcher {
            Matcher::Literal(literal) => name == literal,
            Matcher::Regex(re) => re.is_match(name),
        }
    }

    /// Whether an import is this pattern. A literal names the imported module or a module above it
    /// (`old_log` covers `old_log::Logger`, `old-lib/sub` and `old.mod`): a prefix of the module the
    /// extractor parsed, or, where it parsed none, a path inside the import statement `raw` that
    /// starts at a word boundary (so `old_log` does not cover `crate::old_log` or `old_logger`).
    /// A regex matches anywhere in the module, else in `raw`.
    pub(crate) fn matches_import(&self, module: Option<&str>, raw: &str) -> bool {
        match (&self.matcher, module) {
            (Matcher::Literal(literal), Some(module)) => {
                module.strip_prefix(literal.as_str()).is_some_and(ends_a_module_path)
            }
            (Matcher::Literal(literal), None) => {
                memchr::memmem::find_iter(raw.as_bytes(), literal.as_bytes()).any(|at| {
                    let before = raw[..at].chars().next_back();
                    before
                        .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '/' | '-' | '@' | '$')))
                        && ends_a_module_path(&raw[at + literal.len()..])
                })
            }
            (Matcher::Regex(re), module) => re.is_match(module.unwrap_or(raw)),
        }
    }

    /// The line regex of a `text` pattern; `None` for any other target.
    pub(crate) fn line_regex(&self) -> Option<&Regex> {
        self.line.as_ref()
    }
}

/// Whether the text after a module name ends that name: nothing, or a character no module name
/// continues with (`::`, `.`, `/`, `;`, a quote, a space).
fn ends_a_module_path(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '$')))
}

/// The last segment of a qualified name: `info` of `old_log::info`, `get` of `client.get`.
fn last_segment(name: &str) -> &str {
    let cut = [
        name.rfind("::").map(|at| at + 2),
        name.rfind('.').map(|at| at + 1),
        name.rfind("->").map(|at| at + 2),
    ]
    .into_iter()
    .flatten()
    .max();
    cut.map_or(name, |at| &name[at..])
}

/// Where the qualified callee path ending at byte `name` of `text` starts: back from a call's name
/// over identifier characters and the `::`, `.` and `->` separators, so `old_log::info(` and
/// `client.get(` are read whole although the call is recorded at its name. Whitespace ends the path,
/// so a chain link on its own line (`\n    .get(`) yields `.get`.
pub(crate) fn qualified_start(text: &str, name: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = name.min(bytes.len());
    while at > 0 {
        let b = bytes[at - 1];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'.' | b':') {
            at -= 1;
        } else if b == b'>' && at >= 2 && bytes[at - 2] == b'-' {
            at -= 2;
        } else {
            break;
        }
    }
    at
}

/// The callee expression a call's source starts with: the text before its arguments, generic
/// arguments or macro bang, on the call's first line.
fn call_expression(source: &str) -> &str {
    let end = source
        .find(|c: char| matches!(c, '(' | '!' | '<' | '\n' | '\r') || c.is_whitespace())
        .unwrap_or(source.len());
    &source[..end]
}

/// Reads the migration `name`, or every rule file when `name` is `None`, sorted by name.
pub(crate) fn load(root: &Path, name: Option<&str>) -> Result<Vec<Migration>, EngineError> {
    let dir = migrations_dir(root)?;
    let names = match name {
        Some(name) => {
            check_name("migration", name)?;
            vec![name.to_string()]
        }
        None => list(&dir)?,
    };
    if names.is_empty() {
        return Err(EngineError::invalid_argument(format!(
            "`code migration check`: no rule file in `{MIGRATIONS_DIR}/`. Write one as \
             `{MIGRATIONS_DIR}/<name>.toml` with at least one `[[rule]]`."
        )));
    }
    names
        .into_iter()
        .map(|name| {
            let file = dir.join(format!("{name}.toml"));
            let text = read_rule_file(&file, &name, &dir)?;
            parse(&name, &text)
        })
        .collect()
}

/// The migrations directory under `root`, refused when it resolves outside the root.
fn migrations_dir(root: &Path) -> Result<PathBuf, EngineError> {
    let dir = root.join(MIGRATIONS_DIR);
    let (Ok(resolved), Ok(root)) = (dir.canonicalize(), root.canonicalize()) else {
        return Ok(dir);
    };
    if resolved.starts_with(&root) {
        Ok(dir)
    } else {
        Err(EngineError::invalid_argument(format!(
            "`code migration check`: `{MIGRATIONS_DIR}` leads outside the repository; rule files must \
             live in it."
        )))
    }
}

/// The names of the rule files in `dir`, sorted; none when the directory does not exist.
fn list(dir: &Path) -> Result<Vec<String>, EngineError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(EngineError::internal(format!("read `{MIGRATIONS_DIR}`: {error}")));
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let stem = file_name.to_str()?.strip_suffix(".toml")?;
            valid_name(stem).then(|| stem.to_string())
        })
        .collect();
    names.sort_unstable();
    if names.len() > FILES_MAX {
        return Err(EngineError::invalid_argument(format!(
            "`code migration check`: `{MIGRATIONS_DIR}` holds {} rule files, more than {FILES_MAX}; name the \
             migration to check.",
            names.len()
        )));
    }
    Ok(names)
}

/// The text of the rule file `file`: a regular file (not a symlink), at most [`FILE_BYTES_MAX`].
fn read_rule_file(file: &Path, name: &str, dir: &Path) -> Result<String, EngineError> {
    let shown = format!("{MIGRATIONS_DIR}/{name}.toml");
    let metadata = match std::fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let known = list(dir).unwrap_or_default();
            let known = if known.is_empty() {
                "there is none yet".to_string()
            } else {
                format!("known: {}", known.join(", "))
            };
            return Err(EngineError::invalid_argument(format!(
                "`code migration check`: no rule file `{shown}` ({known})."
            )));
        }
        Err(error) => return Err(EngineError::internal(format!("read `{shown}`: {error}"))),
    };
    if !metadata.file_type().is_file() {
        return Err(EngineError::invalid_argument(format!(
            "`code migration check`: `{shown}` is not a regular file; a rule file may not be a symlink."
        )));
    }
    if metadata.len() > FILE_BYTES_MAX {
        return Err(EngineError::invalid_argument(format!(
            "`code migration check`: `{shown}` is {} bytes, over the {FILE_BYTES_MAX}-byte limit.",
            metadata.len()
        )));
    }
    let bytes = std::fs::read(file).map_err(|error| EngineError::internal(format!("read `{shown}`: {error}")))?;
    if bytes.len() as u64 > FILE_BYTES_MAX {
        return Err(EngineError::invalid_argument(format!(
            "`code migration check`: `{shown}` is over the {FILE_BYTES_MAX}-byte limit."
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| EngineError::invalid_argument(format!("`code migration check`: `{shown}` is not UTF-8.")))
}

/// Compiles the rule file `text` of migration `name`.
pub(crate) fn parse(name: &str, text: &str) -> Result<Migration, EngineError> {
    let shown = format!("{MIGRATIONS_DIR}/{name}.toml");
    let invalid = |problem: String| EngineError::invalid_argument(format!("`{shown}`: {problem}"));
    let raw: RawFile = toml::from_str(text).map_err(|error| invalid(error.to_string()))?;
    if let Some(description) = &raw.description {
        bounded_text("description", description).map_err(invalid)?;
    }
    if raw.rule.is_empty() {
        return Err(invalid("declares no `[[rule]]`".to_string()));
    }
    if raw.rule.len() > RULES_MAX {
        return Err(invalid(format!(
            "declares {} rules, more than {RULES_MAX}",
            raw.rule.len()
        )));
    }
    let mut rules: Vec<Rule> = Vec::with_capacity(raw.rule.len());
    for rule in raw.rule {
        if rules.iter().any(|seen| seen.name == rule.name) {
            return Err(invalid(format!("two rules are named `{}`", rule.name)));
        }
        let at = |problem: String| invalid(format!("rule `{}`: {problem}", rule.name));
        check_name("rule", &rule.name).map_err(|_| {
            invalid(format!(
                "rule name `{}` must be 1-{NAME_MAX} of `A-Z a-z 0-9 _ . -`, starting with a letter or digit",
                rule.name
            ))
        })?;
        if let Some(guidance) = &rule.guidance {
            bounded_text("guidance", guidance).map_err(at)?;
        }
        if rule.matches.is_empty() {
            return Err(at("`match` is empty; name at least one old form".to_string()));
        }
        let include = globs("include", &rule.include).map_err(at)?;
        let exclude = globs("exclude", &rule.exclude).map_err(at)?;
        let matches = patterns("match", rule.matches).map_err(at)?;
        let expect = patterns("expect", rule.expect).map_err(at)?;
        rules.push(Rule {
            name: rule.name,
            guidance: rule.guidance,
            include,
            exclude,
            matches,
            expect,
        });
    }
    Ok(Migration {
        name: name.to_string(),
        description: raw.description,
        rules,
    })
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= NAME_MAX
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn check_name(what: &str, name: &str) -> Result<(), EngineError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(EngineError::invalid_argument(format!(
            "`code migration check`: {what} name `{name}` must be 1-{NAME_MAX} of `A-Z a-z 0-9 _ . -`, starting \
             with a letter or digit; it names `{MIGRATIONS_DIR}/<name>.toml`."
        )))
    }
}

fn bounded_text(field: &str, text: &str) -> Result<(), String> {
    if text.len() > TEXT_BYTES_MAX {
        Err(format!("`{field}` is over {TEXT_BYTES_MAX} bytes"))
    } else {
        Ok(())
    }
}

/// The glob set of `list`, `None` when it is empty. A glob is repository-relative: no leading `/`,
/// drive or `\`, and no `..` segment.
fn globs(field: &str, list: &[String]) -> Result<Option<GlobSet>, String> {
    if list.is_empty() {
        return Ok(None);
    }
    if list.len() > LIST_MAX {
        return Err(format!("`{field}` holds {} globs, more than {LIST_MAX}", list.len()));
    }
    let mut set = GlobSetBuilder::new();
    for glob in list {
        if glob.is_empty() || glob.len() > PATTERN_BYTES_MAX {
            return Err(format!("`{field}` glob must be 1-{PATTERN_BYTES_MAX} bytes"));
        }
        let drive = glob.as_bytes().get(1) == Some(&b':') && glob.as_bytes()[0].is_ascii_alphabetic();
        if glob.starts_with('/') || glob.contains('\\') || drive || glob.split('/').any(|segment| segment == "..") {
            return Err(format!(
                "`{field}` glob `{glob}` leaves the repository; write it relative to the root, without `..`"
            ));
        }
        let compiled = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("`{field}` glob `{glob}`: {error}"))?;
        set.add(compiled);
    }
    set.build().map(Some).map_err(|error| format!("`{field}`: {error}"))
}

/// The compiled patterns of `list`, which `field` names.
fn patterns(field: &str, list: Vec<RawPattern>) -> Result<Vec<Pattern>, String> {
    if list.len() > LIST_MAX {
        return Err(format!("`{field}` holds {} patterns, more than {LIST_MAX}", list.len()));
    }
    list.into_iter().map(|raw| pattern(field, raw)).collect()
}

fn pattern(field: &str, raw: RawPattern) -> Result<Pattern, String> {
    let named: Vec<(Target, String)> = [
        (Target::Call, raw.call),
        (Target::Import, raw.import),
        (Target::Identifier, raw.identifier),
        (Target::Text, raw.text),
    ]
    .into_iter()
    .filter_map(|(target, value)| value.map(|value| (target, value)))
    .collect();
    let [(target, value)] = <[_; 1]>::try_from(named)
        .map_err(|_| format!("each `{field}` pattern names exactly one of `call`, `import`, `identifier` or `text`"))?;
    if value.is_empty() || value.len() > PATTERN_BYTES_MAX {
        return Err(format!("`{field}` pattern must be 1-{PATTERN_BYTES_MAX} bytes"));
    }
    let text = target == Target::Text;
    let line = if text {
        let source = if raw.regex {
            value.clone()
        } else {
            regex::escape(&value)
        };
        Some(compile(&source, true).map_err(|error| format!("`{field}` regex `{value}`: {error}"))?)
    } else {
        None
    };
    let matcher = match (raw.regex, &line) {
        (true, Some(line)) => Matcher::Regex(line.clone()),
        (true, None) => {
            Matcher::Regex(compile(&value, false).map_err(|error| format!("`{field}` regex `{value}`: {error}"))?)
        }
        (false, _) => Matcher::Literal(value),
    };
    Ok(Pattern { target, matcher, line })
}

/// `source` compiled under [`REGEX_SIZE_MAX`]; `lines` anchors `^` / `$` at every line, as grep.
fn compile(source: &str, lines: bool) -> Result<Regex, regex::Error> {
    regex::RegexBuilder::new(source)
        .multi_line(lines)
        .crlf(lines)
        .size_limit(REGEX_SIZE_MAX)
        .dfa_size_limit(REGEX_SIZE_MAX)
        .build()
}

#[cfg(test)]
#[path = "migration_rules_tests.rs"]
mod tests;

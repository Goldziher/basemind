//! Rule files are repository content: each refusal below is a shape a hostile or careless file can
//! take, and each acceptance pins the matching semantics the check relies on.

use super::*;

fn refusal(text: &str) -> String {
    match parse("m", text) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(error) => error.to_string(),
    }
}

const MINIMAL: &str = "[[rule]]\nname = \"r\"\nmatch = [{ call = \"old\" }]\n";

#[test]
fn a_minimal_rule_file_compiles() {
    let migration = parse("m", MINIMAL).expect("valid");
    assert_eq!(migration.rules.len(), 1);
    assert_eq!(migration.rules[0].matches[0].target, Target::Call);
    assert!(migration.rules[0].expect.is_empty());
    assert!(migration.rules[0].selects("any/path.rs"));
}

#[test]
fn an_unknown_key_is_refused_at_every_level() {
    assert!(refusal(&format!("owner = \"x\"\n{MINIMAL}")).contains("owner"));
    assert!(refusal("[[rule]]\nname = \"r\"\nseverity = 1\nmatch = [{ call = \"old\" }]\n").contains("severity"));
    assert!(refusal("[[rule]]\nname = \"r\"\nmatch = [{ call = \"old\", path = \"/etc\" }]\n").contains("path"));
}

#[test]
fn a_pattern_names_exactly_one_target() {
    assert!(refusal("[[rule]]\nname = \"r\"\nmatch = [{ call = \"a\", text = \"b\" }]\n").contains("exactly one"));
    assert!(refusal("[[rule]]\nname = \"r\"\nmatch = [{ regex = true }]\n").contains("exactly one"));
}

#[test]
fn a_rule_needs_a_name_a_match_and_the_file_a_rule() {
    assert!(refusal("description = \"x\"\n").contains("no `[[rule]]`"));
    assert!(refusal("[[rule]]\nname = \"r\"\nmatch = []\n").contains("`match` is empty"));
    assert!(refusal("[[rule]]\nmatch = [{ call = \"a\" }]\n").contains("name"));
    assert!(refusal(&format!("{MINIMAL}{MINIMAL}")).contains("two rules"));
    assert!(refusal("[[rule]]\nname = \"../x\"\nmatch = [{ call = \"a\" }]\n").contains("rule name"));
}

#[test]
fn a_glob_may_not_leave_the_repository() {
    for glob in ["/etc/**", "../**", "src/../../x", "C:/x", "src\\\\x"] {
        let text = format!("[[rule]]\nname = \"r\"\ninclude = [\"{glob}\"]\nmatch = [{{ call = \"a\" }}]\n");
        assert!(refusal(&text).contains("leaves the repository"), "{glob}");
    }
}

#[test]
fn sizes_are_bounded() {
    let long = "a".repeat(PATTERN_BYTES_MAX + 1);
    assert!(refusal(&format!("[[rule]]\nname = \"r\"\nmatch = [{{ call = \"{long}\" }}]\n")).contains("bytes"));
    let many = vec!["{ call = \"a\" }"; LIST_MAX + 1].join(", ");
    assert!(refusal(&format!("[[rule]]\nname = \"r\"\nmatch = [{many}]\n")).contains("more than"));
    // A pattern short enough in text but whose compiled program blows the size limit.
    let blowup = "\\\\w{1000}\\\\w{1000}\\\\w{1000}";
    let text = format!("[[rule]]\nname = \"r\"\nmatch = [{{ text = \"{blowup}\", regex = true }}]\n");
    assert!(refusal(&text).contains("regex"), "{}", refusal(&text));
    assert!(refusal("[[rule]]\nname = \"r\"\nmatch = [{ text = \"(\", regex = true }]\n").contains("regex"));
}

#[test]
fn include_and_exclude_select_paths() {
    let migration = parse(
        "m",
        "[[rule]]\nname = \"r\"\ninclude = [\"src/**\"]\nexclude = [\"src/legacy/**\"]\nmatch = [{ call = \"a\" }]\n",
    )
    .expect("valid");
    let rule = &migration.rules[0];
    assert!(rule.selects("src/a.rs"));
    assert!(rule.selects("src/deep/b.rs"));
    assert!(!rule.selects("src/legacy/c.rs"));
    assert!(!rule.selects("tests/a.rs"));
}

#[test]
fn literal_patterns_match_as_documented() {
    let migration = parse(
        "m",
        "[[rule]]\nname = \"r\"\nmatch = [{ call = \"old_log::info\" }, { import = \"old_log\" }, { identifier = \"Old\" }]\n",
    )
    .expect("valid");
    let [call, import, identifier] = &migration.rules[0].matches[..] else {
        panic!("three patterns");
    };
    assert!(call.matches_call("old_log::info", None));
    assert!(call.matches_call("crate::old_log::info", None));
    assert!(!call.matches_call("old_log::info_all", None));
    // An index that keeps only the last segment: the call expression in the source decides.
    assert!(!call.matches_call("info", None));
    assert!(call.matches_call("info", Some("old_log::info(\"a\");")));
    assert!(call.matches_call("info", Some("crate::old_log::info!(x)")));
    assert!(!call.matches_call("info", Some("tracing::info(\"a\");")));
    assert!(!call.matches_call("info", Some("my_old_log::info(1)")));
    assert!(import.matches_import(Some("old_log"), ""));
    assert!(import.matches_import(Some("old_log::Logger"), ""));
    assert!(import.matches_import(Some("old_log.sub"), ""));
    assert!(import.matches_import(Some("old_log/sub"), ""));
    assert!(!import.matches_import(Some("old_logger"), ""));
    // No parsed module: a path inside the statement, starting at a word boundary.
    assert!(import.matches_import(None, "use old_log::Logger;"));
    assert!(import.matches_import(None, "import x from 'old_log/sub';"));
    assert!(!import.matches_import(None, "use crate::old_log::Logger;"));
    assert!(!import.matches_import(None, "use old_logger::Logger;"));
    assert!(identifier.matches_name("Old"));
    assert!(!identifier.matches_name("OldStyle"));
}

#[test]
fn a_regex_pattern_matches_anywhere() {
    let migration = parse(
        "m",
        "[[rule]]\nname = \"r\"\nmatch = [{ call = \"^old_\", regex = true }]\n",
    )
    .expect("valid");
    let pattern = &migration.rules[0].matches[0];
    assert!(pattern.matches_call("old_info", None));
    assert!(!pattern.matches_call("x::old_info", None));
    assert!(
        pattern.matches_call("info", Some("old_info(1)")),
        "the call expression in the source"
    );
}

#[test]
fn a_literal_text_is_escaped() {
    let migration = parse("m", "[[rule]]\nname = \"r\"\nmatch = [{ text = \"old!(\" }]\n").expect("valid");
    let re = migration.rules[0].matches[0].line_regex().expect("compiles");
    assert!(re.is_match("let x = old!(1);"));
    assert!(!re.is_match("old(1)"));
}

#[test]
fn load_refuses_a_bad_name_and_names_the_known_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let migrations = dir.path().join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&migrations).expect("mkdir");
    std::fs::write(migrations.join("alpha.toml"), MINIMAL).expect("write");
    std::fs::write(migrations.join("notes.txt"), "ignored").expect("write");

    assert!(
        load(dir.path(), Some("../alpha"))
            .unwrap_err()
            .to_string()
            .contains("name")
    );
    let missing = load(dir.path(), Some("beta")).unwrap_err().to_string();
    assert!(missing.contains("known: alpha"), "{missing}");
    let all = load(dir.path(), None).expect("loads");
    assert_eq!(all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["alpha"]);
}

#[test]
fn load_refuses_an_empty_directory_and_a_too_large_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(load(dir.path(), None).unwrap_err().to_string().contains("no rule file"));
    let migrations = dir.path().join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&migrations).expect("mkdir");
    let padding = format!("# {}\n", "x".repeat(usize::try_from(FILE_BYTES_MAX).unwrap()));
    std::fs::write(migrations.join("big.toml"), format!("{padding}{MINIMAL}")).expect("write");
    assert!(load(dir.path(), Some("big")).unwrap_err().to_string().contains("limit"));
}

#[cfg(unix)]
#[test]
fn load_refuses_a_symlinked_rule_file_and_directory() {
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("x.toml"), MINIMAL).expect("write");

    let dir = tempfile::tempdir().expect("tempdir");
    let migrations = dir.path().join(MIGRATIONS_DIR);
    std::fs::create_dir_all(&migrations).expect("mkdir");
    std::os::unix::fs::symlink(outside.path().join("x.toml"), migrations.join("x.toml")).expect("symlink");
    assert!(load(dir.path(), Some("x")).unwrap_err().to_string().contains("symlink"));

    let linked = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(linked.path().join(".basemind")).expect("mkdir");
    std::os::unix::fs::symlink(outside.path(), linked.path().join(MIGRATIONS_DIR)).expect("symlink");
    assert!(
        load(linked.path(), Some("x"))
            .unwrap_err()
            .to_string()
            .contains("outside")
    );
}

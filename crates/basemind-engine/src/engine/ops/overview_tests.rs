use super::*;

fn row(path: &str, language: &str, lines: u32) -> FileRow {
    FileRow {
        path: RelPath::from(path),
        language: language.to_owned(),
        lines,
        bytes: u64::from(lines) * 10,
    }
}

#[test]
fn a_name_has_a_case_only_when_its_shape_shows_one() {
    let cases = [
        ("parse_config", Some("snake_case")),
        ("_private_helper", Some("snake_case")),
        ("MAX_LINES", Some("SCREAMING_SNAKE_CASE")),
        ("ImportGraph", Some("PascalCase")),
        ("parseConfig", Some("camelCase")),
        ("user-profile", Some("kebab-case")),
        ("main", None),
        ("HTTP", None),
        ("Mixed_Case", None),
        ("GET /users", None),
        ("2fa", None),
        ("", None),
    ];
    for (name, case) in cases {
        assert_eq!(case_of(name), case, "{name}");
    }
}

#[test]
fn sections_default_to_all_and_an_unknown_one_is_refused_with_the_list() {
    assert_eq!(sections(None).unwrap(), SECTIONS);
    assert_eq!(sections(Some(" ")).unwrap(), SECTIONS);
    assert_eq!(
        sections(Some("layout, entry-points")).unwrap(),
        ["layout", "entry_points"]
    );
    let error = sections(Some("layout,bogus")).unwrap_err().to_string();
    assert!(
        error.contains("unknown section `bogus`") && error.contains("most_imported"),
        "{error}"
    );
}

#[test]
fn languages_and_layout_rank_by_lines_and_name_the_dominant_language() {
    let rows = [
        row("src/a.rs", "rust", 300),
        row("src/b.py", "python", 50),
        row("web/app.ts", "typescript", 120),
        row("build.rs", "rust", 10),
    ];
    let langs: Vec<(String, usize, u64)> = languages(&rows)
        .into_iter()
        .map(|l| (l.language, l.files, l.lines))
        .collect();
    assert_eq!(
        langs,
        [
            ("rust".into(), 2, 310),
            ("typescript".into(), 1, 120),
            ("python".into(), 1, 50)
        ]
    );
    let dirs: Vec<(String, usize, u64, String)> = layout(&rows)
        .into_iter()
        .map(|d| (d.path, d.files, d.lines, d.language))
        .collect();
    assert_eq!(
        dirs,
        [
            ("src".into(), 2, 350, "rust".into()),
            ("web".into(), 1, 120, "typescript".into()),
            (".".into(), 1, 10, "rust".into()),
        ]
    );
}

#[test]
fn largest_lists_the_longest_files_first_ties_by_path() {
    let rows = [row("b.rs", "rust", 5), row("a.rs", "rust", 5), row("c.rs", "rust", 9)];
    let paths: Vec<String> = largest(&rows, 2).into_iter().map(|f| f.path.to_string()).collect();
    assert_eq!(paths, ["c.rs", "a.rs"]);
}

#[test]
fn the_dominant_case_needs_enough_names_and_reports_its_share() {
    let cases = BTreeMap::from([("snake_case", 3), ("camelCase", 1)]);
    assert_eq!(
        share(&cases),
        Some(CaseShare {
            case: "snake_case",
            percent: 75,
            names: 4
        })
    );
    assert_eq!(share(&BTreeMap::from([("snake_case", 2)])), None);
}

#[test]
fn entry_points_count_tests_and_list_the_rest_up_to_the_limit() {
    use crate::engine::manifests::{Ecosystem, Target};
    let target = |kind, name: &str| Target {
        kind,
        name: Some(name.to_owned()),
        path: None,
        symbol: None,
    };
    let manifest = Manifest {
        path: RelPath::from("Cargo.toml"),
        ecosystem: Ecosystem::Cargo,
        name: None,
        version: None,
        members: Vec::new(),
        dependencies: Default::default(),
        dependency_names: Vec::new(),
        targets: vec![
            target(TargetKind::Lib, "x"),
            target(TargetKind::Bin, "a"),
            target(TargetKind::Bin, "b"),
            target(TargetKind::Test, "t1"),
            target(TargetKind::Test, "t2"),
            target(TargetKind::Example, "e"),
        ],
        targets_partial: false,
        error: None,
    };
    let (points, counted, total) = entry_points(std::slice::from_ref(&manifest), 2);
    assert_eq!(total, 3);
    let names: Vec<_> = points.iter().map(|p| p.target.name.as_deref()).collect();
    assert_eq!(names, [Some("x"), Some("a")]);
    assert_eq!(counted, BTreeMap::from([("example", 1), ("test", 2)]));
}

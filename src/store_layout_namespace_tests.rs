use super::*;

#[test]
fn a_minor_after_the_last_unnamespaced_one_keeps_its_group_database_in_its_own_directory() {
    let group = Path::new("/cache/groups/g");
    let cases: [(&str, u16, &str); 4] = [
        ("an old minor", 33, "/cache/groups/g"),
        (
            "the last unnamespaced minor",
            LAST_UNNAMESPACED_GROUP_MINOR,
            "/cache/groups/g",
        ),
        (
            "the next minor",
            LAST_UNNAMESPACED_GROUP_MINOR + 1,
            "/cache/groups/g/v35",
        ),
        ("a major", 100, "/cache/groups/g/v100"),
    ];
    for (case, minor, expected) in cases {
        assert_eq!(group_index_namespace(group, minor), Path::new(expected), "{case}");
    }
}

#[test]
fn this_binarys_index_path_is_its_namespace_holding_the_fjall_directory() {
    let group = Path::new("/cache/groups/g");
    assert_eq!(
        group_index_path_in(group),
        group_index_namespace(group, crate::version::RELEASE_MINOR).join(crate::index::INDEX_DIR)
    );
}

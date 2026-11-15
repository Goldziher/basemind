use std::path::{Path, PathBuf};

use super::*;
use crate::comms::ids::MAX_ID_LEN;
use crate::comms::v2::ids::{IdKind, InvalidId};

const ROOT: &str = "/home/dev/projects/basemind";

/// `root` as [`canonical_root`] gives it for an absolute path the filesystem does not hold: as it
/// is spelled. Built directly, because the roots here are Unix literals, which are not absolute
/// on every platform, and the id pinned for one must not depend on where the tests run.
fn canonical(root: impl AsRef<Path>) -> CanonicalRoot {
    CanonicalRoot(root.as_ref().to_path_buf())
}

fn inputs<'a>(
    env: Option<&str>,
    config: Option<&str>,
    host: Option<&str>,
    root: &'a CanonicalRoot,
) -> IdentityInputs<'a> {
    IdentityInputs {
        env_agent_id: env.map(str::to_string),
        config_agent_id: config.map(str::to_string),
        env_host: host.map(str::to_string),
        root,
    }
}

fn derived(host: Option<&str>, root: &str) -> String {
    derived_principal(host, &canonical(root)).as_str().to_string()
}

fn rejected(tier: IdentityTier, value: &str) -> RejectedId {
    RejectedId {
        tier,
        value: value.to_string(),
    }
}

fn principal(s: &str) -> Principal {
    Principal::parse(s).expect("principal")
}

#[test]
fn the_derived_id_of_a_fixed_root_and_host_is_pinned() {
    assert_eq!(derived(Some("claude-code"), ROOT), "claudecode-basemind-e3515d");
}

#[test]
fn the_same_inputs_always_resolve_to_the_same_id() {
    let root = canonical(ROOT);
    let request = inputs(None, None, Some("claude-code"), &root);
    let first = resolve(&request);
    assert_eq!(first, resolve(&request));
    assert_eq!(first.principal.as_str(), "claudecode-basemind-e3515d");
    assert_eq!(first.tier, IdentityTier::Derived);
    assert_eq!(first.rejected, Vec::new());
}

#[test]
fn two_roots_with_one_basename_get_different_ids() {
    let one = derived(Some("codex"), "/work/a/basemind");
    let other = derived(Some("codex"), "/work/b/basemind");
    assert!(one.starts_with("codex-basemind-"), "{one}");
    assert!(other.starts_with("codex-basemind-"), "{other}");
    assert_ne!(one, other);
}

#[test]
fn the_host_is_lowercased_reduced_to_alphanumerics_and_cut() {
    assert_eq!(host_part(Some("claude-code")), "claudecode");
    assert_eq!(host_part(Some("Codex CLI")), "codexcli");
    assert_eq!(host_part(Some("Cursor_2.1")), "cursor21");
    assert_eq!(host_part(Some("abcdefghijklmnopqrstuvwxyz")), "abcdefghijklmnop");
    assert_eq!(host_part(Some("--------------------a")), "a");
}

#[test]
fn a_host_with_nothing_usable_is_agent() {
    assert_eq!(host_part(None), "agent");
    assert_eq!(host_part(Some("")), "agent");
    assert_eq!(host_part(Some(" -_. ")), "agent");
    assert_eq!(host_part(Some("хост")), "agent");
    assert_eq!(derived(None, ROOT), "agent-basemind-e3515d");
    assert_eq!(derived(Some(""), ROOT), "agent-basemind-e3515d");
}

#[test]
fn the_host_changes_the_id_but_not_the_hash() {
    assert_eq!(derived(Some("Codex CLI"), ROOT), "codexcli-basemind-e3515d");
    assert_ne!(derived(Some("Codex CLI"), ROOT), derived(Some("claude-code"), ROOT));
}

#[test]
fn the_repo_is_the_basename_lowercased_with_other_bytes_replaced() {
    assert_eq!(repo_part(Path::new("/src/BaseMind")), "basemind");
    assert_eq!(repo_part(Path::new("/src/my_repo.rs v2")), "my-repo-rs-v2");
    assert_eq!(repo_part(Path::new("/src/basemind/")), "basemind");
    assert_eq!(repo_part(Path::new("relative/dir")), "dir");
}

#[test]
fn the_repo_keeps_leading_trailing_and_repeated_dashes() {
    assert_eq!(repo_part(Path::new("/src/-repo-")), "-repo-");
    assert_eq!(repo_part(Path::new("/src/a--b")), "a--b");
    assert_eq!(repo_part(Path::new("/src/.hidden")), "-hidden");
    assert_eq!(repo_part(Path::new("/src/___")), "---");
    assert_eq!(
        derived(Some("x"), "/src/___").split_once("----").map(|(host, _)| host),
        Some("x")
    );
}

#[test]
fn the_repo_is_cut_to_twenty_four_bytes() {
    assert_eq!(
        repo_part(Path::new("/src/abcdefghijklmnopqrstuvwxyz0123456789")),
        "abcdefghijklmnopqrstuvwx"
    );
    assert_eq!(
        repo_part(Path::new("/src/abcdefghijklmnopqrstuvwx")).len(),
        MAX_REPO_LEN
    );
}

#[test]
fn a_multibyte_name_becomes_one_dash_per_byte_and_is_cut_on_bytes() {
    assert_eq!(repo_part(Path::new("/src/café")), "caf--");
    assert_eq!(
        repo_part(Path::new("/src/日本語のリポジトリ名前")),
        "-".repeat(MAX_REPO_LEN)
    );
}

#[test]
fn a_root_without_a_final_component_has_an_empty_repo() {
    for root in ["/", "", "/a/b/.."] {
        assert_eq!(repo_part(Path::new(root)), "", "{root:?}");
    }
    assert_eq!(derived(Some("codex"), "/"), "codex--329f85");
    let empty = derived(None, "");
    assert!(empty.starts_with("agent--"), "{empty}");
    assert_ne!(empty, derived(None, "/"));
}

#[cfg(unix)]
#[test]
fn a_non_utf8_name_is_replaced_byte_by_byte() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = Path::new("/src").join(OsStr::from_bytes(b"re\xffpo\xc3"));
    assert_eq!(repo_part(&root), "re-po-");
    let id = derived_principal(Some("codex"), &canonical(&root));
    assert!(id.as_str().starts_with("codex-re-po--"), "{id}");
    let other = Path::new("/src").join(OsStr::from_bytes(b"re\xfepo\xc3"));
    assert_ne!(id, derived_principal(Some("codex"), &canonical(&other)));
}

#[test]
fn the_environment_id_wins_verbatim() {
    let resolved = resolve(&inputs(
        Some("Team:Lead_1"),
        Some("from-config"),
        Some("codex"),
        &canonical(ROOT),
    ));
    assert_eq!(resolved.principal.as_str(), "Team:Lead_1");
    assert_eq!(resolved.tier, IdentityTier::Env);
    assert_eq!(resolved.rejected, Vec::new());
    assert_eq!(resolved.notices(), Vec::<String>::new());
}

#[test]
fn the_config_id_is_used_when_the_environment_has_none() {
    for env in [None, Some("")] {
        let resolved = resolve(&inputs(env, Some("from-config"), None, &canonical(ROOT)));
        assert_eq!(resolved.principal.as_str(), "from-config");
        assert_eq!(resolved.tier, IdentityTier::Config);
        assert_eq!(resolved.rejected, Vec::new());
    }
}

#[test]
fn an_empty_configured_id_is_unset_not_rejected() {
    let resolved = resolve(&inputs(Some(""), Some(""), Some("codex"), &canonical(ROOT)));
    assert_eq!(resolved.principal.as_str(), "codex-basemind-e3515d");
    assert_eq!(resolved.tier, IdentityTier::Derived);
    assert_eq!(resolved.rejected, Vec::new());
}

#[test]
fn an_invalid_environment_id_falls_to_the_config_id_and_is_recorded() {
    let resolved = resolve(&inputs(Some("lead.dev"), Some("from-config"), None, &canonical(ROOT)));
    assert_eq!(resolved.principal.as_str(), "from-config");
    assert_eq!(resolved.tier, IdentityTier::Config);
    assert_eq!(resolved.rejected, vec![rejected(IdentityTier::Env, "lead.dev")]);
    assert_eq!(resolved.notices(), vec![rejected_id_notice("lead.dev", "from-config")]);
}

#[test]
fn invalid_ids_in_both_tiers_fall_to_the_derived_id_in_tier_order() {
    let resolved = resolve(&inputs(Some("has space"), Some("a.b"), Some("codex"), &canonical(ROOT)));
    assert_eq!(resolved.principal.as_str(), "codex-basemind-e3515d");
    assert_eq!(resolved.tier, IdentityTier::Derived);
    assert_eq!(
        resolved.rejected,
        vec![
            rejected(IdentityTier::Env, "has space"),
            rejected(IdentityTier::Config, "a.b")
        ]
    );
    let notices = resolved.notices();
    assert_eq!(notices.len(), 2);
    assert!(notices[0].contains("\"has space\"") && notices[0].contains("codex-basemind-e3515d"));
    assert!(notices[1].contains("\"a.b\""));
}

#[test]
fn every_invalid_shape_of_configured_id_is_rejected() {
    let too_long = "a".repeat(MAX_ID_LEN + 1);
    for bad in ["a.b", ".", "a b", " ", "a/b", "naïve", "a\n", too_long.as_str()] {
        let resolved = resolve(&inputs(Some(bad), None, None, &canonical(ROOT)));
        assert_eq!(resolved.tier, IdentityTier::Derived, "{bad:?}");
        assert_eq!(resolved.rejected.len(), 1, "{bad:?}");
        assert_eq!(resolved.rejected[0].tier, IdentityTier::Env);
    }
    let longest = "a".repeat(MAX_ID_LEN);
    assert_eq!(
        resolve(&inputs(Some(&longest), None, None, &canonical(ROOT))).tier,
        IdentityTier::Env
    );
}

#[test]
fn a_rejected_value_is_echoed_bounded() {
    let huge = "é".repeat(4096);
    let resolved = resolve(&inputs(Some(&huge), None, None, &canonical(ROOT)));
    assert_eq!(resolved.rejected[0].value, "é".repeat(80));
    assert_eq!(resolved.rejected[0].value.len(), 160);
}

#[test]
fn tier_labels_name_the_setting() {
    assert_eq!(IdentityTier::Env.label(), "BASEMIND_AGENT_ID");
    assert_eq!(IdentityTier::Config.label(), "comms.agent_id");
    assert_eq!(IdentityTier::Derived.label(), "derived");
    assert_eq!(HOST_ENV, "BASEMIND_HOST");
}

/// What `serve` does: the launcher's environment, and the root it was started in, canonical.
fn serve_identity(env: &[(&str, &str)], config: Option<&str>, root: &Path) -> ResolvedIdentity {
    let canonical = canonical_root(&root.canonicalize().expect("canonical root")).expect("an absolute root");
    resolve(&IdentityInputs {
        env_agent_id: lookup(env, AGENT_ID_ENV),
        config_agent_id: config.map(str::to_string),
        env_host: lookup(env, HOST_ENV),
        root: &canonical,
    })
}

/// What a hook does: the same shared environment through the CLI's wrapper, with the root spelled
/// however the host passed it.
fn hook_identity(env: &[(&str, &str)], config: Option<&str>, root: &Path) -> ResolvedIdentity {
    resolve_with(|name| lookup(env, name), config.map(str::to_string), root).expect("an absolute root")
}

fn lookup(env: &[(&str, &str)], name: &str) -> Option<String> {
    env.iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

#[test]
fn a_hook_and_serve_resolve_the_same_id_from_the_same_inputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("My Repo");
    std::fs::create_dir(&root).expect("create root");
    let spellings = [
        root.clone(),
        root.join("."),
        root.join("sub").join(".."),
        PathBuf::from(format!("{}/", root.display())),
    ];
    std::fs::create_dir(root.join("sub")).expect("create sub");

    let environments: [&[(&str, &str)]; 4] = [
        &[("BASEMIND_HOST", "claude-code")],
        &[],
        &[("BASEMIND_HOST", "claude-code"), ("BASEMIND_AGENT_ID", "lead")],
        &[("BASEMIND_HOST", "Codex CLI"), ("BASEMIND_AGENT_ID", "not.valid")],
    ];
    for env in environments {
        for config in [None, Some("from-config")] {
            let serve = serve_identity(env, config, &root);
            for spelling in &spellings {
                assert_eq!(
                    hook_identity(env, config, spelling),
                    serve,
                    "{env:?} {config:?} {spelling:?}"
                );
            }
        }
    }

    let derived = serve_identity(environments[0], None, &root);
    assert_eq!(derived.tier, IdentityTier::Derived);
    assert!(
        derived.principal.as_str().starts_with("claudecode-my-repo-"),
        "{}",
        derived.principal
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_root_resolves_to_the_id_of_its_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("checkout");
    std::fs::create_dir(&root).expect("create root");
    let link = dir.path().join("alias");
    std::os::unix::fs::symlink(&root, &link).expect("symlink");

    let env: &[(&str, &str)] = &[("BASEMIND_HOST", "codex")];
    let through_link = hook_identity(env, None, &link);
    assert_eq!(through_link, serve_identity(env, None, &root));
    assert!(
        through_link.principal.as_str().starts_with("codex-checkout-"),
        "{}",
        through_link.principal
    );
}

#[test]
fn an_absolute_root_that_cannot_be_canonicalised_is_cleaned_lexically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical tempdir");
    let missing = base.join("gone");
    assert_eq!(canonical_root(&missing), Ok(canonical(&missing)));
    assert_eq!(
        canonical_root(&base.join("x").join("..").join("gone").join(".")),
        Ok(canonical(&missing))
    );

    let no_env = |_: &str| None;
    assert_eq!(
        resolve_with(no_env, None, &base.join("x/../gone/.")),
        resolve_with(no_env, None, &missing)
    );
}

#[test]
fn a_relative_root_is_refused_and_never_completed_from_the_working_directory() {
    let no_env = |_: &str| None;
    for relative in [
        "",
        ".",
        "repo",
        "./repo",
        "../repo",
        "no-such-dir-for-identity-tests/a/..",
    ] {
        let refused = IdentityError::RelativeRoot {
            root: PathBuf::from(relative),
        };
        assert_eq!(
            canonical_root(Path::new(relative)),
            Err(refused.clone()),
            "{relative:?}"
        );
        assert_eq!(
            resolve_with(no_env, None, Path::new(relative)),
            Err(refused.clone()),
            "{relative:?}"
        );
        assert_eq!(
            resolve_with(
                |name| (name == AGENT_ID_ENV).then(|| "lead".to_string()),
                None,
                Path::new(relative)
            ),
            Err(refused),
            "a configured id does not excuse a relative root"
        );
    }
    let message = IdentityError::RelativeRoot {
        root: PathBuf::from("re\npo"),
    }
    .to_string();
    assert!(
        message.contains("\"re\\npo\"") && message.contains("absolute"),
        "{message}"
    );
}

/// A canonicaliser as the filesystem would be seen from the working directory `cwd`: a relative
/// path is completed from there, which is how [`Path::canonicalize`] comes to depend on it.
fn canonicalize_from(cwd: &'static str) -> impl FnOnce(&Path) -> std::io::Result<PathBuf> {
    move |path| Ok(lexically_clean(&absolute(cwd).join(path)))
}

/// The `/`-rooted literal `unix` as a path that is absolute where the tests run: itself on Unix,
/// and on a drive on Windows, where a path that names no drive is relative to the current one.
fn absolute(unix: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{unix}"))
    } else {
        PathBuf::from(unix)
    }
}

#[test]
fn the_id_of_an_absolute_root_does_not_depend_on_the_working_directory() {
    let clean = canonical(lexically_clean(&absolute(ROOT)));
    for root in [ROOT, "/home/dev/projects/./basemind/sub/.."] {
        let root = absolute(root);
        let from_one = canonical_root_via(&root, canonicalize_from("/work/one")).expect("absolute");
        let from_other = canonical_root_via(&root, canonicalize_from("/somewhere/else")).expect("absolute");
        assert_eq!(from_one, from_other, "{root:?}");
        assert_eq!(from_one, clean, "{root:?}");
        assert_eq!(
            derived_principal(Some("claude-code"), &from_one),
            derived_principal(Some("claude-code"), &clean),
            "{root:?}"
        );
        // ~keep The pinned id hashes the Unix spelling of the root; a drive letter changes the bytes.
        #[cfg(unix)]
        assert_eq!(
            derived_principal(Some("claude-code"), &from_one).as_str(),
            "claudecode-basemind-e3515d"
        );
    }

    let asked = std::cell::Cell::new(false);
    let refused = canonical_root_via(Path::new("projects/basemind"), |path| {
        asked.set(true);
        canonicalize_from("/work/one")(path)
    });
    assert_eq!(
        refused,
        Err(IdentityError::RelativeRoot {
            root: PathBuf::from("projects/basemind")
        })
    );
    assert!(!asked.get(), "a relative root never reaches the filesystem");
}

#[test]
fn lexical_cleaning_folds_dots_without_the_filesystem() {
    let cases = [
        ("/a/./b//c/", "/a/b/c"),
        ("/a/b/../c", "/a/c"),
        ("/a/b/../../..", "/"),
        ("/..", "/"),
        ("a/../..", ".."),
        ("../a", "../a"),
        ("./a/.", "a"),
        ("", ""),
        ("/", "/"),
    ];
    for (given, clean) in cases {
        assert_eq!(lexically_clean(Path::new(given)), PathBuf::from(clean), "{given:?}");
    }
}

#[test]
fn compose_builds_the_principal_or_its_sub_identity() {
    let lead = principal("lead");
    for absent in [None, Some("")] {
        let actor = compose(&lead, absent).expect("principal actor");
        assert_eq!(actor.as_str(), "lead");
        assert_eq!(actor.sub(), None);
    }
    let reviewer = compose(&lead, Some("reviewer-1")).expect("sub actor");
    assert_eq!(reviewer.as_str(), "lead.reviewer-1");
    assert_eq!(reviewer.principal(), "lead");
    assert_eq!(reviewer.sub(), Some("reviewer-1"));
}

#[test]
fn compose_refuses_a_name_outside_the_grammar() {
    let lead = principal("lead");
    for bad in ["Reviewer", "a.b", "other.agent", "a b", &"a".repeat(33)] {
        assert_eq!(
            compose(&lead, Some(bad)),
            Err(CommsError::InvalidId(InvalidId::new(IdKind::Sub, bad))),
            "{bad:?}"
        );
    }
}

#[test]
fn compose_refuses_a_composed_id_over_the_length_limit() {
    let long = principal(&"p".repeat(MAX_ID_LEN - 4));
    assert_eq!(compose(&long, Some("abc")).expect("fits").as_str().len(), MAX_ID_LEN);
    let composed = format!("{long}.abcd");
    assert_eq!(
        compose(&long, Some("abcd")),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Actor, &composed)))
    );
}

fn hook_name(agent_type: &str, agent_id: &str) -> Option<String> {
    hook_sub_name(agent_type, agent_id).map(|name| name.as_str().to_string())
}

#[test]
fn the_hook_sub_name_joins_the_type_and_eight_hex_of_the_hashed_id() {
    assert_eq!(
        hook_name("Explore", "a1b2c3d4e5f6"),
        Some("explore-d5fae130".to_string())
    );
    assert_eq!(
        hook_name("general-purpose", "ABCDEF0123456789"),
        Some("general-purpose-372ce358".to_string())
    );
    assert_eq!(
        hook_name("test_writer", "abc"),
        Some("test_writer-6437b3ac".to_string())
    );
}

#[test]
fn agent_ids_that_share_their_first_bytes_get_different_sub_names() {
    assert_eq!(
        hook_name("Explore", "a1b2c3d4-0001"),
        Some("explore-71925a85".to_string())
    );
    assert_eq!(
        hook_name("Explore", "a1b2c3d4-0002"),
        Some("explore-17733319".to_string())
    );
    assert_ne!(hook_name("Explore", "abc"), hook_name("Explore", "ABC"));
}

#[test]
fn the_hook_sub_name_replaces_bytes_of_the_type_outside_the_grammar() {
    assert_eq!(
        hook_name("basemind:code reviewer", "a1.b2/c3d4"),
        Some("basemind-code-reviewer-456f8846".to_string())
    );
    assert_eq!(hook_name("café", "12345678"), Some("caf---07d4aeb7".to_string()));
}

#[test]
fn a_long_agent_type_is_cut_to_what_the_hash_leaves() {
    let name = hook_name("basemind:mcp-domain-migrator-extended", "0123456789abcdef").expect("name");
    assert_eq!(name, "basemind-mcp-domain-mig-b8f6b168");
    assert_eq!(name.len(), 32);
    let sibling = hook_name("basemind:mcp-domain-migrator-extended", "fedcba9876543210").expect("name");
    assert_eq!(sibling, "basemind-mcp-domain-mig-56e97f22");
    assert_eq!(hook_name(&"t".repeat(40), "").map(|name| name.len()), Some(32));
}

#[test]
fn an_empty_part_is_left_out_with_its_separator() {
    assert_eq!(hook_name("Explore", ""), Some("explore".to_string()));
    assert_eq!(hook_name("", "a1b2c3d4e5"), Some("52e6ac8f".to_string()));
    assert_eq!(hook_name("::", "..."), Some("---2f1145ba".to_string()));
}

#[test]
fn the_hook_sub_name_is_none_when_nothing_valid_remains() {
    for (agent_type, agent_id) in [("", ""), ("::", ""), ("日本", ""), ("-", "")] {
        assert_eq!(hook_name(agent_type, agent_id), None, "{agent_type:?} {agent_id:?}");
    }
}

#[test]
fn a_hook_sub_name_composes_with_a_principal() {
    let name = hook_sub_name("Explore", "a1b2c3d4e5f6").expect("name");
    let actor = compose(&principal("claudecode-basemind-e3515d"), Some(name.as_str())).expect("actor");
    assert_eq!(actor.as_str(), "claudecode-basemind-e3515d.explore-d5fae130");
}

fn assert_derived_shape(id: &Principal, host: Option<&str>) -> Result<(), proptest::test_runner::TestCaseError> {
    let text = id.as_str();
    proptest::prop_assert_eq!(Principal::parse(text), Ok(id.clone()));
    proptest::prop_assert!(text.len() <= MAX_HOST_LEN + MAX_REPO_LEN + ROOT_HASH_HEX_LEN + 2);
    proptest::prop_assert!(
        text.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    );
    let hash = &text[text.len() - ROOT_HASH_HEX_LEN..];
    proptest::prop_assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let expected_host = host_part(host);
    proptest::prop_assert!(!expected_host.is_empty() && expected_host.len() <= MAX_HOST_LEN);
    let host_prefix = format!("{expected_host}-");
    proptest::prop_assert!(text.starts_with(&host_prefix));
    Ok(())
}

proptest::proptest! {
    #[test]
    fn a_derived_id_is_always_a_valid_principal(
        host in proptest::option::of(proptest::prelude::any::<String>()),
        root in proptest::prelude::any::<String>(),
    ) {
        let id = derived_principal(host.as_deref(), &canonical(&root));
        assert_derived_shape(&id, host.as_deref())?;
        let resolved = resolve(&inputs(None, None, host.as_deref(), &canonical(&root)));
        proptest::prop_assert_eq!(resolved.principal, id);
        proptest::prop_assert_eq!(resolved.tier, IdentityTier::Derived);
    }

    #[test]
    fn a_derived_id_survives_awkward_path_shapes(
        host in "[ -~]{0,40}",
        segments in proptest::collection::vec("(\\.\\.|\\.|-+|[ -.0-~]{0,40}|\\PC{0,12})", 0..6),
        absolute in proptest::prelude::any::<bool>(),
        trailing in proptest::prelude::any::<bool>(),
    ) {
        let mut root = if absolute { String::from("/") } else { String::new() };
        root.push_str(&segments.join("/"));
        if trailing {
            root.push('/');
        }
        let id = derived_principal(Some(&host), &canonical(&root));
        assert_derived_shape(&id, Some(&host))?;
        let cleaned = derived_principal(Some(&host), &canonical(lexically_clean(Path::new(&root))));
        assert_derived_shape(&cleaned, Some(&host))?;
    }

    #[test]
    fn any_configured_id_resolves_to_a_valid_principal(
        env in proptest::option::of("\\PC{0,200}"),
        config in proptest::option::of("\\PC{0,200}"),
    ) {
        let resolved = resolve(&inputs(env.as_deref(), config.as_deref(), None, &canonical(ROOT)));
        proptest::prop_assert_eq!(Principal::parse(resolved.principal.as_str()), Ok(resolved.principal.clone()));
        proptest::prop_assert!(!resolved.principal.as_str().contains('.'));
        proptest::prop_assert!(resolved.rejected.len() <= 2);
        proptest::prop_assert!(resolved.rejected.iter().all(|r| r.value.len() <= 160 && r.tier != IdentityTier::Derived));
    }

    #[test]
    fn a_hook_sub_name_is_valid_whenever_there_is_one(
        agent_type in proptest::prelude::any::<String>(),
        agent_id in proptest::prelude::any::<String>(),
    ) {
        if let Some(name) = hook_sub_name(&agent_type, &agent_id) {
            proptest::prop_assert_eq!(SubName::parse(name.as_str()), Ok(name.clone()));
            proptest::prop_assert!(name.as_str().bytes().any(|b| b != b'-'));
        }
    }
}

#[cfg(unix)]
proptest::proptest! {
    #[test]
    fn a_derived_id_is_valid_for_any_root_bytes(
        host in proptest::option::of(proptest::collection::vec(proptest::prelude::any::<u8>(), 0..40)),
        root in proptest::collection::vec(1u8..=255, 0..80),
    ) {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let host = host.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let id = derived_principal(host.as_deref(), &canonical(OsStr::from_bytes(&root)));
        assert_derived_shape(&id, host.as_deref())?;
    }
}

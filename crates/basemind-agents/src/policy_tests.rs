use super::*;

fn allowing(flavors: Vec<FlavorPolicy>) -> PolicySpec {
    PolicySpec {
        flavors,
        launchers: vec!["lead".to_owned()],
        max_depth: 1,
        max_concurrent: 2,
        max_children: 2,
        wall_time: Duration::from_secs(60),
        budget: Budget {
            max_launches: 8,
            max_turns: 8,
            max_allows: 8,
        },
    }
}

fn bypass(kind: HarnessKind, opt_in: Option<BypassOptIn>) -> FlavorPolicy {
    FlavorPolicy {
        kind,
        bypass: opt_in,
        permission_mode: PermissionMode::Bypass,
        ..FlavorPolicy::supervised(kind)
    }
}

#[test]
fn the_default_policy_allows_nothing() {
    let policy = LaunchPolicy::default();
    assert!(!policy.enabled());
    assert!(!policy.admits("lead"));
    assert!(policy.flavor(HarnessKind::ClaudeCode).is_none());
    assert_eq!(LaunchPolicy::new(PolicySpec::default()).unwrap(), policy);
}

#[test]
fn a_valid_policy_reports_its_rules() {
    let policy = LaunchPolicy::new(allowing(vec![FlavorPolicy::supervised(HarnessKind::Codex)])).unwrap();
    assert!(policy.enabled());
    assert!(policy.admits("lead"));
    assert!(!policy.admits("someone-else"));
    assert_eq!(
        policy.flavor(HarnessKind::Codex).unwrap().caveats(),
        &[Caveat::UserMcpServers]
    );
    assert!(policy.flavor(HarnessKind::ClaudeCode).is_none());
}

#[test]
fn a_half_configured_policy_is_refused() {
    let mut spec = allowing(Vec::new());
    assert_eq!(LaunchPolicy::new(spec.clone()), Err(PolicyError::Incomplete));
    spec.flavors = vec![FlavorPolicy::supervised(HarnessKind::Codex)];
    spec.launchers.clear();
    assert_eq!(LaunchPolicy::new(spec), Err(PolicyError::Incomplete));
}

#[test]
fn a_flavor_named_twice_is_refused() {
    let flavor = FlavorPolicy::supervised(HarnessKind::ClaudeCode);
    assert_eq!(
        LaunchPolicy::new(allowing(vec![flavor.clone(), flavor])),
        Err(PolicyError::DuplicateFlavor("claude"))
    );
}

#[test]
fn bypass_needs_an_opt_in_naming_the_flavor_and_its_mode() {
    let claude = HarnessKind::ClaudeCode;
    assert_eq!(
        LaunchPolicy::new(allowing(vec![bypass(claude, None)])),
        Err(PolicyError::BypassWithoutOptIn {
            harness: "claude",
            mode: "bypassPermissions"
        })
    );
    let other_flavor = BypassOptIn {
        harness: HarnessKind::Codex,
        mode: "bypassPermissions".to_owned(),
    };
    assert_eq!(
        LaunchPolicy::new(allowing(vec![bypass(claude, Some(other_flavor))])),
        Err(PolicyError::OptInMismatch("claude"))
    );
    let other_mode = BypassOptIn {
        harness: claude,
        mode: "danger-full-access".to_owned(),
    };
    assert_eq!(
        LaunchPolicy::new(allowing(vec![bypass(claude, Some(other_mode))])),
        Err(PolicyError::OptInMismatch("claude"))
    );
    let named = BypassOptIn {
        harness: claude,
        mode: "bypassPermissions".to_owned(),
    };
    assert!(LaunchPolicy::new(allowing(vec![bypass(claude, Some(named.clone()))])).is_ok());

    // An opt-in on a flavor that does not run in bypass mode is refused rather than kept.
    let stray = FlavorPolicy {
        bypass: Some(named),
        ..FlavorPolicy::supervised(claude)
    };
    assert_eq!(
        LaunchPolicy::new(allowing(vec![stray])),
        Err(PolicyError::OptInMismatch("claude"))
    );
}

#[test]
fn a_flavor_without_a_bypass_mode_cannot_be_given_one() {
    let opencode = HarnessKind::Acp(AcpFlavor::OpenCode);
    let opt_in = BypassOptIn {
        harness: opencode,
        mode: "anything".to_owned(),
    };
    assert_eq!(
        LaunchPolicy::new(allowing(vec![bypass(opencode, Some(opt_in))])),
        Err(PolicyError::OptInMismatch("opencode"))
    );
}

#[test]
fn an_unverified_mode_opt_in_names_the_flavor_and_its_mode() {
    let gemini = HarnessKind::Acp(AcpFlavor::Gemini);
    let flavor = |harness, mode| FlavorPolicy {
        unverified_mode: Some(UnverifiedModeOptIn { harness, mode }),
        ..FlavorPolicy::supervised(gemini)
    };
    assert!(LaunchPolicy::new(allowing(vec![flavor(gemini, PermissionMode::Supervised)])).is_ok());
    for stray in [
        flavor(HarnessKind::Codex, PermissionMode::Supervised),
        flavor(gemini, PermissionMode::ReadOnly),
    ] {
        assert_eq!(
            LaunchPolicy::new(allowing(vec![stray])),
            Err(PolicyError::OptInMismatch("gemini"))
        );
    }
}

#[test]
fn launchers_follow_the_principal_grammar_and_not_the_launched_prefix() {
    let mut spec = allowing(vec![FlavorPolicy::supervised(HarnessKind::Codex)]);
    spec.launchers = vec!["has space".to_owned()];
    assert_eq!(
        LaunchPolicy::new(spec.clone()),
        Err(PolicyError::InvalidLauncher("has space".to_owned()))
    );
    spec.launchers = vec!["launched-claude-1".to_owned()];
    assert_eq!(
        LaunchPolicy::new(spec),
        Err(PolicyError::ReservedLauncher("launched-claude-1".to_owned()))
    );
}

#[test]
fn every_limit_is_above_zero_and_within_its_ceiling() {
    let base = allowing(vec![FlavorPolicy::supervised(HarnessKind::Codex)]);
    type Case = (fn(&mut PolicySpec), PolicyError);
    let cases: [Case; 10] = [
        (|s| s.max_depth = 0, PolicyError::ZeroLimit("max_depth")),
        (
            |s| s.max_depth = MAX_DEPTH_CEILING + 1,
            PolicyError::LimitTooLarge {
                limit: "max_depth",
                ceiling: 8,
            },
        ),
        (|s| s.max_concurrent = 0, PolicyError::ZeroLimit("max_concurrent")),
        (
            |s| s.max_concurrent = MAX_CONCURRENT_CEILING + 1,
            PolicyError::LimitTooLarge {
                limit: "max_concurrent",
                ceiling: 32,
            },
        ),
        (
            |s| s.max_children = 3,
            PolicyError::LimitTooLarge {
                limit: "max_children",
                ceiling: 2,
            },
        ),
        (|s| s.wall_time = Duration::ZERO, PolicyError::ZeroLimit("wall_time")),
        (
            |s| s.wall_time = MAX_WALL_TIME_CEILING + Duration::from_secs(1),
            PolicyError::LimitTooLarge {
                limit: "wall_time",
                ceiling: 86_400,
            },
        ),
        (
            |s| s.budget.max_launches = 0,
            PolicyError::ZeroLimit("budget.max_launches"),
        ),
        (|s| s.budget.max_turns = 0, PolicyError::ZeroLimit("budget.max_turns")),
        // A policy that allows a launch has to state its budget: there is no unlimited default.
        (
            |s| s.budget = Budget::default(),
            PolicyError::ZeroLimit("budget.max_launches"),
        ),
    ];
    for (change, expected) in cases {
        let mut spec = base.clone();
        change(&mut spec);
        assert_eq!(LaunchPolicy::new(spec), Err(expected.clone()), "{expected}");
    }
}

#[test]
fn opencode_reports_its_detached_server_and_user_mcp_servers() {
    let opencode = caveats(HarnessKind::Acp(AcpFlavor::OpenCode));
    assert_eq!(opencode, &[Caveat::DetachedServer, Caveat::UserMcpServers]);
    assert!(caveats(HarnessKind::ClaudeCode).is_empty());
    assert_eq!(Caveat::DetachedServer.id(), "detached_server");
}

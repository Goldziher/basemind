//! Path roles: the labels a file carries in the code map (`test`, `infra`, `generated`, `vendored`,
//! `example`), from built-in globs and the repository's `[scan.roles]` lists.
//!
//! A role marks a file and never removes it: every file stays indexed, and a reader filters by role.
//! Each role's list is the built-in globs followed by the repository's own, compiled through
//! [`compile_rules`], so a later `!pattern` entry switches off what an earlier one marked. The
//! `test` role's built-in list is `[scan] test_globs`, the one list the document tier also uses.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::rules::{RuleSet, compile_rules};
use super::v1::ScanConfig;

/// What a path is for, when it is not plain product code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Tests, fixtures, mocks, snapshots and end-to-end suites.
    Test,
    /// Infrastructure and delivery: Terraform, containers, Helm and Kubernetes, Ansible, CI
    /// pipelines, build files, cloud-init and systemd units.
    Infra,
    /// Code a tool wrote.
    Generated,
    /// Third-party code checked into the tree.
    Vendored,
    /// Examples and samples.
    Example,
}

impl Role {
    /// Every role, in the order lists and counts report them.
    pub const ALL: [Self; 5] = [Self::Test, Self::Infra, Self::Generated, Self::Vendored, Self::Example];

    /// The lowercase name config keys, tool arguments and `index_status` use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Infra => "infra",
            Self::Generated => "generated",
            Self::Vendored => "vendored",
            Self::Example => "example",
        }
    }
}

const INFRA_GLOBS: &[&str] = &[
    // Terraform, Terragrunt and other HCL.
    "**/*.tf",
    "**/*.tf.json",
    "**/*.tfvars",
    "**/*.hcl",
    // Containers.
    "**/Dockerfile",
    "**/Dockerfile.*",
    "**/*.Dockerfile",
    "**/Containerfile",
    "**/Containerfile.*",
    // Helm charts and Kubernetes manifests.
    "**/Chart.yaml",
    "**/helm/**",
    "**/charts/**",
    "**/k8s/**",
    "**/kubernetes/**",
    "**/kustomize/**",
    "**/kustomization.yaml",
    "**/kustomization.yml",
    // Ansible roles and playbooks.
    "**/ansible/**",
    "**/ansible.cfg",
    "**/playbooks/**",
    "**/roles/*/tasks/**",
    "**/roles/*/handlers/**",
    // CI pipelines.
    ".github/workflows/**",
    ".github/actions/**",
    "**/.gitlab-ci.yml",
    ".circleci/**",
    ".buildkite/**",
    "**/Jenkinsfile",
    "**/Jenkinsfile.*",
    "**/azure-pipelines.yml",
    "**/.drone.yml",
    // Bazel and Starlark build files.
    "**/BUILD",
    "**/BUILD.bazel",
    "**/WORKSPACE",
    "**/WORKSPACE.bazel",
    "**/MODULE.bazel",
    "**/*.bzl",
    "**/.bazelrc",
    // cloud-init and systemd.
    "**/cloud-init/**",
    "**/cloud-config.yaml",
    "**/cloud-config.yml",
    "**/systemd/**",
    "**/*.service",
    "**/*.socket",
    "**/*.timer",
];

const GENERATED_GLOBS: &[&str] = &[
    "**/*.pb.go",
    "**/*.pb.rs",
    "**/*_pb2.py",
    "**/*_pb2.pyi",
    "**/*_pb2_grpc.py",
    "**/*.generated.*",
    "**/*.gen.*",
    "**/__generated__/**",
];

const VENDORED_GLOBS: &[&str] = &["**/vendor/**", "**/third_party/**", "**/node_modules/**"];

const EXAMPLE_GLOBS: &[&str] = &["**/examples/**", "**/example/**", "**/samples/**"];

/// `[scan.roles]`: the repository's own globs for each role, applied after the built-in ones. Same
/// syntax as `[scan] list_only`, ordered like a `.gitignore`: a `!pattern` entry re-includes a path
/// an earlier entry, built-in or not, marked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    /// Extra `test` globs, after `[scan] test_globs`.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub test: Vec<String>,
    /// Extra `infra` globs, after the built-in Terraform, container, Helm and Kubernetes, Ansible, CI,
    /// Bazel, cloud-init and systemd shapes.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub infra: Vec<String>,
    /// Extra `generated` globs, after the built-in protobuf and `*.generated.*` shapes.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub generated: Vec<String>,
    /// Extra `vendored` globs, after `vendor`, `third_party` and `node_modules`.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub vendored: Vec<String>,
    /// Extra `example` globs, after `examples`, `example` and `samples`.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub example: Vec<String>,
}

impl RolesConfig {
    /// Does the table set no list? Such a table is left out when the config is written back.
    pub fn is_empty(&self) -> bool {
        Role::ALL.iter().all(|role| self.list(*role).is_empty())
    }

    fn list(&self, role: Role) -> &[String] {
        match role {
            Role::Test => &self.test,
            Role::Infra => &self.infra,
            Role::Generated => &self.generated,
            Role::Vendored => &self.vendored,
            Role::Example => &self.example,
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Role::ALL.into_iter().find(|role| role.as_str() == name).ok_or_else(|| {
            let known: Vec<&str> = Role::ALL.iter().map(|role| role.as_str()).collect();
            format!("unknown role `{name}`; the roles are {}", known.join(", "))
        })
    }
}

/// A tool's `roles` argument: which files a call may see by the roles they carry.
///
/// The value is `include` (every file), `exclude` (files with no role), `only` (files with some
/// role), a comma-separated list of role names (files carrying one of them), or the same list with
/// each name prefixed by `-` (files carrying none of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleFilter {
    /// Every file passes.
    Include,
    /// A file passes when it carries none of these roles.
    Exclude(Vec<Role>),
    /// A file passes when it carries at least one of these roles.
    Only(Vec<Role>),
}

impl RoleFilter {
    /// Parse a `roles` argument; an unknown name or a list mixing `-name` with plain names is an
    /// error saying what the argument accepts.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        match value {
            "include" => return Ok(Self::Include),
            "exclude" => return Ok(Self::Exclude(Role::ALL.to_vec())),
            "only" => return Ok(Self::Only(Role::ALL.to_vec())),
            _ => {}
        }
        let names: Vec<&str> = value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        if names.is_empty() {
            return Err("`roles` is empty; pass `include`, `exclude`, `only` or role names".to_string());
        }
        let negated = names.iter().filter(|name| name.starts_with('-')).count();
        if negated != 0 && negated != names.len() {
            return Err("`roles` mixes `-role` with plain role names; use one form".to_string());
        }
        let roles = names
            .iter()
            .map(|name| name.trim_start_matches('-').parse::<Role>())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(if negated == 0 {
            Self::Only(roles)
        } else {
            Self::Exclude(roles)
        })
    }

    /// Does a file carrying `roles` pass?
    pub fn admits(&self, roles: &[Role]) -> bool {
        match self {
            Self::Include => true,
            Self::Exclude(drop) => !roles.iter().any(|role| drop.contains(role)),
            Self::Only(keep) => roles.iter().any(|role| keep.contains(role)),
        }
    }
}

/// The built-in globs of `role`; `[scan] test_globs` is the `test` role's.
fn builtin(role: Role, scan: &ScanConfig) -> Vec<&str> {
    match role {
        Role::Test => scan.test_globs.iter().map(String::as_str).collect(),
        Role::Infra => INFRA_GLOBS.to_vec(),
        Role::Generated => GENERATED_GLOBS.to_vec(),
        Role::Vendored => VENDORED_GLOBS.to_vec(),
        Role::Example => EXAMPLE_GLOBS.to_vec(),
    }
}

/// Classifies repo-relative paths into roles from the built-in globs and `[scan.roles]`.
#[derive(Debug, Clone, Default)]
pub struct PathRoles {
    rules: Vec<(Role, RuleSet)>,
}

impl PathRoles {
    /// Compile every role's list; an invalid glob is a config error naming its role.
    pub fn new(scan: &ScanConfig) -> Result<Self, String> {
        let rules = Role::ALL
            .into_iter()
            .map(|role| {
                let patterns = builtin(role, scan)
                    .into_iter()
                    .chain(scan.roles.list(role).iter().map(String::as_str));
                compile_rules(patterns)
                    .map(|set| (role, set))
                    .map_err(|e| format!("invalid glob in [scan.roles] {}: {e}", role.as_str()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }

    /// The roles of `scan`, falling back to the built-in globs alone when the configured lists do
    /// not compile (validation rejects that earlier, so this is a safety net).
    pub fn from_scan(scan: &ScanConfig) -> Self {
        Self::new(scan).unwrap_or_else(|_| Self::new(&ScanConfig::default()).unwrap_or_default())
    }

    /// The roles of the repo-relative, forward-slash `path`, in [`Role::ALL`] order; empty for plain
    /// product code.
    pub fn roles_of(&self, path: &str) -> Vec<Role> {
        self.rules
            .iter()
            .filter(|(_, set)| set.is_match(path))
            .map(|(role, _)| *role)
            .collect()
    }

    /// Does `path` carry `role`?
    pub fn has(&self, path: &str, role: Role) -> bool {
        self.rules
            .iter()
            .any(|(candidate, set)| *candidate == role && set.is_match(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(scan_roles: RolesConfig) -> PathRoles {
        let scan = ScanConfig {
            roles: scan_roles,
            ..ScanConfig::default()
        };
        PathRoles::new(&scan).expect("globs compile")
    }

    fn names(roles: &PathRoles, path: &str) -> Vec<&'static str> {
        roles.roles_of(path).into_iter().map(Role::as_str).collect()
    }

    #[test]
    fn infra_shapes_are_marked_without_configuration() {
        let roles = roles(RolesConfig::default());
        for path in [
            "deploy/main.tf",
            "live/prod/terragrunt.hcl",
            "svc/Dockerfile",
            "svc/Dockerfile.dev",
            "svc/app.Dockerfile",
            "svc/Containerfile",
            "charts/app/Chart.yaml",
            "ops/helm/values.yaml",
            "ops/k8s/deployment.yaml",
            "ops/ansible/site.yml",
            "ops/playbooks/db.yml",
            "ops/roles/web/tasks/main.yml",
            ".github/workflows/ci.yml",
            "svc/Jenkinsfile",
            "svc/BUILD.bazel",
            "tools/defs.bzl",
            "host/cloud-init/user-data",
            "host/app.service",
        ] {
            assert_eq!(names(&roles, path), ["infra"], "{path} is infra");
        }
        for path in [
            "src/lib.rs",
            "docs/dockerfile.md",
            "src/terraform.rs",
            "src/services.rs",
            "ops/roles/web/meta/main.yml",
            "src/build.rs",
        ] {
            assert!(roles.roles_of(path).is_empty(), "{path} has no role");
        }
    }

    #[test]
    fn every_role_has_a_built_in_list() {
        let roles = roles(RolesConfig::default());
        for (path, role) in [
            ("src/tests/a.rs", "test"),
            ("api/user.pb.go", "generated"),
            ("api/user_pb2.py", "generated"),
            ("third_party/zlib/a.c", "vendored"),
            ("examples/basic/main.rs", "example"),
        ] {
            assert_eq!(names(&roles, path), [role], "{path}");
        }
    }

    #[test]
    fn a_path_carries_every_role_it_matches() {
        let roles = roles(RolesConfig::default());
        assert_eq!(names(&roles, "ops/tests/main.tf"), ["test", "infra"]);
    }

    #[test]
    fn a_negated_entry_turns_a_built_in_off() {
        let roles = roles(RolesConfig {
            infra: vec!["!**/*.service".to_string(), "!.github/workflows/**".to_string()],
            ..RolesConfig::default()
        });
        assert!(roles.roles_of("host/app.service").is_empty());
        assert!(roles.roles_of(".github/workflows/ci.yml").is_empty());
        assert!(roles.has("main.tf", Role::Infra), "the rest of the list stays on");
    }

    #[test]
    fn a_later_entry_beats_an_earlier_negation() {
        let roles = roles(RolesConfig {
            infra: vec!["!**/*.service".to_string(), "host/**".to_string()],
            ..RolesConfig::default()
        });
        assert!(roles.has("host/app.service", Role::Infra));
    }

    #[test]
    fn configured_lists_add_to_the_built_ins() {
        let roles = roles(RolesConfig {
            infra: vec!["deploy/**".to_string()],
            generated: vec!["gen/".to_string()],
            test: vec!["qa/**".to_string(), "!**/*.bats".to_string()],
            ..RolesConfig::default()
        });
        assert!(roles.has("deploy/run.sh", Role::Infra));
        assert!(roles.has("gen/a.rs", Role::Generated));
        assert!(roles.has("qa/a.rs", Role::Test));
        assert!(roles.has("tests/a.rs", Role::Test), "test_globs stay on");
        assert!(
            !roles.has("ci/run.bats", Role::Test),
            "a `!` entry turns a test glob off"
        );
    }

    #[test]
    fn the_test_role_follows_scan_test_globs() {
        let scan = ScanConfig {
            test_globs: vec!["qa/**".to_string()],
            ..ScanConfig::default()
        };
        let roles = PathRoles::new(&scan).expect("globs compile");
        assert!(roles.has("qa/a.rs", Role::Test));
        assert!(!roles.has("tests/a.rs", Role::Test));
    }

    #[test]
    fn an_invalid_glob_is_an_error_naming_its_role() {
        let scan = ScanConfig {
            roles: RolesConfig {
                generated: vec!["a/[".to_string()],
                ..RolesConfig::default()
            },
            ..ScanConfig::default()
        };
        let err = PathRoles::new(&scan).expect_err("an unclosed class does not compile");
        assert!(err.starts_with("invalid glob in [scan.roles] generated:"), "{err}");
        assert!(
            PathRoles::from_scan(&scan).has("main.tf", Role::Infra),
            "the fallback keeps the built-ins"
        );
    }

    #[test]
    fn an_unset_roles_table_is_empty() {
        assert!(RolesConfig::default().is_empty());
        assert!(
            !RolesConfig {
                example: vec!["demo/".to_string()],
                ..RolesConfig::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn a_roles_argument_parses_into_a_filter() {
        for (value, expected) in [
            ("include", RoleFilter::Include),
            ("exclude", RoleFilter::Exclude(Role::ALL.to_vec())),
            ("only", RoleFilter::Only(Role::ALL.to_vec())),
            ("infra", RoleFilter::Only(vec![Role::Infra])),
            (" test , infra ", RoleFilter::Only(vec![Role::Test, Role::Infra])),
            (
                "-test,-generated",
                RoleFilter::Exclude(vec![Role::Test, Role::Generated]),
            ),
        ] {
            assert_eq!(RoleFilter::parse(value), Ok(expected), "{value}");
        }
    }

    #[test]
    fn a_bad_roles_argument_is_refused_with_what_it_accepts() {
        for value in ["", " , ", "tests", "-infra,test", "only,infra"] {
            assert!(RoleFilter::parse(value).is_err(), "{value:?} is refused");
        }
        let err = RoleFilter::parse("tests").expect_err("unknown role");
        assert!(err.contains("unknown role `tests`") && err.contains("infra"), "{err}");
    }

    #[test]
    fn a_filter_admits_files_by_the_roles_they_carry() {
        let plain: [Role; 0] = [];
        let both = [Role::Test, Role::Infra];
        let cases = [
            ("include", &plain[..], true),
            ("include", &both[..], true),
            ("exclude", &plain[..], true),
            ("exclude", &both[..], false),
            ("only", &plain[..], false),
            ("only", &both[..], true),
            ("infra", &both[..], true),
            ("generated", &both[..], false),
            ("-test", &both[..], false),
            ("-generated", &both[..], true),
            ("-generated", &plain[..], true),
        ];
        for (value, roles, admitted) in cases {
            let filter = RoleFilter::parse(value).expect("valid");
            assert_eq!(filter.admits(roles), admitted, "{value} over {roles:?}");
        }
    }
}

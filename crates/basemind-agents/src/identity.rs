//! The identity a launched agent carries into basemind's agent comms.
//!
//! The grammar and the variable names are those of `docs/comms-v2.md`: a principal matches
//! `[A-Za-z0-9_:-]{1,128}`, a sub-name `[a-z0-9_-]{1,32}`, and `.` separates the two.

use crate::error::LaunchError;

/// The variable `basemind serve`, the CLI and the hooks read the principal from.
pub const ENV_AGENT_ID: &str = "BASEMIND_AGENT_ID";
/// The variable naming the agent harness, used by the derived id.
pub const ENV_HOST: &str = "BASEMIND_HOST";

const MAX_ID_BYTES: usize = 128;
const MAX_SUB_NAME_BYTES: usize = 32;
const MAX_HOST_BYTES: usize = 16;

/// The principal a launched agent acts as, validated when it is built.
///
/// An id that fails the grammar is refused here rather than exported: the resolver would
/// silently fall through to a derived id, and the launched agent would then post as someone
/// other than the identity its launcher announced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    principal: String,
    host: Option<String>,
}

impl AgentIdentity {
    /// An identity for `principal`.
    ///
    /// # Errors
    ///
    /// [`LaunchError::InvalidIdentity`] when `principal` does not match the grammar.
    pub fn new(principal: impl Into<String>) -> Result<Self, LaunchError> {
        let principal = principal.into();
        let valid = (1..=MAX_ID_BYTES).contains(&principal.len())
            && principal
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-'));
        if !valid {
            return Err(LaunchError::InvalidIdentity(principal));
        }
        Ok(Self { principal, host: None })
    }

    /// The same identity with an explicit `BASEMIND_HOST` instead of the harness id.
    ///
    /// # Errors
    ///
    /// [`LaunchError::InvalidHost`] unless `host` is `[a-z0-9]{1,16}`: the comms resolver
    /// reduces a host to that form, so anything else would be exported as one value and read
    /// as another.
    pub fn with_host(mut self, host: impl Into<String>) -> Result<Self, LaunchError> {
        let host = host.into();
        let valid = (1..=MAX_HOST_BYTES).contains(&host.len())
            && host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !valid {
            return Err(LaunchError::InvalidHost(host));
        }
        self.host = Some(host);
        Ok(self)
    }

    /// The principal, as exported in `BASEMIND_AGENT_ID`.
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// The composed id `<principal>.<name>` a call addresses with `as: "<name>"`.
    ///
    /// # Errors
    ///
    /// [`LaunchError::InvalidSubName`] when `name` fails the grammar or the composed id is
    /// longer than 128 bytes.
    pub fn sub_identity(&self, name: &str) -> Result<String, LaunchError> {
        let valid = (1..=MAX_SUB_NAME_BYTES).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'));
        let composed = format!("{}.{name}", self.principal);
        if !valid || composed.len() > MAX_ID_BYTES {
            return Err(LaunchError::InvalidSubName(name.to_owned()));
        }
        Ok(composed)
    }

    /// The variables to export, with `default_host` standing in when no host was set.
    pub(crate) fn env(&self, default_host: &str) -> [(&'static str, String); 2] {
        [
            (ENV_AGENT_ID, self.principal.clone()),
            (ENV_HOST, self.host.clone().unwrap_or_else(|| default_host.to_owned())),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_grammar_is_enforced() {
        assert!(AgentIdentity::new("codex-basemind-1a2b3c").is_ok());
        assert!(AgentIdentity::new("team:reviewer_2").is_ok());
        assert!(AgentIdentity::new("a".repeat(128)).is_ok());
        for bad in ["", "has.dot", "has space", "naïve", &"a".repeat(129)] {
            assert!(
                matches!(AgentIdentity::new(bad), Err(LaunchError::InvalidIdentity(_))),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn sub_identity_composes_with_a_dot_and_checks_both_limits() {
        let identity = AgentIdentity::new("lead").unwrap();
        assert_eq!(identity.sub_identity("reviewer-1").unwrap(), "lead.reviewer-1");
        for bad in ["", "Upper", "a.b", &"a".repeat(33)] {
            assert!(identity.sub_identity(bad).is_err(), "{bad:?} was accepted");
        }
        let long = AgentIdentity::new("p".repeat(120)).unwrap();
        assert!(long.sub_identity("sevenby").is_ok());
        assert!(long.sub_identity("eightbyt").is_err());
    }

    #[test]
    fn exports_the_principal_and_falls_back_to_the_harness_as_host() {
        let identity = AgentIdentity::new("lead").unwrap();
        assert_eq!(
            identity.env("codex"),
            [(ENV_AGENT_ID, "lead".to_owned()), (ENV_HOST, "codex".to_owned())]
        );
        assert_eq!(
            identity.clone().with_host("custom2").unwrap().env("codex")[1].1,
            "custom2"
        );
    }

    #[test]
    fn a_host_the_resolver_would_rewrite_is_refused() {
        let identity = AgentIdentity::new("lead").unwrap();
        for bad in ["", "Claude", "my-host", "a\nBASEMIND_AGENT_ID=x", &"h".repeat(17)] {
            assert!(
                matches!(identity.clone().with_host(bad), Err(LaunchError::InvalidHost(_))),
                "{bad:?} was accepted"
            );
        }
        assert!(identity.with_host("h".repeat(16)).is_ok());
    }
}

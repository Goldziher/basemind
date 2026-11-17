//! The identity a launched agent carries into basemind's agent comms.
//!
//! The grammar and the variable names are those of `docs/comms-v2.md`: a principal matches
//! `[A-Za-z0-9_:-]{1,128}`, a sub-name `[a-z0-9_-]{1,32}`, and `.` separates the two.

use crate::error::LaunchError;

/// The variable `basemind serve`, the CLI and the hooks read the principal from.
pub const ENV_AGENT_ID: &str = "BASEMIND_AGENT_ID";
/// The variable naming the agent harness, used by the derived id.
pub const ENV_HOST: &str = "BASEMIND_HOST";
/// The variable carrying a launched agent's depth: 1 for an agent a top-level principal
/// launched, one more for each launch below it. Informational: the supervisor reads the depth
/// from its own record of the launch, never from this variable.
pub const ENV_AGENT_DEPTH: &str = "BASEMIND_AGENT_DEPTH";
/// The variable carrying a launched agent's launch token, which it presents to act as the
/// session the supervisor launched (see [`crate::Caller::launched`]).
pub const ENV_LAUNCH_TOKEN: &str = "BASEMIND_LAUNCH_TOKEN";

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
    lineage: Option<Lineage>,
}

/// Where a launched agent sits in the launch tree, exported beside its principal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Lineage {
    depth: u32,
    token: LaunchToken,
}

/// The secret a supervisor hands a launched agent, through [`ENV_LAUNCH_TOKEN`] only. A caller
/// that presents it acts as that session. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LaunchToken(String);

impl std::fmt::Debug for LaunchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchToken(<redacted>)")
    }
}

/// Bytes of operating-system randomness in a token: 256 bits.
const TOKEN_BYTES: usize = 32;

impl LaunchToken {
    /// A fresh token: 32 bytes of the operating system's randomness, as lower-case hex.
    pub(crate) fn generate() -> std::io::Result<Self> {
        Ok(Self(hex(&random_bytes::<TOKEN_BYTES>()?)))
    }

    /// Whether `presented` is this token, compared in time independent of where they differ.
    pub(crate) fn matches(&self, presented: &str) -> bool {
        let (ours, theirs) = (self.0.as_bytes(), presented.as_bytes());
        ours.len() == theirs.len() && ours.iter().zip(theirs).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }

    #[cfg(all(test, unix))]
    pub(crate) fn value(&self) -> &str {
        &self.0
    }
}

/// `N` bytes from the operating system's random source.
pub(crate) fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut bytes = [0; N];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    #[cfg(not(unix))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no random source off Unix",
    ))
}

/// `bytes` as lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
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
        Ok(Self {
            principal,
            host: None,
            lineage: None,
        })
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

    /// The explicit host, when one was set.
    pub(crate) fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The same identity marked as launched at `depth`, carrying `token`.
    pub(crate) fn with_lineage(mut self, depth: u32, token: LaunchToken) -> Self {
        self.lineage = Some(Lineage { depth, token });
        self
    }

    /// The launch depth this identity is exported with, when a supervisor launched it.
    #[must_use]
    pub fn depth(&self) -> Option<u32> {
        self.lineage.as_ref().map(|lineage| lineage.depth)
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

    /// The variables to export, with `default_host` standing in when no host was set, and the
    /// depth and launch token when a supervisor launched the agent.
    pub(crate) fn env(&self, default_host: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (ENV_AGENT_ID, self.principal.clone()),
            (ENV_HOST, self.host.clone().unwrap_or_else(|| default_host.to_owned())),
        ];
        if let Some(lineage) = &self.lineage {
            env.push((ENV_AGENT_DEPTH, lineage.depth.to_string()));
            env.push((ENV_LAUNCH_TOKEN, lineage.token.0.clone()));
        }
        env
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
        assert_eq!(identity.depth(), None);
        assert_eq!(
            identity.clone().with_host("custom2").unwrap().env("codex")[1].1,
            "custom2"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn no_launch_token_is_made_off_unix() {
        let refused = LaunchToken::generate().map(|_| ()).map_err(|error| error.kind());
        assert_eq!(refused, Err(std::io::ErrorKind::Unsupported));
    }

    #[cfg(unix)]
    #[test]
    fn a_launched_identity_exports_its_depth_and_token_and_hides_the_token_in_debug() {
        let token = LaunchToken::generate().unwrap();
        let secret = token.value().to_owned();
        assert_eq!(secret.len(), 64);
        assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(LaunchToken::generate().unwrap(), token, "two tokens were equal");
        let identity = AgentIdentity::new("launched").unwrap().with_lineage(2, token.clone());
        assert_eq!(identity.depth(), Some(2));
        let env = identity.env("claude");
        assert!(env.contains(&(ENV_AGENT_DEPTH, "2".to_owned())));
        assert!(env.contains(&(ENV_LAUNCH_TOKEN, secret.clone())));
        assert!(!format!("{identity:?}").contains(&secret));
        assert!(token.matches(&secret));
        assert!(!token.matches(&secret[1..]));
        assert!(!token.matches(&format!("{}0", &secret[..63])) || secret.ends_with('0'));
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

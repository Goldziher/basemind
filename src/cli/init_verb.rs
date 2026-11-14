//! The verb of a planned `basemind init` write.

/// The action a `Change::Write` performs. One source for both the real run ("wrote") and the
/// `--print` preview ("would write"), so the two can never drift into "would wrote".
#[derive(Clone, Copy)]
pub(crate) enum Verb {
    Write,
    Inject,
    Add,
}

impl Verb {
    pub(super) fn imperative(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Inject => "inject",
            Self::Add => "add",
        }
    }

    pub(super) fn past(self) -> &'static str {
        match self {
            Self::Write => "wrote",
            Self::Inject => "injected",
            Self::Add => "added",
        }
    }
}

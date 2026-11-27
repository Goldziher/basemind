//! Closed value sets for CLI flags, as clap value enums.
//!
//! The MCP parameters carry these as free strings and reject unknown values at call time; the CLI
//! knows the sets, so a typo fails at parse time with the valid choices listed and the shell
//! completions can offer them. Each variant maps back to the exact wire string via `as_str`.

macro_rules! choice_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
        pub enum $name {
            $(#[value(name = $wire)] $variant),+
        }

        impl $name {
            /// The wire string the MCP parameter expects.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

choice_enum! {
    /// `code refs` name matching.
    RefsMatch { Exact => "exact", Substring => "substring" }
}

choice_enum! {
    /// `code find --indexed`: which group of files to list.
    IndexedChoice { True => "true", False => "false", Any => "any" }
}

impl IndexedChoice {
    /// The `find_files` filter this choice stands for.
    pub fn filter(self) -> crate::engine::types::files::IndexedFilter {
        use crate::engine::types::files::{IndexedFilter, IndexedWord};
        match self {
            Self::True => IndexedFilter::Flag(true),
            Self::False => IndexedFilter::Flag(false),
            Self::Any => IndexedFilter::Word(IndexedWord::Any),
        }
    }
}

choice_enum! {
    /// `code search` retrieval lane.
    SearchLane { Hybrid => "hybrid", Semantic => "semantic", Keyword => "keyword" }
}

choice_enum! {
    /// `code refs` relation.
    RefsRelation {
        Callers => "callers",
        Implementations => "implementations",
        Importers => "importers",
        Definition => "definition",
        Cycles => "cycles",
    }
}

choice_enum! {
    /// `admin telemetry` aggregation window.
    TelemetryWindow { Today => "today", Hour => "1h", Day => "24h", All => "all" }
}

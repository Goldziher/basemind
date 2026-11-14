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
    /// `code semantic` retrieval lane.
    SearchLane { Hybrid => "hybrid", Semantic => "semantic", Keyword => "keyword" }
}

choice_enum! {
    /// `git search` field.
    CommitField { Author => "author", Message => "message", All => "all" }
}

choice_enum! {
    /// `git symbol-history` fingerprint strategy.
    HashMode { Normalized => "normalized", Structural => "structural", StructuralLoose => "structural_loose" }
}

choice_enum! {
    /// `admin telemetry` aggregation window.
    TelemetryWindow { Today => "today", Hour => "1h", Day => "24h", All => "all" }
}

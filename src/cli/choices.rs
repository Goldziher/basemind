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
    /// `graph calls` walk direction.
    CallDirection { Callers => "callers", Callees => "callees" }
}

choice_enum! {
    /// `graph neighbors` edge direction.
    NeighborDirection { Both => "both", Out => "out", In => "in" }
}

choice_enum! {
    /// Edge lanes a graph is built over.
    EdgeLane {
        All => "all",
        Calls => "calls",
        Imports => "imports",
        Inherits => "inherits",
        Both => "both",
        Contains => "contains",
    }
}

choice_enum! {
    /// Community-detection algorithm.
    CommunityAlgorithm { LabelPropagation => "label_propagation", Louvain => "louvain" }
}

choice_enum! {
    /// `graph map` rollup level.
    Granularity { Module => "module", File => "file", Symbol => "symbol" }
}

choice_enum! {
    /// `graph export` output format.
    ExportFormat {
        NodeLink => "node_link",
        Dot => "dot",
        Mermaid => "mermaid",
        Graphml => "graphml",
        Cypher => "cypher",
        Html => "html",
        Svg => "svg",
    }
}

choice_enum! {
    /// `graph display` / `graph open` output format (visual formats only).
    VisualFormat { Html => "html", Svg => "svg" }
}

choice_enum! {
    /// `code semantic` retrieval lane.
    SearchLane { Hybrid => "hybrid", Semantic => "semantic", Keyword => "keyword" }
}

choice_enum! {
    /// Wire encoding of a tool response body.
    WireFormat { Json => "json", Toon => "toon" }
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

choice_enum! {
    /// `admin compress` reduction intensity.
    CompressLevel {
        Off => "off",
        Light => "light",
        Moderate => "moderate",
        Aggressive => "aggressive",
        Maximum => "maximum",
    }
}

choice_enum! {
    /// `memory proposals` kind filter.
    ProposalKind { Skill => "skill", Memory => "memory" }
}

pub mod l1;
pub mod l2;
pub mod l3;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::lang::LangError;

/// Bumped any time the FileMap layout changes in an incompatible way OR the on-disk
/// directory shape changes. Stored in every serialized FileMap. Mismatch on read =
/// auto-wipe + re-scan. v3 = per-view index directories under `.gitmind/views/`.
pub const SCHEMA_VER: u16 = 3;

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("non-utf8 source")]
    NonUtf8,
    #[error("tree-sitter parse failure")]
    ParseFailure,
    #[error(transparent)]
    Lang(#[from] LangError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMapL1 {
    pub schema_ver: u16,
    pub language: String,
    pub size_bytes: u64,
    /// True when tree-sitter recovered from one or more syntax errors.
    /// The map still contains every symbol/import the parser was able to identify.
    pub had_errors: bool,
    pub error_count: u32,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_row: u32,
    pub start_col: u32,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Class,
    Interface,
    Trait,
    Type,
    Const,
    Module,
    Macro,
    Unknown,
}

impl SymbolKind {
    pub fn from_capture_suffix(suffix: &str) -> Self {
        match suffix {
            "function" => Self::Function,
            "method" => Self::Method,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "class" => Self::Class,
            "interface" => Self::Interface,
            "trait" => Self::Trait,
            "type" => Self::Type,
            "const" => Self::Const,
            "module" => Self::Module,
            "macro" => Self::Macro,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Import {
    /// Best-effort module path / symbol; None when the language doesn't expose one cleanly.
    pub module: Option<String>,
    pub raw: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMapL2 {
    pub schema_ver: u16,
    pub language: String,
    pub calls: Vec<Call>,
    pub docs: Vec<DocComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Call {
    pub callee: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocComment {
    pub text: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

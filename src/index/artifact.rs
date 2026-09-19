//! Parsed artifact model (plan section 27).
//!
//! A [`ParsedArtifact`] is the deterministic extraction result for one
//! file content in one language. Its identity is
//! `(content_key, language, parser_version)`: identical bytes parsed by the
//! same indexer version always produce the same artifact, independent of
//! path, branch, or worktree — which is exactly what lets artifacts be
//! cached and reused across branches and worktrees.
//!
//! Artifacts are cached as JSON in the shared cache's `parsed_artifact`
//! table; derived `symbol`, `reference`, and `text_chunk` rows are written
//! when the artifact is first parsed.

use serde::{Deserialize, Serialize};

use super::language::Language;

/// Version of the extraction logic.
///
/// Extraction output is a pure function of (content, language, this
/// version). Bumping this constant invalidates every cached artifact and
/// forces a re-parse, which is the only supported upgrade path for changed
/// extraction logic.
pub const PARSER_VERSION: &str = "1";

/// Kind of an extracted definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A free function or procedure.
    Function,
    /// A function bound to a type (method, member function).
    Method,
    /// A class.
    Class,
    /// A struct.
    Struct,
    /// An enum.
    Enum,
    /// A union.
    Union,
    /// A type alias or typedef.
    Type,
    /// An interface, trait, or mixin.
    Interface,
    /// A module or namespace.
    Module,
}

impl SymbolKind {
    /// Stable label used in storage.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Union => "union",
            SymbolKind::Type => "type",
            SymbolKind::Interface => "interface",
            SymbolKind::Module => "module",
        }
    }
}

/// One extracted definition with its source span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    /// Simple name of the definition (no qualification).
    pub name: String,
    /// Classification of the definition.
    pub kind: SymbolKind,
    /// 1-based first line.
    pub line_start: usize,
    /// 1-based last line.
    pub line_end: usize,
    /// 0-based byte offset of the definition's first byte.
    pub byte_start: usize,
    /// 0-based byte offset one past the definition's last byte.
    pub byte_end: usize,
}

/// One extracted import/include/using statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDecl {
    /// Import statement with whitespace collapsed to single spaces.
    pub text: String,
    /// 1-based line of the statement start.
    pub line: usize,
}

/// One identifier reference occurrence.
///
/// Occurrences are deduplicated per `(name, line)` and bounded per file;
/// definition-name occurrences are excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentifierRef {
    /// Referenced name.
    pub name: String,
    /// 1-based line of the occurrence.
    pub line: usize,
}

/// Extraction output of one parse, before content-key stamping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedIndex {
    /// Definitions in document order.
    pub symbols: Vec<SymbolDef>,
    /// Import-like statements in document order.
    pub imports: Vec<ImportDecl>,
    /// Identifier references in document order, deduplicated per
    /// `(name, line)`.
    pub references: Vec<IdentifierRef>,
    /// Compact text skeleton of the file structure.
    pub skeleton: String,
}

/// Maximum number of reference rows kept per file.
///
/// Generated or minified files can contain enormous numbers of identifier
/// occurrences; extraction stays deterministic by keeping the first
/// occurrences in document order.
pub const REFERENCE_ROW_LIMIT: usize = 4096;

/// A parsed, content-addressed source artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedArtifact {
    /// Content key of the parsed bytes (`git:<oid>` or `sha256:<hex>`).
    pub content_key: String,
    /// Language the bytes were parsed as.
    pub language: Language,
    /// Extraction version that produced this artifact.
    pub parser_version: String,
    /// Extracted definitions.
    pub symbols: Vec<SymbolDef>,
    /// Extracted imports/includes/usings.
    pub imports: Vec<ImportDecl>,
    /// Extracted identifier references.
    pub references: Vec<IdentifierRef>,
    /// Compact source skeleton.
    pub skeleton: String,
}

impl ParsedArtifact {
    /// Stamp an extraction with its identity.
    pub fn new(content_key: &str, language: Language, extracted: ExtractedIndex) -> Self {
        ParsedArtifact {
            content_key: content_key.to_owned(),
            language,
            parser_version: PARSER_VERSION.to_owned(),
            symbols: extracted.symbols,
            imports: extracted.imports,
            references: extracted.references,
            skeleton: extracted.skeleton,
        }
    }

    /// Stable identity of this artifact: `(content_key, language,
    /// parser_version)`.
    pub fn identity(&self) -> (String, &'static str, String) {
        (
            self.content_key.clone(),
            self.language.label(),
            self.parser_version.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_roundtrip_through_json() {
        let extracted = ExtractedIndex {
            symbols: vec![SymbolDef {
                name: "Point".to_string(),
                kind: SymbolKind::Struct,
                line_start: 1,
                line_end: 4,
                byte_start: 0,
                byte_end: 42,
            }],
            imports: vec![ImportDecl {
                text: "use std::fmt;".to_string(),
                line: 1,
            }],
            references: vec![IdentifierRef {
                name: "Display".to_string(),
                line: 6,
            }],
            skeleton: "struct Point L1-L4".to_string(),
        };
        let artifact = ParsedArtifact::new("git:0123abcd", Language::Rust, extracted.clone());
        let json = serde_json::to_string(&artifact).unwrap();
        let back: ParsedArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(artifact, back);
        assert_eq!(artifact.parser_version, PARSER_VERSION);
    }

    #[test]
    fn artifact_identity_contains_all_three_components() {
        let artifact =
            ParsedArtifact::new("sha256:ff", Language::TypeScript, ExtractedIndex::default());
        let (key, language, version) = artifact.identity();
        assert_eq!(key, "sha256:ff");
        assert_eq!(language, "typescript");
        assert_eq!(version, PARSER_VERSION);
    }

    #[test]
    fn symbol_kind_labels_match_serde_names() {
        for (kind, label) in [
            (SymbolKind::Function, "function"),
            (SymbolKind::Method, "method"),
            (SymbolKind::Class, "class"),
            (SymbolKind::Struct, "struct"),
            (SymbolKind::Enum, "enum"),
            (SymbolKind::Union, "union"),
            (SymbolKind::Type, "type"),
            (SymbolKind::Interface, "interface"),
            (SymbolKind::Module, "module"),
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{label}\"")
            );
            assert_eq!(kind.label(), label);
            let back: SymbolKind = serde_json::from_str(&format!("\"{label}\"")).unwrap();
            assert_eq!(back, kind);
        }
        assert!(serde_json::from_str::<SymbolKind>("\"namespace\"").is_err());
    }
}

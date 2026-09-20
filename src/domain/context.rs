//! Retrieved context entries and bundles (plan sections 13, 30, 31).
//!
//! Retrieval turns a [`RetrievalQuery`](crate::retrieval::query::RetrievalQuery)
//! into a bounded, ranked [`ContextBundle`]. Entries are the common
//! currency of reviewer initial context and reviewer lookup tools:
//! source definitions and references, project facts and lessons, and
//! built-in practices. Every source entry is snapshot-bound: it is only
//! produced when its content is part of the target snapshot's worktree
//! manifest (plan section 26).

use serde::{Deserialize, Serialize};

/// What kind of content a context entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    /// A definition extracted from target-snapshot source.
    SymbolDefinition,
    /// A reference to a symbol in target-snapshot source.
    SymbolReference,
    /// A compact source skeleton matched by text search.
    SourceSkeleton,
    /// Raw source content read back from the target snapshot by the
    /// `source_lookup` reviewer tool (plan section 22).
    Source,
    /// A supported (or conflicting) durable project fact.
    ProjectFact,
    /// A durable project lesson.
    ProjectLesson,
    /// A statement from a built-in practice pack.
    Practice,
    /// A consolidated group of validated reviewer findings, used as the
    /// input of the non-juror synthesis session (plan section 38).
    FindingGroup,
}

impl ContextKind {
    /// Stable label used in reports.
    pub fn label(self) -> &'static str {
        match self {
            ContextKind::SymbolDefinition => "symbol-definition",
            ContextKind::SymbolReference => "symbol-reference",
            ContextKind::SourceSkeleton => "source-skeleton",
            ContextKind::Source => "source",
            ContextKind::ProjectFact => "project-fact",
            ContextKind::ProjectLesson => "project-lesson",
            ContextKind::Practice => "practice",
            ContextKind::FindingGroup => "finding-group",
        }
    }

    /// Context-priority rank (plan section 31): affected source first,
    /// then verified facts, directly related source, lessons, and
    /// built-in practices. Lower ranks sort first on equal scores.
    /// `Source` and `SymbolReference` share the rank of directly related
    /// source: both are exact, reviewer-requested target-snapshot
    /// content rather than pre-selected affected source.
    pub fn priority(self) -> u8 {
        match self {
            ContextKind::SymbolDefinition => 0,
            ContextKind::ProjectFact => 1,
            ContextKind::SourceSkeleton => 2,
            ContextKind::Source | ContextKind::SymbolReference => 3,
            ContextKind::ProjectLesson => 4,
            ContextKind::Practice => 5,
            // Finding-group entries are direct review input of the
            // synthesis session, never a retrieval signal.
            ContextKind::FindingGroup => 0,
        }
    }
}

/// One bounded, ranked piece of retrieved context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEntry {
    /// What kind of content this entry carries.
    pub kind: ContextKind,
    /// Short human-readable heading (subject, symbol kind and name,
    /// practice title, …).
    pub title: String,
    /// Repository-relative path (or `.nisse/…` record path), when the
    /// entry is path-bound.
    pub path: Option<String>,
    /// Symbol name, when the entry is symbol-bound.
    pub symbol: Option<String>,
    /// First line (1-based, inclusive), when known.
    pub line_start: Option<u32>,
    /// Last line (1-based, inclusive), when known.
    pub line_end: Option<u32>,
    /// Durable record ID, for fact and lesson entries.
    pub record_id: Option<String>,
    /// Built-in practice ID, for practice entries.
    pub practice_id: Option<String>,
    /// Bounded excerpt or statement text.
    pub text: String,
    /// Deterministic relevance score; higher is better.
    pub score: f64,
}

impl ContextEntry {
    /// Byte length of this entry's text.
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }
}

/// The bounded result of one retrieval query.
///
/// Entries are sorted by descending score with deterministic
/// (kind-priority, path, symbol, line) tie-breaking, and the whole bundle
/// respects an explicit context budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBundle {
    /// Ranked entries within budget.
    pub entries: Vec<ContextEntry>,
    /// Whether more candidate entries existed than the budget admitted.
    pub truncated: bool,
    /// Total bytes of entry text in this bundle.
    pub text_bytes: usize,
}

impl ContextBundle {
    /// The empty bundle.
    pub fn empty() -> Self {
        ContextBundle {
            entries: Vec::new(),
            truncated: false,
            text_bytes: 0,
        }
    }

    /// Whether the bundle holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> ContextEntry {
        ContextEntry {
            kind: ContextKind::SymbolDefinition,
            title: "function enqueue".to_string(),
            path: Some("src/queue.rs".to_string()),
            symbol: Some("enqueue".to_string()),
            line_start: Some(42),
            line_end: Some(58),
            record_id: None,
            practice_id: None,
            text: "fn enqueue(&mut self, item: T)".to_string(),
            score: 1.0,
        }
    }

    #[test]
    fn context_kind_roundtrip() {
        for (kind, name) in [
            (ContextKind::SymbolDefinition, "\"symbol_definition\""),
            (ContextKind::SymbolReference, "\"symbol_reference\""),
            (ContextKind::SourceSkeleton, "\"source_skeleton\""),
            (ContextKind::Source, "\"source\""),
            (ContextKind::ProjectFact, "\"project_fact\""),
            (ContextKind::ProjectLesson, "\"project_lesson\""),
            (ContextKind::Practice, "\"practice\""),
            (ContextKind::FindingGroup, "\"finding_group\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
            let back: ContextKind = serde_json::from_str(name).unwrap();
            assert_eq!(kind, back);
            assert_eq!(kind.label().replace('-', "_"), name.trim_matches('"'));
        }
    }

    #[test]
    fn context_kind_rejects_unknown_variants() {
        assert!(serde_json::from_str::<ContextKind>("\"embedding\"").is_err());
        assert!(serde_json::from_str::<ContextKind>("\"SymbolDefinition\"").is_err());
    }

    #[test]
    fn context_priority_follows_plan_ordering() {
        // Affected source, then verified facts, directly related source,
        // lessons, built-in practices (plan section 31).
        assert!(ContextKind::SymbolDefinition.priority() < ContextKind::ProjectFact.priority());
        assert!(ContextKind::ProjectFact.priority() < ContextKind::SourceSkeleton.priority());
        assert!(ContextKind::SourceSkeleton.priority() < ContextKind::SymbolReference.priority());
        // Raw source and references share the directly-related rank.
        assert_eq!(
            ContextKind::Source.priority(),
            ContextKind::SymbolReference.priority()
        );
        assert!(ContextKind::SymbolReference.priority() < ContextKind::ProjectLesson.priority());
        assert!(ContextKind::ProjectLesson.priority() < ContextKind::Practice.priority());
    }

    #[test]
    fn context_entry_and_bundle_roundtrip() {
        let bundle = ContextBundle {
            entries: vec![sample_entry()],
            truncated: true,
            text_bytes: 29,
        };
        let json = serde_json::to_string(&bundle).unwrap();
        let back: ContextBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(bundle, back);
        assert!(json.contains("\"practice_id\""));
        assert!(json.contains("\"record_id\""));
        assert!(json.contains("\"truncated\""));
    }

    #[test]
    fn empty_bundle_is_empty() {
        let bundle = ContextBundle::empty();
        assert!(bundle.is_empty());
        assert!(!bundle.truncated);
        assert_eq!(bundle.text_bytes, 0);
    }
}

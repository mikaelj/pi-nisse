//! Retrieval queries, context budgets, and bundle assembly
//! (plan sections 30 and 31).
//!
//! A [`RetrievalQuery`] carries the free-text query plus the
//! deterministic signals Nisse knows without any LLM: the symbols and
//! paths affected by the work under review. Tokenization is
//! conservative: only alphanumeric/underscore runs become FTS terms, and
//! every term is emitted as a quoted FTS5 phrase, so arbitrary user
//! input can never alter FTS query syntax.
//!
//! Bundle assembly is fully deterministic: entries are ordered by
//! descending score with (kind priority, path, symbol, line,
//! record/practice id) tie-breaking, then truncated to an explicit
//! [`ContextBudget`].

use serde::Serialize;

use super::error::RetrievalError;
use crate::domain::context::{ContextBundle, ContextEntry};
use crate::domain::snapshot::SnapshotDescriptor;

/// Maximum number of search tokens derived from one query text.
pub const MAX_QUERY_TOKENS: usize = 16;

/// Maximum query text size in bytes.
pub const MAX_QUERY_BYTES: usize = 8192;

/// Maximum number of entries one signal (one symbol, one FTS source)
/// may contribute before ranking.
pub const MAX_ENTRIES_PER_SIGNAL: usize = 16;

/// Maximum number of query tokens used for exact symbol lookups.
pub const MAX_SYMBOL_TOKENS: usize = 8;

/// One retrieval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalQuery {
    /// Free-text query.
    pub text: String,
    /// Symbols affected by the work under review (for example changed
    /// symbols of a diff). Exact definitions and references of these
    /// symbols receive priority boosts.
    pub changed_symbols: Vec<String>,
    /// Paths affected by the work under review. Entries on these paths
    /// or in their directories are boosted (changed-path and
    /// module/path proximity signals).
    pub changed_paths: Vec<String>,
}

impl RetrievalQuery {
    /// A plain free-text query with no changed-set signals.
    pub fn from_text(text: impl Into<String>) -> Self {
        RetrievalQuery {
            text: text.into(),
            changed_symbols: Vec::new(),
            changed_paths: Vec::new(),
        }
    }

    /// Validate the query shape.
    pub fn validate(&self) -> Result<(), RetrievalError> {
        if self.text.len() > MAX_QUERY_BYTES {
            return Err(RetrievalError::InvalidQuery {
                reason: format!("query exceeds {MAX_QUERY_BYTES} bytes"),
            });
        }
        Ok(())
    }

    /// Bounded, deduplicated, lowercased search tokens of the query
    /// text, in order of first appearance.
    pub fn tokens(&self) -> Vec<String> {
        tokenize(&self.text)
    }

    /// The bounded set of names used for exact symbol lookups: the
    /// changed symbols plus the first identifier-like query tokens.
    pub fn symbol_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let push = |value: String, names: &mut Vec<String>| {
            if !value.is_empty() && !names.contains(&value) {
                names.push(value);
            }
        };
        for symbol in &self.changed_symbols {
            push(symbol.clone(), &mut names);
        }
        for token in self.tokens().into_iter().take(MAX_SYMBOL_TOKENS) {
            push(token, &mut names);
        }
        names.truncate(MAX_ENTRIES_PER_SIGNAL);
        names
    }
}

/// Split text into bounded, deduplicated, lowercased search tokens.
///
/// Only alphanumeric and underscore runs qualify; everything else is a
/// separator. Because tokens can never contain FTS5 syntax characters
/// like quotes, parentheses, or colons, they can be embedded in match
/// expressions safely.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for token in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if token.is_empty() {
            continue;
        }
        let token = token.to_lowercase();
        if !tokens.contains(&token) {
            tokens.push(token);
            if tokens.len() == MAX_QUERY_TOKENS {
                break;
            }
        }
    }
    tokens
}

/// Build a safe FTS5 match expression from tokens: every token becomes
/// a quoted phrase, phrases are OR-combined so any term can match, and
/// ranking decides relevance.
///
/// Returns `None` when there is nothing to search for.
pub fn fts_match_expr(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let mut expr = String::new();
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            expr.push_str(" OR ");
        }
        expr.push('"');
        // Defensive: tokens cannot contain quotes by construction, but
        // strip any that could ever appear rather than trust it.
        expr.extend(token.chars().filter(|c| *c != '"'));
        expr.push('"');
    }
    Some(expr)
}

/// Explicit context budget bounding one bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Maximum number of entries in the bundle.
    pub max_entries: usize,
    /// Maximum text bytes of a single entry.
    pub max_entry_text_bytes: usize,
    /// Maximum total text bytes across the bundle.
    pub max_total_text_bytes: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        ContextBudget {
            max_entries: 32,
            max_entry_text_bytes: 2048,
            max_total_text_bytes: 64 * 1024,
        }
    }
}

/// Truncate a string to at most `max_bytes` on a char boundary.
pub fn truncate_text(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Deterministic bundle ordering: score descending, then kind priority,
/// then stable textual identity.
pub fn compare_entries(a: &ContextEntry, b: &ContextEntry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let by_score = b.score.total_cmp(&a.score);
    if by_score != Ordering::Equal {
        return by_score;
    }
    a.kind
        .priority()
        .cmp(&b.kind.priority())
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.symbol.cmp(&b.symbol))
        .then_with(|| a.line_start.cmp(&b.line_start))
        .then_with(|| a.title.cmp(&b.title))
        .then_with(|| a.record_id.cmp(&b.record_id))
        .then_with(|| a.practice_id.cmp(&b.practice_id))
}

/// Rank, truncate, and assemble candidate entries into a bundle.
///
/// Candidates may arrive in any order and may exceed the budget; the
/// result is always deterministically ordered and bounded, with
/// `truncated` set when candidates were dropped.
pub fn assemble_bundle(mut candidates: Vec<ContextEntry>, budget: &ContextBudget) -> ContextBundle {
    candidates.sort_by(compare_entries);

    let mut entries = Vec::new();
    let mut text_bytes = 0usize;
    let mut truncated = false;

    for candidate in candidates {
        if entries.len() >= budget.max_entries {
            truncated = true;
            break;
        }
        let text = truncate_text(&candidate.text, budget.max_entry_text_bytes).to_owned();
        if text_bytes + text.len() > budget.max_total_text_bytes {
            truncated = true;
            break;
        }
        text_bytes += text.len();
        entries.push(ContextEntry { text, ..candidate });
    }

    ContextBundle {
        entries,
        truncated,
        text_bytes,
    }
}

/// Machine-readable JSON report of one executed query.
#[derive(Debug, Serialize)]
struct QueryReport<'a> {
    query: &'a str,
    snapshot: &'a SnapshotDescriptor,
    truncated: bool,
    text_bytes: usize,
    entries: &'a [ContextEntry],
}

/// Render the JSON form of a query result (compact, single line).
pub fn render_json(
    query: &str,
    snapshot: &SnapshotDescriptor,
    bundle: &ContextBundle,
) -> Result<String, RetrievalError> {
    serde_json::to_string(&QueryReport {
        query,
        snapshot,
        truncated: bundle.truncated,
        text_bytes: bundle.text_bytes,
        entries: &bundle.entries,
    })
    .map_err(|err| RetrievalError::InvalidQuery {
        reason: format!("failed to serialize query result: {err}"),
    })
}

/// Render the human-readable form of a query result.
pub fn render_text(query: &str, snapshot: &SnapshotDescriptor, bundle: &ContextBundle) -> String {
    let mut out = String::new();
    out.push_str(&format!("nisse query \"{query}\"\n"));
    out.push_str(&format!("snapshot: {}\n", snapshot.mode_label()));
    out.push_str(&format!(
        "entries: {}{}\n",
        bundle.entries.len(),
        if bundle.truncated { " (truncated)" } else { "" }
    ));
    for (index, entry) in bundle.entries.iter().enumerate() {
        out.push_str(&format!("\n{}. [{}] ", index + 1, entry.kind.label()));
        match (&entry.path, entry.line_start) {
            (Some(path), Some(line_start)) => {
                let end = entry
                    .line_end
                    .map_or_else(|| line_start.to_string(), |end| end.to_string());
                out.push_str(&format!("{path}:{line_start}-{end}"));
            }
            (Some(path), None) => out.push_str(path),
            (None, _) => out.push_str(&entry.title),
        }
        out.push_str(&format!(" (score {:.2})\n", entry.score));
        if let Some(symbol) = &entry.symbol {
            out.push_str(&format!("   symbol: {symbol}\n"));
        }
        if let Some(record) = &entry.record_id {
            out.push_str(&format!("   record: {record}\n"));
        }
        if let Some(practice) = &entry.practice_id {
            out.push_str(&format!("   practice: {practice}\n"));
        }
        for line in entry.text.lines() {
            out.push_str(&format!("   {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::context::ContextKind;

    fn entry(kind: ContextKind, title: &str, path: Option<&str>, score: f64) -> ContextEntry {
        ContextEntry {
            kind,
            title: title.to_string(),
            path: path.map(str::to_string),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text: String::new(),
            score,
        }
    }

    #[test]
    fn tokenize_is_conservative_and_bounded() {
        assert_eq!(
            tokenize("Queue boundaries (ISR-safe)"),
            vec![
                "queue".to_string(),
                "boundaries".to_string(),
                "isr".to_string(),
                "safe".to_string()
            ]
        );
        // FTS syntax characters can never enter a token.
        assert_eq!(
            tokenize("a\"b(c):d^e"),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string()
            ]
        );
        // Underscore identifiers stay intact.
        assert_eq!(tokenize("parse_request"), vec!["parse_request".to_string()]);
        // Case-folded, deduplicated.
        assert_eq!(tokenize("Alpha alpha ALPHA"), vec!["alpha".to_string()]);
        // Nothing usable.
        assert!(tokenize("!!! ???").is_empty());
        // Bounded to MAX_QUERY_TOKENS.
        let many = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(tokenize(&many).len(), MAX_QUERY_TOKENS);
    }

    #[test]
    fn fts_match_expr_quotes_every_token() {
        let expr = fts_match_expr(&["queue".to_string(), "boundaries".to_string()]).unwrap();
        assert_eq!(expr, "\"queue\" OR \"boundaries\"");
        assert_eq!(fts_match_expr(&[]), None);
        // Even adversarial tokens lose their quotes.
        let expr = fts_match_expr(&["a\" OR 1=1 --".to_string()]).unwrap();
        assert_eq!(expr, "\"a OR 1=1 --\"");
    }

    #[test]
    fn query_validation_rejects_oversized_text() {
        let big = "x".repeat(MAX_QUERY_BYTES + 1);
        let query = RetrievalQuery::from_text(big);
        assert!(query.validate().is_err());
        assert!(RetrievalQuery::from_text("fine").validate().is_ok());
    }

    #[test]
    fn symbol_names_prefer_changed_symbols_and_deduplicate() {
        let query = RetrievalQuery {
            text: "parse_request parse_request queue".to_string(),
            changed_symbols: vec!["enqueue".to_string()],
            changed_paths: Vec::new(),
        };
        assert_eq!(
            query.symbol_names(),
            vec![
                "enqueue".to_string(),
                "parse_request".to_string(),
                "queue".to_string()
            ]
        );
    }

    #[test]
    fn budget_truncates_entry_text_on_char_boundaries() {
        assert_eq!(truncate_text("abc", 10), "abc");
        assert_eq!(truncate_text("abcdef", 3), "abc");
        // Multibyte characters are never split.
        let text = "åäö";
        assert_eq!(truncate_text(text, 2), "å");
        assert_eq!(truncate_text(text, 5), "åä");
    }

    #[test]
    fn assembly_orders_and_truncates_deterministically() {
        let budget = ContextBudget {
            max_entries: 3,
            max_entry_text_bytes: 10,
            max_total_text_bytes: 25,
        };
        let candidates = vec![
            entry(ContextKind::Practice, "p", None, 0.5),
            entry(ContextKind::SymbolDefinition, "b", Some("src/b.rs"), 1.0),
            entry(ContextKind::SymbolDefinition, "a", Some("src/a.rs"), 1.0),
            entry(ContextKind::SourceSkeleton, "skel", Some("src/c.rs"), 0.9),
        ];
        let bundle = assemble_bundle(
            candidates
                .into_iter()
                .map(|mut e| {
                    e.text = "0123456789".to_string();
                    e
                })
                .collect(),
            &budget,
        );
        // Equal scores tie-break on path; two 10-byte entries fit the
        // 25-byte total, the third would not.
        assert_eq!(bundle.entries.len(), 2);
        assert!(bundle.truncated);
        assert_eq!(bundle.entries[0].path.as_deref(), Some("src/a.rs"));
        assert_eq!(bundle.entries[1].path.as_deref(), Some("src/b.rs"));
        assert_eq!(bundle.text_bytes, 20);
    }

    #[test]
    fn assembly_marks_truncation_from_entry_limit() {
        let budget = ContextBudget {
            max_entries: 1,
            max_entry_text_bytes: 100,
            max_total_text_bytes: 10_000,
        };
        let candidates = vec![
            entry(ContextKind::Practice, "a", None, 0.5),
            entry(ContextKind::Practice, "b", None, 0.4),
        ];
        let bundle = assemble_bundle(candidates, &budget);
        assert_eq!(bundle.entries.len(), 1);
        assert!(bundle.truncated);
    }

    #[test]
    fn assembly_exact_fit_is_not_truncated() {
        let budget = ContextBudget::default();
        let candidates = vec![entry(ContextKind::Practice, "a", None, 0.5)];
        let bundle = assemble_bundle(candidates, &budget);
        assert_eq!(bundle.entries.len(), 1);
        assert!(!bundle.truncated);
    }

    #[test]
    fn renders_are_deterministic_and_informative() {
        let snapshot = SnapshotDescriptor {
            mode: crate::domain::snapshot::SnapshotMode::Working,
            base_commit: None,
            base_tree: None,
            head_commit: None,
            head_tree: None,
        };
        let bundle = ContextBundle {
            entries: vec![ContextEntry {
                kind: ContextKind::SymbolDefinition,
                title: "function alpha".to_string(),
                path: Some("src/lib.rs".to_string()),
                symbol: Some("alpha".to_string()),
                line_start: Some(1),
                line_end: Some(3),
                record_id: None,
                practice_id: None,
                text: "fn alpha() L1-L3".to_string(),
                score: 1.25,
            }],
            truncated: false,
            text_bytes: 16,
        };

        let text = render_text("alpha", &snapshot, &bundle);
        assert!(text.contains("nisse query \"alpha\""));
        assert!(text.contains("snapshot: working"));
        assert!(text.contains("src/lib.rs:1-3"));
        assert!(text.contains("symbol: alpha"));
        assert!(text.contains("fn alpha() L1-L3"));

        let json = render_json("alpha", &snapshot, &bundle).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["query"], "alpha");
        assert_eq!(value["snapshot"]["mode"], "working");
        assert_eq!(value["entries"][0]["kind"], "symbol_definition");
        assert_eq!(value["entries"][0]["path"], "src/lib.rs");
        assert_eq!(value["truncated"], false);
    }
}

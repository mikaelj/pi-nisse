//! Typed retrieval failures.

use thiserror::Error;

/// Failures of baseline retrieval (plan section 30).
#[derive(Debug, Error)]
pub enum RetrievalError {
    /// A cache database operation failed.
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),

    /// The worktree index does not provably reflect the requested
    /// snapshot: the membership data (manifest/path content) was written
    /// for a different snapshot, or no refresh has happened yet.
    ///
    /// Cache presence never proves snapshot membership (plan section 26);
    /// retrieval refuses to guess. Refresh the index for the requested
    /// snapshot and retry.
    #[error(
        "worktree index does not match the requested snapshot (indexed: {found}, requested: {expected}); refresh the index first"
    )]
    SnapshotMismatch { found: String, expected: String },

    /// The query is structurally invalid (for example oversized).
    #[error("invalid retrieval query: {reason}")]
    InvalidQuery { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_mismatch_message_names_both_sides() {
        let err = RetrievalError::SnapshotMismatch {
            found: "none".to_string(),
            expected: "working".to_string(),
        };
        let message = err.to_string();
        assert!(message.contains("none"), "{message}");
        assert!(message.contains("working"), "{message}");
        assert!(message.contains("refresh"), "{message}");
    }
}

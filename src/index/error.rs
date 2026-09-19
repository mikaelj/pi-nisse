//! Typed index failures.

use thiserror::Error;

use super::language::Language;
use crate::git::GitError;
use crate::storage::StorageError;

/// Failures of index construction and refresh.
#[derive(Debug, Error)]
pub enum IndexError {
    /// A Git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A cache database operation failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A language grammar could not be initialized.
    #[error("failed to initialize {language} parser: {reason}")]
    ParserSetup {
        /// Language whose parser failed to load.
        language: Language,
        /// Underlying reason.
        reason: String,
    },
}

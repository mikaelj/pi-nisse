//! Snapshot descriptors.
//!
//! Every review resolves a target snapshot. All source retrieval must prove
//! membership in that snapshot; cache presence never implies membership.

use serde::{Deserialize, Serialize};

/// How a review snapshot was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotMode {
    /// Final working-tree state versus HEAD.
    Working,
    /// Index state versus HEAD.
    Staged,
    /// A commit versus its first parent.
    Commit,
    /// BASE tree versus HEAD tree.
    Range,
    /// Merge-base tree versus HEAD tree.
    Branch,
}

/// Describes the exact source snapshot a review ran against.
///
/// `None` fields are those that do not apply to the mode. For `working` and
/// `staged` modes the target side is defined by the worktree/index state
/// itself and therefore carries no Git object IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDescriptor {
    /// Mode that selected this snapshot.
    pub mode: SnapshotMode,
    /// Base commit OID, when applicable.
    pub base_commit: Option<String>,
    /// Base tree OID, when applicable.
    pub base_tree: Option<String>,
    /// Head commit OID, when applicable.
    pub head_commit: Option<String>,
    /// Head tree OID, when applicable.
    pub head_tree: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_mode_roundtrip() {
        for (mode, name) in [
            (SnapshotMode::Working, "\"working\""),
            (SnapshotMode::Staged, "\"staged\""),
            (SnapshotMode::Commit, "\"commit\""),
            (SnapshotMode::Range, "\"range\""),
            (SnapshotMode::Branch, "\"branch\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), name);
            let back: SnapshotMode = serde_json::from_str(name).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn snapshot_mode_rejects_unknown_variants() {
        assert!(serde_json::from_str::<SnapshotMode>("\"head\"").is_err());
        assert!(serde_json::from_str::<SnapshotMode>("\"Working\"").is_err());
    }

    #[test]
    fn snapshot_descriptor_roundtrip() {
        let descriptor = SnapshotDescriptor {
            mode: SnapshotMode::Commit,
            base_commit: Some("1111111111111111111111111111111111111111".to_string()),
            base_tree: Some("2222222222222222222222222222222222222222".to_string()),
            head_commit: Some("3333333333333333333333333333333333333333".to_string()),
            head_tree: Some("4444444444444444444444444444444444444444".to_string()),
        };
        let json = serde_json::to_string(&descriptor).unwrap();
        let back: SnapshotDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(descriptor, back);
        assert!(json.contains("\"mode\":\"commit\""));
        assert!(json.contains("\"head_tree\""));
    }
}

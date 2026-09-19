//! Built-in practice packs.
//!
//! Practice packs carry reusable domain knowledge that is bundled into
//! reviewer initial context by tag relevance. Project-specific knowledge
//! never lives here; it belongs in durable project records.

use serde::{Deserialize, Serialize};

/// A single reusable practice statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Practice {
    /// Globally unique practice identifier (conventionally
    /// `<pack-id>:<slug>`).
    pub id: String,
    /// Short human-readable title.
    pub title: String,
    /// The reusable domain knowledge statement.
    pub statement: String,
}

/// A built-in practice pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticePack {
    /// Stable pack identifier (lowercase kebab-case).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// What domain the pack covers.
    pub description: String,
    /// Tags used by routing and context selection. Must be nonempty; the
    /// pack id itself is conventionally the primary tag.
    pub tags: Vec<String>,
    /// The practice statements in this pack.
    pub practices: Vec<Practice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pack() -> PracticePack {
        PracticePack {
            id: "concurrency".to_string(),
            title: "Concurrency".to_string(),
            description: "Concurrency domain knowledge.".to_string(),
            tags: vec!["concurrency".to_string()],
            practices: vec![Practice {
                id: "concurrency:owns-the-data".to_string(),
                title: "One owner per piece of mutable state".to_string(),
                statement: "Prefer message passing: one context owns each piece of mutable state."
                    .to_string(),
            }],
        }
    }

    #[test]
    fn practice_pack_roundtrip() {
        let pack = sample_pack();
        let json = serde_json::to_string(&pack).unwrap();
        let back: PracticePack = serde_json::from_str(&json).unwrap();
        assert_eq!(pack, back);
        assert!(json.contains("\"practices\""));
        assert!(json.contains("\"statement\""));
    }

    #[test]
    fn practice_pack_is_strict() {
        let pack = sample_pack();
        let json = serde_json::to_string(&pack).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        let mut extra = value.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("severity".to_string(), serde_json::json!("blocker"));
        assert!(
            serde_json::from_str::<PracticePack>(&serde_json::to_string(&extra).unwrap()).is_err()
        );

        for field in ["id", "title", "description", "tags", "practices"] {
            let mut pruned = value.clone();
            assert!(pruned.as_object_mut().unwrap().remove(field).is_some());
            let pruned_json = serde_json::to_string(&pruned).unwrap();
            assert!(
                serde_json::from_str::<PracticePack>(&pruned_json).is_err(),
                "expected rejection when `{field}` is missing"
            );
        }

        // Practice entries are strict too.
        let mut bad_practice = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        bad_practice["practices"][0]
            .as_object_mut()
            .unwrap()
            .insert("tags".to_string(), serde_json::json!(["x"]));
        assert!(
            serde_json::from_str::<PracticePack>(&serde_json::to_string(&bad_practice).unwrap())
                .is_err()
        );
    }
}

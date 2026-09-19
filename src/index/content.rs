//! Content keys for the source index (plan section 27).
//!
//! Every indexed file is identified by a content key that is a pure
//! function of its bytes, never of its path, branch, or worktree:
//!
//! - content that exists as a Git blob (index and commit-tree sides) uses
//!   `git:<blob-oid>`;
//! - dirty or untracked working-tree content uses `sha256:<hex>`, where
//!   the hex is the SHA-256 digest of the exact bytes read from the
//!   working tree.
//!
//! For working-tree files, the Git blob object ID is computed locally over
//! the exact bytes (`blob <len>\0` + bytes, hashed with the repository's
//! object format). If it equals the stage-0 index entry's blob OID the
//! file is clean and gets the `git:` key, so the same committed content
//! shares one artifact across branches and worktrees. If it differs (or
//! the path is untracked) the bytes get a `sha256:` key. Because keys are
//! computed over the exact bytes that are parsed, filter mismatches (for
//! example CRLF conversion) can only ever fall back to the `sha256:` key,
//! never produce a wrong reuse.

use sha1::{Digest as _, Sha1};
use sha2::Sha256;

use crate::git::ObjectFormat;

/// A content-addressed key for indexed file content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentKey {
    /// `git:<blob-oid>` — content that exists as a Git blob object.
    Git(String),
    /// `sha256:<hex>` — dirty or untracked working-tree content.
    Sha256(String),
}

impl ContentKey {
    /// Build the canonical string form used in storage.
    pub fn as_str(&self) -> String {
        match self {
            ContentKey::Git(oid) => format!("git:{oid}"),
            ContentKey::Sha256(hex) => format!("sha256:{hex}"),
        }
    }
}

impl std::fmt::Display for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Compute the Git blob object ID of `bytes` for a repository object
/// format.
///
/// The header is `blob <len>\0` followed by the exact bytes — identical to
/// what `git hash-object` computes — so clean working-tree files derive
/// the same object ID Git itself recorded in the index.
pub fn git_blob_oid(bytes: &[u8], format: ObjectFormat) -> String {
    let header = format!("blob {}\0", bytes.len());
    match format {
        ObjectFormat::Sha1 => {
            let mut hasher = Sha1::new();
            hasher.update(header);
            hasher.update(bytes);
            to_hex(&hasher.finalize())
        }
        ObjectFormat::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(header);
            hasher.update(bytes);
            to_hex(&hasher.finalize())
        }
    }
}

/// Build the `sha256:` content key of dirty or untracked content.
pub fn working_tree_key(bytes: &[u8]) -> ContentKey {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    ContentKey::Sha256(to_hex(&hasher.finalize()))
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_blob_oid_matches_git_hash_object() {
        let fixture = crate::git::testutil::Fixture::repo("content-hash");
        let content = b"fn main() {\n    println!(\"hello\");\n}\n";
        crate::git::testutil::write_file(&fixture.root, "a.rs", "");
        let path = fixture.root.join("a.rs");
        std::fs::write(&path, content).unwrap();
        let out = crate::git::testutil::git_in(&fixture.root, &["hash-object", "a.rs"]);

        let expected = crate::git::testutil::stdout_trimmed(out);
        assert_eq!(git_blob_oid(content, ObjectFormat::Sha1), expected);
    }

    #[test]
    fn blob_oid_covers_header_and_empty_content() {
        // Well-known empty-blob object IDs.
        assert_eq!(
            git_blob_oid(b"", ObjectFormat::Sha1),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            git_blob_oid(b"", ObjectFormat::Sha256),
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        );
        // Length is part of the header: same suffix, different lengths.
        assert_ne!(
            git_blob_oid(b"abc", ObjectFormat::Sha1),
            git_blob_oid(b"abcd", ObjectFormat::Sha1)
        );
    }

    #[test]
    fn working_tree_key_is_sha256_of_exact_bytes() {
        let key = working_tree_key(b"abc");
        let ContentKey::Sha256(hex) = &key else {
            panic!("expected sha256 key");
        };
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            key.as_str(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(working_tree_key(b"abd"), key);
    }

    #[test]
    fn key_string_forms_are_distinct_and_stable() {
        assert_eq!(
            ContentKey::Git("0123abcd".to_string()).as_str(),
            "git:0123abcd"
        );
        assert_eq!(
            ContentKey::Git("0123abcd".to_string()).to_string(),
            "git:0123abcd"
        );
        assert_eq!(
            ContentKey::Sha256("ff".to_string()).to_string(),
            "sha256:ff"
        );
    }
}

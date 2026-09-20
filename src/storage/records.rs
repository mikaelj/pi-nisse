//! Durable Git-tracked project record store (plan sections 39–45).
//!
//! Durable records are immutable JSON files inside the worktree, laid
//! out exactly as the plan freezes it:
//!
//! ```text
//! .nisse/
//!   people/<contributor-id>/<ULID>.json
//!   common/<ULID>.json
//! ```
//!
//! Key invariants enforced here:
//!
//! - **Immutable create**: a record file is created exactly once via an
//!   atomic exclusive write; it is never modified or overwritten. New
//!   knowledge creates new records (with new ULIDs) that may *supersede*
//!   old ones, which remain for provenance.
//! - **Records are independent of SQLite**: this store never touches the
//!   cache databases, and losing the caches never affects records
//!   (permanent invariant 4).
//! - **Merge-friendly**: every record lives at a path derived from its
//!   own identity, so records created on different branches or in
//!   different worktrees never collide and merge cleanly.
//! - **Strict loading**: every file must be a valid, validating record
//!   whose file name matches its ID, whose author matches its people
//!   directory, and whose IDs are unique across the whole tree. Invalid
//!   content is a hard error, not a silent skip; oversized files are
//!   rejected before they are read, so a hostile `.nisse` tree never
//!   causes unbounded reads.
//! - **Path safety**: contributor directories and record file names are
//!   derived only from validated ULIDs/contributor slugs; traversal
//!   values are rejected before any path is built. The `.nisse` tree is
//!   never traversed through symlinks in either direction: loading
//!   rejects a symlinked `.nisse` root, and creating records walks every
//!   directory component below the worktree root and refuses to
//!   traverse a symlink, so a pre-planted `.nisse/people/<id>` symlink
//!   can never redirect an immutable record outside the worktree.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::ids::is_valid_ulid;
use crate::domain::records::{ProjectRecord, RecordKind, is_valid_contributor_id};

use super::fsutil::{ExclusiveWrite, atomic_write_exclusive};

/// Name of the durable record tree at the worktree root.
pub const NISSE_RECORDS_DIR: &str = ".nisse";

/// Maximum accepted size of one durable record file.
///
/// Validated records are far smaller (every text field is bounded); the
/// bound exists so a hostile or accidentally huge `.nisse` tree fails
/// closed instead of causing unbounded reads.
pub const MAX_RECORD_FILE_BYTES: u64 = 1024 * 1024;

/// Where a record is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordLocation {
    /// A contributor record: `.nisse/people/<contributor-id>/`.
    People {
        /// Contributor directory name.
        contributor: String,
    },
    /// A consolidated shared record: `.nisse/common/`.
    Common,
}

impl RecordLocation {
    /// Human-readable label used in reports.
    pub fn label(&self) -> String {
        match self {
            RecordLocation::People { contributor } => format!("people/{contributor}"),
            RecordLocation::Common => "common".to_string(),
        }
    }
}

/// A record together with the location it was loaded from (or created
/// at).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRecord {
    /// The record payload.
    pub record: ProjectRecord,
    /// Where the record lives in the `.nisse` tree.
    pub location: RecordLocation,
    /// Absolute path of the record file.
    pub path: PathBuf,
}

impl StoredRecord {
    /// The record ID.
    pub fn id(&self) -> &str {
        self.record.id()
    }
}

/// Typed durable-record failures.
#[derive(Debug, Error)]
pub enum RecordStoreError {
    /// A record file already exists; records are immutable.
    #[error("record already exists at `{path}`; durable records are immutable")]
    RecordExists { path: PathBuf },
    /// A location was rejected before any filesystem path was built.
    #[error("invalid record location `{location}`: {reason}")]
    InvalidLocation { location: String, reason: String },
    /// A path below the worktree root resolves through a symlink or a
    /// non-directory. Record paths may only traverse real directories
    /// created by Nisse.
    #[error("unsafe record path `{path}`: {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    /// A record file or directory entry is invalid. Loading is strict:
    /// invalid content is a hard failure, never a silent skip.
    #[error("invalid record at `{path}`: {reason}")]
    InvalidRecord { path: PathBuf, reason: String },
    /// Observations are contributor records and cannot be consolidated
    /// into `.nisse/common` (plan sections 45 and 53).
    #[error("observations cannot be stored under `.nisse/common`; they remain contributor records")]
    ObservationNotAllowedInCommon,
    /// Record validation failed before any write was attempted.
    #[error("record validation failed: {0}")]
    Validation(#[from] crate::domain::records::RecordValidationError),
    /// The record could not be serialized.
    #[error("failed to serialize record: {0}")]
    Serialize(#[source] serde_json::Error),
    /// Filesystem access failed.
    #[error("record I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The durable record store of one worktree.
#[derive(Debug, Clone)]
pub struct RecordStore {
    worktree_root: PathBuf,
}

impl RecordStore {
    /// Open the record store rooted at `worktree_root`.
    ///
    /// The store never requires initialization: the `.nisse` tree is
    /// created on the first record and may be absent otherwise.
    pub fn new(worktree_root: &Path) -> Self {
        RecordStore {
            worktree_root: worktree_root.to_path_buf(),
        }
    }

    /// The worktree root this store operates on.
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// The `.nisse` directory of the worktree.
    pub fn nisse_dir(&self) -> PathBuf {
        self.worktree_root.join(NISSE_RECORDS_DIR)
    }

    /// The `.nisse/people` directory.
    pub fn people_dir(&self) -> PathBuf {
        self.nisse_dir().join("people")
    }

    /// The `.nisse/common` directory.
    pub fn common_dir(&self) -> PathBuf {
        self.nisse_dir().join("common")
    }

    /// Create every missing directory component of `dir` below the
    /// worktree root, refusing to traverse anything that is not a real
    /// directory.
    ///
    /// `fs::create_dir_all` alone is not acceptable here: it succeeds
    /// when an intermediate component already exists as a symlink to a
    /// directory, which would silently redirect durable records outside
    /// the worktree. Every component is therefore inspected with
    /// `symlink_metadata` (which does not follow links) and only plain
    /// directories are traversed; missing components are created with
    /// `create_dir`, which itself refuses to clobber an existing path.
    fn ensure_real_dir(&self, dir: &Path) -> Result<(), RecordStoreError> {
        let relative =
            dir.strip_prefix(&self.worktree_root)
                .map_err(|_| RecordStoreError::UnsafePath {
                    path: dir.to_owned(),
                    reason: "record directory must live below the worktree root".to_string(),
                })?;
        let mut current = self.worktree_root.clone();
        for component in relative.components() {
            if component.as_os_str().to_str().is_none() {
                return Err(RecordStoreError::UnsafePath {
                    path: dir.to_owned(),
                    reason: "record directory components must be valid UTF-8".to_string(),
                });
            }
            current.push(component.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(metadata) => {
                    let reason = if metadata.file_type().is_symlink() {
                        "refusing to traverse a symlinked directory under `.nisse`"
                    } else {
                        "expected a directory, found a file"
                    };
                    return Err(RecordStoreError::UnsafePath {
                        path: current,
                        reason: reason.to_string(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&current).map_err(|source| RecordStoreError::Io {
                        path: current.clone(),
                        source,
                    })?;
                }
                Err(source) => {
                    return Err(RecordStoreError::Io {
                        path: current,
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate a contributor slug for use as a path component.
    fn validate_contributor(contributor: &str) -> Result<(), RecordStoreError> {
        if is_valid_contributor_id(contributor) {
            Ok(())
        } else {
            Err(RecordStoreError::InvalidLocation {
                location: format!("people/{contributor}"),
                reason: "contributor ids must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` \
                     and never contain path separators"
                    .to_string(),
            })
        }
    }

    /// Directory of one location, with contributor slugs validated
    /// before any path is built.
    fn location_dir(&self, location: &RecordLocation) -> Result<PathBuf, RecordStoreError> {
        match location {
            RecordLocation::People { contributor } => {
                Self::validate_contributor(contributor)?;
                Ok(self.people_dir().join(contributor))
            }
            RecordLocation::Common => Ok(self.common_dir()),
        }
    }

    /// Create a contributor record immutably.
    ///
    /// The record is validated first, then written to
    /// `.nisse/people/<author>/<id>.json`. Writing an ID that already
    /// exists fails with [`RecordStoreError::RecordExists`]; existing
    /// record files are never modified.
    pub fn create(&self, record: &ProjectRecord) -> Result<StoredRecord, RecordStoreError> {
        let location = RecordLocation::People {
            contributor: record.author().to_string(),
        };
        self.create_at(record, location)
    }

    /// Create a consolidated record under `.nisse/common`.
    ///
    /// Only facts and lessons may be consolidated; observations remain
    /// contributor records.
    pub fn create_common(&self, record: &ProjectRecord) -> Result<StoredRecord, RecordStoreError> {
        if matches!(record.kind(), RecordKind::Observation) {
            return Err(RecordStoreError::ObservationNotAllowedInCommon);
        }
        self.create_at(record, RecordLocation::Common)
    }

    /// Shared implementation of immutable record creation.
    fn create_at(
        &self,
        record: &ProjectRecord,
        location: RecordLocation,
    ) -> Result<StoredRecord, RecordStoreError> {
        record.validate()?;
        let dir = self.location_dir(&location)?;
        self.ensure_real_dir(&dir)?;
        let path = dir.join(format!("{}.json", record.id()));
        let mut bytes = serde_json::to_vec_pretty(record).map_err(RecordStoreError::Serialize)?;
        bytes.push(b'\n');
        match atomic_write_exclusive(&path, &bytes).map_err(|source| RecordStoreError::Io {
            path: path.clone(),
            source,
        })? {
            ExclusiveWrite::Created => Ok(StoredRecord {
                record: record.clone(),
                location,
                path,
            }),
            ExclusiveWrite::Exists => Err(RecordStoreError::RecordExists { path }),
        }
    }

    /// Load every durable record of this worktree, sorted by path.
    ///
    /// Loading is strict: malformed files, invalid schemas, ID/author
    /// mismatches, invalid directory names, and duplicate IDs are hard
    /// errors. An absent `.nisse` tree is an empty record set.
    pub fn load(&self) -> Result<Vec<StoredRecord>, RecordStoreError> {
        let root = self.nisse_dir();
        let root_metadata = match std::fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(source) => {
                return Err(RecordStoreError::Io { path: root, source });
            }
        };
        if !root_metadata.file_type().is_dir() {
            return Err(RecordStoreError::InvalidRecord {
                path: root,
                reason: "`.nisse` must be a real directory (symlinks are rejected)".to_string(),
            });
        }

        let mut stored = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        for entry in sorted_entries(&root, &root)? {
            let name = entry_name(&entry, &root)?;
            if name.starts_with('.') {
                continue;
            }
            let dir_path = root.join(&name);
            match name.as_str() {
                "people" | "common" => {}
                other => {
                    return Err(RecordStoreError::InvalidRecord {
                        path: root.join(other),
                        reason: format!(
                            "unexpected entry `{other}`; only `people/` and `common/` exist \
                             under `.nisse`"
                        ),
                    });
                }
            }
            if !entry
                .file_type()
                .map_err(|source| RecordStoreError::Io {
                    path: dir_path.clone(),
                    source,
                })?
                .is_dir()
            {
                return Err(RecordStoreError::InvalidRecord {
                    path: dir_path,
                    reason: "expected a directory".to_string(),
                });
            }

            match name.as_str() {
                "people" => self.load_people(&dir_path, &mut stored, &mut seen_ids)?,
                "common" => Self::load_common(&dir_path, &mut stored, &mut seen_ids)?,
                _ => unreachable!("names matched above"),
            }
        }

        stored.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stored)
    }

    /// Load contributor records from `.nisse/people/<id>/`.
    fn load_people(
        &self,
        people_root: &Path,
        stored: &mut Vec<StoredRecord>,
        seen_ids: &mut HashSet<String>,
    ) -> Result<(), RecordStoreError> {
        for entry in sorted_entries(people_root, people_root)? {
            let contributor = entry_name(&entry, people_root)?;
            if contributor.starts_with('.') {
                continue;
            }
            Self::validate_contributor(&contributor).map_err(|err| match err {
                RecordStoreError::InvalidLocation { location, reason } => {
                    RecordStoreError::InvalidRecord {
                        path: people_root.join(&contributor),
                        reason: format!(
                            "invalid contributor directory name ({reason}: {location})"
                        ),
                    }
                }
                other => other,
            })?;
            let contributor_dir = people_root.join(&contributor);
            if !entry
                .file_type()
                .map_err(|source| RecordStoreError::Io {
                    path: contributor_dir.clone(),
                    source,
                })?
                .is_dir()
            {
                return Err(RecordStoreError::InvalidRecord {
                    path: contributor_dir,
                    reason: "contributor entries under `people/` must be directories".to_string(),
                });
            }
            Self::load_record_files(
                &contributor_dir,
                &RecordLocation::People {
                    contributor: contributor.clone(),
                },
                Some(&contributor),
                stored,
                seen_ids,
            )?;
        }
        Ok(())
    }

    /// Load consolidated records from `.nisse/common/`.
    fn load_common(
        common_dir: &Path,
        stored: &mut Vec<StoredRecord>,
        seen_ids: &mut HashSet<String>,
    ) -> Result<(), RecordStoreError> {
        Self::load_record_files(common_dir, &RecordLocation::Common, None, stored, seen_ids)
    }

    /// Load `<ULID>.json` files from one record directory.
    ///
    /// `expected_author` enforces that people records carry their own
    /// directory as author.
    fn load_record_files(
        dir: &Path,
        location: &RecordLocation,
        expected_author: Option<&str>,
        stored: &mut Vec<StoredRecord>,
        seen_ids: &mut HashSet<String>,
    ) -> Result<(), RecordStoreError> {
        for entry in sorted_entries(dir, dir)? {
            let file_name = entry_name(&entry, dir)?;
            if file_name.starts_with('.') {
                // Dot-prefixed entries (temporary files, editor and OS
                // droppings) are ignored.
                continue;
            }
            let path = dir.join(&file_name);
            let Some(id) = file_name.strip_suffix(".json") else {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: "record file names must end in `.json`".to_string(),
                });
            };
            if !is_valid_ulid(id) {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: "record file names must be canonical ULIDs".to_string(),
                });
            }
            if !entry
                .file_type()
                .map_err(|source| RecordStoreError::Io {
                    path: path.clone(),
                    source,
                })?
                .is_file()
            {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: "record entries must be regular files (symlinks are rejected)"
                        .to_string(),
                });
            }

            let bytes = read_bounded(&path)?;
            let record: ProjectRecord =
                serde_json::from_slice(&bytes).map_err(|err| RecordStoreError::InvalidRecord {
                    path: path.clone(),
                    reason: err.to_string(),
                })?;
            record
                .validate()
                .map_err(|err| RecordStoreError::InvalidRecord {
                    path: path.clone(),
                    reason: err.to_string(),
                })?;
            if record.id() != id {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: format!(
                        "record id `{}` does not match its file name `{id}`",
                        record.id()
                    ),
                });
            }
            if let Some(expected) = expected_author
                && record.author() != expected
            {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: format!(
                        "record author `{}` does not match its contributor directory `{expected}`",
                        record.author()
                    ),
                });
            }
            if !seen_ids.insert(record.id().to_string()) {
                return Err(RecordStoreError::InvalidRecord {
                    path,
                    reason: format!(
                        "duplicate record id `{}`; record IDs are unique across the whole \
                         `.nisse` tree",
                        record.id()
                    ),
                });
            }
            stored.push(StoredRecord {
                record,
                location: location.clone(),
                path,
            });
        }
        Ok(())
    }
}

/// IDs of records superseded by any record in `records`.
///
/// A superseded record remains stored for provenance; callers use this
/// set to mark or filter active knowledge.
pub fn superseded_ids(records: &[StoredRecord]) -> HashSet<String> {
    let mut superseded = HashSet::new();
    for record in records {
        superseded.extend(record.record.supersedes().iter().cloned());
    }
    superseded
}

/// Records that no other record supersedes.
pub fn active_records(records: &[StoredRecord]) -> Vec<&StoredRecord> {
    let superseded = superseded_ids(records);
    records
        .iter()
        .filter(|stored| !superseded.contains(stored.id()))
        .collect()
}

/// Read a record file after enforcing regular-file and size bounds.
///
/// The bounds are checked before reading so an oversized (possibly
/// hostile) file never enters memory.
fn read_bounded(path: &Path) -> Result<Vec<u8>, RecordStoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| RecordStoreError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(RecordStoreError::InvalidRecord {
            path: path.to_owned(),
            reason: "record entries must be regular files (symlinks are rejected)".to_string(),
        });
    }
    if metadata.len() > MAX_RECORD_FILE_BYTES {
        return Err(RecordStoreError::InvalidRecord {
            path: path.to_owned(),
            reason: format!("record file exceeds the {MAX_RECORD_FILE_BYTES} byte bound"),
        });
    }
    std::fs::read(path).map_err(|source| RecordStoreError::Io {
        path: path.to_owned(),
        source,
    })
}

/// Read a directory's entries sorted by file name for deterministic
/// processing.
fn sorted_entries(dir: &Path, blame: &Path) -> Result<Vec<std::fs::DirEntry>, RecordStoreError> {
    let io = |source: std::io::Error| RecordStoreError::Io {
        path: blame.to_path_buf(),
        source,
    };
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<Result<_, _>>()
        .map_err(io)?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

/// The UTF-8 file name of a directory entry.
fn entry_name(entry: &std::fs::DirEntry, blame: &Path) -> Result<String, RecordStoreError> {
    entry
        .file_name()
        .into_string()
        .map_err(|name| RecordStoreError::InvalidRecord {
            path: blame.join(name),
            reason: "file names under `.nisse` must be valid UTF-8".to_string(),
        })
}

/// Render the human-readable `nisse records list` report.
pub fn render_listing(root: &Path, records: &[StoredRecord]) -> String {
    let superseded = superseded_ids(records);
    let superseded_count = records
        .iter()
        .filter(|stored| superseded.contains(stored.id()))
        .count();

    let mut out = String::from("nisse records list\n");
    out.push_str(&format!("records: {}\n", root.display()));
    for stored in records {
        let summary = match &stored.record {
            ProjectRecord::Observation(record) => format!(
                "{} {} {}",
                record.observation_type, record.outcome, record.category
            ),
            ProjectRecord::Fact(record) => {
                format!("{} {}", record.state, record.subject)
            }
            ProjectRecord::Lesson(record) => {
                format!("{} {}", record.routing_hint.reviewer_id, record.subject)
            }
        };
        let marker = if superseded.contains(stored.id()) {
            " [superseded]"
        } else {
            ""
        };
        out.push_str(&format!(
            "{:<11} {} {} {} {}{}\n",
            stored.record.kind().as_str(),
            stored.id(),
            stored.location.label(),
            stored.record.created_at(),
            summary,
            marker
        ));
    }
    out.push_str(&format!(
        "{} records ({} superseded)\n",
        records.len(),
        superseded_count
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::records::RecordValidationError;

    // Reuse the record builders shared with the domain tests.
    use crate::domain::records::testutil::{
        fact as build_fact, lesson as build_lesson, observation as build_observation,
    };

    fn new_store(dir: &Path) -> RecordStore {
        RecordStore::new(dir)
    }

    fn observation(author: &str) -> ProjectRecord {
        ProjectRecord::Observation(build_observation(author))
    }

    fn fact(author: &str) -> ProjectRecord {
        ProjectRecord::Fact(build_fact(author))
    }

    fn lesson(author: &str) -> ProjectRecord {
        ProjectRecord::Lesson(build_lesson(author))
    }

    #[test]
    fn create_writes_each_kind_to_its_frozen_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let observation = store.create(&observation("alice")).unwrap();
        assert_eq!(
            observation.path,
            dir.path()
                .join(".nisse")
                .join("people")
                .join("alice")
                .join(format!("{}.json", observation.id()))
        );

        let fact = store.create(&fact("alice")).unwrap();
        assert_eq!(
            fact.path,
            dir.path()
                .join(".nisse")
                .join("people")
                .join("alice")
                .join(format!("{}.json", fact.id()))
        );

        let lesson = store.create(&lesson("bob")).unwrap();
        assert_eq!(
            lesson.path,
            dir.path()
                .join(".nisse")
                .join("people")
                .join("bob")
                .join(format!("{}.json", lesson.id()))
        );

        // Files hold pretty-printed JSON ending in a newline.
        let bytes = std::fs::read(&observation.path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"kind\": \"observation\""));

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 3);
    }

    #[test]
    fn create_common_places_facts_and_lessons_but_rejects_observations() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let fact = store.create_common(&fact("alice")).unwrap();
        assert_eq!(fact.location, RecordLocation::Common);
        assert_eq!(
            fact.path,
            dir.path()
                .join(".nisse")
                .join("common")
                .join(format!("{}.json", fact.id()))
        );
        store.create_common(&lesson("alice")).unwrap();

        let err = store.create_common(&observation("alice")).unwrap_err();
        assert!(matches!(
            err,
            RecordStoreError::ObservationNotAllowedInCommon
        ));
        assert!(
            !dir.path().join(".nisse").join("people").exists(),
            "the rejected observation must not be written"
        );
    }

    #[test]
    fn create_is_immutable_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let created = store.create(&fact("alice")).unwrap();
        let before = std::fs::read(&created.path).unwrap();

        // Creating the same record again fails and leaves the file
        // byte-identical.
        let err = store.create(&created.record).unwrap_err();
        assert!(matches!(err, RecordStoreError::RecordExists { .. }));
        assert_eq!(std::fs::read(&created.path).unwrap(), before);
    }

    #[test]
    fn create_validates_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let mut invalid = build_observation("alice");
        invalid.schema_version = 7;
        let err = store
            .create(&ProjectRecord::Observation(invalid))
            .unwrap_err();
        assert!(matches!(
            err,
            RecordStoreError::Validation(RecordValidationError::UnsupportedSchemaVersion {
                found: 7,
                ..
            })
        ));
        assert!(!dir.path().join(".nisse").exists());
    }

    #[test]
    fn load_of_absent_nisse_tree_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(new_store(dir.path()).load().unwrap().is_empty());
    }

    #[test]
    fn load_roundtrips_created_records_with_locations() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        store.create(&observation("alice")).unwrap();
        store.create(&fact("alice")).unwrap();
        store.create_common(&lesson("alice")).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 3);
        for stored in &loaded {
            match &stored.record {
                ProjectRecord::Observation(_) => {
                    assert_eq!(stored.location.label(), "people/alice");
                }
                ProjectRecord::Fact(_) => {
                    assert_eq!(stored.location.label(), "people/alice");
                }
                ProjectRecord::Lesson(_) => {
                    assert_eq!(stored.location.label(), "common");
                }
            }
        }
    }

    #[test]
    fn load_rejects_invalid_json_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        store.create(&fact("alice")).unwrap();

        let bad = dir
            .path()
            .join(".nisse")
            .join("people")
            .join("alice")
            .join("01ARZ3NDEKTSV4RRFFQ69G5FAV.json");
        std::fs::write(&bad, b"{ not json").unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, RecordStoreError::InvalidRecord { ref path, .. } if path == &bad));

        // Unknown record kinds are rejected.
        std::fs::write(&bad, br#"{"kind":"note"}"#).unwrap();
        assert!(matches!(
            store.load().unwrap_err(),
            RecordStoreError::InvalidRecord { .. }
        ));

        // Structurally valid JSON that fails schema validation is
        // rejected.
        let mut record = build_fact("alice");
        record.schema_version = 99;
        std::fs::write(
            &bad,
            serde_json::to_vec(&ProjectRecord::Fact(record)).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load().unwrap_err(),
            RecordStoreError::InvalidRecord { .. }
        ));
    }

    #[test]
    fn load_rejects_filename_id_and_author_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        // File name does not match the record id.
        let record = build_observation("alice");
        let mismatched = dir
            .path()
            .join(".nisse")
            .join("people")
            .join("alice")
            .join("01ARZ3NDEKTSV4RRFFQ69G5FAV.json");
        std::fs::create_dir_all(mismatched.parent().unwrap()).unwrap();
        std::fs::write(
            &mismatched,
            serde_json::to_vec(&ProjectRecord::Observation(record)).unwrap(),
        )
        .unwrap();
        let err = store.load().unwrap_err();
        assert!(
            err.to_string().contains("does not match its file name"),
            "{err}"
        );

        // Author does not match the contributor directory.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = new_store(dir2.path());
        let record = build_observation("bob");
        let stray = dir2
            .path()
            .join(".nisse")
            .join("people")
            .join("alice")
            .join(format!("{}.json", record.id));
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(
            &stray,
            serde_json::to_vec(&ProjectRecord::Observation(record)).unwrap(),
        )
        .unwrap();
        let err = store2.load().unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match its contributor directory"),
            "{err}"
        );
    }

    #[test]
    fn load_rejects_non_ulid_filenames_and_stray_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        let people = dir.path().join(".nisse").join("people").join("alice");
        std::fs::create_dir_all(&people).unwrap();

        std::fs::write(people.join("not-a-ulid.json"), b"{}").unwrap();
        let err = store.load().unwrap_err();
        assert!(err.to_string().contains("must be canonical ULIDs"), "{err}");

        std::fs::remove_file(people.join("not-a-ulid.json")).unwrap();
        std::fs::write(people.join("notes.txt"), b"").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("`.json`"));
    }

    #[test]
    fn load_rejects_invalid_contributor_directories_and_stray_roots() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        // Traversal-style contributor directory names are rejected.
        let evil = dir.path().join(".nisse").join("people").join("-dash");
        std::fs::create_dir_all(&evil).unwrap();
        let err = store.load().unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid contributor directory name"),
            "{err}"
        );

        let dir2 = tempfile::tempdir().unwrap();
        let store2 = new_store(dir2.path());
        let stray = dir2.path().join(".nisse").join("random.json");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, b"{}").unwrap();
        assert!(
            store2
                .load()
                .unwrap_err()
                .to_string()
                .contains("unexpected entry")
        );
    }

    #[test]
    fn load_ignores_dot_prefixed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        store.create(&fact("alice")).unwrap();

        let root = dir.path().join(".nisse");
        std::fs::write(root.join(".DS_Store"), b"junk").unwrap();
        let people = root.join("people").join("alice");
        std::fs::write(
            people.join(format!(".nisse-tmp-{}", crate::domain::ids::new_ulid())),
            b"interrupted write",
        )
        .unwrap();

        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn load_rejects_duplicate_ids_across_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        // A record legitimately created under `people/`, then illicitly
        // copied to `common/` under the same id.
        let created = store.create(&fact("alice")).unwrap();
        let common_copy = dir
            .path()
            .join(".nisse")
            .join("common")
            .join(format!("{}.json", created.id()));
        std::fs::create_dir_all(common_copy.parent().unwrap()).unwrap();
        std::fs::copy(&created.path, &common_copy).unwrap();

        let err = store.load().unwrap_err();
        assert!(err.to_string().contains("duplicate record id"), "{err}");
    }

    #[test]
    fn location_paths_reject_traversal_contributors() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        for evil in ["../evil", "a/b", "", "..", ".hidden", "-dash"] {
            let err = store
                .location_dir(&RecordLocation::People {
                    contributor: evil.to_string(),
                })
                .unwrap_err();
            assert!(
                matches!(err, RecordStoreError::InvalidLocation { .. }),
                "`{evil}` must be rejected as a location"
            );
            assert!(
                !dir.path().join(".nisse").join("people").join(evil).exists(),
                "`{evil}` must never be used as a path"
            );
        }
    }

    #[test]
    fn create_rejects_traversal_authors_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        // The record itself is invalid (author must be a slug), so
        // validation rejects it before any path is built.
        let err = store.create(&observation("../evil")).unwrap_err();
        assert!(matches!(err, RecordStoreError::Validation(_)));
        assert!(!dir.path().join(".nisse").exists());
    }

    #[test]
    fn load_rejects_symlinked_record_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        store.create(&fact("alice")).unwrap();

        let people = dir.path().join(".nisse").join("people").join("alice");
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, b"{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &outside,
            people.join(format!("{}.json", crate::domain::ids::new_ulid())),
        )
        .unwrap();

        let result = store.load();
        #[cfg(unix)]
        {
            let err = result.unwrap_err();
            assert!(err.to_string().contains("regular files"), "{err}");
        }
        #[cfg(not(unix))]
        {
            assert_eq!(result.unwrap().len(), 1);
        }
    }

    #[test]
    fn load_rejects_oversized_record_files_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        let people = dir.path().join(".nisse").join("people").join("alice");
        std::fs::create_dir_all(&people).unwrap();

        // A hostile record file whose *declared* size already exceeds the
        // bound: the store must fail before reading any content.
        let huge = people.join(format!("{}.json", crate::domain::ids::new_ulid()));
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(MAX_RECORD_FILE_BYTES + 1).unwrap();
        drop(file);

        let err = store.load().unwrap_err();
        assert!(
            err.to_string().contains("byte bound"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_rejects_invalid_utf8_record_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        let people = dir.path().join(".nisse").join("people").join("alice");
        std::fs::create_dir_all(&people).unwrap();

        // Raw invalid UTF-8 bytes: JSON parsing fails closed.
        std::fs::write(
            people.join(format!("{}.json", crate::domain::ids::new_ulid())),
            b"{\"kind\":\"\xff\xfe\"}",
        )
        .unwrap();
        let err = store.load().unwrap_err();
        assert!(
            matches!(err, RecordStoreError::InvalidRecord { .. }),
            "{err}"
        );
    }

    #[test]
    fn load_rejects_symlinked_nisse_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());
        store.create(&fact("alice")).unwrap();

        // Move the real tree away and symlink `.nisse` to a directory
        // outside the worktree: loading must refuse to follow it.
        let real = dir.path().join(".nisse");
        std::fs::rename(&real, outside.path().join("nisse-real")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("nisse-real"), &real).unwrap();

        #[cfg(unix)]
        {
            let err = store.load().unwrap_err();
            assert!(err.to_string().contains("symlinks are rejected"), "{err}");
        }
        #[cfg(not(unix))]
        {
            // Without symlink support the moved-away tree simply loads empty.
            assert!(store.load().unwrap().is_empty());
        }
    }

    #[test]
    fn create_refuses_to_traverse_symlinked_directory_components() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        // Case 1: the contributor directory is a symlink out of the tree.
        let people = dir.path().join(".nisse").join("people");
        std::fs::create_dir_all(&people).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), people.join("alice")).unwrap();
        #[cfg(unix)]
        {
            let err = new_store(dir.path()).create(&fact("alice")).unwrap_err();
            assert!(
                matches!(err, RecordStoreError::UnsafePath { .. }),
                "unexpected error: {err}"
            );
            assert!(err.to_string().contains("symlinked directory"), "{err}");
            // Nothing escaped into the symlink target.
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }

        // Case 2: the `.nisse` root itself is a symlink.
        let dir2 = tempfile::tempdir().unwrap();
        let outside2 = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside2.path(), dir2.path().join(".nisse")).unwrap();
        #[cfg(unix)]
        {
            let err = new_store(dir2.path())
                .create(&observation("bob"))
                .unwrap_err();
            assert!(matches!(err, RecordStoreError::UnsafePath { .. }), "{err}");
            assert_eq!(std::fs::read_dir(outside2.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn create_rejects_non_directory_components() {
        let dir = tempfile::tempdir().unwrap();
        let nisse = dir.path().join(".nisse");
        std::fs::create_dir_all(&nisse).unwrap();
        // `.nisse` is fine, but `people` exists as a plain file.
        std::fs::write(nisse.join("people"), b"not a directory").unwrap();

        let err = new_store(dir.path()).create(&fact("alice")).unwrap_err();
        assert!(matches!(err, RecordStoreError::UnsafePath { .. }), "{err}");
    }

    #[test]
    fn superseded_and_active_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let original = store.create(&fact("alice")).unwrap();
        let mut updated = build_fact("alice");
        updated.supersedes = vec![original.id().to_string()];
        let updated = store.create(&ProjectRecord::Fact(updated)).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 2);
        let superseded = superseded_ids(&loaded);
        assert_eq!(superseded, HashSet::from([original.id().to_string()]));
        let active = active_records(&loaded);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id(), updated.id());
    }

    #[test]
    fn render_listing_shows_each_kind_and_superseded_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = new_store(dir.path());

        let original = store.create(&fact("alice")).unwrap();
        let mut updated = build_fact("alice");
        updated.supersedes = vec![original.id().to_string()];
        store.create(&ProjectRecord::Fact(updated)).unwrap();
        store.create(&observation("alice")).unwrap();
        store.create_common(&lesson("carol")).unwrap();

        let loaded = store.load().unwrap();
        let text = render_listing(&store.nisse_dir(), &loaded);
        assert!(text.starts_with("nisse records list\n"));
        assert!(text.contains("fact"));
        assert!(text.contains("observation"));
        assert!(text.contains("lesson"));
        assert!(text.contains("people/alice"));
        assert!(text.contains("common"));
        assert!(text.contains("supported queue discipline"));
        assert!(text.contains("[superseded]"));
        assert!(text.ends_with("4 records (1 superseded)\n"));
    }

    #[test]
    fn records_from_two_branches_merge_cleanly() {
        use crate::git::testutil::{Fixture, commit_all, git_in, write_file};

        let fixture = Fixture::repo("records-merge");
        write_file(&fixture.root, "README.md", "readme\n");
        commit_all(&fixture.root, "base");

        // Branch A: alice records an observation.
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "feature-a"]);
        let store = RecordStore::new(&fixture.root);
        let observation = store.create(&observation("alice")).unwrap();
        git_in(&fixture.root, &["add", ".nisse"]);
        git_in(&fixture.root, &["commit", "--quiet", "-m", "records-a"]);

        // Branch B (from the same base): bob records a fact.
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        let fact = store.create(&fact("bob")).unwrap();
        git_in(&fixture.root, &["add", ".nisse"]);
        git_in(&fixture.root, &["commit", "--quiet", "-m", "records-b"]);

        // A normal Git merge unifies both record trees without
        // conflicts: every record has its own path.
        git_in(
            &fixture.root,
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "-m",
                "merge records",
                "feature-a",
            ],
        );

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 2, "both branch records survive the merge");
        assert!(loaded.iter().any(|stored| stored.id() == observation.id()));
        assert!(loaded.iter().any(|stored| stored.id() == fact.id()));
    }

    #[test]
    fn two_worktrees_create_independent_records() {
        use crate::git::testutil::{Fixture, commit_all, git_in, write_file};

        let fixture = Fixture::repo("records-worktrees");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let wt_path = fixture
            .sibling("wt")
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "-b", "topic"],
        );
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();

        let main_store = RecordStore::new(&fixture.root);
        let wt_store = RecordStore::new(&wt_root);

        let main_record = main_store.create(&observation("alice")).unwrap();
        let wt_record = wt_store.create(&fact("bob")).unwrap();

        // Each worktree's `.nisse` tree is its own checkout; records do
        // not leak between worktrees.
        let main_loaded = main_store.load().unwrap();
        assert_eq!(main_loaded.len(), 1);
        assert_eq!(main_loaded[0].id(), main_record.id());

        let wt_loaded = wt_store.load().unwrap();
        assert_eq!(wt_loaded.len(), 1);
        assert_eq!(wt_loaded[0].id(), wt_record.id());
    }
}

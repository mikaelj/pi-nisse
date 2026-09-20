//! On-demand population of the record and practice full-text indexes
//! (plan section 30).
//!
//! Both FTS tables live in the worktree cache and are regenerable
//! caches: they are rebuilt from the durable `.nisse` records and the
//! bundled practice catalog whenever the underlying content differs.
//!
//! Only **active** facts and lessons are indexed — observations are raw
//! learning data, and superseded records remain for provenance but are
//! never retrieved as current knowledge (plan sections 45 and 53).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::error::RetrievalError;
use crate::domain::practice::PracticePack;
use crate::domain::records::{ProjectRecord, RecordKind};
use crate::jury::catalog::Catalog;
use crate::storage::meta::{META_PRACTICE_CATALOG_VERSION, meta_get, meta_set};
use crate::storage::records::{RecordLocation, StoredRecord, active_records};

/// Statistics of one record-index synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordSyncStats {
    /// Active facts/lessons newly indexed.
    pub indexed: usize,
    /// Index rows removed (records gone or superseded).
    pub removed: usize,
}

/// One record to index, flattened for FTS.
struct RecordDoc<'a> {
    id: &'a str,
    kind: RecordKind,
    subject: &'a str,
    text: &'a str,
    tags: String,
    /// Fact state label; empty for lessons.
    state: &'a str,
    /// Path of the record file relative to `.nisse/`.
    relative_path: String,
}

/// Flatten a stored fact or lesson into an indexable document.
///
/// Returns `None` for observations and for records under locations the
/// store never writes (defensive; loading already validates locations).
fn record_doc(record: &StoredRecord) -> Option<RecordDoc<'_>> {
    let (subject, text, tags, state) = match &record.record {
        ProjectRecord::Fact(fact) => (
            fact.subject.as_str(),
            fact.text.as_str(),
            fact.tags.join(" "),
            fact.state.as_str(),
        ),
        ProjectRecord::Lesson(lesson) => (
            lesson.subject.as_str(),
            lesson.text.as_str(),
            lesson.tags.join(" "),
            "",
        ),
        ProjectRecord::Observation(_) => return None,
    };
    let relative_path = match &record.location {
        RecordLocation::People { contributor } => {
            format!("people/{contributor}/{}.json", record.id())
        }
        RecordLocation::Common => format!("common/{}.json", record.id()),
    };
    Some(RecordDoc {
        id: record.id(),
        kind: record.record.kind(),
        subject,
        text,
        tags,
        state,
        relative_path,
    })
}

/// Synchronize the record full-text index with the durable records of
/// the worktree.
///
/// Records are immutable, so synchronization is additive: new active
/// facts/lessons are indexed, rows for records that are gone or no
/// longer active are removed. `stale` carries the IDs of facts whose
/// supporting content is absent from the current target snapshot
/// (plan section 48); their cached marking is (re)computed on every
/// sync because it is snapshot-dependent, never part of the immutable
/// record. This is safe to call before every query.
pub fn sync_records(
    conn: &Connection,
    db_path: &Path,
    records: &[StoredRecord],
    stale: &BTreeSet<String>,
) -> Result<RecordSyncStats, RetrievalError> {
    let mut desired: BTreeMap<&str, RecordDoc<'_>> = BTreeMap::new();
    for record in active_records(records) {
        if let Some(doc) = record_doc(record) {
            desired.insert(doc.id, doc);
        }
    }

    let existing: BTreeMap<String, (String, i64)> = {
        let mut stmt = conn
            .prepare("SELECT record_id, kind, potentially_stale FROM record_index")
            .map_err(|err| query_error(db_path, err))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, i64>(2)?),
                ))
            })
            .map_err(|err| query_error(db_path, err))?;
        rows.map(|row| row.map_err(|err| query_error(db_path, err)))
            .collect::<Result<BTreeMap<_, _>, _>>()?
    };

    let mut stats = RecordSyncStats::default();
    let now = crate::domain::time::now_utc_rfc3339();
    let tx = conn
        .unchecked_transaction()
        .map_err(|err| query_error(db_path, err))?;

    // Remove rows whose records are gone or no longer active knowledge.
    for id in existing.keys() {
        if !desired.contains_key(id.as_str()) {
            tx.execute("DELETE FROM record_fts WHERE record_id = ?1", [id])
                .map_err(|err| query_error(db_path, err))?;
            tx.execute("DELETE FROM record_index WHERE record_id = ?1", [id])
                .map_err(|err| query_error(db_path, err))?;
            stats.removed += 1;
        }
    }

    // Index active records that are not in the index yet. Records are
    // immutable, so an existing row never needs rewriting — except the
    // snapshot-dependent staleness marking, which is refreshed for
    // existing rows whenever the assessment changed (for example after
    // content moved on or a stale fact was revalidated and superseded).
    for (id, doc) in &desired {
        if let Some((_, existing_stale)) = existing.get(*id) {
            let flag = i64::from(stale.contains(*id));
            if *existing_stale != flag {
                tx.execute(
                    "UPDATE record_index SET potentially_stale = ?1 WHERE record_id = ?2",
                    rusqlite::params![flag, id],
                )
                .map_err(|err| query_error(db_path, err))?;
            }
            continue;
        }
        tx.execute(
            "INSERT INTO record_fts (subject, text, tags, record_id, kind, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                doc.subject,
                doc.text,
                doc.tags,
                doc.id,
                doc.kind.as_str(),
                doc.state
            ],
        )
        .map_err(|err| query_error(db_path, err))?;
        tx.execute(
            "INSERT INTO record_index (record_id, kind, path, indexed_at, potentially_stale) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                doc.id,
                doc.kind.as_str(),
                doc.relative_path,
                now,
                i64::from(stale.contains(*id))
            ],
        )
        .map_err(|err| query_error(db_path, err))?;
        stats.indexed += 1;
    }

    tx.commit().map_err(|err| query_error(db_path, err))?;
    Ok(stats)
}

/// Deterministic version marker of a practice catalog: a SHA-256 over
/// every pack and practice field that is indexed.
pub fn practice_catalog_version(packs: &[PracticePack]) -> String {
    let mut hasher = Sha256::new();
    for pack in packs {
        hasher.update(pack.id.as_bytes());
        hasher.update([0]);
        for tag in &pack.tags {
            hasher.update(tag.as_bytes());
            hasher.update([0]);
        }
        hasher.update([1]);
        for practice in &pack.practices {
            hasher.update(practice.id.as_bytes());
            hasher.update([0]);
            hasher.update(practice.title.as_bytes());
            hasher.update([0]);
            hasher.update(practice.statement.as_bytes());
            hasher.update([0]);
        }
        hasher.update([2]);
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Synchronize the practice full-text index with the bundled catalog.
///
/// The catalog is static per binary; the indexed version marker makes
/// the sync idempotent and self-healing after cache clears and binary
/// upgrades.
pub fn sync_practices(
    conn: &Connection,
    db_path: &Path,
    catalog: &Catalog,
) -> Result<(), RetrievalError> {
    let version = practice_catalog_version(catalog.practice_packs());
    if meta_get(conn, db_path, META_PRACTICE_CATALOG_VERSION)? == Some(version.clone()) {
        return Ok(());
    }

    let tx = conn
        .unchecked_transaction()
        .map_err(|err| query_error(db_path, err))?;
    tx.execute("DELETE FROM practice_fts", [])
        .map_err(|err| query_error(db_path, err))?;
    for pack in catalog.practice_packs() {
        for practice in &pack.practices {
            tx.execute(
                "INSERT INTO practice_fts (title, statement, tags, practice_id, pack_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    practice.title,
                    practice.statement,
                    pack.tags.join(" "),
                    practice.id,
                    pack.id
                ],
            )
            .map_err(|err| query_error(db_path, err))?;
        }
    }
    tx.commit().map_err(|err| query_error(db_path, err))?;
    meta_set(conn, db_path, META_PRACTICE_CATALOG_VERSION, &version)?;
    Ok(())
}

/// Map a rusqlite failure into a typed storage error.
fn query_error(db_path: &Path, err: rusqlite::Error) -> RetrievalError {
    RetrievalError::Storage(crate::storage::StorageError::Query {
        path: db_path.to_owned(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{EvidenceKind, EvidenceRef};
    use crate::domain::records::{FactRecord, FactState, LessonRecord, RoutingHint};
    use crate::storage::db::{open_or_recover, read_user_version};
    use crate::storage::records::RecordStore;
    use crate::storage::schema::DbKind;

    /// The recorded staleness flag of one indexed record.
    fn stale_flag(conn: &Connection, record_id: &str) -> i64 {
        conn.query_row(
            "SELECT potentially_stale FROM record_index WHERE record_id = ?1",
            [record_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn temp_worktree(label: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("nisse-fts-{label}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join("worktree.sqlite");
        (dir, path)
    }

    fn open(label: &str) -> (tempfile::TempDir, std::path::PathBuf, Connection) {
        let (dir, path) = temp_worktree(label);
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        (dir, path, opened.conn)
    }

    fn evidence() -> Vec<EvidenceRef> {
        vec![EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/queue.rs".to_string()),
            symbol: None,
            line_start: Some(1),
            line_end: Some(2),
            content_key: None,
            plan_section: None,
            record_id: None,
            excerpt: "static QUEUE: Mutex<Queue>".to_string(),
        }]
    }

    fn fact(
        id: &str,
        subject: &str,
        text: &str,
        state: FactState,
        supersedes: Vec<String>,
    ) -> ProjectRecord {
        ProjectRecord::Fact(FactRecord {
            schema_version: 1,
            id: id.to_string(),
            author: "kalle".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            subject: subject.to_string(),
            text: text.to_string(),
            tags: vec!["queue".to_string()],
            state,
            evidence: evidence(),
            source_review_id: None,
            source_record_ids: Vec::new(),
            supersedes,
        })
    }

    fn lesson(id: &str, subject: &str, text: &str) -> ProjectRecord {
        ProjectRecord::Lesson(LessonRecord {
            schema_version: 1,
            id: id.to_string(),
            author: "kalle".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            subject: subject.to_string(),
            text: text.to_string(),
            tags: vec!["queue".to_string()],
            routing_hint: RoutingHint {
                reviewer_id: "concurrency".to_string(),
                path_globs: vec!["src/queue.rs".to_string()],
                required_tags_any: Vec::new(),
                boost: 0.2,
            },
            source_observation_ids: vec!["01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()],
            supersedes: Vec::new(),
        })
    }

    fn fts_count(conn: &Connection, record_id: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM record_fts WHERE record_id = ?1",
            [record_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn index_count(conn: &Connection, record_id: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM record_index WHERE record_id = ?1",
            [record_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn sync_indexes_active_facts_and_lessons_and_skips_observations() {
        let (_dir, path, conn) = open("records");
        let stored: Vec<StoredRecord> = vec![
            StoredRecord {
                record: fact(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "queue locking",
                    "The queue is locked across await points.",
                    FactState::Supported,
                    Vec::new(),
                ),
                location: RecordLocation::People {
                    contributor: "kalle".to_string(),
                },
                path: std::path::PathBuf::from(
                    "/repo/.nisse/people/kalle/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
                ),
            },
            StoredRecord {
                record: lesson(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                    "queue reviews",
                    "Concurrency reviewers catch queue races here.",
                ),
                location: RecordLocation::Common,
                path: std::path::PathBuf::from(
                    "/repo/.nisse/common/01ARZ3NDEKTSV4RRFFQ69G5FAW.json",
                ),
            },
        ];

        let stats = sync_records(&conn, &path, &stored, &BTreeSet::new()).unwrap();
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.removed, 0);
        assert_eq!(fts_count(&conn, "01ARZ3NDEKTSV4RRFFQ69G5FAV"), 1);
        assert_eq!(fts_count(&conn, "01ARZ3NDEKTSV4RRFFQ69G5FAW"), 1);
        // record_index paths are .nisse-relative.
        let indexed_path: String = conn
            .query_row(
                "SELECT path FROM record_index WHERE record_id = '01ARZ3NDEKTSV4RRFFQ69G5FAV'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed_path, "people/kalle/01ARZ3NDEKTSV4RRFFQ69G5FAV.json");

        // Idempotent second run.
        let again = sync_records(&conn, &path, &stored, &BTreeSet::new()).unwrap();
        assert_eq!(again.indexed, 0);
        assert_eq!(again.removed, 0);
    }

    #[test]
    fn sync_removes_rows_of_vanished_records() {
        let (_dir, path, conn) = open("remove");
        let stored: Vec<StoredRecord> = vec![StoredRecord {
            record: fact(
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "s",
                "text",
                FactState::Supported,
                Vec::new(),
            ),
            location: RecordLocation::Common,
            path: std::path::PathBuf::from("/repo/.nisse/common/01ARZ3NDEKTSV4RRFFQ69G5FAV.json"),
        }];
        sync_records(&conn, &path, &stored, &BTreeSet::new()).unwrap();
        assert_eq!(fts_count(&conn, "01ARZ3NDEKTSV4RRFFQ69G5FAV"), 1);

        let stats = sync_records(&conn, &path, &[], &BTreeSet::new()).unwrap();
        assert_eq!(stats.removed, 1);
        assert_eq!(fts_count(&conn, "01ARZ3NDEKTSV4RRFFQ69G5FAV"), 0);
        assert_eq!(index_count(&conn, "01ARZ3NDEKTSV4RRFFQ69G5FAV"), 0);
    }

    #[test]
    fn superseded_records_are_not_indexed_as_current_knowledge() {
        let (_dir, path, conn) = open("superseded");
        let older_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let newer_id = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
        let stored: Vec<StoredRecord> = vec![
            StoredRecord {
                record: fact(
                    older_id,
                    "old subject",
                    "old text",
                    FactState::Supported,
                    Vec::new(),
                ),
                location: RecordLocation::Common,
                path: std::path::PathBuf::from(format!("/repo/.nisse/common/{older_id}.json")),
            },
            StoredRecord {
                record: fact(
                    newer_id,
                    "new subject",
                    "new text",
                    FactState::Supported,
                    vec![older_id.to_string()],
                ),
                location: RecordLocation::Common,
                path: std::path::PathBuf::from(format!("/repo/.nisse/common/{newer_id}.json")),
            },
        ];

        sync_records(&conn, &path, &stored, &BTreeSet::new()).unwrap();
        assert_eq!(fts_count(&conn, older_id), 0, "superseded fact stays out");
        assert_eq!(fts_count(&conn, newer_id), 1);

        // When the newer record disappears (e.g. branch without it), the
        // older record becomes active again and is indexed.
        let only_older: Vec<StoredRecord> = vec![stored[0].clone()];
        let stats = sync_records(&conn, &path, &only_older, &BTreeSet::new()).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.removed, 1);
        assert_eq!(fts_count(&conn, older_id), 1);
    }

    #[test]
    fn practice_sync_is_idempotent_and_version_aware() {
        let (_dir, path, conn) = open("practices");
        let catalog = Catalog::bundled().unwrap();
        sync_practices(&conn, &path, &catalog).unwrap();
        let first: i64 = conn
            .query_row("SELECT count(*) FROM practice_fts", [], |row| row.get(0))
            .unwrap();
        let expected: usize = catalog
            .practice_packs()
            .iter()
            .map(|pack| pack.practices.len())
            .sum();
        assert_eq!(first as usize, expected);

        // Second run is a no-op.
        sync_practices(&conn, &path, &catalog).unwrap();
        let second: i64 = conn
            .query_row("SELECT count(*) FROM practice_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(first, second);

        // A changed catalog version rebuilds the index: mutate the
        // version marker to simulate a different catalog.
        meta_set(&conn, &path, META_PRACTICE_CATALOG_VERSION, "stale").unwrap();
        sync_practices(&conn, &path, &catalog).unwrap();
        let rebuilt: i64 = conn
            .query_row("SELECT count(*) FROM practice_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rebuilt, first);
    }

    #[test]
    fn practice_version_is_deterministic_and_content_sensitive() {
        let catalog = Catalog::bundled().unwrap();
        let packs = catalog.practice_packs().to_vec();
        assert_eq!(
            practice_catalog_version(&packs),
            practice_catalog_version(&packs)
        );

        let mut changed = packs.clone();
        changed[0].practices[0].statement.push(' ');
        assert_ne!(
            practice_catalog_version(&packs),
            practice_catalog_version(&changed)
        );
    }

    #[test]
    fn staleness_marking_is_computed_per_sync() {
        let (_dir, path, conn) = open("stale-marking");
        let fact_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let stored: Vec<StoredRecord> = vec![StoredRecord {
            record: fact(
                fact_id,
                "queue locking",
                "The queue is locked across await points.",
                FactState::Supported,
                Vec::new(),
            ),
            location: RecordLocation::People {
                contributor: "kalle".to_string(),
            },
            path: std::path::PathBuf::from(format!("/repo/.nisse/people/kalle/{fact_id}.json")),
        }];

        // Fresh sync: the fact is current.
        sync_records(&conn, &path, &stored, &BTreeSet::new()).unwrap();
        assert_eq!(stale_flag(&conn, fact_id), 0);

        // The next sync runs against a snapshot where the support is
        // gone: the cached flag flips without rewriting the record.
        let stale = BTreeSet::from([fact_id.to_string()]);
        let stats = sync_records(&conn, &path, &stored, &stale).unwrap();
        assert_eq!(stats.indexed, 0, "the immutable record is not re-indexed");
        assert_eq!(stale_flag(&conn, fact_id), 1);

        // A stale set naming a different record does not leak.
        let other = BTreeSet::from(["01OTHER".to_string()]);
        sync_records(&conn, &path, &stored, &other).unwrap();
        assert_eq!(stale_flag(&conn, fact_id), 0);

        // Idempotent re-sync keeps the flag.
        sync_records(&conn, &path, &stored, &stale).unwrap();
        assert_eq!(stale_flag(&conn, fact_id), 1);
    }

    #[test]
    fn record_sync_after_schema_migration_works_on_fresh_databases() {
        // Fresh databases already sit at the newest schema version.
        let (_dir, path, conn) = open("fresh");
        assert_eq!(read_user_version(&conn, &path).unwrap(), 3);
        assert!(
            sync_records(&conn, &path, &[], &BTreeSet::new())
                .unwrap()
                .indexed
                == 0
        );
    }

    #[test]
    fn sync_with_real_record_store_files() {
        // End-to-end against the real record store layout.
        let dir = tempfile::Builder::new()
            .prefix("nisse-fts-store-")
            .tempdir()
            .unwrap();
        let store = RecordStore::new(dir.path());
        let record = fact(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "build system",
            "Release builds require the embedded linker script.",
            FactState::Supported,
            Vec::new(),
        );
        let stored = store.create(&record).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);

        let (_db_dir, path, conn) = open("store");
        sync_records(&conn, &path, &loaded, &BTreeSet::new()).unwrap();
        let matches: i64 = conn
            .query_row(
                "SELECT count(*) FROM record_fts WHERE record_fts MATCH '\"linker\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matches, 1);
        let _ = stored;
    }
}

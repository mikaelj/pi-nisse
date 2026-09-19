//! Lazy incremental index refresh (plan sections 27 and 28).
//!
//! Normal operations refresh the index on demand; no init or refresh
//! command exists. One refresh enumerates exactly one target snapshot:
//!
//! 1. enumerate the target snapshot through the snapshot reader (Git is
//!    the membership authority);
//! 2. detect each file's language (a recognized extension always wins;
//!    extensionless files are classified by a bounded shebang read) and
//!    obtain a content key;
//! 3. compare the worktree manifest:
//!    - path known with the same key, and the artifact still in the
//!      shared cache → reuse;
//!    - path new or changed, but the artifact already in the shared cache
//!      (another branch/worktree parsed identical content) → reuse;
//!    - otherwise → parse and cache;
//!    - manifest paths absent from the snapshot → remove the path
//!      mapping.
//!
//! Artifacts live in the **shared** cache keyed by
//! `(content_key, language, parser_version)`, which is what lets them be
//! reused across branches and worktrees; the **worktree** manifest only
//! maps the paths of the last refresh.
//!
//! Refresh statistics (`files_seen`, `files_reused`, `files_parsed`,
//! `files_removed`, duration) are recorded in the worktree cache's
//! `refresh_state` table and returned to the caller.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::artifact::{PARSER_VERSION, ParsedArtifact};
use super::content::{ContentKey, git_blob_oid, working_tree_key};
use super::error::IndexError;
use super::extract::{IndexerCache, LanguageIndexer};
use super::language::Language;
use super::shebang;
use crate::domain::time::now_utc_rfc3339;
use crate::git::{ObjectFormat, RepoContext, ResolvedSnapshot, SnapshotReader, SnapshotTarget};
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::schema::DbKind;

/// Statistics of one index refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshStats {
    /// Files of recognized languages enumerated in the target snapshot.
    pub files_seen: u64,
    /// Files whose cached artifact was reused (manifest hit or shared
    /// cache hit).
    pub files_reused: u64,
    /// Files parsed and newly cached.
    pub files_parsed: u64,
    /// Manifest path mappings removed because the path is absent from
    /// the target snapshot (or no longer indexable).
    pub files_removed: u64,
    /// Wall-clock duration of the refresh.
    pub duration: Duration,
}

impl RefreshStats {
    /// Render one summary line for reports and logs.
    pub fn summary(&self) -> String {
        format!(
            "index refresh: {} seen, {} reused, {} parsed, {} removed in {:?}",
            self.files_seen,
            self.files_reused,
            self.files_parsed,
            self.files_removed,
            self.duration
        )
    }
}

/// One indexed path of the refresh result.
struct IndexPath {
    /// Repository-relative path.
    path: String,
    /// Content key of the exact target-side bytes.
    key: ContentKey,
    /// Language detected for the path (extension or shebang).
    language: Language,
    /// Byte length of the content (from the parse read, or from the
    /// recorded content row for reuses).
    byte_len: i64,
}

/// One newly parsed artifact awaiting shared-cache persistence.
struct NewArtifact {
    /// Repository-relative path (for derived rows).
    path: String,
    /// The parsed artifact.
    artifact: ParsedArtifact,
    /// Byte length of the parsed content.
    byte_len: i64,
}

/// Refresh the source index for the target side of `resolved`.
///
/// Both cache databases are created on demand. All shared-cache writes
/// happen in one transaction and all worktree-cache writes in another, so
/// an interrupted refresh leaves both databases consistent.
pub fn refresh_index(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    paths: &StoragePaths,
) -> Result<RefreshStats, IndexError> {
    let started = Instant::now();

    let reader = SnapshotReader::new(ctx, resolved)?;
    let listings = reader.list_files_with_oids()?;

    // Working-tree targets hash the exact bytes; index and commit-tree
    // targets already carry Git blob object IDs.
    let working = matches!(resolved.target, SnapshotTarget::Working);
    let mut index_oids: Option<BTreeMap<String, String>> = None;
    let mut object_format: Option<ObjectFormat> = None;
    if working {
        let mut map = BTreeMap::new();
        for entry in ctx.index_entries()? {
            if entry.stage == 0 {
                map.insert(entry.path.clone(), entry.oid.clone());
            }
        }
        index_oids = Some(map);
        object_format = Some(ctx.object_format()?);
    }

    let shared = open_or_recover(&paths.shared, DbKind::Shared)?;
    let worktree = open_or_recover(&paths.worktree, DbKind::Worktree)?;
    let manifest = load_manifest(&worktree.conn, &paths.worktree)?;

    let mut stats = RefreshStats {
        files_seen: 0,
        files_reused: 0,
        files_parsed: 0,
        files_removed: 0,
        duration: Duration::ZERO,
    };

    let mut current: Vec<IndexPath> = Vec::new();
    let mut new_artifacts: Vec<NewArtifact> = Vec::new();
    let mut indexers = IndexerCache::new();
    // Content keys already parsed (or known cached) during this refresh;
    // duplicate-content paths (copies) reuse the same artifact instead of
    // colliding on the parsed_artifact unique constraint.
    let mut known_keys: BTreeSet<String> = BTreeSet::new();

    // One file at a time, in Git's path order: detect the language (a
    // recognized extension always wins; extensionless files get a
    // bounded shebang read), then reuse-or-parse the exact target-side
    // content.
    for listing in listings {
        let Some(language) = detect_language(&reader, &listing.path)? else {
            continue;
        };
        stats.files_seen += 1;
        let path = listing.path.clone();

        let key = if let Some(oid) = &listing.blob_oid {
            // Index and commit-tree sides are defined by Git objects.
            ContentKey::Git(oid.clone())
        } else {
            // Working-tree side: hash the exact bytes. A tracked file
            // whose content hashes to its stage-0 index blob OID is
            // clean and gets the Git key; everything else (dirty or
            // untracked) gets a sha256 key.
            let Some(file) = reader.read(&path)? else {
                // Enumerated but unreadable (for example a race with the
                // working tree): skip rather than guess.
                continue;
            };
            let format = object_format.expect("working targets resolved the format");
            let hashed = git_blob_oid(&file.bytes, format);
            let clean = index_oids
                .as_ref()
                .and_then(|map| map.get(&path))
                .is_some_and(|oid| *oid == hashed);
            let key = if clean {
                ContentKey::Git(hashed)
            } else {
                working_tree_key(&file.bytes)
            };

            if artifact_exists(&shared.conn, &paths.shared, &key, language)?
                || known_keys.contains(&key.as_str())
            {
                current.push(IndexPath {
                    path,
                    key,
                    language,
                    byte_len: file.bytes.len() as i64,
                });
                stats.files_reused += 1;
                continue;
            }
            // Not cached: parse the exact bytes already read.
            let Ok(text) = std::str::from_utf8(&file.bytes) else {
                // Non-text content is not indexable; it never enters the
                // manifest, so later refreshes reconsider it.
                continue;
            };
            let extracted = indexers.for_detected_path(&path, language)?.parse(text);
            let artifact = ParsedArtifact::new(&key.as_str(), language, extracted);
            stats.files_parsed += 1;
            known_keys.insert(key.as_str());
            new_artifacts.push(NewArtifact {
                path: path.clone(),
                artifact,
                byte_len: file.bytes.len() as i64,
            });
            current.push(IndexPath {
                path,
                key,
                language,
                byte_len: file.bytes.len() as i64,
            });
            continue;
        };

        // Index/commit-tree side: reuse without reading when cached.
        if artifact_exists(&shared.conn, &paths.shared, &key, language)?
            || known_keys.contains(&key.as_str())
        {
            let byte_len = recorded_byte_len(&shared.conn, &paths.shared, &key)?;
            current.push(IndexPath {
                path,
                key,
                language,
                byte_len,
            });
            stats.files_reused += 1;
            continue;
        }
        let Some(file) = reader.read(&path)? else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&file.bytes) else {
            continue;
        };
        let extracted = indexers.for_detected_path(&path, language)?.parse(text);
        let artifact = ParsedArtifact::new(&key.as_str(), language, extracted);
        stats.files_parsed += 1;
        known_keys.insert(key.as_str());
        new_artifacts.push(NewArtifact {
            path: path.clone(),
            artifact,
            byte_len: file.bytes.len() as i64,
        });
        current.push(IndexPath {
            path,
            key,
            language,
            byte_len: file.bytes.len() as i64,
        });
    }

    // Manifest pruning: paths of the previous refresh that this refresh
    // no longer indexes.
    let current_paths: BTreeSet<&str> = current.iter().map(|entry| entry.path.as_str()).collect();
    let removed: Vec<String> = manifest
        .keys()
        .filter(|path| !current_paths.contains(path.as_str()))
        .cloned()
        .collect();
    stats.files_removed = removed.len() as u64;

    persist_shared(&shared.conn, &paths.shared, &new_artifacts)?;
    persist_worktree(&worktree.conn, &paths.worktree, &current, &removed)?;

    stats.duration = started.elapsed();
    persist_refresh_state(&worktree.conn, &paths.worktree, stats, &resolved.descriptor)?;
    Ok(stats)
}

/// Detect the language of one snapshot path.
///
/// A recognized extension always wins — extension-based classification
/// never reads content. Files whose name carries no extension at all are
/// classified by a bounded shebang read (see [`super::shebang`]);
/// everything else is not part of the source index.
fn detect_language(
    reader: &SnapshotReader<'_>,
    path: &str,
) -> Result<Option<Language>, IndexError> {
    if let Some(language) = Language::from_path(path) {
        return Ok(Some(language));
    }
    if !shebang::is_extensionless(path) {
        return Ok(None);
    }
    let Some(prefix) = reader.read_prefix(path, shebang::PREFIX_LIMIT)? else {
        return Ok(None);
    };
    Ok(shebang::language_from_prefix(&prefix))
}

/// Load the worktree manifest: path -> content key.
fn load_manifest(
    conn: &Connection,
    db_path: &Path,
) -> Result<BTreeMap<String, String>, IndexError> {
    let mut stmt = conn
        .prepare("SELECT path, content_key FROM manifest")
        .map_err(query_error(db_path))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(query_error(db_path))?;
    let mut manifest = BTreeMap::new();
    for row in rows {
        let (path, key) = row.map_err(query_error(db_path))?;
        manifest.insert(path, key);
    }
    Ok(manifest)
}

/// Whether the shared cache holds the artifact for `(key, language,
/// parser_version)`.
fn artifact_exists(
    conn: &Connection,
    db_path: &Path,
    key: &ContentKey,
    language: Language,
) -> Result<bool, IndexError> {
    conn.query_row(
        "SELECT 1 FROM parsed_artifact \
         WHERE content_key = ?1 AND language = ?2 AND parser_version = ?3",
        rusqlite::params![key.as_str(), language.label(), PARSER_VERSION],
        |_| Ok(()),
    )
    .map(|()| true)
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(query_error(db_path)(other)),
    })
}

/// Byte length recorded for a content key when it was first cached.
///
/// Returns 0 when the content row does not exist yet — this happens for
/// duplicate-content paths whose artifact was parsed earlier in the same
/// refresh and is persisted only at the end; the row appears with the
/// correct length then.
fn recorded_byte_len(
    conn: &Connection,
    db_path: &Path,
    key: &ContentKey,
) -> Result<i64, IndexError> {
    conn.query_row(
        "SELECT byte_len FROM content_artifact WHERE content_key = ?1",
        rusqlite::params![key.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(0),
        other => Err(other),
    })
    .map_err(query_error(db_path))
}

/// Write newly parsed artifacts and their derived rows into the shared
/// cache in one transaction.
///
/// `parsed_artifact` inserts are plain (not `OR IGNORE`): an artifact row
/// must not silently attach another parse's derived rows under a stale
/// row ID. A conflict here means a concurrent process parsed the same
/// content first, which fails loudly and is retried by the next refresh.
fn persist_shared(
    conn: &Connection,
    db_path: &Path,
    artifacts: &[NewArtifact],
) -> Result<(), IndexError> {
    if artifacts.is_empty() {
        return Ok(());
    }
    let now = now_utc_rfc3339();
    let tx = conn.unchecked_transaction().map_err(query_error(db_path))?;
    for entry in artifacts {
        let artifact = &entry.artifact;
        tx.execute(
            "INSERT OR IGNORE INTO content_artifact (content_key, byte_len, created_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![artifact.content_key, entry.byte_len, now],
        )
        .map_err(query_error(db_path))?;
        tx.execute(
            "INSERT INTO parsed_artifact \
             (content_key, language, parser_version, artifact, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                artifact.content_key,
                artifact.language.label(),
                artifact.parser_version,
                serde_json::to_string(artifact).expect("artifact JSON is always serializable"),
                now
            ],
        )
        .map_err(query_error(db_path))?;
        let artifact_id = tx.last_insert_rowid();
        for symbol in &artifact.symbols {
            tx.execute(
                "INSERT INTO symbol \
                 (parsed_artifact_id, name, kind, path, line_start, line_end, \
                  byte_start, byte_end) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    artifact_id,
                    symbol.name,
                    symbol.kind.label(),
                    entry.path,
                    symbol.line_start as i64,
                    symbol.line_end as i64,
                    symbol.byte_start as i64,
                    symbol.byte_end as i64,
                ],
            )
            .map_err(query_error(db_path))?;
        }
        for reference in &artifact.references {
            tx.execute(
                "INSERT INTO reference (parsed_artifact_id, name, path, line) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    artifact_id,
                    reference.name,
                    entry.path,
                    reference.line as i64,
                ],
            )
            .map_err(query_error(db_path))?;
        }
        if !artifact.skeleton.is_empty() {
            tx.execute(
                "INSERT OR IGNORE INTO text_chunk (content_key, path, chunk_index, text) \
                 VALUES (?1, ?2, 0, ?3)",
                rusqlite::params![artifact.content_key, entry.path, artifact.skeleton],
            )
            .map_err(query_error(db_path))?;
            tx.execute(
                "INSERT INTO text_chunk_fts (text, content_key, path) VALUES (?1, ?2, ?3)",
                rusqlite::params![artifact.skeleton, artifact.content_key, entry.path],
            )
            .map_err(query_error(db_path))?;
        }
    }
    tx.commit().map_err(query_error(db_path))?;
    Ok(())
}

/// Write the worktree manifest and path content in one transaction.
fn persist_worktree(
    conn: &Connection,
    db_path: &Path,
    current: &[IndexPath],
    removed: &[String],
) -> Result<(), IndexError> {
    let now = now_utc_rfc3339();
    let tx = conn.unchecked_transaction().map_err(query_error(db_path))?;
    for entry in current {
        tx.execute(
            "INSERT INTO manifest (path, content_key, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(path) DO UPDATE SET \
             content_key = excluded.content_key, updated_at = excluded.updated_at",
            rusqlite::params![entry.path, entry.key.as_str(), now],
        )
        .map_err(query_error(db_path))?;
        tx.execute(
            "INSERT INTO path_content (path, content_key, language, byte_len, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(path, content_key) DO UPDATE SET \
             language = excluded.language, byte_len = excluded.byte_len, \
             updated_at = excluded.updated_at",
            rusqlite::params![
                entry.path,
                entry.key.as_str(),
                entry.language.label(),
                entry.byte_len,
                now
            ],
        )
        .map_err(query_error(db_path))?;
    }
    for path in removed {
        tx.execute(
            "DELETE FROM manifest WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(query_error(db_path))?;
        tx.execute(
            "DELETE FROM path_content WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(query_error(db_path))?;
    }
    tx.commit().map_err(query_error(db_path))?;
    Ok(())
}

/// Record the last refresh statistics and the snapshot binding of the
/// refreshed membership tables.
///
/// The binding lets later retrieval refuse to answer with membership
/// data of a different snapshot (plan section 26): cache presence never
/// proves snapshot membership.
fn persist_refresh_state(
    conn: &Connection,
    db_path: &Path,
    stats: RefreshStats,
    descriptor: &crate::domain::snapshot::SnapshotDescriptor,
) -> Result<(), IndexError> {
    conn.execute(
        "INSERT INTO refresh_state \
         (id, files_seen, files_reused, files_parsed, files_removed, duration_ms, \
          last_refresh_at) \
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(id) DO UPDATE SET \
         files_seen = excluded.files_seen, files_reused = excluded.files_reused, \
         files_parsed = excluded.files_parsed, files_removed = excluded.files_removed, \
         duration_ms = excluded.duration_ms, last_refresh_at = excluded.last_refresh_at",
        rusqlite::params![
            stats.files_seen as i64,
            stats.files_reused as i64,
            stats.files_parsed as i64,
            stats.files_removed as i64,
            stats.duration.as_secs_f64() * 1000.0,
            now_utc_rfc3339(),
        ],
    )
    .map_err(query_error(db_path))?;
    let binding = serde_json::to_string(descriptor).expect("snapshot descriptors serialize");
    crate::storage::meta::meta_set(
        conn,
        db_path,
        crate::storage::meta::META_LAST_REFRESH_SNAPSHOT,
        &binding,
    )?;
    Ok(())
}

/// Map a rusqlite failure into a typed storage error bound to `db_path`.
fn query_error(db_path: &Path) -> impl Fn(rusqlite::Error) -> IndexError + use<'_> {
    move |err: rusqlite::Error| {
        IndexError::Storage(crate::storage::StorageError::Query {
            path: db_path.to_owned(),
            message: err.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::git::testutil::{Fixture, commit_all, git_in, stdout_trimmed, write_file};
    use crate::git::{SnapshotSpec, resolve_snapshot};

    fn discover(fixture: &Fixture) -> RepoContext {
        RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap()
    }

    fn refresh(ctx: &RepoContext, spec: &SnapshotSpec) -> RefreshStats {
        let resolved = resolve_snapshot(ctx, spec).unwrap();
        let paths = StoragePaths::for_repo(ctx);
        refresh_index(ctx, &resolved, &paths).unwrap()
    }

    fn query(ctx: &RepoContext, db: DbKind, sql: &str) -> i64 {
        let paths = StoragePaths::for_repo(ctx);
        let path = match db {
            DbKind::Shared => &paths.shared,
            DbKind::Worktree => &paths.worktree,
        };
        let opened = open_or_recover(path, db).unwrap();
        opened
            .conn
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .unwrap()
    }

    fn manifest_of(ctx: &RepoContext) -> BTreeMap<String, String> {
        let paths = StoragePaths::for_repo(ctx);
        let opened = open_or_recover(&paths.worktree, DbKind::Worktree).unwrap();
        load_manifest(&opened.conn, &paths.worktree).unwrap()
    }

    fn artifact_json(ctx: &RepoContext, content_key: &str) -> String {
        let paths = StoragePaths::for_repo(ctx);
        let opened = open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        opened
            .conn
            .query_row(
                "SELECT artifact FROM parsed_artifact WHERE content_key = ?1",
                rusqlite::params![content_key],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    }

    /// A fixture with every indexed language plus non-indexable files.
    fn language_fixture() -> Fixture {
        let fixture = Fixture::repo("idx-refresh");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn one() -> u8 {\n    1\n}\n",
        );
        write_file(
            &fixture.root,
            "src/main.c",
            "#include <stdio.h>\n\nint main(void) {\n    return 0;\n}\n",
        );
        write_file(
            &fixture.root,
            "src/app.cpp",
            "class App {\npublic:\n    void run();\n};\n\nvoid App::run() {}\n",
        );
        write_file(
            &fixture.root,
            "src/tool.cs",
            "namespace Tools;\n\npublic class Drill {\n    public void Spin() {}\n}\n",
        );
        write_file(
            &fixture.root,
            "lib/main.dart",
            "class Counter {\n  int value = 0;\n  void increment() {\n    value += 1;\n  }\n}\n",
        );
        write_file(
            &fixture.root,
            "web/index.js",
            "export function boot() {\n    return 1;\n}\n",
        );
        write_file(
            &fixture.root,
            "web/types.ts",
            "export type Id = string;\n\nexport function parse(id: Id): number {\n    return id.length;\n}\n",
        );
        write_file(&fixture.root, "README.md", "# not indexed\n");
        write_file(
            &fixture.root,
            "data/config.json",
            "{\"not\": \"indexed\"}\n",
        );
        fixture
    }

    #[test]
    fn duplicate_content_files_share_one_artifact() {
        let fixture = Fixture::repo("idx-duplicate");
        let content = "pub fn shared() -> u8 {\n    1\n}\n";
        write_file(&fixture.root, "src/one.rs", content);
        write_file(&fixture.root, "vendored/one_copy.rs", content);
        commit_all(&fixture.root, "one");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.files_parsed, 1, "identical bytes parse once");
        assert_eq!(stats.files_reused, 1, "the copy reuses the artifact");
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            1
        );
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest["src/one.rs"], manifest["vendored/one_copy.rs"]);

        // Also true when the duplicates are untracked (sha256 keys).
        write_file(&fixture.root, "other.rs", "fn more() {}\n");
        write_file(&fixture.root, "other_copy.rs", "fn more() {}\n");
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_parsed, 1, "duplicate untracked content");
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest["other.rs"], manifest["other_copy.rs"]);
        assert!(manifest["other.rs"].starts_with("sha256:"));
    }

    #[test]
    fn initial_large_fixture_parses_each_file_once() {
        let fixture = Fixture::repo("idx-large");
        // A larger tree: 40 distinct modules plus one file per remaining
        // language. Distinct content forces distinct content keys, so a
        // hashing or caching bug cannot hide behind identical artifacts.
        for i in 0..40 {
            write_file(
                &fixture.root,
                &format!("src/gen/mod{i}.rs"),
                &format!(
                    "pub struct Item{i} {{\n    pub id: u32,\n}}\n\npub fn process{i}(item: &Item{i}) -> u32 {{\n    item.id + {i}\n}}\n"
                ),
            );
        }
        write_file(
            &fixture.root,
            "src/main.c",
            "int main(void) { return 0; }\n",
        );
        write_file(&fixture.root, "src/app.cpp", "class App {};\n");
        write_file(&fixture.root, "src/tool.cs", "class Drill {}\n");
        write_file(&fixture.root, "lib/main.dart", "void main() {}\n");
        write_file(&fixture.root, "web/index.js", "function boot() {}\n");
        write_file(&fixture.root, "web/types.ts", "type Id = string;\n");
        commit_all(&fixture.root, "one");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 46);
        assert_eq!(stats.files_parsed, 46);
        assert_eq!(stats.files_reused, 0);
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            46
        );
        assert_eq!(
            query(
                &ctx,
                DbKind::Shared,
                "SELECT count(DISTINCT content_key) FROM parsed_artifact"
            ),
            46,
            "every file has a distinct content key"
        );

        // Immediately refreshing again reuses every artifact.
        let second = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(second.files_parsed, 0);
        assert_eq!(second.files_reused, 46);
    }

    #[test]
    fn initial_refresh_parses_every_indexable_file() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);

        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 7, "seven indexed-language files");
        assert_eq!(stats.files_parsed, 7);
        assert_eq!(stats.files_reused, 0);
        assert_eq!(stats.files_removed, 0);

        // Clean tracked content uses Git blob keys.
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest.len(), 7);
        for key in manifest.values() {
            assert!(
                key.starts_with("git:"),
                "clean file must use git key: {key}"
            );
        }

        // Shared cache rows: one artifact per file with derived rows.
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            7
        );
        assert_eq!(
            query(
                &ctx,
                DbKind::Shared,
                "SELECT count(*) FROM content_artifact"
            ),
            7
        );
        assert!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM symbol") >= 7,
            "definitions were extracted"
        );
        assert!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM reference") >= 7,
            "references were extracted"
        );
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM text_chunk"),
            7,
            "one skeleton chunk per artifact"
        );
        // Worktree manifest and path content are maintained.
        assert_eq!(
            query(&ctx, DbKind::Worktree, "SELECT count(*) FROM manifest"),
            7
        );
        assert_eq!(
            query(&ctx, DbKind::Worktree, "SELECT count(*) FROM path_content"),
            7
        );
        // Refresh state records the last refresh.
        let seen = query(
            &ctx,
            DbKind::Worktree,
            "SELECT files_seen FROM refresh_state WHERE id = 1",
        );
        assert_eq!(seen, 7);
    }

    #[test]
    fn second_refresh_without_changes_reparses_nothing() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);

        let first = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(first.files_parsed, 7);
        let second = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(second.files_seen, 7);
        assert_eq!(second.files_reused, 7);
        assert_eq!(second.files_parsed, 0);
        assert_eq!(second.files_removed, 0);
        // Still exactly one artifact per file.
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            7
        );
    }

    #[test]
    fn one_modification_parses_exactly_one_file() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        // Dirty one tracked file: its content no longer matches the index
        // blob, so it gets a sha256 key and must be parsed again.
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn one() -> u8 {\n    1\n}\npub fn two() -> u8 {\n    2\n}\n",
        );
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 7);
        assert_eq!(stats.files_parsed, 1);
        assert_eq!(stats.files_reused, 6);

        let manifest = manifest_of(&ctx);
        let dirty_key = &manifest["src/lib.rs"];
        assert!(dirty_key.starts_with("sha256:"), "dirty file: {dirty_key}");
        assert!(manifest["src/main.c"].starts_with("git:"));

        // The dirty artifact carries the new content.
        let artifact = artifact_json(&ctx, dirty_key);
        assert!(artifact.contains("two"));
    }

    #[test]
    fn revert_reuses_the_original_cached_artifact() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        let original = std::fs::read(fixture.root.join("src/lib.rs")).unwrap();
        let dirty = "pub fn one() -> u8 {\n    99\n}\n";
        std::fs::write(fixture.root.join("src/lib.rs"), dirty).unwrap();
        let modified = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(modified.files_parsed, 1);

        // Revert to the committed content: the key returns to git:<oid>
        // and the artifact is already in the shared cache.
        std::fs::write(fixture.root.join("src/lib.rs"), original).unwrap();
        let reverted = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(reverted.files_parsed, 0, "no reparse after revert");
        assert_eq!(reverted.files_reused, 7);
        let manifest = manifest_of(&ctx);
        assert!(manifest["src/lib.rs"].starts_with("git:"));
        // Only the dirty artifact was ever added: seven clean artifacts
        // plus one dirty artifact equals eight total.
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            8
        );
    }

    #[test]
    fn linked_worktree_reuses_shared_artifacts_without_reparsing() {
        let fixture = language_fixture();
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

        let main_ctx = discover(&fixture);
        let main = refresh(&main_ctx, &SnapshotSpec::Working);
        assert_eq!(main.files_parsed, 7);

        // The linked worktree checks out the same commit: identical
        // content keys, artifacts already in the shared cache.
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        let shared = refresh(&wt_ctx, &SnapshotSpec::Working);
        assert_eq!(shared.files_seen, 7);
        assert_eq!(shared.files_parsed, 0, "worktree reuses shared artifacts");
        assert_eq!(shared.files_reused, 7);

        // The shared cache still holds exactly seven artifacts (no
        // duplicates from the second worktree).
        assert_eq!(
            query(
                &main_ctx,
                DbKind::Shared,
                "SELECT count(*) FROM parsed_artifact"
            ),
            7
        );
        // Each worktree maintains its own manifest.
        assert_eq!(
            query(&wt_ctx, DbKind::Worktree, "SELECT count(*) FROM manifest"),
            7
        );
    }

    #[test]
    fn removed_paths_prune_the_manifest() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        std::fs::remove_file(fixture.root.join("web/index.js")).unwrap();
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 6);
        assert_eq!(stats.files_parsed, 0);
        assert_eq!(stats.files_removed, 1);

        let manifest = manifest_of(&ctx);
        assert!(!manifest.contains_key("web/index.js"));
        assert!(manifest.contains_key("web/types.ts"));
        assert_eq!(
            query(
                &ctx,
                DbKind::Worktree,
                "SELECT count(*) FROM path_content WHERE path = 'web/index.js'"
            ),
            0
        );
    }

    #[test]
    fn untracked_files_use_sha256_keys_and_are_indexed() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        write_file(&fixture.root, "src/new_untracked.rs", "pub fn fresh() {}\n");
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 8);
        assert_eq!(stats.files_parsed, 1);
        let manifest = manifest_of(&ctx);
        let key = &manifest["src/new_untracked.rs"];
        assert!(key.starts_with("sha256:"), "untracked content: {key}");
    }

    #[test]
    fn clean_tracked_file_key_matches_gits_blob_oid() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        let oid = stdout_trimmed(git_in(&fixture.root, &["rev-parse", "HEAD:src/lib.rs"]));
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest["src/lib.rs"], format!("git:{oid}"));
    }

    #[test]
    fn staged_refresh_uses_index_content_not_working_content() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        // Stage different content, then dirty the working tree beyond it.
        write_file(&fixture.root, "src/lib.rs", "pub fn staged() {}\n");
        git_in(&fixture.root, &["add", "src/lib.rs"]);
        write_file(&fixture.root, "src/lib.rs", "pub fn working() {}\n");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Staged);
        assert_eq!(stats.files_seen, 7);
        assert_eq!(stats.files_parsed, 7, "staged content differs from HEAD");

        let staged_oid = stdout_trimmed(git_in(&fixture.root, &["rev-parse", ":src/lib.rs"]));
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest["src/lib.rs"], format!("git:{staged_oid}"));
        // The parsed artifact contains the staged function, never the
        // working-tree-only one.
        let artifact = artifact_json(&ctx, &manifest["src/lib.rs"]);
        assert!(artifact.contains("staged"));
        assert!(!artifact.contains("working"));
    }

    #[test]
    fn commit_refresh_reads_historical_content_not_the_worktree() {
        let fixture = language_fixture();
        let first = commit_all(&fixture.root, "one");
        // Change the working tree well past the first commit.
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn one() -> u8 {\n    1\n}\npub fn future() {}\n",
        );
        commit_all(&fixture.root, "two");

        let ctx = discover(&fixture);
        let stats = refresh(
            &ctx,
            &SnapshotSpec::Commit {
                revision: first.clone(),
            },
        );
        assert_eq!(stats.files_seen, 7);
        assert_eq!(
            stats.files_parsed, 7,
            "historical content differs from HEAD"
        );

        let historical_oid = stdout_trimmed(git_in(
            &fixture.root,
            &["rev-parse", &format!("{first}:src/lib.rs")],
        ));
        let manifest = manifest_of(&ctx);
        assert_eq!(manifest["src/lib.rs"], format!("git:{historical_oid}"));
        let artifact = artifact_json(&ctx, &manifest["src/lib.rs"]);
        assert!(artifact.contains("one"));
        assert!(
            !artifact.contains("future"),
            "historical snapshot must never read newer content"
        );
    }

    #[test]
    fn shared_commit_content_is_reused_across_branches() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "base");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        // Commit the same content on a new branch and switch to it: the
        // blob OIDs are unchanged, so nothing reparses.
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "topic"]);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_parsed, 0);
        assert_eq!(stats.files_reused, 7);
    }

    #[test]
    fn invalid_utf8_files_are_skipped_without_entering_the_manifest() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        std::fs::write(
            fixture.root.join("src/binary.rs"),
            b"\xff\xfe\x00not text\xff",
        )
        .unwrap();
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 8);
        assert_eq!(stats.files_parsed, 0, "non-text files are never parsed");
        let manifest = manifest_of(&ctx);
        assert!(
            !manifest.contains_key("src/binary.rs"),
            "non-text files are never indexed"
        );
    }

    #[test]
    fn cleared_shared_cache_forces_reparse_but_manifest_stays_consistent() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        // The shared cache is disposable: losing it rebuilds.
        std::fs::remove_file(StoragePaths::for_repo(&ctx).shared).unwrap();
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_parsed, 7, "cache loss reparses everything");
        assert_eq!(stats.files_reused, 0);
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            7
        );
    }

    #[test]
    fn unborn_repository_indexes_untracked_files() {
        let fixture = Fixture::repo("idx-unborn");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn one() -> u8 {\n    1\n}\n",
        );
        write_file(&fixture.root, "README.md", "readme\n");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.files_parsed, 1);
        let manifest = manifest_of(&ctx);
        assert!(manifest["src/lib.rs"].starts_with("sha256:"));
    }

    #[test]
    fn nisse_records_are_not_indexed() {
        let fixture = language_fixture();
        commit_all(&fixture.root, "one");
        write_file(
            &fixture.root,
            ".nisse/common/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
            "{\"schema_version\":1}\n",
        );
        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 7, ".json records are not indexable");
    }

    /// A fixture with one file per new indexed language.
    fn new_language_fixture() -> Fixture {
        let fixture = Fixture::repo("idx-new-languages");
        write_file(
            &fixture.root,
            "app/client.py",
            "from protocol import Packet\n\n\nclass Client:\n    async def connect(self):\n        await dial()\n\n    def parse(self, data):\n        return parse_packet(data)\n\n\ndef make_client(config):\n    return Client(config)\n",
        );
        write_file(
            &fixture.root,
            "scripts/lib.sh",
            "#!/usr/bin/env bash\nsource ./lib/common.sh\n\nvalidate() {\n  check_file \"$1\"\n}\n\nmain() {\n  validate\n}\n",
        );
        write_file(
            &fixture.root,
            "lua/modbox.lua",
            "local protocol = require(\"protocol\")\n\nlocal function decode(data)\n    return data\nend\n\nfunction M.send(packet)\nend\n",
        );
        write_file(
            &fixture.root,
            "web/page.html",
            "<!doctype html>\n<html><body>\n<main id=\"checkout\">\n  <a href=\"#checkout\">home</a>\n  <script src=\"js/app.js\"></script>\n</main>\n</body></html>\n",
        );
        fixture
    }

    /// The `path_content.language` recorded for one path.
    fn recorded_language(ctx: &RepoContext, path: &str) -> String {
        let paths = StoragePaths::for_repo(ctx);
        let opened = open_or_recover(&paths.worktree, DbKind::Worktree).unwrap();
        opened
            .conn
            .query_row(
                "SELECT language FROM path_content WHERE path = ?1",
                rusqlite::params![path],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    }

    /// `(language, path)` rows of the symbol table for one symbol name —
    /// the ordinary retrieval path over the shared cache.
    fn symbol_rows_for(ctx: &RepoContext, name: &str) -> Vec<(String, String)> {
        let paths = StoragePaths::for_repo(ctx);
        let opened = open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        let mut stmt = opened
            .conn
            .prepare(
                "SELECT pa.language, s.path FROM symbol s \
                 JOIN parsed_artifact pa ON pa.id = s.parsed_artifact_id \
                 WHERE s.name = ?1 ORDER BY s.path",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn new_languages_participate_in_content_addressed_caching() {
        let fixture = new_language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);

        // 1. Initial parse creates one artifact per file, with the new
        //    languages recorded on the path side.
        let first = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(first.files_seen, 4);
        assert_eq!(first.files_parsed, 4);
        assert_eq!(first.files_reused, 0);
        for (path, language) in [
            ("app/client.py", "python"),
            ("scripts/lib.sh", "bash"),
            ("lua/modbox.lua", "lua"),
            ("web/page.html", "html"),
        ] {
            assert_eq!(recorded_language(&ctx, path), language, "{path}");
        }

        // 2. Re-indexing unchanged content reuses every artifact.
        let second = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(second.files_parsed, 0);
        assert_eq!(second.files_reused, 4);

        // 3. Editing content in every new language produces a different
        //    content key and a new parse.
        write_file(
            &fixture.root,
            "app/client.py",
            "from protocol import Packet\n\n\nclass Client:\n    def parse(self, data):\n        return parse_packet(data)\n",
        );
        write_file(
            &fixture.root,
            "scripts/lib.sh",
            "#!/usr/bin/env bash\nsource ./lib/common.sh\n\nvalidate() {\n  check \"$1\"\n}\n\nextra_fn() {\n  :\n}\n",
        );
        write_file(
            &fixture.root,
            "lua/modbox.lua",
            "local protocol = require(\"protocol\")\n\nlocal function decode(data)\n    return data\nend\n\nfunction M.send(packet)\nend\n\nfunction extra_fn()\nend\n",
        );
        write_file(
            &fixture.root,
            "web/page.html",
            "<!doctype html>\n<html><body>\n<main id=\"checkout\">\n  <a href=\"#checkout\">home</a>\n  <aside id=\"extra\"></aside>\n  <script src=\"js/app.js\"></script>\n</main>\n</body></html>\n",
        );
        let third = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(third.files_parsed, 4, "one new parse per language");
        assert_eq!(third.files_reused, 0);
        let manifest = manifest_of(&ctx);
        for path in [
            "app/client.py",
            "scripts/lib.sh",
            "lua/modbox.lua",
            "web/page.html",
        ] {
            assert!(manifest[path].starts_with("sha256:"), "dirty file: {path}");
        }

        // 4. Reverting to the tracked blobs reuses the old artifacts.
        git_in(
            &fixture.root,
            &[
                "checkout",
                "--",
                "app/client.py",
                "scripts/lib.sh",
                "lua/modbox.lua",
                "web/page.html",
            ],
        );
        let fourth = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(fourth.files_parsed, 0);
        assert_eq!(fourth.files_reused, 4);
        let manifest = manifest_of(&ctx);
        for path in [
            "app/client.py",
            "scripts/lib.sh",
            "lua/modbox.lua",
            "web/page.html",
        ] {
            assert!(manifest[path].starts_with("git:"), "clean again: {path}");
        }
        // Exactly four extra artifacts (the dirty versions) were added.
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            8
        );
    }

    #[test]
    fn linked_worktree_reuses_new_language_artifacts() {
        let fixture = new_language_fixture();
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

        let main_ctx = discover(&fixture);
        let main = refresh(&main_ctx, &SnapshotSpec::Working);
        assert_eq!(main.files_parsed, 4);

        // Same Git blobs in the linked worktree: shared artifacts, no
        // reparse, worktree-local path mapping.
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        let shared = refresh(&wt_ctx, &SnapshotSpec::Working);
        assert_eq!(shared.files_seen, 4);
        assert_eq!(shared.files_parsed, 0);
        assert_eq!(shared.files_reused, 4);
        assert_eq!(
            query(
                &main_ctx,
                DbKind::Shared,
                "SELECT count(*) FROM parsed_artifact"
            ),
            4
        );
        assert_eq!(
            query(&wt_ctx, DbKind::Worktree, "SELECT count(*) FROM manifest"),
            4
        );
    }

    #[test]
    fn extensionless_scripts_are_indexed_by_shebang() {
        let fixture = Fixture::repo("idx-shebang");
        write_file(
            &fixture.root,
            "bin/pytool",
            "#!/usr/bin/env python3\nimport os\n\n\ndef run():\n    return os.getpid()\n",
        );
        write_file(
            &fixture.root,
            "bin/entry",
            "#!/bin/bash\nmain_entry() {\n  :\n}\n",
        );
        write_file(
            &fixture.root,
            "bin/posix",
            "#!/bin/sh\nposix_fn() {\n  :\n}\n",
        );
        write_file(
            &fixture.root,
            "bin/luatool",
            "#!/usr/bin/env lua5.4\nfunction luafn()\nend\n",
        );
        // Extensionless files without a recognized shebang stay out.
        write_file(&fixture.root, "Makefile", "all:\n\techo no shebang\n");
        write_file(&fixture.root, "LICENSE", "no shebang either\n");
        // Files with any extension are never sniffed — even with a
        // shebang — and a recognized extension beats a contradictory
        // shebang.
        write_file(
            &fixture.root,
            "notes.txt",
            "#!/usr/bin/env python\n# not indexed: unknown extension\n",
        );
        write_file(
            &fixture.root,
            "tricky.sh",
            "#!/usr/bin/env python3\ndef py_function():\n    pass\n",
        );
        commit_all(&fixture.root, "one");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 5, "four shebang scripts plus tricky.sh");
        for (path, language) in [
            ("bin/pytool", "python"),
            ("bin/entry", "bash"),
            ("bin/posix", "bash"),
            ("bin/luatool", "lua"),
            ("tricky.sh", "bash"),
        ] {
            assert_eq!(recorded_language(&ctx, path), language, "{path}");
        }
        let manifest = manifest_of(&ctx);
        for absent in ["Makefile", "LICENSE", "notes.txt"] {
            assert!(
                !manifest.contains_key(absent),
                "{absent} must not be indexed"
            );
        }
        // The shebang scripts' symbols are ordinary retrievable rows.
        assert_eq!(
            symbol_rows_for(&ctx, "run"),
            vec![("python".to_string(), "bin/pytool".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "posix_fn"),
            vec![("bash".to_string(), "bin/posix".to_string())]
        );
    }

    #[test]
    fn malformed_new_language_files_do_not_abort_the_refresh() {
        let fixture = Fixture::repo("idx-malformed");
        write_file(&fixture.root, "broken.py", "def broken(:\n");
        write_file(
            &fixture.root,
            "broken.sh",
            "#!/usr/bin/env bash\nbroken() {\n  if fi fi\n",
        );
        write_file(&fixture.root, "broken.lua", "function broken(\n");
        write_file(
            &fixture.root,
            "broken.html",
            "<div id=\"outer\"\n  <p>text\n",
        );
        write_file(&fixture.root, "good.rs", "pub fn good() {}\n");
        write_file(&fixture.root, "good.py", "def fine():\n    pass\n");
        commit_all(&fixture.root, "one");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        assert_eq!(stats.files_seen, 6);
        assert_eq!(
            stats.files_parsed, 6,
            "partial trees still produce artifacts"
        );
        // The good files are fully indexed despite the broken ones.
        assert_eq!(
            symbol_rows_for(&ctx, "fine"),
            vec![("python".to_string(), "good.py".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "good"),
            vec![("rust".to_string(), "good.rs".to_string())]
        );
    }

    #[test]
    fn retrieval_finds_new_language_symbols() {
        let fixture = new_language_fixture();
        commit_all(&fixture.root, "one");
        let ctx = discover(&fixture);
        refresh(&ctx, &SnapshotSpec::Working);

        // Python: query a class/function name, retrieve the artifact.
        assert_eq!(
            symbol_rows_for(&ctx, "Client"),
            vec![("python".to_string(), "app/client.py".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "make_client"),
            vec![("python".to_string(), "app/client.py".to_string())]
        );
        // Bash: shell function name.
        assert_eq!(
            symbol_rows_for(&ctx, "validate"),
            vec![("bash".to_string(), "scripts/lib.sh".to_string())]
        );
        // Lua: function/method name (qualified syntactic form).
        assert_eq!(
            symbol_rows_for(&ctx, "decode"),
            vec![("lua".to_string(), "lua/modbox.lua".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "M.send"),
            vec![("lua".to_string(), "lua/modbox.lua".to_string())]
        );
        // HTML: element ID and custom-element name.
        assert_eq!(
            symbol_rows_for(&ctx, "checkout"),
            vec![("html".to_string(), "web/page.html".to_string())]
        );
        // References are retrievable too (fragment -> id relation).
        let paths = StoragePaths::for_repo(&ctx);
        let opened = open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        let reference_hit: i64 = opened
            .conn
            .query_row(
                "SELECT count(*) FROM reference WHERE name = 'checkout'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reference_hit, 1, "fragment reference row exists");
    }

    #[test]
    fn repository_fixture_files_are_indexed_end_to_end() {
        let fixture = Fixture::repo("idx-repo-fixtures");
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index");
        copy_tree(&source, &fixture.root);
        commit_all(&fixture.root, "one");

        let ctx = discover(&fixture);
        let stats = refresh(&ctx, &SnapshotSpec::Working);
        // 13 fixture files: four Python (incl. .pyi and one shebang
        // script), four Bash (incl. two shebang scripts), three Lua
        // (incl. one shebang script), two HTML.
        assert_eq!(stats.files_seen, 13);
        assert_eq!(stats.files_parsed, 13);

        for (path, language) in [
            ("bash/broken.sh", "bash"),
            ("bash/libbox.sh", "bash"),
            ("bash/runbash", "bash"),
            ("bash/runsh", "bash"),
            ("html/broken.html", "html"),
            ("html/page.html", "html"),
            ("lua/broken.lua", "lua"),
            ("lua/modbox.lua", "lua"),
            ("lua/tool", "lua"),
            ("python/broken.py", "python"),
            ("python/client.py", "python"),
            ("python/protocol.pyi", "python"),
            ("python/tool", "python"),
        ] {
            assert_eq!(recorded_language(&ctx, path), language, "{path}");
        }

        // Representative retrieval per language, straight from the
        // frozen fixtures (including the extensionless ones).
        assert_eq!(
            symbol_rows_for(&ctx, "parse_packet"),
            vec![("python".to_string(), "python/protocol.pyi".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "validate_config"),
            Vec::<(String, String)>::new(),
            "references live in the reference table, not symbols"
        );
        assert_eq!(
            symbol_rows_for(&ctx, "run_all"),
            vec![("bash".to_string(), "bash/runbash".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "detect_os"),
            vec![("bash".to_string(), "bash/runsh".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "object:method"),
            vec![("lua".to_string(), "lua/modbox.lua".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "checkout"),
            vec![("html".to_string(), "html/page.html".to_string())]
        );
        assert_eq!(
            symbol_rows_for(&ctx, "user-profile"),
            vec![("html".to_string(), "html/page.html".to_string())]
        );

        // One skeleton chunk per non-empty skeleton. Twelve of the
        // thirteen artifacts carry structure; `broken.lua` recovers
        // nothing (no shebang, no parseable definition) and therefore
        // contributes an empty skeleton — deterministic, and refresh
        // still succeeds.
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM text_chunk"),
            12
        );
        assert_eq!(
            query(&ctx, DbKind::Shared, "SELECT count(*) FROM parsed_artifact"),
            13
        );
    }

    /// Recursively copy a fixture tree into a repository root.
    fn copy_tree(source: &std::path::Path, target: &std::path::Path) {
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let destination = target.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                std::fs::create_dir_all(&destination).unwrap();
                copy_tree(&entry.path(), &destination);
            } else {
                std::fs::copy(entry.path(), &destination).unwrap();
            }
        }
    }
}

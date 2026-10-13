//! Review snapshot diffs.
//!
//! Computes the exact change set between the base and target sides of a
//! resolved snapshot (plan section 25). Git produces the raw change data;
//! this module parses it into typed entries and applies the default
//! `.nisse/**` exclusion so Nisse's own durable-record storage never enters
//! application code review.
//!
//! Output is deterministic: entries are sorted by path, status letters come
//! from Git, and object IDs are full-length (`--no-abbrev`).

use std::collections::BTreeMap;

use super::error::GitError;
use super::reader::SnapshotReader;
use super::repo::RepoContext;
use super::snapshot::{ResolvedSnapshot, SnapshotBase, SnapshotTarget};

/// What happened to one path between the base and target sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffStatus {
    /// Path exists only on the target side.
    Added,
    /// Path exists only on the base side.
    Deleted,
    /// Content of a same-named blob changed.
    Modified,
    /// Rename detected by Git; [`DiffEntry::old_path`] is the source path.
    Renamed,
    /// Copy detected by Git; [`DiffEntry::old_path`] is the source path.
    Copied,
    /// Type changed (file, symlink, gitlink).
    TypeChanged,
}

impl DiffStatus {
    /// Parse the status letter of a raw diff entry.
    fn parse(status: &str) -> Result<Self, GitError> {
        let letter = status.as_bytes().first().copied().unwrap_or(b'?');
        match letter {
            b'A' => Ok(DiffStatus::Added),
            b'C' => Ok(DiffStatus::Copied),
            b'D' => Ok(DiffStatus::Deleted),
            b'M' => Ok(DiffStatus::Modified),
            b'R' => Ok(DiffStatus::Renamed),
            b'T' => Ok(DiffStatus::TypeChanged),
            _ => Err(GitError::UnexpectedOutput {
                command: "diff --raw".to_string(),
                message: format!("unreviewable diff status `{status}`"),
            }),
        }
    }
}

/// One changed path of a snapshot diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// What happened to the path.
    pub status: DiffStatus,
    /// Path on the target side (for deletions: the base-side path).
    pub path: String,
    /// Source path for renames and copies.
    pub old_path: Option<String>,
    /// Blob OID on the base side; `None` for additions and for zero OIDs.
    pub old_oid: Option<String>,
    /// Blob OID on the target side.
    ///
    /// Always `None` when the target side is the working tree: the final
    /// working-tree content is not a Git object; the content-index milestone
    /// introduces content keys for that side.
    pub new_oid: Option<String>,
    /// Whether either side looks binary (Git's heuristic or a NUL byte in
    /// the first 8000 bytes of working-tree content).
    pub binary: bool,
}

/// One bounded unified diff of a changed path.
///
/// The patch is presentation of Git's own diff for the reviewer context
/// of a code review; the typed [`DiffEntry`] list remains the authority
/// for what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPatch {
    /// Target-side path of the change (base-side path for deletions).
    pub path: String,
    /// What happened to the path.
    pub status: DiffStatus,
    /// Whether the change looks binary.
    pub binary: bool,
    /// Unified diff text, truncated to the caller's per-file budget.
    /// `None` when no textual diff exists (binary or non-UTF-8
    /// content). Empty when Git produced headers only (for example a
    /// mode-only change).
    pub patch: Option<String>,
    /// Whether the text was truncated to fit the budget.
    pub truncated: bool,
}

/// Produce bounded unified diffs for the changed paths of a snapshot.
///
/// `entries` must be the output of [`snapshot_diff`] (already sorted and
/// `.nisse`-excluded); at most `max_entries` paths are covered, each with
/// at most `per_file_budget` bytes of patch text (truncated on a char
/// boundary with an explicit marker).
///
/// Git produces the patches for every mode. Untracked additions of the
/// `working` target (and every file of an unborn `working` target, whose
/// base is the empty tree) never appear in `git diff` output, so their
/// added-file patches are synthesized from the target snapshot reader —
/// the same authority all source reads use.
pub fn snapshot_patches(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    entries: &[DiffEntry],
    max_entries: usize,
    per_file_budget: usize,
) -> Result<Vec<SnapshotPatch>, GitError> {
    let reader = SnapshotReader::new(ctx, resolved)?;
    let unborn_working = matches!(resolved.base, SnapshotBase::EmptyTree)
        && matches!(resolved.target, SnapshotTarget::Working);
    let untracked: BTreeMap<String, bool> = if matches!(resolved.target, SnapshotTarget::Working) {
        let untracked_set: std::collections::BTreeSet<String> =
            ctx.untracked_non_ignored()?.into_iter().collect();
        entries
            .iter()
            .map(|entry| {
                (
                    entry.path.clone(),
                    unborn_working || untracked_set.contains(&entry.path),
                )
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let diff_args = patch_args(ctx, resolved);

    let mut patches = Vec::new();
    for entry in entries.iter().take(max_entries) {
        if entry.binary {
            patches.push(SnapshotPatch {
                path: entry.path.clone(),
                status: entry.status,
                binary: true,
                patch: None,
                truncated: false,
            });
            continue;
        }
        if untracked.get(&entry.path).copied().unwrap_or(false) {
            let patch = reader
                .read(&entry.path)?
                .map(|file| synthesize_added_patch(&file.bytes, &entry.path, per_file_budget));
            let (patch, truncated) = match patch {
                Some(Some(text)) => (Some(text.0), text.1),
                // Non-UTF-8 or absent content: no textual diff.
                Some(None) => (None, false),
                None => (None, false),
            };
            patches.push(SnapshotPatch {
                path: entry.path.clone(),
                status: entry.status,
                binary: false,
                patch,
                truncated,
            });
            continue;
        }
        let mut args: Vec<&str> = vec![
            "--no-pager",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-abbrev",
            "--unified=3",
            "-M",
        ];
        args.extend(diff_args.iter().map(String::as_str));
        args.push("--");
        args.push(&entry.path);
        if let Some(old_path) = &entry.old_path {
            // Rename/copy detection needs both sides of the pair inside
            // the pathspec-limited diff; a destination-only pathspec
            // would degrade the change to a plain addition.
            args.push(old_path);
        }
        let output = ctx.runner().run(&ctx.worktree_root, &args)?;
        let (patch, truncated) = truncate_patch(&output.stdout, &entry.path, per_file_budget);
        patches.push(SnapshotPatch {
            path: entry.path.clone(),
            status: entry.status,
            binary: false,
            patch: Some(patch),
            truncated,
        });
    }
    Ok(patches)
}

/// Revision arguments of `git diff` for the target mode (without the
/// `diff` subcommand and flags; mirrors [`git_diff_entries`]).
///
/// Every commit-based mode names both sides explicitly: a single
/// revision argument would compare against the working tree instead of
/// the resolved head side.
fn patch_args(ctx: &RepoContext, resolved: &ResolvedSnapshot) -> Vec<String> {
    match (&resolved.base, &resolved.target) {
        (SnapshotBase::EmptyTree, SnapshotTarget::Working) => Vec::new(),
        (_, SnapshotTarget::Working) => vec!["HEAD".to_string()],
        (_, SnapshotTarget::Index) => vec!["--cached".to_string()],
        (base, SnapshotTarget::Commit { oid, .. }) => {
            let base_rev = match base {
                SnapshotBase::EmptyTree => ctx.empty_tree_oid(),
                SnapshotBase::Commit { oid: base_oid, .. } => Ok(base_oid.clone()),
            };
            vec![base_rev.unwrap_or_else(|_| oid.clone()), oid.clone()]
        }
    }
}

/// Truncate patch text to a byte budget on a char boundary, marking the
/// truncation deterministically and pointing the reviewer at the tool
/// that can verify the omitted region.
fn truncate_patch(text: &str, path: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_owned();
    truncated.push_str(&format!(
        "\n\u{2026} [diff truncated after {max_bytes} bytes; verify the omitted region via source_lookup (path={path})]"
    ));
    (truncated, true)
}

/// Synthesize the unified added-file patch of content that exists only on
/// the target side (`diff` against `/dev/null`), bounded to the budget.
///
/// Returns `None` for non-UTF-8 content.
fn synthesize_added_patch(
    content: &[u8],
    path: &str,
    per_file_budget: usize,
) -> Option<(String, bool)> {
    let text = std::str::from_utf8(content).ok()?;
    let mut patch = format!("diff --git a/{path} b/{path}\n--- /dev/null\n+++ b/{path}\n");
    if text.is_empty() {
        // An empty file has no hunks.
        return Some(truncate_patch(&patch, path, per_file_budget));
    }
    let has_final_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = body.split('\n').collect();
    // Git's unified-diff convention omits the count when it is one.
    let count = if lines.len() == 1 {
        "1".to_string()
    } else {
        format!("1,{}", lines.len())
    };
    patch.push_str(&format!("@@ -0,0 +{count} @@\n"));
    for (index, line) in lines.iter().enumerate() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
        if index + 1 == lines.len() && !has_final_newline {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    Some(truncate_patch(&patch, path, per_file_budget))
}

/// Compute the diff between the base and target sides of a resolved
/// snapshot, with the default `.nisse/**` exclusion applied.
pub fn snapshot_diff(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
) -> Result<Vec<DiffEntry>, GitError> {
    let mut entries = match (&resolved.base, &resolved.target) {
        // Unborn HEAD working review: the base is empty, so every file of
        // the final working-tree state is an addition.
        (SnapshotBase::EmptyTree, SnapshotTarget::Working) => working_unborn_entries(ctx)?,
        _ => git_diff_entries(ctx, resolved)?,
    };
    entries.retain_mut(include_entry);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Parse `git diff --raw` entries and mark binaries via `--numstat`.
fn git_diff_entries(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
) -> Result<Vec<DiffEntry>, GitError> {
    // For the working target, `ctx.head` is known: the unborn case is
    // handled by `working_unborn_entries` before this point. Arguments
    // follow the `diff` subcommand added by `run_diff`.
    let base = match &resolved.target {
        SnapshotTarget::Commit { .. } => Some(base_rev(ctx, &resolved.base)?),
        _ => None,
    };
    let (diff_args, untracked) = match (&resolved.target, base.as_deref()) {
        (SnapshotTarget::Working, _) => (vec!["--raw", "HEAD"], true),
        (SnapshotTarget::Index, _) => (vec!["--cached", "--raw"], false),
        (SnapshotTarget::Commit { oid, .. }, Some(base)) => {
            (vec!["--raw", base, oid.as_str()], false)
        }
        (SnapshotTarget::Commit { .. }, None) => unreachable!("base resolved above"),
    };
    let mut entries = parse_raw_entries(&run_diff(ctx, &diff_args)?)?;
    let numstat_args = numstat_args(&diff_args);
    let numstat_refs: Vec<&str> = numstat_args.iter().map(String::as_str).collect();
    let binary_by_path = parse_numstat_binary(&run_diff(ctx, &numstat_refs)?)?;
    for entry in &mut entries {
        entry.binary = binary_by_path.get(&entry.path).copied().unwrap_or(false);
    }
    if untracked {
        // The raw format sometimes reports a hashed OID for working-tree
        // content, but that object may not exist in the database. The
        // working tree is the authority, so target-side OIDs are dropped.
        for entry in &mut entries {
            entry.new_oid = None;
        }
        entries.extend(untracked_additions(ctx)?);
    }
    Ok(entries)
}

/// Build the same diff arguments for `--numstat`.
fn numstat_args(diff_args: &[&str]) -> Vec<String> {
    diff_args
        .iter()
        .map(|arg| {
            if *arg == "--raw" {
                "--numstat".to_string()
            } else {
                (*arg).to_string()
            }
        })
        .collect()
}

/// Run one `git diff` invocation with the frozen output contract.
///
/// The fixed flags pin the output format against ambient Git config:
/// external diff drivers and textconv filters are disabled, OIDs are never
/// abbreviated, and rename detection is always on.
fn run_diff(ctx: &RepoContext, diff_args: &[&str]) -> Result<String, GitError> {
    let mut args: Vec<&str> = vec![
        "--no-pager",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-abbrev",
        "-z",
        "-M",
    ];
    args.extend(diff_args.iter().copied());
    let output = ctx.runner().run(&ctx.worktree_root, &args)?;
    Ok(output.stdout)
}

/// Blob OID of the base side, as a Git revision argument.
fn base_rev(ctx: &RepoContext, base: &SnapshotBase) -> Result<String, GitError> {
    match base {
        SnapshotBase::EmptyTree => ctx.empty_tree_oid(),
        SnapshotBase::Commit { oid, .. } => Ok(oid.clone()),
    }
}

/// Entries for a `working` review on an unborn HEAD.
///
/// The base is the empty tree, so the final working-tree state (tracked
/// files that still exist plus untracked non-ignored files) is entirely
/// additions. Tracked files deleted from the working tree are absent from
/// the final state and produce no entry.
fn working_unborn_entries(ctx: &RepoContext) -> Result<Vec<DiffEntry>, GitError> {
    let mut entries = Vec::new();
    for path in ctx.tracked_files()? {
        if !final_state_contains(ctx, &path)? {
            continue;
        }
        let binary = file_prefix_is_binary(ctx, &path)?;
        entries.push(DiffEntry {
            status: DiffStatus::Added,
            path,
            old_path: None,
            old_oid: None,
            new_oid: None,
            binary,
        });
    }
    entries.extend(untracked_additions(ctx)?);
    Ok(entries)
}

/// Whether a tracked path still exists in the working tree.
fn final_state_contains(ctx: &RepoContext, path: &str) -> Result<bool, GitError> {
    let full = ctx.worktree_root.join(path);
    match std::fs::symlink_metadata(&full) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(GitError::Read {
            path: std::path::PathBuf::from(path),
            source: err,
        }),
    }
}

/// Untracked non-ignored files are additions in `working` reviews.
fn untracked_additions(ctx: &RepoContext) -> Result<Vec<DiffEntry>, GitError> {
    let mut entries = Vec::new();
    for path in ctx.untracked_non_ignored()? {
        let binary = file_prefix_is_binary(ctx, &path)?;
        entries.push(DiffEntry {
            status: DiffStatus::Added,
            path,
            old_path: None,
            old_oid: None,
            new_oid: None,
            binary,
        });
    }
    Ok(entries)
}

/// Whether the first 8000 bytes of a working-tree file contain a NUL byte
/// (Git's own binary-content heuristic).
fn file_prefix_is_binary(ctx: &RepoContext, path: &str) -> Result<bool, GitError> {
    const PREFIX_BYTES: usize = 8000;
    use std::io::Read;

    let full = ctx.worktree_root.join(path);
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(GitError::Read {
                path: std::path::PathBuf::from(path),
                source: err,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    let file = std::fs::File::open(&full).map_err(|err| GitError::Read {
        path: std::path::PathBuf::from(path),
        source: err,
    })?;
    file.take(PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(|err| GitError::Read {
            path: std::path::PathBuf::from(path),
            source: err,
        })?;
    Ok(prefix.contains(&0))
}

/// Parse `git diff --raw -z` output into entries.
///
/// Entry shape (all tokens NUL-terminated, no quoting):
///
/// ```text
/// :<src_mode> <dst_mode> <src_oid> <dst_oid> <status>\0<src_path>\0[<dst_path>\0]
/// ```
///
/// `<dst_path>` is present exactly for rename/copy statuses.
fn parse_raw_entries(stdout: &str) -> Result<Vec<DiffEntry>, GitError> {
    let mut tokens = stdout.split('\0').peekable();
    let mut entries = Vec::new();
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        let unexpected = |message: String| GitError::UnexpectedOutput {
            command: "diff --raw".to_string(),
            message,
        };
        // Combined-diff lines (`::...`) only appear for unmerged paths,
        // which snapshot resolution rejects; a defensive typed failure
        // keeps malformed output from being silently misread.
        if token.starts_with("::") {
            return Err(unexpected(
                "combined diff entry (unmerged state?)".to_string(),
            ));
        }
        let Some(meta) = token.strip_prefix(':') else {
            return Err(unexpected(format!("expected raw entry, found `{token}`")));
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(unexpected(format!("malformed raw entry `{meta}`")));
        }
        let status = DiffStatus::parse(fields[4])?;
        let src_path = tokens
            .next()
            .ok_or_else(|| unexpected("raw entry ended before the source path".to_string()))?;
        let dst_path = match status {
            DiffStatus::Renamed | DiffStatus::Copied => tokens.next().ok_or_else(|| {
                unexpected("rename entry ended before the destination path".to_string())
            })?,
            _ => src_path,
        };
        if dst_path.is_empty() || src_path.is_empty() {
            return Err(unexpected("empty path in raw entry".to_string()));
        }
        entries.push(DiffEntry {
            status,
            path: dst_path.to_owned(),
            old_path: if src_path == dst_path {
                None
            } else {
                Some(src_path.to_owned())
            },
            old_oid: zero_to_none(fields[2])?,
            new_oid: zero_to_none(fields[3])?,
            binary: false,
        });
    }
    Ok(entries)
}

/// Map Git's zero OID (absent side) to `None`, validating real OIDs.
fn zero_to_none(oid: &str) -> Result<Option<String>, GitError> {
    if oid.bytes().all(|byte| byte == b'0') {
        Ok(None)
    } else {
        RepoContext::validate_oid(oid)?;
        Ok(Some(oid.to_owned()))
    }
}

/// Parse `git diff --numstat -z` output into a binary flag per destination
/// path.
///
/// Entry shapes (NUL-terminated):
///
/// ```text
/// <added>\t<removed>\t<path>\0
/// <added>\t<removed>\t\0<src_path>\0<dst_path>\0
/// ```
///
/// A `-` line count marks binary content.
fn parse_numstat_binary(stdout: &str) -> Result<BTreeMap<String, bool>, GitError> {
    let unexpected = |message: String| GitError::UnexpectedOutput {
        command: "diff --numstat".to_string(),
        message,
    };
    let mut tokens = stdout.split('\0').peekable();
    let mut binary_by_path = BTreeMap::new();
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        let mut fields = token.splitn(3, '\t');
        let added = fields
            .next()
            .ok_or_else(|| unexpected("missing added-line field".to_string()))?;
        let removed = fields
            .next()
            .ok_or_else(|| unexpected("missing removed-line field".to_string()))?;
        let rest = fields
            .next()
            .ok_or_else(|| unexpected("missing path field".to_string()))?;
        validate_line_count(added)?;
        validate_line_count(removed)?;
        let binary = added == "-" || removed == "-";
        if rest.is_empty() {
            // Rename: the source and destination paths are separate tokens.
            let _src = tokens
                .next()
                .ok_or_else(|| unexpected("numstat rename entry ended early".to_string()))?;
            let dst = tokens
                .next()
                .ok_or_else(|| unexpected("numstat rename entry has no destination".to_string()))?;
            if dst.is_empty() {
                return Err(unexpected(
                    "empty destination path in numstat entry".to_string(),
                ));
            }
            binary_by_path.insert(dst.to_owned(), binary);
        } else {
            binary_by_path.insert(rest.to_owned(), binary);
        }
    }
    Ok(binary_by_path)
}

/// Validate one numstat line-count field: `-` or a non-negative integer.
fn validate_line_count(field: &str) -> Result<(), GitError> {
    let valid =
        field == "-" || (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()));
    if valid {
        Ok(())
    } else {
        Err(GitError::UnexpectedOutput {
            command: "diff --numstat".to_string(),
            message: format!("invalid line count `{field}`"),
        })
    }
}

/// Whether a path belongs to Nisse's durable-record storage.
///
/// `.nisse/**` is excluded from application code review by default.
fn is_excluded(path: &str) -> bool {
    path == ".nisse" || path.starts_with(".nisse/")
}

/// Decide whether one parsed entry belongs in the application-code diff.
///
/// `.nisse/**` is excluded by default. Renames touching `.nisse` are
/// reduced to the application-code view of the change:
///
/// - `.nisse/x` moved to `src/y` appears as an addition of `src/y`;
/// - `src/y` moved into `.nisse/x` appears as a deletion of `src/y`;
/// - moves entirely inside `.nisse` (or entirely outside) are unaffected.
fn include_entry(entry: &mut DiffEntry) -> bool {
    if let Some(old_path) = entry.old_path.clone() {
        let (old_excluded, new_excluded) = (is_excluded(&old_path), is_excluded(&entry.path));
        if old_excluded && new_excluded {
            return false;
        }
        if old_excluded {
            entry.status = DiffStatus::Added;
            entry.old_path = None;
            entry.old_oid = None;
            return true;
        }
        if new_excluded {
            entry.status = DiffStatus::Deleted;
            entry.path = old_path;
            entry.old_path = None;
            entry.new_oid = None;
            return true;
        }
        return true;
    }
    !is_excluded(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::git::snapshot::{SnapshotSpec, resolve_snapshot};
    use crate::git::testutil::{Fixture, commit_all, git_in, write_file};

    fn ctx_of(fixture: &Fixture) -> RepoContext {
        RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap()
    }

    fn diff_for(ctx: &RepoContext, spec: &SnapshotSpec) -> Vec<DiffEntry> {
        let resolved = resolve_snapshot(ctx, spec).unwrap();
        snapshot_diff(ctx, &resolved).unwrap()
    }

    fn find<'a>(entries: &'a [DiffEntry], path: &str) -> &'a DiffEntry {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no diff entry for `{path}` in {entries:?}"))
    }

    #[test]
    fn working_mode_combines_staged_unstaged_and_untracked() {
        let fixture = Fixture::repo("diff-working");
        write_file(&fixture.root, "both.txt", "head\n");
        write_file(&fixture.root, "gone.txt", "head\n");
        write_file(&fixture.root, "clean.txt", "unchanged\n");
        commit_all(&fixture.root, "one");
        let head_blob = blob_oid(&fixture.root, "HEAD:both.txt");

        // Staged and unstaged changes to the same file collapse into one
        // entry against HEAD.
        write_file(&fixture.root, "both.txt", "staged\n");
        git_in(&fixture.root, &["add", "both.txt"]);
        write_file(&fixture.root, "both.txt", "staged and more\n");
        // Deleted from the working tree while still staged as modified.
        write_file(&fixture.root, "gone.txt", "staged delete\n");
        git_in(&fixture.root, &["add", "gone.txt"]);
        std::fs::remove_file(fixture.root.join("gone.txt")).unwrap();
        // Pure worktree modification.
        write_file(&fixture.root, "clean.txt", "unstaged\n");
        // Untracked addition.
        write_file(&fixture.root, "new.txt", "untracked\n");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);

        let both = find(&entries, "both.txt");
        assert_eq!(both.status, DiffStatus::Modified);
        assert_eq!(both.old_oid.as_deref(), Some(head_blob.as_str()));
        // Working-tree content is not a Git object.
        assert_eq!(both.new_oid, None);
        assert!(!both.binary);

        // Deleted from disk: one deletion against HEAD.
        let gone = find(&entries, "gone.txt");
        assert_eq!(gone.status, DiffStatus::Deleted);
        assert_eq!(gone.new_oid, None);

        assert_eq!(find(&entries, "clean.txt").status, DiffStatus::Modified);
        assert_eq!(find(&entries, "new.txt").status, DiffStatus::Added);
        assert_eq!(find(&entries, "new.txt").new_oid, None);

        // Exactly these four paths, sorted.
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["both.txt", "clean.txt", "gone.txt", "new.txt"]);
    }

    #[test]
    fn working_mode_never_reports_target_side_oids() {
        let fixture = Fixture::repo("diff-working-oids");
        write_file(&fixture.root, "a.txt", "head\n");
        commit_all(&fixture.root, "one");
        // A staged-new file (index blob exists) and a staged modification.
        write_file(&fixture.root, "a.txt", "staged\n");
        git_in(&fixture.root, &["add", "a.txt"]);
        write_file(&fixture.root, "staged-new.txt", "new\n");
        git_in(&fixture.root, &["add", "staged-new.txt"]);

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);
        assert!(
            entries.iter().all(|e| e.new_oid.is_none()),
            "working-tree content is not a Git object: {entries:?}"
        );
        // Base-side OIDs stay real HEAD blobs.
        assert!(find(&entries, "a.txt").old_oid.is_some());
    }

    #[test]
    fn working_mode_binary_untracked_addition_is_detected() {
        let fixture = Fixture::repo("diff-working-bin");
        write_file(&fixture.root, "text.txt", "plain\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "text.txt", "still plain\n");
        write_file(&fixture.root, "blob.bin", "binary\0data\n");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);
        assert!(find(&entries, "blob.bin").binary);
        assert!(!find(&entries, "text.txt").binary);
    }

    #[test]
    fn staged_mode_reviews_only_the_index() {
        let fixture = Fixture::repo("diff-staged");
        write_file(&fixture.root, "a.txt", "head\n");
        write_file(&fixture.root, "renamed-src.txt", "l1\nl2\nl3\nl4\nl5\n");
        commit_all(&fixture.root, "one");
        let head_a = blob_oid(&fixture.root, "HEAD:a.txt");

        // Staged: content change and a rename.
        write_file(&fixture.root, "a.txt", "staged\n");
        git_in(&fixture.root, &["add", "a.txt"]);
        git_in(&fixture.root, &["mv", "renamed-src.txt", "renamed-dst.txt"]);
        // Unstaged-only change must not appear.
        write_file(&fixture.root, "unrelated.txt", "created in worktree\n");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Staged);

        assert_eq!(
            entries.len(),
            2,
            "worktree-only files stay out: {entries:?}"
        );
        let a = find(&entries, "a.txt");
        assert_eq!(a.status, DiffStatus::Modified);
        assert_eq!(a.old_oid.as_deref(), Some(head_a.as_str()));
        assert!(a.new_oid.is_some(), "index content has a blob OID");

        let renamed = find(&entries, "renamed-dst.txt");
        assert_eq!(renamed.status, DiffStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("renamed-src.txt"));
        assert!(renamed.old_oid.is_some() && renamed.new_oid.is_some());
    }

    #[test]
    fn staged_mode_detects_binary_content() {
        let fixture = Fixture::repo("diff-staged-bin");
        write_file(&fixture.root, "keep.txt", "text\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "blob.bin", "\0\0\0bin\0");
        git_in(&fixture.root, &["add", "blob.bin"]);
        write_file(&fixture.root, "keep.txt", "still text\n");
        git_in(&fixture.root, &["add", "keep.txt"]);

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Staged);
        assert!(find(&entries, "blob.bin").binary);
        assert!(!find(&entries, "keep.txt").binary);
    }

    #[test]
    fn commit_mode_compares_against_first_parent() {
        let fixture = Fixture::repo("diff-commit");
        write_file(&fixture.root, "mod.txt", "one\n");
        write_file(&fixture.root, "del.txt", "delete me\n");
        write_file(&fixture.root, "old-name.txt", "l1\nl2\nl3\nl4\nl5\n");
        commit_all(&fixture.root, "one");
        let one_mod = blob_oid(&fixture.root, "HEAD:mod.txt");
        let one_old_name = blob_oid(&fixture.root, "HEAD:old-name.txt");

        write_file(&fixture.root, "mod.txt", "two\n");
        std::fs::remove_file(fixture.root.join("del.txt")).unwrap();
        write_file(&fixture.root, "add.txt", "added\n");
        git_in(&fixture.root, &["add", "--all"]);
        git_in(&fixture.root, &["mv", "old-name.txt", "new-name.txt"]);
        let two = commit_all(&fixture.root, "two");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(
            &ctx,
            &SnapshotSpec::Commit {
                revision: two.clone(),
            },
        );

        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["add.txt", "del.txt", "mod.txt", "new-name.txt"]);

        assert_eq!(find(&entries, "add.txt").status, DiffStatus::Added);
        assert_eq!(find(&entries, "del.txt").status, DiffStatus::Deleted);
        let modified = find(&entries, "mod.txt");
        assert_eq!(modified.status, DiffStatus::Modified);
        assert_eq!(modified.old_oid.as_deref(), Some(one_mod.as_str()));
        assert!(modified.new_oid.is_some());
        assert_ne!(modified.old_oid, modified.new_oid);

        let renamed = find(&entries, "new-name.txt");
        assert_eq!(renamed.status, DiffStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old-name.txt"));
        assert_eq!(renamed.old_oid.as_deref(), Some(one_old_name.as_str()));
    }

    #[test]
    fn commit_mode_root_compares_against_empty_tree() {
        let fixture = Fixture::repo("diff-commit-root");
        write_file(&fixture.root, "a.txt", "a\n");
        write_file(&fixture.root, "b/c.txt", "c\n");
        let root = commit_all(&fixture.root, "root");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(
            &ctx,
            &SnapshotSpec::Commit {
                revision: root.clone(),
            },
        );
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.status == DiffStatus::Added));
        assert!(entries.iter().all(|e| e.old_oid.is_none()));
        assert!(entries.iter().all(|e| e.new_oid.is_some()));
    }

    #[test]
    fn commit_mode_detects_binary_change() {
        let fixture = Fixture::repo("diff-commit-bin");
        write_file(&fixture.root, "blob.bin", "text\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "blob.bin", "\0binary\0");
        let two = commit_all(&fixture.root, "two");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Commit { revision: two });
        let entry = find(&entries, "blob.bin");
        assert_eq!(entry.status, DiffStatus::Modified);
        assert!(entry.binary);
    }

    #[test]
    fn range_mode_covers_all_changes_between_two_trees() {
        let fixture = Fixture::repo("diff-range");
        write_file(&fixture.root, "base.txt", "base\n");
        let base = commit_all(&fixture.root, "base");
        // Two commits of changes between base and HEAD.
        write_file(&fixture.root, "base.txt", "changed\n");
        write_file(&fixture.root, "mid.txt", "mid\n");
        commit_all(&fixture.root, "mid");
        write_file(&fixture.root, "last.txt", "last\n");
        std::fs::remove_file(fixture.root.join("mid.txt")).unwrap();
        git_in(&fixture.root, &["add", "--all"]);
        let head = commit_all(&fixture.root, "head");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(
            &ctx,
            &SnapshotSpec::Range {
                spec: format!("{base}..{head}"),
            },
        );
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["base.txt", "last.txt"]);
        assert_eq!(find(&entries, "base.txt").status, DiffStatus::Modified);
        assert_eq!(find(&entries, "last.txt").status, DiffStatus::Added);
    }

    #[test]
    fn range_mode_includes_renames() {
        let fixture = Fixture::repo("diff-range-rename");
        write_file(&fixture.root, "original.txt", "l1\nl2\nl3\nl4\nl5\n");
        let base = commit_all(&fixture.root, "base");
        git_in(&fixture.root, &["mv", "original.txt", "moved.txt"]);
        let head = commit_all(&fixture.root, "renamed");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(
            &ctx,
            &SnapshotSpec::Range {
                spec: format!("{base}..{head}"),
            },
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Renamed);
        assert_eq!(entries[0].old_path.as_deref(), Some("original.txt"));
        assert_eq!(entries[0].path, "moved.txt");
    }

    #[test]
    fn branch_mode_diffs_against_merge_base() {
        let fixture = Fixture::repo("diff-branch");
        write_file(&fixture.root, "common.txt", "common\n");
        let base = commit_all(&fixture.root, "base");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "topic"]);
        write_file(&fixture.root, "topic.txt", "topic work\n");
        write_file(&fixture.root, "common.txt", "changed on topic\n");
        let topic = commit_all(&fixture.root, "topic");
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "main-only.txt", "main work\n");
        let main_tip = commit_all(&fixture.root, "main2");

        let ctx = ctx_of(&fixture);
        // HEAD is main's tip; diffing against `topic` uses the merge base.
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Branch {
                base_ref: "topic".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(
            resolved.descriptor.head_commit.as_deref(),
            Some(main_tip.as_str())
        );
        let entries = snapshot_diff(&ctx, &resolved).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        // Topic-side changes are the base for comparison, so exactly the
        // main-side change appears.
        assert_eq!(paths, ["main-only.txt"]);
        assert_eq!(find(&entries, "main-only.txt").status, DiffStatus::Added);
        let _ = topic;

        // Diffing from main's tip uses the merge base and shows topic work.
        git_in(&fixture.root, &["checkout", "--quiet", "topic"]);
        let ctx = ctx_of(&fixture);
        let entries = diff_for(
            &ctx,
            &SnapshotSpec::Branch {
                base_ref: fixture.branch_name.clone(),
            },
        );
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["common.txt", "topic.txt"]);
        assert_eq!(find(&entries, "topic.txt").status, DiffStatus::Added);
        assert_eq!(find(&entries, "common.txt").status, DiffStatus::Modified);
        let _ = (topic, main_tip);
    }

    #[test]
    fn working_mode_on_unborn_head_adds_every_final_state_file() {
        let fixture = Fixture::repo("diff-unborn");
        write_file(&fixture.root, "staged.txt", "staged\n");
        git_in(&fixture.root, &["add", "staged.txt"]);
        // Staged but then deleted from disk: absent from the final state.
        write_file(&fixture.root, "vanished.txt", "x\n");
        git_in(&fixture.root, &["add", "vanished.txt"]);
        std::fs::remove_file(fixture.root.join("vanished.txt")).unwrap();
        write_file(&fixture.root, "untracked.txt", "untracked\n");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["staged.txt", "untracked.txt"]);
        assert!(entries.iter().all(|e| e.status == DiffStatus::Added));
        assert!(
            entries
                .iter()
                .all(|e| e.old_oid.is_none() && e.new_oid.is_none())
        );
    }

    #[cfg(unix)]
    #[test]
    fn typechange_file_to_symlink_is_reported() {
        let fixture = Fixture::repo("diff-typechange");
        write_file(&fixture.root, "shape.txt", "content\n");
        commit_all(&fixture.root, "one");
        std::fs::remove_file(fixture.root.join("shape.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", fixture.root.join("shape.txt")).unwrap();

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);
        let entry = find(&entries, "shape.txt");
        assert_eq!(entry.status, DiffStatus::TypeChanged);
    }

    #[test]
    fn nisse_paths_are_excluded_by_default() {
        let fixture = Fixture::repo("diff-exclusion");
        write_file(&fixture.root, "app.txt", "app\n");
        write_file(&fixture.root, ".nisse/common/keep.json", "{}\n");
        commit_all(&fixture.root, "one");

        // Staged addition under .nisse never appears.
        write_file(&fixture.root, ".nisse/people/dev/new.json", "{}\n");
        git_in(&fixture.root, &["add", ".nisse"]);
        // Application change appears.
        write_file(&fixture.root, "app.txt", "changed\n");
        git_in(&fixture.root, &["add", "app.txt"]);

        let ctx = ctx_of(&fixture);
        let staged = diff_for(&ctx, &SnapshotSpec::Staged);
        let paths: Vec<&str> = staged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["app.txt"]);

        // The same exclusion holds for the working snapshot.
        let working = diff_for(&ctx, &SnapshotSpec::Working);
        assert!(working.iter().all(|e| !e.path.starts_with(".nisse")));
    }

    #[test]
    fn renames_touching_nisse_reduce_to_the_application_view() {
        let fixture = Fixture::repo("diff-exclusion-rename");
        write_file(&fixture.root, "app.txt", "app\n");
        write_file(&fixture.root, ".nisse/common/note.json", "{}\n");
        commit_all(&fixture.root, "one");

        // Move application code into .nisse: the application view is a
        // deletion of app.txt.
        git_in(&fixture.root, &["mv", "app.txt", ".nisse/common/app.txt"]);
        let ctx = ctx_of(&fixture);
        let staged = diff_for(&ctx, &SnapshotSpec::Staged);
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].status, DiffStatus::Deleted);
        assert_eq!(staged[0].path, "app.txt");
        assert_eq!(staged[0].old_path, None);
        git_in(&fixture.root, &["reset", "--quiet", "--hard", "HEAD"]);

        // Move .nisse content out into application code: the application
        // view is an addition of note.txt, and no .nisse path appears.
        git_in(
            &fixture.root,
            &["mv", ".nisse/common/note.json", "note.txt"],
        );
        let ctx = ctx_of(&fixture);
        let staged = diff_for(&ctx, &SnapshotSpec::Staged);
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].status, DiffStatus::Added);
        assert_eq!(staged[0].path, "note.txt");
        assert_eq!(staged[0].old_path, None);
        assert_eq!(staged[0].old_oid, None);
    }

    #[test]
    fn diff_output_is_deterministic_across_runs() {
        let fixture = Fixture::repo("diff-determinism");
        write_file(&fixture.root, "a.txt", "a\n");
        write_file(&fixture.root, "b.txt", "b\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "b.txt", "b2\n");
        write_file(&fixture.root, "a.txt", "a2\n");
        write_file(&fixture.root, "c.txt", "c\n");

        let ctx = ctx_of(&fixture);
        let first = diff_for(&ctx, &SnapshotSpec::Working);
        let second = diff_for(&ctx, &SnapshotSpec::Working);
        assert_eq!(first, second);
        let paths: Vec<&str> = first.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn paths_with_spaces_and_unicode_parse_correctly() {
        let fixture = Fixture::repo("diff-names");
        write_file(&fixture.root, "with space.txt", "a\n");
        write_file(&fixture.root, "ünïcodé-文件.md", "b\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "with space.txt", "a2\n");
        write_file(&fixture.root, "ünïcodé-文件.md", "b2\n");

        let ctx = ctx_of(&fixture);
        let entries = diff_for(&ctx, &SnapshotSpec::Working);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"with space.txt"));
        assert!(paths.contains(&"ünïcodé-文件.md"));
    }

    /// Blob OID of a revision path like `HEAD:file`.
    fn blob_oid(cwd: &std::path::Path, rev_path: &str) -> String {
        crate::git::testutil::stdout_trimmed(git_in(cwd, &["rev-parse", rev_path]))
    }

    /// Linked worktrees with different same-path content must produce
    /// independent working/staged snapshots and share only commit-object
    /// history (plan sections 24–26).
    #[test]
    fn linked_worktrees_with_same_path_content_are_isolated() {
        use crate::git::reader::SnapshotReader;

        let fixture = Fixture::repo("diff-worktree");
        write_file(&fixture.root, "shared.txt", "base\n");
        write_file(&fixture.root, "only-main.txt", "main\n");
        let base = commit_all(&fixture.root, "base");

        let wt_path = fixture.sibling("wt");
        let wt_path = wt_path
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "-b", "topic"],
        );
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();

        // Both worktrees now diverge in their final state for the SAME
        // path `shared.txt`, and each has its own untracked file.
        write_file(&fixture.root, "shared.txt", "main dirty\n");
        write_file(&fixture.root, "main-untracked.txt", "m\n");
        write_file(&wt_root, "shared.txt", "topic dirty\n");
        write_file(&wt_root, "wt-untracked.txt", "w\n");

        let main_ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        assert_ne!(main_ctx.git_dir, wt_ctx.git_dir);
        assert_eq!(main_ctx.git_common_dir, wt_ctx.git_common_dir);

        // Working diffs are worktree-local: each side sees only its own
        // final state, never the other worktree's content.
        let main_entries = diff_for(&main_ctx, &SnapshotSpec::Working);
        let wt_entries = diff_for(&wt_ctx, &SnapshotSpec::Working);
        let main_paths: Vec<&str> = main_entries.iter().map(|e| e.path.as_str()).collect();
        let wt_paths: Vec<&str> = wt_entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(main_paths, ["main-untracked.txt", "shared.txt"]);
        assert_eq!(wt_paths, ["shared.txt", "wt-untracked.txt"]);

        // Readers resolve the same path to different content per worktree.
        let main_resolved = resolve_snapshot(&main_ctx, &SnapshotSpec::Working).unwrap();
        let wt_resolved = resolve_snapshot(&wt_ctx, &SnapshotSpec::Working).unwrap();
        let main_read = SnapshotReader::new(&main_ctx, &main_resolved).unwrap();
        let wt_read = SnapshotReader::new(&wt_ctx, &wt_resolved).unwrap();
        assert_eq!(
            main_read.read("shared.txt").unwrap().unwrap().bytes,
            b"main dirty\n"
        );
        assert_eq!(
            wt_read.read("shared.txt").unwrap().unwrap().bytes,
            b"topic dirty\n"
        );
        // The other worktree's untracked file does not exist here.
        assert_eq!(main_read.read("wt-untracked.txt").unwrap(), None);
        assert_eq!(wt_read.read("main-untracked.txt").unwrap(), None);

        // Staged snapshots are equally isolated.
        write_file(&wt_root, "shared.txt", "topic staged\n");
        git_in(&wt_root, &["add", "shared.txt"]);
        write_file(&wt_root, "shared.txt", "topic staged then dirtied\n");
        let wt_staged = diff_for(&wt_ctx, &SnapshotSpec::Staged);
        assert_eq!(wt_staged.len(), 1);
        assert_eq!(wt_staged[0].status, DiffStatus::Modified);
        let wt_staged_resolved = resolve_snapshot(&wt_ctx, &SnapshotSpec::Staged).unwrap();
        let wt_staged_read = SnapshotReader::new(&wt_ctx, &wt_staged_resolved).unwrap();
        assert_eq!(
            wt_staged_read.read("shared.txt").unwrap().unwrap().bytes,
            b"topic staged\n"
        );
        // Main's index still holds the base content.
        let main_staged = diff_for(&main_ctx, &SnapshotSpec::Staged);
        assert!(main_staged.is_empty(), "main staged: {main_staged:?}");

        // Historical (commit) snapshots resolve identically from either
        // worktree: objects are shared, filesystem state is irrelevant.
        let wt_commit = diff_for(
            &wt_ctx,
            &SnapshotSpec::Commit {
                revision: base.clone(),
            },
        );
        let main_commit = diff_for(&main_ctx, &SnapshotSpec::Commit { revision: base });
        assert_eq!(wt_commit, main_commit);
        assert_eq!(wt_commit.len(), 2);
        assert!(wt_commit.iter().all(|e| e.status == DiffStatus::Added));

        // Branch snapshots work inside a linked worktree: commit the topic
        // work there, then diff against the main branch. The merge base is
        // the original base commit. The context is re-discovered because
        // HEAD moved with the new commit.
        let topic_tip = commit_all(&wt_root, "topic work");
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        let wt_branch = diff_for(
            &wt_ctx,
            &SnapshotSpec::Branch {
                base_ref: fixture.branch_name.clone(),
            },
        );
        let paths: Vec<&str> = wt_branch.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["shared.txt", "wt-untracked.txt"]);
        assert_eq!(find(&wt_branch, "shared.txt").status, DiffStatus::Modified);
        assert_eq!(
            find(&wt_branch, "wt-untracked.txt").status,
            DiffStatus::Added
        );
        assert_eq!(
            resolve_snapshot(
                &wt_ctx,
                &SnapshotSpec::Branch {
                    base_ref: fixture.branch_name.clone()
                }
            )
            .unwrap()
            .descriptor
            .head_commit
            .as_deref(),
            Some(topic_tip.as_str())
        );
    }

    fn patches_for(
        ctx: &RepoContext,
        spec: &SnapshotSpec,
        per_file_budget: usize,
    ) -> Vec<SnapshotPatch> {
        let resolved = resolve_snapshot(ctx, spec).unwrap();
        let entries = snapshot_diff(ctx, &resolved).unwrap();
        snapshot_patches(ctx, &resolved, &entries, 16, per_file_budget).unwrap()
    }

    fn patch_of<'a>(patches: &'a [SnapshotPatch], path: &str) -> &'a SnapshotPatch {
        patches
            .iter()
            .find(|patch| patch.path == path)
            .unwrap_or_else(|| panic!("no patch for `{path}` in {patches:?}"))
    }

    #[test]
    fn renamed_paths_patch_with_both_sides_of_the_pair() {
        let fixture = Fixture::repo("patch-rename");
        write_file(&fixture.root, "old.txt", "l1\nl2\nl3\nl4\nl5\n");
        commit_all(&fixture.root, "one");
        git_in(&fixture.root, &["mv", "old.txt", "new.txt"]);
        write_file(&fixture.root, "new.txt", "l1\nl2-mod\nl3\nl4\nl5\n");
        git_in(&fixture.root, &["add", "new.txt"]);

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Staged, 16 * 1024);
        assert_eq!(patches.len(), 1, "one renamed entry: {patches:?}");
        let patch = &patches[0];
        assert_eq!(patch.status, DiffStatus::Renamed);
        assert_eq!(patch.path, "new.txt");
        let text = patch.patch.as_deref().expect("textual patch");
        // The patch shows the rename, not a degraded plain addition.
        assert!(text.contains("rename from old.txt"), "{text}");
        assert!(text.contains("rename to new.txt"), "{text}");
        assert!(text.contains("-l2"), "{text}");
        assert!(text.contains("+l2-mod"), "{text}");
    }

    #[test]
    fn working_patches_cover_modified_and_untracked_paths() {
        let fixture = Fixture::repo("patch-working");
        write_file(
            &fixture.root,
            "src/mod.rs",
            "fn one() {
    1
}
",
        );
        commit_all(&fixture.root, "one");
        write_file(
            &fixture.root,
            "src/mod.rs",
            "fn one() {
    2
}
",
        );
        write_file(
            &fixture.root,
            "src/new.rs",
            "pub fn fresh() {}
",
        );
        // A staged-new file is part of the final state through the
        // index: git patches it (not the untracked synthesis).
        write_file(
            &fixture.root,
            "src/staged-new.rs",
            "pub fn staged() {}
",
        );
        git_in(&fixture.root, &["add", "src/staged-new.rs"]);

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Working, 16 * 1024);

        let modified = patch_of(&patches, "src/mod.rs");
        assert_eq!(modified.status, DiffStatus::Modified);
        assert!(!modified.truncated);
        let text = modified.patch.as_deref().expect("textual patch");
        assert!(text.contains("--- a/src/mod.rs"), "{text}");
        assert!(text.contains("+++ b/src/mod.rs"), "{text}");
        assert!(text.contains("-    1"), "{text}");
        assert!(text.contains("+    2"), "{text}");

        let staged = patch_of(&patches, "src/staged-new.rs");
        let text = staged.patch.as_deref().expect("textual patch");
        assert!(text.contains("new file mode"), "{text}");
        assert!(text.contains("+pub fn staged()"), "{text}");

        // Untracked additions never appear in `git diff` output; their
        // added-file patch is synthesized from the target snapshot.
        let added = patch_of(&patches, "src/new.rs");
        assert_eq!(added.status, DiffStatus::Added);
        let text = added.patch.as_deref().expect("textual patch");
        assert!(text.contains("--- /dev/null"), "{text}");
        assert!(text.contains("+++ b/src/new.rs"), "{text}");
        assert!(text.contains("@@ -0,0 +1 @@"), "{text}");
        assert!(text.contains("+pub fn fresh() {}"), "{text}");
        assert!(!text.contains("No newline"), "{text}");
    }

    #[test]
    fn untracked_patch_without_final_newline_is_marked() {
        let fixture = Fixture::repo("patch-untracked-nonl");
        write_file(
            &fixture.root,
            "keep.txt",
            "keep
",
        );
        commit_all(&fixture.root, "one");
        write_file(
            &fixture.root,
            "no-newline.txt",
            "first
second",
        );

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Working, 16 * 1024);
        let text = patch_of(&patches, "no-newline.txt")
            .patch
            .as_deref()
            .expect("textual patch");
        assert!(text.contains("@@ -0,0 +1,2 @@"), "{text}");
        assert!(
            text.contains("+second\n\\ No newline at end of file"),
            "{text}"
        );

        // An empty untracked file has headers only.
        write_file(&fixture.root, "empty.txt", "");
        let patches = patches_for(&ctx, &SnapshotSpec::Working, 16 * 1024);
        let text = patch_of(&patches, "empty.txt")
            .patch
            .as_deref()
            .expect("textual patch");
        assert!(text.contains("+++ b/empty.txt"), "{text}");
        assert!(!text.contains("@@"), "{text}");
    }

    #[test]
    fn staged_patches_review_only_the_index() {
        let fixture = Fixture::repo("patch-staged");
        write_file(
            &fixture.root,
            "a.txt",
            "committed
",
        );
        commit_all(&fixture.root, "one");
        write_file(
            &fixture.root,
            "a.txt",
            "staged
",
        );
        git_in(&fixture.root, &["add", "a.txt"]);
        write_file(
            &fixture.root,
            "a.txt",
            "staged then dirtied
",
        );
        write_file(
            &fixture.root,
            "worktree-only.txt",
            "untracked
",
        );

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Staged, 16 * 1024);
        assert_eq!(patches.len(), 1, "only the staged change: {patches:?}");
        let text = patches[0].patch.as_deref().expect("textual patch");
        assert!(text.contains("+staged"), "{text}");
        assert!(!text.contains("dirtied"), "{text}");
    }

    #[test]
    fn commit_patches_use_the_first_parent_and_the_empty_tree_for_roots() {
        let fixture = Fixture::repo("patch-commit");
        write_file(
            &fixture.root,
            "a.txt",
            "one
",
        );
        let root = commit_all(&fixture.root, "root");
        write_file(
            &fixture.root,
            "a.txt",
            "two
",
        );
        write_file(
            &fixture.root,
            "b.txt",
            "added
",
        );
        git_in(&fixture.root, &["add", "--all"]);
        let second = commit_all(&fixture.root, "second");

        let ctx = ctx_of(&fixture);

        // Root commit: everything is an addition against the empty tree.
        let patches = patches_for(
            &ctx,
            &SnapshotSpec::Commit {
                revision: root.clone(),
            },
            16 * 1024,
        );
        assert_eq!(patches.len(), 1);
        let text = patches[0].patch.as_deref().expect("textual patch");
        assert!(text.contains("+one"), "{text}");

        // Historical commit: the working tree may have moved on; the
        // patch contains exactly the committed change.
        write_file(
            &fixture.root,
            "a.txt",
            "much later content
",
        );
        let patches = patches_for(
            &ctx,
            &SnapshotSpec::Commit {
                revision: second.clone(),
            },
            16 * 1024,
        );
        let paths: Vec<&str> = patches.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"]);
        let text = patch_of(&patches, "a.txt")
            .patch
            .as_deref()
            .expect("textual patch");
        assert!(text.contains("-one"), "{text}");
        assert!(text.contains("+two"), "{text}");
        assert!(!text.contains("much later"), "{text}");
    }

    #[test]
    fn range_patches_compare_the_two_named_trees() {
        let fixture = Fixture::repo("patch-range");
        write_file(
            &fixture.root,
            "a.txt",
            "base
",
        );
        let base = commit_all(&fixture.root, "base");
        write_file(
            &fixture.root,
            "a.txt",
            "changed
",
        );
        let head = commit_all(&fixture.root, "head");

        let ctx = ctx_of(&fixture);
        let patches = patches_for(
            &ctx,
            &SnapshotSpec::Range {
                spec: format!("{base}..{head}"),
            },
            16 * 1024,
        );
        let text = patch_of(&patches, "a.txt")
            .patch
            .as_deref()
            .expect("textual patch");
        assert!(text.contains("-base"), "{text}");
        assert!(text.contains("+changed"), "{text}");
    }

    #[test]
    fn binary_and_truncated_patches_are_bounded() {
        let fixture = Fixture::repo("patch-bounds");
        write_file(
            &fixture.root,
            "small.txt",
            "x
",
        );
        commit_all(&fixture.root, "one");
        write_file(
            &fixture.root,
            "small.txt",
            "y
",
        );
        write_file(&fixture.root, "blob.bin", "\0binary\0");
        write_file(&fixture.root, "big.txt", &"line\n".repeat(2000));

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Working, 512);

        let binary = patch_of(&patches, "blob.bin");
        assert!(binary.binary);
        assert_eq!(binary.patch, None);

        let big = patch_of(&patches, "big.txt");
        assert!(big.truncated);
        let text = big.patch.as_deref().expect("textual patch");
        // The marker grew: it now names the file and the retrieval tool.
        assert!(text.len() < 700, "{}", text.len());
        assert!(text.contains("diff truncated after 512 bytes"), "{text}");
        assert!(
            text.contains("verify the omitted region via source_lookup (path=big.txt)"),
            "{text}"
        );

        // The entry cap bounds how many paths are patched.
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let entries = snapshot_diff(&ctx, &resolved).unwrap();
        let capped = snapshot_patches(&ctx, &resolved, &entries, 1, 4096).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].path, "big.txt", "entries stay path-sorted");
    }

    #[test]
    fn unborn_working_patches_synthesize_every_final_state_file() {
        let fixture = Fixture::repo("patch-unborn");
        write_file(
            &fixture.root,
            "staged.txt",
            "staged content
",
        );
        git_in(&fixture.root, &["add", "staged.txt"]);
        write_file(&fixture.root, "untracked.txt", "untracked content");

        let ctx = ctx_of(&fixture);
        let patches = patches_for(&ctx, &SnapshotSpec::Working, 16 * 1024);
        let paths: Vec<&str> = patches.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, ["staged.txt", "untracked.txt"]);
        for patch in &patches {
            let text = patch.patch.as_deref().expect("textual patch");
            assert!(text.contains("--- /dev/null"), "{text}");
        }
        assert!(
            patch_of(&patches, "staged.txt")
                .patch
                .as_deref()
                .unwrap()
                .contains("+staged content")
        );
    }

    // -----------------------------------------------------------------
    // Milestone 28: malformed Git output fails closed with typed
    // errors instead of being silently misread.
    // -----------------------------------------------------------------

    #[test]
    fn parse_raw_entries_rejects_malformed_output() {
        // Not a raw entry at all.
        for stdout in [
            "garbage\0",
            "::100644 100644 0000 0000 M\0a\0", // combined/unmerged
            ":100644 100644 abc def\0a\0",      // wrong field count
            ":100644 100644 0000 0000 X\0a\0",  // unknown status
            ":100644 100644 0000000 0000000 A\0", // ends before path
            ":100644 100644 0000000 0000000 R\0a\0", // rename without dst
            ":100644 100644 0000000 0000000 A\0\0", // empty path
        ] {
            let err = parse_raw_entries(stdout).unwrap_err();
            assert!(
                matches!(err, GitError::UnexpectedOutput { .. }),
                "expected a typed failure for {stdout:?}, got {err:?}"
            );
        }

        // A non-zero OID that is not a valid OID is rejected.
        let err = parse_raw_entries(":100644 100644 zz 0000000 A\0a\0").unwrap_err();
        assert!(matches!(err, GitError::InvalidOid { .. }), "{err:?}");

        // Well-formed entries still parse.
        let entries = parse_raw_entries(
            ":100644 100644 0000000000000000000000000000000000000000 \
             a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2 A\0src/new.rs\0",
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/new.rs");
        assert!(entries[0].old_oid.is_none());
        assert_eq!(
            entries[0].new_oid.as_deref(),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2")
        );
    }

    #[test]
    fn parse_numstat_binary_rejects_malformed_output() {
        for stdout in [
            "3\0",                // missing fields
            "3\t1\0",             // missing path
            "x\t1\ta\0",          // invalid line count
            "3\ty\ta\0",          // invalid line count
            "3\t1\t\0only-src\0", // rename ends before destination
        ] {
            let err = parse_numstat_binary(stdout).unwrap_err();
            assert!(
                matches!(err, GitError::UnexpectedOutput { .. }),
                "expected a typed failure for {stdout:?}, got {err:?}"
            );
        }

        // Well-formed plain and rename entries parse.
        let map = parse_numstat_binary("3\t1\tsrc/a.rs\0-\t-\t\0old.rs\0new.rs\0").unwrap();
        assert_eq!(map.get("src/a.rs"), Some(&false));
        assert_eq!(map.get("new.rs"), Some(&true));
    }
}

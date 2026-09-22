//! Inventory guard: directory creation is centralized in the three
//! consent-gated helpers (user-requested plan change).
//!
//! Scans `src/` for `create_dir`/`create_dir_all` call sites and asserts
//! they appear only in:
//!
//! 1. `src/storage/records.rs` — `RecordStore::ensure_real_dir` (gate 1,
//!    the durable-record tree walker);
//! 2. `src/storage/db.rs` — `ensure_parent` (gate 2, the cache
//!    directories);
//! 3. `src/config/identity.rs` — `ensure_config_dir` (gate 3, the user
//!    config directory);
//!
//! plus exempt test-support code (`src/git/testutil.rs`,
//! `src/retrieval/fixtures.rs`) and in-file `#[cfg(test)]` modules.

use std::path::PathBuf;

/// The only non-test source files permitted to create directories.
const GATED_FILES: [&str; 3] = [
    "src/storage/records.rs",
    "src/storage/db.rs",
    "src/config/identity.rs",
];

/// Whole-file test-support exemptions.
const TEST_SUPPORT_FILES: [&str; 2] = ["src/git/testutil.rs", "src/retrieval/fixtures.rs"];

#[test]
fn directory_creation_sites_are_only_the_gated_helpers() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("src");
    let mut offenders: Vec<String> = Vec::new();

    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("read src");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(&manifest)
                .expect("path below manifest")
                .to_string_lossy()
                .into_owned();
            if !relative.ends_with(".rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            // Cut in-file test modules (they are trailing blocks).
            let production = match text.find("#[cfg(test)]") {
                Some(index) => &text[..index],
                None => &text[..],
            };
            let creates = production
                .lines()
                .filter(|line| {
                    line.contains("create_dir_all(")
                        || line.contains("create_dir(")
                        || line.contains("DirBuilder")
                })
                .count();
            if creates == 0 {
                continue;
            }
            let gated = GATED_FILES.contains(&relative.as_str());
            let test_support = TEST_SUPPORT_FILES.contains(&relative.as_str());
            if !gated && !test_support {
                offenders.push(format!("{relative}: {creates} creation call(s)"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "directory creation outside the gated helpers:\n{}",
        offenders.join("\n")
    );
}

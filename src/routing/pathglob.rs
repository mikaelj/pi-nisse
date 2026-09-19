//! Deterministic path-glob matching for reviewer path hints and lesson
//! routing hints.
//!
//! Supported pattern syntax (a strict subset of gitignore-style globs
//! used by the frozen reviewer assets):
//!
//! - `*` matches any characters except `/` within one path segment;
//! - `**` matches zero or more whole path segments — except that a
//!   trailing `**` requires at least one segment, so `dir/**` means
//!   strictly everything inside `dir`;
//! - all other characters match literally; matching is case-sensitive.
//!
//! Patterns and paths use `/` separators and are interpreted relative
//! to the repository root.

/// Whether a repository-relative path matches a path-glob pattern.
///
/// Empty patterns and empty paths never match.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if pattern.is_empty() || path.is_empty() {
        return false;
    }
    segments_match(&pattern, &path)
}

/// Match pattern segments against path segments.
fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if *first == "**" {
        // A `**` segment matches zero or more path segments; a trailing
        // `**` requires at least one so that `dir/**` means strictly
        // inside `dir`.
        let minimum = if rest.is_empty() { 1 } else { 0 };
        if path.len() < minimum {
            return false;
        }
        return (minimum..=path.len()).any(|taken| segments_match(rest, &path[taken..]));
    }
    match path.split_first() {
        Some((segment, path_rest)) if segment_matches(first, segment) => {
            segments_match(rest, path_rest)
        }
        _ => false,
    }
}

/// Match one pattern segment against one path segment; `*` matches any
/// run of characters within the segment.
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(star_position) = star {
            p = star_position + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_double_star_matches_any_depth_including_root() {
        assert!(glob_match("**/migrations/**", "db/migrations/0001.sql"));
        assert!(glob_match("**/migrations/**", "migrations/0001.sql"));
        assert!(glob_match("**/*.sql", "schema.sql"));
        assert!(glob_match("**/*.sql", "deep/nested/dir/a.sql"));
        assert!(glob_match("**/CMakeLists.txt", "CMakeLists.txt"));
        assert!(!glob_match("**/*.sql", "src/main.rs"));
    }

    #[test]
    fn trailing_double_star_requires_at_least_one_segment() {
        assert!(glob_match("tests/**", "tests/a.rs"));
        assert!(glob_match("tests/**", "tests/nested/a.rs"));
        assert!(!glob_match("tests/**", "tests"));
        assert!(!glob_match("tests/**", "src/tests/a.rs"));
    }

    #[test]
    fn middle_double_star_may_match_zero_segments() {
        assert!(glob_match("a/**/b", "a/b"));
        assert!(glob_match("a/**/b", "a/x/b"));
        assert!(glob_match("a/**/b", "a/x/y/b"));
        assert!(!glob_match("a/**/b", "x/a/b"));
        assert!(!glob_match("a/**/b", "a/x/c"));
    }

    #[test]
    fn within_segment_star_matches_partial_names() {
        assert!(glob_match("**/login*", "src/auth/login.rs"));
        assert!(glob_match("**/login*", "src/auth/login"));
        assert!(!glob_match("**/login*", "src/auth/oauth.rs"));
        assert!(glob_match("**/Makefile*", "Makefile.mac"));
        assert!(glob_match("**/Makefile*", "build/Makefile"));
        assert!(glob_match("**/*_test.*", "src/foo_test.rs"));
        assert!(glob_match("**/*.test.*", "src/foo.test.js"));
        assert!(!glob_match("**/*_test.*", "src/foo_tests.rs"));
    }

    #[test]
    fn matching_is_case_sensitive_and_rejects_empty() {
        assert!(!glob_match("**/db/**", "DB/migrate.sql"));
        assert!(!glob_match("", "a/b"));
        assert!(!glob_match("a/**", ""));
        assert!(glob_match("**", "anything/at/all"));
    }

    #[test]
    fn patterns_used_by_the_frozen_assets() {
        for (pattern, path) in [
            ("**/auth/**", "src/auth/oauth.rs"),
            ("**/crypto/**", "lib/crypto/aes.c"),
            ("**/testdata/**", "tests/testdata/input.bin"),
            ("**/*.ld", "firmware/stm32.ld"),
            ("**/*.ioc", "board.ioc"),
            ("**/protocol/**", "src/protocol/frame.rs"),
            ("**/net/**", "src/net/socket.rs"),
            ("**/storage/**", "src/storage/keys.rs"),
            ("**/include/**", "include/header.h"),
            ("**/__tests__/**", "src/__tests__/a.ts"),
        ] {
            assert!(glob_match(pattern, path), "{pattern} !~ {path}");
        }
    }
}

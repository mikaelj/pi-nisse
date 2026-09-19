//! Shared shebang-based language recognition for extensionless files.
//!
//! Extension-based detection ([`Language::from_path`]) always wins; the
//! helpers here exist for files whose name carries no extension at all
//! (`gradlew`, `run`, ...). Recognition is deliberately bounded and
//! deterministic:
//!
//! - only the bounded beginning of the file is examined (the caller
//!   passes a prefix; [`PREFIX_LIMIT`] is the recommended bound);
//! - only the first line is considered;
//! - only well-known interpreter names are matched — no shell parsing,
//!   no arbitrary argument analysis;
//! - a recognized extension never consults content, so contradictory
//!   extension/shebang combinations always resolve to the extension.
//!
//! POSIX reserves `#!/` for exactly this purpose, and the recognized
//! interpreter set covers the common Python, Bash/sh, and Lua forms
//! (including `/usr/bin/env`, `env -S`, and versioned interpreter names).

use super::language::Language;

/// How many bytes of a file to read before deciding whether it starts
/// with a recognized shebang.
///
/// Real interpreter lines are far shorter than this; the bound only
/// exists so pathological files cannot force unbounded reads.
pub const PREFIX_LIMIT: usize = 512;

/// Whether a repository-relative path may be shebang-classified at all.
///
/// Only files whose name contains no `.` are considered extensionless.
/// Files with any extension — recognized or not — are never sniffed, and
/// dotfiles such as `.gitignore` count as having an extension here.
pub fn is_extensionless(path: &str) -> bool {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    !file_name.contains('.')
}

/// Classify bounded file prefix bytes by their shebang, if any.
///
/// Returns `None` unless the prefix starts with `#!` and the first line
/// names a recognized interpreter (Python, bash/sh, or Lua forms).
pub fn language_from_prefix(prefix: &[u8]) -> Option<Language> {
    let first_line: &[u8] = match prefix.iter().position(|&b| b == b'\n') {
        Some(end) => &prefix[..end],
        None => prefix,
    };
    let line = std::str::from_utf8(first_line.trim_ascii()).ok()?;
    let rest = line.strip_prefix("#!")?;
    let interpreter = interpreter_token(rest)?;
    match_interpreter(interpreter)
}

/// The interpreter token of a shebang line body (everything after `#!`).
///
/// Handles the direct form (`/usr/bin/python3`) and the `env` form
/// (`/usr/bin/env python3`, `/usr/bin/env -S python3 -u`), returning the
/// interpreter's basename. Bounded: a body without any plausible
/// interpreter token yields `None`.
fn interpreter_token(body: &str) -> Option<&str> {
    let mut tokens = body.split_whitespace();
    let mut token = tokens.next()?;
    // Only absolute interpreter paths are shebang-legal in the direct
    // form; the basename is what identifies the interpreter.
    if !token.starts_with('/') {
        return None;
    }
    if token.rsplit('/').next() == Some("env") {
        // `/usr/bin/env [-flags] interpreter ...`: skip the flags; the
        // interpreter itself need not be a path.
        token = loop {
            let next = tokens.next()?;
            if !next.starts_with('-') {
                break next;
            }
        };
    }
    Some(token.rsplit('/').next().unwrap_or(token))
}

/// Match an interpreter basename against the recognized set.
///
/// Table order is fixed so matching is deterministic. Versioned forms
/// (`python3.11`, `lua5.4`) are accepted; unrelated interpreters
/// (`ruby`, `awk`, ...) are not.
fn match_interpreter(basename: &str) -> Option<Language> {
    // Python: python, python2, python3, python3.11, ...
    if basename == "python" || basename.starts_with("python2") || basename.starts_with("python3") {
        return Some(Language::Python);
    }
    // Bash — including plain POSIX sh, which indexes as bash (there is
    // deliberately no separate POSIX-sh language).
    if basename == "bash" || basename == "sh" {
        return Some(Language::Bash);
    }
    // Lua: lua, lua5, lua5.4, ...
    if basename == "lua" || basename.starts_with("lua5") {
        return Some(Language::Lua);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(line: &str) -> Option<Language> {
        language_from_prefix(line.as_bytes())
    }

    #[test]
    fn recognizes_python_shebang_forms() {
        for line in [
            "#!/usr/bin/python\nrest of file",
            "#!/usr/bin/python3\n",
            "#!/usr/bin/python3.11 -u\n",
            "#!/usr/bin/env python\n",
            "#!/usr/bin/env python3\n",
            "#!/usr/bin/env -S python3 -u\n",
            "#!/usr/bin/env --split-string python -B\n",
            "#! /usr/bin/python\n",
        ] {
            assert_eq!(detect(line), Some(Language::Python), "line: {line}");
        }
    }

    #[test]
    fn recognizes_bash_and_sh_shebang_forms() {
        for line in [
            "#!/bin/bash\n",
            "#!/usr/bin/bash -e\n",
            "#!/bin/sh\n",
            "#!/usr/bin/sh\n",
            "#!/usr/bin/env bash\n",
            "#!/usr/bin/env sh\n",
            "#!/usr/bin/env -S bash -euo pipefail\n",
        ] {
            assert_eq!(detect(line), Some(Language::Bash), "line: {line}");
        }
    }

    #[test]
    fn recognizes_lua_shebang_forms() {
        for line in [
            "#!/usr/bin/lua\n",
            "#!/usr/bin/lua5.4\n",
            "#!/usr/bin/env lua\n",
            "#!/usr/bin/env lua5.4 script.lua\n",
        ] {
            assert_eq!(detect(line), Some(Language::Lua), "line: {line}");
        }
    }

    #[test]
    fn rejects_non_shebang_and_unknown_interpreters() {
        for line in [
            "#!/usr/bin/ruby\n",
            "#!/usr/bin/awk -f\n",
            "#!/usr/bin/env node\n",
            "#!/usr/bin/env -S pnpm dlx\n",
            "#comment\n",
            "// not a shebang\n",
            "\n#!/usr/bin/python\n",
            "#!",
            "#!python\n",
            "#! /usr/bin/env\n",
            "#! /usr/bin/env -S\n",
            "",
        ] {
            assert_eq!(detect(line), None, "line: {line}");
        }
    }

    #[test]
    fn detection_is_bounded_to_the_first_line() {
        // A later shebang-looking line never classifies the file.
        let prefix = b"# -*- coding: utf-8 -*-\n#!/usr/bin/env python\n";
        assert_eq!(language_from_prefix(prefix), None);
        // Truncation mid-first-line works when the interpreter token
        // already ended, and degrades to None when it was cut in half.
        assert_eq!(
            language_from_prefix(b"#!/usr/bin/env python3 -u extra fla"),
            Some(Language::Python)
        );
        assert_eq!(language_from_prefix(b"#!/usr/bin/env pyth"), None);
        assert_eq!(
            language_from_prefix(b"#!/usr/bin/env python3\ntruncated body"),
            Some(Language::Python)
        );
    }

    #[test]
    fn crlf_first_line_is_handled() {
        assert_eq!(detect("#!/usr/bin/env bash\r\n"), Some(Language::Bash));
    }

    #[test]
    fn extensionless_paths_are_the_sniffable_ones() {
        for path in ["gradlew", "scripts/run", "deep/nested/tool"] {
            assert!(is_extensionless(path), "{path}");
        }
        for path in [
            "README.md",
            "script.sh",
            "archive.tar.gz",
            ".gitignore",
            ".env",
        ] {
            assert!(!is_extensionless(path), "{path}");
        }
        // Only the file name matters, not the directories.
        assert!(is_extensionless("dir.d/file"));
    }

    #[test]
    fn prefix_limit_covers_real_shebangs() {
        let longest = "#!/usr/bin/env --split-string python3 -X utf8 -B \n";
        assert!(longest.len() < PREFIX_LIMIT);
    }
}

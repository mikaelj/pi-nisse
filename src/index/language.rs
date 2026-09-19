//! Source language classification for the content-addressed index.
//!
//! Nisse indexes eleven languages: C, C++, C#, Dart, Rust, JavaScript,
//! TypeScript, Python, Bash, Lua, and HTML. Detection is a deterministic
//! function of the repository-relative path's file extension — files
//! without a recognized extension are only indexed when shebang
//! recognition identifies them (see [`super::shebang`]; the first four
//! extension groups below never need content sniffing).
//!
//! Deliberate disambiguation rules:
//!
//! - `.h` headers are indexed as C; C++-only header extensions (`.hpp`,
//!   `.hh`, ...) are indexed as C++.
//! - `.jsx` is JavaScript (the grammar includes JSX syntax); `.tsx` is
//!   TypeScript.
//! - `.pyi` stub files are Python; `.sh` and `.bash` are both Bash (a
//!   POSIX `sh` shebang also indexes as `bash` — there is no separate
//!   POSIX-sh language).
//! - Extension matching is ASCII case-insensitive, so `LIB.RS` and
//!   `lib.rs` classify identically.
//! - A recognized extension always wins over contradictory content:
//!   classification by extension never reads the file.

use serde::{Deserialize, Serialize};

/// An indexed source language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// C (`.c`, `.h`).
    C,
    /// C++ (`.cpp`, `.cc`, `.cxx`, `.c++`, `.hpp`, `.hh`, `.hxx`, `.h++`,
    /// `.inl`, `.ipp`, `.tpp`).
    Cpp,
    /// C# (`.cs`).
    CSharp,
    /// Dart (`.dart`).
    Dart,
    /// Rust (`.rs`).
    Rust,
    /// JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`).
    JavaScript,
    /// TypeScript (`.ts`, `.mts`, `.cts`, `.tsx`).
    TypeScript,
    /// Python (`.py`, `.pyi`; extensionless files with a Python
    /// shebang).
    Python,
    /// Bash (`.sh`, `.bash`; extensionless files with a `bash` or `sh`
    /// shebang).
    Bash,
    /// Lua (`.lua`; extensionless files with a Lua shebang).
    Lua,
    /// HTML (`.html`, `.htm`).
    Html,
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl Language {
    /// Stable label used in storage (the `language` column of parsed
    /// artifacts and the serde representation).
    pub fn label(self) -> &'static str {
        match self {
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Dart => "dart",
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Python => "python",
            Language::Bash => "bash",
            Language::Lua => "lua",
            Language::Html => "html",
        }
    }

    /// Classify a repository-relative path by extension.
    ///
    /// Returns `None` for paths without a recognized extension, including
    /// paths without any extension at all. Extensionless files are instead
    /// classified through shebang recognition (see [`super::shebang`]).
    pub fn from_path(path: &str) -> Option<Self> {
        let extension = path.rsplit_once('.')?.1;
        let lowered = extension.to_ascii_lowercase();
        let language = match lowered.as_str() {
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "inl" | "ipp" | "tpp" => {
                Language::Cpp
            }
            "cs" => Language::CSharp,
            "dart" => Language::Dart,
            "rs" => Language::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" | "tsx" => Language::TypeScript,
            "py" | "pyi" => Language::Python,
            "sh" | "bash" => Language::Bash,
            "lua" => Language::Lua,
            "html" | "htm" => Language::Html,
            _ => return None,
        };
        Some(language)
    }

    /// Every indexed language, in declaration order.
    pub fn all() -> [Language; 11] {
        [
            Language::C,
            Language::Cpp,
            Language::CSharp,
            Language::Dart,
            Language::Rust,
            Language::JavaScript,
            Language::TypeScript,
            Language::Python,
            Language::Bash,
            Language::Lua,
            Language::Html,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_all_supported_extensions() {
        let cases = [
            ("a.c", Language::C),
            ("inc/header.h", Language::C),
            ("src/lib.cpp", Language::Cpp),
            ("src/lib.cc", Language::Cpp),
            ("src/lib.cxx", Language::Cpp),
            ("src/lib.hpp", Language::Cpp),
            ("src/lib.hh", Language::Cpp),
            ("src/lib.inl", Language::Cpp),
            ("src/lib.ipp", Language::Cpp),
            ("src/lib.tpp", Language::Cpp),
            ("Program.cs", Language::CSharp),
            ("lib/main.dart", Language::Dart),
            ("src/lib.rs", Language::Rust),
            ("mod.rs", Language::Rust),
            ("index.js", Language::JavaScript),
            ("module.mjs", Language::JavaScript),
            ("common.cjs", Language::JavaScript),
            ("component.jsx", Language::JavaScript),
            ("types.ts", Language::TypeScript),
            ("module.mts", Language::TypeScript),
            ("module.cts", Language::TypeScript),
            ("component.tsx", Language::TypeScript),
            ("app/main.py", Language::Python),
            ("stubs/protocol.pyi", Language::Python),
            ("scripts/run.sh", Language::Bash),
            ("scripts/run.bash", Language::Bash),
            ("legacy/Setup.PY", Language::Python),
            ("page/INDEX.HTML", Language::Html),
            ("page/about.htm", Language::Html),
            ("mod/init.lua", Language::Lua),
        ];
        for (path, expected) in cases {
            assert_eq!(
                Language::from_path(path),
                Some(expected),
                "wrong classification for {path}"
            );
        }
    }

    #[test]
    fn classification_is_ascii_case_insensitive() {
        assert_eq!(Language::from_path("LIB.RS"), Some(Language::Rust));
        assert_eq!(Language::from_path("App.CS"), Some(Language::CSharp));
        assert_eq!(Language::from_path("TOOL.SH"), Some(Language::Bash));
    }

    #[test]
    fn unrecognized_paths_are_none() {
        for path in [
            "README.md",
            "data.json",
            "Cargo.toml",
            "Makefile",
            "no-extension",
            ".gitignore",
            "archive.tar.gz",
            "template.erb",
            "config.xml",
            "Component.vue",
        ] {
            assert_eq!(
                Language::from_path(path),
                None,
                "{path} must not be indexed"
            );
        }
    }

    #[test]
    fn labels_match_serde_names() {
        for language in Language::all() {
            let json = serde_json::to_string(&language).unwrap();
            assert_eq!(json, format!("\"{}\"", language.label()));
            let back: Language = serde_json::from_str(&json).unwrap();
            assert_eq!(back, language);
        }
    }

    #[test]
    fn serde_rejects_unknown_languages() {
        assert!(serde_json::from_str::<Language>("\"kotlin\"").is_err());
        assert!(serde_json::from_str::<Language>("\"CSharp\"").is_err());
        assert!(serde_json::from_str::<Language>("\"posix-sh\"").is_err());
    }

    #[test]
    fn new_languages_are_part_of_the_registry() {
        let all = Language::all();
        assert_eq!(all.len(), 11, "eleven indexed languages");
        for (language, label) in [
            (Language::Python, "python"),
            (Language::Bash, "bash"),
            (Language::Lua, "lua"),
            (Language::Html, "html"),
        ] {
            assert!(all.contains(&language), "{label} missing from all()");
            assert_eq!(language.label(), label);
            assert_eq!(language.to_string(), label);
        }
    }
}

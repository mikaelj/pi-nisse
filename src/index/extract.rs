//! Language indexers: deterministic extraction over tree-sitter parse
//! trees (plan section 27).
//!
//! [`LanguageIndexer`] is the abstraction; [`TreeSitterIndexer`] is the
//! real implementation for the seven Nisse 1.0 languages. Extraction is a
//! pure function of `(source text, language, extraction version)`:
//!
//! - **definitions** — functions, methods, classes/structs, enums, unions,
//!   types, interfaces/traits/mixins, modules/namespaces — with spans;
//! - **imports** — import/include/using statements;
//! - **identifier references** — deduplicated per `(name, line)`, with
//!   definition-name occurrences excluded and a hard row limit;
//! - **skeleton** — a compact one-line-per-definition text rendering.
//!
//! The rules are static tables, not LLM output: a reviewer never invents
//! its own review methodology and neither does the indexer. Tree-sitter's
//! error-tolerant parsing means malformed files still produce
//! deterministic (partial) artifacts instead of failures.

use std::collections::{BTreeSet, HashMap};

use tree_sitter::{Node, Parser, Tree};

use super::artifact::{
    ExtractedIndex, IdentifierRef, ImportDecl, REFERENCE_ROW_LIMIT, SymbolDef, SymbolKind,
};
use super::language::Language;

/// Extracts an [`ExtractedIndex`] from source text.
pub trait LanguageIndexer {
    /// Language this indexer handles.
    fn language(&self) -> Language;

    /// Extract the index of `source`.
    fn parse(&mut self, source: &str) -> ExtractedIndex;
}

/// Tree-sitter-backed indexer for one grammar.
///
/// The parser is created once and reused across files; the grammar never
/// changes for the lifetime of the indexer.
pub struct TreeSitterIndexer {
    language: Language,
    parser: Parser,
}

impl TreeSitterIndexer {
    /// Build an indexer for a language's default grammar.
    pub fn new(language: Language) -> Result<Self, super::error::IndexError> {
        let grammar =
            grammar_for(language).ok_or_else(|| super::error::IndexError::ParserSetup {
                language,
                reason: "no built-in grammar".to_string(),
            })?;
        Self::with_grammar(language, grammar)
    }

    /// Build an indexer selected by a repository-relative path: the
    /// language is detected from the extension and `.tsx` files select
    /// the TSX grammar (which accepts JSX syntax); every other extension
    /// uses its language's default grammar.
    pub fn new_for_path(path: &str) -> Result<Self, super::error::IndexError> {
        let language =
            Language::from_path(path).ok_or_else(|| super::error::IndexError::ParserSetup {
                language: Language::Rust,
                reason: format!("path `{path}` has no indexed language"),
            })?;
        let grammar = if matches!(language, Language::TypeScript)
            && path.to_ascii_lowercase().ends_with(".tsx")
        {
            Some(tree_sitter_typescript::LANGUAGE_TSX)
        } else {
            grammar_for(language)
        };
        let grammar = grammar.ok_or_else(|| super::error::IndexError::ParserSetup {
            language,
            reason: "no built-in grammar".to_string(),
        })?;
        Self::with_grammar(language, grammar)
    }

    fn with_grammar(
        language: Language,
        grammar: tree_sitter_language::LanguageFn,
    ) -> Result<Self, super::error::IndexError> {
        let ts_language: tree_sitter::Language = grammar.into();
        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|err| super::error::IndexError::ParserSetup {
                language,
                reason: err.to_string(),
            })?;
        Ok(TreeSitterIndexer { language, parser })
    }
}

impl LanguageIndexer for TreeSitterIndexer {
    fn language(&self) -> Language {
        self.language
    }

    fn parse(&mut self, source: &str) -> ExtractedIndex {
        let tree = self
            .parser
            .parse(source, None)
            .expect("parser was configured with a language at construction");
        extract(self.language, tree, source)
    }
}

/// Lazily built per-grammar indexers for one refresh.
///
/// Grammars (not languages) are the cache key: `.ts` and `.tsx` share the
/// TypeScript language label but use different grammars.
#[derive(Default)]
pub struct IndexerCache {
    indexers: HashMap<(Language, bool), TreeSitterIndexer>,
}

impl IndexerCache {
    /// Build an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The indexer for `path`, creating it on first use.
    pub fn for_path(
        &mut self,
        path: &str,
    ) -> Result<&mut TreeSitterIndexer, super::error::IndexError> {
        let language =
            Language::from_path(path).ok_or_else(|| super::error::IndexError::ParserSetup {
                language: Language::Rust,
                reason: format!("path `{path}` has no indexed language"),
            })?;
        let tsx =
            matches!(language, Language::TypeScript) && path.to_ascii_lowercase().ends_with(".tsx");
        match self.indexers.entry((language, tsx)) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let indexer = TreeSitterIndexer::new_for_path(path)?;
                Ok(entry.insert(indexer))
            }
        }
    }
}

/// Grammar table for the seven indexed languages.
fn grammar_for(language: Language) -> Option<tree_sitter_language::LanguageFn> {
    match language {
        Language::C => Some(tree_sitter_c::LANGUAGE),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE),
        Language::CSharp => Some(tree_sitter_c_sharp::LANGUAGE),
        Language::Dart => Some(tree_sitter_dart::LANGUAGE),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
    }
}

/// How to classify one definition node kind.
#[derive(Debug, Clone, Copy)]
struct DefRule {
    /// Base classification.
    kind: SymbolKind,
    /// How a function-like node is reclassified as a method.
    method: MethodResolution,
    /// Skip nodes without a `body` field (C/C++ forward declarations).
    requires_body: bool,
    /// C++ member functions: `field_declaration` whose declarator is a
    /// `function_declarator`. Data members are skipped entirely.
    member_function: bool,
}

/// Reclassification strategy for function-like definitions.
#[derive(Debug, Clone, Copy)]
enum MethodResolution {
    /// Never a method.
    Never,
    /// Method when the direct parent has one of these kinds (C++:
    /// in-class definitions inside `field_declaration_list`).
    Parent { kinds: &'static [&'static str] },
    /// Method when the parent is a wrapper kind (Rust:
    /// `declaration_list`) whose own parent is a container kind
    /// (`impl_item`/`trait_item`). Free and nested functions stay
    /// functions.
    ParentWrapper {
        wrapper: &'static str,
        containers: &'static [&'static str],
    },
}

/// Per-language definition rule tables.
mod rules {
    use super::{DefRule, Language, MethodResolution, SymbolKind};

    const fn plain(kind: SymbolKind) -> DefRule {
        DefRule {
            kind,
            method: MethodResolution::Never,
            requires_body: false,
            member_function: false,
        }
    }

    const fn function_or_method(method: MethodResolution) -> DefRule {
        DefRule {
            kind: SymbolKind::Function,
            method,
            requires_body: false,
            member_function: false,
        }
    }

    const fn with_body(kind: SymbolKind) -> DefRule {
        DefRule {
            kind,
            method: MethodResolution::Never,
            requires_body: true,
            member_function: false,
        }
    }

    const fn member_function() -> DefRule {
        DefRule {
            kind: SymbolKind::Method,
            method: MethodResolution::Never,
            requires_body: false,
            member_function: true,
        }
    }

    static RULES_C: &[(&str, DefRule)] = &[
        ("function_definition", plain(SymbolKind::Function)),
        ("struct_specifier", with_body(SymbolKind::Struct)),
        ("union_specifier", with_body(SymbolKind::Union)),
        ("enum_specifier", with_body(SymbolKind::Enum)),
        ("type_definition", plain(SymbolKind::Type)),
    ];

    static RULES_CPP: &[(&str, DefRule)] = &[
        (
            "function_definition",
            function_or_method(MethodResolution::Parent {
                kinds: &["field_declaration_list"],
            }),
        ),
        ("function_declaration", plain(SymbolKind::Function)),
        ("field_declaration", member_function()),
        ("class_specifier", with_body(SymbolKind::Class)),
        ("struct_specifier", with_body(SymbolKind::Struct)),
        ("union_specifier", with_body(SymbolKind::Union)),
        ("enum_specifier", with_body(SymbolKind::Enum)),
        ("alias_declaration", plain(SymbolKind::Type)),
        ("type_definition", plain(SymbolKind::Type)),
        ("namespace_definition", plain(SymbolKind::Module)),
    ];

    static RULES_CSHARP: &[(&str, DefRule)] = &[
        ("class_declaration", plain(SymbolKind::Class)),
        ("struct_declaration", plain(SymbolKind::Struct)),
        ("interface_declaration", plain(SymbolKind::Interface)),
        ("enum_declaration", plain(SymbolKind::Enum)),
        ("record_declaration", plain(SymbolKind::Class)),
        ("delegate_declaration", plain(SymbolKind::Type)),
        ("method_declaration", plain(SymbolKind::Method)),
        ("constructor_declaration", plain(SymbolKind::Method)),
        ("namespace_declaration", plain(SymbolKind::Module)),
        (
            "file_scoped_namespace_declaration",
            plain(SymbolKind::Module),
        ),
    ];

    static RULES_DART: &[(&str, DefRule)] = &[
        ("class_declaration", plain(SymbolKind::Class)),
        // Mixins are Dart's reusable interface-like unit.
        ("mixin_declaration", plain(SymbolKind::Interface)),
        ("enum_declaration", plain(SymbolKind::Enum)),
        // Top-level functions wrap their signature; methods inside class
        // bodies are `method_declaration` nodes.
        ("function_declaration", plain(SymbolKind::Function)),
        ("method_declaration", plain(SymbolKind::Method)),
        ("type_alias", plain(SymbolKind::Type)),
    ];

    static RULES_RUST: &[(&str, DefRule)] = &[
        (
            "function_item",
            function_or_method(MethodResolution::ParentWrapper {
                wrapper: "declaration_list",
                containers: &["impl_item", "trait_item"],
            }),
        ),
        // Trait method signatures (bodies optional).
        ("function_signature_item", plain(SymbolKind::Method)),
        ("struct_item", plain(SymbolKind::Struct)),
        ("union_item", plain(SymbolKind::Union)),
        ("enum_item", plain(SymbolKind::Enum)),
        ("trait_item", plain(SymbolKind::Interface)),
        ("type_item", plain(SymbolKind::Type)),
        ("mod_item", plain(SymbolKind::Module)),
    ];

    static RULES_JS: &[(&str, DefRule)] = &[
        ("function_declaration", plain(SymbolKind::Function)),
        (
            "generator_function_declaration",
            plain(SymbolKind::Function),
        ),
        ("class_declaration", plain(SymbolKind::Class)),
        ("method_definition", plain(SymbolKind::Method)),
    ];

    static RULES_TS: &[(&str, DefRule)] = &[
        ("function_declaration", plain(SymbolKind::Function)),
        (
            "generator_function_declaration",
            plain(SymbolKind::Function),
        ),
        ("class_declaration", plain(SymbolKind::Class)),
        ("method_definition", plain(SymbolKind::Method)),
        ("interface_declaration", plain(SymbolKind::Interface)),
        ("type_alias_declaration", plain(SymbolKind::Type)),
        ("enum_declaration", plain(SymbolKind::Enum)),
    ];

    /// All definition rules of one language: `(node kind, rule)` pairs.
    pub fn for_language(language: Language) -> &'static [(&'static str, DefRule)] {
        match language {
            Language::C => RULES_C,
            Language::Cpp => RULES_CPP,
            Language::CSharp => RULES_CSHARP,
            Language::Dart => RULES_DART,
            Language::Rust => RULES_RUST,
            Language::JavaScript => RULES_JS,
            Language::TypeScript => RULES_TS,
        }
    }
}

/// Import-statement node kinds per language.
fn import_kinds(language: Language) -> &'static [&'static str] {
    match language {
        Language::C => &["preproc_include"],
        Language::Cpp => &["preproc_include", "using_declaration"],
        Language::CSharp => &["using_directive"],
        Language::Dart => &["import_or_export"],
        Language::Rust => &["use_declaration"],
        Language::JavaScript | Language::TypeScript => &["import_statement"],
    }
}

/// Identifier-like node kinds considered for references.
const REFERENCE_KINDS: &[&str] = &[
    "identifier",
    "type_identifier",
    "field_identifier",
    "property_identifier",
    "namespace_identifier",
];

/// One raw identifier occurrence gathered during the walk.
struct RawRef {
    name: String,
    line: usize,
    byte_start: usize,
    byte_end: usize,
}

/// Extract the index of a parsed tree.
fn extract(language: Language, tree: Tree, source: &str) -> ExtractedIndex {
    let def_rules = rules::for_language(language);
    let imports = import_kinds(language);

    let mut symbols: Vec<SymbolDef> = Vec::new();
    let mut import_lines: Vec<ImportDecl> = Vec::new();
    let mut raw_refs: Vec<RawRef> = Vec::new();
    // Byte ranges of nodes recorded as definition names; those
    // occurrences are definitions, not references.
    let mut name_spans: Vec<(usize, usize)> = Vec::new();

    // Iterative document-order walk over named nodes (an explicit stack
    // so pathological nesting cannot overflow the call stack).
    let root = tree.root_node();
    if root.is_named() {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let kind = node.kind();

            if let Some((_, rule)) = def_rules.iter().find(|(k, _)| *k == kind) {
                if let Some(def) = definition_of(node, *rule, source, &mut name_spans) {
                    symbols.push(def);
                }
            } else if imports.contains(&kind)
                && let Ok(text) = node.utf8_text(source.as_bytes())
            {
                import_lines.push(ImportDecl {
                    text: collapse_whitespace(text),
                    line: node.start_position().row + 1,
                });
            }

            if REFERENCE_KINDS.contains(&kind)
                && let Ok(name) = node.utf8_text(source.as_bytes())
            {
                raw_refs.push(RawRef {
                    name: name.to_owned(),
                    line: node.start_position().row + 1,
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                });
            }

            // Push children in reverse so they pop in document order.
            let child_count = node.child_count();
            for index in (0..child_count).rev() {
                if let Some(child) = node.child(index) {
                    stack.push(child);
                }
            }
        }
    }

    // References: drop definition-name occurrences, deduplicate per
    // (name, line), and bound the row count in document order.
    let mut seen = BTreeSet::new();
    let mut references = Vec::new();
    for raw in raw_refs {
        if references.len() >= REFERENCE_ROW_LIMIT {
            break;
        }
        if name_spans
            .iter()
            .any(|(start, end)| *start == raw.byte_start && *end == raw.byte_end)
        {
            continue;
        }
        if seen.insert((raw.name.clone(), raw.line)) {
            references.push(IdentifierRef {
                name: raw.name,
                line: raw.line,
            });
        }
    }

    let skeleton = render_skeleton(&import_lines, &symbols);

    ExtractedIndex {
        symbols,
        imports: import_lines,
        references,
        skeleton,
    }
}

/// Build the definition record for one node, if it has a usable name.
fn definition_of(
    node: Node<'_>,
    rule: DefRule,
    source: &str,
    name_spans: &mut Vec<(usize, usize)>,
) -> Option<SymbolDef> {
    if rule.requires_body && node.child_by_field_name("body").is_none() {
        // Forward declarations have no definitional span.
        return None;
    }

    let mut kind = rule.kind;
    if rule.member_function {
        // Only member *functions*: the declarator must be a
        // function_declarator. Data members are not symbols.
        let declarator = node.child_by_field_name("declarator")?;
        if declarator.kind() != "function_declarator" {
            return None;
        }
    } else {
        kind = match rule.method {
            MethodResolution::Never => kind,
            MethodResolution::Parent { kinds } => {
                if let Some(parent) = node.parent()
                    && kinds.contains(&parent.kind())
                {
                    SymbolKind::Method
                } else {
                    kind
                }
            }
            MethodResolution::ParentWrapper {
                wrapper,
                containers,
            } => {
                if let Some(parent) = node.parent()
                    && parent.kind() == wrapper
                    && let Some(container) = parent.parent()
                    && containers.contains(&container.kind())
                {
                    SymbolKind::Method
                } else {
                    kind
                }
            }
        };
    }

    let name_node = resolve_name(node)?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_owned();
    name_spans.push((name_node.start_byte(), name_node.end_byte()));
    Some(SymbolDef {
        name,
        kind,
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
    })
}

/// Resolve the simple name node of a definition node.
///
/// Strategy, in order:
/// 1. the node's own `name` field (Rust, C#, JS/TS, C/C++ type
///    specifiers, Dart signatures);
/// 2. the first identifier-like node inside the `declarator` field
///    (C/C++ functions, members, typedefs; qualified names resolve to
///    their final component);
/// 3. recursion into the `signature` field (Dart function and method
///    declarations);
/// 4. the first descendant carrying a `name` field (Dart method
///    signatures wrapped in each other);
/// 5. the first `type_identifier` descendant (Dart type aliases).
fn resolve_name<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }
    if let Some(declarator) = node.child_by_field_name("declarator")
        && let Some(found) = first_identifier_like(declarator)
    {
        return Some(found);
    }
    if let Some(signature) = node.child_by_field_name("signature")
        && let Some(found) = resolve_name(signature)
    {
        return Some(found);
    }
    if let Some(found) = first_with_name_field(node) {
        return Some(found);
    }
    first_of_kinds(node, &["type_identifier"])
}

/// First node in document order (including `node` itself) whose kind is
/// identifier-like. Qualified names resolve to their final component.
fn first_identifier_like(node: Node<'_>) -> Option<Node<'_>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "qualified_identifier" {
            // `Namespace::name` — the simple name is the final component.
            return current.child_by_field_name("name").or(Some(current));
        }
        if REFERENCE_KINDS.contains(&current.kind()) {
            return Some(current);
        }
        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index) {
                stack.push(child);
            }
        }
    }
    None
}

/// First node in document order (including `node` itself) with one of
/// `kinds`.
fn first_of_kinds<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if kinds.contains(&current.kind()) {
            return Some(current);
        }
        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index) {
                stack.push(child);
            }
        }
    }
    None
}

/// First `name` field in document order under `node` (exclusive).
fn first_with_name_field<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.id() != node.id()
            && let Some(name) = current.child_by_field_name("name")
        {
            return Some(name);
        }
        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index) {
                stack.push(child);
            }
        }
    }
    None
}

/// Collapse runs of whitespace into single spaces.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render the compact skeleton: imports first, then definitions, both in
/// document order.
fn render_skeleton(imports: &[ImportDecl], symbols: &[SymbolDef]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for import in imports {
        let _ = writeln!(out, "import {}", import.text);
    }
    for symbol in symbols {
        let _ = writeln!(
            out,
            "{} {} L{}-L{}",
            symbol.kind.label(),
            symbol.name,
            symbol.line_start,
            symbol.line_end
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_for(language: Language, source: &str) -> ExtractedIndex {
        let mut indexer = TreeSitterIndexer::new(language).unwrap();
        indexer.parse(source)
    }

    fn symbol_names(index: &ExtractedIndex) -> Vec<(String, SymbolKind)> {
        index
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect()
    }

    #[test]
    fn extracts_rust_definitions_imports_and_references() {
        let source = r#"use std::collections::HashMap;

pub struct Point {
    pub x: f64,
}

trait Shape {
    fn area(&self) -> f64;
}

impl Shape for Point {
    fn area(&self) -> f64 {
        let map: HashMap<String, f64> = HashMap::new();
        map.len() as f64
    }
}

fn main() {
    let p = Point { x: 1.0 };
    println!("{}", p.x);
}
"#;
        let index = extract_for(Language::Rust, source);
        assert_eq!(
            symbol_names(&index),
            vec![
                ("Point".to_string(), SymbolKind::Struct),
                ("Shape".to_string(), SymbolKind::Interface),
                // Trait method and impl method.
                ("area".to_string(), SymbolKind::Method),
                ("area".to_string(), SymbolKind::Method),
                ("main".to_string(), SymbolKind::Function),
            ]
        );
        assert_eq!(index.imports.len(), 1);
        assert_eq!(index.imports[0].text, "use std::collections::HashMap;");

        // Definition-name occurrences are excluded from references; real
        // uses are kept.
        let point_def_line = index.symbols[0].line_start;
        let point_refs: Vec<usize> = index
            .references
            .iter()
            .filter(|r| r.name == "Point")
            .map(|r| r.line)
            .collect();
        assert!(!point_refs.contains(&point_def_line));
        assert!(index.references.iter().any(|r| r.name == "HashMap"));

        // Spans cover the exact definition text.
        let area_impl = index
            .symbols
            .iter()
            .find(|s| s.name == "area" && s.line_end > s.line_start)
            .expect("impl method has a multi-line span");
        assert_eq!(
            &source[area_impl.byte_start..area_impl.byte_end],
            "fn area(&self) -> f64 {\n        let map: HashMap<String, f64> = HashMap::new();\n        map.len() as f64\n    }"
        );

        assert!(
            index
                .skeleton
                .contains("import use std::collections::HashMap;")
        );
        assert!(index.skeleton.contains("struct Point L3-L5"));
        assert!(index.skeleton.contains("interface Shape L7-L9"));
        assert!(index.skeleton.contains("method area"));
    }

    #[test]
    fn extracts_c_definitions_and_includes() {
        let source = r##"#include <stdio.h>
#include "local.h"

typedef struct Point Point;

struct Point {
    int x;
};

enum Color { RED, GREEN };

static int helper(int v) {
    return v + 1;
}

int main(void) {
    return helper(0);
}
"##;
        let index = extract_for(Language::C, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("Point".to_string(), SymbolKind::Struct)));
        assert!(names.contains(&("Point".to_string(), SymbolKind::Type)));
        assert!(names.contains(&("Color".to_string(), SymbolKind::Enum)));
        assert!(names.contains(&("helper".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("main".to_string(), SymbolKind::Function)));
        // The forward declaration inside the typedef is not a body
        // definition: exactly one struct Point.
        assert_eq!(
            index
                .symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Struct)
                .count(),
            1
        );
        assert_eq!(index.imports.len(), 2);
        assert_eq!(index.imports[0].text, "#include <stdio.h>");
        assert_eq!(index.imports[1].text, "#include \"local.h\"");
    }

    #[test]
    fn extracts_c_typedefs_without_struct() {
        let index = extract_for(Language::C, "typedef unsigned long ulong_t;\n");
        assert_eq!(
            symbol_names(&index),
            vec![("ulong_t".to_string(), SymbolKind::Type)]
        );
    }

    #[test]
    fn extracts_cpp_classes_members_and_namespaces() {
        let source = r#"#include <vector>

namespace app {

class Widget {
public:
    void draw();
    int width_ = 0;
};

void Widget::draw() {
    std::vector<int> v;
    (void)v;
}

struct Node { int id; };
using Alias = Widget;
using std::size_t;

}
"#;
        let index = extract_for(Language::Cpp, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("Widget".to_string(), SymbolKind::Class)));
        // In-class declared member function, not the data member.
        assert!(names.contains(&("draw".to_string(), SymbolKind::Method)));
        assert!(!names.contains(&("width_".to_string(), SymbolKind::Method)));
        // Out-of-line definition resolves the qualified name to `draw`
        // and classifies as a free function.
        assert!(names.contains(&("draw".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("Node".to_string(), SymbolKind::Struct)));
        assert!(names.contains(&("Alias".to_string(), SymbolKind::Type)));
        assert!(names.contains(&("app".to_string(), SymbolKind::Module)));
        assert_eq!(index.imports.len(), 2);
    }

    #[test]
    fn extracts_csharp_declarations() {
        let source = r#"using System;
using System.Collections.Generic;

namespace App;

public interface IShape {
    double Area();
}

public record Point(double X, double Y);

public struct Box {
    public int Size;
}

public enum Kind { A, B }

public class Widget : IShape {
    public double Area() => 1.0;
    public Widget() {}
}
"#;
        let index = extract_for(Language::CSharp, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("IShape".to_string(), SymbolKind::Interface)));
        assert!(names.contains(&("Point".to_string(), SymbolKind::Class)));
        assert!(names.contains(&("Box".to_string(), SymbolKind::Struct)));
        assert!(names.contains(&("Kind".to_string(), SymbolKind::Enum)));
        assert!(names.contains(&("Widget".to_string(), SymbolKind::Class)));
        assert!(names.contains(&("Area".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("App".to_string(), SymbolKind::Module)));
        assert_eq!(index.imports.len(), 2);
    }

    #[test]
    fn extracts_dart_definitions_and_imports() {
        let source = r#"import 'dart:async';
import 'package:flutter/widgets.dart';

mixin Printable {
  void describe();
}

enum Level { low, high }

typedef Count = int;

class Counter extends Object with Printable {
  int value = 0;
  void increment() {
    value += 1;
  }
  int get next => value + 1;
}

void main() {
  final c = Counter();
  c.increment();
}
"#;
        let index = extract_for(Language::Dart, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("Printable".to_string(), SymbolKind::Interface)));
        assert!(names.contains(&("Level".to_string(), SymbolKind::Enum)));
        assert!(names.contains(&("Count".to_string(), SymbolKind::Type)));
        assert!(names.contains(&("Counter".to_string(), SymbolKind::Class)));
        assert!(names.contains(&("increment".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("next".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("main".to_string(), SymbolKind::Function)));
        assert_eq!(index.imports.len(), 2);
        assert_eq!(
            index.imports[1].text,
            "import 'package:flutter/widgets.dart';"
        );
    }

    #[test]
    fn extracts_javascript_definitions() {
        let source = r#"import { helper } from './helper.js';

export function main() {
  return helper();
}

function* gen() {
  yield 1;
}

class Thing {
  constructor() {}
  run() {
    return this;
  }
}
"#;
        let index = extract_for(Language::JavaScript, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("main".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("gen".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("Thing".to_string(), SymbolKind::Class)));
        assert!(names.contains(&("run".to_string(), SymbolKind::Method)));
        assert_eq!(index.imports.len(), 1);
    }

    #[test]
    fn extracts_typescript_definitions() {
        let source = r#"import type { A } from './a';

export interface Shape {
  area(): number;
}

export type ID = string;

export enum Mode { On, Off }

export function area(s: Shape): number {
  return s.area();
}
"#;
        let index = extract_for(Language::TypeScript, source);
        let names = symbol_names(&index);
        assert!(names.contains(&("Shape".to_string(), SymbolKind::Interface)));
        assert!(names.contains(&("ID".to_string(), SymbolKind::Type)));
        assert!(names.contains(&("Mode".to_string(), SymbolKind::Enum)));
        assert!(names.contains(&("area".to_string(), SymbolKind::Function)));
        assert_eq!(index.imports.len(), 1);
    }

    #[test]
    fn tsx_paths_parse_jsx_through_the_tsx_grammar() {
        let mut indexer = TreeSitterIndexer::new_for_path("ui/App.tsx").unwrap();
        let index = indexer.parse("const a = <div className=\"x\"/>;\nexport default a;\n");
        // JSX without parse errors still yields references (no symbols in
        // this snippet, but extraction must not fall apart).
        assert!(index.references.iter().any(|r| r.name == "a"));
        assert_eq!(indexer.language(), Language::TypeScript);

        let ts = TreeSitterIndexer::new_for_path("src/types.ts").unwrap();
        assert_eq!(
            ts.language(),
            Language::TypeScript,
            "plain .ts stays TypeScript"
        );
    }

    #[test]
    fn references_are_deduplicated_per_name_and_line_and_exclude_names() {
        let source = "fn one() {\n    helper();\n    helper();\n}\nfn helper() {}\n";
        let index = extract_for(Language::Rust, source);
        let helper_refs: Vec<&IdentifierRef> = index
            .references
            .iter()
            .filter(|r| r.name == "helper")
            .collect();
        // One row per line for the two call lines; the definition name on
        // line 5 is excluded.
        assert_eq!(helper_refs.len(), 2);
        assert!(helper_refs.iter().all(|r| r.line != 5));
    }

    #[test]
    fn reference_rows_are_bounded() {
        let mut source = String::from("fn sink() {\n");
        for i in 0..(REFERENCE_ROW_LIMIT + 500) {
            source.push_str(&format!("    let v{i} = {i};\n"));
        }
        source.push_str("}\n");
        let index = extract_for(Language::Rust, &source);
        assert_eq!(index.references.len(), REFERENCE_ROW_LIMIT);
    }

    #[test]
    fn malformed_source_still_extracts_deterministically() {
        let broken = "fn broken( {\n";
        let index = extract_for(Language::Rust, broken);
        assert!(index.symbols.is_empty() || index.symbols.iter().all(|s| s.name == "broken"));
    }

    #[test]
    fn empty_source_extracts_empty_index() {
        for language in Language::all() {
            let index = extract_for(language, "");
            assert!(index.symbols.is_empty(), "{language:?}");
            assert!(index.imports.is_empty(), "{language:?}");
            assert!(index.references.is_empty(), "{language:?}");
            assert_eq!(index.skeleton, "", "{language:?}");
        }
    }

    #[test]
    fn symbols_stay_in_document_order_with_valid_spans() {
        let source = "struct A;\nfn b() {}\n";
        let index = extract_for(Language::Rust, source);
        for symbol in &index.symbols {
            assert!(symbol.byte_start <= symbol.byte_end);
            assert!(symbol.line_start <= symbol.line_end);
            assert!(symbol.byte_end <= source.len());
        }
        let starts: Vec<usize> = index.symbols.iter().map(|s| s.byte_start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn spans_report_real_lines_and_bytes() {
        let source = "mod outer;\n\nfn later() {\n    ()\n}\n";
        let index = extract_for(Language::Rust, source);
        let later = index.symbols.iter().find(|s| s.name == "later").unwrap();
        assert_eq!(later.line_start, 3);
        assert_eq!(later.line_end, 5);
        assert_eq!(
            &source[later.byte_start..later.byte_end],
            "fn later() {\n    ()\n}"
        );
    }

    #[test]
    fn indexer_cache_reuses_indexers_per_grammar() {
        let mut cache = IndexerCache::new();
        let a = cache.for_path("a.ts").unwrap() as *const TreeSitterIndexer;
        let b = cache.for_path("b.ts").unwrap() as *const TreeSitterIndexer;
        let c = cache.for_path("c.tsx").unwrap() as *const TreeSitterIndexer;
        let d = cache.for_path("d.rs").unwrap() as *const TreeSitterIndexer;
        assert_eq!(a, b, "same grammar is reused");
        assert_ne!(a, c, "tsx is a different grammar");
        assert_ne!(a, d);
        assert!(cache.for_path("x.md").is_err());
    }
}

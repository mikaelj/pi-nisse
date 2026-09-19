//! Language indexers: deterministic extraction over tree-sitter parse
//! trees (plan section 27).
//!
//! [`LanguageIndexer`] is the abstraction; [`TreeSitterIndexer`] is the
//! real implementation for the eleven Nisse indexed languages.
//! Extraction is a pure function of `(source text, language, extraction
//! version)`:
//!
//! - **definitions** — functions, methods, classes/structs, enums,
//!   unions, types, interfaces/traits/mixins, modules/namespaces, and
//!   HTML elements — with spans;
//! - **imports** — import/include/using statements and literal
//!   dependency statements (`source`, `require`, `script src`, ...);
//! - **identifier references** — deduplicated per `(name, line)`, with
//!   definition-name occurrences excluded and a hard row limit;
//! - **skeleton** — a compact one-line-per-definition text rendering
//!   plus bounded per-language header lines (shebangs, doctype).
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
use super::languages::{Extraction, augment, collapse_whitespace};

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
        Self::for_detected_path(path, language)
    }

    /// Build an indexer for a path whose language was already detected —
    /// by extension or shebang. `.tsx` still selects the TSX grammar;
    /// everything else uses its language's default grammar.
    pub fn for_detected_path(
        path: &str,
        language: Language,
    ) -> Result<Self, super::error::IndexError> {
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
        self.for_detected_path(path, language)
    }

    /// The indexer for a path whose language was already detected — by
    /// extension or shebang — creating it on first use.
    pub fn for_detected_path(
        &mut self,
        path: &str,
        language: Language,
    ) -> Result<&mut TreeSitterIndexer, super::error::IndexError> {
        let tsx =
            matches!(language, Language::TypeScript) && path.to_ascii_lowercase().ends_with(".tsx");
        match self.indexers.entry((language, tsx)) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let indexer = TreeSitterIndexer::for_detected_path(path, language)?;
                Ok(entry.insert(indexer))
            }
        }
    }
}

/// Grammar table for the eleven indexed languages.
fn grammar_for(language: Language) -> Option<tree_sitter_language::LanguageFn> {
    match language {
        Language::C => Some(tree_sitter_c::LANGUAGE),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE),
        Language::CSharp => Some(tree_sitter_c_sharp::LANGUAGE),
        Language::Dart => Some(tree_sitter_dart::LANGUAGE),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        Language::Python => Some(tree_sitter_python::LANGUAGE),
        Language::Bash => Some(tree_sitter_bash::LANGUAGE),
        Language::Lua => Some(tree_sitter_lua::LANGUAGE),
        Language::Html => Some(tree_sitter_html::LANGUAGE),
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
    /// `declaration_list`; Python: class-body `block`) whose own parent
    /// is a container kind (`impl_item`/`trait_item`/
    /// `class_definition`). Free and nested functions stay functions.
    ParentWrapper {
        wrapper: &'static str,
        containers: &'static [&'static str],
    },
    /// Method when the resolved name is colon-qualified (Lua
    /// `function obj:method()`; dotted `module.fn` stays a function).
    ColonQualified,
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

    static RULES_PYTHON: &[(&str, DefRule)] = &[
        // Free, nested, and async functions; methods are the ones whose
        // surrounding block belongs to a class definition. Decorators
        // wrap definitions in `decorated_definition` nodes and are never
        // symbols themselves.
        (
            "function_definition",
            function_or_method(MethodResolution::ParentWrapper {
                wrapper: "block",
                containers: &["class_definition"],
            }),
        ),
        ("class_definition", plain(SymbolKind::Class)),
    ];

    static RULES_BASH: &[(&str, DefRule)] = &[("function_definition", plain(SymbolKind::Function))];

    static RULES_LUA: &[(&str, DefRule)] = &[(
        // `function foo()`, `local function foo()`, `function module.fn()`,
        // `function obj:method()` — the name field carries the qualified
        // syntactic form.
        "function_declaration",
        function_or_method(MethodResolution::ColonQualified),
    )];

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
            Language::Python => RULES_PYTHON,
            Language::Bash => RULES_BASH,
            Language::Lua => RULES_LUA,
            Language::Html => &[],
        }
    }
}

/// Import-statement node kinds per language. Languages whose
/// dependencies are not distinct node kinds (Bash `source`, Lua
/// `require`, HTML resource attributes) extract them in
/// [`super::languages`] instead.
fn import_kinds(language: Language) -> &'static [&'static str] {
    match language {
        Language::C => &["preproc_include"],
        Language::Cpp => &["preproc_include", "using_declaration"],
        Language::CSharp => &["using_directive"],
        Language::Dart => &["import_or_export"],
        Language::Rust => &["use_declaration"],
        Language::JavaScript | Language::TypeScript => &["import_statement"],
        Language::Python => &[
            "import_statement",
            "import_from_statement",
            "future_import_statement",
        ],
        Language::Bash | Language::Lua | Language::Html => &[],
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

/// One raw identifier occurrence gathered during a walk.
pub(crate) struct RawRef {
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
}

/// Extract the index of a parsed tree.
///
/// The generic walk applies the per-language rule tables; the
/// per-language detail pass ([`augment`]) then appends whatever those
/// tables cannot express (Bash `source`/commands, Lua `require`/calls,
/// Python decorators, HTML structure). For HTML the generic walk matches
/// nothing and the detail pass does all the work.
fn extract(language: Language, tree: Tree, source: &str) -> ExtractedIndex {
    let def_rules = rules::for_language(language);
    let imports = import_kinds(language);

    let mut ex = Extraction::new();

    // Iterative document-order walk over named nodes (an explicit stack
    // so pathological nesting cannot overflow the call stack).
    let root = tree.root_node();
    if root.is_named() {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let kind = node.kind();

            if let Some((_, rule)) = def_rules.iter().find(|(k, _)| *k == kind) {
                if let Some(def) = definition_of(node, *rule, source, &mut ex.name_spans) {
                    ex.symbols.push(def);
                }
            } else if imports.contains(&kind)
                && let Ok(text) = node.utf8_text(source.as_bytes())
            {
                ex.imports.push(ImportDecl {
                    text: collapse_whitespace(text),
                    line: node.start_position().row + 1,
                });
            }

            if REFERENCE_KINDS.contains(&kind)
                && let Ok(name) = node.utf8_text(source.as_bytes())
            {
                ex.refs.push(RawRef {
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

    augment(language, &tree, source, &mut ex);
    finish(ex)
}

/// Turn the collected extraction into the final index.
///
/// References: merge the generic walk and the detail pass in document
/// order, drop definition-name occurrences, deduplicate per
/// `(name, line)`, and bound the row count.
fn finish(mut ex: Extraction) -> ExtractedIndex {
    let mut raw_refs = std::mem::take(&mut ex.refs);
    raw_refs.sort_by_key(|raw| raw.byte_start);

    let mut seen = BTreeSet::new();
    let mut references = Vec::new();
    for raw in raw_refs {
        if references.len() >= REFERENCE_ROW_LIMIT {
            break;
        }
        if ex
            .name_spans
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

    let skeleton = render_skeleton(&ex);

    ExtractedIndex {
        symbols: ex.symbols,
        imports: ex.imports,
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

    if rule.member_function {
        // Only member *functions*: the declarator must be a
        // function_declarator. Data members are not symbols.
        let declarator = node.child_by_field_name("declarator")?;
        if declarator.kind() != "function_declarator" {
            return None;
        }
    }

    let name_node = resolve_name(node)?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_owned();

    let kind = match rule.method {
        MethodResolution::Never => rule.kind,
        MethodResolution::Parent { kinds } => {
            if let Some(parent) = node.parent()
                && kinds.contains(&parent.kind())
            {
                SymbolKind::Method
            } else {
                rule.kind
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
                rule.kind
            }
        }
        MethodResolution::ColonQualified => {
            if name.contains(':') {
                SymbolKind::Method
            } else {
                rule.kind
            }
        }
    };

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

/// Render the compact skeleton: bounded header lines first (shebangs,
/// doctype, title), then imports, then definitions with their
/// decorators, all in document order.
fn render_skeleton(ex: &Extraction) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for line in &ex.skeleton_prefix {
        let _ = writeln!(out, "{line}");
    }
    for import in &ex.imports {
        let _ = writeln!(out, "import {}", import.text);
    }
    for symbol in &ex.symbols {
        if let Some(decorators) = ex.decorators.get(&symbol.byte_start) {
            for decorator in decorators {
                let _ = writeln!(out, "{decorator}");
            }
        }
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
    fn registry_maps_every_language_to_its_indexer() {
        // Every registered language builds its grammar and reports itself.
        for language in Language::all() {
            let indexer =
                TreeSitterIndexer::new(language).unwrap_or_else(|err| panic!("{language}: {err}"));
            assert_eq!(indexer.language(), language);
        }
        // The path-based registry resolves the four new languages.
        let mut cache = IndexerCache::new();
        for (path, expected) in [
            ("app.py", Language::Python),
            ("stubs/protocol.pyi", Language::Python),
            ("run.sh", Language::Bash),
            ("run.bash", Language::Bash),
            ("init.lua", Language::Lua),
            ("page.html", Language::Html),
            ("page.htm", Language::Html),
        ] {
            assert_eq!(
                cache.for_path(path).unwrap().language(),
                expected,
                "wrong indexer for {path}"
            );
        }
        // Shebang-detected (extensionless) files resolve through the
        // same registry with their default grammar.
        assert_eq!(
            cache
                .for_detected_path("bin/tool", Language::Python)
                .unwrap()
                .language(),
            Language::Python
        );
    }

    #[test]
    fn extracts_python_definitions_imports_and_references() {
        let source = r#"import asyncio
import foo.bar as fb
from protocol import Packet
from . import peer
from ..lib import helper

@dataclass
class Client(Base):
    def __init__(self, transport):
        self.transport = transport

    async def connect(self):
        await asyncio.sleep(1)

    def parse(self, data):
        return parse_packet(data)

def make_client(config):
    async def inner():
        pass
    return Client(config)
"#;
        let index = extract_for(Language::Python, source);
        let names = symbol_names(&index);
        // Classes, methods, free/async/nested functions. Methods are
        // recorded by simple name, matching the existing symbol model
        // (like C++ `Widget::draw` -> `draw`).
        assert!(names.contains(&("Client".to_string(), SymbolKind::Class)));
        assert!(names.contains(&("__init__".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("connect".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("parse".to_string(), SymbolKind::Method)));
        assert!(names.contains(&("make_client".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("inner".to_string(), SymbolKind::Function)));
        // Imports: plain, dotted, aliased, from, relative — statement text
        // preserves module, member, and relative level.
        let texts: Vec<&str> = index.imports.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "import asyncio",
                "import foo.bar as fb",
                "from protocol import Packet",
                "from . import peer",
                "from ..lib import helper",
            ]
        );
        // References: plain calls and attribute calls are kept; the
        // class-name occurrence at its definition is excluded but the
        // constructor call is kept.
        assert!(index.references.iter().any(|r| r.name == "parse_packet"));
        assert!(index.references.iter().any(|r| r.name == "sleep"));
        let client_lines: Vec<usize> = index
            .references
            .iter()
            .filter(|r| r.name == "Client")
            .map(|r| r.line)
            .collect();
        assert!(!client_lines.contains(&8), "definition line excluded");
        assert!(client_lines.contains(&21), "call line kept");
        // Decorators are not symbols but ride along in the skeleton.
        assert!(!names.iter().any(|(name, _)| name == "dataclass"));
        assert!(index.skeleton.contains("@dataclass\nclass Client L8-L16"));
        assert!(index.skeleton.contains("method connect L12-L13"));
        assert!(index.skeleton.contains("function make_client L18-L21"));
        // Spans cover the exact definition bytes.
        let client = index.symbols.iter().find(|s| s.name == "Client").unwrap();
        assert!(source[client.byte_start..client.byte_end].starts_with("class Client"));
    }

    #[test]
    fn python_pyi_files_parse_as_python() {
        let index = extract_for(Language::Python, "def greet(name: str) -> str: ...\n");
        assert_eq!(
            symbol_names(&index),
            vec![("greet".to_string(), SymbolKind::Function)]
        );
    }

    #[test]
    fn malformed_python_still_extracts_deterministically() {
        let broken = "def broken(:\n    return\nclass OK:\n";
        let first = extract_for(Language::Python, broken);
        let second = extract_for(Language::Python, broken);
        assert_eq!(first, second);
        assert!(
            first
                .symbols
                .iter()
                .all(|s| s.name == "OK" || s.name == "broken")
        );
    }

    #[test]
    fn extracts_bash_functions_sources_and_command_references() {
        let source = r#"#!/usr/bin/env bash
source ./lib/common.sh
. ./lib/other.sh
source "$DIR/dynamic.sh"

function foo {
    validate_config "$file"
}

function bar() {
    :
}

baz() {
    if true; then echo hi; fi
}

main
"#;
        let index = extract_for(Language::Bash, source);
        // All three function definition forms.
        let names = symbol_names(&index);
        assert_eq!(
            names,
            vec![
                ("foo".to_string(), SymbolKind::Function),
                ("bar".to_string(), SymbolKind::Function),
                ("baz".to_string(), SymbolKind::Function),
            ]
        );
        // Static source/dot dependencies plus the dynamic expression as
        // literal text — never shell-expanded or resolved.
        let texts: Vec<&str> = index.imports.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "source ./lib/common.sh",
                ". ./lib/other.sh",
                "source \"$DIR/dynamic.sh\"",
            ]
        );
        // Plain command invocations become unresolved references,
        // including indexed function invocations.
        assert!(index.references.iter().any(|r| r.name == "validate_config"));
        assert!(index.references.iter().any(|r| r.name == "main"));
        // Shell keywords never become references, and expansion or path
        // forms are filtered out.
        for keyword in [
            "if", "then", "fi", "else", "for", "while", "do", "done", "case", "esac", "function",
        ] {
            assert!(
                !index.references.iter().any(|r| r.name == keyword),
                "{keyword}"
            );
        }
        assert!(
            index
                .references
                .iter()
                .all(|r| !r.name.contains('$') && !r.name.contains('/'))
        );
        // The definition-name words are not references.
        assert!(
            index
                .references
                .iter()
                .all(|r| !matches!(r.name.as_str(), "foo" | "bar" | "baz"))
        );
        // Skeleton: shebang, dependencies, function names — no bodies.
        assert!(index.skeleton.starts_with("#!/usr/bin/env bash\n"));
        assert!(index.skeleton.contains("import source ./lib/common.sh"));
        assert!(index.skeleton.contains("function foo L6-L8"));
        assert!(!index.skeleton.contains("validate_config"));
    }

    #[test]
    fn malformed_bash_still_extracts_deterministically() {
        let broken = "#!/bin/bash\nbroken() {\n  if fi fi\n";
        let first = extract_for(Language::Bash, broken);
        let second = extract_for(Language::Bash, broken);
        assert_eq!(first, second);
    }

    #[test]
    fn extracts_lua_functions_requires_and_call_references() {
        let source = r#"#!/usr/bin/env lua
local protocol = require("protocol")
local util = require 'util'
local dyn = require(prefix .. ".mod")

function foo(a, b)
    return a + b
end

local function helper(x)
    return x * 2
end

function module.fn()
end

function object:method()
end

M.send(packet)
obj:call()
plain()
"#;
        let index = extract_for(Language::Lua, source);
        let names = symbol_names(&index);
        // Global, local, dotted, and colon forms; qualified names are
        // kept syntactically, colon methods classify as methods.
        assert!(names.contains(&("foo".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("helper".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("module.fn".to_string(), SymbolKind::Function)));
        assert!(names.contains(&("object:method".to_string(), SymbolKind::Method)));
        assert_eq!(names.len(), 4, "table assignments are not symbols");
        // Literal requires (both quoting styles) are dependencies; the
        // dynamic require is not.
        let texts: Vec<&str> = index.imports.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["require(\"protocol\")", "require 'util'"]);
        // Call references keep the syntactic callee form without
        // claiming runtime dispatch.
        assert!(index.references.iter().any(|r| r.name == "M.send"));
        assert!(index.references.iter().any(|r| r.name == "obj:call"));
        assert!(index.references.iter().any(|r| r.name == "plain"));
        // Skeleton: shebang, requires, definitions — no bodies.
        assert!(index.skeleton.starts_with("#!/usr/bin/env lua\n"));
        assert!(index.skeleton.contains("import require(\"protocol\")"));
        assert!(index.skeleton.contains("function foo L6-L8"));
        assert!(index.skeleton.contains("method object:method L17-L18"));
        assert!(!index.skeleton.contains("x * 2"));
    }

    #[test]
    fn malformed_lua_still_extracts_deterministically() {
        let broken = "#! /usr/bin/lua5.4\nfunction broken(\n  local x = = 2\n";
        let first = extract_for(Language::Lua, broken);
        let second = extract_for(Language::Lua, broken);
        assert_eq!(first, second);
    }

    #[test]
    fn extracts_html_structure_dependencies_and_fragment_references() {
        let source = r##"<!doctype html>
<html>
<head>
  <title>Checkout page</title>
  <link href="styles/main.css">
  <script src="/js/app.js"></script>
  <script>inline_call();</script>
  <style>body { color: red; }</style>
</head>
<body>
  <main id="checkout" class="hero big">
    <p id="intro">Welcome prose.</p>
    <a href="#checkout">home</a>
    <a href="https://example.com/x">ext</a>
    <a href="#">empty</a>
    <user-profile></user-profile>
    <img src="images/logo.svg">
    <img src="/static/pic.png"/>
  </main>
</body>
</html>
"##;
        let index = extract_for(Language::Html, source);
        let names = symbol_names(&index);
        // id-bearing elements and custom elements only.
        assert!(names.contains(&("checkout".to_string(), SymbolKind::Element)));
        assert!(names.contains(&("intro".to_string(), SymbolKind::Element)));
        assert!(names.contains(&("user-profile".to_string(), SymbolKind::Element)));
        // Ordinary tags and CSS classes never become symbols.
        for not_symbol in [
            "html", "head", "body", "main", "p", "a", "img", "link", "script", "style", "hero",
            "big", "title",
        ] {
            assert!(
                !names.iter().any(|(name, _)| name == not_symbol),
                "{not_symbol} must not be a symbol"
            );
        }
        // Resource dependencies, literal text preserved; external URLs
        // stay recorded as-is (deterministically) and are never fetched.
        let texts: Vec<&str> = index.imports.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                r#"<link href="styles/main.css">"#,
                r#"<script src="/js/app.js">"#,
                r#"<img src="images/logo.svg">"#,
                r#"<img src="/static/pic.png"/>"#,
            ]
        );
        // Fragment references relate to id symbols in the same document;
        // external and empty fragments are not fragment references.
        assert!(index.references.iter().any(|r| r.name == "checkout"));
        assert!(
            index
                .references
                .iter()
                .all(|r| !r.name.starts_with("http") && !r.name.is_empty())
        );
        // Inline script/style content is never parsed as JS or CSS.
        assert!(!index.references.iter().any(|r| r.name == "inline_call"));
        assert!(!index.skeleton.contains("color: red"));
        // Spans: the id symbol covers the start tag.
        let checkout = index.symbols.iter().find(|s| s.name == "checkout").unwrap();
        assert_eq!(checkout.line_start, 11);
        assert_eq!(checkout.line_end, 11);
        assert!(
            source[checkout.byte_start..checkout.byte_end].starts_with("<main id=\"checkout\"")
        );
        // Skeleton: doctype, title, dependencies, symbols — no prose.
        let skeleton = &index.skeleton;
        assert!(skeleton.contains("<!doctype html>"));
        assert!(skeleton.contains("title Checkout page"));
        assert!(skeleton.contains(r#"import <script src="/js/app.js">"#));
        assert!(skeleton.contains("element checkout L11-L11"));
        assert!(skeleton.contains("element user-profile L16-L16"));
        assert!(!skeleton.contains("Welcome prose."));
    }

    #[test]
    fn malformed_html_still_extracts_deterministically() {
        let broken = "<div id=\"outer\"\n  <p>text\n  <span id=\"s\">x\n";
        let first = extract_for(Language::Html, broken);
        let second = extract_for(Language::Html, broken);
        assert_eq!(first, second);
        // Recovered structure still yields the id symbols.
        let names = symbol_names(&first);
        assert!(names.iter().any(|(name, _)| name == "outer"));
    }

    #[test]
    fn htm_extension_parses_as_html() {
        let index = extract_for(Language::Html, "<p id=\"x\">t</p>\n");
        assert_eq!(
            symbol_names(&index),
            vec![("x".to_string(), SymbolKind::Element)]
        );
    }

    #[test]
    fn skeletons_stay_bounded_for_large_bodies() {
        // Python: a 300-line function body collapses to one skeleton line.
        let mut python = String::from("def big():\n");
        for i in 0..300 {
            python.push_str(&format!("    x{i} = compute({i})\n"));
        }
        python.push_str("\ndef small():\n    pass\n");
        let index = extract_for(Language::Python, &python);
        assert_eq!(index.skeleton.lines().count(), 2);
        assert!(!index.skeleton.contains("compute"));

        // Bash.
        let mut bash = String::from("#!/bin/bash\nbig() {\n");
        for i in 0..300 {
            bash.push_str(&format!("  step_{i}\n"));
        }
        bash.push_str("}\n");
        let index = extract_for(Language::Bash, &bash);
        assert_eq!(index.skeleton.lines().count(), 2);
        assert!(!index.skeleton.contains("step_"));

        // Lua.
        let mut lua = String::from("function big()\n");
        for i in 0..300 {
            lua.push_str(&format!("  local v{i} = step({i})\n"));
        }
        lua.push_str("end\n");
        let index = extract_for(Language::Lua, &lua);
        assert_eq!(index.skeleton.lines().count(), 1);
        assert!(!index.skeleton.contains("step"));

        // HTML: a huge text body disappears; ids survive.
        let mut html = String::from("<!doctype html>\n<body>\n<div id=\"wrap\">\n");
        for i in 0..200 {
            html.push_str(&format!("<p>paragraph {i} with a lot of prose text</p>\n"));
        }
        html.push_str("</div>\n</body>\n");
        let index = extract_for(Language::Html, &html);
        assert!(index.skeleton.contains("element wrap L"));
        assert!(!index.skeleton.contains("prose"));
        assert!(index.skeleton.len() < html.len() / 10);
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

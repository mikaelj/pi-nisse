//! Per-language extraction details beyond the generic rule tables.
//!
//! The generic walk in [`super::extract`] handles every language whose
//! definitions are name-bearing nodes (Python fits entirely). Some
//! languages carry dependencies or references that are not distinct node
//! kinds and therefore need a bounded, deterministic post-pass:
//!
//! - **Bash** — `source`/`.` commands are dependencies (recorded as
//!   literal statement text, never shell-expanded); plain command names
//!   are best-effort references with shell keywords excluded; the
//!   shebang line heads the skeleton.
//! - **Lua** — literal `require("mod")` calls are dependencies; call
//!   callees (including `module.fn` and `obj:method` forms) are
//!   best-effort references; the shebang line heads the skeleton.
//! - **Python** — decorators attached to indexed definitions are kept in
//!   the skeleton.
//! - **HTML** — full structural extraction: `id` attributes and custom
//!   elements become symbols; `script src` / `link href` / `img src`
//!   become literal resource dependencies; `href="#fragment"` becomes an
//!   intra-document reference; doctype and title head the skeleton.
//!   Inline `<script>`/`<style>` content is intentionally left as
//!   unparsed text.
//!
//! Everything here is a syntactic fact recorded without guessing:
//! dynamic `source "$X"` / `require(x .. y)` targets stay literal text,
//! and nothing is resolved against a runtime environment.

use std::collections::BTreeMap;

use tree_sitter::{Node, Tree};

use super::artifact::{ImportDecl, SymbolDef, SymbolKind};
use super::extract::RawRef;
use super::language::Language;

/// Mutable extraction state shared with the generic walk.
///
/// Owned by [`super::extract::extract`]; the passes here only append to
/// it, in document order.
pub(crate) struct Extraction {
    /// Definitions in document order.
    pub symbols: Vec<SymbolDef>,
    /// Import-like statements in document order.
    pub imports: Vec<ImportDecl>,
    /// Raw identifier occurrences in document order.
    pub refs: Vec<RawRef>,
    /// Byte ranges of nodes recorded as definition names; those
    /// occurrences are definitions, not references.
    pub name_spans: Vec<(usize, usize)>,
    /// Skeleton lines rendered before the import lines (shebangs,
    /// doctype, document title).
    pub skeleton_prefix: Vec<String>,
    /// Decorator lines keyed by the definition's byte start, rendered
    /// immediately before that symbol's skeleton line.
    pub decorators: BTreeMap<usize, Vec<String>>,
}

impl Extraction {
    pub(crate) fn new() -> Self {
        Extraction {
            symbols: Vec::new(),
            imports: Vec::new(),
            refs: Vec::new(),
            name_spans: Vec::new(),
            skeleton_prefix: Vec::new(),
            decorators: BTreeMap::new(),
        }
    }
}

/// Run the per-language detail pass for `language` (a no-op for the
/// languages the generic tables already cover completely).
pub(crate) fn augment(language: Language, tree: &Tree, source: &str, ex: &mut Extraction) {
    match language {
        Language::Bash => bash(tree, source, ex),
        Language::Lua => lua(tree, source, ex),
        Language::Python => python_decorators(tree, source, ex),
        Language::Html => html(tree, source, ex),
        _ => {}
    }
}

/// Walk named nodes in document order (iterative, like the generic walk).
fn walk_named<'a, F>(root: Node<'a>, source: &'a str, mut visit: F)
where
    F: FnMut(Node<'a>, &'a str),
{
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        visit(node, source);
        let child_count = node.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
}

/// Collapse runs of whitespace into single spaces.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------
// Bash
// ---------------------------------------------------------------------

/// Shell keywords that never become command references (plan §7.3).
const BASH_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "in", "select", "time",
];

fn bash(tree: &Tree, source: &str, ex: &mut Extraction) {
    walk_named(tree.root_node(), source, |node, source| match node.kind() {
        "command" => bash_command(node, source, ex),
        // The shebang parses as a leading comment; keep it in the
        // skeleton (plan §7.4).
        "comment" => {
            if let Ok(text) = node.utf8_text(source.as_bytes())
                && text.starts_with("#!")
                && node.start_byte() == 0
            {
                ex.skeleton_prefix.push(collapse_whitespace(text));
            }
        }
        _ => {}
    });
}

/// One `command` node: record `source`/`.` dependencies and plain
/// command-name references.
fn bash_command(node: Node<'_>, source: &str, ex: &mut Extraction) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name_text) = name.utf8_text(source.as_bytes()) else {
        return;
    };

    // Static source/dot commands are file dependencies. Dynamic targets
    // (`source "$DIR/x.sh"`) stay literal statement text — never
    // shell-expanded or resolved (plan §7.2).
    if (name_text == "source" || name_text == ".")
        && let Ok(text) = node.utf8_text(source.as_bytes())
    {
        ex.imports.push(ImportDecl {
            text: collapse_whitespace(text),
            line: node.start_position().row + 1,
        });
    }

    // Best-effort invocation references: only syntactically plain names
    // (no expansions, quotes, substitutions, or path components).
    if is_plain_command_name(name_text)
        && !BASH_KEYWORDS.contains(&name_text)
        && !name_text.starts_with('.')
    {
        ex.refs.push(RawRef {
            name: name_text.to_owned(),
            line: name.start_position().row + 1,
            byte_start: name.start_byte(),
            byte_end: name.end_byte(),
        });
    }
}

/// Whether a command name is a plain identifier-like token (letters,
/// digits, `_`, `-`, `.`), so quoting/expansion/path forms are excluded.
fn is_plain_command_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() && first != '_' {
        return false;
    }
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

// ---------------------------------------------------------------------
// Lua
// ---------------------------------------------------------------------

fn lua(tree: &Tree, source: &str, ex: &mut Extraction) {
    walk_named(tree.root_node(), source, |node, source| match node.kind() {
        "hash_bang_line" => {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                ex.skeleton_prefix.push(collapse_whitespace(text));
            }
        }
        "function_call" => lua_call(node, source, ex),
        _ => {}
    });
}

/// One `function_call`: literal `require` dependencies and callee
/// references (`plain`, `module.fn`, `obj:method`).
fn lua_call(node: Node<'_>, source: &str, ex: &mut Extraction) {
    let Some(callee) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(callee_text) = callee.utf8_text(source.as_bytes()) else {
        return;
    };

    // Literal `require("mod")` / `require 'mod'` calls are module
    // dependencies. Anything else (dynamic arguments such as
    // `require(prefix .. ".mod")`) is not a dependency — the argument is
    // not a plain string node (plan §8.2).
    if callee.kind() == "identifier"
        && callee_text == "require"
        && let Some(arguments) = node.child_by_field_name("arguments")
        && arguments.named_child_count() == 1
        && arguments
            .named_child(0)
            .is_some_and(|arg| arg.kind() == "string")
        && let Ok(text) = node.utf8_text(source.as_bytes())
    {
        ex.imports.push(ImportDecl {
            text: collapse_whitespace(text),
            line: node.start_position().row + 1,
        });
    }

    // Call references keep the syntactic callee form; dispatch is never
    // resolved (plan §8.3).
    if matches!(
        callee.kind(),
        "identifier" | "dot_index_expression" | "method_index_expression"
    ) {
        ex.refs.push(RawRef {
            name: callee_text.to_owned(),
            line: callee.start_position().row + 1,
            byte_start: callee.start_byte(),
            byte_end: callee.end_byte(),
        });
    }
}

// ---------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------

/// Keep decorators attached to indexed definitions in the skeleton
/// (plan §6.4). Decorators are not symbols themselves.
fn python_decorators(tree: &Tree, source: &str, ex: &mut Extraction) {
    walk_named(tree.root_node(), source, |node, source| {
        if node.kind() != "decorated_definition" {
            return;
        }
        let Some(definition) = node.child_by_field_name("definition") else {
            return;
        };
        let decorators: Vec<String> = node
            .children(&mut node.walk())
            .filter(|child| child.kind() == "decorator")
            .filter_map(|decorator| decorator.utf8_text(source.as_bytes()).ok())
            .map(collapse_whitespace)
            .collect();
        if !decorators.is_empty() {
            ex.decorators.insert(definition.start_byte(), decorators);
        }
    });
}

// ---------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------

/// Longest skeleton title line; longer titles are truncated (plan §13).
const TITLE_LIMIT: usize = 120;

/// Full HTML extraction (plan §9). HTML has no definitions in the
/// programming-language sense; the pass records structural facts only:
///
/// - `id` attribute values and hyphenated custom-element tag names as
///   [`SymbolKind::Element`] symbols spanning the start tag;
/// - `script src` / `link href` / `img src` as literal resource
///   dependencies (external URLs included, never fetched);
/// - `href="#fragment"` values as intra-document references;
/// - doctype and first `<title>` as skeleton header lines.
///
/// Inline `<script>`/`<style>` bodies are raw text nodes and produce
/// nothing (plan §9.5).
fn html(tree: &Tree, source: &str, ex: &mut Extraction) {
    let mut title_seen = false;
    walk_named(tree.root_node(), source, |node, source| match node.kind() {
        "doctype" => {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                ex.skeleton_prefix.push(collapse_whitespace(text));
            }
        }
        "start_tag" | "self_closing_tag" => {
            // The document title heads the skeleton right after the
            // doctype; the title text lives in the surrounding element.
            if !title_seen
                && tag_name_of(node, source).is_some_and(|name| name == "title")
                && let Some(parent) = node.parent()
                && let Some(text) = direct_text(parent, source)
            {
                title_seen = true;
                ex.skeleton_prefix
                    .push(truncate_title(&collapse_whitespace(&text)));
            }
            html_tag(node, source, ex);
        }
        _ => {}
    });
}

/// One start/self-closing tag: symbols and dependencies and fragment
/// references derived from its attributes.
fn html_tag(node: Node<'_>, source: &str, ex: &mut Extraction) {
    let Some(tag) = tag_name_of(node, source) else {
        return;
    };
    let attributes = TagAttributes::collect(node, source);

    // id-bearing elements become symbols (span: the start tag — the
    // smallest useful node containing the ID, plan §9.1).
    if let Some(id) = attributes.value("id")
        && !id.is_empty()
    {
        ex.symbols.push(SymbolDef {
            name: id.to_owned(),
            kind: SymbolKind::Element,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
    }

    // Custom elements (hyphenated tag names) become symbols; ordinary
    // standard tags never do (plan §9.1).
    if tag.contains('-') {
        ex.symbols.push(SymbolDef {
            name: tag.to_owned(),
            kind: SymbolKind::Element,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
    }

    // Static resource dependencies, literal target text preserved
    // (plan §9.2). External URLs stay recorded as-is and are never
    // fetched; inline content is not a dependency.
    let dependency = match tag {
        "script" => attributes.value("src"),
        "link" => attributes.value("href"),
        "img" => attributes.value("src"),
        _ => None,
    };
    if dependency.is_some()
        && let Ok(text) = node.utf8_text(source.as_bytes())
    {
        ex.imports.push(ImportDecl {
            text: collapse_whitespace(text),
            line: node.start_position().row + 1,
        });
    }

    // Static intra-document fragment references (plan §9.3).
    if let Some(href) = attributes.value("href")
        && let Some(fragment) = href.strip_prefix('#')
        && !fragment.is_empty()
        && let Some(value_node) = attributes.value_node("href")
    {
        ex.refs.push(RawRef {
            name: fragment.to_owned(),
            line: value_node.start_position().row + 1,
            byte_start: value_node.start_byte(),
            byte_end: value_node.end_byte(),
        });
    }
}

/// The tag name of a start/self-closing tag (or title element).
fn tag_name_of<'a>(node: Node<'a>, source: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "tag_name" {
            return child.utf8_text(source.as_bytes()).ok();
        }
    }
    None
}

/// Concatenated direct `text` children (for `<title>`).
fn direct_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut out = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "text"
            && let Ok(text) = child.utf8_text(source.as_bytes())
        {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// One attribute value: its literal text and the node carrying it.
type AttributeValue<'a> = Option<(&'a str, Option<Node<'a>>)>;

/// Attributes of one tag: literal `(name, value)` pairs in document
/// order. Unquoted and quoted values are both handled; valueless
/// attributes have no value.
struct TagAttributes<'a> {
    values: Vec<(&'a str, AttributeValue<'a>)>,
}

impl<'a> TagAttributes<'a> {
    fn collect(node: Node<'a>, source: &'a str) -> Self {
        let mut values = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "attribute" {
                continue;
            }
            let name = child
                .children(&mut child.walk())
                .find(|part| part.kind() == "attribute_name")
                .and_then(|part| part.utf8_text(source.as_bytes()).ok());
            let Some(name) = name else {
                continue;
            };
            values.push((name, first_attribute_value(child, source)));
        }
        TagAttributes { values }
    }

    /// The literal value of the first attribute named `name`.
    fn value(&self, name: &str) -> Option<&'a str> {
        match self.values.iter().find(|(attr, _)| *attr == name) {
            Some((_, Some((text, _)))) => Some(text),
            _ => None,
        }
    }

    /// The value node of the first attribute named `name`.
    fn value_node(&self, name: &str) -> Option<Node<'a>> {
        match self.values.iter().find(|(attr, _)| *attr == name) {
            Some((_, Some((_, node)))) => *node,
            _ => None,
        }
    }
}

/// The first `attribute_value` descendant of an attribute node, as text
/// and as a node.
fn first_attribute_value<'a>(attribute: Node<'a>, source: &'a str) -> AttributeValue<'a> {
    let mut stack = vec![attribute];
    while let Some(node) = stack.pop() {
        if node.kind() == "attribute_value" {
            return node
                .utf8_text(source.as_bytes())
                .ok()
                .map(|text| (text, Some(node)));
        }
        let child_count = node.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    None
}

/// Bound a skeleton title line.
fn truncate_title(title: &str) -> String {
    if title.len() <= TITLE_LIMIT {
        format!("title {title}")
    } else {
        let mut cut = TITLE_LIMIT;
        while !title.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("title {}...", &title[..cut])
    }
}

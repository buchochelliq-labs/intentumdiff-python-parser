//! Python parser plugin - full-parse mode.
//!
//! Parses source files with tree-sitter-python directly and emits a SemanticNode
//! tree focused on semantically meaningful constructs:
//!   - module, function_definition, class_definition, decorated_definition
//!   - assignment, augmented_assignment, return_statement, import_statement,
//!     if_statement, for_statement, while_statement, try_statement, …
//!
//! Trivia stripped by the host: comment, whitespace (see trivia_node_types).

use intentdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{NodeFacts, SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct PythonParser;

// ---------------------------------------------------------------------------
// Trivia node types to strip (returned to the host)
// ---------------------------------------------------------------------------
const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment", "whitespace"];

// ---------------------------------------------------------------------------
// Semantic node types to preserve
// ---------------------------------------------------------------------------
const SEMANTIC_TYPES: &[&str] = &[
    "module",
    "function_definition",
    "async_function_def",
    "class_definition",
    "decorated_definition",
    "assignment",
    "augmented_assignment",
    "return_statement",
    "import_statement",
    "import_from_statement",
    "if_statement",
    "elif_clause",
    "else_clause",
    "for_statement",
    "while_statement",
    "try_statement",
    "except_clause",
    "with_statement",
    "raise_statement",
    "assert_statement",
    "delete_statement",
    // Trivial statements are REAL body content (issue #41: with pass pruned, `def f(): pass`
    // parsed body-less, so pass -> print(...) had no deletion side and the whole edit
    // vanished into a false style-only).
    "pass_statement",
    "break_statement",
    "continue_statement",
    "ellipsis",
    "expression_statement",
    "call",
    "identifier",
    "string",
    "integer",
    "float",
    "true",
    "false",
    "none",
    "parameters",
    "argument_list",
    "type",
    "type_annotation",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // For compound nodes, use the first identifier child as the label
    for child in &node.children {
        if child.node_type == "identifier" {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_method_def(node_type: &str) -> bool {
    matches!(node_type, "function_definition" | "async_function_def")
}

// ---------------------------------------------------------------------------
// Privacy-safe structural facts (computed from the full CST, before pruning)
// ---------------------------------------------------------------------------

/// The `block` body of a function/class definition, if present.
fn body_block(node: &CstNode) -> Option<&CstNode> {
    node.children.iter().find(|c| c.node_type == "block")
}

/// Whether a raise statement raises `NotImplementedError` (a stub sentinel).
fn raises_not_implemented(stmt: &CstNode) -> bool {
    stmt.walk()
        .any(|d| d.node_type == "identifier" && d.text_or_empty() == "NotImplementedError")
}

/// A no-op statement: `pass`, `...`, a bare docstring, or `raise NotImplementedError`.
fn is_trivial_stmt(stmt: &CstNode) -> bool {
    match stmt.node_type.as_str() {
        "pass_statement" => true,
        "expression_statement" => {
            !stmt.children.is_empty()
                && stmt.children.iter().all(|c| {
                    matches!(
                        c.node_type.as_str(),
                        "string" | "concatenated_string" | "ellipsis"
                    )
                })
        }
        "raise_statement" => raises_not_implemented(stmt),
        _ => false,
    }
}

/// Classify a body block as empty / stub (no-op) / substantive.
fn classify_body(block: &CstNode) -> &'static str {
    if block.children.is_empty() {
        "empty"
    } else if block.children.iter().all(is_trivial_stmt) {
        "stub"
    } else {
        "substantive"
    }
}

/// Compute privacy-safe facts for a Python definition node from its full CST
/// subtree (only counts/enums/flags — never source text, names, or literals).
fn python_node_facts(node: &CstNode) -> NodeFacts {
    let mut facts = NodeFacts::default();
    let is_fn = is_method_def(&node.node_type);
    if is_fn {
        if let Some(params) = node.children.iter().find(|c| c.node_type == "parameters") {
            facts.param_count = Some(params.children.len() as u32);
        }
        if node.node_type == "async_function_def" {
            facts.is_async = Some(true);
        }
        if let Some(block) = body_block(node) {
            facts.body = Some(classify_body(block).to_string());
            let mut returns_value = false;
            let mut is_generator = false;
            for descendant in block.walk() {
                match descendant.node_type.as_str() {
                    "return_statement" if !descendant.children.is_empty() => returns_value = true,
                    "yield" => is_generator = true,
                    _ => {}
                }
            }
            facts.returns = Some(if returns_value { "value" } else { "none" }.to_string());
            if is_generator {
                facts.is_generator = Some(true);
            }
        } else {
            facts.body = Some("empty".to_string());
            facts.returns = Some("none".to_string());
        }
    } else if node.node_type == "class_definition" {
        if let Some(block) = body_block(node) {
            facts.body = Some(classify_body(block).to_string());
        }
    }
    facts
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    // When the current node is a class_definition, its direct children (methods
    // inside the block) should see this class as their enclosing class.
    let own_class_label: Option<String> = if node.node_type == "class_definition" {
        Some(label_for(node))
    } else {
        None
    };
    // Children of a class_definition see the class name; other nodes propagate
    // the existing parent_class context unchanged.
    let child_parent_class: Option<&str> = own_class_label.as_deref().or(parent_class);

    let children: Vec<SemanticNode> = node
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| convert(c, &format!("{}.{}", id_prefix, i), child_parent_class, memo))
        .collect();
    if !is_semantic(&node.node_type) && children.is_empty() {
        return None;
    }

    let hash = structural_hash_with_memo(node, memo);

    let mut builder = SemanticNodeBuilder::new(
        id_prefix,
        &node.node_type,
        label_for(node),
        node.start_line,
        node.start_col,
        node.end_line,
        node.end_col,
        hash,
    )
    .children(children);

    // Tag function/method nodes with the enclosing class name so that the
    // Python host can detect PULL_UP / PUSH_DOWN refactorings.
    if is_method_def(&node.node_type) {
        if let Some(class_name) = parent_class {
            builder = builder.parent_type(class_name);
        }
    }

    // Attach privacy-safe structural facts for definition nodes, computed from the
    // full CST subtree here (the body block is pruned from the semantic tree below,
    // so downstream consumers cannot recover it — this is the only place it exists).
    if is_method_def(&node.node_type) || node.node_type == "class_definition" {
        builder = builder.facts(python_node_facts(node));
    }

    Some(builder.build())
}

fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let children: Vec<CstNode> = (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .map(|child| node_to_cst(child, source))
        .collect();

    let text = if children.is_empty() {
        Some(
            node.utf8_text(source)
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
        )
    } else {
        None
    };

    // tree-sitter-python parses `async def` as a plain `function_definition` whose first
    // (anonymous) child is the `async` keyword; named-children iteration drops it, so the
    // async toggle used to produce byte-identical trees and read as STYLE-ONLY. Emit the
    // `async_function_def` vocabulary (already consumed by NodeFacts `is_async` above and
    // the engine's entity lists) so async-ness survives into the semantic tree.
    let mut node_type = node.kind().to_string();
    if node_type == "function_definition"
        && node.child(0).is_some_and(|child| child.kind() == "async")
    {
        node_type = "async_function_def".to_string();
    }
    CstNode {
        node_type,
        named: node.is_named(),
        text,
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children,
    }
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_python::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load Python grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for PythonParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "python".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.ends_with(".py") || filename.ends_with(".pyi") {
            "python".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "def greet(name):\n    print(\"Hello, \" + name)\n\ndef add(a, b):\n    return a + b\n\nclass Counter:\n    def __init__(self):\n        self.count = 0\n\n    def increment(self):\n        self.count += 1\n".to_string(),
            new: "def greet(name: str) -> None:\n    print(f\"Hello, {name}\")\n\ndef add(x: int, y: int) -> int:\n    return x + y\n\nclass Counter:\n    def __init__(self) -> None:\n        self.count: int = 0\n\n    def increment(self) -> None:\n        self.count += 1\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["python".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(PythonParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!PythonParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = PythonParser::grammar_id();
        let ids = PythonParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = PythonParser::detect_language("test.py".to_string(), "".to_string());
        assert_eq!(r.as_str(), "python");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            PythonParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    /// Find the first node of a given type in a built SemanticNode tree.
    fn find<'a>(node: &'a SemanticNode, node_type: &str) -> Option<&'a SemanticNode> {
        if node.node_type == node_type {
            return Some(node);
        }
        node.children.iter().find_map(|c| find(c, node_type))
    }

    fn facts_of(source: &str, node_type: &str) -> NodeFacts {
        let root = parse_source(source).expect("parse");
        let mut memo = std::collections::HashMap::new();
        let sem = convert(&root, "0", None, &mut memo).expect("semantic tree");
        find(&sem, node_type)
            .and_then(|n| n.facts.clone())
            .unwrap_or_else(|| panic!("no facts on {node_type}"))
    }

    #[test]
    fn noop_stub_has_no_params_returns_none_and_stub_body() {
        let facts = facts_of("def ccc():\n    pass\n", "function_definition");
        assert_eq!(facts.param_count, Some(0), "def ccc() takes no parameters");
        assert_eq!(facts.returns.as_deref(), Some("none"));
        assert_eq!(facts.body.as_deref(), Some("stub"));
    }

    #[test]
    fn async_def_surfaces_as_async_function_def_with_is_async_fact() {
        // `def f()` vs `async def f()` must never be tree-identical: the async toggle changes
        // runtime semantics (call site gets a coroutine). node_to_cst rewrites the kind to
        // async_function_def, which also activates the is_async NodeFact.
        let facts = facts_of("async def f():\n    return 1\n", "async_function_def");
        assert_eq!(facts.is_async, Some(true));
        // And the sync form must NOT produce the async node type.
        let root = parse_source("def f():\n    return 1\n").expect("parse");
        let mut memo = std::collections::HashMap::new();
        let sem = convert(&root, "0", None, &mut memo).expect("semantic tree");
        assert!(find(&sem, "async_function_def").is_none());
        assert!(find(&sem, "function_definition").is_some());
    }

    #[test]
    fn ellipsis_and_docstring_bodies_are_stubs() {
        assert_eq!(
            facts_of("def a():\n    ...\n", "function_definition").body.as_deref(),
            Some("stub")
        );
        assert_eq!(
            facts_of("def a():\n    \"\"\"docs\"\"\"\n", "function_definition").body.as_deref(),
            Some("stub")
        );
        assert_eq!(
            facts_of("def a():\n    raise NotImplementedError\n", "function_definition")
                .body
                .as_deref(),
            Some("stub")
        );
    }

    #[test]
    fn substantive_function_reports_params_and_value_return() {
        let facts = facts_of("def add(a, b):\n    return a + b\n", "function_definition");
        assert_eq!(facts.param_count, Some(2));
        assert_eq!(facts.returns.as_deref(), Some("value"));
        assert_eq!(facts.body.as_deref(), Some("substantive"));
    }

    #[test]
    fn generator_and_bare_return_are_detected() {
        let gen = facts_of("def g():\n    yield 1\n", "function_definition");
        assert_eq!(gen.is_generator, Some(true));
        let bare = facts_of("def f(x):\n    x += 1\n    return\n", "function_definition");
        assert_eq!(bare.returns.as_deref(), Some("none"), "bare return yields None");
    }

    #[test]
    fn definition_facts_serialize_and_non_definitions_have_none() {
        let root = parse_source("x = 1\ndef ccc():\n    pass\n").expect("parse");
        let mut memo = std::collections::HashMap::new();
        let sem = convert(&root, "0", None, &mut memo).expect("tree");
        // A non-definition (assignment) carries no facts.
        assert!(find(&sem, "assignment").and_then(|n| n.facts.clone()).is_none());
        // Facts survive JSON serialisation.
        let json = serde_json::to_string(&sem).expect("json");
        assert!(json.contains("\"facts\""));
        assert!(json.contains("\"body\":\"stub\""));
    }
}

//! Python parser plugin - full-parse mode.
//!
//! Parses source files with tree-sitter-python directly and emits a SemanticNode
//! tree focused on semantically meaningful constructs:
//!   - module, function_definition, class_definition, decorated_definition
//!   - assignment, augmented_assignment, return_statement, import_statement,
//!     if_statement, for_statement, while_statement, try_statement, …
//!
//! Trivia stripped by the host: comment, whitespace (see trivia_node_types).

use intentumdiff_plugin_sdk::{
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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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
    // Anonymous operator / keyword tokens (see OPERATOR_NODE_TYPE).
    OPERATOR_NODE_TYPE,
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

// ---------------------------------------------------------------------------
// Anonymous-token contract (core #229/#230): operators and behaviour-selecting keywords
// ---------------------------------------------------------------------------

/// Node type of the leaf that carries one anonymous operator/keyword token; its text (and
/// so its label) is the token itself. This is the vocabulary core's native Python serializer
/// emits and core's Wasm recovery pass treats as already present.
const OPERATOR_NODE_TYPE: &str = "operator";

/// Node kinds whose anonymous tokens are behaviour-changing operators (mirrors core's
/// `is_operator_bearing`, restricted to what tree-sitter-python produces).
fn is_operator_bearing(kind: &str) -> bool {
    const PARTS: &[&str] = &[
        "binary",
        "unary",
        "operator",
        "assignment",
        "comparison",
        "boolean",
    ];
    PARTS.iter().any(|part| kind.contains(part))
        && !kind.contains("declarat")
        && !kind.contains("definition")
}

const WORD_OPERATORS: &[&str] = &["and", "or", "not", "is", "in"];
const SYMBOL_CHARS: &[char] = &[
    '+', '-', '*', '/', '%', '<', '>', '=', '!', '&', '|', '^', '~', '?', '@',
];

/// Is this whole anonymous token an operator? `is not` / `not in` arrive as one token.
fn is_operator_token(token: &str) -> bool {
    !token.is_empty()
        && (token.chars().all(|c| SYMBOL_CHARS.contains(&c))
            || token
                .split_whitespace()
                .all(|word| WORD_OPERATORS.contains(&word)))
}

/// `async for`, `async with` and `[x async for x in xs]` select a different iteration /
/// context protocol; tree-sitter keeps `async` as an anonymous first child of these.
fn carries_async_keyword(kind: &str) -> bool {
    matches!(kind, "for_statement" | "with_statement" | "for_in_clause")
}

fn token_leaf(token: tree_sitter::Node<'_>) -> CstNode {
    CstNode {
        node_type: OPERATOR_NODE_TYPE.to_string(),
        named: false,
        // The token KIND, not its source slice: `is  not` and `is not` are the same operator,
        // so layout never reaches the label or the hash.
        text: Some(token.kind().to_string()),
        start_line: token.start_position().row as u32,
        start_col: token.start_position().column as u32,
        end_line: token.end_position().row as u32,
        end_col: token.end_position().column as u32,
        children: Vec::new(),
    }
}

fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let operator_bearing = is_operator_bearing(node.kind());
    let async_keyword = carries_async_keyword(node.kind());
    let mut cursor = node.walk();
    let children: Vec<CstNode> = node
        .children(&mut cursor)
        .enumerate()
        .filter_map(|(i, child)| {
            if child.is_named() {
                Some(node_to_cst(child, source))
            } else if (operator_bearing && is_operator_token(child.kind()))
                || (async_keyword && i == 0 && child.kind() == "async")
            {
                Some(token_leaf(child))
            } else {
                None
            }
        })
        .collect();

    let text = if children.is_empty() {
        // Semantic hashes must include the complete leaf, including long literal tails.
        Some(node.utf8_text(source).unwrap_or("").to_owned())
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
    use intentumdiff_plugin_sdk::testing as t;

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

    // ---- Complete-payload contract (issue #4): no local 4096-character leaf cap ----

    fn root_hash(source: &str) -> String {
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        tree["structural_hash"].as_str().unwrap().to_string()
    }

    #[test]
    fn long_string_suffix_changes_root_hash() {
        // The issue's exact control: x = "a"*n + X vs Y, n = 120 / 4096 / 8192, plus the
        // boundary neighbours and a multibyte prefix that crosses the old character cap.
        for n in [120, 4095, 4096, 4097, 8192, 32768] {
            for prefix in ["a".repeat(n), "é🦀".repeat(n)] {
                let old = format!("x = \"{prefix}X\"\n");
                let new = format!("x = \"{prefix}Y\"\n");
                assert_ne!(root_hash(&old), root_hash(&new), "prefix length {n}");
                assert_eq!(process_impl(&old), process_impl(&old), "deterministic");
            }
        }
    }

    #[test]
    fn long_string_leaf_keeps_complete_utf8_text_and_byte_span() {
        for payload in [format!("{}X", "a".repeat(8192)), format!("{}X", "é界🦀".repeat(4200))] {
            let source = format!("x = \"{payload}\"\n");
            let root = parse_source(&source).unwrap();
            let leaf = root
                .walk()
                .find(|n| n.node_type == "string_content")
                .expect("string_content leaf");
            assert_eq!(leaf.text.as_deref(), Some(payload.as_str()));
            assert_eq!((leaf.start_line, leaf.start_col), (0, 5));
            assert_eq!((leaf.end_line, leaf.end_col), (0, 5 + payload.len() as u32));
            assert!(source.is_char_boundary(leaf.start_col as usize));
            assert!(source.is_char_boundary(leaf.end_col as usize));
            assert_eq!(&source[leaf.start_col as usize..leaf.end_col as usize], payload);
        }
    }

    #[test]
    fn long_integer_and_identifier_suffixes_change_root_hash() {
        let digits = "7".repeat(5000);
        assert_ne!(root_hash(&format!("x = {digits}1\n")), root_hash(&format!("x = {digits}2\n")));
        let name = "n".repeat(5000);
        assert_ne!(root_hash(&format!("{name}a = 1\n")), root_hash(&format!("{name}b = 1\n")));
    }

    #[test]
    fn unchanged_long_string_keeps_hash_across_spacing() {
        let payload = "é🦀".repeat(4200);
        assert_eq!(
            root_hash(&format!("x = \"{payload}\"\n")),
            root_hash(&format!("x   =   \"{payload}\"\n"))
        );
    }

    // ---- Anonymous-token contract (issue #4 / core #230): operators are emitted ----

    fn operator_leaves(source: &str) -> Vec<(String, (u32, u32, u32, u32))> {
        fn collect(node: &SemanticNode, out: &mut Vec<(String, (u32, u32, u32, u32))>) {
            if node.node_type == OPERATOR_NODE_TYPE {
                let p = &node.position;
                out.push((node.label.clone(), (p.start_line, p.start_col, p.end_line, p.end_col)));
            }
            node.children.iter().for_each(|c| collect(c, out));
        }
        let tree: SemanticNode = serde_json::from_str(&process_impl(source)).unwrap();
        let mut out = Vec::new();
        collect(&tree, &mut out);
        out
    }

    #[test]
    fn binary_operator_is_emitted_with_its_source_span() {
        let old = "def f(a,b):\n    return a + b\n";
        let new = "def f(a,b):\n    return a - b\n";
        assert_eq!(operator_leaves(old), vec![("+".to_string(), (1, 13, 1, 14))]);
        assert_eq!(operator_leaves(new), vec![("-".to_string(), (1, 13, 1, 14))]);
        assert_ne!(root_hash(old), root_hash(new));
        // The function's identity label is untouched by the operator leaf.
        let tree: serde_json::Value = serde_json::from_str(&process_impl(old)).unwrap();
        assert_eq!(tree["children"][0]["label"], "f");
    }

    #[test]
    fn every_python_operator_family_changes_the_hash() {
        for (old, new) in [
            ("x = a < b\n", "x = a > b\n"),
            ("x = a is b\n", "x = a is not b\n"),
            ("x = a in b\n", "x = a not in b\n"),
            ("x = a and b\n", "x = a or b\n"),
            ("x = -y\n", "x = ~y\n"),
            ("x = not y\n", "x = y\n"),
            ("x += 1\n", "x -= 1\n"),
            ("x = a * b\n", "x = a @ b\n"),
            ("x = a // b\n", "x = a / b\n"),
            ("x = a ** b\n", "x = a * b\n"),
        ] {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} vs {new:?}");
        }
    }

    #[test]
    fn operator_tokens_ignore_layout() {
        assert_eq!(root_hash("x = a is not b\n"), root_hash("x = a is   not b\n"));
        // (Comments are trivia stripped host-side via trivia_node_types, not here.)
        assert_eq!(root_hash("x = a+b\n"), root_hash("x = a  +  b\n"));
        assert_eq!(operator_leaves("x = a is   not b\n")[1].0, "is not");
    }

    #[test]
    fn punctuation_and_parameter_counts_are_not_operator_leaves() {
        // `(`, `,`, `:` and keyword-argument `=` are not operators; param_count unchanged.
        assert!(operator_leaves("f(a, b=1)\n").is_empty());
        let facts = facts_of("def g(a, b=1, *c, **d):\n    pass\n", "function_definition");
        assert_eq!(facts.param_count, Some(4));
    }

    #[test]
    fn async_for_and_with_keep_the_async_keyword() {
        for (sync, asynchronous) in [
            (
                "async def f(xs):\n    for x in xs:\n        pass\n",
                "async def f(xs):\n    async for x in xs:\n        pass\n",
            ),
            (
                "async def f(c):\n    with c:\n        pass\n",
                "async def f(c):\n    async with c:\n        pass\n",
            ),
            (
                "async def f(xs):\n    return [x for x in xs]\n",
                "async def f(xs):\n    return [x async for x in xs]\n",
            ),
        ] {
            assert_ne!(root_hash(sync), root_hash(asynchronous), "{asynchronous:?}");
            assert!(operator_leaves(asynchronous).iter().any(|(t, _)| t == "async"));
        }
    }
}

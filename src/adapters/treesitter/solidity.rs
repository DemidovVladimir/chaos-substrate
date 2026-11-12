//! Solidity `LangSpec`: contracts / interfaces / libraries as containers,
//! functions, constructors, `receive` / `fallback`, modifiers, events, custom
//! errors, state variables, file-level constants, structs, enums and
//! user-defined value types; `import` directives (with local alias names);
//! calls with receivers typed inside the file (casts, state variables,
//! parameters, locals, mapping values, `using … for`), modifier invocations
//! and `new X(…)` as call candidates; `emit` / `revert` / `require(…, E())`
//! as `Emits` / `Reverts` relations; `is` inheritance as `Extends`; Foundry
//! `vm.env*` reads as env-var surfaces; and package names from non-relative
//! import paths.
//!
//! Metadata written per symbol (the contract the ABI consumers read):
//! * contract / interface / library: `header`, `bases` (ancestor names as
//!   written in `is`), `abstract`, `library`.
//! * functions, constructors, `receive` / `fallback`, modifiers: `header`,
//!   `visibility`, `mutability`, `modifiers`, `guards`, `params`
//!   (`[{name,type,location}]`), `returns` (`[{name,type}]`), `virtual`,
//!   `override`, `special`.
//! * state variables (`Field`): `state_var`, `visibility` (default
//!   `internal`), `type`, `constant` / `immutable`, `value`, `header`; public
//!   ones `getter`, `mutability: "view"`, `params` (the getter's inputs:
//!   mapping keys, `uint256` per array dimension) and `returns`.
//! * file-level constants: `constant`, `type`, `value`, `header`.
//! * errors: `error`, `params` (`[{name,type}]`); events: `event`, `params`
//!   (`[{name,type,indexed}]`), `anonymous`.
//! * structs: `struct`, `members`; user-defined value types: `udvt`,
//!   `underlying`.
//! * any documented symbol: `natspec` (`title`, `notice`, `dev`, `params`,
//!   `returns`, `inheritdoc`, `custom`) parsed from its doc comment.

use super::lang::{DefRule, LangSpec};
use super::walker::{enclosing_symbol, text};
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{
    CallCandidate, FileExtraction, FileInput, ImportRef, Relation, Surface, SurfaceKind, Symbol,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Tree};

const fn rule(
    node_type: &'static str,
    kind: NodeKind,
    container: bool,
    method_when_nested: bool,
) -> DefRule {
    DefRule {
        node_type,
        kind,
        name_field: "name",
        container,
        methods_inside: container,
        method_when_nested,
    }
}

const DEFS: &[DefRule] = &[
    rule("contract_declaration", NodeKind::Type, true, false),
    rule("interface_declaration", NodeKind::Interface, true, false),
    rule("library_declaration", NodeKind::Type, true, false),
    rule("function_definition", NodeKind::Function, false, true),
    rule("modifier_definition", NodeKind::Function, false, false),
    rule("event_definition", NodeKind::Field, false, false),
    rule("struct_declaration", NodeKind::Type, false, false),
    rule("enum_declaration", NodeKind::Enum, false, false),
    rule("state_variable_declaration", NodeKind::Field, false, false),
    rule(
        "constant_variable_declaration",
        NodeKind::Field,
        false,
        false,
    ),
    rule("error_declaration", NodeKind::Field, false, false),
    rule(
        "user_defined_type_definition",
        NodeKind::TypeAlias,
        false,
        false,
    ),
];

/// One match per imported name; `merge_imports` rebuilds one import per
/// directive from the tree.
const IMPORT_QUERY: &str = r#"
(import_directive import_name: (identifier) @name source: (string) @specifier)
(import_directive source: (string) @specifier)
"#;

const CALL_QUERY: &str = r#"
(call_expression function: (expression (identifier) @callee))
(call_expression function: (expression (member_expression object: (_) @receiver property: (identifier) @callee)))
"#;

static SPEC: LangSpec = LangSpec {
    language: || tree_sitter_solidity::LANGUAGE.into(),
    defs: DEFS,
    import_query: IMPORT_QUERY,
    call_query: CALL_QUERY,
    doc_comment_types: &["comment"],
    body_types: &["contract_body", "function_body", "struct_body", "enum_body"],
    extras: &[
        nameless_functions,
        metadata_and_inheritance,
        keep_top_level_fields_in_head,
        typed_receivers,
        modifier_calls,
        creations,
        emits_and_reverts,
        foundry_env_vars,
        merge_imports,
        packages,
        natspec,
    ],
};

/// The Solidity language specification.
pub fn spec() -> Option<&'static LangSpec> {
    Some(&SPEC)
}

/// Function-like declarations: what a function scope (parameters, locals)
/// and a modifier list belong to.
const FN_KINDS: &[&str] = &[
    "function_definition",
    "modifier_definition",
    "constructor_definition",
    "fallback_receive_definition",
];

const CONTAINER_KINDS: &[&str] = &[
    "contract_declaration",
    "interface_declaration",
    "library_declaration",
];

/// `max` characters kept of a constant / immutable initializer.
const MAX_VALUE: usize = 120;

fn walk<'t>(tree: &'t Tree, mut f: impl FnMut(Node<'t>)) {
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

fn symbol_at(out: &FileExtraction, node: Node) -> Option<usize> {
    out.symbols
        .iter()
        .position(|s| s.byte_start == node.start_byte())
}

/// Whitespace-normalized text: every run of whitespace becomes one space.
fn ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip single-child `expression` wrappers.
fn unwrap_expr(mut n: Node) -> Node {
    while n.kind() == "expression" && n.named_child_count() == 1 {
        match n.named_child(0) {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

fn named_children(n: Node) -> Vec<Node> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}

fn has_token(n: Node, token: &str) -> bool {
    let mut cursor = n.walk();
    let found = n
        .children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token);
    found
}

fn enclosing_of_kind<'t>(mut n: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    while let Some(p) = n.parent() {
        if kinds.contains(&p.kind()) {
            return Some(p);
        }
        n = p;
    }
    None
}

/// `constructor(...)`, `receive()` and `fallback()` have no name field, so
/// the generic walk skips them: add a `Method` named `constructor`, `receive`
/// or `fallback` (old-style `function()` is a fallback) under its contract
/// and re-home its calls.
fn nameless_functions(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut added: Vec<Symbol> = Vec::new();
    walk(tree, |n| {
        let name = match n.kind() {
            "constructor_definition" => "constructor",
            "fallback_receive_definition" => match n.child(0).map(|c| c.kind()) {
                Some("receive") => "receive",
                _ => "fallback",
            },
            _ => return,
        };
        let Some(contract) = n.parent().and_then(|b| b.parent()) else {
            return;
        };
        let Some(parent_idx) = symbol_at(out, contract) else {
            return;
        };
        let parent = &out.symbols[parent_idx];
        let qualified = format!("{}::{name}", parent.qualified_name);
        let base = format!("solidity:{}:method:{qualified}", file.path);
        let taken = |id: &str| {
            out.symbols.iter().any(|s| s.stable_id == id) || added.iter().any(|s| s.stable_id == id)
        };
        let mut stable_id = base.clone();
        let mut k = 1;
        while taken(&stable_id) {
            k += 1;
            stable_id = format!("{base}#{k}");
        }
        let body_start = n
            .child_by_field_name("body")
            .map(|b| b.start_byte())
            .unwrap_or(n.end_byte());
        let signature: String = src[n.start_byte()..body_start]
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('{')
            .trim()
            .trim_end_matches(';')
            .chars()
            .take(200)
            .collect();
        added.push(Symbol {
            stable_id,
            kind: NodeKind::Method,
            name: name.into(),
            qualified_name: qualified,
            parent: Some(parent.stable_id.clone()),
            signature: Some(signature),
            doc: preceding_comments(n, src),
            line_start: n.start_position().row as u32 + 1,
            line_end: n.end_position().row as u32 + 1,
            byte_start: n.start_byte(),
            byte_end: n.end_byte(),
            segments: n
                .child_by_field_name("body")
                .map(|b| {
                    named_children(b)
                        .iter()
                        .map(|c| (c.start_byte(), c.end_byte()))
                        .collect()
                })
                .unwrap_or_default(),
            metadata: json!({}),
        });
    });
    for sym in added {
        for call in &mut out.calls {
            if call.line >= sym.line_start
                && call.line <= sym.line_end
                && call.caller == sym.parent.clone().unwrap_or_default()
            {
                call.caller = sym.stable_id.clone();
            }
        }
        out.symbols.push(sym);
    }
    out.symbols.sort_by_key(|s| s.byte_start);
}

/// Contiguous `comment` siblings directly above `node`, markers stripped.
fn preceding_comments(node: Node, src: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut expected_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "comment" || p.end_position().row + 1 < expected_row {
            break;
        }
        let cleaned: Vec<String> = text(p, src)
            .lines()
            .rev()
            .map(|l| {
                let mut s = l.trim();
                for prefix in ["///", "//", "/**", "/*", "*/", "*"] {
                    if let Some(rest) = s.strip_prefix(prefix) {
                        s = rest;
                        break;
                    }
                }
                s.trim_end_matches("*/").trim().to_string()
            })
            .collect();
        lines.extend(cleaned);
        expected_row = p.start_position().row;
        prev = p.prev_named_sibling();
    }
    lines.reverse();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then(|| doc.chars().take(1000).collect())
}

/// Byte where the contiguous comment block directly above `node` starts (the
/// same rule as [`preceding_comments`] and the walker's doc ownership).
fn leading_comment_start(node: Node) -> usize {
    let mut start = node.start_byte();
    let mut expected_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "comment" || p.end_position().row + 1 < expected_row {
            break;
        }
        start = p.start_byte();
        expected_row = p.start_position().row;
        prev = p.prev_named_sibling();
    }
    start
}

/// Declaration text from `node` up to `end`, whitespace-joined and without
/// comments, without a trailing `;` / `=` / `{`.
fn header_until(node: Node, end: usize, src: &str) -> String {
    code_text(node, end, src)
        .trim_end_matches(['{', ';', '='])
        .trim()
        .to_string()
}

/// The source of `node` up to `end`, every `comment` inside it replaced by a
/// space, whitespace-joined: a `// commented-out` base or note never reads
/// as code once the lines are joined.
fn code_text(node: Node, end: usize, src: &str) -> String {
    let start = node.start_byte();
    let end = end.clamp(start, src.len());
    let mut comments: Vec<(usize, usize)> = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.end_byte() <= start || n.start_byte() >= end {
            continue;
        }
        if n.kind() == "comment" {
            comments.push((n.start_byte(), n.end_byte()));
            continue;
        }
        let mut cursor = n.walk();
        stack.extend(n.children(&mut cursor));
    }
    if comments.is_empty() {
        return ws(&src[start..end]);
    }
    comments.sort_unstable();
    let mut out = String::new();
    let mut at = start;
    for (s, e) in comments {
        if s > at {
            out.push_str(&src[at..s.min(end)]);
        }
        out.push(' ');
        at = at.max(e);
    }
    if at < end {
        out.push_str(&src[at..end]);
    }
    ws(&out)
}

/// A type node's text without comments, whitespace-joined.
fn type_text(t: Node, src: &str) -> String {
    code_text(t, t.end_byte(), src)
}

/// `[{name, type, location}]` of the `parameter` children of `node`; with
/// `after_returns`, only those after (`true`) or before (`false`) a
/// `returns` keyword among its children.
fn params_of(node: Node, src: &str, after_returns: Option<bool>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut past_returns = false;
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if !c.is_named() && c.kind() == "returns" {
            past_returns = true;
            continue;
        }
        if c.kind() != "parameter" || after_returns.is_some_and(|a| a != past_returns) {
            continue;
        }
        out.push(param_json(c, src, true));
    }
    out
}

fn param_json(p: Node, src: &str, with_location: bool) -> Value {
    let name = p
        .child_by_field_name("name")
        .map_or(String::new(), |n| text(n, src).trim().to_string());
    let ty = p
        .child_by_field_name("type")
        .map_or(String::new(), |t| type_text(t, src));
    if !with_location {
        return json!({"name": name, "type": ty});
    }
    let location = p
        .child_by_field_name("location")
        .map_or(String::new(), |l| text(l, src).trim().to_string());
    json!({"name": name, "type": ty, "location": location})
}

/// A `type_name` that is an array (`T[]`, `T[N]`): its element type.
fn array_element(t: Node) -> Option<Node> {
    if !has_token(t, "[") {
        return None;
    }
    named_children(t)
        .into_iter()
        .find(|c| c.kind() == "type_name")
}

/// One mapping / array level down: the mapping value type or the array
/// element type, with the getter input that indexes it.
fn index_level<'t>(t: Node<'t>, src: &str) -> Option<(Node<'t>, Value)> {
    if let Some(value) = t.child_by_field_name("value_type") {
        let key = t
            .child_by_field_name("key_type")
            .map_or(String::new(), |k| type_text(k, src));
        let name = t
            .child_by_field_name("key_identifier")
            .map_or(String::new(), |k| text(k, src).trim().to_string());
        return Some((value, json!({"name": name, "type": key})));
    }
    array_element(t).map(|e| (e, json!({"name": "", "type": "uint256"})))
}

/// `metadata.library/modifier/event/error/state_var/...` flags, function
/// access (`visibility`, `mutability`, `modifiers`, `guards` — what makes a
/// function part of a contract's public ABI and who may call it), parameter
/// lists, and `is A, B` → `Extends`. See the module docs for every key.
fn metadata_and_inheritance(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    walk(tree, |n| {
        let kind = n.kind();
        let relevant = FN_KINDS.contains(&kind)
            || CONTAINER_KINDS.contains(&kind)
            || matches!(
                kind,
                "event_definition"
                    | "error_declaration"
                    | "state_variable_declaration"
                    | "constant_variable_declaration"
                    | "user_defined_type_definition"
                    | "struct_declaration"
            );
        if !relevant {
            return;
        }
        let Some(idx) = symbol_at(out, n) else {
            return;
        };
        if FN_KINDS.contains(&kind) {
            function_metadata(n, src, &mut out.symbols[idx].metadata);
            return;
        }
        let meta = &mut out.symbols[idx].metadata;
        match kind {
            "event_definition" => {
                meta["event"] = json!(true);
                let params: Vec<Value> = named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "event_parameter")
                    .map(|p| {
                        let mut v = param_json(p, src, false);
                        v["indexed"] = json!(has_token(p, "indexed"));
                        v
                    })
                    .collect();
                meta["params"] = json!(params);
                if has_token(n, "anonymous") {
                    meta["anonymous"] = json!(true);
                }
                meta["header"] = json!(header_until(n, n.end_byte(), src));
            }
            "error_declaration" => {
                meta["error"] = json!(true);
                let params: Vec<Value> = named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "error_parameter")
                    .map(|p| param_json(p, src, false))
                    .collect();
                meta["params"] = json!(params);
                meta["header"] = json!(header_until(n, n.end_byte(), src));
            }
            "state_variable_declaration" | "constant_variable_declaration" => {
                let constant = kind == "constant_variable_declaration" || has_token(n, "constant");
                let immutable = named_children(n).iter().any(|c| c.kind() == "immutable");
                let ty = n.child_by_field_name("type");
                if kind == "state_variable_declaration" {
                    meta["state_var"] = json!(true);
                    let vis = n
                        .child_by_field_name("visibility")
                        .map_or("internal".to_string(), |v| text(v, src).trim().to_string());
                    if vis == "public" {
                        meta["getter"] = json!(true);
                        meta["mutability"] = json!("view");
                        let mut inputs = Vec::new();
                        let mut t = ty;
                        while let Some((next, input)) = t.and_then(|t| index_level(t, src)) {
                            inputs.push(input);
                            t = Some(next);
                        }
                        meta["params"] = json!(inputs);
                        let ret = t.map_or(String::new(), |t| type_text(t, src));
                        meta["returns"] = json!([{"name": "", "type": ret}]);
                    }
                    meta["visibility"] = json!(vis);
                }
                if constant {
                    meta["constant"] = json!(true);
                }
                if immutable {
                    meta["immutable"] = json!(true);
                }
                if let Some(t) = ty {
                    meta["type"] = json!(type_text(t, src));
                }
                let value = n.child_by_field_name("value");
                if let Some(v) = value.filter(|_| constant || immutable) {
                    meta["value"] =
                        json!(ws(text(v, src)).chars().take(MAX_VALUE).collect::<String>());
                }
                let end = value
                    .map(|v| v.start_byte())
                    .or_else(|| n.child_by_field_name("name").map(|x| x.end_byte()))
                    .unwrap_or(n.end_byte());
                meta["header"] = json!(header_until(n, end, src));
            }
            "user_defined_type_definition" => {
                meta["udvt"] = json!(true);
                if let Some(p) = named_children(n)
                    .into_iter()
                    .find(|c| c.kind() == "primitive_type")
                {
                    meta["underlying"] = json!(ws(text(p, src)));
                }
            }
            "struct_declaration" => {
                meta["struct"] = json!(true);
                let members: Vec<Value> = n
                    .child_by_field_name("body")
                    .map(named_children)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| c.kind() == "struct_member")
                    .map(|m| param_json(m, src, false))
                    .collect();
                meta["members"] = json!(members);
            }
            _ => {
                // contract / interface / library
                if kind == "library_declaration" {
                    meta["library"] = json!(true);
                }
                if n.child(0).is_some_and(|c| c.kind() == "abstract") {
                    meta["abstract"] = json!(true);
                }
                let end = n
                    .child_by_field_name("body")
                    .map_or(n.end_byte(), |b| b.start_byte());
                meta["header"] = json!(header_until(n, end, src));
                let bases: Vec<String> = named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "inheritance_specifier")
                    .filter_map(|s| s.child_by_field_name("ancestor"))
                    .map(|a| ws(text(a, src)).replace(' ', ""))
                    .collect();
                if !bases.is_empty() {
                    meta["bases"] = json!(bases);
                }
                let source = out.symbols[idx].stable_id.clone();
                for target_name in bases {
                    out.relations.push(Relation {
                        source: source.clone(),
                        kind: EdgeKind::Extends,
                        target_name,
                    });
                }
            }
        }
    });
}

fn function_metadata(n: Node, src: &str, meta: &mut Value) {
    let kind = n.kind();
    // One-line header up to the body: multi-line parameter lists are cut by
    // the generic first-line signature.
    let end = n
        .child_by_field_name("body")
        .map_or(n.end_byte(), |b| b.start_byte());
    meta["header"] = json!(header_until(n, end, src));
    if kind == "modifier_definition" {
        meta["modifier"] = json!(true);
    }
    if kind == "fallback_receive_definition" {
        let special = match n.child(0).map(|c| c.kind()) {
            Some("receive") => "receive",
            _ => "fallback",
        };
        meta["special"] = json!(special);
    }
    let mut modifiers: Vec<String> = Vec::new();
    let mut guards: Vec<String> = Vec::new();
    let mut cursor = n.walk();
    for c in n.children(&mut cursor) {
        let t = text(c, src).trim().to_string();
        if !c.is_named() {
            // constructor_definition keeps these as bare keywords.
            match c.kind() {
                "payable" if kind == "constructor_definition" => {
                    meta["mutability"] = json!("payable")
                }
                "internal" | "public" if kind == "constructor_definition" => {
                    meta["visibility"] = json!(t)
                }
                _ => {}
            }
            continue;
        }
        match c.kind() {
            "visibility" => meta["visibility"] = json!(t),
            "state_mutability" => meta["mutability"] = json!(t),
            "virtual" => meta["virtual"] = json!(true),
            "override_specifier" => {
                let names: Vec<String> = named_children(c)
                    .into_iter()
                    .filter(|u| u.kind() != "comment")
                    .map(|u| ws(text(u, src)).replace(' ', ""))
                    .collect();
                meta["override"] = if names.is_empty() {
                    json!(true)
                } else {
                    json!(names)
                };
            }
            "modifier_invocation" => {
                let name = t.split('(').next().unwrap_or("").trim().to_string();
                if !name.is_empty() {
                    modifiers.push(name);
                }
                guards.push(ws(&t));
            }
            _ => {}
        }
    }
    if !modifiers.is_empty() {
        meta["modifiers"] = json!(modifiers);
        meta["guards"] = json!(guards);
    }
    let (params, returns) = match kind {
        "function_definition" => (
            params_of(n, src, None),
            n.child_by_field_name("return_type")
                .map(|r| params_of(r, src, None))
                .unwrap_or_default(),
        ),
        "fallback_receive_definition" => (
            params_of(n, src, Some(false)),
            params_of(n, src, Some(true)),
        ),
        _ => (params_of(n, src, None), Vec::new()),
    };
    meta["params"] = json!(params);
    if kind != "constructor_definition" && kind != "modifier_definition" {
        let returns: Vec<Value> = returns
            .into_iter()
            .map(|r| json!({"name": r["name"], "type": r["type"]}))
            .collect();
        meta["returns"] = json!(returns);
    }
}

/// File-level constants, errors and events are symbols (so they resolve and
/// get selectors) but no chunk unit: put their text, with the comment block
/// above, back into the `file_head` ranges so search still reaches them.
fn keep_top_level_fields_in_head(tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    let starts: HashSet<usize> = out
        .symbols
        .iter()
        .filter(|s| s.parent.is_none() && s.kind == NodeKind::Field)
        .map(|s| s.byte_start)
        .collect();
    if starts.is_empty() {
        return;
    }
    for n in named_children(tree.root_node()) {
        if starts.contains(&n.start_byte()) {
            out.free_ranges
                .push((leading_comment_start(n), n.end_byte()));
        }
    }
    out.free_ranges.sort_unstable();
    out.free_ranges.dedup();
}

/// name → declared type node; `None` when the name is bound to two
/// different types in the same scope (never guessed).
type Bindings<'t> = HashMap<String, Option<Node<'t>>>;

fn bind<'t>(b: &mut Bindings<'t>, decl: Node<'t>, src: &str) {
    let (Some(name), Some(ty)) = (
        decl.child_by_field_name("name"),
        decl.child_by_field_name("type"),
    ) else {
        return;
    };
    let name = text(name, src).trim().to_string();
    match b.get(&name) {
        None => {
            b.insert(name, Some(ty));
        }
        Some(Some(prev)) if ws(text(*prev, src)) == ws(text(ty, src)) => {}
        Some(_) => {
            b.insert(name, None);
        }
    }
}

/// Parameters, return parameters and every local of a function-like node.
fn fn_bindings<'t>(f: Node<'t>, src: &str) -> Bindings<'t> {
    let mut b = Bindings::new();
    let mut stack = vec![f];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "parameter" | "variable_declaration") {
            bind(&mut b, n, src);
            continue;
        }
        stack.extend(named_children(n));
    }
    b
}

/// One `using L for T` (or `using {f, g} for T`): `target` `None` = `*`.
struct UsingFor {
    library: Option<String>,
    functions: Vec<String>,
    target: Option<String>,
}

fn using_directives(parent: Node, src: &str) -> Vec<UsingFor> {
    named_children(parent)
        .into_iter()
        .filter(|c| c.kind() == "using_directive")
        .map(|u| {
            let kids = named_children(u);
            let library = kids
                .iter()
                .find(|k| k.kind() == "type_alias")
                .map(|k| ws(text(*k, src)).replace(' ', ""));
            let functions = kids
                .iter()
                .filter(|k| k.kind() == "using_alias" && !has_token(**k, "as"))
                .filter_map(|k| named_children(*k).into_iter().next())
                .map(|t| last_identifier(t, src))
                .collect();
            let target = u
                .child_by_field_name("source")
                .filter(|s| s.kind() != "any_source_type")
                .map(|s| ws(text(s, src)));
            UsingFor {
                library,
                functions,
                target,
            }
        })
        .collect()
}

/// Last `identifier` of a dotted path node (`A.B.C` → `C`), else its text.
fn last_identifier(n: Node, src: &str) -> String {
    named_children(n)
        .into_iter()
        .filter(|c| c.kind() == "identifier")
        .next_back()
        .map_or_else(
            || text(n, src).trim().to_string(),
            |i| text(i, src).trim().to_string(),
        )
}

/// The type a call receiver evaluates to, from the bindings in scope:
/// `(type name to use as receiver, type node)`. A cast `IFoo(x)` gives
/// `IFoo` with no node.
fn receiver_type<'t>(
    obj: Node<'t>,
    src: &str,
    lookup: &dyn Fn(&str) -> Option<Node<'t>>,
) -> Option<(Option<String>, Option<Node<'t>>)> {
    let obj = unwrap_expr(obj);
    match obj.kind() {
        "call_expression" => {
            let f = unwrap_expr(obj.child_by_field_name("function")?);
            let name = text(f, src).trim();
            (f.kind() == "identifier" && name.starts_with(|c: char| c.is_ascii_uppercase()))
                .then(|| (Some(name.to_string()), None))
        }
        "identifier" | "array_access" => {
            let mut depth = 0;
            let mut base = obj;
            while base.kind() == "array_access" {
                base = unwrap_expr(base.child_by_field_name("base")?);
                depth += 1;
            }
            if base.kind() != "identifier" {
                return None;
            }
            let mut ty = lookup(text(base, src).trim())?;
            for _ in 0..depth {
                ty = index_level(ty, src)?.0;
            }
            let user = named_children(ty)
                .into_iter()
                .find(|c| c.kind() == "user_defined_type")
                .filter(|_| ty.named_child_count() == 1)
                .map(|u| last_identifier(u, src));
            Some((user, Some(ty)))
        }
        _ => None,
    }
}

/// Type call receivers inside the file: a cast `IFoo(x).f()` gets receiver
/// `IFoo`; a state variable, constant, parameter, local, mapping value or
/// array element of a user-defined type gets that type's name. A name bound
/// to two different types in one function stays untyped. `using L for T`
/// adds a second candidate with receiver `L` when the receiver's type is
/// elementary or a struct / value type declared in this file; `using {f}
/// for T` adds a bare `f` candidate.
fn typed_receivers(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let root = tree.root_node();
    let mut file_scope = Bindings::new();
    let mut local_types: HashSet<String> = HashSet::new();
    walk(tree, |n| match n.kind() {
        "constant_variable_declaration" if n.parent().is_some_and(|p| p.id() == root.id()) => {
            bind(&mut file_scope, n, src)
        }
        "struct_declaration" | "user_defined_type_definition" => {
            if let Some(name) = n.child_by_field_name("name") {
                local_types.insert(text(name, src).trim().to_string());
            }
        }
        _ => {}
    });
    let file_using = using_directives(root, src);
    let mut contract_scopes: HashMap<usize, (Bindings, Vec<UsingFor>)> = HashMap::new();
    let mut fn_scopes: HashMap<usize, Bindings> = HashMap::new();
    let mut rewrites: Vec<(String, u32, String, String)> = Vec::new();
    let mut extra: Vec<(String, u32, Option<String>, String)> = Vec::new();
    walk(tree, |call| {
        if call.kind() != "call_expression" {
            return;
        }
        let Some(member) = call
            .child_by_field_name("function")
            .map(unwrap_expr)
            .filter(|f| f.kind() == "member_expression")
        else {
            return;
        };
        let (Some(obj), Some(prop)) = (
            member.child_by_field_name("object"),
            member.child_by_field_name("property"),
        ) else {
            return;
        };
        let contract = enclosing_of_kind(call, CONTAINER_KINDS);
        let function = enclosing_of_kind(call, FN_KINDS);
        if let Some(c) = contract {
            contract_scopes.entry(c.id()).or_insert_with(|| {
                let mut b = Bindings::new();
                let body = c.child_by_field_name("body");
                for d in body.map(named_children).unwrap_or_default() {
                    if d.kind() == "state_variable_declaration" {
                        bind(&mut b, d, src);
                    }
                }
                (
                    b,
                    body.map(|b| using_directives(b, src)).unwrap_or_default(),
                )
            });
        }
        if let Some(f) = function {
            fn_scopes
                .entry(f.id())
                .or_insert_with(|| fn_bindings(f, src));
        }
        let fn_b = function.and_then(|f| fn_scopes.get(&f.id()));
        let ct = contract.and_then(|c| contract_scopes.get(&c.id()));
        let lookup = |name: &str| -> Option<Node> {
            if let Some(hit) = fn_b.and_then(|b| b.get(name)) {
                return *hit;
            }
            if let Some(hit) = ct.and_then(|(b, _)| b.get(name)) {
                return *hit;
            }
            file_scope.get(name).copied().flatten()
        };
        let Some((typed, ty)) = receiver_type(obj, src, &lookup) else {
            return;
        };
        let callee = text(prop, src).to_string();
        let line = prop.start_position().row as u32 + 1;
        let receiver = text(obj, src).trim().to_string();
        if let Some(t) = typed.clone() {
            rewrites.push((callee.clone(), line, receiver.clone(), t));
        }
        let Some(ty) = ty else {
            return;
        };
        let attachable = typed.as_ref().is_none_or(|t| local_types.contains(t));
        let ty_text = ws(text(ty, src));
        let usings = ct
            .map(|(_, u)| u.as_slice())
            .unwrap_or(&[])
            .iter()
            .chain(&file_using);
        for u in usings {
            if u.target.as_ref().is_some_and(|t| *t != ty_text) {
                continue;
            }
            if let Some(lib) = u.library.as_ref().filter(|_| attachable) {
                extra.push((callee.clone(), line, Some(lib.clone()), receiver.clone()));
            }
            if u.functions.contains(&callee) {
                extra.push((callee.clone(), line, None, receiver.clone()));
            }
        }
    });
    for (callee, line, receiver, typed) in rewrites {
        for c in out.calls.iter_mut().filter(|c| {
            c.callee == callee && c.line == line && c.receiver.as_deref() == Some(receiver.as_str())
        }) {
            c.receiver = Some(typed.clone());
        }
    }
    for (callee, line, receiver, original) in extra {
        let caller = out
            .calls
            .iter()
            .find(|c| c.callee == callee && c.line == line)
            .map(|c| c.caller.clone());
        let Some(caller) = caller else {
            continue;
        };
        let dup = out.calls.iter().any(|c| {
            c.callee == callee && c.line == line && c.receiver == receiver && c.caller == caller
        });
        if !dup && original != receiver.clone().unwrap_or_default() {
            out.calls.push(CallCandidate {
                caller,
                callee,
                receiver,
                line,
            });
        }
    }
    out.calls.sort_by_key(|c| c.line);
}

/// Every modifier invocation of a function, constructor, `receive` or
/// `fallback` is a call candidate from that symbol (`onlyRole(ROLE)` →
/// `onlyRole`; a base-constructor call `Ownable(msg.sender)` → `Ownable`).
fn modifier_calls(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    walk(tree, |n| {
        if n.kind() != "modifier_invocation" {
            return;
        }
        let Some(owner) = n.parent().filter(|p| FN_KINDS.contains(&p.kind())) else {
            return;
        };
        let Some(idx) = symbol_at(out, owner) else {
            return;
        };
        let path: Vec<String> = named_children(n)
            .into_iter()
            .filter(|c| c.kind() == "identifier")
            .map(|c| text(c, src).trim().to_string())
            .collect();
        let Some((callee, qualifier)) = path.split_last() else {
            return;
        };
        found.push(CallCandidate {
            caller: out.symbols[idx].stable_id.clone(),
            callee: callee.clone(),
            receiver: (!qualifier.is_empty()).then(|| qualifier.join(".")),
            line: n.start_position().row as u32 + 1,
        });
    });
    out.calls.extend(found);
    out.calls.sort_by_key(|c| c.line);
}

/// `new X(...)` deploys contract `X`: a call candidate to `X`. The grammar
/// reads `new A.B(...)` as a call of member `B` on `new A`; that candidate is
/// replaced by `B` with receiver `A`.
fn creations(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    walk(tree, |n| {
        if n.kind() != "new_expression" {
            return;
        }
        let Some(user) = n.child_by_field_name("name").and_then(|t| {
            named_children(t)
                .into_iter()
                .find(|c| c.kind() == "user_defined_type")
        }) else {
            return;
        };
        let mut path: Vec<String> = named_children(user)
            .into_iter()
            .map(|c| text(c, src).trim().to_string())
            .collect();
        let mut up = n;
        while let Some(p) = up.parent().filter(|p| p.kind() == "expression") {
            up = p;
        }
        if let Some(prop) = up
            .parent()
            .filter(|m| m.kind() == "member_expression")
            .and_then(|m| m.child_by_field_name("property"))
        {
            path.push(text(prop, src).trim().to_string());
        }
        let Some((callee, qualifier)) = path.split_last() else {
            return;
        };
        found.push(CallCandidate {
            caller: enclosing_symbol(&out.symbols, &file.path, n.start_byte()),
            callee: callee.clone(),
            receiver: (!qualifier.is_empty()).then(|| qualifier.join(".")),
            line: n.start_position().row as u32 + 1,
        });
    });
    out.calls
        .retain(|c| !c.receiver.as_deref().is_some_and(|r| r.starts_with("new ")));
    out.calls.extend(found);
    out.calls.sort_by_key(|c| c.line);
}

/// Name of an event / error reference: an identifier or a dotted path
/// (`IVault.Other`), whitespace removed.
fn reference_name(n: Node, src: &str) -> Option<String> {
    let n = unwrap_expr(n);
    matches!(n.kind(), "identifier" | "member_expression")
        .then(|| text(n, src).split_whitespace().collect::<String>())
}

/// `emit E(...)` → `Emits E`; `revert E(...)` and `require(cond, E(...))` →
/// `Reverts E`, from the enclosing symbol.
fn emits_and_reverts(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    walk(tree, |n| {
        let (kind, target) = match n.kind() {
            "emit_statement" => (EdgeKind::Emits, n.child_by_field_name("name")),
            "revert_statement" => (EdgeKind::Reverts, n.child_by_field_name("error")),
            "call_expression" => {
                let is_require = n
                    .child_by_field_name("function")
                    .map(unwrap_expr)
                    .is_some_and(|f| f.kind() == "identifier" && text(f, src).trim() == "require");
                if !is_require {
                    return;
                }
                let err = named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "call_argument")
                    .nth(1)
                    .and_then(|a| a.named_child(0))
                    .map(unwrap_expr)
                    .filter(|e| e.kind() == "call_expression")
                    .and_then(|e| e.child_by_field_name("function"));
                (EdgeKind::Reverts, err)
            }
            _ => return,
        };
        let Some(target_name) = target.and_then(|t| reference_name(t, src)) else {
            return;
        };
        found.push(Relation {
            source: enclosing_symbol(&out.symbols, &file.path, n.start_byte()),
            kind,
            target_name,
        });
    });
    out.relations.extend(found);
}

/// Foundry cheatcode reads `vm.envUint("PRIVATE_KEY")`, `vm.envOr("X", d)`:
/// env-var surfaces (`detail` = the cheatcode).
fn foundry_env_vars(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    walk(tree, |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let Some(member) = n
            .child_by_field_name("function")
            .map(unwrap_expr)
            .filter(|f| f.kind() == "member_expression")
        else {
            return;
        };
        let object = member.child_by_field_name("object").map(unwrap_expr);
        let property = member.child_by_field_name("property");
        let (Some(object), Some(property)) = (object, property) else {
            return;
        };
        let cheat = text(property, src).trim();
        if object.kind() != "identifier"
            || text(object, src).trim() != "vm"
            || !cheat.starts_with("env")
        {
            return;
        }
        let Some(lit) = named_children(n)
            .into_iter()
            .find(|c| c.kind() == "call_argument")
            .and_then(|a| a.named_child(0))
            .map(unwrap_expr)
            .filter(|e| e.kind() == "string_literal")
        else {
            return;
        };
        let name = text(lit, src).trim().trim_matches(['"', '\'']).to_string();
        if name.is_empty() {
            return;
        }
        let owner = enclosing_symbol(&out.symbols, &file.path, n.start_byte());
        found.push(Surface {
            kind: SurfaceKind::EnvVar,
            name,
            line: n.start_position().row as u32 + 1,
            owner: (!owner.starts_with("file:")).then_some(owner),
            detail: Some(format!("vm.{cheat}")),
        });
    });
    out.surfaces.extend(found);
}

/// One import per directive, in source order (`line` = the `import`
/// keyword's line), with the LOCAL names it binds: `{A as B, C}` → `[B, C]`;
/// `* as C` and `"x.sol" as C` → `[C]`; a plain `import "x.sol";` → `[]`.
/// Rebuilt from the tree because the query yields one match per name and
/// line, which splits a multi-line `{...}` list and drops aliases.
fn merge_imports(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut imports: Vec<ImportRef> = Vec::new();
    for d in named_children(tree.root_node()) {
        if d.kind() != "import_directive" {
            continue;
        }
        let Some(specifier) = d
            .child_by_field_name("source")
            .map(|s| text(s, src).trim().trim_matches(['"', '\'']).to_string())
        else {
            continue;
        };
        let mut names: Vec<String> = Vec::new();
        let mut last_was_name = false;
        let mut cursor = d.walk();
        for (i, c) in d.children(&mut cursor).enumerate() {
            match d.field_name_for_child(i as u32) {
                Some("import_name") => {
                    names.push(text(c, src).trim().to_string());
                    last_was_name = true;
                }
                Some("alias") => {
                    let alias = text(c, src).trim().to_string();
                    match names.last_mut().filter(|_| last_was_name) {
                        Some(last) => *last = alias,
                        None => names.push(alias),
                    }
                    last_was_name = false;
                }
                _ if c.is_named() => last_was_name = false,
                _ => {}
            }
        }
        imports.push(ImportRef {
            specifier,
            names,
            line: d.start_position().row as u32 + 1,
        });
    }
    out.imports = imports;
}

/// `@scope/name` or the first path segment of every non-relative import.
fn packages(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    for imp in &out.imports {
        let path = imp.specifier.as_str();
        if path.starts_with('.') || path.is_empty() {
            continue;
        }
        let mut segs = path.split('/');
        let first = segs.next().unwrap_or("");
        let pkg = if first.starts_with('@') {
            match segs.next() {
                Some(second) => format!("{first}/{second}"),
                None => continue,
            }
        } else {
            first.to_string()
        };
        if pkg.is_empty() || out.packages.contains(&pkg) {
            continue;
        }
        out.packages.push(pkg);
    }
}

/// NatSpec tags of every documented symbol as `metadata.natspec`.
fn natspec(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    for s in &mut out.symbols {
        if let Some(n) = s.doc.as_deref().and_then(parse_natspec) {
            s.metadata["natspec"] = n;
        }
    }
}

/// Parse a cleaned doc comment into `{title, notice, dev, params: {name:
/// text}, returns: [text], inheritdoc, custom: {tag: text}}` (empty parts
/// left out). A line without a tag continues the previous tag; untagged
/// leading text is the notice (the NatSpec rule for `///`). Unknown tags
/// and their continuation lines are dropped. `None` when nothing is left.
fn parse_natspec(doc: &str) -> Option<Value> {
    #[derive(Clone)]
    enum Cur {
        Title,
        Notice,
        Dev,
        Param(String),
        Return,
        Custom(String),
        Skip,
    }
    fn append(slot: &mut String, text: &str) {
        if text.is_empty() {
            return;
        }
        if !slot.is_empty() {
            slot.push(' ');
        }
        slot.push_str(text);
    }
    let (mut title, mut notice, mut dev) = (String::new(), String::new(), String::new());
    let mut params: Vec<(String, String)> = Vec::new();
    let mut returns: Vec<String> = Vec::new();
    let mut custom: Vec<(String, String)> = Vec::new();
    let mut inheritdoc = String::new();
    let mut cur = Cur::Notice;
    for line in doc.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let (tag, rest) = match l.strip_prefix('@') {
            Some(t) => {
                let (tag, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
                (Some(tag), rest.trim())
            }
            None => (None, l),
        };
        if let Some(tag) = tag {
            cur = match tag {
                "title" => Cur::Title,
                "notice" => Cur::Notice,
                "dev" => Cur::Dev,
                "param" => {
                    let (name, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    let text = text.trim().trim_start_matches(['-', ':']).trim();
                    params.push((name.to_string(), text.to_string()));
                    cur = Cur::Param(name.to_string());
                    continue;
                }
                "return" => {
                    returns.push(rest.to_string());
                    cur = Cur::Return;
                    continue;
                }
                "inheritdoc" => {
                    inheritdoc = rest.split_whitespace().next().unwrap_or("").to_string();
                    cur = Cur::Skip;
                    continue;
                }
                t => match t.strip_prefix("custom:") {
                    Some(c) => {
                        custom.push((c.to_string(), rest.to_string()));
                        cur = Cur::Custom(c.to_string());
                        continue;
                    }
                    None => Cur::Skip,
                },
            };
        }
        match &cur {
            Cur::Title => append(&mut title, rest),
            Cur::Notice => append(&mut notice, rest),
            Cur::Dev => append(&mut dev, rest),
            Cur::Param(name) => {
                if let Some(p) = params.iter_mut().rev().find(|p| &p.0 == name) {
                    append(&mut p.1, rest);
                }
            }
            Cur::Return => {
                if let Some(r) = returns.last_mut() {
                    append(r, rest);
                }
            }
            Cur::Custom(tag) => {
                if let Some(c) = custom.iter_mut().rev().find(|c| &c.0 == tag) {
                    append(&mut c.1, rest);
                }
            }
            Cur::Skip => {}
        }
    }
    let mut out = serde_json::Map::new();
    for (k, v) in [
        ("title", title),
        ("notice", notice),
        ("dev", dev),
        ("inheritdoc", inheritdoc),
    ] {
        if !v.is_empty() {
            out.insert(k.into(), json!(v));
        }
    }
    if !params.is_empty() {
        let m: serde_json::Map<String, Value> =
            params.into_iter().map(|(k, v)| (k, json!(v))).collect();
        out.insert("params".into(), Value::Object(m));
    }
    if !returns.is_empty() {
        out.insert("returns".into(), json!(returns));
    }
    if !custom.is_empty() {
        let m: serde_json::Map<String, Value> =
            custom.into_iter().map(|(k, v)| (k, json!(v))).collect();
        out.insert("custom".into(), Value::Object(m));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::treesitter::walker::extract;
    use crate::domain::model::Language;

    const TOKEN: &str = r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Ownable, Context} from "@openzeppelin/contracts/access/Ownable.sol";
import "./libs/SafeCast.sol";
import {Pausable} from "solmate/utils/Pausable.sol";

/// Call kind of an execution.
type CallType is bytes1;

/// Upper bound of the mint fee.
uint256 constant MAX_FEE = 1 ether;

error Unauthorized(address caller);

library MathLib {
    function min(uint256 a, uint256 b) internal pure returns (uint256) {
        return a < b ? a : b;
    }
}

interface IVault {
    function deposit(uint256 amount) external;
}

/// @title A simple token
/// @notice ERC20-style with owner controls
contract Token is IERC20, Ownable(msg.sender) {
    struct Info { uint256 supply; }
    enum State { Open, Closed }

    /// Emitted on transfer.
    event Transfer(address indexed from, address indexed to, uint256 value);

    error InsufficientBalance(uint256 have, uint256 want);

    bytes32 public constant UPGRADER_ROLE = keccak256("UPGRADER_ROLE");
    address public immutable token;
    mapping(address => uint256) private _balances;

    modifier onlyPositive(uint256 amount) {
        require(amount > 0, "zero");
        _;
    }

    /// Sets the owner.
    constructor(string memory name_) Ownable(msg.sender) {
        _mint(msg.sender, 1000);
    }

    receive() external payable {}

    fallback() external payable {}

    function transfer(address to, uint256 amount) public onlyPositive(amount) returns (bool) {
        _balances[to] += MathLib.min(amount, _balances[msg.sender]);
        emit Transfer(msg.sender, to, amount);
        return super.transfer(to, amount);
    }

    function _mint(address to, uint256 amount) internal {
        IVault(to).deposit(amount);
    }
}

function freeHelper(uint256 x) pure returns (uint256) {
    return x + 1;
}
"#;

    fn parse(path: &str, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: Language::Solidity,
            content: src.into(),
        };
        extract(spec().unwrap(), &file)
    }

    fn sol() -> FileExtraction {
        parse("contracts/Token.sol", TOKEN)
    }

    fn find<'a>(x: &'a FileExtraction, id: &str) -> &'a Symbol {
        x.symbols
            .iter()
            .find(|s| s.stable_id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    fn calls(x: &FileExtraction) -> Vec<(&str, Option<&str>, &str)> {
        x.calls
            .iter()
            .map(|c| (c.callee.as_str(), c.receiver.as_deref(), c.caller.as_str()))
            .collect()
    }

    #[test]
    fn symbol_kinds_ids_and_parents() {
        let x = sol();
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "solidity:contracts/Token.sol:type_alias:CallType",
                "solidity:contracts/Token.sol:field:MAX_FEE",
                "solidity:contracts/Token.sol:field:Unauthorized",
                "solidity:contracts/Token.sol:type:MathLib",
                "solidity:contracts/Token.sol:method:MathLib::min",
                "solidity:contracts/Token.sol:interface:IVault",
                "solidity:contracts/Token.sol:method:IVault::deposit",
                "solidity:contracts/Token.sol:type:Token",
                "solidity:contracts/Token.sol:type:Token::Info",
                "solidity:contracts/Token.sol:enum:Token::State",
                "solidity:contracts/Token.sol:field:Token::Transfer",
                "solidity:contracts/Token.sol:field:Token::InsufficientBalance",
                "solidity:contracts/Token.sol:field:Token::UPGRADER_ROLE",
                "solidity:contracts/Token.sol:field:Token::token",
                "solidity:contracts/Token.sol:field:Token::_balances",
                "solidity:contracts/Token.sol:function:Token::onlyPositive",
                "solidity:contracts/Token.sol:method:Token::constructor",
                "solidity:contracts/Token.sol:method:Token::receive",
                "solidity:contracts/Token.sol:method:Token::fallback",
                "solidity:contracts/Token.sol:method:Token::transfer",
                "solidity:contracts/Token.sol:method:Token::_mint",
                "solidity:contracts/Token.sol:function:freeHelper",
            ]
        );
        let ctor = find(&x, "solidity:contracts/Token.sol:method:Token::constructor");
        assert_eq!(ctor.kind, NodeKind::Method);
        assert_eq!(
            ctor.parent.as_deref(),
            Some("solidity:contracts/Token.sol:type:Token")
        );
        assert_eq!(
            ctor.signature.as_deref(),
            Some("constructor(string memory name_) Ownable(msg.sender)")
        );
        assert_eq!(ctor.doc.as_deref(), Some("Sets the owner."));
        assert_eq!(ctor.segments.len(), 1);
        let token = find(&x, "solidity:contracts/Token.sol:type:Token");
        assert_eq!(
            token.doc.as_deref(),
            Some("@title A simple token\n@notice ERC20-style with owner controls")
        );
        assert_eq!(
            token.signature.as_deref(),
            Some("contract Token is IERC20, Ownable(msg.sender)")
        );
        assert_eq!(token.segments.len(), 15, "members + the two doc comments");
        assert_eq!(
            find(&x, "solidity:contracts/Token.sol:method:Token::transfer").kind,
            NodeKind::Method
        );
        assert_eq!(
            find(&x, "solidity:contracts/Token.sol:function:freeHelper").kind,
            NodeKind::Function
        );
    }

    #[test]
    fn metadata_flags() {
        let x = sol();
        assert_eq!(
            find(&x, "solidity:contracts/Token.sol:type:MathLib").metadata["library"],
            true
        );
        assert_eq!(
            find(
                &x,
                "solidity:contracts/Token.sol:function:Token::onlyPositive"
            )
            .metadata["modifier"],
            true
        );
        let ev = find(&x, "solidity:contracts/Token.sol:field:Token::Transfer");
        assert_eq!(ev.metadata["event"], true);
        let transfer = find(&x, "solidity:contracts/Token.sol:method:Token::transfer");
        assert_eq!(transfer.metadata["visibility"], "public");
        assert_eq!(
            transfer.metadata["modifiers"],
            serde_json::json!(["onlyPositive"])
        );
        assert!(transfer.metadata.get("mutability").is_none());
        let min = find(&x, "solidity:contracts/Token.sol:method:MathLib::min");
        assert_eq!(min.metadata["visibility"], "internal");
        assert_eq!(min.metadata["mutability"], "pure");
        let deposit = find(&x, "solidity:contracts/Token.sol:method:IVault::deposit");
        assert_eq!(deposit.metadata["visibility"], "external");
        assert_eq!(
            transfer.metadata["header"],
            "function transfer(address to, uint256 amount) public onlyPositive(amount) returns (bool)"
        );
        assert_eq!(
            deposit.metadata["header"],
            "function deposit(uint256 amount) external"
        );
        assert_eq!(ev.doc.as_deref(), Some("Emitted on transfer."));
        // Parameter lists: what canonical ABI signatures are built from.
        assert_eq!(
            transfer.metadata["params"],
            json!([
                {"name": "to", "type": "address", "location": ""},
                {"name": "amount", "type": "uint256", "location": ""}
            ])
        );
        assert_eq!(
            transfer.metadata["returns"],
            json!([{"name": "", "type": "bool"}])
        );
        assert_eq!(transfer.metadata["guards"], json!(["onlyPositive(amount)"]));
        let token = find(&x, "solidity:contracts/Token.sol:type:Token");
        assert_eq!(token.metadata["bases"], json!(["IERC20", "Ownable"]));
        assert_eq!(
            token.metadata["header"],
            "contract Token is IERC20, Ownable(msg.sender)"
        );
        assert!(token.metadata.get("abstract").is_none());
        assert_eq!(
            find(&x, "solidity:contracts/Token.sol:type:Token::Info").metadata["members"],
            json!([{"name": "supply", "type": "uint256"}])
        );
        let ctor = find(&x, "solidity:contracts/Token.sol:method:Token::constructor");
        assert_eq!(
            ctor.metadata["params"],
            json!([{"name": "name_", "type": "string", "location": "memory"}])
        );
        assert_eq!(ctor.metadata["modifiers"], json!(["Ownable"]));
    }

    #[test]
    fn contract_header_bases_and_function_overrides() {
        let src = r#"abstract contract OnChainLab is
    IOnChainLab,
    Initializable,
    UUPSUpgradeable(),
    Lib.Base
{
    function execute(bytes32 mode, bytes calldata data) external payable virtual override(IOnChainLab, Lib.Base) onlyEntryPointOrOwner whenInitialized {}
    function hook() internal override {}
    fallback(bytes calldata input) external returns (bytes memory out) {}
}
"#;
        let x = parse("src/OnChainLab.sol", src);
        let lab = find(&x, "solidity:src/OnChainLab.sol:type:OnChainLab");
        assert_eq!(lab.metadata["abstract"], true);
        assert_eq!(
            lab.metadata["header"],
            "abstract contract OnChainLab is IOnChainLab, Initializable, UUPSUpgradeable(), Lib.Base"
        );
        assert_eq!(
            lab.metadata["bases"],
            json!([
                "IOnChainLab",
                "Initializable",
                "UUPSUpgradeable",
                "Lib.Base"
            ])
        );
        let exec = find(&x, "solidity:src/OnChainLab.sol:method:OnChainLab::execute");
        assert_eq!(exec.metadata["virtual"], true);
        assert_eq!(
            exec.metadata["override"],
            json!(["IOnChainLab", "Lib.Base"])
        );
        assert_eq!(exec.metadata["mutability"], "payable");
        assert_eq!(
            exec.metadata["modifiers"],
            json!(["onlyEntryPointOrOwner", "whenInitialized"])
        );
        assert_eq!(
            exec.metadata["params"][1],
            json!({"name": "data", "type": "bytes", "location": "calldata"})
        );
        let hook = find(&x, "solidity:src/OnChainLab.sol:method:OnChainLab::hook");
        assert_eq!(hook.metadata["override"], true);
        let fb = find(
            &x,
            "solidity:src/OnChainLab.sol:method:OnChainLab::fallback",
        );
        assert_eq!(fb.metadata["special"], "fallback");
        assert_eq!(
            fb.metadata["params"],
            json!([{"name": "input", "type": "bytes", "location": "calldata"}])
        );
        assert_eq!(
            fb.metadata["returns"],
            json!([{"name": "out", "type": "bytes"}])
        );
    }

    #[test]
    fn headers_and_types_leave_comments_out() {
        // onchainlabs/src/OnChainLab.sol:69-81 comments out `IERC1271` in
        // its `is` list (review finding: it showed up in the header).
        let src = r#"contract OnChainLab is
    ERC4337Account, // Base: provides AbstractSigner
    IERC165, // Interfaces (order doesn't matter)
    // IERC1271, // Interfaces COMMENT OUT
    /* IERC1155Receiver, */ IOnChainLab
{
    mapping(address /* owner */ => uint256) public balances;
    event Moved(address indexed from, // sender
        uint256 /* wei */ amount);
    function move(
        address to, // recipient
        uint256 amount /* wei */
    ) external /* no guard */ returns (bool) {}
    function hook() internal override(ERC4337Account, // first
        IOnChainLab) {}
}
"#;
        let x = parse("src/OnChainLab.sol", src);
        let lab = find(&x, "solidity:src/OnChainLab.sol:type:OnChainLab");
        assert_eq!(
            lab.metadata["header"],
            "contract OnChainLab is ERC4337Account, IERC165, IOnChainLab"
        );
        assert_eq!(
            lab.metadata["bases"],
            json!(["ERC4337Account", "IERC165", "IOnChainLab"])
        );
        let mv = find(&x, "solidity:src/OnChainLab.sol:method:OnChainLab::move");
        assert_eq!(
            mv.metadata["header"],
            "function move( address to, uint256 amount ) external returns (bool)"
        );
        let ev = find(&x, "solidity:src/OnChainLab.sol:field:OnChainLab::Moved");
        assert_eq!(
            ev.metadata["header"],
            "event Moved(address indexed from, uint256 amount)"
        );
        let hook = find(&x, "solidity:src/OnChainLab.sol:method:OnChainLab::hook");
        assert_eq!(
            hook.metadata["override"],
            json!(["ERC4337Account", "IOnChainLab"])
        );
        let bal = find(&x, "solidity:src/OnChainLab.sol:field:OnChainLab::balances");
        assert_eq!(bal.metadata["type"], "mapping(address => uint256)");
        assert_eq!(
            bal.metadata["params"],
            json!([{"name": "", "type": "address"}])
        );
        assert_eq!(
            bal.metadata["header"],
            "mapping(address => uint256) public balances"
        );
    }

    #[test]
    fn symbol_kinds_include_state_vars_errors_udvts() {
        let x = sol();
        for (id, kind, parent) in [
            (
                "solidity:contracts/Token.sol:type_alias:CallType",
                NodeKind::TypeAlias,
                None,
            ),
            (
                "solidity:contracts/Token.sol:field:MAX_FEE",
                NodeKind::Field,
                None,
            ),
            (
                "solidity:contracts/Token.sol:field:Unauthorized",
                NodeKind::Field,
                None,
            ),
            (
                "solidity:contracts/Token.sol:field:Token::InsufficientBalance",
                NodeKind::Field,
                Some("solidity:contracts/Token.sol:type:Token"),
            ),
            (
                "solidity:contracts/Token.sol:field:Token::UPGRADER_ROLE",
                NodeKind::Field,
                Some("solidity:contracts/Token.sol:type:Token"),
            ),
        ] {
            let s = find(&x, id);
            assert_eq!(s.kind, kind, "{id}");
            assert_eq!(s.parent.as_deref(), parent, "{id}");
        }
        let err = find(
            &x,
            "solidity:contracts/Token.sol:field:Token::InsufficientBalance",
        );
        assert_eq!(err.metadata["error"], true);
        assert_eq!(
            err.metadata["params"],
            json!([{"name": "have", "type": "uint256"}, {"name": "want", "type": "uint256"}])
        );
        assert_eq!(
            err.metadata["header"],
            "error InsufficientBalance(uint256 have, uint256 want)"
        );
        let file_err = find(&x, "solidity:contracts/Token.sol:field:Unauthorized");
        assert_eq!(
            file_err.metadata["params"],
            json!([{"name": "caller", "type": "address"}])
        );
        let fee = find(&x, "solidity:contracts/Token.sol:field:MAX_FEE");
        assert_eq!(fee.metadata["constant"], true);
        assert_eq!(fee.metadata["type"], "uint256");
        assert_eq!(fee.metadata["value"], "1 ether");
        assert_eq!(fee.metadata["header"], "uint256 constant MAX_FEE");
        assert!(fee.metadata.get("state_var").is_none());
        assert_eq!(fee.doc.as_deref(), Some("Upper bound of the mint fee."));
    }

    #[test]
    fn state_var_metadata_flags() {
        let x = sol();
        let role = find(
            &x,
            "solidity:contracts/Token.sol:field:Token::UPGRADER_ROLE",
        );
        assert_eq!(
            role.metadata,
            json!({
                "state_var": true, "visibility": "public", "getter": true, "mutability": "view",
                "params": [], "returns": [{"name": "", "type": "bytes32"}],
                "constant": true, "type": "bytes32", "value": "keccak256(\"UPGRADER_ROLE\")",
                "header": "bytes32 public constant UPGRADER_ROLE",
            })
        );
        let token = find(&x, "solidity:contracts/Token.sol:field:Token::token");
        assert_eq!(token.metadata["immutable"], true);
        assert_eq!(token.metadata["type"], "address");
        assert!(token.metadata.get("value").is_none());
        assert_eq!(token.metadata["header"], "address public immutable token");
        let bal = find(&x, "solidity:contracts/Token.sol:field:Token::_balances");
        assert_eq!(bal.metadata["visibility"], "private");
        assert_eq!(bal.metadata["type"], "mapping(address => uint256)");
        assert!(bal.metadata.get("getter").is_none());
        assert!(bal.metadata.get("value").is_none());
        // Internal by default; public mapping / array getters take their keys.
        let src = r#"contract Registry {
    uint256 count;
    mapping(bytes32 oclId => mapping(bytes32 provider => mapping(bytes32 subject => bytes32))) public activeDidHash;
    mapping(uint256 labId => ILabToken) public tokenized;
    uint256[3][] public grid;
}
"#;
        let x = parse("src/Registry.sol", src);
        let count = find(&x, "solidity:src/Registry.sol:field:Registry::count");
        assert_eq!(count.metadata["visibility"], "internal");
        let did = find(
            &x,
            "solidity:src/Registry.sol:field:Registry::activeDidHash",
        );
        assert_eq!(
            did.metadata["params"],
            json!([
                {"name": "oclId", "type": "bytes32"},
                {"name": "provider", "type": "bytes32"},
                {"name": "subject", "type": "bytes32"}
            ])
        );
        assert_eq!(
            did.metadata["returns"],
            json!([{"name": "", "type": "bytes32"}])
        );
        let tok = find(&x, "solidity:src/Registry.sol:field:Registry::tokenized");
        assert_eq!(
            tok.metadata["returns"],
            json!([{"name": "", "type": "ILabToken"}])
        );
        let grid = find(&x, "solidity:src/Registry.sol:field:Registry::grid");
        assert_eq!(
            grid.metadata["params"],
            json!([{"name": "", "type": "uint256"}, {"name": "", "type": "uint256"}])
        );
        assert_eq!(
            grid.metadata["returns"],
            json!([{"name": "", "type": "uint256"}])
        );
    }

    #[test]
    fn receive_and_fallback_are_methods() {
        let x = sol();
        for name in ["receive", "fallback"] {
            let s = find(
                &x,
                &format!("solidity:contracts/Token.sol:method:Token::{name}"),
            );
            assert_eq!(s.kind, NodeKind::Method);
            assert_eq!(s.metadata["visibility"], "external");
            assert_eq!(s.metadata["mutability"], "payable");
            assert_eq!(s.metadata["special"], name);
            assert_eq!(
                s.signature.as_deref(),
                Some(format!("{name}() external payable").as_str())
            );
        }
        // Old-style `function() external payable` is the fallback.
        let x = parse(
            "src/Old.sol",
            "contract Old { function() external payable {} }\n",
        );
        assert_eq!(
            find(&x, "solidity:src/Old.sol:method:Old::fallback").metadata["special"],
            "fallback"
        );
    }

    #[test]
    fn top_level_fields_stay_in_file_head() {
        let x = sol();
        let free: Vec<&str> = x.free_ranges.iter().map(|(s, e)| &TOKEN[*s..*e]).collect();
        assert!(
            free.contains(&"/// Upper bound of the mint fee.\nuint256 constant MAX_FEE = 1 ether;"),
            "{free:?}"
        );
        assert!(
            free.contains(&"error Unauthorized(address caller);"),
            "{free:?}"
        );
        // A user-defined value type is a unit of its own, not file_head text.
        assert!(!free.iter().any(|f| f.contains("type CallType")));
        let mut sorted = x.free_ranges.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, x.free_ranges);
    }

    #[test]
    fn udvt_is_type_alias_unit() {
        let x = sol();
        let ct = find(&x, "solidity:contracts/Token.sol:type_alias:CallType");
        assert!(ct.kind.is_unit());
        assert_eq!(
            ct.metadata,
            json!({"udvt": true, "underlying": "bytes1",
                "natspec": {"notice": "Call kind of an execution."}})
        );
        assert_eq!(ct.doc.as_deref(), Some("Call kind of an execution."));
    }

    #[test]
    fn event_params_and_anonymous() {
        let x = sol();
        let ev = find(&x, "solidity:contracts/Token.sol:field:Token::Transfer");
        assert_eq!(
            ev.metadata["params"],
            json!([
                {"name": "from", "type": "address", "indexed": true},
                {"name": "to", "type": "address", "indexed": true},
                {"name": "value", "type": "uint256", "indexed": false}
            ])
        );
        assert!(ev.metadata.get("anonymous").is_none());
        let x = parse(
            "src/E.sol",
            "interface E { event Raw(bytes32 indexed, uint) anonymous; }\n",
        );
        let raw = find(&x, "solidity:src/E.sol:field:E::Raw");
        assert_eq!(raw.metadata["anonymous"], true);
        assert_eq!(
            raw.metadata["params"],
            json!([
                {"name": "", "type": "bytes32", "indexed": true},
                {"name": "", "type": "uint", "indexed": false}
            ])
        );
    }

    #[test]
    fn inheritance_becomes_extends() {
        let x = sol();
        let extends: Vec<&Relation> = x
            .relations
            .iter()
            .filter(|r| r.kind == EdgeKind::Extends)
            .collect();
        assert_eq!(
            extends,
            vec![
                &Relation {
                    source: "solidity:contracts/Token.sol:type:Token".into(),
                    kind: EdgeKind::Extends,
                    target_name: "IERC20".into()
                },
                &Relation {
                    source: "solidity:contracts/Token.sol:type:Token".into(),
                    kind: EdgeKind::Extends,
                    target_name: "Ownable".into()
                },
            ]
        );
    }

    #[test]
    fn imports_and_packages() {
        let x = sol();
        let imports: Vec<(&str, Vec<&str>)> = x
            .imports
            .iter()
            .map(|i| {
                (
                    i.specifier.as_str(),
                    i.names.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            imports,
            vec![
                ("@openzeppelin/contracts/token/ERC20/IERC20.sol", vec![]),
                (
                    "@openzeppelin/contracts/access/Ownable.sol",
                    vec!["Ownable", "Context"]
                ),
                ("./libs/SafeCast.sol", vec![]),
                ("solmate/utils/Pausable.sol", vec!["Pausable"]),
            ]
        );
        assert_eq!(x.imports[1].line, 5);
        assert_eq!(x.packages, vec!["@openzeppelin/contracts", "solmate"]);
        let free: Vec<&str> = x.free_ranges.iter().map(|(s, e)| &TOKEN[*s..*e]).collect();
        assert_eq!(free[0], "// SPDX-License-Identifier: MIT");
        assert!(free.contains(&"pragma solidity ^0.8.20;"));
        assert_eq!(free.len(), 8, "{free:?}");
    }

    #[test]
    fn imports_record_local_alias_names() {
        let src = "import {A as B, C} from \"./x.sol\";\nimport * as D from \"./y.sol\";\nimport \"./z.sol\" as E;\nimport {\n    F,\n    G as H\n} from \"src/f.sol\";\n";
        let x = parse("src/I.sol", src);
        let imports: Vec<(&str, Vec<&str>, u32)> = x
            .imports
            .iter()
            .map(|i| {
                (
                    i.specifier.as_str(),
                    i.names.iter().map(String::as_str).collect(),
                    i.line,
                )
            })
            .collect();
        assert_eq!(
            imports,
            vec![
                ("./x.sol", vec!["B", "C"], 1),
                ("./y.sol", vec!["D"], 2),
                ("./z.sol", vec!["E"], 3),
                ("src/f.sol", vec!["F", "H"], 4),
            ]
        );
    }

    #[test]
    fn calls_with_receivers_and_callers() {
        let x = sol();
        assert_eq!(
            calls(&x),
            vec![
                (
                    "keccak256",
                    None,
                    "solidity:contracts/Token.sol:field:Token::UPGRADER_ROLE"
                ),
                (
                    "require",
                    None,
                    "solidity:contracts/Token.sol:function:Token::onlyPositive"
                ),
                (
                    "Ownable",
                    None,
                    "solidity:contracts/Token.sol:method:Token::constructor"
                ),
                (
                    "_mint",
                    None,
                    "solidity:contracts/Token.sol:method:Token::constructor"
                ),
                (
                    "onlyPositive",
                    None,
                    "solidity:contracts/Token.sol:method:Token::transfer"
                ),
                (
                    "min",
                    Some("MathLib"),
                    "solidity:contracts/Token.sol:method:Token::transfer"
                ),
                (
                    "transfer",
                    Some("super"),
                    "solidity:contracts/Token.sol:method:Token::transfer"
                ),
                (
                    "IVault",
                    None,
                    "solidity:contracts/Token.sol:method:Token::_mint"
                ),
                (
                    "deposit",
                    Some("IVault"),
                    "solidity:contracts/Token.sol:method:Token::_mint"
                ),
            ]
        );
    }

    #[test]
    fn modifier_invocations_become_calls_and_guards() {
        let src = r#"contract Factory {
    function upgrade(address impl) external onlyRole(UPGRADER_ROLE) Guards.whenLive {}
}
"#;
        let x = parse("src/Factory.sol", src);
        let id = "solidity:src/Factory.sol:method:Factory::upgrade";
        assert_eq!(
            calls(&x),
            vec![("onlyRole", None, id), ("whenLive", Some("Guards"), id)]
        );
        assert_eq!(
            find(&x, id).metadata["guards"],
            json!(["onlyRole(UPGRADER_ROLE)", "Guards.whenLive"])
        );
    }

    #[test]
    fn new_expression_is_call_candidate() {
        // onchainlabs src/factory/OnChainLabFactory.sol:81-103
        let src = r#"contract OnChainLabFactory {
    constructor(address _entryPoint) {
        OnChainLab impl = new OnChainLab(IEntryPoint(_entryPoint), BEACON);
        new OnChainLabRouter{salt: bytes32(0)}(BEACON);
        bytes memory buf = new bytes(4);
        new Deps.Child();
    }
}
"#;
        let x = parse("src/factory/OnChainLabFactory.sol", src);
        let ctor =
            "solidity:src/factory/OnChainLabFactory.sol:method:OnChainLabFactory::constructor";
        let got = calls(&x);
        assert!(got.contains(&("OnChainLab", None, ctor)), "{got:?}");
        assert!(got.contains(&("IEntryPoint", None, ctor)), "{got:?}");
        assert!(got.contains(&("OnChainLabRouter", None, ctor)), "{got:?}");
        assert!(got.contains(&("Child", Some("Deps"), ctor)), "{got:?}");
        assert!(!got.iter().any(|c| c.0 == "bytes"), "{got:?}");
    }

    #[test]
    fn emit_and_revert_relations() {
        let src = r#"error Zero();
contract Vault is IVault {
    error InsufficientBalance(uint256 have, uint256 want);
    function deposit(uint256 amount) external {
        emit Deposited(msg.sender, amount);
        emit IVault.Other(1);
        if (amount == 0) revert InsufficientBalance(0, amount);
        if (amount == 1) revert Errors.Zero();
        require(amount > 2, Unauthorized(msg.sender));
        require(amount > 3, "plain");
        revert("plain");
    }
}
"#;
        let x = parse("src/Vault.sol", src);
        let dep = "solidity:src/Vault.sol:method:Vault::deposit";
        let rels: Vec<(EdgeKind, &str, &str)> = x
            .relations
            .iter()
            .filter(|r| r.kind != EdgeKind::Extends)
            .map(|r| (r.kind, r.source.as_str(), r.target_name.as_str()))
            .collect();
        assert_eq!(
            rels,
            vec![
                (EdgeKind::Emits, dep, "Deposited"),
                (EdgeKind::Emits, dep, "IVault.Other"),
                (EdgeKind::Reverts, dep, "InsufficientBalance"),
                (EdgeKind::Reverts, dep, "Errors.Zero"),
                (EdgeKind::Reverts, dep, "Unauthorized"),
            ]
        );
    }

    #[test]
    fn vm_env_reads_are_env_surfaces() {
        // ocltokenizer script/DeployOclTokenizer.s.sol:31 and the terms URL read.
        let src = r#"contract DeployOclTokenizer is Script {
    function run() external {
        uint256 deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        string memory base = vm.envOr("TERMS_BASE_URL", string("https://x"));
        other.envUint("NOT_VM");
    }
}
"#;
        let x = parse("script/DeployOclTokenizer.s.sol", src);
        let got: Vec<(&str, u32, Option<&str>, Option<&str>)> = x
            .surfaces
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.line,
                    s.owner.as_deref(),
                    s.detail.as_deref(),
                )
            })
            .collect();
        let run = "solidity:script/DeployOclTokenizer.s.sol:method:DeployOclTokenizer::run";
        assert_eq!(
            got,
            vec![
                ("PRIVATE_KEY", 3, Some(run), Some("vm.envUint")),
                ("TERMS_BASE_URL", 4, Some(run), Some("vm.envOr")),
            ]
        );
        assert!(x.surfaces.iter().all(|s| s.kind == SurfaceKind::EnvVar));
    }

    #[test]
    fn state_var_and_immutable_receivers_are_typed() {
        // onchainlabs OnChainLabFactory: typed state variable and immutable.
        let src = r#"contract OnChainLabFactory {
    IOclDerivationConfig public derivationConfig;
    IERC6551Registry public immutable REGISTRY;
    IERC20 private _asset;
    function create(address labNftContract, uint256 tokenId) external {
        derivationConfig.salt(tokenId);
        REGISTRY.createAccount(tokenId);
        _asset.transfer(msg.sender, 1);
        ILabNFT(labNftContract).notifyIdentityCreated(tokenId);
        this.create(labNftContract, 1);
    }
}
"#;
        let x = parse("src/factory/OnChainLabFactory.sol", src);
        let got: Vec<(&str, Option<&str>)> = calls(&x).iter().map(|c| (c.0, c.1)).collect();
        assert_eq!(
            got,
            vec![
                ("salt", Some("IOclDerivationConfig")),
                ("createAccount", Some("IERC6551Registry")),
                ("transfer", Some("IERC20")),
                ("ILabNFT", None),
                ("notifyIdentityCreated", Some("ILabNFT")),
                ("create", Some("this")),
            ]
        );
    }

    #[test]
    fn local_and_tuple_bindings_type_receivers() {
        // labs-contracts AccessResolver: casts inside try statements, locals.
        let src = r#"contract AccessResolver {
    function resolve(address tba, IOcl given) external {
        (uint256 a, IERC20 tok) = load();
        tok.transfer(msg.sender, a);
        IOcl local = given;
        local.owner();
        given.owner();
        try IOcl(tba).owner() returns (address oclOwner) {} catch {}
    }
}
"#;
        let x = parse("src/AccessResolver.sol", src);
        let got: Vec<(&str, Option<&str>)> = calls(&x).iter().map(|c| (c.0, c.1)).collect();
        assert_eq!(
            got,
            vec![
                ("load", None),
                ("transfer", Some("IERC20")),
                ("owner", Some("IOcl")),
                ("owner", Some("IOcl")),
                ("IOcl", None),
                ("owner", Some("IOcl")),
            ]
        );
    }

    #[test]
    fn mapping_value_receiver() {
        // ocltokenizer: mapping(uint256 labId => ILabToken) public tokenized;
        let src = r#"contract OclTokenizer {
    mapping(uint256 labId => ILabToken) public tokenized;
    mapping(address => mapping(uint256 => IVault)) vaults;
    IModule[] modules;
    function f(uint256 id) external {
        tokenized[id].wrappedToken();
        vaults[msg.sender][id].deposit(1);
        modules[0].onInstall("");
        vaults[msg.sender].deposit(1);
    }
}
"#;
        let x = parse("src/OclTokenizer.sol", src);
        let got: Vec<(&str, Option<&str>)> = calls(&x).iter().map(|c| (c.0, c.1)).collect();
        assert_eq!(
            got,
            vec![
                ("wrappedToken", Some("ILabToken")),
                ("deposit", Some("IVault")),
                ("onInstall", Some("IModule")),
                ("deposit", Some("vaults[msg.sender]")),
            ]
        );
    }

    #[test]
    fn using_for_adds_library_candidate() {
        let src = r#"type ValidationData is uint256;
using {vTypeEqual} for ValidationData global;
contract Account {
    using ECDSA for bytes32;
    using SafeERC20 for IERC20;
    using Packing for ValidationData;
    function check(bytes32 hash, IERC20 token, ValidationData v) external {
        hash.recover(sig);
        token.safeTransfer(msg.sender, 1);
        v.unpack();
        v.vTypeEqual(v);
    }
}
"#;
        let x = parse("src/Account.sol", src);
        let got: Vec<(&str, Option<&str>)> = calls(&x).iter().map(|c| (c.0, c.1)).collect();
        assert_eq!(
            got,
            vec![
                ("recover", Some("hash")),
                ("recover", Some("ECDSA")),
                ("safeTransfer", Some("IERC20")),
                ("unpack", Some("ValidationData")),
                ("unpack", Some("Packing")),
                ("vTypeEqual", Some("ValidationData")),
                ("vTypeEqual", Some("Packing")),
                ("vTypeEqual", None),
            ]
        );
    }

    #[test]
    fn shadowed_names_are_not_typed() {
        let src = r#"contract S {
    IVault vault;
    function f(bool b) external {
        if (b) { IERC20 x = a(); x.transfer(1); }
        else { IVault x = c(); x.deposit(1); }
        vault.deposit(2);
    }
    function g(IERC20 vault) external {
        vault.transfer(3);
    }
}
"#;
        let x = parse("src/S.sol", src);
        let got: Vec<(&str, Option<&str>)> = calls(&x)
            .iter()
            .filter(|c| c.1.is_some())
            .map(|c| (c.0, c.1))
            .collect();
        assert_eq!(
            got,
            vec![
                ("transfer", Some("x")),
                ("deposit", Some("x")),
                ("deposit", Some("IVault")),
                ("transfer", Some("IERC20")),
            ]
        );
    }

    #[test]
    fn natspec_tags_parse() {
        let src = r#"pragma solidity ^0.8.20;
contract Vault {
    /// @notice Deposit tokens into the vault
    ///   for the caller.
    /// @dev Reentrancy is guarded by the caller.
    /// @param amount How much to deposit
    /// @return ok Whether it worked
    /// @custom:storage-location erc7201:molecule.vault
    function deposit(uint256 amount) external returns (bool ok) {}

    /// Plain text is the notice.
    /// @author someone
    function plain() external {}
}
"#;
        let x = parse("src/Vault.sol", src);
        let n = &find(&x, "solidity:src/Vault.sol:method:Vault::deposit").metadata["natspec"];
        assert_eq!(
            *n,
            json!({
                "notice": "Deposit tokens into the vault for the caller.",
                "dev": "Reentrancy is guarded by the caller.",
                "params": {"amount": "How much to deposit"},
                "returns": ["ok Whether it worked"],
                "custom": {"storage-location": "erc7201:molecule.vault"},
            })
        );
        let n = &find(&x, "solidity:src/Vault.sol:method:Vault::plain").metadata["natspec"];
        assert_eq!(*n, json!({"notice": "Plain text is the notice."}));
        // The contract-level title of the shared fixture.
        let token = sol();
        let t = &find(&token, "solidity:contracts/Token.sol:type:Token").metadata["natspec"];
        assert_eq!(t["title"], "A simple token");
        assert_eq!(t["notice"], "ERC20-style with owner controls");
    }

    #[test]
    fn natspec_block_comment_params() {
        // onchainlabs src/interfaces/PackedUserOperation.sol:18-28.
        let src = r#"pragma solidity ^0.8.33;
/**
 * User Operation struct
 * @param sender                - The sender account of this request.
 * @param nonce                 - Unique value the sender uses to verify it is not a replay.
 * @param initCode              - If set, the account contract will be created by this constructor
 *                                with this initCode.
 */
struct PackedUserOperation {
    address sender;
    uint256 nonce;
    bytes initCode;
}

contract Beacon {
    /// @inheritdoc IOnChainLabBeacon
    function getTrustedAttesters() external view returns (address[] memory) {}
}
"#;
        let x = parse("src/interfaces/PackedUserOperation.sol", src);
        let n = &find(
            &x,
            "solidity:src/interfaces/PackedUserOperation.sol:type:PackedUserOperation",
        )
        .metadata["natspec"];
        assert_eq!(n["notice"], "User Operation struct");
        assert_eq!(n["params"]["sender"], "The sender account of this request.");
        assert_eq!(
            n["params"]["initCode"],
            "If set, the account contract will be created by this constructor with this initCode."
        );
        let m = &find(
            &x,
            "solidity:src/interfaces/PackedUserOperation.sol:method:Beacon::getTrustedAttesters",
        )
        .metadata["natspec"];
        assert_eq!(*m, json!({"inheritdoc": "IOnChainLabBeacon"}));
        assert_eq!(parse_natspec("@unknown x\ncontinued"), None);
    }
}

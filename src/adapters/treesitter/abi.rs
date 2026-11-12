//! Contract ABIs: the IDL a contract consumer is written against, and the
//! call sites that use it. tree-sitter node-shape walks only (never regex).
//!
//! | Pass | Found |
//! |---|---|
//! | [`json_spec`] | an ABI JSON file (`Language::Abi`): a plain ABI array, or a Foundry/Hardhat artifact's `abi` array (`contractName` names it) → one `Abi` symbol over the array only (bytecode never becomes a chunk) |
//! | [`ts_definitions`] | TS/JS constants holding an ABI: an `as const` array of ABI item objects (`ts_const`), `parseAbi([...])` / `parseAbiItem(...)` (`parse_abi`), an array of human-readable item strings (`human_readable`), and `[...A, ...B] as Abi` (`combined`, parts in `metadata.abi.combines`) |
//! | [`ts_contract_calls`] | objects with `functionName` / `eventName` / `abi` keys (viem actions, wagmi hooks, `decodeEventLog`, `encodeFunctionData`, …) → `FileExtraction.contract_calls` |
//!
//! Every `Abi` symbol carries `metadata.abi = {source, contract?, functions,
//! reads, events, errors}`: canonical `name(t1,t2)` signatures
//! ([`crate::domain::abi`]) for functions, events and errors, and the names of
//! the `view`/`pure` functions. Resolution (which ABI a call names, which
//! contract an ABI mirrors) happens in `application`.

use super::entry::{named_children, ts_consts, ts_prop, ts_string};
use super::lang::LangSpec;
use super::surface::string_value;
use super::typescript::{
    leading_comments, outer_statement, unique_id, value_signature, walk_named,
};
use super::walker::{enclosing_symbol, text};
use crate::domain::abi::{
    canonical_type, human_readable_item, signature, tuple_type, AbiItem, AbiItemKind, UNKNOWN_TYPE,
};
use crate::domain::model::NodeKind;
use crate::ports::extractor::{
    AbiScope, ContractCall, ContractMemberKind, FileExtraction, FileInput, Symbol,
};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use tree_sitter::{Node, Tree};

/// Share of array elements that must be ABI items for a JSON file to count.
const MIN_ABI_SHARE: (usize, usize) = (4, 5);
/// Alias hops followed when a call names an ABI through `const a = b`.
const MAX_ALIAS_HOPS: usize = 8;
/// Callees that take a contract-call object: viem actions and utilities,
/// wagmi hooks and actions (v1 and v2 names). A literal member with no ABI in
/// reach counts only inside one of these.
const CONTRACT_APIS: &[&str] = &[
    "createContractEventFilter",
    "decodeEventLog",
    "decodeFunctionData",
    "decodeFunctionResult",
    "encodeEventTopics",
    "encodeFunctionData",
    "encodeFunctionResult",
    "estimateContractGas",
    "getContractEvents",
    "multicall",
    "parseEventLogs",
    "prepareEncodeFunctionData",
    "readContract",
    "readContracts",
    "simulateContract",
    "useContractEvent",
    "useContractInfiniteReads",
    "useContractRead",
    "useContractReads",
    "useContractWrite",
    "useInfiniteReadContracts",
    "usePrepareContractWrite",
    "useReadContract",
    "useReadContracts",
    "useSimulateContract",
    "useWatchContractEvent",
    "useWriteContract",
    "watchContractEvent",
    "writeContract",
    "writeContractAsync",
];

static JSON: LangSpec = LangSpec {
    language: || tree_sitter_json::LANGUAGE.into(),
    defs: &[],
    import_query: "",
    call_query: "",
    doc_comment_types: &[],
    body_types: &["array"],
    extras: &[json_definitions],
};

/// The ABI JSON language specification (`Language::Abi`).
pub fn json_spec() -> Option<&'static LangSpec> {
    Some(&JSON)
}

// ---------------------------------------------------------------------------
// shared: ABI item objects (JSON and TS object literals share node names)
// ---------------------------------------------------------------------------

/// Non-comment named children.
fn elements(n: Node) -> Vec<Node> {
    named_children(n)
}

/// Unwrap `x as const`, `x satisfies T`, `(x)`, `x!`.
fn unwrap_expr(mut n: Node) -> Node {
    while matches!(
        n.kind(),
        "as_expression"
            | "satisfies_expression"
            | "parenthesized_expression"
            | "non_null_expression"
    ) {
        match n.named_child(0) {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

/// A string literal: JSON (escapes decoded) or TS/JS (`'x'`, `"x"`, a plain
/// template), `as const` unwrapped.
fn lit_str(n: Node, src: &str) -> Option<String> {
    let n = unwrap_expr(n);
    if n.kind() != "string" && n.kind() != "template_string" {
        return None;
    }
    let t = text(n, src);
    if t.starts_with('"') {
        if let Ok(s) = serde_json::from_str::<String>(t) {
            return Some(s);
        }
    }
    string_value(n, src)
}

fn prop<'t>(obj: Node<'t>, key: &str, src: &str) -> Option<Node<'t>> {
    ts_prop(obj, key, src)
}

/// Canonical type of one ABI parameter object (`{type, components?}`):
/// tuples written out from their components.
fn param_type(p: Node, src: &str) -> String {
    let p = unwrap_expr(p);
    if p.kind() != "object" {
        return UNKNOWN_TYPE.into();
    }
    let Some(t) = prop(p, "type", src).and_then(|v| lit_str(v, src)) else {
        return UNKNOWN_TYPE.into();
    };
    if let Some(suffix) = t.strip_prefix("tuple") {
        let comps = prop(p, "components", src)
            .map(unwrap_expr)
            .filter(|c| c.kind() == "array");
        if let Some(c) = comps {
            let inner: Vec<String> = elements(c)
                .into_iter()
                .map(|e| param_type(e, src))
                .collect();
            return tuple_type(&inner, suffix);
        }
    }
    canonical_type(&t)
}

/// One ABI item from an object literal. `explicit_type` requires a `type`
/// pair (TS); a JSON item without one is a function (old solc output).
fn item_from_object(obj: Node, src: &str, explicit_type: bool) -> Option<AbiItem> {
    let obj = unwrap_expr(obj);
    if obj.kind() != "object" {
        return None;
    }
    let ty = match prop(obj, "type", src) {
        Some(v) => lit_str(v, src)?,
        None if explicit_type => return None,
        None => "function".into(),
    };
    let kind = AbiItemKind::parse(&ty)?;
    let name = match kind {
        AbiItemKind::Function | AbiItemKind::Event | AbiItemKind::Error => {
            prop(obj, "name", src).and_then(|v| lit_str(v, src))?
        }
        _ => ty.clone(),
    };
    let types: Vec<String> = prop(obj, "inputs", src)
        .map(unwrap_expr)
        .filter(|a| a.kind() == "array")
        .map(|a| {
            elements(a)
                .into_iter()
                .map(|p| param_type(p, src))
                .collect()
        })
        .unwrap_or_default();
    let mutability = prop(obj, "stateMutability", src)
        .and_then(|v| lit_str(v, src))
        .or_else(|| {
            prop(obj, "constant", src)
                .filter(|v| text(*v, src) == "true")
                .map(|_| "view".to_string())
        });
    Some(AbiItem {
        kind,
        signature: signature(&name, &types),
        name,
        mutability,
    })
}

/// `metadata.abi` for a set of items.
fn abi_metadata(source: &str, contract: Option<&str>, items: &[AbiItem]) -> Value {
    let of = |k: AbiItemKind| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i.kind == k)
            .map(|i| i.signature.as_str())
            .collect()
    };
    let reads: Vec<&str> = items
        .iter()
        .filter(|i| {
            i.kind == AbiItemKind::Function
                && matches!(i.mutability.as_deref(), Some("view" | "pure"))
        })
        .map(|i| i.name.as_str())
        .collect();
    let mut abi = json!({
        "source": source,
        "functions": of(AbiItemKind::Function),
        "reads": reads,
        "events": of(AbiItemKind::Event),
        "errors": of(AbiItemKind::Error),
    });
    if let Some(c) = contract {
        abi["contract"] = json!(c);
    }
    json!({ "abi": abi })
}

/// `"<name> ABI: 23 functions, 5 events, 14 errors"`.
fn abi_summary(name: &str, items: &[AbiItem]) -> String {
    let n = |k: AbiItemKind| items.iter().filter(|i| i.kind == k).count();
    format!(
        "{name} ABI: {} functions, {} events, {} errors",
        n(AbiItemKind::Function),
        n(AbiItemKind::Event),
        n(AbiItemKind::Error)
    )
}

// ---------------------------------------------------------------------------
// ABI JSON files
// ---------------------------------------------------------------------------

/// File stem before the first `.`: `abis/AccessResolver.json` → `AccessResolver`.
fn file_stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.split('.').next().unwrap_or(file)
}

/// A plain ABI array, or a Foundry/Hardhat artifact's `abi` array, becomes one
/// `Abi` symbol over the array; anything else stays file-level with a warning.
fn json_definitions(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let not_abi =
        |out: &mut FileExtraction| out.warnings.push(format!("{}: not an ABI", file.path));
    let Some(value) = elements(tree.root_node()).into_iter().next() else {
        return not_abi(out);
    };
    let (array, contract) = match value.kind() {
        // A truncated file parses as one ERROR node holding the complete items.
        "array" | "ERROR" => (value, None),
        "object" => match prop(value, "abi", src).filter(|a| a.kind() == "array") {
            Some(a) => (
                a,
                prop(value, "contractName", src).and_then(|v| lit_str(v, src)),
            ),
            None => return not_abi(out),
        },
        _ => return not_abi(out),
    };
    let elems: Vec<Node> = elements(array)
        .into_iter()
        .filter(|e| !e.is_error() && (!array.is_error() || e.kind() == "object"))
        .collect();
    let items: Vec<AbiItem> = elems
        .iter()
        .filter_map(|e| item_from_object(*e, src, false))
        .collect();
    let (num, den) = MIN_ABI_SHARE;
    if elems.is_empty() || items.len() * den < elems.len() * num {
        return not_abi(out);
    }
    let name = contract
        .clone()
        .unwrap_or_else(|| file_stem(&file.path).to_string());
    out.symbols.push(Symbol {
        stable_id: format!("{}:{}:abi:{name}", file.language.as_str(), file.path),
        kind: NodeKind::Abi,
        name: name.clone(),
        qualified_name: name.clone(),
        parent: None,
        signature: Some(abi_summary(&name, &items)),
        doc: None,
        line_start: array.start_position().row as u32 + 1,
        line_end: array.end_position().row as u32 + 1,
        byte_start: array.start_byte(),
        byte_end: array.end_byte(),
        segments: elems
            .iter()
            .map(|e| (e.start_byte(), e.end_byte()))
            .collect(),
        metadata: abi_metadata("json", contract.as_deref(), &items),
    });
    // The ABI array is the unit; bytecode and other artifact keys never chunk.
    out.free_ranges.clear();
}

// ---------------------------------------------------------------------------
// TS/JS ABI constants
// ---------------------------------------------------------------------------

/// The ABI-helper name a call invokes: `parseAbi(…)` or `x.parseAbi(…)`.
fn callee_name<'s>(call: Node, src: &'s str) -> Option<&'s str> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => Some(text(f, src)),
        "member_expression" => f.child_by_field_name("property").map(|p| text(p, src)),
        _ => None,
    }
}

fn is_parse_abi(name: &str) -> bool {
    matches!(name, "parseAbi" | "parseAbiItem")
}

/// Whether a type node names viem/abitype's `Abi` (`Abi`, `readonly Abi`, `Abi | …`).
fn names_abi_type(n: Node, src: &str) -> bool {
    text(n, src)
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w == "Abi")
}

/// The value with `as`/`satisfies`/parentheses unwrapped, and whether any
/// layer (or the declarator's type annotation) asserts `Abi`.
fn unwrap_asserted<'t>(decl: Node<'t>, value: Node<'t>, src: &str) -> (Node<'t>, bool) {
    let mut asserted = decl
        .child_by_field_name("type")
        .is_some_and(|t| names_abi_type(t, src));
    let mut n = value;
    loop {
        match n.kind() {
            "as_expression" | "satisfies_expression" => {
                let kids = elements(n);
                if kids.iter().skip(1).any(|t| names_abi_type(*t, src)) {
                    asserted = true;
                }
                match kids.first() {
                    Some(c) => n = *c,
                    None => break,
                }
            }
            "parenthesized_expression" | "non_null_expression" => match n.named_child(0) {
                Some(c) => n = c,
                None => break,
            },
            _ => break,
        }
    }
    (n, asserted)
}

/// Default imports of `.json` / `.abi` files (and `require` of one):
/// local name → specifier.
fn json_default_imports(root: Node, src: &str) -> HashMap<String, String> {
    let is_abi_spec = |s: &str| {
        let l = s.to_ascii_lowercase();
        l.ends_with(".json") || l.ends_with(".abi")
    };
    let mut out = HashMap::new();
    walk_named(root, |n| match n.kind() {
        "import_statement" => {
            let Some(spec) = n
                .child_by_field_name("source")
                .and_then(|s| lit_str(s, src))
            else {
                return;
            };
            if !is_abi_spec(&spec) {
                return;
            }
            let clause = elements(n)
                .into_iter()
                .find(|c| c.kind() == "import_clause");
            if let Some(local) = clause
                .and_then(|c| c.named_child(0))
                .filter(|c| c.kind() == "identifier")
            {
                out.insert(text(local, src).to_string(), spec);
            }
        }
        "variable_declarator" => {
            let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) else {
                return;
            };
            let value = unwrap_expr(value);
            if name.kind() != "identifier" || value.kind() != "call_expression" {
                return;
            }
            if callee_name(value, src) != Some("require") {
                return;
            }
            let spec = value
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
                .and_then(|a| lit_str(a, src));
            if let Some(spec) = spec.filter(|s| is_abi_spec(s)) {
                out.insert(text(name, src).to_string(), spec);
            }
        }
        _ => {}
    });
    out
}

/// What a declarator's value holds, when it is an ABI.
struct Found<'t> {
    source: &'static str,
    items: Vec<AbiItem>,
    segments: Vec<Node<'t>>,
    combines: Vec<Value>,
}

/// Items of an array of human-readable strings (struct definitions skipped);
/// `None` unless every other element parses.
fn human_readable_array<'t>(arr: Node<'t>, src: &str) -> Option<(Vec<AbiItem>, Vec<Node<'t>>)> {
    let elems = elements(arr);
    if elems.is_empty() {
        return None;
    }
    let mut items = Vec::new();
    for e in &elems {
        let s = lit_str(*e, src)?;
        if s.trim_start().starts_with("struct ") {
            continue;
        }
        items.push(human_readable_item(&s)?);
    }
    (!items.is_empty()).then_some((items, elems))
}

fn classify<'t>(
    value: Node<'t>,
    asserted: bool,
    src: &str,
    imports: &HashMap<String, String>,
) -> Option<Found<'t>> {
    match value.kind() {
        "array" => {
            let elems = elements(value);
            if elems.is_empty() {
                return None;
            }
            if elems.iter().all(|e| unwrap_expr(*e).kind() == "object") {
                let items: Option<Vec<AbiItem>> = elems
                    .iter()
                    .map(|e| item_from_object(*e, src, true))
                    .collect();
                let items = items?;
                let callable = items
                    .iter()
                    .any(|i| matches!(i.kind, AbiItemKind::Function | AbiItemKind::Event));
                return callable.then_some(Found {
                    source: "ts_const",
                    items,
                    segments: elems,
                    combines: vec![],
                });
            }
            if let Some((items, segments)) = human_readable_array(value, src) {
                return Some(Found {
                    source: "human_readable",
                    items,
                    segments,
                    combines: vec![],
                });
            }
            let spreads: Option<Vec<String>> = elems
                .iter()
                .map(|e| {
                    let inner = (e.kind() == "spread_element")
                        .then(|| e.named_child(0))
                        .flatten()
                        .map(unwrap_expr)?;
                    spread_part(inner, src)
                })
                .collect();
            let spreads = spreads.filter(|_| asserted)?;
            let combines = spreads
                .into_iter()
                .map(|name| match imports.get(&name) {
                    Some(spec) => json!({"name": name, "import": spec}),
                    None => json!({"name": name}),
                })
                .collect();
            Some(Found {
                source: "combined",
                items: vec![],
                segments: elems,
                combines,
            })
        }
        "call_expression" => {
            if !is_parse_abi(callee_name(value, src)?) {
                return None;
            }
            let arg = value.child_by_field_name("arguments")?.named_child(0)?;
            let arg = unwrap_expr(arg);
            let (items, segments) = match arg.kind() {
                "array" => human_readable_array(arg, src)?,
                "string" | "template_string" => {
                    (vec![human_readable_item(&lit_str(arg, src)?)?], vec![arg])
                }
                _ => return None, // `parseAbi(OTHER)` is an alias, not a definition
            };
            Some(Found {
                source: "parse_abi",
                items,
                segments,
                combines: vec![],
            })
        }
        _ => None,
    }
}

/// The ABI a combined-ABI spread names: `...A`, or `...A.filter(…)` /
/// `...A.slice(…)` (desci-infra `LOCKED_TOKEN_COMBINED_ABI` drops one event
/// of `LockedToken`).
fn spread_part(n: Node, src: &str) -> Option<String> {
    match n.kind() {
        "identifier" => Some(text(n, src).to_string()),
        "call_expression" => {
            let f = n.child_by_field_name("function")?;
            let prop = f.child_by_field_name("property")?;
            let obj = unwrap_expr(f.child_by_field_name("object")?);
            (f.kind() == "member_expression"
                && matches!(text(prop, src), "filter" | "slice")
                && obj.kind() == "identifier")
                .then(|| text(obj, src).to_string())
        }
        _ => None,
    }
}

/// TS/JS ABI constants become `Abi` symbols (see the module docs). Runs right
/// after `function_values`, so `own_exports` takes their statements out of
/// the file head.
pub fn ts_definitions(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let root = tree.root_node();
    let imports = json_default_imports(root, src);
    let mut found: Vec<(Node, Node, String, Found)> = Vec::new();
    walk_named(root, |n| {
        if n.kind() != "variable_declarator" {
            return;
        }
        let (Some(name), Some(value)) = (
            n.child_by_field_name("name"),
            n.child_by_field_name("value"),
        ) else {
            return;
        };
        if name.kind() != "identifier" {
            return;
        }
        let (value, asserted) = unwrap_asserted(n, value, src);
        if let Some(f) = classify(value, asserted, src, &imports) {
            let decl = n.parent().unwrap_or(n);
            found.push((outer_statement(decl), value, text(name, src).to_string(), f));
        }
    });
    for (outer, value, name, f) in found {
        let base = format!("{}:{}:abi:{name}", file.language.as_str(), file.path);
        let (doc, _) = leading_comments(outer, src);
        let mut metadata = abi_metadata(f.source, None, &f.items);
        if !f.combines.is_empty() {
            metadata["abi"]["combines"] = Value::Array(f.combines);
        }
        out.symbols.push(Symbol {
            stable_id: unique_id(&out.symbols, base),
            kind: NodeKind::Abi,
            name: name.clone(),
            qualified_name: name,
            parent: None,
            signature: Some(value_signature(outer, value.start_byte(), src)),
            doc,
            line_start: outer.start_position().row as u32 + 1,
            line_end: outer.end_position().row as u32 + 1,
            byte_start: outer.start_byte(),
            byte_end: outer.end_byte(),
            segments: f
                .segments
                .iter()
                .map(|s| (s.start_byte(), s.end_byte()))
                .collect(),
            metadata,
        });
    }
    out.symbols.sort_by_key(|s| s.byte_start);
}

// ---------------------------------------------------------------------------
// TS/JS contract call sites
// ---------------------------------------------------------------------------

/// The identifier an `abi:` value names: `X`, `X as Abi`, the first segment
/// of `X.abi`, `parseAbi(X)`; `None` for inline ABIs and anything else.
fn abi_ident(n: Node, src: &str) -> Option<String> {
    let mut n = unwrap_expr(n);
    loop {
        match n.kind() {
            "identifier" | "shorthand_property_identifier" => {
                return Some(text(n, src).to_string())
            }
            "member_expression" => n = unwrap_expr(n.child_by_field_name("object")?),
            "call_expression" if is_parse_abi(callee_name(n, src)?) => {
                n = unwrap_expr(n.child_by_field_name("arguments")?.named_child(0)?)
            }
            _ => return None,
        }
    }
}

/// The identifier a declarator's value aliases: `B`, `B as Abi`,
/// `parseAbi(B)`, `B.abi` → B.
fn alias_target(value: Node, src: &str) -> Option<String> {
    let v = unwrap_expr(value);
    match v.kind() {
        "identifier" | "member_expression" => abi_ident(v, src),
        "call_expression" if callee_name(v, src).is_some_and(is_parse_abi) => abi_ident(v, src),
        _ => None,
    }
}

/// Function-like nodes: their parameters and `var` declarations live there.
fn is_function_scope(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "function"
            | "arrow_function"
            | "method_definition"
            | "generator_function_declaration"
            | "generator_function"
    )
}

/// The scope a `const`/`let` (nearest block) or `var` (nearest function)
/// declarator belongs to; the program at the top.
fn declarator_scope(decl: Node) -> Option<Node> {
    let stmt = decl.parent()?;
    let function_scoped = stmt.kind() == "variable_declaration";
    let mut cur = stmt.parent();
    while let Some(p) = cur {
        let block = matches!(
            p.kind(),
            "statement_block" | "switch_body" | "for_statement"
        );
        if p.kind() == "program" || is_function_scope(p.kind()) || (block && !function_scoped) {
            return Some(p);
        }
        cur = p.parent();
    }
    None
}

/// Identifiers a binding pattern or parameter list declares (default values
/// and type annotations left out).
fn bound_names(n: Node, src: &str, out: &mut Vec<String>) {
    let field = match n.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.push(text(n, src).to_string());
            return;
        }
        "object_pattern" | "array_pattern" | "rest_pattern" | "formal_parameters" => {
            for c in elements(n) {
                bound_names(c, src, out);
            }
            return;
        }
        "pair_pattern" => "value",
        "assignment_pattern" | "object_assignment_pattern" => "left",
        "required_parameter" | "optional_parameter" => "pattern",
        _ => return,
    };
    if let Some(c) = n.child_by_field_name(field) {
        bound_names(c, src, out);
    }
}

/// Same-file ABI aliases, resolved by lexical scope: `const a = B`, `const a
/// = B as Abi`, `const a = parseAbi(B)`, `const a = B.abi` make `a` → B in
/// the scope of the declaration. Every other declaration and every
/// parameter of the same name shadows outer aliases.
struct AbiAliases<'t> {
    /// Scope node id → name → each declaration there.
    scopes: HashMap<usize, HashMap<String, Vec<Decl<'t>>>>,
}

/// One declaration of a name: its alias target (`None` = not an alias) and its node.
type Decl<'t> = (Option<String>, Node<'t>);

impl<'t> AbiAliases<'t> {
    fn build(root: Node<'t>, src: &str) -> Self {
        let mut scopes: HashMap<usize, HashMap<String, Vec<Decl<'t>>>> = HashMap::new();
        let mut declare = |scope: Node<'t>, names: Vec<String>, target: Option<String>, at| {
            for name in names {
                scopes
                    .entry(scope.id())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push((target.clone(), at));
            }
        };
        walk_named(root, |n| {
            let mut names = Vec::new();
            match n.kind() {
                "variable_declarator" => {
                    let (Some(name), Some(scope)) =
                        (n.child_by_field_name("name"), declarator_scope(n))
                    else {
                        return;
                    };
                    bound_names(name, src, &mut names);
                    let target = (name.kind() == "identifier")
                        .then(|| n.child_by_field_name("value"))
                        .flatten()
                        .and_then(|v| alias_target(v, src))
                        .filter(|t| t != text(name, src));
                    declare(scope, names, target, n);
                }
                "formal_parameters" => {
                    if let Some(f) = n.parent() {
                        bound_names(n, src, &mut names);
                        declare(f, names, None, n);
                    }
                }
                "arrow_function" => {
                    if let Some(p) = n.child_by_field_name("parameter") {
                        bound_names(p, src, &mut names);
                        declare(n, names, None, p);
                    }
                }
                _ => {}
            }
        });
        Self { scopes }
    }

    /// The declarations of `name` in the innermost scope around `at` that has one.
    fn lookup(&self, at: Node<'t>, name: &str) -> Option<&[Decl<'t>]> {
        let mut cur = Some(at);
        while let Some(n) = cur {
            if let Some(ds) = self.scopes.get(&n.id()).and_then(|m| m.get(name)) {
                return Some(ds);
            }
            cur = n.parent();
        }
        None
    }

    /// `name` as used at `at`, followed through aliases, each hop resolved
    /// from its declaration. A name that is not an alias there, or that one
    /// scope declares twice (ambiguous), stays as written.
    fn follow(&self, at: Node<'t>, name: String) -> String {
        let mut cur = name;
        let mut from = at;
        for _ in 0..MAX_ALIAS_HOPS {
            match self.lookup(from, &cur) {
                Some([(Some(next), decl)]) => {
                    from = *decl;
                    cur = next.clone();
                }
                _ => break,
            }
        }
        cur
    }
}

/// The function an object literal is passed to: the nearest call whose
/// arguments hold it, through arrays, pairs and enclosing objects
/// (`useReadContracts({contracts: [{…}]})` → `useReadContracts`).
fn via_call(obj: Node, src: &str) -> Option<String> {
    let mut cur = obj;
    loop {
        let p = cur.parent()?;
        match p.kind() {
            "arguments" => {
                let call = p.parent().filter(|c| c.kind() == "call_expression")?;
                return callee_name(call, src).map(str::to_string);
            }
            "array"
            | "pair"
            | "object"
            | "as_expression"
            | "satisfies_expression"
            | "parenthesized_expression"
            | "spread_element" => cur = p,
            _ => return None,
        }
    }
}

/// One call-site object before the symbol-scope ABI fallback.
struct Site {
    members: Vec<(Option<String>, ContractMemberKind)>,
    /// `Some(None)` = an `abi` key whose value names no identifier (inline ABI).
    abi: Option<Option<String>>,
    via: Option<String>,
    owner: Option<String>,
    line: u32,
    at: usize,
}

/// The ABI the objects inside the innermost enclosing symbol name, widening
/// to outer symbols while none is named: `None` when no level names one,
/// `Some(None)` when the first level that does names two different ABIs. A
/// file-level object looks at file-level objects only.
fn scoped_abi(site: &Site, sites: &[Site], symbols: &[Symbol]) -> Option<Option<String>> {
    let mut chain: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.kind != NodeKind::Abi && s.byte_start <= site.at && site.at < s.byte_end)
        .collect();
    chain.sort_by_key(|s| s.byte_end - s.byte_start);
    let named = |inside: &dyn Fn(&Site) -> bool| -> BTreeSet<&str> {
        sites
            .iter()
            .filter(|o| inside(o))
            .filter_map(|o| o.abi.as_ref().and_then(|a| a.as_deref()))
            .collect()
    };
    let pick = |set: BTreeSet<&str>| match set.len() {
        0 => None,
        1 => Some(set.into_iter().next().map(str::to_string)),
        _ => Some(None),
    };
    if chain.is_empty() {
        return pick(named(&|o: &Site| o.owner.is_none()));
    }
    chain
        .into_iter()
        .find_map(|s| pick(named(&|o: &Site| s.byte_start <= o.at && o.at < s.byte_end)))
}

/// Contract call sites (see the module docs): every object with a
/// `functionName` / `eventName` / `abi` key. An object with a member but no
/// `abi` takes the single ABI named by the objects of its enclosing symbol
/// (`AbiScope::Symbol`, see [`scoped_abi`]); two different ABIs there leave
/// it `None`. Kept: an ABI-only use where the object names the ABI; a literal
/// member when its object names an ABI, an enclosing symbol names one, or it
/// is passed to a [`CONTRACT_APIS`] callee. So `results.push({eventName})` and
/// `logger.info({functionName: call.functionName})` are not calls.
pub fn ts_contract_calls(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let root = tree.root_node();
    let consts = ts_consts(root, src);
    let imports = json_default_imports(root, src);
    let aliases = AbiAliases::build(root, src);
    let symbols = out.symbols.clone();
    let line_of = |n: Node| n.start_position().row as u32 + 1;
    let mut sites: Vec<Site> = Vec::new();
    walk_named(root, |n| {
        if n.kind() != "object" {
            return;
        }
        let mut members: Vec<(Option<String>, ContractMemberKind)> = Vec::new();
        let mut member_line = None;
        let mut abi: Option<Option<String>> = None;
        let mut abi_line = None;
        for p in elements(n) {
            let (key, value) = match p.kind() {
                "pair" => {
                    let Some(k) = p.child_by_field_name("key") else {
                        continue;
                    };
                    let key = string_value(k, src).unwrap_or_else(|| text(k, src).to_string());
                    (key, p.child_by_field_name("value"))
                }
                "shorthand_property_identifier" => (text(p, src).to_string(), None),
                _ => continue,
            };
            let kind = match key.as_str() {
                "functionName" => ContractMemberKind::Function,
                "eventName" => ContractMemberKind::Event,
                "abi" => {
                    abi = Some(match value {
                        Some(v) => abi_ident(v, src).map(|a| aliases.follow(v, a)),
                        None => Some(aliases.follow(p, "abi".into())),
                    });
                    abi_line = Some(line_of(p));
                    continue;
                }
                _ => continue,
            };
            member_line = Some(line_of(p));
            let value = value.map(unwrap_expr);
            match value {
                Some(v) if v.kind() == "array" => {
                    let names: Vec<String> = elements(v)
                        .into_iter()
                        .filter_map(|e| ts_string(e, src, &consts))
                        .collect();
                    if names.is_empty() {
                        members.push((None, ContractMemberKind::Abi));
                    }
                    members.extend(names.into_iter().map(|m| (Some(m), kind)));
                }
                Some(v) => match ts_string(v, src, &consts) {
                    Some(m) => members.push((Some(m), kind)),
                    None => members.push((None, ContractMemberKind::Abi)),
                },
                None => members.push((None, ContractMemberKind::Abi)),
            }
        }
        if members.is_empty() && abi.is_none() {
            return;
        }
        if members.is_empty() {
            members.push((None, ContractMemberKind::Abi));
        }
        let owner = enclosing_symbol(&symbols, &file.path, n.start_byte());
        sites.push(Site {
            members,
            abi,
            via: via_call(n, src),
            owner: (!owner.starts_with("file:")).then_some(owner),
            line: member_line.or(abi_line).unwrap_or_else(|| line_of(n)),
            at: n.start_byte(),
        });
    });

    let mut calls: Vec<ContractCall> = Vec::new();
    for s in &sites {
        let scoped = match s.abi {
            Some(_) => None,
            None => scoped_abi(s, &sites, &symbols),
        };
        let (abi, scope) = match (&s.abi, &scoped) {
            (Some(a), _) => (a.clone(), AbiScope::Object),
            (None, Some(Some(a))) => (Some(a.clone()), AbiScope::Symbol),
            _ => (None, AbiScope::Object),
        };
        let api = s.via.as_deref().is_some_and(|v| CONTRACT_APIS.contains(&v));
        for (member, kind) in s.members.iter().cloned() {
            let keep = match member {
                None => s.abi.is_some() && abi.is_some(),
                Some(_) => s.abi.is_some() || scoped.is_some() || api,
            };
            if !keep {
                continue;
            }
            let call = ContractCall {
                member,
                kind,
                abi_import: abi.as_ref().and_then(|a| imports.get(a)).cloned(),
                abi: abi.clone(),
                scope,
                via: s.via.clone(),
                owner: s.owner.clone(),
                line: s.line,
            };
            if !calls.contains(&call) {
                calls.push(call);
            }
        }
    }
    calls.sort_by_key(|c| c.line);
    out.contract_calls = calls;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::treesitter::{typescript, walker};
    use crate::domain::model::Language;

    fn json_file(path: &str, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: Language::Abi,
            content: src.into(),
        };
        walker::extract(json_spec().unwrap(), &file)
    }

    fn ts_file(path: &str, src: &str) -> FileExtraction {
        let lang = Language::from_path(path).unwrap();
        let file = FileInput {
            path: path.into(),
            language: lang,
            content: src.into(),
        };
        walker::extract(typescript::spec(lang, path).unwrap(), &file)
    }

    fn abis(x: &FileExtraction) -> Vec<&Symbol> {
        x.symbols
            .iter()
            .filter(|s| s.kind == NodeKind::Abi)
            .collect()
    }

    fn strs(v: &Value) -> Vec<&str> {
        v.as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    const ACCESS_RESOLVER_JSON: &str = r#"[
  {"type": "constructor", "inputs": [], "stateMutability": "nonpayable"},
  {"type": "function", "name": "ROLE_VIEWER", "inputs": [], "outputs": [{"name": "", "type": "uint8"}], "stateMutability": "view"},
  {"type": "function", "name": "grantRole", "inputs": [
    {"name": "oclId", "type": "bytes32", "internalType": "bytes32"},
    {"name": "account", "type": "address", "internalType": "address"},
    {"name": "role", "type": "uint8", "internalType": "uint8"},
    {"name": "expiry", "type": "uint64", "internalType": "uint64"},
    {"name": "isAgent", "type": "bool", "internalType": "bool"}
  ], "outputs": [], "stateMutability": "nonpayable"},
  {"type": "event", "name": "RoleGranted", "inputs": [{"name": "oclId", "type": "bytes32", "indexed": true}, {"name": "terms", "type": "tuple", "components": [{"name": "a", "type": "uint256"}, {"name": "b", "type": "address[]"}]}], "anonymous": false},
  {"type": "error", "name": "Unauthorized", "inputs": []}
]
"#;

    #[test]
    fn json_plain_abi_array_becomes_one_abi_symbol() {
        let path = "lambda/common/assets/abis/AccessResolver.json";
        let x = json_file(path, ACCESS_RESOLVER_JSON);
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        let [s] = abis(&x)[..] else {
            panic!("{:?}", x.symbols)
        };
        assert_eq!(s.stable_id, format!("abi:{path}:abi:AccessResolver"));
        assert_eq!(s.name, "AccessResolver");
        assert_eq!(s.segments.len(), 5);
        let abi = &s.metadata["abi"];
        assert_eq!(abi["source"], "json");
        assert_eq!(
            strs(&abi["functions"]),
            [
                "ROLE_VIEWER()",
                "grantRole(bytes32,address,uint8,uint64,bool)"
            ]
        );
        assert_eq!(strs(&abi["reads"]), ["ROLE_VIEWER"]);
        assert_eq!(
            strs(&abi["events"]),
            ["RoleGranted(bytes32,(uint256,address[]))"]
        );
        assert_eq!(strs(&abi["errors"]), ["Unauthorized()"]);
        assert!(x.free_ranges.is_empty());
        assert_eq!(
            s.signature.as_deref(),
            Some("AccessResolver ABI: 2 functions, 1 events, 1 errors")
        );
    }

    #[test]
    fn json_solc_abi_without_type_and_with_constant() {
        let x = json_file(
            "bin/src/interfaces/IAccessResolver.abi",
            r#"[{"constant":true,"inputs":[{"name":"signer","type":"address"},{"name":"ipnftId","type":"uint256"}],"name":"isAuthorizedSignerForIpnft","outputs":[{"type":"bool"}]}]"#,
        );
        let s = abis(&x)[0];
        assert_eq!(s.name, "IAccessResolver");
        assert_eq!(
            strs(&s.metadata["abi"]["functions"]),
            ["isAuthorizedSignerForIpnft(address,uint256)"]
        );
        assert_eq!(
            strs(&s.metadata["abi"]["reads"]),
            ["isAuthorizedSignerForIpnft"]
        );
    }

    #[test]
    fn json_foundry_artifact_uses_abi_array_only() {
        let src = r#"{"abi": [{"type": "function", "name": "mintFeeWei", "inputs": [], "stateMutability": "view"}], "bytecode": {"object": "0x6080604052"}}"#;
        let x = json_file("out/LabNFT.sol/LabNFT.abi.json", src);
        let s = abis(&x)[0];
        assert_eq!(s.name, "LabNFT");
        assert!(src[s.byte_start..s.byte_end].starts_with('['));
        assert!(!src[s.byte_start..s.byte_end].contains("bytecode"));
        assert!(
            x.free_ranges.is_empty(),
            "no file_head: {:?}",
            x.free_ranges
        );
    }

    #[test]
    fn json_hardhat_artifact_takes_contract_name() {
        let x = json_file(
            "artifacts/abi/x.json",
            r#"{"_format": "hh-sol-artifact-1", "contractName": "OclTokenizer", "abi": [{"type": "event", "name": "Tokenized", "inputs": []}]}"#,
        );
        let s = abis(&x)[0];
        assert_eq!(s.name, "OclTokenizer");
        assert_eq!(s.metadata["abi"]["contract"], "OclTokenizer");
        assert_eq!(strs(&s.metadata["abi"]["events"]), ["Tokenized()"]);
    }

    #[test]
    fn json_non_abi_is_file_level_with_warning() {
        let x = json_file("abis/config.json", r#"{"chains": [1, 8453]}"#);
        assert!(abis(&x).is_empty());
        assert!(x.warnings.iter().any(|w| w.contains("not an ABI")));
        assert!(!x.free_ranges.is_empty());
        let y = json_file("abis/list.json", r#"[{"label": "a"}, {"label": "b"}]"#);
        assert!(abis(&y).is_empty());
    }

    #[test]
    fn json_syntax_error_recovers_partial_items() {
        let x = json_file(
            "abis/Broken.json",
            r#"[{"type": "function", "name": "a", "inputs": []}, {"type": "function", "name": "b", "inputs": [] ,"#,
        );
        assert!(!x.warnings.is_empty());
        let s = abis(&x)[0];
        assert!(strs(&s.metadata["abi"]["functions"]).contains(&"a()"));
    }

    #[test]
    fn json_tuple_suffix_is_canonical_or_unknown() {
        let x = json_file(
            "abis/Tuples.json",
            r#"[{"type":"function","name":"f","inputs":[
  {"type":"tuple[2] []","components":[{"type":"uint"},{"type":"address[]"}]},
  {"type":"tuple,(]é,x)","components":[{"type":"uint256"}]}
]}]"#,
        );
        let s = abis(&x)[0];
        assert_eq!(
            strs(&s.metadata["abi"]["functions"]),
            ["f((uint256,address[])[2][],*)"]
        );
    }

    const OCL_TOKENIZER_TS: &str = r#"// OclTokenizer — tokenizes an OnChainLab (OCL).
// Source: moleculeprotocol/ocltokenizer `src/OclTokenizer.sol`.
export const OclTokenizerAbi = [
  {
    inputs: [{ internalType: 'uint256', name: 'labId', type: 'uint256' }],
    name: 'tokenized',
    outputs: [{ internalType: 'contract ILabToken', name: '', type: 'address' }],
    stateMutability: 'view',
    type: 'function',
  },
  { inputs: [], name: 'MustControlLab', type: 'error' },
] as const

// Minimal read surface shared by LabToken and WrappedLabToken (ILabToken).
export const LabTokenAbi = [
  // Links the token back to its lab.
  {
    inputs: [],
    name: 'metadata',
    outputs: [{ name: '', type: 'string' }],
    stateMutability: 'view',
    type: 'function',
  },
] as const
"#;

    #[test]
    fn ts_as_const_abi_array_is_an_abi_symbol() {
        let x = ts_file(
            "packages/common/src/abis/ocl-tokenizer.ts",
            OCL_TOKENIZER_TS,
        );
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        let found = abis(&x);
        let ids: Vec<&str> = found.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "typescript:packages/common/src/abis/ocl-tokenizer.ts:abi:OclTokenizerAbi",
                "typescript:packages/common/src/abis/ocl-tokenizer.ts:abi:LabTokenAbi"
            ]
        );
        let t = found[0];
        assert!(t
            .doc
            .as_deref()
            .unwrap()
            .starts_with("OclTokenizer — tokenizes"));
        assert_eq!(t.signature.as_deref(), Some("export const OclTokenizerAbi"));
        assert_eq!(t.metadata["abi"]["source"], "ts_const");
        assert_eq!(
            strs(&t.metadata["abi"]["functions"]),
            ["tokenized(uint256)"]
        );
        assert_eq!(strs(&t.metadata["abi"]["errors"]), ["MustControlLab()"]);
        assert_eq!(t.segments.len(), 2);
        assert_eq!(found[1].segments.len(), 1, "comments are not segments");
        assert!(x.free_ranges.is_empty(), "{:?}", x.free_ranges);
    }

    #[test]
    fn ts_satisfies_abi_parse_abi_and_human_readable() {
        let src = r#"import { parseAbi, type Abi } from 'viem';
const REGISTRY_ABI = parseAbi([
  "function getActiveDid(bytes32 oclId, bytes32 provider, bytes32 subject) view returns (string)",
  "function paused() view returns (bool)",
]);
const ACCESS_RESOLVER_ABI = [
  'function grantRole(bytes32 oclId, address account, uint8 role, uint64 expiry, bool isAgent) external',
  'event RoleGranted(bytes32 indexed oclId, address indexed account, uint8 indexed role, uint64 expiry, bool isAgent, address grantedBy)',
];
export const OwnerAbi = [{ type: 'function', name: 'ownerOf', inputs: [{ type: 'uint256' }], stateMutability: 'view' }] satisfies Abi;
const ONE = parseAbiItem('function owner() view returns (address)');
const NOT = ['a', 'b'];
"#;
        let x = ts_file("scripts/link.ts", src);
        let found: Vec<(&str, &str)> = abis(&x)
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.metadata["abi"]["source"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("REGISTRY_ABI", "parse_abi"),
                ("ACCESS_RESOLVER_ABI", "human_readable"),
                ("OwnerAbi", "ts_const"),
                ("ONE", "parse_abi"),
            ]
        );
        let reg = abis(&x)[0];
        assert_eq!(
            strs(&reg.metadata["abi"]["functions"]),
            ["getActiveDid(bytes32,bytes32,bytes32)", "paused()"]
        );
        assert_eq!(
            strs(&reg.metadata["abi"]["reads"]),
            ["getActiveDid", "paused"]
        );
        assert_eq!(
            strs(&abis(&x)[1].metadata["abi"]["events"]),
            ["RoleGranted(bytes32,address,uint8,uint64,bool,address)"]
        );
    }

    #[test]
    fn ts_combined_abi_requires_abi_assertion() {
        let src = r#"import { type Abi } from "abitype";
import LabNFT from "../assets/abis/LabNFT.json";
import OclTokenizer from "../assets/abis/OclTokenizer.json";
const OCL_COMBINED_ABI = [
  ...LabNFT,
  ...OclTokenizer,
] as Abi;
const merged = [...a, ...b];
const typed: Abi = [...LabNFT];
"#;
        let x = ts_file("lambda/common/utils/event.ts", src);
        let found = abis(&x);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["OCL_COMBINED_ABI", "typed"]);
        assert_eq!(found[0].metadata["abi"]["source"], "combined");
        assert_eq!(
            found[0].metadata["abi"]["combines"],
            json!([
                {"name": "LabNFT", "import": "../assets/abis/LabNFT.json"},
                {"name": "OclTokenizer", "import": "../assets/abis/OclTokenizer.json"}
            ])
        );
    }

    #[test]
    fn ts_combined_abi_takes_filtered_spreads() {
        // desci-infra lambda/common/utils/event.ts LOCKED_TOKEN_COMBINED_ABI.
        let src = r#"import LockedToken from "../assets/abis/LockedToken.json";
import LockedTokenFactory from "../assets/abis/LockedTokenFactory.json";
const LOCKED_TOKEN_COMBINED_ABI = [
  ...LockedTokenFactory,
  ...LockedToken.filter(
    (entry) => !(entry.type === "event" && entry.name === "UnlockDelayUpdated")
  ),
] as Abi;
const mapped = [...LockedToken.map((e) => e)] as Abi;
"#;
        let x = ts_file("lambda/common/utils/event.ts", src);
        let found = abis(&x);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].metadata["abi"]["combines"],
            json!([
                {"name": "LockedTokenFactory", "import": "../assets/abis/LockedTokenFactory.json"},
                {"name": "LockedToken", "import": "../assets/abis/LockedToken.json"}
            ])
        );
    }

    #[test]
    fn ts_non_abi_object_arrays_ignored() {
        let x = ts_file(
            "src/ui.ts",
            "const buttons = [{ type: 'button', name: 'ok' }];\nconst errs = [{ type: 'error', name: 'E' }];\nconst empty = [];\n",
        );
        assert!(abis(&x).is_empty(), "{:?}", abis(&x));
    }

    /// (member, kind, abi, via, scope) per call.
    type CallRow<'a> = (
        Option<&'a str>,
        ContractMemberKind,
        Option<&'a str>,
        Option<&'a str>,
        AbiScope,
    );

    fn calls(x: &FileExtraction) -> Vec<CallRow<'_>> {
        x.contract_calls
            .iter()
            .map(|c| {
                (
                    c.member.as_deref(),
                    c.kind,
                    c.abi.as_deref(),
                    c.via.as_deref(),
                    c.scope,
                )
            })
            .collect()
    }

    #[test]
    fn contract_calls_record_function_event_abi_and_via() {
        let src = r#"import { OclTokenizerAbi, AccessResolverAbi } from '@moleculexyz/common/abis'
export function useLabToken(labId: bigint) {
  const registryRead = useReadContract({
    abi: OclTokenizerAbi,
    functionName: 'tokenized',
    args: [labId],
  })
  const data = encodeFunctionData({ abi: AccessResolverAbi, functionName: 'grantRole', args })
  const logs = parseEventLogs({ abi: AccessResolverAbi, eventName: 'RoleGranted', logs: r.logs })
  const many = useReadContracts({ contracts: [{ abi: OclTokenizerAbi, functionName: 'controllerOf' }] })
  return registryRead
}
"#;
        let x = ts_file("apps/labs/src/use-lab-token.ts", src);
        use ContractMemberKind::*;
        assert_eq!(
            calls(&x),
            [
                (
                    Some("tokenized"),
                    Function,
                    Some("OclTokenizerAbi"),
                    Some("useReadContract"),
                    AbiScope::Object
                ),
                (
                    Some("grantRole"),
                    Function,
                    Some("AccessResolverAbi"),
                    Some("encodeFunctionData"),
                    AbiScope::Object
                ),
                (
                    Some("RoleGranted"),
                    Event,
                    Some("AccessResolverAbi"),
                    Some("parseEventLogs"),
                    AbiScope::Object
                ),
                (
                    Some("controllerOf"),
                    Function,
                    Some("OclTokenizerAbi"),
                    Some("useReadContracts"),
                    AbiScope::Object
                ),
            ]
        );
        let c = &x.contract_calls[0];
        assert_eq!(
            c.owner.as_deref(),
            Some("typescript:apps/labs/src/use-lab-token.ts:function:useLabToken")
        );
        assert_eq!(c.line, 5);
    }

    #[test]
    fn contract_calls_unwrap_as_and_member_paths() {
        let src = r#"const a = await client.readContract({ abi: SafeMultisig as Abi, functionName: 'getOwners' })
const b = await client.readContract({ abi: IPNFT_CONTRACT.abi, functionName: 'ownerOf' })
"#;
        let x = ts_file("src/x.ts", src);
        let abis: Vec<Option<&str>> = x.contract_calls.iter().map(|c| c.abi.as_deref()).collect();
        assert_eq!(abis, [Some("SafeMultisig"), Some("IPNFT_CONTRACT")]);
        assert!(x.contract_calls.iter().all(|c| c.owner.is_none()));
    }

    #[test]
    fn default_json_import_binds_abi_import() {
        let src = r#"import AccessResolver from "../assets/abis/AccessResolver.json";
import IPNFT from "../assets/abis/IPNFT.json";
export const decode = (data, topics) => decodeEventLog({ abi: IPNFT as Abi, data, topics });
export async function check(client) {
  return client.readContract({ abi: AccessResolver, functionName: "hasRole" });
}
"#;
        let x = ts_file("lambda/common/utils/event.ts", src);
        let got: Vec<(Option<&str>, Option<&str>, Option<&str>)> = x
            .contract_calls
            .iter()
            .map(|c| {
                (
                    c.member.as_deref(),
                    c.abi.as_deref(),
                    c.abi_import.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (None, Some("IPNFT"), Some("../assets/abis/IPNFT.json")),
                (
                    Some("hasRole"),
                    Some("AccessResolver"),
                    Some("../assets/abis/AccessResolver.json")
                ),
            ]
        );
        assert_eq!(x.contract_calls[0].kind, ContractMemberKind::Abi);
        assert_eq!(x.contract_calls[0].via.as_deref(), Some("decodeEventLog"));
    }

    #[test]
    fn local_alias_through_parse_abi() {
        let src = r#"const ACCESS_RESOLVER_ABI = ['function revokeRole(bytes32 oclId, address account) external'];
const changeRole = async ({ functionName, args }) => {
  const abi = parseAbi(ACCESS_RESOLVER_ABI);
  const txHash = await walletClient.writeContract({ address: ACCESS_RESOLVER, abi, functionName, args });
  await walletClient.writeContract({ abi, functionName: 'revokeRole', args });
};
"#;
        let x = ts_file("bruno/agent-connect/lib/agent-connect.js", src);
        use ContractMemberKind::*;
        assert_eq!(
            calls(&x),
            [
                (
                    None,
                    Abi,
                    Some("ACCESS_RESOLVER_ABI"),
                    Some("writeContract"),
                    AbiScope::Object
                ),
                (
                    Some("revokeRole"),
                    Function,
                    Some("ACCESS_RESOLVER_ABI"),
                    Some("writeContract"),
                    AbiScope::Object
                ),
            ]
        );
        // `const abi = parseAbi(X)` is an alias, never a second ABI.
        assert_eq!(abis(&x).len(), 1);
    }

    #[test]
    fn aliases_resolve_by_lexical_scope() {
        let src = r#"const abi = TopAbi;
export async function readA(client) {
  const abi = parseAbi(['function foo() view returns (uint256)']);
  return client.readContract({ abi, functionName: 'foo' });
}
export async function readB(client) {
  const abi = BAbi;
  if (client) {
    return client.readContract({ abi, functionName: 'bar' });
  }
}
export async function readC(client, { abi }) {
  return client.readContract({ abi, functionName: 'baz' });
}
export const readD = (client) => client.readContract({ abi, functionName: 'qux' });
export async function readE(client) {
  const local = abi;
  return client.readContract({ abi: local, functionName: 'quux' });
}
export function readF(client) {
  var abi = AAbi;
  var abi = CAbi;
  return client.readContract({ abi, functionName: 'corge' });
}
"#;
        let x = ts_file("src/reads.ts", src);
        let got: Vec<(Option<&str>, Option<&str>)> = x
            .contract_calls
            .iter()
            .map(|c| (c.member.as_deref(), c.abi.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                // readA's own inline ABI, not readB's alias.
                (Some("foo"), Some("abi")),
                (Some("bar"), Some("BAbi")),
                // A destructured parameter shadows the top-level alias.
                (Some("baz"), Some("abi")),
                (Some("qux"), Some("TopAbi")),
                (Some("quux"), Some("TopAbi")),
                // Declared twice in one scope: ambiguous, left as written.
                (Some("corge"), Some("abi")),
            ]
        );
    }

    #[test]
    fn contract_calls_fall_back_to_the_single_abi_in_the_symbol() {
        let src = r#"export function useGrantRole() {
  const { execute } = useAdaptiveTransaction({ abi: AccessResolverAbi, operation: 'inviteMember' })
  const grantRole = (call) => execute({ address, args: [1], functionName: 'grantRole' })
  return { grantRole }
}
export function useTwo() {
  useAdaptiveTransaction({ abi: AAbi })
  useAdaptiveTransaction({ abi: BAbi })
  run({ functionName: 'x' })
}
"#;
        let x = ts_file("apps/labs/src/hooks/use-grant-role.ts", src);
        use ContractMemberKind::*;
        let got = calls(&x);
        assert!(got.contains(&(
            None,
            Abi,
            Some("AccessResolverAbi"),
            Some("useAdaptiveTransaction"),
            AbiScope::Object
        )));
        // The arrow `grantRole` names no ABI itself: the enclosing hook does.
        assert!(
            got.contains(&(
                Some("grantRole"),
                Function,
                Some("AccessResolverAbi"),
                Some("execute"),
                AbiScope::Symbol
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&(Some("x"), Function, None, Some("run"), AbiScope::Object)),
            "{got:?}"
        );
    }

    #[test]
    fn contract_calls_symbol_scope_inside_one_function() {
        let src = r#"export function useGrantRole() {
  const { execute } = useAdaptiveTransaction({ abi: AccessResolverAbi })
  function grant() {}
  execute({ functionName: 'grantRole' })
}
"#;
        let x = ts_file("apps/labs/src/hooks/use-grant.ts", src);
        let c = x
            .contract_calls
            .iter()
            .find(|c| c.member.as_deref() == Some("grantRole"))
            .unwrap();
        assert_eq!(c.abi.as_deref(), Some("AccessResolverAbi"));
        assert_eq!(c.scope, AbiScope::Symbol);
    }

    #[test]
    fn dynamic_function_name_is_abi_use_only() {
        let src = "export function run(call) {\n  return writeContract({ abi: OclTokenizerAbi, functionName: call.functionName, args: call.args })\n}\n";
        let x = ts_file("src/run.ts", src);
        use ContractMemberKind::*;
        assert_eq!(
            calls(&x),
            [(
                None,
                Abi,
                Some("OclTokenizerAbi"),
                Some("writeContract"),
                AbiScope::Object
            )]
        );
    }

    #[test]
    fn non_contract_objects_are_not_calls() {
        // desci-infra bio-agent-execution-processor and condition-evaluator shapes.
        let src = r#"export function updateList(results) {
  results.push({ status: "processed", eventName: "NewPersona" });
}
export async function evaluate(client, condition) {
  const abi = [condition.functionAbi] as const;
  logger.info("evaluating", { functionName: condition.functionName });
  return client.readContract({ address, abi, functionName: condition.functionName });
}
export function readName(client) {
  return client.readContract({ ...erc20, functionName: "name" });
}
"#;
        let x = ts_file("lambda/x/index.ts", src);
        use ContractMemberKind::*;
        assert_eq!(
            calls(&x),
            [
                (
                    None,
                    Abi,
                    Some("abi"),
                    Some("readContract"),
                    AbiScope::Object
                ),
                (
                    Some("name"),
                    Function,
                    None,
                    Some("readContract"),
                    AbiScope::Object
                ),
            ]
        );
    }

    #[test]
    fn type_annotation_function_name_is_ignored() {
        let src = "interface Call { functionName: string; abi: Abi }\ntype T = { eventName: 'X' };\nconst log = { functionName: 'handler' };\n";
        let x = ts_file("src/types.ts", src);
        assert!(x.contract_calls.is_empty(), "{:?}", x.contract_calls);
    }
}

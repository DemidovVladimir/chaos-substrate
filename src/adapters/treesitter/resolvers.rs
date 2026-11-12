//! GraphQL resolver `ExtraPass`es for TS/JS: where a GraphQL field is wired
//! to code. tree-sitter node-shape walks only (never regex); names are exact.
//!
//! | Pass | Found | Output |
//! |---|---|---|
//! | [`dispatch_cases`] | `switch (fieldName)` / `switch (x.fieldName)` (any receiver, e.g. `event.fieldName`, `ctx.info.fieldName`): each `case "<field>":` group (fall-through labels share one) | one `Resolver` symbol per group, `Resolves` relations |
//! | [`bindings`] | object literals whose `typeName` and `fieldName` are both literal strings (or same-file `const`s): AppSync `getResolvers()` entries, `createResolver(id, {typeName, fieldName})` | `FileExtraction.graphql_bindings`, `Binds` relations |
//! | [`bindings`] | `entry:` properties of a Lambda construct: a literal (repo-root relative, as the CDK app runs from the repo root) or `path.join(__dirname, "…")` | `FileExtraction.lambda_entries` |
//!
//! A `Resolver` is `typescript:{path}:resolver:{host qualified}::{first label}`
//! (e.g. `typescript:lambda/x/index.ts:resolver:originalHandler::createLab`),
//! parented to the enclosing symbol, with `metadata = {fields, parent_types,
//! dispatch}`. `parent_types` are the string literals the case body compares
//! (`===`, `!==`, `==`, `!=`) against `parentTypeName` / `x.parentTypeName`.
//! Each label yields a `Resolves` relation to `Type.label` per parent type,
//! plus one to the bare label when the case is unguarded or its guards leave
//! other parent types to the rest of the case ([`guards_cover`]: an
//! `if (p === "Lab") {…}` branch followed by the root-field path). The
//! analyzer resolves a bare label against the coordinates the case's guards
//! did not name: by bindings, then by root uniqueness. Non-literal
//! registrations (`{ typeName: r.typeName, … }`) and
//! dynamic `entry:` values are skipped: never guessed.

use super::entry::{ts_consts, ts_prop, ts_string};
use super::surface::string_value;
use super::typescript::{leading_comments, unique_id};
use super::walker::{enclosing_symbol, file_id, text};
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput, GraphqlBinding, Relation, Symbol};
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet};
use tree_sitter::{Node, Tree};

const FIELD_NAME: &str = "fieldName";
const PARENT_TYPE_NAME: &str = "parentTypeName";
/// Handler files a Lambda `entry:` may name.
const ENTRY_EXTS: &[&str] = &[".ts", ".tsx", ".js", ".mjs", ".cjs", ".py"];

/// A GraphQL name: `[_A-Za-z][_0-9A-Za-z]*`.
pub fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Pre-order, source-ordered traversal of named nodes (the nodes outlive the
/// callback, so a pass can collect them).
fn walk<'t>(root: Node<'t>, mut f: impl FnMut(Node<'t>)) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

fn named_kids(n: Node) -> Vec<Node> {
    let mut cursor = n.walk();
    let kids: Vec<Node> = n
        .named_children(&mut cursor)
        .filter(|k| k.kind() != "comment")
        .collect();
    kids
}

/// `(x)`, `x!`, `x as T`, `x satisfies T` → `x`.
fn unwrap_expr(mut n: Node) -> Node {
    while matches!(
        n.kind(),
        "parenthesized_expression"
            | "non_null_expression"
            | "as_expression"
            | "satisfies_expression"
    ) {
        match named_kids(n).first() {
            Some(inner) => n = *inner,
            None => break,
        }
    }
    n
}

/// `prop` itself or any `x.prop` / `x?.prop` member access.
fn names_prop(n: Node, src: &str, prop: &str) -> bool {
    let n = unwrap_expr(n);
    match n.kind() {
        "identifier" => text(n, src) == prop,
        "member_expression" => n
            .child_by_field_name("property")
            .is_some_and(|p| text(p, src) == prop),
        _ => false,
    }
}

/// The statements of a `switch_case` (comments are extras, never `body`).
fn case_body(case: Node) -> Vec<Node> {
    let mut cursor = case.walk();
    let body: Vec<Node> = case
        .children_by_field_name("body", &mut cursor)
        .filter(|s| s.kind() != "comment")
        .collect();
    body
}

/// The doc of a case group: comments directly above its first label, whether
/// the grammar placed them between the cases or at the tail of the case before.
fn case_doc(first: Node, src: &str) -> Option<String> {
    if let (Some(doc), _) = leading_comments(first, src) {
        return Some(doc);
    }
    let prev = first.prev_named_sibling()?;
    let mut cursor = prev.walk();
    let kids: Vec<Node> = prev.named_children(&mut cursor).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut expected_row = first.start_position().row;
    for k in kids.iter().rev() {
        if k.kind() != "comment" || k.end_position().row + 1 < expected_row {
            break;
        }
        let cleaned: Vec<String> = text(*k, src)
            .lines()
            .rev()
            .map(|l| {
                let t = l.trim();
                let t = ["/**", "/*", "*/", "//", "*"]
                    .iter()
                    .find_map(|p| t.strip_prefix(p))
                    .unwrap_or(t);
                t.trim_end_matches("*/").trim().to_string()
            })
            .collect();
        lines.extend(cleaned);
        expected_row = k.start_position().row;
    }
    lines.reverse();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then(|| doc.chars().take(1000).collect())
}

/// String literals compared (`===`, `!==`, `==`, `!=`) against
/// `parentTypeName` / `x.parentTypeName` anywhere under `n`.
fn parent_types(n: Node, src: &str, out: &mut BTreeSet<String>) {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if x.kind() == "binary_expression" {
            let op = x.child_by_field_name("operator").map(|o| text(o, src));
            if matches!(op, Some("===" | "!==" | "==" | "!=")) {
                if let (Some(l), Some(r)) = (
                    x.child_by_field_name("left"),
                    x.child_by_field_name("right"),
                ) {
                    for (a, b) in [(l, r), (r, l)] {
                        if names_prop(a, src, PARENT_TYPE_NAME) {
                            if let Some(t) =
                                string_value(unwrap_expr(b), src).filter(|t| is_graphql_name(t))
                            {
                                out.insert(t);
                            }
                        }
                    }
                }
            }
        }
        let mut cursor = x.walk();
        stack.extend(x.named_children(&mut cursor));
    }
}

/// How a condition tests `parentTypeName`: `p === "A"` / `p == "A"` and
/// `||` of those admit the named types; `p !== "A"` / `p != "A"` and `&&` of
/// those rule them out; `!(…)` flips. Anything else is not a guard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    Admits,
    Excludes,
}

fn guard(n: Node, src: &str) -> Option<Guard> {
    let n = unwrap_expr(n);
    match n.kind() {
        "unary_expression" => {
            if text(n.child_by_field_name("operator")?, src) != "!" {
                return None;
            }
            match guard(n.child_by_field_name("argument")?, src)? {
                Guard::Admits => Some(Guard::Excludes),
                Guard::Excludes => Some(Guard::Admits),
            }
        }
        "binary_expression" => {
            let op = text(n.child_by_field_name("operator")?, src);
            let (l, r) = (
                n.child_by_field_name("left")?,
                n.child_by_field_name("right")?,
            );
            match op {
                "===" | "==" | "!==" | "!=" => {
                    let compares = [(l, r), (r, l)].into_iter().any(|(a, b)| {
                        names_prop(a, src, PARENT_TYPE_NAME)
                            && string_value(unwrap_expr(b), src).is_some()
                    });
                    compares.then_some(if op.starts_with('!') {
                        Guard::Excludes
                    } else {
                        Guard::Admits
                    })
                }
                "||" => (guard(l, src)? == Guard::Admits && guard(r, src)? == Guard::Admits)
                    .then_some(Guard::Admits),
                "&&" => (guard(l, src)? == Guard::Excludes && guard(r, src)? == Guard::Excludes)
                    .then_some(Guard::Excludes),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `break` / `return` / `throw` / `continue`, or a block ending in one.
fn exits(n: Node) -> bool {
    match n.kind() {
        "break_statement" | "return_statement" | "throw_statement" | "continue_statement" => true,
        "statement_block" => named_kids(n).last().is_some_and(|s| exits(*s)),
        _ => false,
    }
}

/// Whether `n` returns from the dispatcher (a `return` outside nested functions).
fn returns(n: Node) -> bool {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        match x.kind() {
            "return_statement" => return true,
            "arrow_function"
            | "function_expression"
            | "function_declaration"
            | "generator_function"
            | "generator_function_declaration"
            | "method_definition"
            | "class_declaration"
            | "class" => continue,
            _ => stack.extend(named_kids(x)),
        }
    }
    false
}

/// What a top-level `if` of a case does with parent types.
enum IfGuard {
    /// `if (p !== "A" && p !== "B") break;`: the rest of the case serves A and B only.
    Narrows,
    /// `if (p === "A" || p === "B") {…}`, `else if` chains of such included;
    /// `closed` when a final `else` only exits (`break` / `throw`).
    Admits { closed: bool },
    /// An `else` that serves the types the chain did not name.
    Serves,
}

fn if_guard(s: Node, src: &str) -> Option<IfGuard> {
    if s.kind() != "if_statement" {
        return None;
    }
    let cond = |n: Node| {
        n.child_by_field_name("condition")
            .and_then(|c| guard(c, src))
    };
    if cond(s)? == Guard::Excludes {
        return s
            .child_by_field_name("consequence")
            .filter(|c| exits(*c))
            .map(|_| IfGuard::Narrows);
    }
    let mut cur = s;
    loop {
        let Some(stmt) = cur
            .child_by_field_name("alternative")
            .and_then(|alt| named_kids(alt).first().copied())
        else {
            return Some(IfGuard::Admits { closed: false });
        };
        if stmt.kind() == "if_statement" && cond(stmt) == Some(Guard::Admits) {
            cur = stmt;
            continue;
        }
        return Some(if exits(stmt) && !returns(stmt) {
            IfGuard::Admits { closed: true }
        } else {
            IfGuard::Serves
        });
    }
}

/// Whether a case's `parentTypeName` guards cover every parent type it
/// serves, read off its top-level statements. A guard that rules types out
/// and exits (`if (p !== "Lab" && p !== "LabRef") break;`) narrows the rest of
/// the case to the named types. Guards that admit types (`if (p === "Lab" ||
/// p === "LabRef") {…}`) serve only their branch: the code after them serves
/// the other parent types too (typically the root field) unless it reaches
/// `break` / `throw` before any `return`, or a final `else` only exits.
fn guards_cover(stmts: &[Node], src: &str) -> bool {
    let mut admitted = false;
    for s in stmts {
        match if_guard(*s, src) {
            Some(IfGuard::Narrows) => return true,
            Some(IfGuard::Serves) => return false,
            Some(IfGuard::Admits { closed }) => {
                if closed {
                    return true;
                }
                admitted = true;
            }
            None if admitted => {
                if returns(*s) {
                    return false;
                }
                if matches!(s.kind(), "break_statement" | "throw_statement") {
                    return true;
                }
            }
            None => {}
        }
    }
    admitted
}

/// One `case` group about to become a `Resolver`.
struct Group<'t> {
    labels: Vec<String>,
    first: Node<'t>,
    last: Node<'t>,
    dispatch: String,
}

/// `switch (fieldName)` dispatcher cases → `Resolver` symbols (see module docs).
/// Runs before the call pass, so calls inside a case belong to the case.
pub fn dispatch_cases(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut groups: Vec<Group> = Vec::new();
    walk(tree.root_node(), |n| {
        if n.kind() != "switch_statement" {
            return;
        }
        let Some(value) = n.child_by_field_name("value") else {
            return;
        };
        let disc = unwrap_expr(value);
        if !names_prop(disc, src, FIELD_NAME) {
            return;
        }
        let Some(body) = n.child_by_field_name("body") else {
            return;
        };
        let dispatch = text(disc, src).split_whitespace().collect::<String>();
        let mut pending: Vec<(String, Node)> = Vec::new();
        for case in named_kids(body) {
            if case.kind() != "switch_case" {
                // `default:` (or anything else) ends a fall-through run.
                pending.clear();
                continue;
            }
            let label = case
                .child_by_field_name("value")
                .and_then(|v| string_value(unwrap_expr(v), src))
                .filter(|l| is_graphql_name(l));
            let Some(label) = label else {
                pending.clear();
                continue;
            };
            pending.push((label, case));
            if case_body(case).is_empty() {
                continue; // falls through to the next label
            }
            groups.push(Group {
                labels: pending.iter().map(|(l, _)| l.clone()).collect(),
                first: pending[0].1,
                last: case,
                dispatch: dispatch.clone(),
            });
            pending.clear();
        }
    });
    if groups.is_empty() {
        return;
    }
    let hosts = out.symbols.clone();
    let mut made: Vec<Symbol> = Vec::new();
    for g in groups {
        let start = g.first.start_byte();
        // A trailing comment of the last case documents the NEXT case.
        let end_node = named_kids(g.last).last().copied().unwrap_or(g.last);
        let parent_id = enclosing_symbol(&hosts, &file.path, start);
        let parent = hosts
            .iter()
            .find(|s| s.stable_id == parent_id)
            .map(|s| (s.stable_id.clone(), s.qualified_name.clone()));
        let label = g.labels[0].clone();
        let qualified = match &parent {
            Some((_, q)) => format!("{q}::{label}"),
            None => label.clone(),
        };
        let stable_id = unique_id(
            &made,
            format!(
                "{}:{}:{}:{qualified}",
                file.language.as_str(),
                file.path,
                NodeKind::Resolver.as_str()
            ),
        );
        let mut types = BTreeSet::new();
        parent_types(g.last, src, &mut types);
        let body = case_body(g.last);
        let segments: Vec<(usize, usize)> = match body.as_slice() {
            [block] if block.kind() == "statement_block" => {
                let mut cursor = block.walk();
                block
                    .named_children(&mut cursor)
                    .map(|c| (c.start_byte(), c.end_byte()))
                    .collect()
            }
            stmts => stmts
                .iter()
                .map(|s| (s.start_byte(), s.end_byte()))
                .collect(),
        };
        let stmts: Vec<Node> = match body.as_slice() {
            [block] if block.kind() == "statement_block" => named_kids(*block),
            stmts => stmts.to_vec(),
        };
        // Unguarded, or guards that leave other parent types to the rest of
        // the case: the bare label too (the analyzer resolves it against the
        // coordinates the guards did not name).
        let bare = types.is_empty() || !guards_cover(&stmts, src);
        for l in &g.labels {
            if bare {
                out.relations.push(Relation {
                    source: stable_id.clone(),
                    kind: EdgeKind::Resolves,
                    target_name: l.clone(),
                });
            }
            for t in &types {
                out.relations.push(Relation {
                    source: stable_id.clone(),
                    kind: EdgeKind::Resolves,
                    target_name: format!("{t}.{l}"),
                });
            }
        }
        made.push(Symbol {
            stable_id,
            kind: NodeKind::Resolver,
            name: label,
            qualified_name: qualified,
            parent: parent.map(|(id, _)| id),
            signature: Some(
                g.labels
                    .iter()
                    .map(|l| format!("case \"{l}\":"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            doc: case_doc(g.first, src),
            line_start: g.first.start_position().row as u32 + 1,
            line_end: end_node.end_position().row as u32 + 1,
            byte_start: start,
            byte_end: end_node.end_byte().max(start),
            segments,
            metadata: json!({
                "fields": g.labels,
                "parent_types": types,
                "dispatch": g.dispatch,
            }),
        });
    }
    out.symbols.extend(made);
    out.symbols.sort_by_key(|s| s.byte_start);
}

/// `rel` joined onto the repo-relative directory `base`, `.`/`..` collapsed;
/// `None` when it is absolute, escapes the repository or is not a handler file.
fn repo_path(base: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    let p = parts.join("/");
    ENTRY_EXTS.iter().any(|e| p.ends_with(e)).then_some(p)
}

/// The handler file an `entry:` value names (see module docs).
fn entry_path(v: Node, src: &str, path: &str, consts: &HashMap<String, String>) -> Option<String> {
    let v = unwrap_expr(v);
    if let Some(s) = ts_string(v, src, consts) {
        return repo_path("", &s);
    }
    if v.kind() != "call_expression" {
        return None;
    }
    let f = v.child_by_field_name("function")?;
    let method = match f.kind() {
        "identifier" => text(f, src),
        "member_expression" => {
            let obj = f.child_by_field_name("object")?;
            if obj.kind() != "identifier" || text(obj, src) != "path" {
                return None;
            }
            text(f.child_by_field_name("property")?, src)
        }
        _ => return None,
    };
    if !matches!(method, "join" | "resolve") {
        return None;
    }
    let args = named_kids(v.child_by_field_name("arguments")?);
    let (first, rest) = args.split_first()?;
    if first.kind() != "identifier" || text(*first, src) != "__dirname" || rest.is_empty() {
        return None;
    }
    let tail: Option<Vec<String>> = rest.iter().map(|a| ts_string(*a, src, consts)).collect();
    let dir = path.rfind('/').map(|i| &path[..i]).unwrap_or("");
    repo_path(dir, &tail?.join("/"))
}

/// AppSync resolver registrations and Lambda `entry:` props (see module docs).
pub fn bindings(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let consts = ts_consts(tree.root_node(), src);
    let mut found: Vec<GraphqlBinding> = Vec::new();
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    let mut entries: Vec<String> = Vec::new();
    let symbols = &out.symbols;
    walk(tree.root_node(), |n| match n.kind() {
        "object" => {
            let (Some(t), Some(f)) = (
                ts_prop(n, "typeName", src).and_then(|v| ts_string(v, src, &consts)),
                ts_prop(n, "fieldName", src).and_then(|v| ts_string(v, src, &consts)),
            ) else {
                return;
            };
            if !is_graphql_name(&t) || !is_graphql_name(&f) {
                return;
            }
            let coordinate = format!("{t}.{f}");
            let id = enclosing_symbol(symbols, &file.path, n.start_byte());
            let owner = (id != file_id(&file.path)).then_some(id);
            if seen.insert((coordinate.clone(), owner.clone())) {
                found.push(GraphqlBinding {
                    coordinate,
                    line: n.start_position().row as u32 + 1,
                    owner,
                });
            }
        }
        "pair" => {
            let is_entry = n.child_by_field_name("key").is_some_and(|k| {
                text(k, src) == "entry" || string_value(k, src).as_deref() == Some("entry")
            });
            if !is_entry {
                return;
            }
            if let Some(p) = n
                .child_by_field_name("value")
                .and_then(|v| entry_path(v, src, &file.path, &consts))
            {
                if !entries.contains(&p) {
                    entries.push(p);
                }
            }
        }
        _ => {}
    });
    for b in &found {
        out.relations.push(Relation {
            source: b.owner.clone().unwrap_or_else(|| file_id(&file.path)),
            kind: EdgeKind::Binds,
            target_name: b.coordinate.clone(),
        });
    }
    out.graphql_bindings.extend(found);
    for e in entries {
        if !out.lambda_entries.contains(&e) {
            out.lambda_entries.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::treesitter::typescript::spec;
    use crate::adapters::treesitter::walker::extract;
    use crate::domain::model::Language;

    fn ts(path: &str, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: Language::TypeScript,
            content: src.into(),
        };
        extract(spec(Language::TypeScript, path).unwrap(), &file)
    }

    fn resolvers(x: &FileExtraction) -> Vec<&Symbol> {
        x.symbols
            .iter()
            .filter(|s| s.kind == NodeKind::Resolver)
            .collect()
    }

    fn resolves(x: &FileExtraction) -> Vec<(&str, &str)> {
        x.relations
            .iter()
            .filter(|r| r.kind == EdgeKind::Resolves)
            .map(|r| (r.source.as_str(), r.target_name.as_str()))
            .collect()
    }

    /// The labs Lambda shape (desci-infra lambda/appsync-resolver-labs-lambda/index.ts).
    const LABS: &str = r#"import { validateOclId } from "./validate";

export const originalHandler = async (event: AppSyncEvent, deps: Deps) => {
  const fieldName = event.fieldName;
  const args = event.arguments;
  deps.logger.info("dispatch", { fieldName });
  switch (fieldName) {
    // IP-2213 — on-chain activity feed.
    case "onChainActivity": {
      return await handleOnChainActivity(args);
    }

    case "createLab": {
      const validatedOclId = validateOclId(args.oclId);
      const adminAddress = args.adminAddress;
      return await deps.dataRoomResolvers.handleCreateLab(
        validatedOclId,
        adminAddress
      );
    }
    // Field resolver: LabRef nodes also appear nested under searchLabs.
    case "members": {
      if (
        event.parentTypeName !== "Lab" &&
        event.parentTypeName !== "LabRef"
      ) {
        break;
      }
      return await listMembers(event.source);
    }

    default:
      throw new Error(`Unknown field: ${fieldName}`);
  }
};
"#;

    #[test]
    fn fieldname_switch_cases_become_resolver_symbols() {
        let x = ts("lambda/x/index.ts", LABS);
        let rs = resolvers(&x);
        let ids: Vec<&str> = rs.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "typescript:lambda/x/index.ts:resolver:originalHandler::onChainActivity",
                "typescript:lambda/x/index.ts:resolver:originalHandler::createLab",
                "typescript:lambda/x/index.ts:resolver:originalHandler::members",
            ]
        );
        let create = rs[1];
        assert_eq!(create.name, "createLab");
        assert_eq!(create.qualified_name, "originalHandler::createLab");
        assert_eq!(
            create.parent.as_deref(),
            Some("typescript:lambda/x/index.ts:function:originalHandler")
        );
        assert_eq!((create.line_start, create.line_end), (13, 20));
        assert_eq!(create.signature.as_deref(), Some("case \"createLab\":"));
        assert_eq!(create.segments.len(), 3);
        assert_eq!(create.metadata["fields"], json!(["createLab"]));
        assert_eq!(create.metadata["parent_types"], json!([]));
        assert_eq!(create.metadata["dispatch"], "fieldName");
        assert!(LABS[create.byte_start..create.byte_end].starts_with("case \"createLab\": {"));
        assert!(LABS[create.byte_start..create.byte_end].ends_with('}'));
        assert_eq!(
            rs[0].doc.as_deref(),
            Some("IP-2213 — on-chain activity feed.")
        );
        // A comment between two cases documents the next one, never the one before.
        assert_eq!(
            rs[2].doc.as_deref(),
            Some("Field resolver: LabRef nodes also appear nested under searchLabs.")
        );
        assert!(create.doc.is_none());
        // `default:` is not a resolver; resolvers are units with chunks.
        assert!(!rs.iter().any(|s| s.name == "default"));
        assert!(NodeKind::Resolver.is_unit());
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
    }

    #[test]
    fn calls_inside_a_case_belong_to_the_case() {
        let x = ts("lambda/x/index.ts", LABS);
        let caller = |callee: &str| {
            x.calls
                .iter()
                .find(|c| c.callee == callee)
                .unwrap_or_else(|| panic!("no call to {callee}: {:?}", x.calls))
                .caller
                .as_str()
        };
        assert_eq!(
            caller("handleCreateLab"),
            "typescript:lambda/x/index.ts:resolver:originalHandler::createLab"
        );
        assert_eq!(
            caller("validateOclId"),
            "typescript:lambda/x/index.ts:resolver:originalHandler::createLab"
        );
        // A call before the switch stays on the host function.
        assert_eq!(
            caller("info"),
            "typescript:lambda/x/index.ts:function:originalHandler"
        );
    }

    #[test]
    fn fall_through_labels_share_one_resolver() {
        let src = "export async function handler(event) {\n  switch (event.fieldName) {\n    case \"linkToken\":\n    case \"unlinkToken\": {\n      if (event.fieldName === \"linkToken\") return link(event);\n      return unlink(event);\n    }\n  }\n}\n";
        let x = ts("lambda/tokens/index.ts", src);
        let rs = resolvers(&x);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].name, "linkToken");
        assert_eq!(
            rs[0].metadata["fields"],
            json!(["linkToken", "unlinkToken"])
        );
        assert_eq!(rs[0].metadata["dispatch"], "event.fieldName");
        assert_eq!(
            rs[0].signature.as_deref(),
            Some("case \"linkToken\": case \"unlinkToken\":")
        );
        assert_eq!((rs[0].line_start, rs[0].line_end), (3, 7));
        let id = "typescript:lambda/tokens/index.ts:resolver:handler::linkToken";
        assert_eq!(resolves(&x), [(id, "linkToken"), (id, "unlinkToken")]);
    }

    #[test]
    fn parent_type_guards_qualify_the_target() {
        let x = ts("lambda/x/index.ts", LABS);
        let id = "typescript:lambda/x/index.ts:resolver:originalHandler::members";
        let members: Vec<&str> = resolves(&x)
            .into_iter()
            .filter(|(s, _)| *s == id)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(members, ["Lab.members", "LabRef.members"]);
        let src = "const fieldName = event.fieldName;\nconst parentTypeName = event.parentTypeName;\nswitch (fieldName) {\n  case \"tokens\": {\n    if (parentTypeName === \"Query\") { return all(); }\n    if (parentTypeName === \"Lab\") { return forLab(); }\n    break;\n  }\n}\n";
        let x = ts("lambda/tokens/index.ts", src);
        let rs = resolvers(&x);
        assert_eq!(rs.len(), 1);
        // A top-level switch: no host symbol, so the qualified name is the label.
        assert_eq!(
            rs[0].stable_id,
            "typescript:lambda/tokens/index.ts:resolver:tokens"
        );
        assert!(rs[0].parent.is_none());
        assert_eq!(rs[0].metadata["parent_types"], json!(["Lab", "Query"]));
        let targets: Vec<&str> = resolves(&x).into_iter().map(|(_, t)| t).collect();
        assert_eq!(targets, ["Lab.tokens", "Query.tokens"]);
    }

    /// desci-infra lambda/appsync-resolver-labs-lambda/index.ts
    /// `case "legalAgreementStatus"`: an admitting guard for the field
    /// resolvers, then the `Query` path below it.
    #[test]
    fn admitting_guard_then_root_path_keeps_the_bare_label() {
        let src = r#"export const originalHandler = async (event, deps) => {
  const args = event.arguments;
  switch (event.fieldName) {
    case "legalAgreementStatus": {
      // Field resolver on Lab / LabRef.
      if (
        event.parentTypeName === "Lab" ||
        event.parentTypeName === "LabRef"
      ) {
        const seeded = event.source?.seeded;
        if (seeded) {
          return seeded;
        }
        return await deps.legal.handleLegalAgreementStatus(event.source.oclId, args.type);
      }

      // Query path.
      const oclId = args.oclId;
      if (!oclId || !args.type) {
        throw new Error("Missing required parameters");
      }
      return await deps.legal.handleLegalAgreementStatus(oclId, args.type);
    }
  }
};
"#;
        let x = ts("lambda/x/index.ts", src);
        let id = "typescript:lambda/x/index.ts:resolver:originalHandler::legalAgreementStatus";
        assert_eq!(
            resolves(&x),
            [
                (id, "legalAgreementStatus"),
                (id, "Lab.legalAgreementStatus"),
                (id, "LabRef.legalAgreementStatus"),
            ]
        );
        assert_eq!(
            resolvers(&x)[0].metadata["parent_types"],
            json!(["Lab", "LabRef"])
        );
    }

    #[test]
    fn guards_cover_reads_the_case_shape() {
        let targets = |body: &str| -> Vec<String> {
            let src = format!(
                "function h(event, p) {{\n  switch (event.fieldName) {{\n    case \"f\": {{\n{body}\n    }}\n  }}\n}}\n"
            );
            let x = ts("lambda/x/index.ts", &src);
            resolves(&x)
                .into_iter()
                .map(|(_, t)| t.to_string())
                .collect()
        };
        let p = "event.parentTypeName";
        // Ruled out and exited: the rest serves Lab only.
        assert_eq!(
            targets(&format!("if ({p} !== \"Lab\") break;\nreturn a();")),
            ["Lab.f"]
        );
        assert_eq!(
            targets(&format!(
                "if (!({p} === \"Lab\" || {p} === \"LabRef\")) {{ throw e; }}\nreturn a();"
            )),
            ["Lab.f", "LabRef.f"]
        );
        // Admitted, then only `break` / `throw` for the rest.
        assert_eq!(
            targets(&format!(
                "if ({p} === \"Query\") {{ return a(); }}\nlog(\"x\");\nbreak;"
            )),
            ["Query.f"]
        );
        assert_eq!(
            targets(&format!("if ({p} === \"Query\") {{ return a(); }} else if ({p} == \"Lab\") {{ return b(); }} else {{ throw e; }}")),
            ["Lab.f", "Query.f"]
        );
        // Admitted, then a path for the others.
        assert_eq!(
            targets(&format!(
                "if ({p} === \"Lab\") {{ return a(); }} else {{ return b(); }}"
            )),
            ["f", "Lab.f"]
        );
        assert_eq!(
            targets(&format!(
                "if ({p} === \"Lab\") return a();\nif (x) {{ return b(); }}\nbreak;"
            )),
            ["f", "Lab.f"]
        );
        // A guard that does not exit narrows nothing; a nested guard is not read.
        assert_eq!(
            targets(&format!("if ({p} !== \"Lab\") {{ warn(); }}\nreturn a();")),
            ["f", "Lab.f"]
        );
        assert_eq!(
            targets(&format!(
                "try {{ if ({p} !== \"Lab\") break; }} catch (e) {{}}\nreturn a();"
            )),
            ["f", "Lab.f"]
        );
        // Mixed conditions are not guards.
        assert_eq!(
            targets(&format!(
                "if ({p} !== \"Lab\" && ready) break;\nreturn a();"
            )),
            ["f", "Lab.f"]
        );
        // A `return` inside a callback is not the dispatcher's.
        assert_eq!(
            targets(&format!("if ({p} === \"Lab\") return a();\nthrow new Error(xs.map((x) => {{ return x; }}).join());")),
            ["Lab.f"]
        );
    }

    #[test]
    fn other_switches_are_not_dispatchers() {
        for disc in ["kind", "event.type", "x.fieldNames", "getFieldName()"] {
            let src = format!(
                "function f(event, x, kind) {{\n  switch ({disc}) {{\n    case \"createLab\": {{ return 1; }}\n  }}\n}}\n"
            );
            assert!(resolvers(&ts("src/a.ts", &src)).is_empty(), "{disc}");
        }
        let src = "function f(event) {\n  switch (event.info.fieldName) {\n    case \"lab\": { return 1; }\n    case 42: { return 2; }\n    case \"not-a-field\": { return 3; }\n  }\n}\n";
        let rs = resolvers(&ts("src/a.ts", src)).len();
        assert_eq!(rs, 1);
    }

    /// The AppSync config shape (desci-infra lib/desci-api-app-sync/constructs/
    /// appsync-labs-lambda-config.ts).
    const CONFIG: &str = r#"import * as path from "path";

export const EXTRA: readonly { typeName: string; fieldName: string }[] = [
  { typeName: "Mutation", fieldName: "issueConsumerCredential" },
];

export class Config {
  createLambda() {
    return createLambdaTemplate(this, "Labs", {
      entry: "lambda/appsync-resolver-labs-lambda/index.ts",
      handler: "handler",
    });
  }

  getResolvers() {
    const out = [];
    for (const field of schema.fields) {
      out.push({ typeName: "Query", fieldName: field.name.value });
    }
    return [
      { typeName: "Query", fieldName: "labs" },
      {
        typeName: "Mutation",
        fieldName: "createLab",
        caching: { ttlSeconds: 60 },
      },
      { typeName: "Lab", fieldName: "members", sourcePassthrough: true },
      ...EXTRA,
    ];
  }
}
"#;

    #[test]
    fn appsync_bindings_from_object_literals() {
        let x = ts("lib/c.ts", CONFIG);
        let got: Vec<(&str, u32, Option<&str>)> = x
            .graphql_bindings
            .iter()
            .map(|b| (b.coordinate.as_str(), b.line, b.owner.as_deref()))
            .collect();
        let owner = Some("typescript:lib/c.ts:method:Config::getResolvers");
        assert_eq!(
            got,
            [
                ("Mutation.issueConsumerCredential", 4, None),
                ("Query.labs", 21, owner),
                ("Mutation.createLab", 22, owner),
                ("Lab.members", 27, owner),
            ]
        );
        let binds: Vec<(&str, &str)> = x
            .relations
            .iter()
            .filter(|r| r.kind == EdgeKind::Binds)
            .map(|r| (r.source.as_str(), r.target_name.as_str()))
            .collect();
        assert_eq!(
            binds,
            [
                ("file:lib/c.ts", "Mutation.issueConsumerCredential"),
                (
                    "typescript:lib/c.ts:method:Config::getResolvers",
                    "Query.labs"
                ),
                (
                    "typescript:lib/c.ts:method:Config::getResolvers",
                    "Mutation.createLab"
                ),
                (
                    "typescript:lib/c.ts:method:Config::getResolvers",
                    "Lab.members"
                ),
            ]
        );
    }

    #[test]
    fn binding_strings_resolve_same_file_consts() {
        let x = ts(
            "lib/c.ts",
            "const ROOT = \"Mutation\";\nexport const R = [{ typeName: ROOT, fieldName: `x` }];\n",
        );
        assert_eq!(x.graphql_bindings.len(), 1);
        assert_eq!(x.graphql_bindings[0].coordinate, "Mutation.x");
    }

    #[test]
    fn create_resolver_with_variables_binds_nothing() {
        let src = "export function registerAll(api, registration) {\n  const dataSource = api.addLambdaDataSource(`${registration.name}DataSource`, registration.lambda);\n  for (const resolver of registration.resolvers) {\n    const resolverConstruct = dataSource.createResolver(resolverId, {\n      typeName: resolver.typeName,\n      fieldName: resolver.fieldName,\n      runtime: appsync.FunctionRuntime.JS_1_0_0,\n    });\n  }\n}\n";
        let x = ts(
            "lib/desci-api-app-sync/constructs/resolver-register.ts",
            src,
        );
        assert!(x.graphql_bindings.is_empty());
        assert!(!x.relations.iter().any(|r| r.kind == EdgeKind::Binds));
    }

    #[test]
    fn lambda_entries_are_repo_relative() {
        let x = ts(
            "lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts",
            CONFIG,
        );
        assert_eq!(
            x.lambda_entries,
            ["lambda/appsync-resolver-labs-lambda/index.ts"]
        );
        let x = ts(
            "lib/desci-api-app-sync/constructs/desci-api-lambda-config.ts",
            "import * as path from \"path\";\nexport class C {\n  make() {\n    return new NodejsFunction(this, \"Fn\", {\n      entry: path.join(__dirname, `../../../lambda/desci-api-lambda/index.ts`),\n    });\n  }\n}\n",
        );
        assert_eq!(x.lambda_entries, ["lambda/desci-api-lambda/index.ts"]);
        let x = ts(
            "lib/a.ts",
            "const someVar = compute();\nexport const a = { entry: someVar };\nexport const b = { entry: path.join(__dirname, \"../../../escape.ts\") };\nexport const c = { entry: \"/abs/handler.ts\" };\nexport const d = { entry: \"README.md\" };\n",
        );
        assert!(x.lambda_entries.is_empty(), "{:?}", x.lambda_entries);
        assert_eq!(
            repo_path("lib/x", "./../y/h.py").as_deref(),
            Some("lib/y/h.py")
        );
    }

    #[test]
    fn graphql_names() {
        assert!(is_graphql_name("createLab") && is_graphql_name("_x1"));
        assert!(!is_graphql_name("1x") && !is_graphql_name("a-b") && !is_graphql_name(""));
    }
}

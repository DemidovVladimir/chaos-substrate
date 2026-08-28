//! Solidity `LangSpec`: contracts / interfaces / libraries as containers,
//! functions, constructors, modifiers, events, structs, enums; `import`
//! directives; calls with receivers; `is` inheritance as `Extends`; and
//! package names from non-relative import paths.

use super::lang::{DefRule, LangSpec};
use super::walker::text;
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput, ImportRef, Relation, Symbol};
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
];

/// One match per imported name; `merge_imports` folds them back per directive.
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
        constructors,
        metadata_and_inheritance,
        merge_imports,
        packages,
    ],
};

/// The Solidity language specification.
pub fn spec() -> Option<&'static LangSpec> {
    Some(&SPEC)
}

fn walk(tree: &Tree, mut f: impl FnMut(Node)) {
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

/// `constructor(...)` has no name field, so the generic walk skips it: add a
/// `Method` named `constructor` under its contract and re-home its calls.
fn constructors(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut added = Vec::new();
    walk(tree, |n| {
        if n.kind() != "constructor_definition" {
            return;
        }
        let Some(contract) = n.parent().and_then(|b| b.parent()) else {
            return;
        };
        let Some(parent_idx) = symbol_at(out, contract) else {
            return;
        };
        let parent = &out.symbols[parent_idx];
        let qualified = format!("{}::constructor", parent.qualified_name);
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
            .chars()
            .take(200)
            .collect();
        added.push(Symbol {
            stable_id: format!("solidity:{}:method:{qualified}", file.path),
            kind: NodeKind::Method,
            name: "constructor".into(),
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
                    let mut cursor = b.walk();
                    b.named_children(&mut cursor)
                        .map(|c| (c.start_byte(), c.end_byte()))
                        .collect()
                })
                .unwrap_or_default(),
            metadata: serde_json::json!({}),
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

/// `metadata.library/modifier/event` flags and `is A, B` → `Extends` relations.
fn metadata_and_inheritance(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    walk(tree, |n| {
        let flag = match n.kind() {
            "library_declaration" => "library",
            "modifier_definition" => "modifier",
            "event_definition" => "event",
            "contract_declaration" | "interface_declaration" => "",
            _ => return,
        };
        let Some(idx) = symbol_at(out, n) else {
            return;
        };
        if !flag.is_empty() {
            out.symbols[idx].metadata[flag] = serde_json::Value::Bool(true);
        }
        let source = out.symbols[idx].stable_id.clone();
        let mut cursor = n.walk();
        for spec in n
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "inheritance_specifier")
        {
            if let Some(ancestor) = spec.child_by_field_name("ancestor") {
                out.relations.push(Relation {
                    source: source.clone(),
                    kind: EdgeKind::Extends,
                    target_name: text(ancestor, src).trim().to_string(),
                });
            }
        }
    });
}

/// Strip the string quotes and fold per-name matches of one directive together.
fn merge_imports(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    let mut merged: Vec<ImportRef> = Vec::new();
    for mut imp in out.imports.drain(..) {
        imp.specifier = imp.specifier.trim_matches(['"', '\'']).to_string();
        match merged
            .last_mut()
            .filter(|m| m.line == imp.line && m.specifier == imp.specifier)
        {
            Some(m) => m.names.extend(imp.names),
            None => merged.push(imp),
        }
    }
    out.imports = merged;
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

    mapping(address => uint256) private _balances;

    modifier onlyPositive(uint256 amount) {
        require(amount > 0, "zero");
        _;
    }

    /// Sets the owner.
    constructor(string memory name_) Ownable(msg.sender) {
        _mint(msg.sender, 1000);
    }

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

    fn sol() -> FileExtraction {
        let file = FileInput {
            path: "contracts/Token.sol".into(),
            language: Language::Solidity,
            content: TOKEN.into(),
        };
        extract(spec().unwrap(), &file)
    }

    fn find<'a>(x: &'a FileExtraction, id: &str) -> &'a Symbol {
        x.symbols
            .iter()
            .find(|s| s.stable_id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    #[test]
    fn symbol_kinds_ids_and_parents() {
        let x = sol();
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "solidity:contracts/Token.sol:type:MathLib",
                "solidity:contracts/Token.sol:method:MathLib::min",
                "solidity:contracts/Token.sol:interface:IVault",
                "solidity:contracts/Token.sol:method:IVault::deposit",
                "solidity:contracts/Token.sol:type:Token",
                "solidity:contracts/Token.sol:type:Token::Info",
                "solidity:contracts/Token.sol:enum:Token::State",
                "solidity:contracts/Token.sol:field:Token::Transfer",
                "solidity:contracts/Token.sol:function:Token::onlyPositive",
                "solidity:contracts/Token.sol:method:Token::constructor",
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
        assert_eq!(token.segments.len(), 10, "members + the two doc comments");
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
        assert_eq!(ev.doc.as_deref(), Some("Emitted on transfer."));
    }

    #[test]
    fn inheritance_becomes_extends() {
        let x = sol();
        assert_eq!(
            x.relations,
            vec![
                Relation {
                    source: "solidity:contracts/Token.sol:type:Token".into(),
                    kind: EdgeKind::Extends,
                    target_name: "IERC20".into()
                },
                Relation {
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
        assert_eq!(free.len(), 6, "{free:?}");
    }

    #[test]
    fn calls_with_receivers_and_callers() {
        let x = sol();
        let calls: Vec<(&str, Option<&str>, &str)> = x
            .calls
            .iter()
            .map(|c| (c.callee.as_str(), c.receiver.as_deref(), c.caller.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![
                (
                    "require",
                    None,
                    "solidity:contracts/Token.sol:function:Token::onlyPositive"
                ),
                (
                    "_mint",
                    None,
                    "solidity:contracts/Token.sol:method:Token::constructor"
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
                    Some("IVault(to)"),
                    "solidity:contracts/Token.sol:method:Token::_mint"
                ),
            ]
        );
    }
}

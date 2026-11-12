//! Contract ABI algebra: canonical parameter types, `name(t1,t2)` signatures
//! and their comparison, human-readable ABI items
//! (`"function f(uint a) view returns (bool)"`), and the name rules that tie a
//! consumer's ABI to the contract it mirrors. Pure: no I/O.
//!
//! Spelling matches what the Solidity extractor writes as
//! `metadata.abi_signature` (canonical ABI types: `uint` → `uint256`,
//! `address payable` → `address`, tuples as `(t1,t2)`, array suffixes kept).
//! A type the consumer's text cannot spell out (a struct name in a
//! human-readable ABI, a bare `tuple` with no components) becomes `*`, which
//! [`signatures_agree`] treats as a wildcard.

/// What an ABI item declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiItemKind {
    Function,
    Event,
    Error,
    Constructor,
    Fallback,
    Receive,
}

impl AbiItemKind {
    /// The JSON `type` / human-readable keyword (`function`, `event`, …).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "function" => Self::Function,
            "event" => Self::Event,
            "error" => Self::Error,
            "constructor" => Self::Constructor,
            "fallback" => Self::Fallback,
            "receive" => Self::Receive,
            _ => return None,
        })
    }
}

/// One ABI item with its canonical signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiItem {
    pub kind: AbiItemKind,
    pub name: String,
    /// Canonical `name(t1,t2)`; `*` stands for a type the source cannot spell.
    pub signature: String,
    /// `view`, `pure`, `payable`, `nonpayable` when stated.
    pub mutability: Option<String>,
}

/// Wildcard for a type the consumer's ABI text cannot spell out.
pub const UNKNOWN_TYPE: &str = "*";

/// Shortest contract-name key [`hint_keys`] produces: shorter keys
/// (`lab`, `ocl`, `rwa`) would name too many contracts.
pub const MIN_HINT_KEY: usize = 4;

/// Function, event and error names every ERC-20/721/1155, Ownable,
/// AccessControl, Pausable, UUPS or EIP-712 contract shares. A call to one of
/// these, with no contract named, says nothing about which contract it hits.
pub const STANDARD_NAMES: &[&str] = &[
    "allowance",
    "approve",
    "balanceOf",
    "balanceOfBatch",
    "burn",
    "burnFrom",
    "decimals",
    "decreaseAllowance",
    "DEFAULT_ADMIN_ROLE",
    "DOMAIN_SEPARATOR",
    "eip712Domain",
    "getApproved",
    "getRoleAdmin",
    "grantRole",
    "hasRole",
    "increaseAllowance",
    "isApprovedForAll",
    "mint",
    "name",
    "nonces",
    "owner",
    "ownerOf",
    "pause",
    "paused",
    "pendingOwner",
    "acceptOwnership",
    "permit",
    "proxiableUUID",
    "renounceOwnership",
    "renounceRole",
    "revokeRole",
    "safeBatchTransferFrom",
    "safeTransferFrom",
    "setApprovalForAll",
    "supportsInterface",
    "symbol",
    "tokenByIndex",
    "tokenOfOwnerByIndex",
    "tokenURI",
    "totalSupply",
    "transfer",
    "transferFrom",
    "transferOwnership",
    "unpause",
    "UPGRADE_INTERFACE_VERSION",
    "upgradeToAndCall",
    "uri",
    "Approval",
    "ApprovalForAll",
    "Initialized",
    "OwnershipTransferred",
    "Paused",
    "RoleAdminChanged",
    "RoleGranted",
    "RoleRevoked",
    "Transfer",
    "TransferBatch",
    "TransferSingle",
    "Unpaused",
    "Upgraded",
];

/// Whether `name` is a standard-interface member ([`STANDARD_NAMES`]).
pub fn is_standard_name(name: &str) -> bool {
    STANDARD_NAMES.contains(&name)
}

/// A standard ABI (an ERC-20 ABI used for any token): at least one name, and
/// at least 80% of them standard.
pub fn is_standard_abi<S: AsRef<str>>(names: &[S]) -> bool {
    let std = names
        .iter()
        .filter(|n| is_standard_name(n.as_ref()))
        .count();
    !names.is_empty() && std * 5 >= names.len() * 4
}

/// Split `s` on `sep` at parenthesis depth 0, trimming each piece; an empty
/// string gives no pieces.
pub fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(s[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last);
    }
    out
}

/// Byte index of the `)` matching the `(` at `open`.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `[2][]` kept, whitespace inside brackets dropped; anything that is not a
/// bracket group ends the suffix.
fn array_suffix(rest: &str) -> String {
    let mut out = String::new();
    let mut s = rest.trim_start();
    while let Some(r) = s.strip_prefix('[') {
        let Some(end) = r.find(']') else { break };
        out.push('[');
        out.push_str(&r[..end].split_whitespace().collect::<String>());
        out.push(']');
        s = r[end + 1..].trim_start();
    }
    out
}

fn sized(base: &str, prefix: &str, ok: impl Fn(u32) -> bool) -> bool {
    base.strip_prefix(prefix)
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(ok)
}

fn canonical_base(base: &str) -> String {
    match base {
        "uint" => "uint256".into(),
        "int" => "int256".into(),
        "byte" => "bytes1".into(),
        "fixed" => "fixed128x18".into(),
        "ufixed" => "ufixed128x18".into(),
        "address" | "bool" | "string" | "bytes" | "function" => base.into(),
        b if sized(b, "uint", |n| n % 8 == 0 && (8..=256).contains(&n))
            || sized(b, "int", |n| n % 8 == 0 && (8..=256).contains(&n))
            || sized(b, "bytes", |n| (1..=32).contains(&n)) =>
        {
            b.into()
        }
        b if (b.starts_with("fixed") || b.starts_with("ufixed")) && b.contains('x') => b.into(),
        _ => UNKNOWN_TYPE.into(),
    }
}

/// Canonical ABI spelling of the type that LEADS `raw` (a JSON `type`
/// string, or a human-readable parameter such as `uint indexed amount`):
/// `uint` → `uint256`, `int` → `int256`, `address payable` → `address`,
/// data locations, `indexed` and the parameter name ignored, array suffixes
/// kept (`uint[2]` → `uint256[2]`), tuples written out (`(uint a, T b)[]` →
/// `(uint256,*)[]`). A bare `tuple` or a user type (struct, contract or enum
/// name the text cannot resolve) becomes `*`; `contract X` → `address`,
/// `enum X` → `uint8` (solc `internalType` spellings).
pub fn canonical_type(raw: &str) -> String {
    let s = raw.trim();
    let group = s
        .strip_prefix("tuple")
        .filter(|r| r.starts_with('('))
        .unwrap_or(s);
    if group.starts_with('(') {
        let Some(close) = matching_paren(group, 0) else {
            return UNKNOWN_TYPE.into();
        };
        let inner: Vec<String> = split_top_level(&group[1..close], ',')
            .into_iter()
            .map(canonical_type)
            .collect();
        return format!("({}){}", inner.join(","), array_suffix(&group[close + 1..]));
    }
    let mut words = s.split_whitespace();
    let Some(first) = words.next() else {
        return UNKNOWN_TYPE.into();
    };
    let (first, rest_after_word) = match first {
        "contract" | "struct" | "enum" => {
            let Some(name) = words.next() else {
                return UNKNOWN_TYPE.into();
            };
            let base = match first {
                "contract" => "address",
                "enum" => "uint8",
                _ => UNKNOWN_TYPE,
            };
            let suffix = name.find('[').map_or("", |i| &name[i..]);
            return format!("{base}{}", array_suffix(suffix));
        }
        f => (f, &s[f.len()..]),
    };
    let (base, suffix) = match first.find('[') {
        Some(i) => (&first[..i], format!("{}{rest_after_word}", &first[i..])),
        None => (first, rest_after_word.to_string()),
    };
    format!("{}{}", canonical_base(base), array_suffix(&suffix))
}

/// `name(t1,t2)` from canonical types.
pub fn signature<S: AsRef<str>>(name: &str, types: &[S]) -> String {
    let ts: Vec<&str> = types.iter().map(AsRef::as_ref).collect();
    format!("{name}({})", ts.join(","))
}

/// `name(t1,t2)` → (`name`, [`t1`, `t2`]).
pub fn parse_signature(sig: &str) -> Option<(&str, Vec<&str>)> {
    let open = sig.find('(')?;
    let close = matching_paren(sig, open)?;
    let name = sig[..open].trim();
    if name.is_empty() {
        return None;
    }
    Some((name, split_top_level(&sig[open + 1..close], ',')))
}

/// The name part of `name(t1,t2)` (the whole text when there is no `(`).
pub fn signature_name(sig: &str) -> &str {
    sig.split('(').next().unwrap_or(sig).trim()
}

/// The parameter count of `name(t1,t2)`.
pub fn signature_arity(sig: &str) -> Option<usize> {
    parse_signature(sig).map(|(_, ts)| ts.len())
}

/// Base type and array suffix: `(a,b)[2]` → (`(a,b)`, `[2]`).
fn split_suffix(t: &str) -> (&str, &str) {
    let end = if t.starts_with('(') {
        matching_paren(t, 0).map_or(t.len(), |c| c + 1)
    } else {
        t.find('[').unwrap_or(t.len())
    };
    (&t[..end], &t[end..])
}

/// The member types of a tuple base `(t1,t2)`; `None` when `t` is not one.
fn tuple_members(t: &str) -> Option<Vec<&str>> {
    let inner = t.strip_prefix('(')?.strip_suffix(')')?;
    Some(split_top_level(inner, ','))
}

/// Two canonical types agree when equal, or when either base is `*` with the
/// same array suffix, or when both are tuples whose members agree pairwise.
pub fn types_agree(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ab, asuf) = split_suffix(a);
    let (bb, bsuf) = split_suffix(b);
    if asuf != bsuf {
        return false;
    }
    if ab == UNKNOWN_TYPE || bb == UNKNOWN_TYPE {
        return true;
    }
    match (tuple_members(ab), tuple_members(bb)) {
        (Some(ai), Some(bi)) => {
            ai.len() == bi.len() && ai.iter().zip(&bi).all(|(x, y)| types_agree(x, y))
        }
        _ => false,
    }
}

/// A JSON ABI tuple written out from its canonical component types, with the
/// array suffix that followed `tuple` in its `type` (`tuple[2][]` → `[2][]`).
/// A suffix that is not bracket groups (a malformed type) makes it `*`.
pub fn tuple_type<S: AsRef<str>>(components: &[S], suffix: &str) -> String {
    let canon = array_suffix(suffix);
    if canon != suffix.split_whitespace().collect::<String>() {
        return UNKNOWN_TYPE.into();
    }
    let inner: Vec<&str> = components.iter().map(AsRef::as_ref).collect();
    format!("({}){canon}", inner.join(","))
}

/// Same name, same arity, and every parameter type agrees ([`types_agree`]).
pub fn signatures_agree(a: &str, b: &str) -> bool {
    let (Some((an, at)), Some((bn, bt))) = (parse_signature(a), parse_signature(b)) else {
        return false;
    };
    an == bn && at.len() == bt.len() && at.iter().zip(&bt).all(|(x, y)| types_agree(x, y))
}

fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Parse one human-readable ABI item (viem `parseAbi` / ethers style):
/// `function getActiveDid(bytes32 oclId, bytes32 provider) view returns (string)`,
/// `event RoleGranted(bytes32 indexed oclId, …)`, `error Unauthorized()`,
/// `constructor(address a)`. Tokens, not regex; `None` for anything else
/// (struct definitions included: their names become `*` in the items that use them).
pub fn human_readable_item(s: &str) -> Option<AbiItem> {
    let s = s.trim();
    let word_end = s
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(s.len());
    let kind = AbiItemKind::parse(&s[..word_end])?;
    let rest = s[word_end..].trim_start();
    let (name, rest) = match kind {
        AbiItemKind::Constructor | AbiItemKind::Fallback | AbiItemKind::Receive => {
            (s[..word_end].to_string(), rest)
        }
        _ => {
            let open = rest.find('(')?;
            (rest[..open].trim().to_string(), &rest[open..])
        }
    };
    if !is_ident(&name) || !rest.starts_with('(') {
        return None;
    }
    let close = matching_paren(rest, 0)?;
    let types: Vec<String> = split_top_level(&rest[1..close], ',')
        .into_iter()
        .map(canonical_type)
        .collect();
    let tail = &rest[close + 1..];
    let tail = tail.split("returns").next().unwrap_or(tail);
    let mutability = tail
        .split_whitespace()
        .find(|w| matches!(*w, "view" | "pure" | "payable" | "nonpayable"))
        .map(str::to_string);
    Some(AbiItem {
        kind,
        signature: signature(&name, &types),
        name,
        mutability,
    })
}

/// Words of an identifier, lower-cased: snake, camel and acronym boundaries
/// (`LABNFT_OWNER_OF_ABI` → labnft, owner, of, abi; `ILabNFTMinimal` → i, lab,
/// nft, minimal; `ERC721Upgradeable` → erc721, upgradeable).
pub fn name_words(ident: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in ident.split(|c: char| !c.is_ascii_alphanumeric()) {
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 && !cur.is_empty() && c.is_ascii_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                if prev.is_ascii_lowercase()
                    || prev.is_ascii_digit()
                    || (prev.is_ascii_uppercase() && next_lower)
                {
                    words.push(std::mem::take(&mut cur).to_ascii_lowercase());
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            words.push(cur.to_ascii_lowercase());
        }
    }
    words
}

/// The key a contract name is matched by: lower-case, `_` removed.
pub fn contract_key(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Contract-name keys an ABI or interface name hints at, longest first:
/// joined word prefixes with trailing `abi`/`contract` words dropped; for an
/// `I`-prefixed name also the prefixes without the `i`. Keys shorter than
/// [`MIN_HINT_KEY`] are left out (`LABNFT_OWNER_OF_ABI` → labnftownerof,
/// labnftowner, labnft).
pub fn hint_keys(name: &str) -> Vec<String> {
    let mut words = name_words(name);
    while words
        .last()
        .is_some_and(|w| w == "abi" || w == "contract" || w == "json")
    {
        words.pop();
    }
    let strip_i = words.len() > 1 && words[0] == "i";
    let mut out: Vec<String> = Vec::new();
    for k in (1..=words.len()).rev() {
        let mut cands = vec![words[..k].concat()];
        if strip_i && k >= 2 {
            cands.push(words[1..k].concat());
        }
        for c in cands {
            if c.len() >= MIN_HINT_KEY && !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Normalised ABI identifier for loose same-repo matching: lower-case, `_`
/// removed, a trailing `abi` dropped (`ACCESS_RESOLVER_ABI` → accessresolver).
pub fn abi_name_key(name: &str) -> String {
    let k = contract_key(name);
    match k.strip_suffix("abi") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_type_table() {
        let table = [
            ("uint", "uint256"),
            ("int", "int256"),
            ("uint8", "uint8"),
            ("address payable", "address"),
            ("string memory", "string"),
            ("bytes32 indexed oclId", "bytes32"),
            ("tuple", "*"),
            ("tuple[]", "*[]"),
            ("uint[2]", "uint256[2]"),
            ("uint[2][] calldata xs", "uint256[2][]"),
            ("ILabToken", "*"),
            ("ILabToken token", "*"),
            ("(uint a, ILabToken b)[] memory items", "(uint256,*)[]"),
            ("tuple(address,uint)", "(address,uint256)"),
            ("contract ILabNFTMinimal", "address"),
            ("enum State", "uint8"),
            ("struct Info[]", "*[]"),
            ("bytes33", "*"),
            ("uint7", "*"),
        ];
        for (raw, want) in table {
            assert_eq!(canonical_type(raw), want, "{raw}");
        }
    }

    #[test]
    fn selectors_agree_table() {
        let table = [
            (
                "hasRole(bytes32,address)",
                "hasRole(bytes32,address,uint8)",
                false,
            ),
            ("tokenize(*,bytes)", "tokenize(uint256,bytes)", true),
            ("tokenize(*,bytes)", "tokenize(*,bytes)", true),
            ("tokenize(uint256)", "tokenized(uint256)", false),
            ("f(*[])", "f(uint256[])", true),
            ("f(*[])", "f(uint256)", false),
            ("f((uint256,*))", "f((uint256,address))", true),
            ("f((uint256,*))", "f((uint256,address,bool))", false),
            ("f(address)", "f(uint256)", false),
            ("f()", "f()", true),
        ];
        for (a, b, want) in table {
            assert_eq!(signatures_agree(a, b), want, "{a} vs {b}");
            assert_eq!(signatures_agree(b, a), want, "{b} vs {a}");
        }
        assert_eq!(signature_arity("f(a,(b,c))"), Some(2));
        assert_eq!(signature_arity("f()"), Some(0));
        assert_eq!(signature_name("ownerOf(uint256)"), "ownerOf");
    }

    #[test]
    fn malformed_types_never_panic() {
        // `(]é` splits into an unclosed tuple whose last char is multi-byte.
        assert!(!signatures_agree(
            "f((uint256),(]é,x))",
            "f((uint256),(address),uint256)"
        ));
        assert!(!types_agree("(]é", "(address)"));
        assert!(!types_agree("(", "(address)"));
        assert!(types_agree("()", "()"));
        assert_eq!(
            tuple_type(&["uint256", "address[]"], "[2] []"),
            "(uint256,address[])[2][]"
        );
        assert_eq!(tuple_type::<&str>(&[], ""), "()");
        assert_eq!(tuple_type(&["uint256"], ",(]é,x)"), UNKNOWN_TYPE);
    }

    #[test]
    fn human_readable_items() {
        let f = human_readable_item(
            "function getActiveDid(bytes32 oclId, bytes32 provider, bytes32 subject) view returns (string)",
        )
        .unwrap();
        assert_eq!(f.kind, AbiItemKind::Function);
        assert_eq!(f.signature, "getActiveDid(bytes32,bytes32,bytes32)");
        assert_eq!(f.mutability.as_deref(), Some("view"));
        let e = human_readable_item(
            "event RoleGranted(bytes32 indexed oclId, address indexed account, uint8 indexed role, uint64 expiry, bool isAgent, address grantedBy)",
        )
        .unwrap();
        assert_eq!(e.kind, AbiItemKind::Event);
        assert_eq!(
            e.signature,
            "RoleGranted(bytes32,address,uint8,uint64,bool,address)"
        );
        let g = human_readable_item(
            "function grantRole(bytes32 oclId, address account, uint8 role, uint64 expiry, bool isAgent) external",
        )
        .unwrap();
        assert_eq!(g.signature, "grantRole(bytes32,address,uint8,uint64,bool)");
        assert_eq!(g.mutability, None);
        let err = human_readable_item("error Unauthorized()").unwrap();
        assert_eq!(
            (err.kind, err.signature.as_str()),
            (AbiItemKind::Error, "Unauthorized()")
        );
        let s = human_readable_item("function tokenize(Terms terms, bytes sig)").unwrap();
        assert_eq!(s.signature, "tokenize(*,bytes)");
        let c = human_readable_item("constructor(address owner)").unwrap();
        assert_eq!(c.signature, "constructor(address)");
        for bad in [
            "hello world",
            "function (uint256)",
            "function f(uint256",
            "struct Terms { uint256 a; }",
            "",
        ] {
            assert!(human_readable_item(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn name_words_and_hint_keys() {
        assert_eq!(
            name_words("LABNFT_OWNER_OF_ABI"),
            ["labnft", "owner", "of", "abi"]
        );
        assert_eq!(name_words("ILabNFTMinimal"), ["i", "lab", "nft", "minimal"]);
        assert_eq!(name_words("ERC721Upgradeable"), ["erc721", "upgradeable"]);
        assert_eq!(
            name_words("onChainLabFactoryAbi"),
            ["on", "chain", "lab", "factory", "abi"]
        );
        let k = hint_keys("LABNFT_OWNER_OF_ABI");
        assert_eq!(k[0], "labnftownerof");
        assert!(k.contains(&"labnft".to_string()));
        assert_eq!(hint_keys("labNftAbi"), ["labnft"]);
        assert_eq!(hint_keys("onChainLabFactoryAbi")[0], "onchainlabfactory");
        let i = hint_keys("ILabNFTMinimal");
        assert!(i.contains(&"ilabnft".to_string()) && i.contains(&"labnft".to_string()));
        assert!(!i.iter().any(|k| k.len() < MIN_HINT_KEY));
        assert_eq!(hint_keys("IPNFT_CONTRACT"), ["ipnft"]);
        assert_eq!(contract_key("Access_Resolver"), "accessresolver");
        assert_eq!(abi_name_key("ACCESS_RESOLVER_ABI"), "accessresolver");
        assert_eq!(abi_name_key("AGENTFACTORY"), "agentfactory");
        assert_eq!(abi_name_key("abi"), "abi");
    }

    #[test]
    fn standard_abi_detection() {
        let erc20 = [
            "name",
            "symbol",
            "decimals",
            "totalSupply",
            "balanceOf",
            "transfer",
            "approve",
            "allowance",
            "transferFrom",
        ];
        assert!(is_standard_abi(&erc20));
        let resolver = [
            "getRole",
            "grantRole",
            "revokeRole",
            "hasRole",
            "isAuthorizedSignerForIpnft",
            "ownersOfIpnft",
        ];
        assert!(!is_standard_abi(&resolver));
        assert!(!is_standard_abi::<&str>(&[]));
        assert!(is_standard_name("ownerOf") && !is_standard_name("tokenized"));
    }
}

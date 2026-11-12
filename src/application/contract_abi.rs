//! Contract consumers → the Solidity contract they call, resolved from the
//! persisted index. Shared by `correlations` (cross-repo `contract_abi`
//! links) and `entry_points` (callers on contract rows). Pure: no I/O.
//!
//! **Providers** are Solidity contracts and interfaces (not libraries, deploy
//! scripts or tests). A container's *surface* is its public/external
//! `Method`s (not the constructor, `receive` or `fallback`), its public
//! state-variable getters (`Field`, `metadata.getter`) and its events
//! (`Field`, `metadata.event`); its *family* adds the surface of every
//! in-repo ancestor (`Extends` edges). A container's *leaf* is the unique
//! most-derived contract below it (`AccessResolver` for
//! `IAccessResolver`), else itself.
//!
//! **Consumers** are the call sites recorded on File nodes
//! (`metadata.contract_calls`, test files left out), and the functions of a
//! consumer-side Solidity interface (one no in-repo container implements,
//! e.g. a minimal `ILabNft` a contract keeps of another repo's contract). A
//! call's ABI is the `Abi` node its `UsesAbi` edge reaches; a call whose ABI
//! is named but not indexed (viem's `erc20Abi`) is outside: never linked.
//!
//! **Tiers**, tried in order (never fuzzy):
//! 1. *named*: the ABI's name (or Hardhat `contractName`) names a container
//!    ([`hint_keys`]; `i`+key when no container has the key itself); the
//!    member is in its leaf's family with an agreeing signature →
//!    [`CONTRACT_ABI_NAMED_CONFIDENCE`]. Not in the indexed family at all,
//!    but the leaf inherits bases the index does not hold (`metadata.bases`,
//!    e.g. OpenZeppelin `ERC721`) → the leaf container at
//!    [`CONTRACT_ABI_INHERITED_CONFIDENCE`] (*inherited*), unless another
//!    indexed contract declares the member: then the call is left unlinked.
//! 2. a standard ABI (mostly ERC-20/721 names) stops here.
//! 3. *overlap*: the ABI has ≥ 2 functions; one leaf family agrees on ≥ 2 and
//!    ≥ half of them, strictly more than any other, and provides the member →
//!    [`CONTRACT_ABI_OVERLAP_CONFIDENCE`].
//! 4. *unique*: no ABI, or one with ≤ 3 items; exactly one leaf family
//!    provides the member with an agreeing signature (a standard name only
//!    when its signature is known) → [`CONTRACT_ABI_UNIQUE_CONFIDENCE`]. A
//!    consumer interface never links to its own repo through this tier.
//!
//! A consumer interface is never resolved to a container declared in its own
//! file: it mirrors another contract, not its neighbour.
//!
//! An ABI-only use (no member: `decodeEventLog({abi})`) links through the
//! named tier when the family covers ≥ half the ABI, or the overlap tier,
//! [`ABI_USE_PENALTY`] lower; a combined ABI expands to its parts.
//!
//! Signatures agree per [`signatures_agree`] when both sides carry one
//! (`metadata.abi_signature` on the provider); otherwise by name and arity
//! (`metadata.params`), otherwise by name.

use crate::application::entry_points::{is_script_or_test, is_test_path};
use crate::domain::abi::{
    abi_name_key, contract_key, hint_keys, is_standard_abi, is_standard_name, signature_arity,
    signature_name, signatures_agree,
};
use crate::domain::model::{Edge, EdgeKind, Node, NodeKind};
use crate::ports::extractor::{ContractCall, ContractMemberKind};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

pub const CONTRACT_ABI_NAMED_CONFIDENCE: f32 = 0.85;
pub const CONTRACT_ABI_INHERITED_CONFIDENCE: f32 = 0.6;
pub const CONTRACT_ABI_OVERLAP_CONFIDENCE: f32 = 0.7;
pub const CONTRACT_ABI_UNIQUE_CONFIDENCE: f32 = 0.6;
/// Subtracted when a use names only the ABI, not a member.
pub const ABI_USE_PENALTY: f32 = 0.05;
/// Most items an ABI may hold for the unique tier to trust a lone member.
const UNIQUE_MAX_ITEMS: usize = 3;

/// One repository's persisted graph, as this resolver reads it.
pub struct RepoGraph<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
}

/// How a call was tied to its contract (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Named,
    Inherited,
    Overlap,
    Unique,
}

impl Tier {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Inherited => "inherited",
            Self::Overlap => "overlap",
            Self::Unique => "unique",
        }
    }
}

/// One consumer use resolved to a provider contract.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractLink {
    /// Repo index (into the slice given to [`contract_links`]) of the consumer.
    pub consumer: usize,
    /// The consumer node: the call's owner symbol, else its File node; the
    /// interface for a consumer interface.
    pub consumer_node: Option<Uuid>,
    /// The consumer's file id.
    pub consumer_file: Option<Uuid>,
    pub path: String,
    pub line: u32,
    /// The function the call object is passed to; `interface` for a consumer interface.
    pub via: Option<String>,
    /// The ABI identifier as written (the interface name for an interface).
    pub abi: Option<String>,
    pub member: Option<String>,
    pub kind: ContractMemberKind,
    pub provider: usize,
    /// The leaf container node.
    pub container: Uuid,
    pub container_name: String,
    pub container_path: String,
    /// The member's node (in the leaf when it declares it, else the declaring
    /// ancestor); `None` for ABI-only uses and the inherited tier.
    pub member_node: Option<Uuid>,
    pub tier: Tier,
    pub confidence: f32,
    /// Unindexed bases the inherited tier relies on.
    pub bases: Vec<String>,
}

impl ContractLink {
    /// `LabNFT.ownerOf <- readContract (path:44) via LABNFT_OWNER_OF_ABI [named]`.
    pub fn evidence(&self) -> String {
        let tier = if self.tier == Tier::Inherited {
            format!("inherited from unindexed base: {}", self.bases.join(", "))
        } else {
            self.tier.label().to_string()
        };
        format!(
            "{}.{} <- {} ({}:{}) via {} [{tier}]",
            self.container_name,
            self.member.as_deref().unwrap_or("*"),
            self.via.as_deref().unwrap_or("?"),
            self.path,
            self.line,
            self.abi.as_deref().unwrap_or("?"),
        )
    }
}

fn meta_str<'a>(n: &'a Node, key: &str) -> &'a str {
    n.metadata[key].as_str().unwrap_or("")
}

fn is_sol(n: &Node) -> bool {
    meta_str(n, "language") == "solidity"
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A function or event as one side declares it: the name, and the canonical
/// signature / arity when known.
#[derive(Debug, Clone)]
struct Sig {
    name: String,
    sig: Option<String>,
    arity: Option<usize>,
}

impl Sig {
    fn from_signature(s: &str) -> Self {
        Self {
            name: signature_name(s).to_string(),
            arity: signature_arity(s),
            sig: Some(s.to_string()),
        }
    }

    /// A Solidity node: `metadata.abi_signature`, else the arity of `metadata.params`.
    fn from_node(n: &Node) -> Self {
        let sig = n.metadata["abi_signature"].as_str().map(str::to_string);
        Self {
            name: n.name.clone(),
            arity: sig
                .as_deref()
                .and_then(signature_arity)
                .or_else(|| n.metadata["params"].as_array().map(Vec::len)),
            sig,
        }
    }

    fn agrees(&self, other: &Sig) -> bool {
        if self.name != other.name {
            return false;
        }
        match (&self.sig, &other.sig) {
            (Some(a), Some(b)) => signatures_agree(a, b),
            _ => match (self.arity, other.arity) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            },
        }
    }
}

struct Member<'a> {
    node: &'a Node,
    sig: Sig,
    event: bool,
}

struct Container<'a> {
    repo: usize,
    node: &'a Node,
    key: String,
    interface: bool,
}

/// Providers across all repos (see the module docs).
struct ProviderIndex<'a> {
    containers: Vec<Container<'a>>,
    by_key: HashMap<String, Vec<usize>>,
    members: Vec<Vec<Member<'a>>>,
    /// Transitive in-repo ancestors, nearest first.
    ancestors: Vec<Vec<usize>>,
    children: Vec<Vec<usize>>,
    leaf: Vec<usize>,
    /// `metadata.bases` entries that are not in-repo containers.
    unindexed_bases: Vec<Vec<String>>,
    /// Family surface per container: name → (declaring container, member), own first.
    family: Vec<HashMap<&'a str, Vec<(usize, usize)>>>,
}

impl<'a> ProviderIndex<'a> {
    fn build(repos: &[RepoGraph<'a>]) -> Self {
        let mut containers = Vec::new();
        let mut at: HashMap<(usize, &str, &str), usize> = HashMap::new();
        let mut by_id: HashMap<Uuid, usize> = HashMap::new();
        for (ri, r) in repos.iter().enumerate() {
            for n in r.nodes {
                if !is_sol(n)
                    || !matches!(n.kind, NodeKind::Type | NodeKind::Interface)
                    || meta_str(n, "qualified_name").contains("::")
                    || n.metadata["library"] == Value::Bool(true)
                    || is_script_or_test(meta_str(n, "path"))
                {
                    continue;
                }
                at.insert((ri, meta_str(n, "path"), n.name.as_str()), containers.len());
                by_id.insert(n.id, containers.len());
                containers.push(Container {
                    repo: ri,
                    node: n,
                    key: contract_key(&n.name),
                    interface: n.kind == NodeKind::Interface,
                });
            }
        }
        let mut members: Vec<Vec<Member>> = containers.iter().map(|_| Vec::new()).collect();
        let mut parents: Vec<Vec<usize>> = containers.iter().map(|_| Vec::new()).collect();
        for (ri, r) in repos.iter().enumerate() {
            for n in r.nodes.iter().filter(|n| is_sol(n)) {
                let Some((owner, _)) = meta_str(n, "qualified_name").split_once("::") else {
                    continue;
                };
                let Some(&ci) = at.get(&(ri, meta_str(n, "path"), owner)) else {
                    continue;
                };
                let vis = meta_str(n, "visibility");
                let event = n.kind == NodeKind::Field && n.metadata["event"] == Value::Bool(true);
                // `receive` / `fallback` (`metadata.special`) have no ABI
                // name: no call or interface can name them.
                let callable = (n.kind == NodeKind::Method
                    && (vis == "public" || vis == "external")
                    && n.name != "constructor"
                    && n.metadata.get("special").is_none())
                    || (n.kind == NodeKind::Field && n.metadata["getter"] == Value::Bool(true));
                if event || callable {
                    members[ci].push(Member {
                        node: n,
                        sig: Sig::from_node(n),
                        event,
                    });
                }
            }
            for e in r.edges.iter().filter(|e| e.kind == EdgeKind::Extends) {
                if let (Some(&s), Some(&t)) =
                    (by_id.get(&e.source_node_id), by_id.get(&e.target_node_id))
                {
                    if s != t && !parents[s].contains(&t) {
                        parents[s].push(t);
                    }
                }
            }
        }
        let n = containers.len();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (c, ps) in parents.iter().enumerate() {
            for p in ps {
                children[*p].push(c);
            }
        }
        let closure = |start: usize, next: &Vec<Vec<usize>>| -> Vec<usize> {
            let mut seen = vec![start];
            let mut out = Vec::new();
            let mut frontier = vec![start];
            while !frontier.is_empty() {
                let mut nf = Vec::new();
                for c in frontier {
                    for &p in &next[c] {
                        if !seen.contains(&p) {
                            seen.push(p);
                            out.push(p);
                            nf.push(p);
                        }
                    }
                }
                frontier = nf;
            }
            out
        };
        let ancestors: Vec<Vec<usize>> = (0..n).map(|c| closure(c, &parents)).collect();
        let leaf: Vec<usize> = (0..n)
            .map(|c| {
                let mut cands: Vec<usize> = closure(c, &children);
                cands.push(c);
                let leaves: Vec<usize> = cands
                    .into_iter()
                    .filter(|d| !containers[*d].interface && children[*d].is_empty())
                    .collect();
                match leaves.as_slice() {
                    [one] => *one,
                    _ => c,
                }
            })
            .collect();
        let unindexed_bases: Vec<Vec<String>> = containers
            .iter()
            .map(|c| {
                str_list(&c.node.metadata["bases"])
                    .into_iter()
                    .filter(|b| {
                        !containers
                            .iter()
                            .any(|o| o.repo == c.repo && o.node.name == *b)
                    })
                    .collect()
            })
            .collect();
        let family = (0..n)
            .map(|c| {
                let mut m: HashMap<&'a str, Vec<(usize, usize)>> = HashMap::new();
                for d in std::iter::once(c).chain(ancestors[c].iter().copied()) {
                    for (mi, mem) in members[d].iter().enumerate() {
                        m.entry(mem.node.name.as_str()).or_default().push((d, mi));
                    }
                }
                m
            })
            .collect();
        let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in containers.iter().enumerate() {
            by_key.entry(c.key.clone()).or_default().push(i);
        }
        Self {
            containers,
            by_key,
            members,
            ancestors,
            children,
            leaf,
            unindexed_bases,
            family,
        }
    }

    /// Bases the family of `c` inherits that the index does not hold.
    fn family_bases(&self, c: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in std::iter::once(c).chain(self.ancestors[c].iter().copied()) {
            for b in &self.unindexed_bases[d] {
                if !out.contains(b) {
                    out.push(b.clone());
                }
            }
        }
        out
    }

    /// Family members named `name` of the right kind (events or callables).
    fn named(&self, c: usize, name: &str, event: bool) -> Vec<&Member<'a>> {
        self.family[c]
            .get(name)
            .into_iter()
            .flatten()
            .map(|(d, mi)| &self.members[*d][*mi])
            .filter(|m| m.event == event)
            .collect()
    }

    /// The first family member agreeing with any of `wants` (or, with no
    /// wants, the first named `name`).
    fn provides(&self, c: usize, name: &str, event: bool, wants: &[&Sig]) -> Option<&Member<'a>> {
        self.named(c, name, event)
            .into_iter()
            .find(|m| wants.is_empty() || wants.iter().any(|w| w.agrees(&m.sig)))
    }
}

/// A consumer ABI definition: an `Abi` node or a consumer-side interface.
struct AbiDef<'a> {
    repo: usize,
    node: &'a Node,
    hint: String,
    functions: Vec<Sig>,
    events: Vec<Sig>,
    /// Indices of the defs a combined ABI spreads.
    combines: Vec<usize>,
    /// A consumer interface (`Some(container index)` in the provider index).
    interface: Option<usize>,
}

impl AbiDef<'_> {
    fn items(&self) -> impl Iterator<Item = (&Sig, bool)> {
        self.functions
            .iter()
            .map(|s| (s, false))
            .chain(self.events.iter().map(|s| (s, true)))
    }

    fn wants(&self, name: &str, event: bool) -> Vec<&Sig> {
        let list = if event { &self.events } else { &self.functions };
        list.iter().filter(|s| s.name == name).collect()
    }

    fn function_names(&self) -> Vec<&str> {
        self.functions.iter().map(|s| s.name.as_str()).collect()
    }
}

/// One resolution of a use onto a leaf container.
struct Res<'p, 'a> {
    container: usize,
    member: Option<&'p Member<'a>>,
    tier: Tier,
    confidence: f32,
    bases: Vec<String>,
}

struct Resolver<'p, 'a> {
    index: &'p ProviderIndex<'a>,
    defs: Vec<AbiDef<'a>>,
    /// Consumer interfaces mirror another repo's contract: never providers.
    consumer_ifaces: HashSet<usize>,
    allow_same_repo: bool,
}

impl<'p, 'a> Resolver<'p, 'a> {
    /// Whether `c` may provide for a use from repo `consumer`. `iface` is the
    /// consumer interface being resolved: it mirrors another contract, so
    /// neither it nor a neighbour declared in its file provides for it
    /// (onchainlabs `IBeacon` next to `OnChainLabRouter`).
    fn eligible(&self, c: usize, consumer: usize, iface: Option<usize>) -> bool {
        let cs = &self.index.containers;
        let same_file = |x: usize| {
            cs[x].repo == cs[c].repo && meta_str(cs[x].node, "path") == meta_str(cs[c].node, "path")
        };
        (self.allow_same_repo || cs[c].repo != consumer)
            && !iface.is_some_and(same_file)
            && !self.consumer_ifaces.contains(&c)
    }

    /// Share of the def's items the family of `c` provides with an agreeing
    /// signature. A standard name the family lacks is left out of the count
    /// (inherited from a base the index does not hold).
    fn coverage(&self, def: &AbiDef, c: usize) -> f32 {
        let mut total = 0;
        let mut agree = 0;
        for (s, event) in def.items() {
            let hit = self.index.provides(c, &s.name, event, &[s]).is_some();
            if !hit && is_standard_name(&s.name) {
                continue;
            }
            total += 1;
            agree += usize::from(hit);
        }
        if total == 0 {
            0.0
        } else {
            agree as f32 / total as f32
        }
    }

    fn resolve(
        &self,
        def: Option<usize>,
        member: Option<&str>,
        kind: ContractMemberKind,
        consumer: usize,
    ) -> Vec<Res<'p, 'a>> {
        let event = kind == ContractMemberKind::Event;
        if let Some(d) = def {
            let parts = &self.defs[d].combines;
            if !parts.is_empty() {
                return match member {
                    None => parts
                        .iter()
                        .flat_map(|p| self.resolve_one(Some(*p), None, event, consumer))
                        .collect(),
                    Some(m) => parts
                        .iter()
                        .find(|p| !self.defs[**p].wants(m, event).is_empty())
                        .map(|p| self.resolve_one(Some(*p), Some(m), event, consumer))
                        .unwrap_or_default(),
                };
            }
        }
        self.resolve_one(def, member, event, consumer)
    }

    fn resolve_one(
        &self,
        def: Option<usize>,
        member: Option<&str>,
        event: bool,
        consumer: usize,
    ) -> Vec<Res<'p, 'a>> {
        let ix = self.index;
        let d = def.map(|i| &self.defs[i]);
        let iface = d.and_then(|d| d.interface);
        let wants: Vec<&Sig> = match (d, member) {
            (Some(d), Some(m)) => d.wants(m, event),
            _ => Vec::new(),
        };
        let penalty = if member.is_none() {
            ABI_USE_PENALTY
        } else {
            0.0
        };

        // 1. named
        if let Some(d) = d {
            for key in hint_keys(&d.hint) {
                // `Foo` first; `IFoo` only when no `Foo` is eligible (an
                // `ILabToken` with two implementers stays its own leaf and
                // must not make `LabTokenAbi` ambiguous).
                let keyed = |k: &str| -> Vec<usize> {
                    ix.by_key
                        .get(k)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|c| self.eligible(*c, consumer, iface))
                        .collect()
                };
                let mut cands = keyed(&key);
                if cands.is_empty() {
                    cands = keyed(&format!("i{key}"));
                }
                if cands.is_empty() {
                    continue;
                }
                let mut named: Vec<Res> = Vec::new();
                let mut inherited: Vec<Res> = Vec::new();
                // Another indexed contract declares the member: the named leaf
                // inheriting it from an unindexed base would be a guess.
                let declared_elsewhere = |leaf: usize, m: &str| {
                    (0..ix.containers.len())
                        .map(|c| ix.leaf[c])
                        .filter(|l| *l != leaf && self.eligible(*l, consumer, iface))
                        .any(|l| ix.provides(l, m, event, &wants).is_some())
                };
                for c in cands {
                    let leaf = Some(ix.leaf[c])
                        .filter(|l| self.eligible(*l, consumer, iface))
                        .unwrap_or(c);
                    match member {
                        Some(m) => {
                            if !ix.named(leaf, m, event).is_empty() {
                                if let Some(mem) = ix.provides(leaf, m, event, &wants) {
                                    named.push(Res {
                                        container: leaf,
                                        member: Some(mem),
                                        tier: Tier::Named,
                                        confidence: CONTRACT_ABI_NAMED_CONFIDENCE,
                                        bases: vec![],
                                    });
                                }
                                continue;
                            }
                            let bases = ix.family_bases(leaf);
                            if !bases.is_empty() && !declared_elsewhere(leaf, m) {
                                inherited.push(Res {
                                    container: leaf,
                                    member: None,
                                    tier: Tier::Inherited,
                                    confidence: CONTRACT_ABI_INHERITED_CONFIDENCE,
                                    bases,
                                });
                            }
                        }
                        None => {
                            if self.coverage(d, leaf) >= 0.5 {
                                named.push(Res {
                                    container: leaf,
                                    member: None,
                                    tier: Tier::Named,
                                    confidence: CONTRACT_ABI_NAMED_CONFIDENCE - penalty,
                                    bases: vec![],
                                });
                            }
                        }
                    }
                }
                let pick = if named.is_empty() { inherited } else { named };
                let leaves: BTreeSet<usize> = pick.iter().map(|r| r.container).collect();
                return if leaves.len() == 1 {
                    pick.into_iter().take(1).collect()
                } else {
                    Vec::new()
                };
            }
            // 2. a standard ABI never links unnamed
            if is_standard_abi(&d.function_names()) {
                return Vec::new();
            }
        }

        let families: BTreeSet<usize> = (0..ix.containers.len())
            .map(|c| ix.leaf[c])
            .filter(|l| self.eligible(*l, consumer, iface))
            .collect();

        // 3. overlap
        if let Some(d) = d.filter(|d| d.functions.len() >= 2) {
            let mut scored: Vec<(usize, usize)> = families
                .iter()
                .map(|l| {
                    let agree = d
                        .functions
                        .iter()
                        .filter(|s| ix.provides(*l, &s.name, false, &[*s]).is_some())
                        .count();
                    (agree, *l)
                })
                .filter(|(a, _)| *a > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            if let Some(&(best, l)) = scored.first() {
                let runner = scored.get(1).map_or(0, |s| s.0);
                if best >= 2 && best * 2 >= d.functions.len() && best > runner {
                    let mem = member.map(|m| ix.provides(l, m, event, &wants));
                    match mem {
                        Some(None) => {}
                        Some(found) => {
                            return vec![Res {
                                container: l,
                                member: found,
                                tier: Tier::Overlap,
                                confidence: CONTRACT_ABI_OVERLAP_CONFIDENCE,
                                bases: vec![],
                            }]
                        }
                        None => {
                            return vec![Res {
                                container: l,
                                member: None,
                                tier: Tier::Overlap,
                                confidence: CONTRACT_ABI_OVERLAP_CONFIDENCE - penalty,
                                bases: vec![],
                            }]
                        }
                    }
                }
            }
        }

        // 4. unique
        let Some(m) = member else {
            return Vec::new();
        };
        if d.is_some_and(|d| d.functions.len() + d.events.len() > UNIQUE_MAX_ITEMS) {
            return Vec::new();
        }
        let sig_known = wants.iter().any(|w| w.sig.is_some());
        if is_standard_name(m) && !sig_known {
            return Vec::new();
        }
        let hits: Vec<(usize, &Member)> = families
            .iter()
            // A lone name match never ties a consumer interface to its own
            // repo: it mirrors a contract the index lacks more often than not.
            .filter(|l| iface.is_none() || ix.containers[**l].repo != consumer)
            .filter_map(|l| {
                let mem = ix.provides(*l, m, event, &wants)?;
                // A provider without a signature cannot confirm a standard name.
                (!is_standard_name(m) || mem.sig.sig.is_some()).then_some((*l, mem))
            })
            .collect();
        match hits.as_slice() {
            [(l, mem)] if d.is_none_or(|d| self.coverage(d, *l) >= 0.5) => vec![Res {
                container: *l,
                member: Some(*mem),
                tier: Tier::Unique,
                confidence: CONTRACT_ABI_UNIQUE_CONFIDENCE,
                bases: vec![],
            }],
            _ => Vec::new(),
        }
    }
}

/// Consumer ABI definitions over all repos, and the `UsesAbi` targets per
/// source node.
fn build_defs<'a>(
    repos: &[RepoGraph<'a>],
    index: &ProviderIndex<'a>,
) -> (Vec<AbiDef<'a>>, HashMap<Uuid, usize>) {
    let mut defs: Vec<AbiDef> = Vec::new();
    let mut by_node: HashMap<Uuid, usize> = HashMap::new();
    for (ri, r) in repos.iter().enumerate() {
        for n in r.nodes.iter().filter(|n| n.kind == NodeKind::Abi) {
            let abi = &n.metadata["abi"];
            by_node.insert(n.id, defs.len());
            defs.push(AbiDef {
                repo: ri,
                node: n,
                hint: abi["contract"]
                    .as_str()
                    .map_or_else(|| n.name.clone(), str::to_string),
                functions: str_list(&abi["functions"])
                    .iter()
                    .map(|s| Sig::from_signature(s))
                    .collect(),
                events: str_list(&abi["events"])
                    .iter()
                    .map(|s| Sig::from_signature(s))
                    .collect(),
                combines: Vec::new(),
                interface: None,
            });
        }
    }
    // Consumer interfaces: no in-repo container extends them and no
    // same-named contract (`IFoo` / `Foo`) sits in their repo.
    for (ci, c) in index.containers.iter().enumerate() {
        if !c.interface || !index.children[ci].is_empty() {
            continue;
        }
        let bare = c.node.name.strip_prefix('I').unwrap_or(&c.node.name);
        if index.containers.iter().any(|o| {
            o.repo == c.repo && !o.interface && (o.node.name == bare || o.node.name == c.node.name)
        }) {
            continue;
        }
        let functions: Vec<Sig> = index.members[ci]
            .iter()
            .filter(|m| !m.event)
            .map(|m| m.sig.clone())
            .collect();
        if functions.is_empty() {
            continue;
        }
        by_node.insert(c.node.id, defs.len());
        defs.push(AbiDef {
            repo: c.repo,
            node: c.node,
            hint: c.node.name.clone(),
            functions,
            events: index.members[ci]
                .iter()
                .filter(|m| m.event)
                .map(|m| m.sig.clone())
                .collect(),
            combines: Vec::new(),
            interface: Some(ci),
        });
    }
    for r in repos {
        for e in r.edges.iter().filter(|e| e.kind == EdgeKind::UsesAbi) {
            if let (Some(&s), Some(&t)) = (
                by_node.get(&e.source_node_id),
                by_node.get(&e.target_node_id),
            ) {
                if !defs[s].combines.contains(&t) {
                    defs[s].combines.push(t);
                }
            }
        }
    }
    (defs, by_node)
}

fn file_stem(path: &str) -> &str {
    let f = path.rsplit('/').next().unwrap_or(path);
    f.split('.').next().unwrap_or(f)
}

/// The definition a call's `abi` names: its `UsesAbi` target from the owner
/// or the file, else the repo's one `Abi` node of that name. `Err(())` = an
/// ABI is named but not indexed (outside: never linked).
fn def_for(
    call: &ContractCall,
    sources: &[Uuid],
    ri: usize,
    uses: &HashMap<Uuid, Vec<Uuid>>,
    defs: &[AbiDef],
    by_node: &HashMap<Uuid, usize>,
) -> Result<Option<usize>, ()> {
    let Some(abi) = call.abi.as_deref() else {
        return Ok(None);
    };
    let key = abi_name_key(abi);
    let import_stem = call.abi_import.as_deref().map(file_stem);
    let matches = |d: &AbiDef| {
        d.node.name == abi
            || abi_name_key(&d.node.name) == key
            || import_stem.is_some_and(|s| file_stem(meta_str(d.node, "path")) == s)
    };
    let targets: BTreeSet<usize> = sources
        .iter()
        .flat_map(|s| uses.get(s).into_iter().flatten())
        .filter_map(|t| by_node.get(t).copied())
        .filter(|d| matches(&defs[*d]))
        .collect();
    if targets.len() == 1 {
        return Ok(targets.into_iter().next());
    }
    let named: Vec<usize> = defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.repo == ri && d.interface.is_none() && d.node.name == abi)
        .map(|(i, _)| i)
        .collect();
    match named.as_slice() {
        [one] => Ok(Some(*one)),
        _ => Err(()),
    }
}

/// Every consumer use across `repos` resolved to a contract (see the module
/// docs). `allow_same_repo` keeps uses whose contract sits in the consumer's
/// own repo (entry points); correlations pass `false`.
pub fn contract_links(repos: &[RepoGraph], allow_same_repo: bool) -> Vec<ContractLink> {
    let index = ProviderIndex::build(repos);
    let (defs, by_node) = build_defs(repos, &index);
    let resolver = Resolver {
        index: &index,
        consumer_ifaces: defs.iter().filter_map(|d| d.interface).collect(),
        defs,
        allow_same_repo,
    };
    let mut out: Vec<ContractLink> = Vec::new();
    let mut push = |ri: usize, res: Res, base: &ContractLink| {
        let c = &index.containers[res.container];
        let link = ContractLink {
            provider: c.repo,
            container: c.node.id,
            container_name: c.node.name.clone(),
            container_path: meta_str(c.node, "path").to_string(),
            member_node: res.member.map(|m| m.node.id),
            tier: res.tier,
            confidence: res.confidence,
            bases: res.bases,
            consumer: ri,
            ..base.clone()
        };
        if !out.contains(&link) {
            out.push(link);
        }
    };
    for (ri, r) in repos.iter().enumerate() {
        let by_stable: HashMap<&str, &Node> =
            r.nodes.iter().map(|n| (n.stable_id.as_str(), n)).collect();
        let mut uses: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in r.edges.iter().filter(|e| e.kind == EdgeKind::UsesAbi) {
            uses.entry(e.source_node_id)
                .or_default()
                .push(e.target_node_id);
        }
        let mut files: Vec<&Node> = r
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::File && n.metadata["contract_calls"].is_array())
            .filter(|n| !is_test_path(&n.name))
            .collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        for f in files {
            let calls: Vec<ContractCall> =
                serde_json::from_value(f.metadata["contract_calls"].clone()).unwrap_or_default();
            for call in calls {
                let owner = call
                    .owner
                    .as_deref()
                    .and_then(|o| by_stable.get(o))
                    .copied();
                let sources: Vec<Uuid> = owner.map(|o| o.id).into_iter().chain([f.id]).collect();
                let Ok(def) = def_for(&call, &sources, ri, &uses, &resolver.defs, &by_node) else {
                    continue;
                };
                let base = ContractLink {
                    consumer: ri,
                    consumer_node: Some(owner.map_or(f.id, |o| o.id)),
                    consumer_file: f.file_id,
                    path: f.name.clone(),
                    line: call.line,
                    via: call.via.clone(),
                    abi: call.abi.clone(),
                    member: call.member.clone(),
                    kind: call.kind,
                    provider: 0,
                    container: Uuid::nil(),
                    container_name: String::new(),
                    container_path: String::new(),
                    member_node: None,
                    tier: Tier::Named,
                    confidence: 0.0,
                    bases: vec![],
                };
                for res in resolver.resolve(def, call.member.as_deref(), call.kind, ri) {
                    push(ri, res, &base);
                }
            }
        }
    }
    // Consumer interfaces: each function is a use of the contract it mirrors.
    for (di, d) in resolver.defs.iter().enumerate() {
        let Some(ci) = d.interface else { continue };
        let mut seen: HashSet<&str> = HashSet::new();
        for m in index.members[ci].iter().filter(|m| !m.event) {
            if !seen.insert(m.node.name.as_str()) {
                continue;
            }
            let base = ContractLink {
                consumer: d.repo,
                consumer_node: Some(d.node.id),
                consumer_file: d.node.file_id,
                path: meta_str(d.node, "path").to_string(),
                line: m.node.line_start.unwrap_or(0).max(0) as u32,
                via: Some("interface".into()),
                abi: Some(d.node.name.clone()),
                member: Some(m.node.name.clone()),
                kind: ContractMemberKind::Function,
                provider: 0,
                container: Uuid::nil(),
                container_name: String::new(),
                container_path: String::new(),
                member_node: None,
                tier: Tier::Named,
                confidence: 0.0,
                bases: vec![],
            };
            for res in resolver.resolve(
                Some(di),
                Some(&m.node.name),
                ContractMemberKind::Function,
                d.repo,
            ) {
                push(d.repo, res, &base);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::hash;
    use crate::ports::extractor::AbiScope;
    use serde_json::json;

    /// A fixture repo built node by node.
    #[derive(Default)]
    struct Repo {
        alias: &'static str,
        nodes: Vec<Node>,
        edges: Vec<Edge>,
    }

    impl Repo {
        fn new(alias: &'static str) -> Self {
            Self {
                alias,
                ..Default::default()
            }
        }

        fn id(&self, stable: &str) -> Uuid {
            hash::scoped_id(hash::repo_id(self.alias), stable)
        }

        fn node(&mut self, kind: NodeKind, stable: &str, name: &str, meta: Value) -> Uuid {
            let id = self.id(stable);
            let path = meta["path"].as_str().unwrap_or("").to_string();
            self.nodes.push(Node {
                id,
                repo_id: hash::repo_id(self.alias),
                file_id: Some(self.id(&format!("source_file:{path}"))),
                kind,
                stable_id: stable.into(),
                name: name.into(),
                signature: None,
                doc: None,
                line_start: Some(7),
                line_end: Some(9),
                metadata: meta,
            });
            id
        }

        /// A Solidity contract or interface with members `(kind, name, extra meta)`.
        fn container(
            &mut self,
            path: &str,
            kind: NodeKind,
            name: &str,
            extra: Value,
            members: &[(NodeKind, &str, Value)],
        ) -> Uuid {
            let k = if kind == NodeKind::Interface {
                "interface"
            } else {
                "type"
            };
            let mut meta = json!({"language": "solidity", "path": path, "qualified_name": name});
            merge(&mut meta, extra);
            let id = self.node(kind, &format!("solidity:{path}:{k}:{name}"), name, meta);
            for (mk, m, extra) in members {
                let mut meta = json!({"language": "solidity", "path": path, "qualified_name": format!("{name}::{m}")});
                merge(&mut meta, extra.clone());
                self.node(
                    *mk,
                    &format!("solidity:{path}:{}:{name}::{m}", mk.as_str()),
                    m,
                    meta,
                );
            }
            id
        }

        fn edge(&mut self, from: Uuid, to: Uuid, kind: EdgeKind) {
            self.edges.push(Edge {
                id: hash::edge_id(from, to, kind.as_str()),
                repo_id: hash::repo_id(self.alias),
                source_node_id: from,
                target_node_id: to,
                kind,
                confidence: 1.0,
            });
        }

        /// A TS/JSON `Abi` node.
        fn abi(&mut self, path: &str, name: &str, functions: &[&str], events: &[&str]) -> Uuid {
            let lang = if path.ends_with(".json") {
                "abi"
            } else {
                "typescript"
            };
            self.node(
                NodeKind::Abi,
                &format!("{lang}:{path}:abi:{name}"),
                name,
                json!({"language": lang, "path": path, "qualified_name": name,
                       "abi": {"source": "ts_const", "functions": functions, "events": events}}),
            )
        }

        /// A File node carrying `calls`; returns its node id.
        fn calls(&mut self, path: &str, calls: Vec<ContractCall>) -> Uuid {
            self.node(
                NodeKind::File,
                &format!("file:{path}"),
                path,
                json!({"language": "typescript", "contract_calls": calls}),
            )
        }

        fn graph(&self) -> RepoGraph<'_> {
            RepoGraph {
                nodes: &self.nodes,
                edges: &self.edges,
            }
        }
    }

    fn merge(into: &mut Value, extra: Value) {
        if let (Value::Object(a), Value::Object(b)) = (into, extra) {
            a.extend(b);
        }
    }

    fn func(name: &str, sig: Option<&str>) -> (NodeKind, &'static str, Value) {
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let mut meta = json!({"visibility": "external"});
        if let Some(s) = sig {
            meta["abi_signature"] = json!(s);
        }
        (NodeKind::Method, name, meta)
    }

    fn getter(name: &'static str, sig: &str) -> (NodeKind, &'static str, Value) {
        (
            NodeKind::Field,
            name,
            json!({"getter": true, "state_var": true, "visibility": "public", "mutability": "view", "abi_signature": sig}),
        )
    }

    fn event(name: &'static str) -> (NodeKind, &'static str, Value) {
        (NodeKind::Field, name, json!({"event": true}))
    }

    fn call(
        member: Option<&str>,
        kind: ContractMemberKind,
        abi: Option<&str>,
        line: u32,
    ) -> ContractCall {
        ContractCall {
            member: member.map(str::to_string),
            kind,
            abi: abi.map(str::to_string),
            abi_import: None,
            scope: AbiScope::Object,
            via: Some("readContract".into()),
            owner: None,
            line,
        }
    }

    fn fcall(member: &str, abi: Option<&str>, line: u32) -> ContractCall {
        call(Some(member), ContractMemberKind::Function, abi, line)
    }

    /// Provider repos shaped like Labs, onchainlabs and ocltokenizer.
    fn providers() -> (Repo, Repo, Repo) {
        let mut labs = Repo::new("labs");
        let iface = labs.container(
            "src/interfaces/IAccessResolver.sol",
            NodeKind::Interface,
            "IAccessResolver",
            json!({}),
            &[func(
                "isAuthorizedSignerForIpnft",
                Some("isAuthorizedSignerForIpnft(address,uint256)"),
            )],
        );
        let ar = labs.container(
            "src/AccessResolver.sol",
            NodeKind::Type,
            "AccessResolver",
            json!({"bases": ["IAccessResolver", "UUPSUpgradeable"]}),
            &[
                func(
                    "grantRole",
                    Some("grantRole(bytes32,address,uint8,uint64,bool)"),
                ),
                func("hasRole", Some("hasRole(bytes32,address,uint8)")),
                func("ownersOfIpnft", Some("ownersOfIpnft(uint256)")),
                func("getRole", Some("getRole(bytes32,address)")),
                func(
                    "isAuthorizedSignerForIpnft",
                    Some("isAuthorizedSignerForIpnft(address,uint256)"),
                ),
                event("RoleGranted"),
            ],
        );
        labs.edge(ar, iface, EdgeKind::Extends);
        // A consumer interface: Labs keeps a minimal view of onchainlabs LabNFT.
        labs.container(
            "src/interfaces/ILabNft.sol",
            NodeKind::Interface,
            "ILabNft",
            json!({}),
            &[func("accountOf", Some("accountOf(uint256)"))],
        );
        let mut ocl = Repo::new("onchainlabs");
        let ilab = ocl.container(
            "src/NFT/ILabNFT.sol",
            NodeKind::Interface,
            "ILabNFT",
            json!({}),
            &[func("accountOf", Some("accountOf(uint256)"))],
        );
        let lab = ocl.container(
            "src/NFT/LabNFT.sol",
            NodeKind::Type,
            "LabNFT",
            json!({"bases": ["Initializable", "ERC721", "AccessControl", "UUPSUpgradeable", "ILabNFT"]}),
            &[
                func("accountOf", Some("accountOf(uint256)")),
                func("oclIdOf", Some("oclIdOf(uint256)")),
                getter("mintFeeWei", "mintFeeWei()"),
            ],
        );
        ocl.edge(lab, ilab, EdgeKind::Extends);
        ocl.container(
            "src/OnChainLab.sol",
            NodeKind::Type,
            "OnChainLab",
            json!({}),
            &[
                func("execute", Some("execute(address,uint256,bytes,uint8)")),
                func("isValidSigner", Some("isValidSigner(address,bytes)")),
                func("token", Some("token()")),
            ],
        );
        let mut tok = Repo::new("ocltokenizer");
        tok.container(
            "src/OclTokenizer.sol",
            NodeKind::Type,
            "OclTokenizer",
            json!({}),
            &[
                getter("tokenized", "tokenized(uint256)"),
                func("tokenize", Some("tokenize(uint256,(string,string),bytes)")),
                func("controllerOf", Some("controllerOf(uint256)")),
            ],
        );
        (labs, ocl, tok)
    }

    fn links(repos: &[&Repo], same: bool) -> Vec<ContractLink> {
        let gs: Vec<RepoGraph> = repos.iter().map(|r| r.graph()).collect();
        contract_links(&gs, same)
    }

    fn one(ls: &[ContractLink], line: u32) -> Option<&ContractLink> {
        let hits: Vec<&ContractLink> = ls.iter().filter(|l| l.line == line).collect();
        assert!(hits.len() <= 1, "{hits:?}");
        hits.first().copied()
    }

    #[test]
    fn named_tier_links_to_the_contract_leaf() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        let abi = web.abi(
            "packages/common/src/abis/access-resolver.ts",
            "AccessResolverAbi",
            &[
                "grantRole(bytes32,address,uint8,uint64,bool)",
                "getRole(bytes32,address)",
            ],
            &[],
        );
        let f = web.calls(
            "apps/labs/use-grant-role.ts",
            vec![fcall("grantRole", Some("AccessResolverAbi"), 58)],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        let ls = links(&[&labs, &ocl, &tok, &web], false);
        let l = one(&ls, 58).expect("linked");
        assert_eq!(
            (l.container_name.as_str(), l.tier),
            ("AccessResolver", Tier::Named)
        );
        assert_eq!(l.confidence, CONTRACT_ABI_NAMED_CONFIDENCE);
        assert_eq!(l.provider, 0);
        assert!(l.member_node.is_some());
        assert_eq!(
            l.evidence(),
            "AccessResolver.grantRole <- readContract (apps/labs/use-grant-role.ts:58) via AccessResolverAbi [named]"
        );
    }

    #[test]
    fn getter_field_is_a_provided_member() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/ocl-tokenizer.ts",
            "OclTokenizerAbi",
            &["tokenized(uint256)", "controllerOf(uint256)"],
            &[],
        );
        let f = web.calls(
            "use-lab-token.ts",
            vec![fcall("tokenized", Some("OclTokenizerAbi"), 66)],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        let ls = links(&[&labs, &ocl, &tok, &web], false);
        let l = one(&ls, 66).unwrap();
        assert_eq!(
            (l.container_name.as_str(), l.tier),
            ("OclTokenizer", Tier::Named)
        );
        let member = tok
            .nodes
            .iter()
            .find(|n| Some(n.id) == l.member_node)
            .unwrap();
        assert_eq!(member.metadata["getter"], true);
    }

    #[test]
    fn inherited_member_via_unindexed_bases() {
        let (labs, ocl, tok) = providers();
        let mut infra = Repo::new("infra");
        let abi = infra.abi(
            "utils/labnft-ownership.ts",
            "LABNFT_OWNER_OF_ABI",
            &["ownerOf(uint256)"],
            &[],
        );
        let f = infra.calls(
            "utils/labnft-ownership.ts",
            vec![fcall("ownerOf", Some("LABNFT_OWNER_OF_ABI"), 44)],
        );
        infra.edge(f, abi, EdgeKind::UsesAbi);
        let ls = links(&[&labs, &ocl, &tok, &infra], false);
        let l = one(&ls, 44).unwrap();
        assert_eq!(
            (l.container_name.as_str(), l.tier),
            ("LabNFT", Tier::Inherited)
        );
        assert_eq!(l.confidence, CONTRACT_ABI_INHERITED_CONFIDENCE);
        assert_eq!(l.member_node, None);
        assert_eq!(
            l.bases,
            [
                "Initializable",
                "ERC721",
                "AccessControl",
                "UUPSUpgradeable"
            ]
        );
        assert!(l.evidence().ends_with(
            "[inherited from unindexed base: Initializable, ERC721, AccessControl, UUPSUpgradeable]"
        ));
    }

    /// `LabTokenAbi.wrappedToken`: `LabToken` inherits unindexed ERC-20
    /// bases, but `WrappedLabToken` declares `wrappedToken`, so the inherited
    /// tier would guess. The call stays unlinked; `metadata` still links.
    #[test]
    fn inherited_tier_yields_to_a_contract_that_declares_the_member() {
        let mut tok = Repo::new("ocltokenizer");
        tok.container(
            "src/LabToken.sol",
            NodeKind::Type,
            "LabToken",
            json!({"bases": ["ERC20Upgradeable", "OwnableUpgradeable"]}),
            &[func("metadata", Some("metadata()"))],
        );
        tok.container(
            "src/WrappedLabToken.sol",
            NodeKind::Type,
            "WrappedLabToken",
            json!({"bases": ["ERC20Upgradeable"]}),
            &[getter("wrappedToken", "wrappedToken()")],
        );
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/lab-token.ts",
            "LabTokenAbi",
            &["metadata()", "wrappedToken()"],
            &[],
        );
        let f = web.calls(
            "use-token-contract-reads.ts",
            vec![
                fcall("metadata", Some("LabTokenAbi"), 64),
                fcall("wrappedToken", Some("LabTokenAbi"), 70),
            ],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        let ls = links(&[&tok, &web], false);
        let l = one(&ls, 64).expect("metadata links");
        assert_eq!(
            (l.container_name.as_str(), l.tier),
            ("LabToken", Tier::Named)
        );
        assert!(one(&ls, 70).is_none(), "{ls:?}");
    }

    #[test]
    fn selector_disagreement_rejects() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/rwa-factory.ts",
            "rwaFactoryAbi",
            &["hasRole(bytes32,address)", "create(address)"],
            &[],
        );
        let f = web.calls(
            "use-is-operator.ts",
            vec![fcall("hasRole", Some("rwaFactoryAbi"), 34)],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        // Without an ABI the standard name never links unique either.
        let g = web.calls("loose.ts", vec![fcall("hasRole", None, 35)]);
        let _ = g;
        let ls = links(&[&labs, &ocl, &tok, &web], false);
        assert!(one(&ls, 34).is_none(), "{ls:?}");
        assert!(one(&ls, 35).is_none(), "{ls:?}");
    }

    #[test]
    fn standard_abi_never_links_unnamed() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/erc20.ts",
            "erc20Abi",
            &[
                "balanceOf(address)",
                "transfer(address,uint256)",
                "approve(address,uint256)",
                "decimals()",
            ],
            &[],
        );
        let f = web.calls(
            "use-balance.ts",
            vec![fcall("balanceOf", Some("erc20Abi"), 12)],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        // A token that declares the whole ERC-20 surface itself: without the
        // guard the overlap tier would tie every ERC-20 ABI to it.
        let mut gov = Repo::new("gov");
        gov.container(
            "src/GovToken.sol",
            NodeKind::Type,
            "GovToken",
            json!({}),
            &[
                func("balanceOf", Some("balanceOf(address)")),
                func("transfer", Some("transfer(address,uint256)")),
                func("approve", Some("approve(address,uint256)")),
                func("decimals", Some("decimals()")),
            ],
        );
        assert!(links(&[&labs, &ocl, &tok, &web, &gov], false)
            .iter()
            .all(|l| l.consumer != 3));
    }

    #[test]
    fn external_abi_never_links() {
        let (labs, ocl, tok) = providers();
        let mut infra = Repo::new("infra");
        // `erc20Abi` from viem: named, but no Abi node, no UsesAbi edge.
        infra.calls(
            "utils/contract.ts",
            vec![fcall("tokenize", Some("erc20Abi"), 9)],
        );
        assert!(links(&[&labs, &ocl, &tok, &infra], false)
            .iter()
            .all(|l| l.consumer != 3));
    }

    #[test]
    fn overlap_tier() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        let acct = web.abi(
            "abis/onchain-lab.ts",
            "LabAccountAbi",
            &[
                "execute(address,uint256,bytes,uint8)",
                "isValidSigner(address,bytes)",
                "token()",
            ],
            &[],
        );
        let labs_abi = web.abi(
            "abis/labs.ts",
            "LabsAbi",
            &[
                "ownersOfIpnft(uint256)",
                "getRole(bytes32,address)",
                "unrelated()",
            ],
            &[],
        );
        let f = web.calls(
            "hooks.ts",
            vec![
                fcall("execute", Some("LabAccountAbi"), 1),
                fcall("ownersOfIpnft", Some("LabsAbi"), 2),
            ],
        );
        web.edge(f, acct, EdgeKind::UsesAbi);
        web.edge(f, labs_abi, EdgeKind::UsesAbi);
        let ls = links(&[&labs, &ocl, &tok, &web], false);
        let a = one(&ls, 1).unwrap();
        assert_eq!(
            (a.container_name.as_str(), a.tier, a.confidence),
            ("OnChainLab", Tier::Overlap, CONTRACT_ABI_OVERLAP_CONFIDENCE)
        );
        let b = one(&ls, 2).unwrap();
        assert_eq!(
            (b.container_name.as_str(), b.tier),
            ("AccessResolver", Tier::Overlap)
        );
    }

    #[test]
    fn unique_tier_without_abi() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        web.calls(
            "execute.ts",
            vec![
                fcall("tokenize", None, 3),
                fcall("balanceOf", None, 4),
                fcall("nowhere", None, 5),
            ],
        );
        let ls = links(&[&labs, &ocl, &tok, &web], false);
        let l = one(&ls, 3).unwrap();
        assert_eq!(
            (l.container_name.as_str(), l.tier, l.confidence),
            ("OclTokenizer", Tier::Unique, CONTRACT_ABI_UNIQUE_CONFIDENCE)
        );
        assert!(one(&ls, 4).is_none() && one(&ls, 5).is_none());
    }

    #[test]
    fn consumer_solidity_interface() {
        let (labs, ocl, tok) = providers();
        let ls = links(&[&labs, &ocl, &tok], false);
        let l = ls
            .iter()
            .find(|l| l.abi.as_deref() == Some("ILabNft"))
            .expect("ILabNft links");
        assert_eq!((l.consumer, l.provider), (0, 1));
        assert_eq!(
            (l.container_name.as_str(), l.member.as_deref()),
            ("LabNFT", Some("accountOf"))
        );
        assert_eq!(l.via.as_deref(), Some("interface"));
        // IAccessResolver has an in-repo implementer: it is a provider, never a consumer.
        assert!(!ls
            .iter()
            .any(|l| l.abi.as_deref() == Some("IAccessResolver")));
        // Same-repo allowed: the interface never resolves to itself.
        let same = links(&[&labs, &ocl, &tok], true);
        let l = same
            .iter()
            .find(|l| l.abi.as_deref() == Some("ILabNft"))
            .unwrap();
        assert_eq!(l.container_name, "LabNFT");
    }

    #[test]
    fn abi_only_combined_use_expands_to_parts() {
        let (labs, ocl, tok) = providers();
        let mut infra = Repo::new("infra");
        let lab = infra.abi(
            "assets/abis/LabNFT.json",
            "LabNFT",
            &["accountOf(uint256)", "oclIdOf(uint256)", "ownerOf(uint256)"],
            &[],
        );
        let tokz = infra.abi(
            "assets/abis/OclTokenizer.json",
            "OclTokenizer",
            &[],
            &["Tokenized(uint256,address)"],
        );
        let combined = infra.abi("utils/event.ts", "OCL_COMBINED_ABI", &[], &[]);
        infra.edge(combined, lab, EdgeKind::UsesAbi);
        infra.edge(combined, tokz, EdgeKind::UsesAbi);
        let decode = infra.node(
            NodeKind::Function,
            "typescript:utils/event.ts:function:decodeOclEvent",
            "decodeOclEvent",
            json!({"path": "utils/event.ts"}),
        );
        infra.edge(decode, combined, EdgeKind::UsesAbi);
        let mut c = call(None, ContractMemberKind::Abi, Some("OCL_COMBINED_ABI"), 76);
        c.owner = Some("typescript:utils/event.ts:function:decodeOclEvent".into());
        c.via = Some("decodeEventLog".into());
        infra.calls("utils/event.ts", vec![c]);
        let ls = links(&[&labs, &ocl, &tok, &infra], false);
        let got: Vec<(&str, Tier, f32)> = ls
            .iter()
            .filter(|l| l.line == 76)
            .map(|l| (l.container_name.as_str(), l.tier, l.confidence))
            .collect();
        // LabNFT covers its ABI (ownerOf is a standard name left out); the
        // event-only OclTokenizer ABI has no indexed events to cover.
        assert_eq!(
            got,
            [(
                "LabNFT",
                Tier::Named,
                CONTRACT_ABI_NAMED_CONFIDENCE - ABI_USE_PENALTY
            )]
        );
        assert_eq!(ls[0].consumer_node, Some(decode));
    }

    #[test]
    fn leaf_ambiguous_keeps_interface() {
        let mut chain = Repo::new("chain");
        let ip = chain.container(
            "src/IPermissioner.sol",
            NodeKind::Interface,
            "IPermissioner",
            json!({}),
            &[func("accept", Some("accept(address)"))],
        );
        for n in ["A", "B"] {
            let c = chain.container(
                &format!("src/{n}.sol"),
                NodeKind::Type,
                n,
                json!({}),
                &[func("accept", Some("accept(address)"))],
            );
            chain.edge(c, ip, EdgeKind::Extends);
        }
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/permissioner.ts",
            "IPermissionerAbi",
            &["accept(address)"],
            &[],
        );
        let f = web.calls("p.ts", vec![fcall("accept", Some("IPermissionerAbi"), 4)]);
        web.edge(f, abi, EdgeKind::UsesAbi);
        // `PermissionerAbi`: no `Permissioner` container, so `i`+key names it.
        let bare = web.abi(
            "abis/permissioner-bare.ts",
            "PermissionerAbi",
            &["accept(address)"],
            &[],
        );
        let g = web.calls("q.ts", vec![fcall("accept", Some("PermissionerAbi"), 5)]);
        web.edge(g, bare, EdgeKind::UsesAbi);
        let ls = links(&[&chain, &web], false);
        for line in [4, 5] {
            let l = one(&ls, line).unwrap();
            assert_eq!(
                (l.container_name.as_str(), l.tier),
                ("IPermissioner", Tier::Named)
            );
        }
    }

    #[test]
    fn named_key_wins_over_an_interface_with_two_implementers() {
        // ocltokenizer: ILabToken has two implementers, so its leaf stays
        // ILabToken; `LabTokenAbi` still names LabToken.
        let mut tok = Repo::new("ocltokenizer");
        let surface = || {
            [
                func("metadata", Some("metadata()")),
                func("wrappedToken", Some("wrappedToken()")),
            ]
        };
        let ilt = tok.container(
            "src/ILabToken.sol",
            NodeKind::Interface,
            "ILabToken",
            json!({}),
            &surface(),
        );
        for n in ["LabToken", "WrappedLabToken"] {
            let c = tok.container(
                &format!("src/{n}.sol"),
                NodeKind::Type,
                n,
                json!({}),
                &surface(),
            );
            tok.edge(c, ilt, EdgeKind::Extends);
        }
        let mut web = Repo::new("web");
        let abi = web.abi(
            "abis/lab-token.ts",
            "LabTokenAbi",
            &["metadata()", "wrappedToken()"],
            &[],
        );
        let f = web.calls(
            "use-token-contract-reads.ts",
            vec![
                fcall("metadata", Some("LabTokenAbi"), 64),
                fcall("wrappedToken", Some("LabTokenAbi"), 70),
            ],
        );
        web.edge(f, abi, EdgeKind::UsesAbi);
        let ls = links(&[&tok, &web], false);
        for line in [64, 70] {
            let l = one(&ls, line).expect("linked");
            assert_eq!(
                (l.container_name.as_str(), l.tier, l.confidence),
                ("LabToken", Tier::Named, CONTRACT_ABI_NAMED_CONFIDENCE)
            );
        }
    }

    #[test]
    fn consumer_interface_never_resolves_to_its_neighbour() {
        // onchainlabs OnChainLabRouter.sol declares the OpenZeppelin beacon it
        // calls (IBeacon) next to the router, which has its own implementation().
        let mut chain = Repo::new("onchainlabs");
        let router = "src/core/OnChainLabRouter.sol";
        chain.container(
            router,
            NodeKind::Interface,
            "IBeacon",
            json!({}),
            &[func("implementation", Some("implementation()"))],
        );
        // Two functions the neighbour also has: the overlap tier's shape.
        chain.container(
            router,
            NodeKind::Interface,
            "IRouterLike",
            json!({}),
            &[
                func("implementation", Some("implementation()")),
                func("route", Some("route(bytes4)")),
            ],
        );
        chain.container(
            router,
            NodeKind::Type,
            "OnChainLabRouter",
            json!({}),
            &[
                func("implementation", Some("implementation()")),
                func("route", Some("route(bytes4)")),
            ],
        );
        // Same repo, other file: a lone name match is no evidence either.
        chain.container(
            "src/core/IRegistryView.sol",
            NodeKind::Interface,
            "IRegistryView",
            json!({}),
            &[func("lookupLab", Some("lookupLab(uint256)"))],
        );
        chain.container(
            "src/core/LabDirectory.sol",
            NodeKind::Type,
            "LabDirectory",
            json!({}),
            &[func("lookupLab", Some("lookupLab(uint256)"))],
        );
        for same in [false, true] {
            let ls = links(&[&chain], same);
            assert!(ls.is_empty(), "same={same}: {ls:?}");
        }
        // Another repo's contract still resolves the interface.
        let mut beacons = Repo::new("beacons");
        beacons.container(
            "src/LabBeacon.sol",
            NodeKind::Type,
            "LabBeacon",
            json!({}),
            &[func("implementation", Some("implementation()"))],
        );
        let ls = links(&[&chain, &beacons], true);
        let l = ls
            .iter()
            .find(|l| l.abi.as_deref() == Some("IBeacon"))
            .expect("cross-repo link");
        assert_eq!(
            (l.container_name.as_str(), l.provider, l.tier),
            ("LabBeacon", 1, Tier::Unique)
        );
    }

    #[test]
    fn test_paths_skipped() {
        let (labs, ocl, tok) = providers();
        let mut web = Repo::new("web");
        web.calls(
            "src/tokenize-call.test.ts",
            vec![fcall("tokenize", None, 3)],
        );
        web.calls("e2e/live/flow.ts", vec![fcall("tokenize", None, 4)]);
        assert!(links(&[&labs, &ocl, &tok, &web], false)
            .iter()
            .all(|l| l.consumer != 3));
    }

    #[test]
    fn same_repo_only_when_allowed() {
        let (labs, ocl, tok) = providers();
        let mut tok = tok;
        tok.calls("script/use.ts", vec![fcall("tokenize", None, 8)]);
        assert!(links(&[&labs, &ocl, &tok], false)
            .iter()
            .all(|l| l.consumer != l.provider));
        let same = links(&[&labs, &ocl, &tok], true);
        assert!(same
            .iter()
            .any(|l| l.line == 8 && l.container_name == "OclTokenizer"));
    }

    #[test]
    fn name_and_arity_fallback_without_signatures() {
        // Before the Solidity side writes abi_signature: params arity, else name.
        let mut chain = Repo::new("chain");
        chain.container(
            "src/Registry.sol",
            NodeKind::Type,
            "MoleculeOclDidRegistry",
            json!({}),
            &[(
                NodeKind::Method,
                "getActiveDid",
                json!({"visibility": "external", "params": [{}, {}, {}]}),
            )],
        );
        let mut infra = Repo::new("infra");
        let abi = infra.abi(
            "scripts/link.ts",
            "REGISTRY_ABI",
            &["getActiveDid(bytes32,bytes32,bytes32)", "paused()"],
            &[],
        );
        let wrong = infra.abi(
            "scripts/other.ts",
            "OTHER_ABI",
            &["getActiveDid(bytes32)"],
            &[],
        );
        let f = infra.calls(
            "scripts/link.ts",
            vec![fcall("getActiveDid", Some("REGISTRY_ABI"), 816)],
        );
        infra.edge(f, abi, EdgeKind::UsesAbi);
        let g = infra.calls(
            "scripts/other.ts",
            vec![fcall("getActiveDid", Some("OTHER_ABI"), 5)],
        );
        infra.edge(g, wrong, EdgeKind::UsesAbi);
        let ls = links(&[&chain, &infra], false);
        let l = one(&ls, 816).unwrap();
        assert_eq!(
            (l.container_name.as_str(), l.tier),
            ("MoleculeOclDidRegistry", Tier::Unique)
        );
        assert!(one(&ls, 5).is_none(), "arity 1 ≠ 3");
    }
}

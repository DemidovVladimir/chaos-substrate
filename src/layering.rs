//! Architectural **layer** classification — "where does this code sit in the
//! user's journey?"
//!
//! `chaos_components` orders the parts of an area the way a person actually meets
//! a feature: start at what they touch (a screen, a CLI, a page), then the API
//! that serves it, then the logic behind it, then the contracts/infra at the
//! bottom. Code-level "who imports whom" can't give that order — it doesn't even
//! cross a repo or language boundary — so we derive a layer for each piece from
//! signals every file already has: its path and node kind.
//!
//! Deterministic and dependency-free: same inputs ⇒ same layer. No embeddings,
//! no network, no guessing beyond a fixed rule set.

/// Where a piece of code sits in the journey, outermost (`Entry`) to innermost
/// (`Foundation`). `Unknown` means we couldn't tell — shown in the middle and
/// labelled honestly rather than faked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// What a user/actor touches first: UI, client, CLI, pages, apps.
    Entry,
    /// The API surface those entry points call: resolvers, controllers, routes.
    Interface,
    /// Business logic and data access: services, repositories, domain.
    Core,
    /// The base everything rests on: smart contracts, infra, config, low-level types.
    Foundation,
    /// Couldn't be determined from path/kind.
    Unknown,
}

impl Layer {
    /// Stable label used as the component "role" and in CSS.
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Entry => "entry",
            Layer::Interface => "interface",
            Layer::Core => "core",
            Layer::Foundation => "foundation",
            Layer::Unknown => "unknown",
        }
    }

    /// Journey rank for ordering: smaller = read earlier (more outward).
    /// `Unknown` sits between core and foundation so it never jumps the queue.
    pub fn rank(self) -> u8 {
        match self {
            Layer::Entry => 0,
            Layer::Interface => 1,
            Layer::Core => 2,
            Layer::Unknown => 3,
            Layer::Foundation => 4,
        }
    }
}

/// Classify a single file path into a layer. Checks the most *intentful*
/// directory names first (a `repositories/` folder is data-access even when it
/// lives under `infra/lambda/`), so segment order resolves the common conflicts.
pub fn classify_path(path: &str, kind: &str) -> Layer {
    if path.is_empty() {
        // No path — fall back to the node kind for the few signals it carries.
        return match kind {
            "deployment_resource" => Layer::Foundation,
            _ => Layer::Unknown,
        };
    }
    let p = path.to_ascii_lowercase();
    let segs: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let has = |name: &str| segs.contains(&name);

    // 1. Core — explicit logic/data-access folders (checked first: most specific).
    if has("repositories")
        || has("repository")
        || has("services")
        || has("service")
        || has("domain")
        || has("usecases")
        || has("use-cases")
        || has("business")
        || has("logic")
        || has("models")
    {
        return Layer::Core;
    }
    // 2. Interface — the API surface entry points call.
    if has("api")
        || has("resolvers")
        || has("resolver")
        || has("controllers")
        || has("controller")
        || has("handlers")
        || has("handler")
        || has("routes")
        || has("graphql")
        || has("endpoints")
        || p.contains("/pages/api/")
    {
        return Layer::Interface;
    }
    // 3. Entry — what a user/actor touches first.
    if has("apps")
        || has("app")
        || has("client")
        || has("clients")
        || has("frontend")
        || has("ui")
        || has("web")
        || has("cli")
        || has("cmd")
        || has("bin")
        || has("pages")
        || has("screens")
        || has("views")
    {
        return Layer::Entry;
    }
    // 4. Foundation — contracts, infra, and low-level building blocks.
    if p.ends_with(".sol")
        || p.ends_with(".tf")
        || kind == "deployment_resource"
        || has("contracts")
        || has("contract")
        || has("infra")
        || has("infrastructure")
        || has("cdk")
        || has("terraform")
        || has("stacks")
        || has("stack")
        || has("deploy")
        || has("deployment")
        || has("config")
        || has("types")
        || has("constants")
        || has("abi")
        || has("abis")
    {
        return Layer::Foundation;
    }
    Layer::Unknown
}

/// Generic words a person tacks onto a layer name ("client features", "core
/// layer") that carry no layer signal of their own — stripped before matching so
/// the meaningful token is what decides.
const LAYER_FILLER: &[&str] = &[
    "features", "feature", "layer", "layers", "side", "code", "stuff", "things", "part", "parts",
    "level", "tier", "the", "all",
];

/// Interpret a short query as a request for one journey layer — the engine behind
/// `chaos features client` meaning "every entry-layer feature". Conservative on
/// purpose: it answers only when, after dropping filler words, exactly ONE token
/// remains and that token is a known layer synonym. A descriptive phrase like
/// "access control" or "payment service" has more than one meaningful token, so
/// it returns `None` and the caller routes it to a topic/semantic match instead.
pub fn layer_from_query(query: &str) -> Option<Layer> {
    let tokens: Vec<String> = query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !LAYER_FILLER.contains(&t.as_str()))
        .collect();
    let [word] = tokens.as_slice() else {
        return None;
    };
    layer_for_word(word)
}

/// Map a single lowercase word to a layer. Shares the vocabulary `classify_path`
/// keys on, plus the canonical layer names so `--layer entry` round-trips.
fn layer_for_word(word: &str) -> Option<Layer> {
    const ENTRY: &[&str] = &[
        "entry",
        "client",
        "clients",
        "ui",
        "frontend",
        "web",
        "webapp",
        "app",
        "apps",
        "cli",
        "cmd",
        "page",
        "pages",
        "screen",
        "screens",
        "view",
        "views",
        "portal",
        "dashboard",
        "console",
    ];
    const INTERFACE: &[&str] = &[
        "interface",
        "api",
        "apis",
        "resolver",
        "resolvers",
        "controller",
        "controllers",
        "handler",
        "handlers",
        "route",
        "routes",
        "endpoint",
        "endpoints",
        "graphql",
        "rest",
        "gateway",
    ];
    const CORE: &[&str] = &[
        "core",
        "service",
        "services",
        "logic",
        "business",
        "domain",
        "repository",
        "repositories",
        "usecase",
        "usecases",
        "model",
        "models",
    ];
    const FOUNDATION: &[&str] = &[
        "foundation",
        "contract",
        "contracts",
        "infra",
        "infrastructure",
        "config",
        "types",
        "constants",
        "abi",
        "abis",
        "terraform",
        "cdk",
        "stack",
        "stacks",
        "deploy",
        "deployment",
        "onchain",
        "solidity",
        "sol",
    ];
    if ENTRY.contains(&word) {
        Some(Layer::Entry)
    } else if INTERFACE.contains(&word) {
        Some(Layer::Interface)
    } else if CORE.contains(&word) {
        Some(Layer::Core)
    } else if FOUNDATION.contains(&word) {
        Some(Layer::Foundation)
    } else {
        None
    }
}

/// Derive a community's layer from its members `(name, kind, path)`. Plurality
/// vote across members; ties break toward the more *outward* layer so the
/// community is read a little earlier rather than buried. `Unknown` members are
/// ignored unless they are all there is.
pub fn classify_community(members: &[(String, String, String)]) -> Layer {
    // Tally, indexed by Layer::rank() so iteration is deterministic.
    let mut counts = [0usize; 5];
    let bucket = |l: Layer| l.rank() as usize;
    let mut any_known = false;
    for (_name, kind, path) in members {
        let layer = classify_path(path, kind);
        if layer != Layer::Unknown {
            any_known = true;
        }
        counts[bucket(layer)] += 1;
    }
    if !any_known {
        return Layer::Unknown;
    }
    // Pick the highest count among the *known* layers; tie → smallest rank.
    let candidates = [
        Layer::Entry,
        Layer::Interface,
        Layer::Core,
        Layer::Foundation,
    ];
    let mut best = Layer::Unknown;
    let mut best_count = 0usize;
    for layer in candidates {
        let c = counts[bucket(layer)];
        if c > best_count {
            best_count = c;
            best = layer;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_map_to_journey_layers() {
        assert_eq!(
            classify_path("apps/screener/src/App.tsx", "function"),
            Layer::Entry
        );
        assert_eq!(
            classify_path("packages/client/ui/Button.tsx", "function"),
            Layer::Entry
        );
        assert_eq!(
            classify_path(
                "desci-infra/lambda/appsync-resolver/resolvers/x.ts",
                "function"
            ),
            Layer::Interface
        );
        // A repository under infra/lambda is data-access (core), not infra.
        assert_eq!(
            classify_path(
                "desci-infra/lambda/common/repositories/ocl-repository.ts",
                "function"
            ),
            Layer::Core
        );
        assert_eq!(
            classify_path("onchainlabs/src/types/Constants.sol", "struct"),
            Layer::Foundation
        );
        assert_eq!(
            classify_path(
                "desci-ecosystem/packages/common/src/config/domains/ocl.ts",
                "function"
            ),
            Layer::Foundation
        );
        assert_eq!(
            classify_path("infra/cdk/stacks/api-stack.ts", "function"),
            Layer::Foundation
        );
        assert_eq!(classify_path("src/widget.ts", "function"), Layer::Unknown);
    }

    #[test]
    fn empty_path_uses_kind() {
        assert_eq!(classify_path("", "deployment_resource"), Layer::Foundation);
        assert_eq!(classify_path("", "function"), Layer::Unknown);
    }

    #[test]
    fn community_layer_is_plurality_outermost_on_tie() {
        // Two core, one foundation → core.
        let members = vec![
            ("A".into(), "function".into(), "services/a.ts".into()),
            ("B".into(), "function".into(), "services/b.ts".into()),
            ("C".into(), "struct".into(), "contracts/C.sol".into()),
        ];
        assert_eq!(classify_community(&members), Layer::Core);

        // Tie entry vs foundation → entry (more outward).
        let tie = vec![
            ("A".into(), "function".into(), "apps/web/a.tsx".into()),
            ("B".into(), "struct".into(), "contracts/B.sol".into()),
        ];
        assert_eq!(classify_community(&tie), Layer::Entry);

        // All unknown → unknown.
        let unknown = vec![("A".into(), "function".into(), "src/x.ts".into())];
        assert_eq!(classify_community(&unknown), Layer::Unknown);
    }

    #[test]
    fn layer_query_maps_single_known_word_and_strips_filler() {
        // Bare synonyms.
        assert_eq!(layer_from_query("client"), Some(Layer::Entry));
        assert_eq!(layer_from_query("UI"), Some(Layer::Entry));
        assert_eq!(layer_from_query("api"), Some(Layer::Interface));
        assert_eq!(layer_from_query("contracts"), Some(Layer::Foundation));
        // Canonical layer names round-trip (used by --layer).
        assert_eq!(layer_from_query("core"), Some(Layer::Core));
        assert_eq!(layer_from_query("foundation"), Some(Layer::Foundation));
        // Filler words are dropped: still one meaningful token.
        assert_eq!(layer_from_query("client features"), Some(Layer::Entry));
        assert_eq!(layer_from_query("the core layer"), Some(Layer::Core));
        // Multi-word descriptive phrases are NOT layers → routed to topic match.
        assert_eq!(layer_from_query("access control"), None);
        assert_eq!(layer_from_query("payment service flow"), None);
        // Unknown single word → None.
        assert_eq!(layer_from_query("payments"), None);
    }
}

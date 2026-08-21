//! `chaos sui-migration-impact` — the Sui migration impact report.
//!
//! Answers *"if this project moves to Sui, which existing features are
//! affected, which Sui primitives map to them, and what should be reviewed
//! first?"* — from the persisted index alone, plus the compiled-in
//! [`crate::sui_docs`] profile of official Sui / Walrus / Seal documentation.
//!
//! The pipeline is deliberately deterministic and embedder-free (like
//! `chaos stack`):
//!
//! 1. **Source-stack detection (R2)** — manifest-declared dependencies
//!    ([`Storage::stack_dependencies`]), lexical chunk scans
//!    ([`Storage::scan_chunks`] ILIKE prefilter + exact substring match in
//!    Rust), Solidity contract definitions
//!    ([`Storage::solidity_contract_nodes`]), and bounded disk probes for
//!    well-known toolchain configs (foundry.toml, hardhat.config.*,
//!    Anchor.toml, …) become [`Signal`]s with per-signal evidence files.
//! 2. **Feature impact (R3)** — evidence files map to L1 communities via
//!    [`Storage::dominant_community_for_files`]; each affected feature carries
//!    the migration areas, evidence, and top symbols. Prior generated feature
//!    pages are correlated via `load_feature_matches`.
//! 3. **Migration mappings** — signals trigger entries of the static
//!    [`MAPPINGS`] table (EVM/Solana pattern → Sui concept), every entry citing
//!    official docs by id. Evidence-triggered only: no signal, no claim.
//! 4. **Storage (R4) & Seal classification** — storage flows are classified
//!    into Walrus / Walrus+Seal / keep-offchain / review buckets, and
//!    access-control candidates state explicitly when Seal is NOT needed.
//!
//! Like the other surfacing tools it ALWAYS writes an interactive HTML report
//! (default `docs/features_memory/sui-migration-impact.html`, manifest embedded
//! under id="chaos-sui-migration-manifest") and returns a COMPACT JSON summary
//! (capped lists with `*_omitted` counts) with provenance breadcrumbs
//! throughout. It promises an impact map, not an automatic migration: no Move
//! code is generated and no correctness claims are made.

use crate::{
    export_util::{features_memory_dir, resolve_indexed_repo},
    feature_context::load_feature_matches,
    provenance::{source, Breadcrumb},
    storage::{StackDependencyRow, Storage},
    sui_docs,
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Inline caps for the compact MCP/CLI return. The HTML keeps every entry;
/// omission counts are lifted to `*_omitted` fields.
const MAX_COMPACT_SIGNALS: usize = 18;
const MAX_COMPACT_MAPPINGS: usize = 16;
const MAX_COMPACT_STORAGE: usize = 8;
const MAX_COMPACT_ACCESS: usize = 6;
const MAX_COMPACT_RELATED: usize = 3;
/// Evidence files kept per signal / feature (full set sizes are reported).
const MAX_EVIDENCE_FILES: usize = 6;
const TOP_SYMBOLS_PER_FEATURE: i64 = 6;
const CHUNK_SCAN_LIMIT: i64 = 4000;
const CONFIG_PROBE_MAX_DEPTH: usize = 3;

/// Review order across migration areas — contracts first (everything consumes
/// their types), docs last.
const AREA_ORDER: [&str; 7] = [
    "contracts",
    "client",
    "storage",
    "access",
    "indexer",
    "infra",
    "docs",
];

#[derive(Debug, Default, Clone)]
pub struct SuiMigrationOptions {
    /// `ethereum` | `solana` | `mixed` | `auto` (default).
    pub source: Option<String>,
    pub output_html: Option<PathBuf>,
    pub features_dir: Option<PathBuf>,
    /// Max affected features in the compact return (default 12).
    pub limit: usize,
}

// ---------------------------------------------------------------------------
// Detection rule tables. These describe EVIDENCE the extractor persists
// (dependency names, code literals, toolchain config filenames) — the same
// kind of extractor-side pattern knowledge as `chaos stack`'s manifest section
// names, not query-side phrasing lists.
// ---------------------------------------------------------------------------

/// A dependency-name rule. `pattern` ending in `*` is a prefix match.
struct DepRule {
    /// `npm` | `cargo` | `*` (either).
    ecosystem: &'static str,
    pattern: &'static str,
    chain: Option<&'static str>,
    area: &'static str,
    label: &'static str,
    mappings: &'static [&'static str],
}

const DEP_RULES: &[DepRule] = &[
    // ---- Ethereum ----------------------------------------------------------
    DepRule {
        ecosystem: "npm",
        pattern: "hardhat",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat toolchain",
        mappings: &["evm-toolchain"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@nomicfoundation/*",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat toolchain",
        mappings: &["evm-toolchain"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@nomiclabs/*",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat toolchain",
        mappings: &["evm-toolchain"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "truffle",
        chain: Some("ethereum"),
        area: "infra",
        label: "Truffle toolchain",
        mappings: &["evm-toolchain"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@openzeppelin/*",
        chain: Some("ethereum"),
        area: "contracts",
        // Library presence alone implies role patterns; upgradeable/proxy
        // mappings only trigger on actual code evidence (chunk rules).
        label: "OpenZeppelin contracts library",
        mappings: &["evm-access-control"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "ethers",
        chain: Some("ethereum"),
        area: "client",
        label: "ethers.js client SDK",
        mappings: &["evm-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "viem",
        chain: Some("ethereum"),
        area: "client",
        label: "viem client SDK",
        mappings: &["evm-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "wagmi",
        chain: Some("ethereum"),
        area: "client",
        label: "wagmi React hooks",
        mappings: &["evm-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@wagmi/*",
        chain: Some("ethereum"),
        area: "client",
        label: "wagmi React hooks",
        mappings: &["evm-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "web3",
        chain: Some("ethereum"),
        area: "client",
        label: "web3.js client SDK",
        mappings: &["evm-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@rainbow-me/*",
        chain: Some("ethereum"),
        area: "client",
        label: "EVM wallet connector",
        mappings: &["wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@walletconnect/*",
        chain: Some("ethereum"),
        area: "client",
        label: "EVM wallet connector",
        mappings: &["wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@graphprotocol/*",
        chain: Some("ethereum"),
        area: "indexer",
        label: "The Graph subgraph tooling",
        mappings: &["events-indexing"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "alloy*",
        chain: Some("ethereum"),
        area: "client",
        label: "Alloy EVM Rust SDK",
        mappings: &["evm-client-sdk"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "ethers",
        chain: Some("ethereum"),
        area: "client",
        label: "ethers-rs client SDK",
        mappings: &["evm-client-sdk"],
    },
    // ---- Solana ------------------------------------------------------------
    DepRule {
        ecosystem: "npm",
        pattern: "@solana/spl-token",
        chain: Some("solana"),
        area: "client",
        label: "SPL Token JS client",
        mappings: &["spl-token"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@solana/*",
        chain: Some("solana"),
        area: "client",
        label: "Solana JS SDK",
        mappings: &["solana-client-sdk", "wallets"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@coral-xyz/anchor",
        chain: Some("solana"),
        area: "client",
        label: "Anchor JS client",
        mappings: &["anchor"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@project-serum/anchor",
        chain: Some("solana"),
        area: "client",
        label: "Anchor JS client",
        mappings: &["anchor"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@metaplex-foundation/*",
        chain: Some("solana"),
        area: "client",
        label: "Metaplex SDK",
        mappings: &["metaplex-metadata"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "helius-sdk",
        chain: Some("solana"),
        area: "indexer",
        label: "Helius indexing SDK",
        mappings: &["events-indexing"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "anchor-lang",
        chain: Some("solana"),
        area: "contracts",
        label: "Anchor framework program",
        mappings: &["anchor", "pda-objects", "solana-rent", "program-upgrade"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "anchor-spl",
        chain: Some("solana"),
        area: "contracts",
        label: "Anchor SPL integrations",
        mappings: &["spl-token"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "solana-program",
        chain: Some("solana"),
        area: "contracts",
        label: "Native Solana program",
        mappings: &[
            "solana-program",
            "pda-objects",
            "solana-rent",
            "program-upgrade",
        ],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "solana-sdk",
        chain: Some("solana"),
        area: "client",
        label: "Solana Rust SDK",
        mappings: &["solana-client-sdk"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "spl-token",
        chain: Some("solana"),
        area: "contracts",
        label: "SPL Token program usage",
        mappings: &["spl-token"],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "mpl-token-metadata",
        chain: Some("solana"),
        area: "contracts",
        label: "Metaplex token metadata",
        mappings: &["metaplex-metadata"],
    },
    // ---- Already Sui ---------------------------------------------------------
    DepRule {
        ecosystem: "npm",
        pattern: "@mysten/*",
        chain: Some("sui"),
        area: "client",
        label: "Mysten Labs SDK (already Sui-aware)",
        mappings: &[],
    },
    DepRule {
        ecosystem: "cargo",
        pattern: "sui-sdk",
        chain: Some("sui"),
        area: "client",
        label: "Sui Rust SDK (already Sui-aware)",
        mappings: &[],
    },
    // ---- Storage services ------------------------------------------------------
    DepRule {
        ecosystem: "npm",
        pattern: "ipfs-http-client",
        chain: None,
        area: "storage",
        label: "IPFS client",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "kubo-rpc-client",
        chain: None,
        area: "storage",
        label: "IPFS client",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "helia",
        chain: None,
        area: "storage",
        label: "IPFS client",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@helia/*",
        chain: None,
        area: "storage",
        label: "IPFS client",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@pinata/*",
        chain: None,
        area: "storage",
        label: "Pinata pinning service",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "pinata*",
        chain: None,
        area: "storage",
        label: "Pinata pinning service",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "nft.storage",
        chain: None,
        area: "storage",
        label: "nft.storage pinning service",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "web3.storage",
        chain: None,
        area: "storage",
        label: "web3.storage pinning service",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@web3-storage/*",
        chain: None,
        area: "storage",
        label: "web3.storage pinning service",
        mappings: &["ipfs-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "arweave",
        chain: None,
        area: "storage",
        label: "Arweave/Irys storage",
        mappings: &["arweave-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@irys/*",
        chain: None,
        area: "storage",
        label: "Arweave/Irys storage",
        mappings: &["arweave-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "arbundles",
        chain: None,
        area: "storage",
        label: "Arweave/Irys storage",
        mappings: &["arweave-walrus"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@aws-sdk/client-s3",
        chain: None,
        area: "storage",
        label: "S3 object storage client",
        mappings: &["s3-review"],
    },
    // The v2 monolith is the WHOLE AWS SDK — S3 use is possible, not proven,
    // so the label must not claim more than the dependency shows.
    DepRule {
        ecosystem: "npm",
        pattern: "aws-sdk",
        chain: None,
        area: "storage",
        label: "AWS SDK usage (S3-capable, service unconfirmed)",
        mappings: &["s3-review"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "minio",
        chain: None,
        area: "storage",
        label: "S3-compatible object storage client",
        mappings: &["s3-review"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "@google-cloud/storage",
        chain: None,
        area: "storage",
        label: "GCS object storage client",
        mappings: &["s3-review"],
    },
    // ---- Access control / encryption ----------------------------------------------
    DepRule {
        ecosystem: "npm",
        pattern: "@lit-protocol/*",
        chain: None,
        area: "access",
        label: "Lit Protocol token-gating",
        mappings: &["gated-content-seal"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "openpgp",
        chain: None,
        area: "access",
        label: "Client-side encryption library",
        mappings: &["encrypted-content-seal"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "eciesjs",
        chain: None,
        area: "access",
        label: "Client-side encryption library",
        mappings: &["encrypted-content-seal"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "tweetnacl",
        chain: None,
        area: "access",
        label: "Client-side encryption library",
        mappings: &["encrypted-content-seal"],
    },
    DepRule {
        ecosystem: "npm",
        pattern: "libsodium-wrappers",
        chain: None,
        area: "access",
        label: "Client-side encryption library",
        mappings: &["encrypted-content-seal"],
    },
];

/// A chunk-content rule: `needle` is matched case-insensitively against
/// indexed chunk content (SQL ILIKE prefilter, exact substring in Rust).
struct ChunkRule {
    needle: &'static str,
    chain: Option<&'static str>,
    area: &'static str,
    label: &'static str,
    mappings: &'static [&'static str],
}

const CHUNK_RULES: &[ChunkRule] = &[
    // ---- Ethereum ----------------------------------------------------------
    ChunkRule {
        needle: "erc721",
        chain: Some("ethereum"),
        area: "contracts",
        label: "ERC-721 usage",
        mappings: &["erc721", "nft-metadata-walrus"],
    },
    ChunkRule {
        needle: "erc1155",
        chain: Some("ethereum"),
        area: "contracts",
        label: "ERC-1155 usage",
        mappings: &["erc721"],
    },
    ChunkRule {
        needle: "erc20",
        chain: Some("ethereum"),
        area: "contracts",
        label: "ERC-20 usage",
        mappings: &["erc20"],
    },
    ChunkRule {
        needle: "onlyowner",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Ownable access control",
        mappings: &["evm-access-control"],
    },
    // Anchored to the OZ import path / inheritance so client-side phrases like
    // Lit's `accessControlConditions` don't read as Solidity role evidence.
    ChunkRule {
        needle: "accesscontrol.sol",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Role-based access control",
        mappings: &["evm-access-control"],
    },
    ChunkRule {
        needle: "is accesscontrol",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Role-based access control",
        mappings: &["evm-access-control"],
    },
    ChunkRule {
        needle: "delegatecall",
        chain: Some("ethereum"),
        area: "contracts",
        label: "delegatecall / proxy pattern",
        mappings: &["evm-upgrades"],
    },
    ChunkRule {
        needle: "upgradeable",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Upgradeable contract pattern",
        mappings: &["evm-upgrades"],
    },
    ChunkRule {
        needle: "multicall",
        chain: Some("ethereum"),
        area: "client",
        label: "Multicall batching",
        mappings: &["multicall-ptb"],
    },
    ChunkRule {
        needle: "tokenuri",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Token metadata URI",
        mappings: &["nft-metadata-walrus", "erc721"],
    },
    ChunkRule {
        needle: "baseuri",
        chain: Some("ethereum"),
        area: "contracts",
        label: "Token metadata URI",
        mappings: &["nft-metadata-walrus"],
    },
    // Anchored to The Graph's domain — the bare word "subgraph" is a common
    // graph-theory term and would mislabel non-Web3 code as indexer evidence
    // (subgraph.yaml presence is caught by the config probe instead).
    ChunkRule {
        needle: "thegraph.com",
        chain: Some("ethereum"),
        area: "indexer",
        label: "The Graph endpoint references",
        mappings: &["events-indexing"],
    },
    // ---- Solana ------------------------------------------------------------
    ChunkRule {
        needle: "declare_id!",
        chain: Some("solana"),
        area: "contracts",
        label: "Solana program id declaration",
        mappings: &["solana-program", "program-upgrade", "solana-rent"],
    },
    ChunkRule {
        needle: "#[program]",
        chain: Some("solana"),
        area: "contracts",
        label: "Anchor program module",
        mappings: &["anchor", "pda-objects"],
    },
    ChunkRule {
        needle: "find_program_address",
        chain: Some("solana"),
        area: "contracts",
        label: "PDA derivation",
        mappings: &["pda-objects"],
    },
    ChunkRule {
        needle: "invoke_signed",
        chain: Some("solana"),
        area: "contracts",
        label: "CPI with signer seeds",
        mappings: &["cpi-ptb"],
    },
    ChunkRule {
        needle: "cpicontext",
        chain: Some("solana"),
        area: "contracts",
        label: "Anchor CPI calls",
        mappings: &["cpi-ptb"],
    },
    ChunkRule {
        needle: "lamports",
        chain: Some("solana"),
        area: "contracts",
        label: "Lamports / rent handling",
        mappings: &["solana-rent"],
    },
    // ---- Storage URIs ----------------------------------------------------------
    ChunkRule {
        needle: "ipfs://",
        chain: None,
        area: "storage",
        label: "ipfs:// URIs",
        mappings: &["ipfs-walrus"],
    },
    ChunkRule {
        needle: "ipfs.io/ipfs",
        chain: None,
        area: "storage",
        label: "IPFS gateway URLs",
        mappings: &["ipfs-walrus"],
    },
    ChunkRule {
        needle: "cloudflare-ipfs",
        chain: None,
        area: "storage",
        label: "IPFS gateway URLs",
        mappings: &["ipfs-walrus"],
    },
    ChunkRule {
        needle: "gateway.pinata",
        chain: None,
        area: "storage",
        label: "Pinata gateway URLs",
        mappings: &["ipfs-walrus"],
    },
    ChunkRule {
        needle: "ar://",
        chain: None,
        area: "storage",
        label: "ar:// URIs",
        mappings: &["arweave-walrus"],
    },
    ChunkRule {
        needle: "arweave.net",
        chain: None,
        area: "storage",
        label: "Arweave gateway URLs",
        mappings: &["arweave-walrus"],
    },
    ChunkRule {
        needle: "s3.amazonaws.com",
        chain: None,
        area: "storage",
        label: "S3 URLs",
        mappings: &["s3-review"],
    },
    // ---- Access / encryption ------------------------------------------------------
    ChunkRule {
        needle: "encrypt",
        chain: None,
        area: "access",
        label: "Encryption usage in code",
        mappings: &["encrypted-content-seal"],
    },
];

/// A well-known toolchain config filename probed on disk (these files are not
/// indexed as content, so presence is the honest signal we can report).
struct ConfigProbe {
    file_name: &'static str,
    chain: Option<&'static str>,
    area: &'static str,
    label: &'static str,
    mappings: &'static [&'static str],
}

const CONFIG_PROBES: &[ConfigProbe] = &[
    ConfigProbe {
        file_name: "foundry.toml",
        chain: Some("ethereum"),
        area: "infra",
        label: "Foundry toolchain config",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "hardhat.config.ts",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat config",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "hardhat.config.js",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat config",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "hardhat.config.cjs",
        chain: Some("ethereum"),
        area: "infra",
        label: "Hardhat config",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "truffle-config.js",
        chain: Some("ethereum"),
        area: "infra",
        label: "Truffle config",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "remappings.txt",
        chain: Some("ethereum"),
        area: "infra",
        label: "Solidity import remappings",
        mappings: &["evm-toolchain"],
    },
    ConfigProbe {
        file_name: "Anchor.toml",
        chain: Some("solana"),
        area: "infra",
        label: "Anchor workspace config",
        mappings: &["anchor"],
    },
    ConfigProbe {
        file_name: "subgraph.yaml",
        chain: Some("ethereum"),
        area: "indexer",
        label: "The Graph subgraph manifest",
        mappings: &["events-indexing"],
    },
    ConfigProbe {
        file_name: "Move.toml",
        chain: Some("sui"),
        area: "contracts",
        label: "Move package (already targeting Move)",
        mappings: &[],
    },
];

/// One migration mapping: a detected source-chain pattern and the Sui concept
/// that replaces it, citing the official docs that back the claim. Entries are
/// only shown when evidence triggers them.
struct MigrationMapping {
    id: &'static str,
    /// `ethereum` | `solana` | `any`.
    source_chain: &'static str,
    area: &'static str,
    source_pattern: &'static str,
    sui_concept: &'static str,
    notes: &'static str,
    docs: &'static [&'static str],
    /// `rethink` (architecture changes) | `rewrite` (same shape, new code) |
    /// `adapt` (mechanical) | `review` (decision needed).
    effort: &'static str,
}

const MAPPINGS: &[MigrationMapping] = &[
    // ---- Ethereum → Sui -----------------------------------------------------
    MigrationMapping {
        id: "solidity-move", source_chain: "ethereum", area: "contracts",
        source_pattern: "Solidity contracts & storage mappings",
        sui_concept: "Move modules: objects, dynamic fields, tables/bags instead of contract storage",
        notes: "Contract state stops being a key-value store inside one account; each asset becomes an addressable object with an owner. Port state layout first — every other layer consumes the new object types.",
        docs: &["sui-for-ethereum", "sui-move-concepts", "object-model", "dynamic-fields"], effort: "rewrite",
    },
    MigrationMapping {
        id: "erc20", source_chain: "ethereum", area: "contracts",
        source_pattern: "ERC-20 fungible token",
        sui_concept: "Sui Coin standard (closed-loop token for restricted transfer/spend)",
        notes: "Coin<T> with a TreasuryCap replaces balances mapping + mint/burn role; approvals disappear (objects move by ownership, not allowance).",
        docs: &["coin", "closed-loop-token", "sui-for-ethereum"], effort: "rewrite",
    },
    MigrationMapping {
        id: "erc721", source_chain: "ethereum", area: "contracts",
        source_pattern: "ERC-721 / ERC-1155 NFTs",
        sui_concept: "Sui objects + Display metadata + Kiosk with transfer policies",
        notes: "Each NFT is a real object, not a tokenId in a mapping. Royalties/transfer hooks become enforced Kiosk transfer policies; tokenURI metadata becomes a Display template.",
        docs: &["kiosk", "display", "transfer-rules", "object-model"], effort: "rewrite",
    },
    MigrationMapping {
        id: "evm-access-control", source_chain: "ethereum", area: "contracts",
        source_pattern: "Ownable / AccessControl roles",
        sui_concept: "Capability objects and explicit object ownership",
        notes: "msg.sender checks become possession of a capability object (e.g. AdminCap) passed into entry functions — authorization moves from address comparison to the type system. This is contract authorization, NOT a Seal use case.",
        docs: &["sui-for-ethereum", "object-ownership", "sui-move-concepts"], effort: "rethink",
    },
    MigrationMapping {
        id: "evm-upgrades", source_chain: "ethereum", area: "contracts",
        source_pattern: "Proxy / delegatecall upgrade patterns",
        sui_concept: "Native package upgrades gated by UpgradeCap",
        notes: "No proxies on Sui: packages upgrade in place under compatibility rules, authorized by the UpgradeCap object. Storage-slot collision concerns disappear; layout compatibility rules apply instead.",
        docs: &["packages"], effort: "rethink",
    },
    MigrationMapping {
        id: "multicall-ptb", source_chain: "ethereum", area: "client",
        source_pattern: "Multicall / batched contract calls",
        sui_concept: "Programmable transaction blocks (PTBs)",
        notes: "PTBs compose up to 1024 calls atomically with outputs piped between them — multicall contracts and batching SDKs are no longer needed.",
        docs: &["ptb"], effort: "adapt",
    },
    MigrationMapping {
        id: "evm-client-sdk", source_chain: "ethereum", area: "client",
        source_pattern: "ethers / viem / wagmi client code",
        sui_concept: "@mysten/sui TypeScript SDK + dapp-kit",
        notes: "Contract reads become object/owned-object queries; writes become PTBs. Event subscriptions and ABI typing are replaced by the Sui RPC/GraphQL surface.",
        docs: &["ts-sdk", "sui-for-ethereum"], effort: "rewrite",
    },
    MigrationMapping {
        id: "evm-toolchain", source_chain: "ethereum", area: "infra",
        source_pattern: "Hardhat / Foundry / Truffle build & deploy pipeline",
        sui_concept: "Sui CLI + Move build/test/publish",
        notes: "Compile/test/deploy scripts move to `sui move build/test` and `sui client publish`; deployment addresses become package ids + shared object ids in config.",
        docs: &["sui-for-ethereum", "packages"], effort: "rewrite",
    },
    // ---- Solana → Sui -------------------------------------------------------
    MigrationMapping {
        id: "solana-program", source_chain: "solana", area: "contracts",
        source_pattern: "Native Solana program (accounts + entrypoint)",
        sui_concept: "Move modules; accounts become owned/shared objects",
        notes: "Account deserialization and ownership checks become typed object parameters — the runtime enforces what account constraints used to assert by hand.",
        docs: &["sui-for-solana", "object-model", "object-ownership"], effort: "rewrite",
    },
    MigrationMapping {
        id: "anchor", source_chain: "solana", area: "contracts",
        source_pattern: "Anchor instructions & account constraints",
        sui_concept: "Move entry/public functions with typed object parameters",
        notes: "#[derive(Accounts)] constraint blocks map to function signatures: ownership, mutability, and type checks come from the object model rather than attribute macros.",
        docs: &["sui-for-solana", "sui-move-concepts"], effort: "rewrite",
    },
    MigrationMapping {
        id: "pda-objects", source_chain: "solana", area: "contracts",
        source_pattern: "PDAs and seed-derived accounts",
        sui_concept: "Dynamic (object) fields and derived child objects",
        notes: "Seed-addressed storage becomes dynamic fields attached to a parent object; signer-less PDAs become objects owned by a package-controlled parent.",
        docs: &["dynamic-fields", "sui-for-solana"], effort: "rethink",
    },
    MigrationMapping {
        id: "cpi-ptb", source_chain: "solana", area: "contracts",
        source_pattern: "CPI-heavy instruction flows",
        sui_concept: "Programmable transaction blocks or direct package calls",
        notes: "Cross-program invocations either become plain Move calls inside one package, or PTB steps composed client-side — invoke_signed ceremony disappears.",
        docs: &["ptb"], effort: "adapt",
    },
    MigrationMapping {
        id: "spl-token", source_chain: "solana", area: "contracts",
        source_pattern: "SPL Token / Token-2022",
        sui_concept: "Sui Coin standard + closed-loop token policies",
        notes: "Mint + token accounts collapse into Coin<T> objects held directly by owners; Token-2022 extensions (transfer hooks, fees) map to closed-loop token rules.",
        docs: &["coin", "closed-loop-token"], effort: "rewrite",
    },
    MigrationMapping {
        id: "metaplex-metadata", source_chain: "solana", area: "contracts",
        source_pattern: "Metaplex NFT metadata",
        sui_concept: "Display standard + Kiosk; media blobs to Walrus",
        notes: "Metadata PDAs become Display templates on the object type; collection-level royalties become Kiosk transfer policies.",
        docs: &["display", "kiosk", "walrus"], effort: "rewrite",
    },
    MigrationMapping {
        id: "solana-client-sdk", source_chain: "solana", area: "client",
        source_pattern: "@solana/web3.js client code",
        sui_concept: "@mysten/sui TypeScript SDK + dapp-kit",
        notes: "getAccountInfo/getProgramAccounts become object queries; transactions become PTBs; wallet adapters move to the Sui wallet standard.",
        docs: &["ts-sdk", "sui-for-solana"], effort: "rewrite",
    },
    MigrationMapping {
        id: "solana-rent", source_chain: "solana", area: "contracts",
        source_pattern: "Rent-exempt balances & account lifecycle",
        sui_concept: "Storage fees with rebates; object deletion reclaims",
        notes: "Rent-exemption top-ups become upfront storage fees refunded on object deletion — account-closing flows become object deletion, and 'reopen' bugs disappear.",
        docs: &["storage-fund"], effort: "rethink",
    },
    MigrationMapping {
        id: "program-upgrade", source_chain: "solana", area: "infra",
        source_pattern: "Program upgrade authority",
        sui_concept: "Package upgrades gated by UpgradeCap",
        notes: "The upgrade-authority keypair becomes an UpgradeCap object you can hold, wrap in governance, or burn to freeze the package.",
        docs: &["packages"], effort: "adapt",
    },
    // ---- Cross-chain: wallets / events / storage / access -----------------------
    MigrationMapping {
        id: "wallets", source_chain: "any", area: "client",
        source_pattern: "Wallet connection & signing (wallet adapters, EIP-712 / message signing)",
        sui_concept: "Sui wallet standard via dapp-kit; intent signing",
        notes: "Wallet connectors move to the Sui wallet standard (dapp-kit hooks); typed-data and message signing become Sui intent signing — session keys and zkLogin become available options.",
        docs: &["ts-sdk"], effort: "adapt",
    },
    MigrationMapping {
        id: "events-indexing", source_chain: "any", area: "indexer",
        source_pattern: "Event logs + external indexers (The Graph, Helius, custom)",
        sui_concept: "Sui events + GraphQL RPC / checkpoint-based indexing",
        notes: "Move events are typed structs queryable via GraphQL; indexers re-point at checkpoints instead of log topics. Schema changes are unavoidable — plan a reindex.",
        docs: &["events", "graphql-rpc"], effort: "rewrite",
    },
    MigrationMapping {
        id: "ipfs-walrus", source_chain: "any", area: "storage",
        source_pattern: "IPFS-pinned content (ipfs:// URIs, pinning services)",
        sui_concept: "Walrus blobs (HTTP publisher/aggregator API)",
        notes: "Content-addressed public blobs map directly to Walrus, with availability that Sui contracts can verify — pinning services and gateway dependence go away.",
        docs: &["walrus", "walrus-web-api", "walrus-design"], effort: "adapt",
    },
    MigrationMapping {
        id: "arweave-walrus", source_chain: "any", area: "storage",
        source_pattern: "Arweave permanent storage",
        sui_concept: "Walrus blobs (note the epoch-based retention model)",
        notes: "Walrus stores blobs for paid epochs, not 'forever' — review which data actually needs permanence vs verifiable availability before swapping.",
        docs: &["walrus", "walrus-design"], effort: "review",
    },
    MigrationMapping {
        id: "nft-metadata-walrus", source_chain: "any", area: "storage",
        source_pattern: "tokenURI / baseURI offchain metadata",
        sui_concept: "Display standard onchain + Walrus for media",
        notes: "The metadata template moves onchain (Display); only the actual media blobs need storage — put those on Walrus and drop the JSON-blob indirection.",
        docs: &["display", "walrus"], effort: "adapt",
    },
    MigrationMapping {
        id: "s3-review", source_chain: "any", area: "storage",
        source_pattern: "S3 / object-storage buckets",
        sui_concept: "Usually keep as auxiliary cache/CDN; Walrus only when availability must be contract-verifiable",
        notes: "Low-latency caches and private operational data should stay offchain with Sui/Walrus as source of truth. Only flows whose availability a contract must verify belong on Walrus.",
        docs: &["walrus-design"], effort: "review",
    },
    MigrationMapping {
        id: "encrypted-content-seal", source_chain: "any", area: "access",
        source_pattern: "Encrypted offchain content",
        sui_concept: "Seal (threshold encryption, onchain access policy) + Walrus for ciphertext",
        notes: "Seal applies when WHO may decrypt is a product rule worth putting onchain. Transport encryption or internal secrets are not Seal use cases — verify before adopting.",
        docs: &["seal", "walrus"], effort: "review",
    },
    MigrationMapping {
        id: "gated-content-seal", source_chain: "any", area: "access",
        source_pattern: "Token-gated content access (e.g. Lit Protocol)",
        sui_concept: "Seal access policies bound to Sui objects",
        notes: "Gating conditions ('holds NFT X', 'member of Y') become Seal policies evaluated against Sui state — the closest like-for-like replacement in this report.",
        docs: &["seal"], effort: "rewrite",
    },
];

fn mapping(id: &str) -> Option<&'static MigrationMapping> {
    MAPPINGS.iter().find(|m| m.id == id)
}

// ---------------------------------------------------------------------------
// Report data model (serialized into the HTML manifest).
// ---------------------------------------------------------------------------

/// One piece of detected source-stack evidence.
#[derive(Debug, Clone, Serialize)]
pub struct Signal {
    pub label: String,
    /// `ethereum` | `solana` | `sui` | null (chain-neutral, e.g. storage).
    pub chain: Option<&'static str>,
    pub area: &'static str,
    /// `dependency` | `code-pattern` | `contract-definition` | `config-file`.
    pub via: &'static str,
    pub detail: String,
    pub files: Vec<String>,
    pub files_total: usize,
    #[serde(skip)]
    mappings: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceResolution {
    pub requested: String,
    /// `ethereum` | `solana` | `mixed` | `unknown`.
    pub resolved: String,
    /// Signal counts per chain.
    pub evidence: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AffectedFeature {
    pub community_id: Uuid,
    pub label: String,
    pub member_count: i32,
    pub areas: Vec<String>,
    pub evidence_files: Vec<String>,
    pub evidence_files_total: usize,
    /// "name (kind)" pairs.
    pub top_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Serializable view of a triggered [`MigrationMapping`] with docs resolved.
#[derive(Debug, Clone, Serialize)]
pub struct MappingView {
    pub id: &'static str,
    pub source_chain: &'static str,
    pub area: &'static str,
    pub source_pattern: &'static str,
    pub sui_concept: &'static str,
    pub notes: &'static str,
    pub effort: &'static str,
    pub docs: Vec<DocRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocRef {
    pub id: &'static str,
    pub title: &'static str,
    pub url: &'static str,
}

fn doc_refs(ids: &[&'static str]) -> Vec<DocRef> {
    ids.iter()
        .filter_map(|id| sui_docs::doc(id))
        .map(|d| DocRef {
            id: d.id,
            title: d.title,
            url: d.url,
        })
        .collect()
}

/// A storage flow classified per R4.
#[derive(Debug, Clone, Serialize)]
pub struct StorageCandidate {
    pub flow: String,
    /// `walrus-blob` | `walrus-sites` | `walrus-plus-seal` | `sui-object-state`
    /// | `keep-offchain` | `review`.
    pub classification: &'static str,
    pub reasoning: String,
    pub evidence_files: Vec<String>,
    pub docs: Vec<DocRef>,
}

/// An access-control flow with an explicit Seal verdict (including "not
/// needed" — the report must not blindly recommend Seal).
#[derive(Debug, Clone, Serialize)]
pub struct AccessCandidate {
    pub pattern: String,
    /// `strong` | `possible` | `not-needed`.
    pub seal_fit: &'static str,
    pub reasoning: String,
    pub evidence_files: Vec<String>,
    pub docs: Vec<DocRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewStep {
    pub order: usize,
    pub area: &'static str,
    pub instruction: &'static str,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedPage {
    pub page: String,
    pub title: String,
    pub score: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocsProfileView {
    pub name: &'static str,
    pub retrieved: &'static str,
    pub entries_total: usize,
    pub cited: Vec<&'static sui_docs::DocEntry>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub signals: usize,
    pub affected_features: usize,
    pub mappings: usize,
    pub storage_candidates: usize,
    pub access_candidates: usize,
    pub docs_cited: usize,
}

/// Honesty contract: what this report reads, and what it cannot see.
#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub included: Vec<String>,
    pub not_covered: Vec<String>,
}

impl Coverage {
    fn current() -> Self {
        Self {
            included: vec![
                "manifest-declared npm/cargo dependencies from the persisted index".into(),
                "lexical scans over indexed chunk content (Solidity, Rust, TS/JS, Python, Markdown, PDF text)".into(),
                "Solidity contract/interface/library definitions from AST extraction".into(),
                "disk presence probes for toolchain configs (foundry.toml, hardhat.config.*, Anchor.toml, subgraph.yaml, Move.toml)".into(),
                "L1 feature communities + prior generated feature pages for impact mapping".into(),
            ],
            not_covered: vec![
                "contents of unindexed configs (foundry.toml/Anchor.toml internals — only presence is detected)".into(),
                "Dockerfiles, CI workflows, Terraform — not indexed".into(),
                "runtime behavior, gas/economics modeling, oracle and bridge dependencies".into(),
                "the walrus-sites and sui-object-state storage buckets have no automatic detector yet — they are part of the classification vocabulary but are never auto-assigned; flows that fit them surface as 'review'".into(),
                "automatic Solidity/Anchor → Move translation — this report maps impact, it does not generate Move code or guarantee correctness".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiMigrationManifest {
    pub schema_version: String,
    pub repo_name: String,
    pub title: String,
    pub subtitle: String,
    pub overview: String,
    pub source: SourceResolution,
    pub languages: Value,
    pub totals: Totals,
    pub signals: Vec<Signal>,
    pub affected_features: Vec<AffectedFeature>,
    pub mappings: Vec<MappingView>,
    pub storage_candidates: Vec<StorageCandidate>,
    pub access_candidates: Vec<AccessCandidate>,
    pub review_order: Vec<ReviewStep>,
    pub docs_profile: DocsProfileView,
    pub related_pages: Vec<RelatedPage>,
    pub coverage: Coverage,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Run the migration impact report: detect → classify → map to features →
/// write HTML → return the compact JSON.
pub async fn run(storage: &Storage, repo: &str, opts: &SuiMigrationOptions) -> Result<Value> {
    let (repo, repo_root) = resolve_indexed_repo(storage, repo).await?;
    let requested = opts.source.clone().unwrap_or_else(|| "auto".into());
    if !matches!(requested.as_str(), "auto" | "ethereum" | "solana" | "mixed") {
        anyhow::bail!("source must be one of: auto, ethereum, solana, mixed (got {requested})");
    }
    let feature_cap = if opts.limit > 0 { opts.limit } else { 12 };

    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ---- R2: source-stack detection -----------------------------------------
    let dependencies = storage.stack_dependencies(repo.id).await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "stack_dependencies",
            format!(
                "{} manifest-declared dependency row(s) matched against {} chain/storage/access rules",
                dependencies.len(),
                DEP_RULES.len()
            ),
        )
        .with_locator("nodes"),
    );
    let mut signals = detect_dependency_signals(&dependencies);

    let patterns: Vec<String> = CHUNK_RULES
        .iter()
        .map(|r| format!("%{}%", r.needle))
        .collect();
    let chunk_rows = storage
        .scan_chunks(repo.id, &patterns, CHUNK_SCAN_LIMIT)
        .await?;
    provenance.push(
        Breadcrumb::new(
            source::REGEX,
            "scan_chunks",
            format!(
                "{} chunk(s) hit the ILIKE prefilter for {} code patterns; exact substring match applied in Rust",
                chunk_rows.len(),
                CHUNK_RULES.len()
            ),
        )
        .with_locator("chunks"),
    );
    if chunk_rows.len() as i64 >= CHUNK_SCAN_LIMIT {
        warnings.push(format!(
            "chunk scan hit its {CHUNK_SCAN_LIMIT}-row prefilter cap — code-pattern evidence may be incomplete for very large repos (match counts are a lower bound)"
        ));
    }
    signals.extend(detect_chunk_signals(&chunk_rows));

    let contracts = storage.solidity_contract_nodes(repo.id).await?;
    if !contracts.is_empty() {
        provenance.push(
            Breadcrumb::new(
                source::AST,
                "solidity_contract_nodes",
                format!(
                    "{} Solidity contract/interface/library definition(s) from AST extraction",
                    contracts.len()
                ),
            )
            .with_locator("nodes"),
        );
        let mut files: BTreeSet<String> =
            contracts.iter().map(|(_, _, path)| path.clone()).collect();
        files.remove("");
        let names: Vec<String> = contracts
            .iter()
            .take(5)
            .map(|(name, kind, _)| format!("{name} ({kind})"))
            .collect();
        signals.push(make_signal(
            "Solidity contract definitions".into(),
            Some("ethereum"),
            "contracts",
            "contract-definition",
            format!("{} definition(s): {}", contracts.len(), names.join(", ")),
            files,
            vec!["solidity-move"],
        ));
    }

    let (config_signals, config_crumbs) = probe_config_files(&repo_root);
    provenance.extend(config_crumbs);
    signals.extend(config_signals);

    let languages = storage.group_counts(repo.id, "files", "language").await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "group_counts",
            "files grouped by language",
        )
        .with_locator("files"),
    );

    let source_resolution = resolve_source(&requested, &signals, &mut warnings);
    if signals.is_empty() {
        warnings.push(
            "no source-chain, storage, or access evidence detected — this may not be a Web3 repository, or the index predates manifest/chunk extraction (re-run chaos_analyze)"
                .into(),
        );
    }

    // ---- R3: map evidence files onto L1 feature communities --------------------
    let mut evidence_files: BTreeSet<String> = BTreeSet::new();
    let mut file_areas: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    for signal in &signals {
        for file in &signal.files {
            evidence_files.insert(file.clone());
            file_areas
                .entry(file.clone())
                .or_default()
                .insert(signal.area);
        }
    }
    let evidence_paths: Vec<String> = evidence_files.iter().cloned().collect();
    let file_to_community = storage
        .dominant_community_for_files(repo.id, &evidence_paths)
        .await?;
    provenance.push(
        Breadcrumb::new(
            source::GRAPH,
            "dominant_community_for_files",
            format!(
                "{} of {} evidence file(s) mapped to L1 feature communities",
                file_to_community.len(),
                evidence_paths.len()
            ),
        )
        .with_locator("community_members"),
    );
    let mut community_files: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for (file, community) in &file_to_community {
        community_files
            .entry(*community)
            .or_default()
            .insert(file.clone());
    }
    let community_ids: Vec<Uuid> = community_files.keys().copied().collect();
    let briefs = storage
        .load_community_briefs(repo.id, &community_ids)
        .await?;
    let mut affected_features: Vec<AffectedFeature> = Vec::new();
    for brief in &briefs {
        let files = community_files.get(&brief.id).cloned().unwrap_or_default();
        let mut areas: BTreeSet<&'static str> = BTreeSet::new();
        for file in &files {
            if let Some(file_area) = file_areas.get(file) {
                areas.extend(file_area.iter().copied());
            }
        }
        let symbols = storage
            .load_community_top_symbols(brief.id, TOP_SYMBOLS_PER_FEATURE)
            .await?;
        let files_total = files.len();
        let mut files: Vec<String> = files.into_iter().collect();
        files.truncate(MAX_EVIDENCE_FILES);
        affected_features.push(AffectedFeature {
            community_id: brief.id,
            label: brief.label.clone(),
            member_count: brief.member_count,
            areas: order_areas(&areas),
            evidence_files: files,
            evidence_files_total: files_total,
            top_symbols: symbols
                .into_iter()
                .map(|(name, kind, _)| format!("{name} ({kind})"))
                .collect(),
            summary: brief.summary.clone().map(|s| excerpt(&s, 240)),
        });
    }
    // Review-order area first, then most evidence.
    affected_features.sort_by(|a, b| {
        area_rank(a.areas.first().map(String::as_str))
            .cmp(&area_rank(b.areas.first().map(String::as_str)))
            .then(b.evidence_files_total.cmp(&a.evidence_files_total))
            .then(a.label.cmp(&b.label))
    });
    if community_ids.is_empty() && !signals.is_empty() {
        warnings.push(
            "no L1 feature communities matched the evidence files — the hierarchy may be missing (re-run chaos_analyze) so impact is reported at file level only"
                .into(),
        );
    }

    // ---- Prior generated feature pages ----------------------------------------
    let features_dir = opts
        .features_dir
        .clone()
        .unwrap_or_else(|| features_memory_dir(&repo_root));
    let correlation_query = correlation_query(&signals, &source_resolution.resolved);
    let related_pages: Vec<RelatedPage> = match load_feature_matches(
        &correlation_query,
        &features_dir,
        3,
        4,
    ) {
        Ok(matches) => {
            provenance.push(
                Breadcrumb::new(
                    source::MANIFEST,
                    "load_feature_matches",
                    format!(
                        "{} prior feature page(s) correlated with the detected migration surface",
                        matches.len()
                    ),
                )
                .with_locator(features_dir.display().to_string()),
            );
            matches
                .into_iter()
                .map(|m| RelatedPage {
                    page: m.page.display().to_string(),
                    title: m.title,
                    score: m.score,
                })
                .collect()
        }
        Err(err) => {
            warnings.push(format!(
                    "prior feature pages could not be read from {} ({err}) — related-page correlation skipped",
                    features_dir.display()
                ));
            Vec::new()
        }
    };

    // ---- Mappings, storage, access ----------------------------------------------
    let triggered = triggered_mappings(&signals);
    let storage_candidates = classify_storage(&signals);
    let access_candidates = classify_access(&signals);
    let review_order = build_review_order(&affected_features, &triggered);
    let cited = cited_docs(&triggered, &storage_candidates, &access_candidates);
    provenance.push(Breadcrumb::new(
        source::DOCS,
        "sui_docs_profile",
        format!(
            "{} official doc(s) cited from profile '{}' (URLs verified {})",
            cited.len(),
            sui_docs::PROFILE_NAME,
            sui_docs::PROFILE_RETRIEVED
        ),
    ));
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "sui_migration_impact",
        format!(
            "aggregated {} signal(s) → {} affected feature(s), {} mapping(s), {} storage / {} access candidate(s)",
            signals.len(),
            affected_features.len(),
            triggered.len(),
            storage_candidates.len(),
            access_candidates.len()
        ),
    ));

    let totals = Totals {
        signals: signals.len(),
        affected_features: affected_features.len(),
        mappings: triggered.len(),
        storage_candidates: storage_candidates.len(),
        access_candidates: access_candidates.len(),
        docs_cited: cited.len(),
    };
    let overview = compose_overview(&repo.name, &source_resolution, &totals);
    let manifest = SuiMigrationManifest {
        schema_version: "sui-migration-impact-1".into(),
        repo_name: repo.name.clone(),
        title: format!("{} — Sui migration impact", repo.name),
        subtitle: "Which existing features a Sui migration touches, which Sui primitives replace the current patterns, and what to review first — evidence-triggered, official-docs-backed, no auto-migration claims."
            .into(),
        overview,
        source: source_resolution,
        languages,
        totals,
        signals: signals.clone(),
        affected_features,
        mappings: triggered
            .iter()
            .map(|m| MappingView {
                id: m.id,
                source_chain: m.source_chain,
                area: m.area,
                source_pattern: m.source_pattern,
                sui_concept: m.sui_concept,
                notes: m.notes,
                effort: m.effort,
                docs: doc_refs(m.docs),
            })
            .collect(),
        storage_candidates,
        access_candidates,
        review_order,
        docs_profile: DocsProfileView {
            name: sui_docs::PROFILE_NAME,
            retrieved: sui_docs::PROFILE_RETRIEVED,
            entries_total: sui_docs::ENTRIES.len(),
            cited,
        },
        related_pages,
        coverage: Coverage::current(),
        provenance,
        warnings,
    };

    let output = opts
        .output_html
        .clone()
        .unwrap_or_else(|| features_memory_dir(&repo_root).join("sui-migration-impact.html"));
    write_sui_migration_html(&output, &manifest)?;

    Ok(compact_return(&manifest, &output, repo.id, feature_cap))
}

// ---------------------------------------------------------------------------
// Detection.
// ---------------------------------------------------------------------------

fn dep_matches(rule: &DepRule, ecosystem: &str, name: &str) -> bool {
    if rule.ecosystem != "*" && rule.ecosystem != ecosystem {
        return false;
    }
    match rule.pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == rule.pattern,
    }
}

fn make_signal(
    label: String,
    chain: Option<&'static str>,
    area: &'static str,
    via: &'static str,
    detail: String,
    files: BTreeSet<String>,
    mappings: Vec<&'static str>,
) -> Signal {
    let files_total = files.len();
    let mut files: Vec<String> = files.into_iter().collect();
    files.truncate(MAX_EVIDENCE_FILES);
    Signal {
        label,
        chain,
        area,
        via,
        detail,
        files,
        files_total,
        mappings,
    }
}

/// Fold dependency rows through [`DEP_RULES`] (first matching rule wins per
/// dependency; most specific rules are listed first) and group by rule label.
fn detect_dependency_signals(rows: &[StackDependencyRow]) -> Vec<Signal> {
    // label -> (rule fields, package names, manifest files)
    struct Acc {
        chain: Option<&'static str>,
        area: &'static str,
        mappings: Vec<&'static str>,
        packages: BTreeSet<String>,
        manifests: BTreeSet<String>,
    }
    let mut by_label: BTreeMap<&'static str, Acc> = BTreeMap::new();
    for row in rows {
        let Some(rule) = DEP_RULES
            .iter()
            .find(|rule| dep_matches(rule, &row.ecosystem, &row.name))
        else {
            continue;
        };
        let acc = by_label.entry(rule.label).or_insert_with(|| Acc {
            chain: rule.chain,
            area: rule.area,
            mappings: rule.mappings.to_vec(),
            packages: BTreeSet::new(),
            manifests: BTreeSet::new(),
        });
        acc.packages.insert(row.name.clone());
        if !row.manifest.is_empty() {
            acc.manifests.insert(row.manifest.clone());
        }
    }
    by_label
        .into_iter()
        .map(|(label, acc)| {
            let packages: Vec<String> = acc.packages.iter().take(5).cloned().collect();
            make_signal(
                label.to_string(),
                acc.chain,
                acc.area,
                "dependency",
                format!(
                    "declared package(s): {}{}",
                    packages.join(", "),
                    if acc.packages.len() > packages.len() {
                        format!(" (+{} more)", acc.packages.len() - packages.len())
                    } else {
                        String::new()
                    }
                ),
                acc.manifests,
                acc.mappings,
            )
        })
        .collect()
}

/// Apply [`CHUNK_RULES`] to the prefiltered chunk rows (exact, case-insensitive
/// substring — the SQL ILIKE was only a superset prefilter) and group by rule
/// label.
fn detect_chunk_signals(rows: &[(String, String)]) -> Vec<Signal> {
    struct Acc {
        chain: Option<&'static str>,
        area: &'static str,
        mappings: Vec<&'static str>,
        hits: usize,
        files: BTreeSet<String>,
    }
    let mut by_label: BTreeMap<&'static str, Acc> = BTreeMap::new();
    for (path, content) in rows {
        let lowered = content.to_lowercase();
        for rule in CHUNK_RULES {
            if !lowered.contains(rule.needle) {
                continue;
            }
            let acc = by_label.entry(rule.label).or_insert_with(|| Acc {
                chain: rule.chain,
                area: rule.area,
                mappings: rule.mappings.to_vec(),
                hits: 0,
                files: BTreeSet::new(),
            });
            acc.hits += 1;
            if !path.is_empty() {
                acc.files.insert(path.clone());
            }
        }
    }
    by_label
        .into_iter()
        .map(|(label, acc)| {
            let file_count = acc.files.len();
            make_signal(
                label.to_string(),
                acc.chain,
                acc.area,
                "code-pattern",
                format!("{} chunk match(es) across {} file(s)", acc.hits, file_count),
                acc.files,
                acc.mappings,
            )
        })
        .collect()
}

/// Bounded disk walk (depth ≤ 3, skipping vendored/build dirs) probing for
/// well-known toolchain config filenames the index does not store as content.
fn probe_config_files(repo_root: &Path) -> (Vec<Signal>, Vec<Breadcrumb>) {
    const SKIP_DIRS: [&str; 8] = [
        "node_modules",
        ".git",
        "target",
        "dist",
        "build",
        ".next",
        "out",
        "artifacts",
    ];
    fn walk(dir: &Path, depth: usize, found: &mut Vec<(usize, PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if depth < CONFIG_PROBE_MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                    walk(&path, depth + 1, found);
                }
            } else if let Some(idx) = CONFIG_PROBES.iter().position(|p| p.file_name == name) {
                found.push((idx, path));
            }
        }
    }
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    walk(repo_root, 0, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut by_label: BTreeMap<&'static str, (usize, BTreeSet<String>)> = BTreeMap::new();
    for (idx, path) in &found {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .display()
            .to_string();
        let entry = by_label
            .entry(CONFIG_PROBES[*idx].label)
            .or_insert((*idx, BTreeSet::new()));
        entry.1.insert(rel);
    }
    let mut crumbs = Vec::new();
    let signals: Vec<Signal> = by_label
        .into_iter()
        .map(|(label, (idx, files))| {
            let probe = &CONFIG_PROBES[idx];
            crumbs.push(
                Breadcrumb::new(
                    source::FILE,
                    "probe_config_files",
                    format!("{label}: {} file(s) present on disk", files.len()),
                )
                .with_locator(files.iter().next().cloned().unwrap_or_default()),
            );
            make_signal(
                label.to_string(),
                probe.chain,
                probe.area,
                "config-file",
                format!(
                    "{} config file(s) present (contents not indexed — presence only)",
                    files.len()
                ),
                files,
                probe.mappings.to_vec(),
            )
        })
        .collect();
    (signals, crumbs)
}

fn resolve_source(
    requested: &str,
    signals: &[Signal],
    warnings: &mut Vec<String>,
) -> SourceResolution {
    let mut evidence: BTreeMap<String, usize> = BTreeMap::new();
    for chain in ["ethereum", "solana", "sui"] {
        let count = signals.iter().filter(|s| s.chain == Some(chain)).count();
        if count > 0 {
            evidence.insert(chain.to_string(), count);
        }
    }
    let eth = evidence.get("ethereum").copied().unwrap_or(0);
    let sol = evidence.get("solana").copied().unwrap_or(0);
    let detected = match (eth > 0, sol > 0) {
        (true, true) => "mixed",
        (true, false) => "ethereum",
        (false, true) => "solana",
        (false, false) => "unknown",
    };
    let resolved = if requested == "auto" {
        detected.to_string()
    } else {
        if detected != "unknown" && detected != requested {
            warnings.push(format!(
                "source forced to '{requested}' but the evidence reads as '{detected}' ({eth} ethereum / {sol} solana signal(s)) — consider --source {detected}"
            ));
        }
        requested.to_string()
    };
    if evidence.contains_key("sui") {
        warnings.push(
            "the repository already references Sui (Mysten SDK / Move package detected) — parts of the migration may already be underway"
                .into(),
        );
    }
    SourceResolution {
        requested: requested.to_string(),
        resolved,
        evidence,
    }
}

// ---------------------------------------------------------------------------
// Classification.
// ---------------------------------------------------------------------------

fn area_rank(area: Option<&str>) -> usize {
    area.and_then(|a| AREA_ORDER.iter().position(|x| *x == a))
        .unwrap_or(AREA_ORDER.len())
}

fn order_areas(areas: &BTreeSet<&'static str>) -> Vec<String> {
    let mut ordered: Vec<&'static str> = areas.iter().copied().collect();
    ordered.sort_by_key(|a| area_rank(Some(a)));
    ordered.into_iter().map(String::from).collect()
}

/// Union of mapping ids across all signals, resolved against [`MAPPINGS`] and
/// ordered by review-order area then table order.
fn triggered_mappings(signals: &[Signal]) -> Vec<&'static MigrationMapping> {
    let ids: BTreeSet<&'static str> = signals
        .iter()
        .flat_map(|s| s.mappings.iter().copied())
        .collect();
    let mut out: Vec<&'static MigrationMapping> = ids.iter().filter_map(|id| mapping(id)).collect();
    out.sort_by(|a, b| {
        area_rank(Some(a.area))
            .cmp(&area_rank(Some(b.area)))
            .then_with(|| {
                let pos = |m: &MigrationMapping| MAPPINGS.iter().position(|x| x.id == m.id);
                pos(a).cmp(&pos(b))
            })
    });
    out
}

fn signal_files_for(signals: &[Signal], mapping_id: &str) -> Vec<String> {
    let mut files: BTreeSet<String> = BTreeSet::new();
    for signal in signals {
        if signal.mappings.contains(&mapping_id) {
            files.extend(signal.files.iter().cloned());
        }
    }
    files.into_iter().take(MAX_EVIDENCE_FILES).collect()
}

fn has_mapping(signals: &[Signal], mapping_id: &str) -> bool {
    signals.iter().any(|s| s.mappings.contains(&mapping_id))
}

/// R4: classify detected storage flows into Walrus / Walrus+Seal /
/// keep-offchain / review buckets. Only evidence-backed candidates are
/// emitted — no storage evidence, no Walrus pitch.
fn classify_storage(signals: &[Signal]) -> Vec<StorageCandidate> {
    let mut out = Vec::new();
    let gated = has_mapping(signals, "gated-content-seal");
    let encrypted = has_mapping(signals, "encrypted-content-seal");

    if has_mapping(signals, "ipfs-walrus") {
        out.push(StorageCandidate {
            flow: "Content-addressed public blobs on IPFS".into(),
            classification: "walrus-blob",
            reasoning: "ipfs:// URIs / pinning-service usage detected. Content-addressed public blobs map directly to Walrus blobs, and availability becomes verifiable by Sui contracts — pinning subscriptions and gateway dependence go away.".into(),
            evidence_files: signal_files_for(signals, "ipfs-walrus"),
            docs: doc_refs(&["walrus", "walrus-web-api", "walrus-design"]),
        });
        if gated || encrypted {
            out.push(StorageCandidate {
                flow: "Encrypted / gated subset of the blob flows".into(),
                classification: "walrus-plus-seal",
                reasoning: "the repo combines blob storage with encryption or token-gating evidence — for flows where WHO may decrypt is a product rule, store ciphertext on Walrus and bind the decryption policy to Sui state with Seal.".into(),
                evidence_files: signal_files_for(
                    signals,
                    if gated { "gated-content-seal" } else { "encrypted-content-seal" },
                ),
                docs: doc_refs(&["seal", "walrus"]),
            });
        }
    } else if gated || encrypted {
        out.push(StorageCandidate {
            flow: "Encrypted / gated offchain content".into(),
            classification: "walrus-plus-seal",
            reasoning: "encryption or token-gating evidence without a detected blob store — if the encrypted content is product data with an onchain access rule, Walrus (ciphertext) + Seal (policy) is the Sui-native shape.".into(),
            evidence_files: signal_files_for(
                signals,
                if gated { "gated-content-seal" } else { "encrypted-content-seal" },
            ),
            docs: doc_refs(&["seal", "walrus"]),
        });
    }
    if has_mapping(signals, "arweave-walrus") {
        out.push(StorageCandidate {
            flow: "Arweave permanent storage".into(),
            classification: "review",
            reasoning: "Arweave usage detected. Walrus covers verifiable availability but stores blobs for paid epochs, not permanently — review which data actually needs permanence before swapping.".into(),
            evidence_files: signal_files_for(signals, "arweave-walrus"),
            docs: doc_refs(&["walrus", "walrus-design"]),
        });
    }
    if has_mapping(signals, "nft-metadata-walrus") {
        out.push(StorageCandidate {
            flow: "NFT metadata & media URIs (tokenURI/baseURI)".into(),
            classification: "walrus-blob",
            reasoning: "offchain token metadata detected. On Sui the metadata template moves onchain via the Display standard; only the media blobs need storage — Walrus candidates.".into(),
            evidence_files: signal_files_for(signals, "nft-metadata-walrus"),
            docs: doc_refs(&["display", "walrus"]),
        });
    }
    if has_mapping(signals, "s3-review") {
        out.push(StorageCandidate {
            flow: "S3 / object-storage buckets".into(),
            classification: "keep-offchain",
            reasoning: "S3-style storage detected. Low-latency caches and private operational data should usually stay offchain (with Sui/Walrus as source of truth); move a flow to Walrus only when a contract must verify its availability. Requires a per-bucket review.".into(),
            evidence_files: signal_files_for(signals, "s3-review"),
            docs: doc_refs(&["walrus-design"]),
        });
    }
    out
}

/// Seal candidates with explicit verdicts — including `not-needed` for
/// contract-role access control, which maps to capabilities, not Seal.
fn classify_access(signals: &[Signal]) -> Vec<AccessCandidate> {
    let mut out = Vec::new();
    if has_mapping(signals, "gated-content-seal") {
        out.push(AccessCandidate {
            pattern: "Token-gated content access (Lit Protocol or similar)".into(),
            seal_fit: "strong",
            reasoning: "gating conditions evaluated against chain state are exactly what Seal policies express on Sui — the closest like-for-like replacement in this report.".into(),
            evidence_files: signal_files_for(signals, "gated-content-seal"),
            docs: doc_refs(&["seal"]),
        });
    }
    if has_mapping(signals, "encrypted-content-seal") {
        out.push(AccessCandidate {
            pattern: "Encryption usage in code".into(),
            seal_fit: "possible",
            reasoning: "encryption evidence found, but Seal only applies when WHO may decrypt is a product rule worth putting onchain. Transport encryption, hashing, or internal secrets are NOT Seal use cases — verify each flow before adopting.".into(),
            evidence_files: signal_files_for(signals, "encrypted-content-seal"),
            docs: doc_refs(&["seal", "walrus"]),
        });
    }
    if has_mapping(signals, "evm-access-control") {
        out.push(AccessCandidate {
            pattern: "Contract-role access control (Ownable / AccessControl)".into(),
            seal_fit: "not-needed",
            reasoning: "contract authorization maps to capability objects and object ownership on Sui — public data guarded by roles needs no encryption, so Seal is not the tool here.".into(),
            evidence_files: signal_files_for(signals, "evm-access-control"),
            docs: doc_refs(&["object-ownership", "sui-for-ethereum"]),
        });
    }
    out
}

const REVIEW_INSTRUCTIONS: [(&str, &str); 7] = [
    (
        "contracts",
        "Port contract/program state and entry points to Move modules and objects first — every other layer consumes the new object types.",
    ),
    (
        "client",
        "Swap chain SDKs for @mysten/sui + dapp-kit against the new package; object reads replace contract calls, PTBs replace batched writes.",
    ),
    (
        "storage",
        "Move blob flows to Walrus (and metadata templates to Display) once object types exist to anchor them.",
    ),
    (
        "access",
        "Decide Seal policies after object ownership is settled — policies bind to Sui state, so they cannot be designed before the objects exist.",
    ),
    (
        "indexer",
        "Rebuild event consumers on Sui events + GraphQL RPC once contract events are final; plan a full reindex.",
    ),
    (
        "infra",
        "Replace the build/deploy toolchain (Hardhat/Anchor → Sui CLI + Move) and update CI; package ids replace contract addresses in config.",
    ),
    (
        "docs",
        "Refresh READMEs and docs that describe chain-specific flows.",
    ),
];

fn build_review_order(
    features: &[AffectedFeature],
    mappings: &[&'static MigrationMapping],
) -> Vec<ReviewStep> {
    let active_areas: BTreeSet<&str> = features
        .iter()
        .flat_map(|f| f.areas.iter().map(String::as_str))
        .chain(mappings.iter().map(|m| m.area))
        .collect();
    let mut order = 0usize;
    REVIEW_INSTRUCTIONS
        .iter()
        .filter(|(area, _)| active_areas.contains(area))
        .map(|(area, instruction)| {
            order += 1;
            ReviewStep {
                order,
                area,
                instruction,
                features: features
                    .iter()
                    .filter(|f| f.areas.iter().any(|a| a == area))
                    .map(|f| f.label.clone())
                    .collect(),
            }
        })
        .collect()
}

fn cited_docs(
    mappings: &[&'static MigrationMapping],
    storage_candidates: &[StorageCandidate],
    access_candidates: &[AccessCandidate],
) -> Vec<&'static sui_docs::DocEntry> {
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for m in mappings {
        ids.extend(m.docs.iter().copied());
    }
    for c in storage_candidates {
        ids.extend(c.docs.iter().map(|d| d.id));
    }
    for c in access_candidates {
        ids.extend(c.docs.iter().map(|d| d.id));
    }
    // Migration guides are always worth citing once any chain evidence exists.
    sui_docs::ENTRIES
        .iter()
        .filter(|e| ids.contains(e.id))
        .collect()
}

/// Token soup for prior-page correlation: detected labels + chains. Purely a
/// keyword query for `load_feature_matches` (which scores by token overlap).
fn correlation_query(signals: &[Signal], resolved: &str) -> String {
    let mut parts: Vec<String> = vec!["Sui migration".into(), resolved.to_string()];
    for signal in signals.iter().take(12) {
        parts.push(signal.label.clone());
    }
    parts.join(" ")
}

fn excerpt(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Deterministic extractive overview (pure — same inputs ⇒ same text).
fn compose_overview(repo_name: &str, source: &SourceResolution, totals: &Totals) -> String {
    let stack = match source.resolved.as_str() {
        "unknown" => "no recognizable source-chain stack".to_string(),
        resolved => {
            let article = if resolved.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            format!("{article} {resolved} stack")
        }
    };
    format!(
        "{repo_name} reads as {stack}: {} migration signal(s), {} existing feature(s) affected, {} Sui concept mapping(s) triggered, {} storage and {} access-control flow(s) classified. {} official doc(s) cited from the {} profile (retrieved {}). Full evidence in the HTML report.",
        totals.signals,
        totals.affected_features,
        totals.mappings,
        totals.storage_candidates,
        totals.access_candidates,
        totals.docs_cited,
        sui_docs::PROFILE_NAME,
        sui_docs::PROFILE_RETRIEVED,
    )
}

// ---------------------------------------------------------------------------
// Compact return.
// ---------------------------------------------------------------------------

/// The compact MCP/CLI return: capped lists, lifted omission counts, full
/// detail in the HTML.
fn compact_return(
    manifest: &SuiMigrationManifest,
    output: &Path,
    repo_id: Uuid,
    feature_cap: usize,
) -> Value {
    let signals: Vec<Value> = manifest
        .signals
        .iter()
        .take(MAX_COMPACT_SIGNALS)
        .map(|s| {
            json!({
                "label": s.label,
                "chain": s.chain,
                "area": s.area,
                "via": s.via,
                "files": s.files_total,
                "example": s.files.first(),
            })
        })
        .collect();
    let features: Vec<Value> = manifest
        .affected_features
        .iter()
        .take(feature_cap)
        .map(|f| {
            json!({
                "label": f.label,
                "areas": f.areas,
                "evidence_files": f.evidence_files.iter().take(3).collect::<Vec<_>>(),
                "evidence_files_total": f.evidence_files_total,
                "symbols": f.top_symbols.iter().take(4).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mappings: Vec<Value> = manifest
        .mappings
        .iter()
        .take(MAX_COMPACT_MAPPINGS)
        .map(|m| {
            json!({
                "source_pattern": m.source_pattern,
                "sui_concept": m.sui_concept,
                "area": m.area,
                "effort": m.effort,
                "docs": m.docs.iter().map(|d| d.id).collect::<Vec<_>>(),
            })
        })
        .collect();
    let storage: Vec<Value> = manifest
        .storage_candidates
        .iter()
        .take(MAX_COMPACT_STORAGE)
        .map(|c| {
            json!({
                "flow": c.flow,
                "classification": c.classification,
                "docs": c.docs.iter().map(|d| d.id).collect::<Vec<_>>(),
            })
        })
        .collect();
    let access: Vec<Value> = manifest
        .access_candidates
        .iter()
        .take(MAX_COMPACT_ACCESS)
        .map(|c| {
            json!({
                "pattern": c.pattern,
                "seal_fit": c.seal_fit,
            })
        })
        .collect();

    json!({
        "status": "ok",
        "repo": manifest.repo_name,
        "repo_id": repo_id,
        "overview": manifest.overview,
        "source": manifest.source,
        "totals": manifest.totals,
        "signals": signals,
        "signals_omitted": manifest.signals.len().saturating_sub(MAX_COMPACT_SIGNALS),
        "affected_features": features,
        "affected_features_omitted": manifest.affected_features.len().saturating_sub(feature_cap),
        "mappings": mappings,
        "mappings_omitted": manifest.mappings.len().saturating_sub(MAX_COMPACT_MAPPINGS),
        "storage_candidates": storage,
        "storage_candidates_omitted": manifest.storage_candidates.len().saturating_sub(MAX_COMPACT_STORAGE),
        "access_candidates": access,
        "access_candidates_omitted": manifest.access_candidates.len().saturating_sub(MAX_COMPACT_ACCESS),
        "review_order": manifest.review_order.iter().map(|s| s.area).collect::<Vec<_>>(),
        "docs_profile": {
            "name": manifest.docs_profile.name,
            "retrieved": manifest.docs_profile.retrieved,
            "cited": manifest.docs_profile.cited.iter().map(|d| d.id).collect::<Vec<_>>(),
        },
        "related_pages": manifest.related_pages.iter().take(MAX_COMPACT_RELATED).collect::<Vec<_>>(),
        "coverage": manifest.coverage,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": manifest.warnings,
    })
}

// ---------------------------------------------------------------------------
// HTML.
// ---------------------------------------------------------------------------

fn write_sui_migration_html(path: &Path, manifest: &SuiMigrationManifest) -> Result<()> {
    crate::export_util::write_report_page(
        path,
        SUI_MIGRATION_HTML,
        &serde_json::to_string(manifest)?,
    )
}

pub(crate) const SUI_MIGRATION_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sui migration impact</title>
<style>
__THEME__
__REPORT_CSS__
/* ===== sui migration impact (light editorial) ===== */
#overview{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.55;max-width:80ch}
.sub{color:var(--color-ink-400);max-width:76ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
h3{font:var(--type-h5);color:var(--color-ink-700);margin:18px 0 8px}
.lang{display:inline-flex;align-items:center;gap:6px;border-radius:var(--radius-pill);padding:3px 10px;margin:6px 6px 0 0;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-blue-50);color:var(--color-blue-700)}
table{width:100%;border-collapse:collapse;font:var(--type-body-sm)}
th{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;color:var(--fg-tertiary);text-align:left;padding:8px 10px;border-bottom:var(--border-hairline)}
td{padding:7px 10px;border-bottom:var(--border-hairline);color:var(--color-ink-500);vertical-align:top}
td.mono{font-family:var(--font-mono)}
.tag{display:inline-block;border-radius:var(--radius-pill);padding:2px 9px;margin:1px 4px 1px 0;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.05em;background:var(--color-surface-2);color:var(--color-ink-500)}
.tag.ethereum{color:#5a4ae3;background:rgba(98,76,255,.12)}
.tag.solana{color:#0e8a6d;background:rgba(20,241,149,.14)}
.tag.sui{color:#0a6fb8;background:rgba(77,162,255,.16)}
.tag.contracts{color:var(--color-blue-700);background:var(--color-blue-100)}
.tag.client{color:var(--color-purple-500);background:var(--color-purple-100)}
.tag.storage{color:#9a6700;background:rgba(255,193,7,.16)}
.tag.access{color:#b33059;background:rgba(255,77,128,.12)}
.tag.indexer,.tag.infra,.tag.docs{color:var(--color-ink-500);background:var(--color-surface-3)}
.effort{display:inline-block;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.05em}
.effort.rethink{color:#b33059;background:rgba(255,77,128,.12)}
.effort.rewrite{color:#9a6700;background:rgba(255,193,7,.16)}
.effort.adapt{color:#007f76;background:rgba(0,200,187,.12)}
.effort.review{color:var(--color-blue-700);background:var(--color-blue-100)}
.cls{display:inline-block;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.05em}
.cls.walrus-blob{color:#007f76;background:rgba(0,200,187,.12)}
.cls.walrus-sites{color:#007f76;background:rgba(0,200,187,.12)}
.cls.walrus-plus-seal{color:#b33059;background:rgba(255,77,128,.12)}
.cls.sui-object-state{color:var(--color-blue-700);background:var(--color-blue-100)}
.cls.keep-offchain{color:var(--color-ink-500);background:var(--color-surface-3)}
.cls.review{color:#9a6700;background:rgba(255,193,7,.16)}
.fit{display:inline-block;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase}
.fit.strong{color:#007f76;background:rgba(0,200,187,.12)}
.fit.possible{color:#9a6700;background:rgba(255,193,7,.16)}
.fit.not-needed{color:var(--color-ink-500);background:var(--color-surface-3)}
.card{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:16px;margin-top:12px}
.card strong{color:var(--color-ink-700);font-weight:500}
.card .files{margin-top:8px}
.card .files code{display:inline-block;margin:2px 6px 2px 0;font:var(--type-body-xs);font-family:var(--font-mono);background:var(--color-surface-3);border-radius:var(--radius-sm);padding:1px 7px;color:var(--color-ink-500)}
.card .docs{margin-top:8px}
.card .docs a{display:inline-block;margin:2px 8px 2px 0;font:var(--type-body-xs);color:var(--color-blue-700)}
.steps{counter-reset:step;display:grid;gap:10px}
.step{display:flex;gap:14px;border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:14px 16px}
.step .n{font:var(--type-h3);font-family:var(--font-display);color:var(--color-blue-500);line-height:1;min-width:34px}
.step .b{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.55}
.step .b b{color:var(--color-ink-700);font-weight:500;text-transform:capitalize}
.cov{display:grid;grid-template-columns:1fr 1fr;gap:16px}
@media(max-width:760px){.cov{grid-template-columns:1fr}}
.cov .box{border:var(--border-hairline);border-radius:var(--radius-md);padding:16px 18px}
.cov .box.ok{background:rgba(0,200,187,.06)}
.cov .box.gap{background:var(--color-blue-50)}
.cov h4{margin:0 0 8px;font:var(--type-h6);color:var(--color-ink-700)}
.cov li{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:4px 0 4px 16px}
details{border:var(--border-hairline);border-radius:var(--radius-md);padding:10px 14px;margin-top:10px}
summary{cursor:pointer;color:var(--color-ink-700);font:var(--type-h6)}
.doc-group h4{margin:14px 0 4px;font:var(--type-h6);color:var(--color-ink-700)}
.doc-group .doc{font:var(--type-body-sm);color:var(--color-ink-500);margin:4px 0 4px 4px;line-height:1.5}
.doc-group .doc a{color:var(--color-blue-700)}
</style>
</head>
<body data-chaos-sui-migration>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Migration<span class="sep">&rsaquo;</span><b>Sui impact</b></span><span class="sp"></span><span class="pilltag">Sui migration</span></div></div>

<header class="ov">
  <div class="wrap">
    <div class="eyebrow">Sui migration impact</div>
    <h1 id="title">Sui migration impact</h1>
    <div id="overview"></div>
    <div class="sub" id="subtitle"></div>
  </div>
</header>

<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div><div id="langs" style="margin-top:14px"></div></section>
    <section class="panel" data-sui-source><h2>Detected source stack</h2><div class="muted" style="margin-bottom:10px">Evidence-only: every row names where it was found (dependency manifests, indexed code, AST definitions, or config files present on disk).</div><div id="source-res" style="margin-bottom:10px"></div><div id="signals"></div></section>
    <section class="panel" data-sui-features><h2>Affected features</h2><div class="muted" style="margin-bottom:10px">L1 feature communities whose member files carry migration evidence &mdash; the product features a Sui migration actually touches, ordered by review priority.</div><div id="features"></div></section>
    <section class="panel" data-sui-mappings><h2>Source-chain &rarr; Sui concept mapping</h2><div class="muted" style="margin-bottom:10px">Only mappings triggered by detected evidence are listed. Each one cites the official doc that backs it.</div><div id="mappings"></div></section>
    <section class="panel" data-sui-storage><h2>Storage migration</h2><div class="muted" style="margin-bottom:10px">Storage is a first-class migration concern. Classification per flow: Walrus blob, Walrus Sites, Walrus + Seal, Sui object state, keep offchain, or review.</div><div id="storage"></div></section>
    <section class="panel" data-sui-access><h2>Access control &amp; Seal</h2><div class="muted" style="margin-bottom:10px">Seal is recommended only where an onchain decryption policy is the product rule &mdash; including explicit &ldquo;not needed&rdquo; verdicts.</div><div id="access"></div></section>
    <section class="panel" data-sui-review><h2>Review order</h2><div class="steps" id="review"></div></section>
    <section class="panel" data-sui-docs><h2>Official docs cited</h2><div class="muted" id="docs-meta" style="margin-bottom:10px"></div><div id="docs"></div></section>
    <section class="panel" data-sui-related><h2>Related feature pages</h2><div id="related"></div></section>
    <section class="panel" data-sui-coverage><h2>Coverage</h2><div class="muted" style="margin-bottom:10px">What this report reads &mdash; and what it cannot see. It maps impact; it does not generate Move code or guarantee correctness.</div><div class="cov" id="coverage"></div></section>
    <section class="panel" data-sui-provenance><h2>How this was generated</h2><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>

<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>

<script type="application/json" id="chaos-sui-migration-manifest">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-sui-migration-manifest").textContent);
__REPORT_JS__
function tag(v){return v?'<span class="tag '+esc(v)+'">'+esc(v)+'</span>':'';}
function files(list,total){if(!list||!list.length)return'';var s='<div class="files">'+list.map(function(f){return '<code>'+esc(f)+'</code>';}).join("")+'</div>';if(total>list.length)s+='<div class="muted" style="font-size:12px">+'+(total-list.length)+' more file(s)</div>';return s;}
function docs(list){if(!list||!list.length)return'';return '<div class="docs">'+list.map(function(d){return '<a href="'+esc(d.url)+'" target="_blank" rel="noopener">'+esc(d.title)+' &nearr;</a>';}).join("")+'</div>';}
document.getElementById("title").textContent=D.title||"Sui migration impact";
document.getElementById("overview").textContent=D.overview||"";
document.getElementById("subtitle").textContent=D.subtitle||"";
var T=D.totals||{};
var stat=[[T.signals||0,"signals"],[T.affected_features||0,"features affected"],[T.mappings||0,"concept mappings"],[T.storage_candidates||0,"storage flows"],[T.access_candidates||0,"access flows"],[T.docs_cited||0,"docs cited"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("");
document.getElementById("langs").innerHTML=(D.languages||[]).map(function(l){return '<span class="lang">'+esc(l.name)+' &middot; '+l.count+'</span>';}).join("");
var S=D.source||{};
var ev=Object.keys(S.evidence||{}).map(function(k){return tag(k)+' '+S.evidence[k]+' signal(s)';}).join(" &nbsp; ");
document.getElementById("source-res").innerHTML='<b>Requested:</b> '+esc(S.requested)+' &nbsp;&middot;&nbsp; <b>Resolved:</b> '+tag(S.resolved)+(ev?' &nbsp;&middot;&nbsp; '+ev:'');
var sig=document.getElementById("signals");
if((D.signals||[]).length){
  sig.innerHTML='<table><tr><th>Signal</th><th>Chain</th><th>Area</th><th>Via</th><th>Evidence</th></tr>'+
    D.signals.map(function(s){return '<tr><td><strong>'+esc(s.label)+'</strong><div class="muted" style="font-size:12px">'+esc(s.detail)+'</div></td><td>'+tag(s.chain)+'</td><td>'+tag(s.area)+'</td><td class="mono">'+esc(s.via)+'</td><td>'+files(s.files,s.files_total)+'</td></tr>';}).join("")+'</table>';
}else{sig.innerHTML='<div class="muted">No source-chain, storage, or access evidence detected.</div>';}
var feat=document.getElementById("features");
(D.affected_features||[]).forEach(function(f){
  var el=document.createElement("div");el.className="card";
  el.innerHTML='<strong>'+esc(f.label)+'</strong> <span class="muted">('+f.member_count+' members)</span><div style="margin-top:6px">'+(f.areas||[]).map(tag).join("")+'</div>'+(f.summary?'<div class="muted" style="margin-top:6px">'+esc(f.summary)+'</div>':'')+files(f.evidence_files,f.evidence_files_total)+((f.top_symbols||[]).length?'<div class="muted" style="margin-top:6px;font-size:12px">symbols: '+f.top_symbols.map(esc).join(", ")+'</div>':'');
  feat.appendChild(el);
});
if(!feat.children.length)feat.innerHTML='<div class="muted">No feature communities matched the evidence files (hierarchy missing or evidence outside any feature) &mdash; see warnings.</div>';
var map=document.getElementById("mappings");
if((D.mappings||[]).length){
  map.innerHTML='<table><tr><th>Today (source chain)</th><th>On Sui</th><th>Area</th><th>Effort</th></tr>'+
    D.mappings.map(function(m){return '<tr><td><strong>'+esc(m.source_pattern)+'</strong><div class="muted" style="font-size:12px">'+esc(m.source_chain)+'</div></td><td>'+esc(m.sui_concept)+'<div class="muted" style="font-size:12px;margin-top:4px">'+esc(m.notes)+'</div>'+docs(m.docs)+'</td><td>'+tag(m.area)+'</td><td><span class="effort '+esc(m.effort)+'">'+esc(m.effort)+'</span></td></tr>';}).join("")+'</table>';
}else{map.innerHTML='<div class="muted">No mappings triggered &mdash; no chain evidence detected.</div>';}
var sto=document.getElementById("storage");
(D.storage_candidates||[]).forEach(function(c){
  var el=document.createElement("div");el.className="card";
  el.innerHTML='<strong>'+esc(c.flow)+'</strong> <span class="cls '+esc(c.classification)+'">'+esc(c.classification)+'</span><div class="muted" style="margin-top:6px">'+esc(c.reasoning)+'</div>'+files(c.evidence_files,c.evidence_files.length)+docs(c.docs);
  sto.appendChild(el);
});
if(!sto.children.length)sto.innerHTML='<div class="muted">No storage dependencies detected in the index &mdash; nothing to classify (and no Walrus pitch without evidence).</div>';
var acc=document.getElementById("access");
(D.access_candidates||[]).forEach(function(c){
  var el=document.createElement("div");el.className="card";
  el.innerHTML='<strong>'+esc(c.pattern)+'</strong> <span class="fit '+esc(c.seal_fit)+'">Seal: '+esc(c.seal_fit)+'</span><div class="muted" style="margin-top:6px">'+esc(c.reasoning)+'</div>'+files(c.evidence_files,c.evidence_files.length)+docs(c.docs);
  acc.appendChild(el);
});
if(!acc.children.length)acc.innerHTML='<div class="muted">No encryption or gated-access evidence &mdash; Seal does not apply to this repo as indexed.</div>';
var rev=document.getElementById("review");
(D.review_order||[]).forEach(function(s){
  var el=document.createElement("div");el.className="step";
  el.innerHTML='<div class="n">'+s.order+'</div><div class="b"><b>'+esc(s.area)+'</b> &mdash; '+esc(s.instruction)+((s.features||[]).length?'<div class="muted" style="margin-top:4px;font-size:12px">features: '+s.features.map(esc).join(", ")+'</div>':'')+'</div>';
  rev.appendChild(el);
});
if(!rev.children.length)rev.innerHTML='<div class="muted">No active migration areas.</div>';
var P=D.docs_profile||{};
document.getElementById("docs-meta").textContent='Profile "'+(P.name||"")+'" — URLs verified '+(P.retrieved||"")+'. '+( (P.cited||[]).length)+' of '+(P.entries_total||0)+' entries cited by this report.';
var dg=document.getElementById("docs");var bySec={};
(P.cited||[]).forEach(function(d){(bySec[d.section]=bySec[d.section]||[]).push(d);});
dg.innerHTML=Object.keys(bySec).map(function(sec){return '<div class="doc-group"><h4>'+esc(sec)+'</h4>'+bySec[sec].map(function(d){return '<div class="doc"><a href="'+esc(d.url)+'" target="_blank" rel="noopener">'+esc(d.title)+'</a> &mdash; '+esc(d.summary)+'</div>';}).join("")+'</div>';}).join("")||'<div class="muted">No docs cited.</div>';
var rel=document.getElementById("related");
(D.related_pages||[]).forEach(function(r){var el=document.createElement("div");el.className="card";el.innerHTML='<strong>'+esc(r.title)+'</strong><div class="muted" style="font-size:12px">'+esc(r.page)+' &middot; score '+r.score+'</div>';rel.appendChild(el);});
if(!rel.children.length)rel.innerHTML='<div class="muted">No previously generated feature pages correlated with the migration surface.</div>';
var cov=document.getElementById("coverage");var C=D.coverage||{};
cov.innerHTML='<div class="box ok"><h4>Read by this report</h4><ul>'+(C.included||[]).map(function(x){return '<li>'+esc(x)+'</li>';}).join("")+'</ul></div>'+
  '<div class="box gap"><h4>Not covered</h4><ul>'+(C.not_covered||[]).map(function(x){return '<li>'+esc(x)+'</li>';}).join("")+'</ul></div>';
renderProvenance(document.getElementById("provenance"),D.provenance);
var w=document.getElementById("warnings");
(D.warnings||[]).forEach(function(x){var el=document.createElement("div");el.className="item warn";el.innerHTML='<strong>Note</strong><div>'+esc(x)+'</div>';w.appendChild(el);});
if(!w.children.length)w.innerHTML='<div class="muted">No warnings.</div>';
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(eco: &str, name: &str, manifest: &str) -> StackDependencyRow {
        StackDependencyRow {
            ecosystem: eco.into(),
            name: name.into(),
            version: "1.0.0".into(),
            section: "dependencies".into(),
            manifest: manifest.into(),
        }
    }

    #[test]
    fn rule_tables_are_internally_consistent() {
        // Every mapping id referenced by a rule must exist in MAPPINGS, and
        // every doc id cited by a mapping must resolve in the docs profile.
        for rule in DEP_RULES {
            for id in rule.mappings {
                assert!(
                    mapping(id).is_some(),
                    "dep rule {} cites unknown mapping {id}",
                    rule.pattern
                );
            }
        }
        for rule in CHUNK_RULES {
            for id in rule.mappings {
                assert!(
                    mapping(id).is_some(),
                    "chunk rule {} cites unknown mapping {id}",
                    rule.needle
                );
            }
        }
        for probe in CONFIG_PROBES {
            for id in probe.mappings {
                assert!(
                    mapping(id).is_some(),
                    "config probe {} cites unknown mapping {id}",
                    probe.file_name
                );
            }
        }
        for m in MAPPINGS {
            assert!(!m.docs.is_empty(), "mapping {} cites no docs", m.id);
            for doc_id in m.docs {
                assert!(
                    sui_docs::doc(doc_id).is_some(),
                    "mapping {} cites unknown doc {doc_id}",
                    m.id
                );
            }
            assert!(
                AREA_ORDER.contains(&m.area),
                "mapping {} has unknown area",
                m.id
            );
            assert!(
                matches!(m.effort, "rethink" | "rewrite" | "adapt" | "review"),
                "mapping {} has unknown effort {}",
                m.id,
                m.effort
            );
        }
    }

    #[test]
    fn dependency_detection_groups_and_attributes_chains() {
        let rows = vec![
            dep("npm", "hardhat", "package.json"),
            dep("npm", "@openzeppelin/contracts", "contracts/package.json"),
            dep("npm", "ethers", "client/package.json"),
            dep("npm", "@pinata/sdk", "client/package.json"),
            dep(
                "npm",
                "@lit-protocol/lit-node-client",
                "client/package.json",
            ),
            dep("cargo", "anchor-lang", "programs/x/Cargo.toml"),
            dep("npm", "left-pad", "package.json"),
        ];
        let signals = detect_dependency_signals(&rows);
        let labels: Vec<&str> = signals.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"Hardhat toolchain"));
        assert!(labels.contains(&"OpenZeppelin contracts library"));
        assert!(labels.contains(&"ethers.js client SDK"));
        assert!(labels.contains(&"Pinata pinning service"));
        assert!(labels.contains(&"Lit Protocol token-gating"));
        assert!(labels.contains(&"Anchor framework program"));
        // Unmatched dependencies produce no signal.
        assert!(!labels.iter().any(|l| l.contains("left-pad")));
        let anchor = signals
            .iter()
            .find(|s| s.label == "Anchor framework program")
            .unwrap();
        assert_eq!(anchor.chain, Some("solana"));
        assert_eq!(anchor.area, "contracts");
        assert!(anchor.mappings.contains(&"solana-rent"));
        let pinata = signals
            .iter()
            .find(|s| s.label == "Pinata pinning service")
            .unwrap();
        assert_eq!(pinata.chain, None);
        assert_eq!(pinata.files, vec!["client/package.json".to_string()]);
    }

    #[test]
    fn chunk_detection_matches_exact_substring_and_groups_by_label() {
        let rows = vec![
            (
                "contracts/Nft.sol".to_string(),
                "contract Nft is ERC721, Ownable { function tokenURI() onlyOwner {} }".to_string(),
            ),
            (
                "client/upload.ts".to_string(),
                "const url = `ipfs://${cid}`;".to_string(),
            ),
            // ILIKE prefilter superset: contains none of the needles exactly.
            ("src/other.rs".to_string(), "nothing relevant".to_string()),
        ];
        let signals = detect_chunk_signals(&rows);
        let labels: Vec<&str> = signals.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"ERC-721 usage"));
        assert!(labels.contains(&"Ownable access control"));
        assert!(labels.contains(&"Token metadata URI"));
        assert!(labels.contains(&"ipfs:// URIs"));
        let ipfs = signals.iter().find(|s| s.label == "ipfs:// URIs").unwrap();
        assert_eq!(ipfs.files, vec!["client/upload.ts".to_string()]);
        assert_eq!(ipfs.area, "storage");
    }

    #[test]
    fn source_resolution_auto_and_forced() {
        let eth = make_signal(
            "x".into(),
            Some("ethereum"),
            "contracts",
            "dependency",
            "d".into(),
            BTreeSet::new(),
            vec![],
        );
        let sol = make_signal(
            "y".into(),
            Some("solana"),
            "contracts",
            "dependency",
            "d".into(),
            BTreeSet::new(),
            vec![],
        );
        let mut warnings = Vec::new();
        let both = resolve_source("auto", &[eth.clone(), sol.clone()], &mut warnings);
        assert_eq!(both.resolved, "mixed");
        let only_eth = resolve_source("auto", std::slice::from_ref(&eth), &mut warnings);
        assert_eq!(only_eth.resolved, "ethereum");
        let none = resolve_source("auto", &[], &mut warnings);
        assert_eq!(none.resolved, "unknown");
        // Forced source that contradicts evidence warns but is respected.
        let mut forced_warnings = Vec::new();
        let forced = resolve_source("ethereum", &[sol], &mut forced_warnings);
        assert_eq!(forced.resolved, "ethereum");
        assert!(forced_warnings.iter().any(|w| w.contains("solana")));
    }

    #[test]
    fn storage_classification_is_evidence_gated() {
        // No evidence → no candidates, no Walrus pitch.
        assert!(classify_storage(&[]).is_empty());
        let ipfs = make_signal(
            "ipfs:// URIs".into(),
            None,
            "storage",
            "code-pattern",
            "d".into(),
            BTreeSet::from(["client/upload.ts".to_string()]),
            vec!["ipfs-walrus"],
        );
        let lit = make_signal(
            "Lit Protocol token-gating".into(),
            None,
            "access",
            "dependency",
            "d".into(),
            BTreeSet::from(["client/package.json".to_string()]),
            vec!["gated-content-seal"],
        );
        let candidates = classify_storage(&[ipfs, lit]);
        assert_eq!(candidates[0].classification, "walrus-blob");
        assert!(candidates
            .iter()
            .any(|c| c.classification == "walrus-plus-seal"));
        // S3 alone → keep-offchain, not a Walrus recommendation.
        let s3 = make_signal(
            "S3 object storage client".into(),
            None,
            "storage",
            "dependency",
            "d".into(),
            BTreeSet::new(),
            vec!["s3-review"],
        );
        let s3_only = classify_storage(&[s3]);
        assert_eq!(s3_only.len(), 1);
        assert_eq!(s3_only[0].classification, "keep-offchain");
    }

    #[test]
    fn access_classification_includes_not_needed_verdict() {
        let ownable = make_signal(
            "Ownable access control".into(),
            Some("ethereum"),
            "contracts",
            "code-pattern",
            "d".into(),
            BTreeSet::new(),
            vec!["evm-access-control"],
        );
        let out = classify_access(&[ownable]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seal_fit, "not-needed");
        assert!(out[0].reasoning.contains("capability"));
    }

    #[test]
    fn triggered_mappings_order_by_review_area() {
        let signals = vec![
            make_signal(
                "a".into(),
                None,
                "storage",
                "code-pattern",
                "d".into(),
                BTreeSet::new(),
                vec!["ipfs-walrus"],
            ),
            make_signal(
                "b".into(),
                Some("ethereum"),
                "contracts",
                "code-pattern",
                "d".into(),
                BTreeSet::new(),
                vec!["erc721", "evm-client-sdk"],
            ),
        ];
        let triggered = triggered_mappings(&signals);
        let ids: Vec<&str> = triggered.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["erc721", "evm-client-sdk", "ipfs-walrus"]);
    }

    #[test]
    fn review_order_starts_with_contracts_and_lists_features() {
        let features = vec![AffectedFeature {
            community_id: Uuid::nil(),
            label: "nft minting".into(),
            member_count: 9,
            areas: vec!["contracts".into(), "storage".into()],
            evidence_files: vec![],
            evidence_files_total: 0,
            top_symbols: vec![],
            summary: None,
        }];
        let signals = vec![make_signal(
            "x".into(),
            Some("ethereum"),
            "client",
            "dependency",
            "d".into(),
            BTreeSet::new(),
            vec!["evm-client-sdk"],
        )];
        let steps = build_review_order(&features, &triggered_mappings(&signals));
        assert_eq!(steps[0].area, "contracts");
        assert_eq!(steps[0].features, vec!["nft minting".to_string()]);
        assert!(steps.iter().any(|s| s.area == "client"));
        let orders: Vec<usize> = steps.iter().map(|s| s.order).collect();
        assert_eq!(orders, (1..=steps.len()).collect::<Vec<_>>());
    }

    fn test_manifest() -> SuiMigrationManifest {
        let signals = vec![make_signal(
            "ERC-721 usage".into(),
            Some("ethereum"),
            "contracts",
            "code-pattern",
            "2 chunk match(es) across 1 file(s)".into(),
            BTreeSet::from(["contracts/Nft.sol".to_string()]),
            vec!["erc721"],
        )];
        let triggered = triggered_mappings(&signals);
        let storage_candidates = classify_storage(&signals);
        let access_candidates = classify_access(&signals);
        let cited = cited_docs(&triggered, &storage_candidates, &access_candidates);
        let totals = Totals {
            signals: signals.len(),
            affected_features: 0,
            mappings: triggered.len(),
            storage_candidates: storage_candidates.len(),
            access_candidates: access_candidates.len(),
            docs_cited: cited.len(),
        };
        let source = SourceResolution {
            requested: "auto".into(),
            resolved: "ethereum".into(),
            evidence: BTreeMap::from([("ethereum".to_string(), 1)]),
        };
        SuiMigrationManifest {
            schema_version: "sui-migration-impact-1".into(),
            repo_name: "demo".into(),
            title: "demo — Sui migration impact".into(),
            subtitle: "s".into(),
            overview: compose_overview("demo", &source, &totals),
            source,
            languages: json!([{"name": "solidity", "count": 2}]),
            totals,
            signals,
            affected_features: vec![],
            mappings: triggered
                .iter()
                .map(|m| MappingView {
                    id: m.id,
                    source_chain: m.source_chain,
                    area: m.area,
                    source_pattern: m.source_pattern,
                    sui_concept: m.sui_concept,
                    notes: m.notes,
                    effort: m.effort,
                    docs: doc_refs(m.docs),
                })
                .collect(),
            storage_candidates,
            access_candidates,
            review_order: vec![],
            docs_profile: DocsProfileView {
                name: sui_docs::PROFILE_NAME,
                retrieved: sui_docs::PROFILE_RETRIEVED,
                entries_total: sui_docs::ENTRIES.len(),
                cited,
            },
            related_pages: vec![],
            coverage: Coverage::current(),
            provenance: vec![Breadcrumb::new(source::REGEX, "scan_chunks", "1 chunk")],
            warnings: vec![],
        }
    }

    #[test]
    fn compact_return_caps_and_lifts_omissions() {
        let mut manifest = test_manifest();
        for i in 0..30 {
            manifest.signals.push(make_signal(
                format!("sig{i:02}"),
                None,
                "storage",
                "code-pattern",
                "d".into(),
                BTreeSet::new(),
                vec![],
            ));
        }
        let out = compact_return(&manifest, Path::new("/tmp/x.html"), Uuid::nil(), 12);
        assert_eq!(
            out["signals"].as_array().unwrap().len(),
            MAX_COMPACT_SIGNALS
        );
        assert_eq!(out["signals_omitted"], json!(31 - MAX_COMPACT_SIGNALS));
        assert_eq!(out["status"], json!("ok"));
        assert!(!out["docs_profile"]["cited"].as_array().unwrap().is_empty());
        // Uniform rows.
        for row in out["signals"].as_array().unwrap() {
            assert!(row.get("label").is_some());
        }
    }

    #[test]
    fn html_renders_with_embedded_manifest() {
        let manifest = test_manifest();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sui-migration-impact.html");
        write_sui_migration_html(&path, &manifest).unwrap();
        let html = fs::read_to_string(&path).unwrap();
        assert!(html.contains("chaos-sui-migration-manifest"));
        assert!(html.contains("data-chaos-sui-migration"));
        assert!(html.contains("data-sui-storage"));
        assert!(html.contains("data-sui-access"));
        assert!(html.contains("Official docs cited"));
    }

    #[test]
    fn config_probe_finds_markers_and_skips_vendored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hardhat.config.ts"), "export default {}").unwrap();
        fs::create_dir_all(dir.path().join("contracts")).unwrap();
        fs::write(
            dir.path().join("contracts/foundry.toml"),
            "[profile.default]",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("node_modules/dep")).unwrap();
        fs::write(dir.path().join("node_modules/dep/Anchor.toml"), "x").unwrap();
        let (signals, crumbs) = probe_config_files(dir.path());
        let labels: Vec<&str> = signals.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"Hardhat config"));
        assert!(labels.contains(&"Foundry toolchain config"));
        // Vendored Anchor.toml must not count.
        assert!(!labels.contains(&"Anchor workspace config"));
        assert_eq!(crumbs.len(), signals.len());
        assert!(signals.iter().all(|s| s.via == "config-file"));
    }

    #[test]
    fn overview_is_deterministic_and_grounded() {
        let manifest = test_manifest();
        assert!(manifest
            .overview
            .contains("demo reads as an ethereum stack"));
        assert!(manifest.overview.contains(sui_docs::PROFILE_NAME));
        // Article agrees with the resolved stack name.
        for (resolved, expected) in [
            ("solana", "a solana stack"),
            ("mixed", "a mixed stack"),
            ("ethereum", "an ethereum stack"),
        ] {
            let source = SourceResolution {
                requested: "auto".into(),
                resolved: resolved.into(),
                evidence: BTreeMap::new(),
            };
            assert!(
                compose_overview("demo", &source, &Totals::default()).contains(expected),
                "expected '{expected}'"
            );
        }
    }
}

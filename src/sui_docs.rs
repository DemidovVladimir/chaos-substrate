//! The `sui-official` docs profile — a versioned, citable reference set of
//! OFFICIAL Sui, Walrus, and Seal documentation used by `chaos
//! sui-migration-impact`.
//!
//! This is a curated REFERENCE profile, not a crawler: every entry names an
//! official page (source URL, title, section/content group, one-line summary)
//! verified reachable on the date in [`PROFILE_RETRIEVED`]. Migration mappings
//! cite entries by [`DocEntry::id`], so each recommendation in the report
//! links to the official doc that backs it — official-docs-backed, not generic
//! LLM advice. The Move Book is included strictly as a SUPPLEMENTAL language
//! reference; migration guidance always prefers the refreshable docs.sui.io /
//! docs.wal.app / seal-docs.wal.app trees.
//!
//! The profile is deliberately static data compiled into the binary: it can be
//! reviewed in a diff, versioned with the release, and carries provenance
//! (URL + retrieved date) without adding network calls or a docs-ingestion
//! pipeline to the runtime. Refreshing it is a code change, which is exactly
//! the auditability the migration report needs.

use serde::Serialize;

/// One official documentation page the migration report may cite.
#[derive(Debug, Clone, Serialize)]
pub struct DocEntry {
    /// Stable citation id used by migration mappings (e.g. `"kiosk"`).
    pub id: &'static str,
    pub title: &'static str,
    pub url: &'static str,
    /// Content group (R1): object model, Move concepts, migration guides,
    /// PTBs, upgrades, tokens, events/indexing, Walrus, Seal, SDKs.
    pub section: &'static str,
    /// What the page covers, in one line — shown next to citations.
    pub summary: &'static str,
}

pub const PROFILE_NAME: &str = "sui-official";
/// The date every URL in this profile was last verified reachable.
pub const PROFILE_RETRIEVED: &str = "2026-06-11";

pub const ENTRIES: &[DocEntry] = &[
    // ---- Migration guides -------------------------------------------------
    DocEntry {
        id: "sui-for-ethereum",
        title: "Sui for Ethereum developers",
        url: "https://docs.sui.io/getting-started/sui-for-ethereum",
        section: "Migration guides",
        summary: "Official EVM-to-Sui mapping: accounts vs objects, Solidity vs Move, gas, tooling.",
    },
    DocEntry {
        id: "sui-for-solana",
        title: "Sui for Solana developers",
        url: "https://docs.sui.io/getting-started/sui-for-solana",
        section: "Migration guides",
        summary: "Official Solana-to-Sui mapping: accounts/PDAs vs objects, programs vs Move packages.",
    },
    // ---- Object model & ownership -----------------------------------------
    DocEntry {
        id: "object-model",
        title: "Object model",
        url: "https://docs.sui.io/concepts/object-model",
        section: "Object model & ownership",
        summary: "Everything on Sui is an object with id, version, and owner — the unit state lives in.",
    },
    DocEntry {
        id: "object-ownership",
        title: "Object ownership",
        url: "https://docs.sui.io/concepts/object-ownership",
        section: "Object model & ownership",
        summary: "Owned, shared, immutable, and wrapped objects — replaces account/signer authorization models.",
    },
    DocEntry {
        id: "dynamic-fields",
        title: "Dynamic fields",
        url: "https://docs.sui.io/concepts/dynamic-fields",
        section: "Object model & ownership",
        summary: "Attach fields/objects at runtime — the Sui answer to mappings, PDAs, and open-ended state.",
    },
    // ---- Move concepts ------------------------------------------------------
    DocEntry {
        id: "sui-move-concepts",
        title: "Sui Move concepts",
        url: "https://docs.sui.io/develop/write-move/sui-move-concepts",
        section: "Move concepts",
        summary: "How Sui Move differs from core Move: object-centric storage, entry functions, abilities.",
    },
    DocEntry {
        id: "packages",
        title: "Move packages (publish & upgrade)",
        url: "https://docs.sui.io/concepts/sui-move-concepts/packages",
        section: "Package upgrades",
        summary: "Package publishing, UpgradeCap, and upgrade compatibility rules — replaces proxy patterns.",
    },
    // ---- Transactions -------------------------------------------------------
    DocEntry {
        id: "ptb",
        title: "Programmable transaction blocks",
        url: "https://docs.sui.io/concepts/transactions/prog-txn-blocks",
        section: "Transactions (PTBs)",
        summary: "Compose up to 1024 heterogeneous calls in one atomic transaction — replaces multicall/CPI chains.",
    },
    // ---- Tokens & assets ----------------------------------------------------
    DocEntry {
        id: "coin",
        title: "Coin standard",
        url: "https://docs.sui.io/standards/coin",
        section: "Tokens & assets",
        summary: "Fungible tokens on Sui (the ERC-20 / SPL Token counterpart), TreasuryCap-based supply.",
    },
    DocEntry {
        id: "closed-loop-token",
        title: "Closed-loop token standard",
        url: "https://docs.sui.io/standards/closed-loop-token",
        section: "Tokens & assets",
        summary: "Tokens with restricted transfer/spend policies — covers Token-2022-style constraints.",
    },
    DocEntry {
        id: "kiosk",
        title: "Kiosk standard",
        url: "https://docs.sui.io/standards/kiosk",
        section: "Tokens & assets",
        summary: "Onchain commerce/trading primitive for NFTs with enforced royalty & transfer rules.",
    },
    DocEntry {
        id: "display",
        title: "Object Display standard",
        url: "https://docs.sui.io/standards/display",
        section: "Tokens & assets",
        summary: "Offchain-renderable metadata templates for objects — the tokenURI counterpart.",
    },
    DocEntry {
        id: "transfer-rules",
        title: "Custom transfer rules",
        url: "https://docs.sui.io/concepts/transfers/custom-rules",
        section: "Tokens & assets",
        summary: "Restricting how objects move — transfer policies replace ERC-721 hook/approval patterns.",
    },
    // ---- Events & indexing --------------------------------------------------
    DocEntry {
        id: "events",
        title: "Using events",
        url: "https://docs.sui.io/guides/developer/sui-101/using-events",
        section: "Events & indexing",
        summary: "Emitting and subscribing to Move events — the EVM logs / Anchor events counterpart.",
    },
    DocEntry {
        id: "graphql-rpc",
        title: "GraphQL RPC",
        url: "https://docs.sui.io/concepts/graphql-rpc",
        section: "Events & indexing",
        summary: "Querying objects, transactions, and events via GraphQL — what indexers consume on Sui.",
    },
    // ---- Economics / storage lifecycle ---------------------------------------
    DocEntry {
        id: "storage-fund",
        title: "Storage fund (fees & rebates)",
        url: "https://docs.sui.io/concepts/tokenomics/storage-fund",
        section: "Object model & ownership",
        summary: "Storage fees paid upfront and rebated on deletion — the rent-exemption counterpart.",
    },
    // ---- Walrus ---------------------------------------------------------------
    DocEntry {
        id: "walrus",
        title: "Walrus documentation",
        url: "https://docs.wal.app/",
        section: "Storage (Walrus)",
        summary: "Decentralized blob storage on Sui: erasure-coded, contract-verifiable availability.",
    },
    DocEntry {
        id: "walrus-design",
        title: "Walrus design overview",
        url: "https://docs.wal.app/design/overview.html",
        section: "Storage (Walrus)",
        summary: "Architecture, epochs, and availability guarantees — what Walrus does and does not promise.",
    },
    DocEntry {
        id: "walrus-web-api",
        title: "Walrus HTTP API",
        url: "https://docs.wal.app/usage/web-api.html",
        section: "Storage (Walrus)",
        summary: "Publisher/aggregator HTTP endpoints — the drop-in surface for IPFS-gateway-style flows.",
    },
    DocEntry {
        id: "walrus-sites",
        title: "Walrus Sites",
        url: "https://docs.wal.app/walrus-sites/intro.html",
        section: "Storage (Walrus)",
        summary: "Decentralized static websites served from Walrus — the IPFS/Fleek site-hosting counterpart.",
    },
    // ---- Seal -----------------------------------------------------------------
    DocEntry {
        id: "seal",
        title: "Seal documentation",
        url: "https://seal-docs.wal.app/",
        section: "Access control (Seal)",
        summary: "Threshold encryption with onchain access policies — gated/private content bound to Sui state.",
    },
    // ---- Client SDKs ------------------------------------------------------------
    DocEntry {
        id: "ts-sdk",
        title: "Sui TypeScript SDK",
        url: "https://sdk.mystenlabs.com/typescript",
        section: "Client SDKs",
        summary: "@mysten/sui client SDK and dapp-kit — replaces ethers/viem/wagmi and @solana/web3.js.",
    },
    // ---- Supplemental -------------------------------------------------------------
    DocEntry {
        id: "move-book",
        title: "The Move Book (supplemental)",
        url: "https://move-book.com/",
        section: "Supplemental",
        summary: "Move language reference for learning — supplemental only; migration guidance cites docs.sui.io.",
    },
];

/// Look up an entry by its citation id.
pub fn doc(id: &str) -> Option<&'static DocEntry> {
    ENTRIES.iter().find(|entry| entry.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_resolvable() {
        let mut seen = HashSet::new();
        for entry in ENTRIES {
            assert!(seen.insert(entry.id), "duplicate doc id: {}", entry.id);
            assert!(entry.url.starts_with("https://"), "{} url", entry.id);
            assert!(!entry.summary.is_empty());
            assert_eq!(doc(entry.id).unwrap().url, entry.url);
        }
        assert!(doc("nope").is_none());
    }

    #[test]
    fn required_content_groups_are_covered() {
        // R1 minimum content groups, each must have at least one entry.
        for needed in [
            "Migration guides",
            "Object model & ownership",
            "Move concepts",
            "Transactions (PTBs)",
            "Package upgrades",
            "Tokens & assets",
            "Events & indexing",
            "Storage (Walrus)",
            "Access control (Seal)",
        ] {
            assert!(
                ENTRIES.iter().any(|e| e.section == needed),
                "missing content group: {needed}"
            );
        }
    }
}

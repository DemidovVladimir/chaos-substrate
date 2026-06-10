# Sui Migration Impact Proposal

## Summary

Add a Sui migration impact workflow that helps agents assess and plan migrations from Ethereum,
Solana, IPFS, and adjacent Web3 stacks into Sui. The first deliverable should be an impact and
planning tool, not an automatic compiler.

The core idea is to combine:

- the target project's indexed source graph: contracts, clients, infra, scripts, and docs
- official Sui documentation, including Move, Sui objects, PTBs, Walrus, and Seal
- existing feature-memory pages in `docs/features_memory`
- the existing `chaos_impact`, `chaos_feature_context`, and `chaos_change_plan` patterns

The workflow should answer: "If this project moves to Sui, which existing features are affected,
which Sui primitives map to them, and what should be reviewed first?"

## Why Official Docs First

Use official docs as the primary migration source. Books can remain supplemental context, but the
tool should prefer docs that can be refreshed, versioned, cited, and split into feature-specific
chunks.

Initial source set:

- Sui docs: `https://docs.sui.io/`
- Sui Ethereum migration guide: `https://docs.sui.io/getting-started/sui-for-ethereum`
- Sui Solana migration guide: `https://docs.sui.io/getting-started/sui-for-solana`
- Sui Move concepts: `https://docs.sui.io/develop/write-move/sui-move-concepts`
- Sui stack docs for Walrus and Seal, when available from the official docs tree
- Walrus docs: `https://docs.wal.app/`
- Move Book and Move Reference only as supplemental language references: `https://move-book.com/`

This matters because a migration assistant must track current chain capabilities, SDK behavior,
storage integration patterns, and security primitives. A static book is useful for language learning,
but less appropriate as the source of truth for migration impact.

## Proposed Tool Shape

Start with a new MCP and CLI concept:

```text
chaos_sui_migration_impact
chaos sui-migration-impact <repo> --source ethereum|solana|mixed
```

Inputs:

- `repo`: indexed repository path or project selector
- `source`: `ethereum`, `solana`, `mixed`, or `auto`
- `docs_profile`: default `sui-official`
- `storage_profile`: optional `ipfs`, `arweave`, `s3`, `custom`, or `auto`
- `features_dir`: optional override for generated feature pages
- `output_html`: optional override; default should be
  `docs/features_memory/sui-migration-impact.html`

Compact MCP return:

- detected source stack
- affected feature count
- affected files and symbols, capped
- storage migration candidates
- security/access-control migration candidates
- warnings and unknowns
- path to the full HTML report

Full HTML report:

- existing feature impact map
- source-chain model to Sui model mapping
- smart contract/program migration notes
- client and SDK impact
- infra/indexer/job impact
- storage impact, including IPFS-to-Walrus candidates
- access-control and secrets impact, including Seal candidates
- risk register and review order
- provenance for code hits, docs hits, and generated feature-page matches

## Migration Dimensions

### Ethereum To Sui

Map common EVM patterns to Sui concepts:

- Solidity storage and mappings -> Sui objects, dynamic fields, tables, bags
- ERC-20/ERC-721/ERC-1155 -> Sui coin, closed-loop token, kiosk, transfer policies, object display
- `Ownable` and `AccessControl` -> capability objects and explicit object ownership
- proxy upgrades -> Sui package upgrade compatibility and upgrade capabilities
- multicall and contract composition -> programmable transaction blocks
- events and indexers -> Sui events, object queries, GraphQL/RPC/indexing changes
- IPFS token metadata or offchain blobs -> Walrus candidates
- encrypted/private offchain content -> Seal plus Walrus candidates

### Solana To Sui

Map common Solana patterns to Sui concepts:

- accounts and PDAs -> owned objects, shared objects, dynamic object fields
- Anchor instructions and account constraints -> Move entry/public functions and object checks
- signer checks -> capability objects or address checks
- CPI-heavy flows -> programmable transaction blocks or package calls
- SPL Token / Token-2022 / Metaplex metadata -> Sui coin, token, kiosk, display, transfer policies
- rent-exempt storage and account lifecycle -> Sui storage fees, rebates, object lifecycle
- program upgrade authority -> Sui upgrade capability
- offchain metadata and media -> Walrus candidates

### Storage Migration

Storage should be a first-class section, not a footnote. Many Ethereum and Solana apps use IPFS,
Arweave, S3, or custom object storage for metadata, media, proofs, documents, model artifacts, or
agent memory.

The migration report should classify each storage use:

- content-addressed public blobs: likely Walrus candidate
- static decentralized websites: likely Walrus Sites candidate
- data whose availability must be contract-verifiable: strong Walrus candidate
- encrypted content with onchain access policy: Walrus plus Seal candidate
- small mutable app state: likely Sui object or dynamic field, not Walrus
- low-latency cache: probably keep offchain cache/CDN with Sui/Walrus as source of truth

The tool should inspect frontend clients, contract metadata URIs, backend upload services, indexer
jobs, environment variables, and deployment docs to find storage dependencies.

### Seal And Access Control

Seal should be considered when the existing project has:

- encrypted files
- gated downloads
- private metadata
- token-gated access to offchain content
- per-user or per-role decryption
- secrets bound to onchain policy

The report should not blindly recommend Seal. It should explain when access is already public,
when only integrity/availability is needed, and when encryption/access control is actually part of
the product feature.

## Fit With Existing Chaos Features

This should build on existing machinery rather than create a separate migration subsystem.

Reuse:

- repository extraction for Solidity, Rust, TypeScript, JavaScript, Python, Markdown, and PDFs
- feature hierarchy and community summaries
- generated feature pages in `docs/features_memory`
- manifest correlation via `load_feature_matches`
- `chaos_impact` style compact return plus full HTML artifact
- `chaos_change_plan` style ordered review plan
- provenance breadcrumbs for every code/doc/docs-profile decision

Likely implementation path:

1. Add a docs profile abstraction for official external documentation bundles.
2. Ingest/cache official Sui, Walrus, and Seal docs as supplemental docs with provenance and version
   metadata.
3. Add a migration classifier that detects source-chain patterns and storage dependencies.
4. Add `sui_migration_impact` as a sibling to `impact.rs`, reusing feature-context retrieval.
5. Render an HTML report under `docs/features_memory`.
6. Expose CLI and MCP surfaces.
7. Only after impact reports are trustworthy, consider optional Move skeleton generation.

## Non-Goals

- Do not promise one-click automatic migration.
- Do not compile Solidity or Solana Rust directly into Move.
- Do not run Node or Python services for extraction.
- Do not generate fake vectors or offline placeholder embeddings.
- Do not treat Walrus as a replacement for every storage layer.
- Do not treat Seal as needed for public data.

## Branch And Rollback Strategy

This proposal belongs on an isolated branch because it touches product direction and may later add a
new MCP surface.

Recommended branch:

```text
codex/sui-migration-impact-proposal
```

Rollout should stay reversible:

1. proposal doc only
2. docs-profile data model
3. read-only impact report
4. CLI/MCP exposure
5. optional generator experiments behind explicit flags

Each step can be reviewed and reverted independently.

## Acceptance Criteria

The first working version is acceptable when it can:

- analyze an indexed Ethereum, Solana, or mixed Web3 repo
- identify impacted features across contracts, clients, infra, and docs
- show code and docs provenance
- distinguish smart contract migration from storage migration
- identify IPFS-like storage and explain whether Walrus fits
- identify encrypted/gated file access and explain whether Seal fits
- write a compact summary plus an HTML report in `docs/features_memory`
- avoid unsupported claims about automatic correctness


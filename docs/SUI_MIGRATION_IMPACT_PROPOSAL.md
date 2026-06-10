# Sui Migration Impact PRD

## Summary

Build a Sui migration impact workflow for Sui Overflow 2026. The product helps teams assess and
plan migrations from Ethereum, Solana, IPFS, and adjacent Web3 stacks into Sui by combining indexed
project knowledge with official Sui, Walrus, and Seal documentation. The first deliverable is an
impact and planning tool, not an automatic compiler.

The core idea is to combine:

- the target project's indexed source graph: contracts, clients, infra, scripts, and docs
- official Sui documentation, including Move, Sui objects, PTBs, Walrus, and Seal
- existing feature-memory pages in `docs/features_memory`
- the existing `chaos_impact`, `chaos_feature_context`, and `chaos_change_plan` patterns

The workflow should answer: "If this project moves to Sui, which existing features are affected,
which Sui primitives map to them, and what should be reviewed first?"

## Overflow Fit

Sui Overflow 2026 runs from May through August 2026 and positions itself around builders creating
real Sui projects across focused tracks. This product fits best as an `Infra & DevX` submission
because it improves the builder experience for teams evaluating or starting Sui migrations. It also
has credible secondary alignment with:

- `Agentic Web`: the tool is an AI agent workflow that can inspect a repo, reason over docs, and
  produce a migration plan with code-linked evidence.
- `Walrus`: the tool detects IPFS, Arweave, S3, and custom storage use, then explains whether Walrus
  or Walrus Sites fit each data flow.
- `Explorations`: the tool targets multi-chain migration and can surface cross-chain architecture
  impacts before implementation.
- `DeFi & Payments` and `Payments & Wallets`: the tool can classify token, wallet, payment, event,
  and indexer migration risks for existing EVM or Solana apps.

Submission positioning:

```text
Chaos Substrate: Sui Migration Copilot
An AI DevX tool that turns an existing Ethereum or Solana repo into a Sui migration impact report,
using official Sui, Walrus, and Seal docs plus the project's real contracts, clients, infra, and
feature graph.
```

Primary track:

```text
Infra & DevX
```

Optional specialized track angle:

```text
Walrus
```

The hackathon demo should show a real Ethereum or Solana project, run the migration impact command,
and open the generated HTML report. The report should make it obvious that Chaos understands more
than smart contracts: clients, storage, indexers, deployment scripts, docs, and correlated feature
pages are part of the migration surface.

## Problem

Teams do not migrate to Sui file-by-file. They migrate product features: assets, payments, access
control, metadata, storage, indexers, clients, deployment flows, and operational assumptions. Current
AI coding tools can explain snippets, but they usually miss how contracts, clients, infra, and docs
fit together across an existing repo.

For Sui adoption, this creates three problems:

- builders do not know which parts of their existing Ethereum or Solana app are impacted
- official docs are available but not connected to the team's actual code paths
- storage and access-control migration decisions are often discovered late

Chaos Substrate already indexes code and feature relationships. The Sui migration workflow should
turn that into a concrete migration map.

## Target Users

- Web3 teams migrating an existing Ethereum or Solana app to Sui
- hackathon teams forking or porting an existing project into Sui
- protocol teams evaluating whether Sui, Walrus, or Seal fits a product line
- agents inside Codex, Claude Code, Cursor, or other MCP clients that need source-grounded migration
  context before editing code

## Product Goals

- produce a source-grounded Sui migration impact report in minutes
- connect official Sui docs to actual affected files, symbols, and features
- classify smart contract, client, infra, indexer, storage, and access-control migration work
- make Walrus and Seal recommendations only when existing feature evidence supports them
- keep output compact for agents and rich for humans through an HTML report
- preserve Chaos hard rules: Rust runtime, Postgres/pgvector persistence, real embeddings, stdio MCP

## Non-Goals

- Do not promise one-click automatic migration.
- Do not compile Solidity or Solana Rust directly into Move.
- Do not run Node or Python services for extraction.
- Do not generate fake vectors or offline placeholder embeddings.
- Do not treat Walrus as a replacement for every storage layer.
- Do not treat Seal as needed for public data.
- Do not submit a generic chatbot. The deliverable must use the existing repo graph and produce
  verifiable artifacts.

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

## User Journey

1. A builder indexes an existing Ethereum, Solana, or mixed Web3 repo with `chaos_analyze`.
2. The builder runs `chaos_sui_migration_impact` from an MCP client or CLI.
3. Chaos detects source-chain patterns, storage dependencies, client SDK usage, infra scripts, and
   related feature-memory pages.
4. Chaos retrieves relevant official Sui, Walrus, and Seal docs from a versioned docs profile.
5. Chaos writes `docs/features_memory/sui-migration-impact.html`.
6. The builder opens the report and sees the migration split by feature and subsystem.
7. The builder can optionally run `chaos_change_plan` for a selected migration slice, such as
   "port IPFS NFT metadata to Sui objects and Walrus".

## MVP Requirements

### R1: Docs Profile

Add a `sui-official` docs profile that can ingest or reference official Sui, Walrus, and Seal docs
as supplemental documentation with provenance. The profile should store source URL, title, retrieved
timestamp, and section identity where available.

Minimum content groups:

- Sui object model and ownership
- Move concepts for Sui
- Ethereum-to-Sui guidance
- Solana-to-Sui guidance
- programmable transaction blocks
- package upgrades
- events and indexing
- Walrus storage and Walrus Sites
- Seal access control and encryption guidance, when official docs are available

### R2: Source Stack Detection

Detect and report source-chain evidence:

- Solidity, Foundry, Hardhat, OpenZeppelin, ERC standards
- Solana Rust, Anchor, SPL Token, Metaplex, PDAs
- TypeScript/JavaScript client SDKs
- backend/indexer/storage services
- deployment and environment configuration

### R3: Feature Impact Report

Reuse the `chaos_impact` pattern: return compact JSON over MCP and write full HTML under
`docs/features_memory`.

The report must include:

- affected files and symbols
- related generated feature pages
- chain migration dimensions
- storage migration candidates
- access-control and secrets candidates
- confidence and warnings
- review order
- provenance breadcrumbs

### R4: Storage Migration Classification

Classify storage dependencies into:

- likely Sui object state
- likely Walrus blob storage
- likely Walrus Sites
- likely Walrus plus Seal
- likely external cache/CDN that should remain auxiliary
- unclear, requires manual review

### R5: Demo Fixture

Create or use a small realistic demo project containing:

- one Solidity or Solana program feature
- a TypeScript client
- an IPFS-like metadata or media flow
- one deployment/config file
- docs or README references

The demo must produce a report that highlights contracts, client code, storage, and docs.

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

## Demo Script For Overflow

The short demo should be:

```bash
cargo run -- analyze /absolute/path/to/example-web3-repo
cargo run -- sui-migration-impact /absolute/path/to/example-web3-repo --source auto
open /absolute/path/to/example-web3-repo/docs/features_memory/sui-migration-impact.html
```

Demo beats:

1. show an existing Ethereum or Solana repo with contracts, client, storage, and infra
2. run the migration impact tool
3. open the report
4. click the storage section and show IPFS-to-Walrus reasoning
5. click the access-control section and show whether Seal fits
6. click an affected feature and show source-linked contract/client/infra evidence
7. show compact MCP output for agent use

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

## Judging Narrative

The product is useful even before it writes any Move code because migration risk is the expensive
part. It makes Sui adoption easier by telling builders what they need to port, which Sui concepts
apply, and where Walrus or Seal changes the architecture. It is also naturally agentic: MCP clients
can call the tool before edits, then use the generated report as source-grounded migration context.

What makes it differentiated:

- repo-aware, not only docs-aware
- feature-aware, not only file-aware
- official-docs-backed, not generic LLM advice
- storage and secrets are first-class migration concerns
- output is both agent-readable and judge/demo-friendly

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

## Open Questions

- Which sample repo should anchor the Overflow demo: Ethereum NFT/media app, Solana Anchor app, or
  a deliberately mixed project?
- Should the MVP ship docs-profile ingestion first, or use a checked-in curated docs manifest for
  hackathon reliability?
- Should `chaos_sui_migration_impact` be a dedicated tool, or should it start as a specialized mode
  of `chaos_impact`?
- How much optional Move skeleton generation is worth including for the demo without undermining the
  more credible impact-analysis story?

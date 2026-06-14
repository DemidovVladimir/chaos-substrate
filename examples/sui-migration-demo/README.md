# Onchain Gallery — Sui migration demo fixture

A small but realistic Ethereum NFT app used to demo `chaos sui-migration-impact`
(R5 of the Sui Migration Impact PRD). It deliberately exercises every detection
surface:

- **Contracts** — `contracts/contracts/GalleryNFT.sol`: an ERC-721 with
  `Ownable` access control and an `ipfs://` base URI for token metadata.
- **Client** — `client/src/mint.ts` (ethers.js mint flow + Pinata IPFS upload)
  and `client/src/gated.ts` (Lit Protocol token-gated decryption of a private
  artwork file).
- **Storage** — token metadata and media are pinned to IPFS via Pinata and read
  back through public gateways.
- **Infra** — `contracts/hardhat.config.ts` deployment toolchain config.
- **Docs** — this README describes the metadata flow, so doc chunks correlate
  with the code.

## The metadata flow

1. The artist uploads an image; `mint.ts` pins it to IPFS via Pinata and builds
   a metadata JSON (`name`, `description`, `image: ipfs://<cid>`).
2. The metadata JSON is pinned too, and `mintWithURI` stores
   `ipfs://<metadataCid>` onchain via `tokenURI`.
3. Collectors view art through the `gateway.pinata.cloud` public gateway.
4. Holders of the NFT can decrypt a bonus high-resolution file: `gated.ts`
   encrypts it with Lit Protocol under an "owns token X" condition.

## Demo script

```sh
cargo run -- analyze  $(pwd)/examples/sui-migration-demo
cargo run -- sui-migration-impact $(pwd)/examples/sui-migration-demo --source auto
open examples/sui-migration-demo/docs/features_memory/sui-migration-impact.html
```

The report should classify: the ERC-721 + Ownable contract (objects, Kiosk,
Display, capability objects), the ethers client (@mysten/sui SDK), the IPFS
flows (Walrus blob candidates), and the Lit-gated file (Walrus + Seal
candidate) — each citing official Sui / Walrus / Seal docs.

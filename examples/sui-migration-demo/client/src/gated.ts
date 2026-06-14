/**
 * Token-gated bonus content: the high-resolution artwork file is encrypted
 * with Lit Protocol under an "owns this GalleryNFT token" condition, pinned to
 * IPFS, and decrypted client-side only for current holders.
 */
import { LitNodeClient } from "@lit-protocol/lit-node-client";

export interface GatedFile {
  /** ipfs:// URI of the encrypted blob. */
  uri: string;
  ciphertext: string;
  dataToEncryptHash: string;
}

const accessControlConditions = (tokenId: string) => [
  {
    contractAddress: process.env.GALLERY_ADDRESS!,
    standardContractType: "ERC721",
    chain: "sepolia",
    method: "ownerOf",
    parameters: [tokenId],
    returnValueTest: { comparator: "=", value: ":userAddress" },
  },
];

export async function encryptBonusFile(
  lit: LitNodeClient,
  tokenId: string,
  file: Uint8Array,
): Promise<GatedFile> {
  const { ciphertext, dataToEncryptHash } = await lit.encrypt({
    accessControlConditions: accessControlConditions(tokenId),
    dataToEncrypt: file,
  });
  // The ciphertext itself is public — pinned to IPFS like any other blob;
  // only holders satisfying the condition can decrypt it.
  return { uri: `ipfs://<pinned-ciphertext-cid>`, ciphertext, dataToEncryptHash };
}

export async function decryptBonusFile(
  lit: LitNodeClient,
  tokenId: string,
  gated: GatedFile,
  sessionSigs: unknown,
): Promise<Uint8Array> {
  const { decryptedData } = await lit.decrypt({
    accessControlConditions: accessControlConditions(tokenId),
    ciphertext: gated.ciphertext,
    dataToEncryptHash: gated.dataToEncryptHash,
    sessionSigs: sessionSigs as never,
  });
  return decryptedData;
}

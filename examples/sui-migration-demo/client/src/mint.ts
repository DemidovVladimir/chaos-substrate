/**
 * Mint flow: pin artwork + metadata to IPFS via Pinata, then call
 * GalleryNFT.mintWithURI with the metadata CID.
 */
import { readFile } from "node:fs/promises";
import { Contract, JsonRpcProvider, Wallet } from "ethers";
import pinataSDK from "@pinata/sdk";

const GALLERY_ABI = [
  "function mintWithURI(address collector, string metadataCid) returns (uint256)",
  "event ArtworkMinted(uint256 indexed tokenId, address indexed collector, string metadataCid)",
];

const PUBLIC_GATEWAY = "https://gateway.pinata.cloud/ipfs/";

export interface ArtworkInput {
  title: string;
  description: string;
  imagePath: string;
  collector: string;
}

export async function mintArtwork(input: ArtworkInput): Promise<string> {
  const pinata = new pinataSDK({ pinataJWTKey: process.env.PINATA_JWT! });

  // 1. Pin the image blob.
  const image = await readFile(input.imagePath);
  const imagePin = await pinata.pinFileToIPFS(image as never, {
    pinataMetadata: { name: `${input.title}-image` },
  });

  // 2. Pin the ERC-721 metadata JSON pointing at the image.
  const metadata = {
    name: input.title,
    description: input.description,
    image: `ipfs://${imagePin.IpfsHash}`,
  };
  const metadataPin = await pinata.pinJSONToIPFS(metadata, {
    pinataMetadata: { name: `${input.title}-metadata` },
  });

  // 3. Record the metadata CID onchain.
  const provider = new JsonRpcProvider(process.env.SEPOLIA_RPC_URL);
  const curator = new Wallet(process.env.CURATOR_KEY!, provider);
  const gallery = new Contract(process.env.GALLERY_ADDRESS!, GALLERY_ABI, curator);
  const tx = await gallery.mintWithURI(input.collector, metadataPin.IpfsHash);
  await tx.wait();

  console.log(`minted — view at ${PUBLIC_GATEWAY}${metadataPin.IpfsHash}`);
  return metadataPin.IpfsHash;
}

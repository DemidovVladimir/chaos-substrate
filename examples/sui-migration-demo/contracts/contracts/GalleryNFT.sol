// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {ERC721} from "@openzeppelin/contracts/token/ERC721/ERC721.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

/// @title GalleryNFT — a curated onchain art gallery.
/// @notice Each token's metadata JSON lives on IPFS; `tokenURI` returns an
///         ipfs:// URI built from the per-token CID set at mint time.
contract GalleryNFT is ERC721, Ownable {
    uint256 public nextTokenId;

    /// tokenId => IPFS CID of the metadata JSON.
    mapping(uint256 => string) private _metadataCids;

    /// Gateway-agnostic base, e.g. "ipfs://".
    string public baseURI = "ipfs://";

    event ArtworkMinted(uint256 indexed tokenId, address indexed collector, string metadataCid);
    event BaseURIChanged(string newBaseURI);

    constructor() ERC721("Onchain Gallery", "GLRY") Ownable(msg.sender) {}

    /// @notice Curator-only mint: pins are created offchain (see client/src/mint.ts),
    ///         the resulting metadata CID is stored here.
    function mintWithURI(address collector, string calldata metadataCid)
        external
        onlyOwner
        returns (uint256 tokenId)
    {
        tokenId = nextTokenId++;
        _safeMint(collector, tokenId);
        _metadataCids[tokenId] = metadataCid;
        emit ArtworkMinted(tokenId, collector, metadataCid);
    }

    function setBaseURI(string calldata newBaseURI) external onlyOwner {
        baseURI = newBaseURI;
        emit BaseURIChanged(newBaseURI);
    }

    function tokenURI(uint256 tokenId) public view override returns (string memory) {
        _requireOwned(tokenId);
        return string.concat(baseURI, _metadataCids[tokenId]);
    }
}

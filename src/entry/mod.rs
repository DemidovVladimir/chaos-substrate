//! Driving adapters: CLI and MCP server. Wire adapters into use-cases.
pub mod cli;
pub mod doctor;
pub mod mcp;

/// Cross-track contract, end to end: the tree-sitter extractors feed
/// `analyze`, and `contract_abi` reads what the Solidity side writes
/// (`getter`, `params`, `abi_signature`, `selector`, `bases`, `special`)
/// against what the TypeScript side records (`Abi` nodes, `contract_calls`,
/// `UsesAbi` edges). Lives here because only `entry` may wire adapters into
/// application use-cases.
#[cfg(test)]
mod cross_track {
    use crate::adapters::treesitter::TreeSitterExtractor;
    use crate::application::analyze::{analyze, Analyzed};
    use crate::application::contract_abi::{
        contract_links, RepoGraph, Tier, CONTRACT_ABI_NAMED_CONFIDENCE,
    };
    use crate::domain::hash;
    use crate::domain::model::{Language, Node};
    use crate::ports::extractor::{FileInput, SourceExtractor};

    const LAB_NFT_SOL: &str = r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {ERC721} from "@openzeppelin/contracts/token/ERC721/ERC721.sol";

contract LabNFT is ERC721 {
    struct Terms {
        uint fee;
        address payable payee;
    }

    uint256 public mintFeeWei;
    mapping(uint256 => Terms) public termsOf;

    constructor() ERC721("Lab", "LAB") {}

    receive() external payable {}

    function setTerms(uint256 tokenId, Terms calldata terms) external {
        termsOf[tokenId] = terms;
    }
}
"#;

    const LAB_NFT_ABI_TS: &str = r#"export const LabNftAbi = [
  { type: "function", name: "mintFeeWei", inputs: [], outputs: [{ name: "", type: "uint256" }], stateMutability: "view" },
  { type: "function", name: "setTerms", inputs: [{ name: "tokenId", type: "uint256" }, { name: "terms", type: "tuple", components: [{ name: "fee", type: "uint256" }, { name: "payee", type: "address" }] }], outputs: [], stateMutability: "nonpayable" },
  { type: "function", name: "termsOf", inputs: [{ name: "", type: "bytes32" }], outputs: [{ name: "fee", type: "uint256" }, { name: "payee", type: "address" }], stateMutability: "view" },
] as const;
"#;

    const USE_LAB_TS: &str = r#"import { readContract, writeContract } from "viem/actions";
import { LabNftAbi } from "./abis/lab-nft";

export async function labFee(client: any, address: `0x${string}`) {
  return readContract(client, { address, abi: LabNftAbi, functionName: "mintFeeWei" });
}

export async function saveTerms(client: any, address: `0x${string}`, terms: any) {
  return writeContract(client, { address, abi: LabNftAbi, functionName: "setTerms", args: [1n, terms] });
}

export async function labTerms(client: any, address: `0x${string}`) {
  return readContract(client, { address, abi: LabNftAbi, functionName: "termsOf", args: ["0x00"] });
}

export async function pay(client: any, address: `0x${string}`) {
  return writeContract(client, { address, abi: LabNftAbi, functionName: "receive" });
}
"#;

    const USE_LAB_TEST_TS: &str = r#"import { readContract } from "viem/actions";
import { LabNftAbi } from "../abis/lab-nft";

export async function feeInTest(client: any, address: `0x${string}`) {
  return readContract(client, { address, abi: LabNftAbi, functionName: "mintFeeWei" });
}
"#;

    fn index(root: &str, files: &[(&str, Language, &str)]) -> Analyzed {
        let inputs: Vec<FileInput> = files
            .iter()
            .map(|(path, language, content)| FileInput {
                path: (*path).into(),
                language: *language,
                content: (*content).into(),
            })
            .collect();
        let extractions = TreeSitterExtractor.extract_all(&inputs);
        let pairs: Vec<_> = inputs.into_iter().zip(extractions).collect();
        analyze(hash::repo_id(root), &pairs)
    }

    fn node<'a>(a: &'a Analyzed, stable_id: &str) -> &'a Node {
        a.bundle
            .nodes
            .iter()
            .find(|n| n.stable_id == stable_id)
            .unwrap_or_else(|| panic!("no node {stable_id}"))
    }

    #[test]
    fn solidity_getter_and_ts_consumer_link_by_agreeing_signature() {
        let chain = index(
            "/test/chain",
            &[("src/LabNFT.sol", Language::Solidity, LAB_NFT_SOL)],
        );
        let web = index(
            "/test/web",
            &[
                ("src/abis/lab-nft.ts", Language::TypeScript, LAB_NFT_ABI_TS),
                ("src/use-lab.ts", Language::TypeScript, USE_LAB_TS),
                (
                    "src/__tests__/use-lab.test.ts",
                    Language::TypeScript,
                    USE_LAB_TEST_TS,
                ),
            ],
        );

        // Provider side, as the Solidity track writes it.
        let contract = node(&chain, "solidity:src/LabNFT.sol:type:LabNFT");
        assert_eq!(contract.metadata["bases"], serde_json::json!(["ERC721"]));
        let fee = node(&chain, "solidity:src/LabNFT.sol:field:LabNFT::mintFeeWei");
        assert_eq!(fee.metadata["getter"], true);
        assert_eq!(fee.metadata["abi_signature"], "mintFeeWei()");
        assert_eq!(
            fee.metadata["selector"],
            hash::selector_hex("mintFeeWei()").as_str()
        );
        let set_terms = node(&chain, "solidity:src/LabNFT.sol:method:LabNFT::setTerms");
        assert_eq!(
            set_terms.metadata["abi_signature"],
            "setTerms(uint256,(uint256,address))"
        );
        let terms_of = node(&chain, "solidity:src/LabNFT.sol:field:LabNFT::termsOf");
        assert_eq!(terms_of.metadata["abi_signature"], "termsOf(uint256)");
        let receive = node(&chain, "solidity:src/LabNFT.sol:method:LabNFT::receive");
        assert_eq!(receive.metadata["special"], "receive");

        // Consumer side, as the ABI track records it.
        let abi = node(&web, "typescript:src/abis/lab-nft.ts:abi:LabNftAbi");
        let functions = abi.metadata["abi"]["functions"].clone();
        assert_eq!(
            functions,
            serde_json::json!([
                "mintFeeWei()",
                "setTerms(uint256,(uint256,address))",
                "termsOf(bytes32)"
            ])
        );

        let links = contract_links(
            &[
                RepoGraph {
                    nodes: &web.bundle.nodes,
                    edges: &web.bundle.edges,
                },
                RepoGraph {
                    nodes: &chain.bundle.nodes,
                    edges: &chain.bundle.edges,
                },
            ],
            false,
        );
        let got: Vec<(&str, Option<&str>, Tier, &str)> = links
            .iter()
            .map(|l| {
                (
                    l.member.as_deref().unwrap_or(""),
                    l.member_node.map(|id| {
                        chain
                            .bundle
                            .nodes
                            .iter()
                            .find(|n| n.id == id)
                            .map_or("?", |n| n.stable_id.as_str())
                    }),
                    l.tier,
                    l.path.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "mintFeeWei",
                    Some("solidity:src/LabNFT.sol:field:LabNFT::mintFeeWei"),
                    Tier::Named,
                    "src/use-lab.ts"
                ),
                (
                    "setTerms",
                    Some("solidity:src/LabNFT.sol:method:LabNFT::setTerms"),
                    Tier::Named,
                    "src/use-lab.ts"
                ),
                // `receive` is no ABI function: the call only reaches the
                // contract through its unindexed base, never the Method.
                ("receive", None, Tier::Inherited, "src/use-lab.ts"),
            ],
            "termsOf(bytes32) disagrees with the getter's termsOf(uint256); the test file is no consumer"
        );
        assert_eq!(links[0].confidence, CONTRACT_ABI_NAMED_CONFIDENCE);
        assert_eq!(links[0].container, contract.id);
        assert_eq!(
            links[0].evidence(),
            "LabNFT.mintFeeWei <- readContract (src/use-lab.ts:5) via LabNftAbi [named]"
        );
    }

    /// molecule-plugin mcp/server.py (the GraphQL literals verbatim): two
    /// tools send the same two operations, written as implicit
    /// concatenations. Each tool `Defines` its own copies (`…#2` for the
    /// second), as the 1.3.1 index had them under `anon#n` names.
    const SERVER_PY: &str = r#"def issue_service_token(serviceName, expiresIn, labsUrl, addr, message_signature):
    msg = labs_graphql_call(
        "query GetServiceSignInMessage($walletAddress: String!, $serviceName: String!) "
        "{ getServiceSignInMessage(walletAddress: $walletAddress, serviceName: $serviceName) { message } }",
        {"walletAddress": addr, "serviceName": serviceName},
        "none",
        labsUrl,
    )
    tok = labs_graphql_call(
        "mutation GenerateServiceToken($serviceName: String!, $expiresIn: String!, $walletAddress: String, $messageSignature: String) "
        "{ generateServiceToken(serviceName: $serviceName, expiresIn: $expiresIn, walletAddress: $walletAddress, messageSignature: $messageSignature) "
        "{ token tokenId serviceName expiresAt isSuccess message } }",
        {"serviceName": serviceName, "expiresIn": expiresIn},
        "none",
        labsUrl,
    )
    return tok


def issue_owner_service_token(serviceName, expiresIn, labsUrl, acct, signature):
    msg_q = ("query GetServiceSignInMessage($w: String!, $s: String!) { "
             "getServiceSignInMessage(walletAddress: $w, serviceName: $s) { message } }")
    m = labs_graphql_call(msg_q, {"w": acct.address, "s": serviceName}, "api-key", labsUrl)
    tok_q = ("mutation GenerateServiceToken($s: String!, $e: String, $w: String, $m: String) { "
             "generateServiceToken(serviceName: $s, expiresIn: $e, walletAddress: $w, messageSignature: $m) "
             "{ token tokenId expiresAt isSuccess message } }")
    return labs_graphql_call(tok_q, {"s": serviceName, "e": expiresIn}, "api-key", labsUrl)
"#;

    #[test]
    fn same_named_python_operations_each_keep_their_tool() {
        use crate::domain::model::EdgeKind;
        let a = index(
            "/test/plugin",
            &[("mcp/server.py", Language::Python, SERVER_PY)],
        );
        let mut defines: Vec<(String, String)> = a
            .bundle
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Defines)
            .filter_map(|e| {
                let name = |nid| {
                    a.bundle
                        .nodes
                        .iter()
                        .find(|n| n.id == nid)
                        .map(|n| n.stable_id.clone())
                };
                Some((name(e.source_node_id)?, name(e.target_node_id)?))
            })
            .filter(|(_, t)| t.starts_with("graphql:"))
            .collect();
        defines.sort();
        let op = |q: &str| format!("graphql:mcp/server.py:operation:{q}");
        let tool = |f: &str| format!("python:mcp/server.py:function:{f}");
        assert_eq!(
            defines,
            vec![
                (
                    tool("issue_owner_service_token"),
                    op("GenerateServiceToken#2")
                ),
                (
                    tool("issue_owner_service_token"),
                    op("GetServiceSignInMessage#2")
                ),
                (tool("issue_service_token"), op("GenerateServiceToken")),
                (tool("issue_service_token"), op("GetServiceSignInMessage")),
            ]
        );
        // Both copies keep the root field they select.
        for q in ["GetServiceSignInMessage", "GetServiceSignInMessage#2"] {
            assert_eq!(
                node(&a, &op(q)).metadata["root_fields"],
                serde_json::json!(["Query.getServiceSignInMessage"])
            );
        }
    }
}

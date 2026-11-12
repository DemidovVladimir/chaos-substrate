//! Content hashing and deterministic ids.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Namespace for every uuid5 in the system (fixed, never changes).
pub const NAMESPACE: Uuid = Uuid::from_bytes([
    0x9e, 0x3a, 0x1c, 0x52, 0x7b, 0x4d, 0x4e, 0x8a, 0x9f, 0x10, 0x2c, 0x6e, 0x5d, 0x71, 0x33, 0x08,
]);

/// Lower-case hex SHA-256.
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Hash of several parts joined by newlines, in the order the caller gives.
pub fn combine(parts: &[String]) -> String {
    sha256_hex(&parts.join("\n"))
}

/// Deterministic id scoped to a repository (node/community ids), so a re-index
/// or incremental merge produces the same id for the same stable key.
pub fn scoped_id(repo_id: Uuid, key: &str) -> Uuid {
    Uuid::new_v5(&repo_id, key.as_bytes())
}

/// Deterministic repository id from its canonical root path.
pub fn repo_id(root_path: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, root_path.as_bytes())
}

/// Deterministic edge id.
pub fn edge_id(source: Uuid, target: Uuid, kind: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("{source}:{target}:{kind}").as_bytes())
}

/// Ethereum Keccak-256 (the pre-NIST padding, not SHA3-256).
pub fn keccak256(input: &[u8]) -> [u8; 32] {
    use sha3::{Digest as _, Keccak256};
    Keccak256::digest(input).into()
}

/// Function or error selector of a canonical signature: `0x` + the first 4
/// bytes of its Keccak-256, lower-case hex (`transfer(address,uint256)` →
/// `0xa9059cbb`).
pub fn selector_hex(signature: &str) -> String {
    let h = keccak256(signature.as_bytes());
    format!("0x{}", hex(&h[..4]))
}

/// Event topic 0 of a canonical signature: `0x` + the full Keccak-256.
pub fn topic_hex(signature: &str) -> String {
    format!("0x{}", hex(&keccak256(signature.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let r = repo_id("/a");
        assert_eq!(scoped_id(r, "x"), scoped_id(r, "x"));
        assert_ne!(scoped_id(r, "x"), scoped_id(repo_id("/b"), "x"));
        assert_eq!(sha256_hex("a").len(), 64);
    }

    #[test]
    fn keccak_selectors_and_topics() {
        assert_eq!(selector_hex("transfer(address,uint256)"), "0xa9059cbb");
        // onchainlabs src/types/Constants.sol ERC1271_MAGICVALUE.
        assert_eq!(
            selector_hex("isValidSignature(bytes32,bytes)"),
            "0x1626ba7e"
        );
        assert_eq!(
            topic_hex("Transfer(address,address,uint256)"),
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
        assert_eq!(
            hex(&keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }
}

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
}

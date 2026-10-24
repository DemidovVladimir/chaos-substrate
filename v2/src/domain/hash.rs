//! BLAKE3 content hashes with domain separation, so a leaf can never be
//! mistaken for an interior node of the Merkle tree:
//!
//! ```text
//! leaf(data)          = BLAKE3(0x00 ‖ data)
//! node([(label, h)…]) = BLAKE3(0x01 ‖ for each child: len(label) as u64 LE ‖ label ‖ h)
//! ```

use serde::{Serialize, Serializer};
use std::fmt;

const LEAF: u8 = 0x00;
const NODE: u8 = 0x01;

/// A 32-byte BLAKE3 digest, shown as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    /// Parse 64 hex characters; `None` for anything else.
    pub fn from_hex(hex: &str) -> Option<Digest> {
        blake3::Hash::from_hex(hex)
            .ok()
            .map(|h| Digest(*h.as_bytes()))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

/// Hash of one leaf's bytes.
pub fn leaf(data: &[u8]) -> Digest {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF]).update(data);
    Digest(*h.finalize().as_bytes())
}

/// Hash of an interior node from its ordered, labelled children. The label is
/// part of the hash, so renaming a child changes its parent.
pub fn node<'a>(children: impl IntoIterator<Item = (&'a str, Digest)>) -> Digest {
    let mut h = blake3::Hasher::new();
    h.update(&[NODE]);
    for (label, child) in children {
        h.update(&(label.len() as u64).to_le_bytes())
            .update(label.as_bytes())
            .update(&child.0);
    }
    Digest(*h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_the_hex() {
        let d = leaf(b"x");
        assert_eq!(format!("{d:?}"), format!("Digest({})", d.to_hex()));
    }

    #[test]
    fn hex_round_trip() {
        let d = leaf(b"x");
        let hex = d.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(Digest::from_hex(&hex), Some(d));
        assert_eq!(d.to_string(), hex);
        assert_eq!(
            serde_json::to_value(d).unwrap(),
            serde_json::Value::String(hex)
        );
    }

    #[test]
    fn from_hex_rejects_malformed() {
        assert_eq!(Digest::from_hex(""), None);
        assert_eq!(Digest::from_hex("zz"), None);
        assert_eq!(Digest::from_hex(&"a".repeat(63)), None);
        assert_eq!(Digest::from_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn leaf_matches_blake3_with_prefix() {
        let expected = blake3::hash(&[0u8, b'a', b'b']);
        assert_eq!(leaf(b"ab").0, *expected.as_bytes());
        assert_ne!(leaf(b"ab"), leaf(b"ba"));
    }

    #[test]
    fn leaf_and_node_are_domain_separated() {
        // A node over no children hashes just the 0x01 tag; a leaf of the same
        // bytes must differ from it and from the empty leaf.
        assert_ne!(node([]), leaf(&[]));
        assert_ne!(node([]), leaf(&[1]));
    }

    #[test]
    fn node_depends_on_order_labels_and_children() {
        let (a, b) = (leaf(b"a"), leaf(b"b"));
        let ab = node([("x", a), ("y", b)]);
        assert_eq!(ab, node([("x", a), ("y", b)]));
        assert_ne!(ab, node([("y", b), ("x", a)]), "order");
        assert_ne!(ab, node([("x", a), ("z", b)]), "label");
        assert_ne!(ab, node([("x", a), ("y", a)]), "child");
    }

    #[test]
    fn node_labels_are_length_prefixed() {
        // Without the length prefix ("ab", h) and ("a", "b"‖h…) could collide.
        let h = leaf(b"h");
        assert_ne!(node([("ab", h)]), node([("a", h), ("b", h)]));
        assert_ne!(node([("ab", h), ("", h)]), node([("a", h), ("b", h)]));
    }
}

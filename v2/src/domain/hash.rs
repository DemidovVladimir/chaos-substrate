//! BLAKE3 content hashes of two kinds, tagged so one can never equal the other:
//!
//! ```text
//! bytes(data)         = BLAKE3(0x00 ‖ data)
//! list([(label, h)…]) = BLAKE3(0x01 ‖ for each item: len(label) as u64 LE ‖ label ‖ h)
//! ```
//!
//! `bytes` hashes words (a block's fingerprint); `list` hashes an ordered,
//! labelled list of such hashes (a note, a chunk's key).

use serde::{Serialize, Serializer};
use std::fmt;

const BYTES: u8 = 0x00;
const LIST: u8 = 0x01;

/// A 32-byte BLAKE3 digest, shown as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
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

/// Hash of some bytes.
pub fn bytes(data: &[u8]) -> Digest {
    let mut h = blake3::Hasher::new();
    h.update(&[BYTES]).update(data);
    Digest(*h.finalize().as_bytes())
}

/// Hash of an ordered list of labelled hashes. The label is part of the hash,
/// so renaming an item changes the list's hash.
pub fn list<'a>(items: impl IntoIterator<Item = (&'a str, Digest)>) -> Digest {
    let mut h = blake3::Hasher::new();
    h.update(&[LIST]);
    for (label, child) in items {
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
        let d = bytes(b"x");
        assert_eq!(format!("{d:?}"), format!("Digest({})", d.to_hex()));
    }

    #[test]
    fn hex_is_64_lowercase_characters_everywhere() {
        let d = bytes(b"x");
        let hex = d.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(d.to_string(), hex);
        assert_eq!(
            serde_json::to_value(d).unwrap(),
            serde_json::Value::String(hex)
        );
    }

    #[test]
    fn bytes_matches_blake3_with_prefix() {
        let expected = blake3::hash(&[0u8, b'a', b'b']);
        assert_eq!(bytes(b"ab").0, *expected.as_bytes());
        assert_ne!(bytes(b"ab"), bytes(b"ba"));
    }

    #[test]
    fn bytes_and_list_are_tagged_apart() {
        // An empty list hashes just the 0x01 tag; bytes hashed with the other
        // tag must differ from it, the empty bytes included.
        assert_ne!(list([]), bytes(&[]));
        assert_ne!(list([]), bytes(&[1]));
    }

    #[test]
    fn list_depends_on_order_labels_and_items() {
        let (a, b) = (bytes(b"a"), bytes(b"b"));
        let ab = list([("x", a), ("y", b)]);
        assert_eq!(ab, list([("x", a), ("y", b)]));
        assert_ne!(ab, list([("y", b), ("x", a)]), "order");
        assert_ne!(ab, list([("x", a), ("z", b)]), "label");
        assert_ne!(ab, list([("x", a), ("y", a)]), "child");
    }

    #[test]
    fn list_labels_are_length_prefixed() {
        // Without the length prefix ("ab", h) and ("a", "b"‖h…) could collide.
        let h = bytes(b"h");
        assert_ne!(list([("ab", h)]), list([("a", h), ("b", h)]));
        assert_ne!(list([("ab", h), ("", h)]), list([("a", h), ("b", h)]));
    }
}

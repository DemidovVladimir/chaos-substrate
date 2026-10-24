//! Text fingerprints for exact "is this already known?" checks. Two texts get
//! the same fingerprint when they say the same words: case, punctuation,
//! Markdown symbols, Unicode compatibility forms and spacing are ignored.

use super::hash::{self, Digest};

/// Canonical form of `text`: Unicode NFKC, lowercase, every run of
/// non-alphanumeric characters (punctuation, Markdown marks, whitespace)
/// collapsed to one space, trimmed. Letters and digits of every script stay.
/// `"- **Driving licence:** B (2015)."` → `"driving licence b 2015"`.
pub fn normalize(text: &str) -> String {
    let _ = text;
    todo!()
}

/// Leaf hash of the normalized text. Empty when the text has no letters or
/// digits — such text never matches anything (see [`is_blank`]).
pub fn fingerprint(text: &str) -> Digest {
    hash::leaf(normalize(text).as_bytes())
}

/// True when `text` holds no letter or digit (nothing to fingerprint).
pub fn is_blank(text: &str) -> bool {
    let _ = text;
    todo!()
}

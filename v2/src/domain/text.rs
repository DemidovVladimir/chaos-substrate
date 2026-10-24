//! Text fingerprints for exact "is this already known?" checks. Two texts get
//! the same fingerprint when they say the same words: case, punctuation,
//! Markdown symbols, Unicode compatibility forms and spacing are ignored.

use super::hash::{self, Digest};
use unicode_normalization::UnicodeNormalization;

/// Canonical form of `text`: Unicode NFKC, lowercase, every run of
/// non-alphanumeric characters (punctuation, Markdown marks, whitespace)
/// collapsed to one space, trimmed. Letters and digits of every script stay.
/// `"- **Driving licence:** B (2015)."` → `"driving licence b 2015"`.
pub fn normalize(text: &str) -> String {
    let lower = text.nfkc().collect::<String>().to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    words.join(" ")
}

/// Leaf hash of the normalized text. Empty when the text has no letters or
/// digits — such text never matches anything (see [`is_blank`]).
pub fn fingerprint(text: &str) -> Digest {
    hash::leaf(normalize(text).as_bytes())
}

/// True when `text` holds no letter or digit (nothing to fingerprint). Same
/// answer as `normalize(text).is_empty()` without building the string:
/// lowercasing never turns a letter into a non-letter or the reverse.
pub fn is_blank(text: &str) -> bool {
    !text.nfkc().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_and_collapses_symbols() {
        assert_eq!(
            normalize("- **Driving licence:** B (2015)."),
            "driving licence b 2015"
        );
        assert_eq!(normalize("  Hello,\n\tWORLD!!  "), "hello world");
        assert_eq!(
            normalize("snake_case-and/slashes"),
            "snake case and slashes"
        );
        assert_eq!(
            normalize("[[Oil change|oil]] #car ==hi=="),
            "oil change oil car hi"
        );
        assert_eq!(normalize("x"), "x");
    }

    #[test]
    fn normalize_keeps_letters_and_digits_of_every_script() {
        assert_eq!(
            normalize("Права категории «B», 2015 г."),
            "права категории b 2015 г"
        );
        assert_eq!(normalize("ÄRZTIN Müller"), "ärztin müller");
        assert_eq!(normalize("東京タワー!"), "東京タワー");
        assert_eq!(normalize("٣ arabic-indic"), "٣ arabic indic");
    }

    #[test]
    fn normalize_applies_nfkc() {
        assert_eq!(normalize("ﬁle"), "file");
        assert_eq!(normalize("Ａ１"), "a1");
        assert_eq!(normalize("５㎏"), "5kg");
        assert_eq!(normalize("x²"), "x2");
        assert_eq!(normalize("x\u{a0}y"), "x y", "no-break space is a space");
        // A decomposed `é` composes; without NFKC the accent would split the word.
        assert_eq!(normalize("cafe\u{301} crème"), "café crème");
    }

    #[test]
    fn fingerprint_ignores_case_punctuation_markdown_and_spacing() {
        let a = fingerprint("Driving licence: B (2015)");
        assert_eq!(a, fingerprint("- **driving   LICENCE** — b, 2015."));
        assert_eq!(a, fingerprint("> _Driving_ `licence`\nB 2015"));
        assert_eq!(a, fingerprint("  driving licence b 2015  "));
        assert_ne!(a, fingerprint("Driving licence: C (2015)"));
        assert_ne!(a, fingerprint("Driving licence: B (2016)"));
        assert_ne!(
            fingerprint("ab c"),
            fingerprint("a bc"),
            "word boundaries count"
        );
    }

    #[test]
    fn fingerprint_unifies_unicode_forms_and_case() {
        assert_eq!(fingerprint("ﬁle"), fingerprint("FILE"));
        assert_eq!(fingerprint("Ａ１"), fingerprint("a1"));
        assert_eq!(fingerprint("ПРАВА: «B»"), fingerprint("права b"));
        assert_ne!(
            fingerprint("ёлка"),
            fingerprint("елка"),
            "ё and е are different letters"
        );
    }

    #[test]
    fn fingerprint_is_the_leaf_of_the_normalized_text() {
        assert_eq!(fingerprint("Hi, there!"), hash::leaf(b"hi there"));
        assert_eq!(fingerprint("**"), hash::leaf(b""));
        assert_eq!(fingerprint(""), fingerprint("  --- "));
    }

    #[test]
    fn is_blank_means_no_letter_or_digit() {
        for blank in [
            "",
            "   ",
            "\n\t ",
            "---",
            "**",
            "> - [ ] ()",
            "→ ★ ©",
            "\u{301}",
        ] {
            assert!(is_blank(blank), "{blank:?} is blank");
        }
        // `℀`, `™` and `㎏` are symbols, but NFKC spells them with letters.
        for text in ["a", "7", "Я", "- [x] ок", "²", "東", "℀", "™", "㎏"] {
            assert!(!is_blank(text), "{text:?} is not blank");
        }
    }

    #[test]
    fn is_blank_agrees_with_normalize() {
        for t in [
            "",
            " \t\n",
            "\u{a0}\u{3000}",
            "--- *** ___",
            "> [!note]",
            "→ ★ © ®",
            "\u{301}\u{307}",
            "Права",
            "ёлка",
            "0",
            "١٢٣",
            "²",
            "ﬁ",
            "Ａ",
            "℀",
            "™",
            "㎏",
            "東京",
        ] {
            assert_eq!(is_blank(t), normalize(t).is_empty(), "{t:?}");
        }
    }
}

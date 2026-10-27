//! Text fingerprints for exact "is this already known?" checks. Two block
//! texts get the same fingerprint when they say the same thing: case,
//! spacing, sentence punctuation, quotes, brackets, leftover emphasis marks
//! and Unicode compatibility forms are ignored. Every other symbol counts, so
//! `A+` and `A-`, or `≤ 140` and `≥ 140`, stay different facts, and so does a
//! colon between digits: `1:5` is a ratio, `1.5` a number.

use super::hash::{self, Digest};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Characters that never change what a fact says (after NFKC, which turns
/// full-width forms and `…` into these).
const IGNORED: &str = ".,;:!?\"'()[]{}*_`~|“”„‘’‚«»‹›—";

/// Canonical form of plain text (Markdown syntax already removed by the vault
/// reader): Unicode NFKC, lowercase, then words of letters, digits and their
/// combining marks, and every other non-ignored symbol as a word of its own,
/// joined by single spaces. A colon between two digits is kept as a word.
/// `"Driving licence: B+ (2015)."` → `"driving licence b + 2015"`.
pub fn normalize(text: &str) -> String {
    let lower: Vec<char> = text
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .chars()
        .collect();
    let mut out = String::new();
    let mut in_word = false;
    for (i, &c) in lower.iter().enumerate() {
        let letter = c.is_alphanumeric() || (in_word && is_combining_mark(c));
        if letter && in_word {
            out.push(c);
            continue;
        }
        in_word = letter;
        let digit = |at: Option<&char>| at.is_some_and(|c| c.is_numeric());
        let ratio = c == ':' && i > 0 && digit(lower.get(i - 1)) && digit(lower.get(i + 1));
        let ignored = c.is_whitespace() || IGNORED.contains(c) || is_combining_mark(c);
        if letter || ratio || !ignored {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push(c);
        }
    }
    out
}

/// Leaf hash of the normalized text; `None` when the text holds no letter or
/// digit (see [`is_blank`]), which is never stored and never matches.
pub fn fingerprint(text: &str) -> Option<Digest> {
    (!is_blank(text)).then(|| hash::bytes(normalize(text).as_bytes()))
}

/// True when `text` holds no letter or digit: nothing worth storing or
/// looking up, even if symbols remain (`---`, `→ ★`).
pub fn is_blank(text: &str) -> bool {
    !text.nfkc().any(char::is_alphanumeric)
}

/// The first words of a passage on one line, at most `max_chars` characters:
/// whitespace runs become one space, and a longer text is cut at the last
/// word boundary that fits, ending in `…`.
pub fn gist(text: &str, max_chars: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let whole = words.join(" ");
    if whole.chars().count() <= max_chars {
        return whole;
    }
    let budget = max_chars.saturating_sub(1);
    let mut out = String::new();
    for word in &words {
        let joined = out.chars().count() + usize::from(!out.is_empty()) + word.chars().count();
        if joined > budget {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        out = words[0].chars().take(budget).collect();
    }
    if max_chars > 0 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &str, b: &str) -> bool {
        fingerprint(a).unwrap() == fingerprint(b).unwrap()
    }

    #[test]
    fn normalize_lowercases_and_drops_punctuation() {
        assert_eq!(
            normalize("Driving licence: B+ (2015)."),
            "driving licence b + 2015"
        );
        assert_eq!(normalize("  Hello,\n\tWORLD!!  "), "hello world");
        assert_eq!(
            normalize("“Quoted” — [bracketed], *starred*; done…"),
            "quoted bracketed starred done"
        );
        assert_eq!(
            normalize("snake_case-and/slashes"),
            "snake case - and / slashes"
        );
        assert_eq!(normalize("x"), "x");
    }

    #[test]
    fn symbols_that_carry_meaning_are_words_of_their_own() {
        for (a, b) in [
            ("Blood type: A+", "Blood type: A-"),
            ("-2.5 dioptres", "+2.5 dioptres"),
            ("blood pressure ≤ 140", "blood pressure ≥ 140"),
            ("I write C++", "I write C#"),
            ("I write C#", "I write C"),
            ("1.5 mg", "1-5 mg"),
            ("1.5 mg", "15 mg"),
            ("x < 3", "x > 3"),
            ("50% off", "50 off"),
            ("1:5 mg", "1.5 mg"),
            ("at 10:30", "at 10 30"),
        ] {
            assert_ne!(fingerprint(a), fingerprint(b), "{a:?} vs {b:?}");
        }
        assert_eq!(normalize("Blood type: A+"), "blood type a +");
        assert_eq!(normalize("C++ and C#"), "c + + and c #");
        assert_eq!(normalize("-2.5"), "- 2 5");
        assert_eq!(normalize("24/7, 100%"), "24 / 7 100 %");
        // A colon between digits is a ratio or a time; elsewhere it is punctuation.
        assert_eq!(normalize("Ratio 1:5, at 10:30."), "ratio 1 : 5 at 10 : 30");
        assert_eq!(normalize("Dose: 5"), "dose 5");
        assert_eq!(normalize("Room 4:B"), "room 4 b");
        assert_eq!(normalize("Dose:5"), "dose 5");
        assert_eq!(normalize(":5"), "5");
        // A decimal comma reads like a decimal point.
        assert_eq!(fingerprint("1,5 mg"), fingerprint("1.5 mg"));
    }

    #[test]
    fn combining_marks_stay_inside_their_word() {
        // The virama joins the Devanagari word; it is no word break.
        assert_ne!(fingerprint("हिन्दी"), fingerprint("हिन दी"));
        assert_eq!(normalize("हिन्दी"), "हिन्दी");
        // A mark with no letter before it is nothing.
        assert_eq!(normalize("\u{301} x"), "x");
    }

    #[test]
    fn normalize_keeps_letters_and_digits_of_every_script() {
        assert_eq!(
            normalize("Права категории «B», 2015 г."),
            "права категории b 2015 г"
        );
        assert_eq!(normalize("ÄRZTIN Müller"), "ärztin müller");
        assert_eq!(normalize("東京タワー!"), "東京タワー");
        assert_eq!(normalize("٣ arabic"), "٣ arabic");
    }

    #[test]
    fn normalize_applies_nfkc() {
        assert_eq!(normalize("ﬁle"), "file");
        assert_eq!(normalize("Ａ１"), "a1");
        assert_eq!(normalize("５㎏"), "5kg");
        assert_eq!(normalize("x²"), "x2");
        assert_eq!(normalize("x\u{a0}y"), "x y", "no-break space is a space");
        assert_eq!(normalize("wait…"), "wait", "… becomes ...");
        // A decomposed `é` composes; without NFKC the accent would split the word.
        assert_eq!(normalize("cafe\u{301} crème"), "café crème");
    }

    #[test]
    fn fingerprint_ignores_case_punctuation_and_spacing() {
        assert!(same("Allergic to penicillin.", "allergic to PENICILLIN"));
        assert!(same(
            "Driving licence: B (2015)",
            "driving   LICENCE — b, 2015."
        ));
        assert!(same("Driving licence B 2015", "  driving licence b 2015  "));
        assert!(!same(
            "Driving licence: B (2015)",
            "Driving licence: C (2015)"
        ));
        assert!(!same(
            "Driving licence: B (2015)",
            "Driving licence: B (2016)"
        ));
        assert!(!same("ab c", "a bc"), "word boundaries count");
    }

    #[test]
    fn fingerprint_unifies_unicode_forms_and_case() {
        assert!(same("ﬁle", "FILE"));
        assert!(same("Ａ１", "a1"));
        assert!(same("ПРАВА: «B»", "права b"));
        assert!(!same("ёлка", "елка"), "ё and е are different letters");
    }

    #[test]
    fn fingerprint_is_the_leaf_of_the_normalized_text() {
        assert_eq!(fingerprint("Hi, there!"), Some(hash::bytes(b"hi there")));
        assert_eq!(fingerprint("A+"), Some(hash::bytes(b"a +")));
    }

    #[test]
    fn blank_text_has_no_fingerprint() {
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
            assert_eq!(fingerprint(blank), None, "{blank:?}");
        }
        // `℀`, `™` and `㎏` are symbols, but NFKC spells them with letters.
        for text in ["a", "7", "Я", "- [x] ок", "²", "東", "℀", "™", "㎏"] {
            assert!(!is_blank(text), "{text:?} is not blank");
            assert!(fingerprint(text).is_some(), "{text:?}");
        }
    }

    #[test]
    fn a_gist_is_one_line_of_the_first_words() {
        assert_eq!(
            gist("I care more\n\nabout  comfort.", 80),
            "I care more about comfort."
        );
        let long = "I care more about comfort than speed. A quiet cabin matters more to me.";
        let g = gist(long, 40);
        assert_eq!(g, "I care more about comfort than speed. A…");
        assert!(g.chars().count() <= 40);
        assert_eq!(
            gist("Сплю с 23:30 до 7:00, после кофе плохо.", 20),
            "Сплю с 23:30 до…"
        );
        assert_eq!(
            gist("Supercalifragilistic", 8),
            "Superca…",
            "one long word is cut hard"
        );
        assert_eq!(gist("  ", 10), "");
    }
}

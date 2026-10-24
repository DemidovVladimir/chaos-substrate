//! The vault: a folder of Markdown notes, as Obsidian keeps them. Notes are
//! parsed with pulldown-cmark into blocks; the folder is only ever written by
//! [`Vault::append`], atomically.

use crate::domain::note::Note;
use crate::domain::path::NotePath;
use crate::ports::Vault;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Notes larger than this are skipped (a knowledge note is never this big).
pub const MAX_NOTE_BYTES: u64 = 4 * 1024 * 1024;

pub struct ObsidianVault {
    root: PathBuf,
}

impl ObsidianVault {
    /// A vault rooted at an existing folder.
    pub fn new(root: &Path) -> Result<ObsidianVault> {
        let _ = root;
        todo!()
    }
}

/// Parse one note's Markdown into blocks (pure; no file access).
pub fn parse(path: NotePath, markdown: &str) -> Note {
    let _ = (path, markdown);
    todo!()
}

/// `existing` with `text` placed as [`Vault::append`] describes (pure).
fn place(existing: &str, heading: Option<&str>, text: &str) -> String {
    let _ = (existing, heading, text);
    todo!()
}

impl Vault for ObsidianVault {
    fn notes(&self) -> Result<Vec<Note>> {
        let _ = &self.root;
        todo!()
    }

    fn append(&self, note: &NotePath, heading: Option<&str>, text: &str) -> Result<()> {
        let _ = (note, heading, text);
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note::{Block, section};
    use std::fs;
    use tempfile::tempdir;

    fn doc(markdown: &str) -> Note {
        parse(NotePath::parse("test.md").unwrap(), markdown)
    }

    fn texts(note: &Note) -> Vec<&str> {
        note.blocks.iter().map(|b| b.text.as_str()).collect()
    }

    /// `(text, section)` of every block.
    fn outline(note: &Note) -> Vec<(&str, String)> {
        note.blocks
            .iter()
            .map(|b| (b.text.as_str(), section(&b.headings)))
            .collect()
    }

    /// `(source, lines)` of every block.
    fn spans(note: &Note) -> Vec<(&str, [u32; 2])> {
        note.blocks
            .iter()
            .map(|b| (b.source.as_str(), b.lines))
            .collect()
    }

    // ---- parse -----------------------------------------------------------

    #[test]
    fn headings_nest_and_reset() {
        let note = doc(
            "above\n\n# Car\n\nintro\n\n## Oil\n\noil text\n\n### Brand\n\nbrand text\n\n\
             ## Tyres\n\ntyre text\n\n# House\n\nhouse text\n",
        );
        assert_eq!(
            outline(&note),
            [
                ("above", String::new()),
                ("intro", "Car".into()),
                ("oil text", "Car > Oil".into()),
                ("brand text", "Car > Oil > Brand".into()),
                ("tyre text", "Car > Tyres".into()),
                ("house text", "House".into()),
            ]
        );
    }

    #[test]
    fn a_heading_pops_to_its_own_level_when_levels_are_skipped() {
        let note = doc("# A\n\n### B\n\nb\n\n### C\n\nc\n\n## D\n\nd\n");
        assert_eq!(
            outline(&note),
            [
                ("b", "A > B".to_string()),
                ("c", "A > C".into()),
                ("d", "A > D".into()),
            ]
        );
    }

    #[test]
    fn heading_text_is_plain() {
        let note = doc("## **Bold** `code` [[Target|alias]]\n\ntext\n\nSetext\n======\n\nmore\n");
        assert_eq!(
            outline(&note),
            [
                ("text", "Bold code alias".to_string()),
                ("more", "Setext".into()),
            ]
        );
    }

    #[test]
    fn list_items_are_blocks_without_their_nested_items() {
        let note = doc(
            "- parent item\n  - child item\n    more child\n  - second child\n\
             - [ ] task item\n- [x] done **bold**\n",
        );
        assert_eq!(
            texts(&note),
            [
                "parent item",
                "child item more child",
                "second child",
                "task item",
                "done bold",
            ]
        );
        assert_eq!(
            spans(&note),
            [
                ("- parent item", [1, 1]),
                ("- child item\n    more child", [2, 3]),
                ("- second child", [4, 4]),
                ("- [ ] task item", [5, 5]),
                ("- [x] done **bold**", [6, 6]),
            ]
        );
    }

    #[test]
    fn a_loose_item_keeps_its_paragraphs_and_text_after_a_nested_list_is_kept() {
        let note =
            doc("1. first\n\n   more of first\n2. second\n\n   - nested\n\n   after nested\n");
        assert_eq!(
            texts(&note),
            ["first\nmore of first", "second", "nested", "after nested"]
        );
        assert_eq!(
            spans(&note),
            [
                ("1. first\n\n   more of first", [1, 3]),
                ("2. second", [4, 4]),
                ("- nested", [6, 6]),
                ("after nested", [8, 8]),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_code_fence_is_code_not_a_heading() {
        let note =
            doc("## Setup\n\n```sh\n# install\nmake\n```\n\nafter\n\n    indented\n    code\n");
        assert_eq!(
            outline(&note),
            [
                ("# install\nmake", "Setup".to_string()),
                ("after", "Setup".into()),
                ("indented\ncode", "Setup".into()),
            ]
        );
        assert_eq!(spans(&note)[0], ("```sh\n# install\nmake\n```", [3, 6]));
        assert_eq!(note.blocks[2].lines, [10, 11]);
    }

    #[test]
    fn front_matter_is_one_block() {
        let note = doc("---\ntitle: Car\ntags: [a, b]\n---\n# Car\n\nbody\n");
        assert_eq!(
            note.blocks[0],
            Block {
                headings: vec![],
                text: "title: Car\ntags: [a, b]".into(),
                source: "---\ntitle: Car\ntags: [a, b]\n---".into(),
                lines: [1, 4],
            }
        );
        assert_eq!(outline(&note)[1], ("body", "Car".to_string()));
        assert_eq!(note.blocks[1].lines, [7, 7]);
    }

    #[test]
    fn front_matter_after_a_byte_order_mark() {
        let note = doc("\u{feff}---\nk: v\n---\nbody\n");
        assert_eq!(spans(&note), [("---\nk: v\n---", [1, 3]), ("body", [4, 4])]);
        assert_eq!(texts(&note), ["k: v", "body"]);
    }

    #[test]
    fn a_metadata_block_mid_note_is_an_ordinary_block() {
        let note = doc("# H\n\npara\n\n---\nk: v\n---\n");
        assert_eq!(
            outline(&note),
            [("para", "H".to_string()), ("k: v", "H".into())]
        );
        assert_eq!(spans(&note)[1], ("---\nk: v\n---", [5, 7]));
    }

    #[test]
    fn plain_text_keeps_labels_and_drops_markup() {
        let note = doc(
            "See [[Oil change|the oil]] and [[Tyres]], ![[pic.png]], [site](https://x.y), \
             ![alt](a.png) and `cfg` ~~old~~ *em* <span>raw</span>.\nnext line  \nhard\\\nbreak\n",
        );
        assert_eq!(
            texts(&note),
            ["See the oil and Tyres, pic.png, site, alt and cfg old em raw. next line hard break"]
        );
    }

    #[test]
    fn a_cyrillic_note() {
        let note = doc(
            "# Здоровье\n\n## Лекарства\n\n- Аспирин 100 мг\n  - утром\n\n\
             Принимать [[Врач|после еды]].\n",
        );
        assert_eq!(
            outline(&note),
            [
                ("Аспирин 100 мг", "Здоровье > Лекарства".to_string()),
                ("утром", "Здоровье > Лекарства".into()),
                ("Принимать после еды.", "Здоровье > Лекарства".into()),
            ]
        );
        assert_eq!(
            spans(&note),
            [
                ("- Аспирин 100 мг", [5, 5]),
                ("- утром", [6, 6]),
                ("Принимать [[Врач|после еды]].", [8, 8]),
            ]
        );
    }

    #[test]
    fn a_table_is_one_block() {
        let table =
            "| Name | Dose |\n|------|------|\n| Aspirin | 100 mg |\n| Ibuprofen | 200 mg |";
        let note = doc(&format!("# T\n\n{table}\n\nafter\n"));
        assert_eq!(
            texts(&note),
            ["Name | Dose\nAspirin | 100 mg\nIbuprofen | 200 mg", "after"]
        );
        assert_eq!(spans(&note), [(table, [3, 6]), ("after", [8, 8])]);
    }

    #[test]
    fn a_blockquote_is_one_block_with_its_contents() {
        let quote = "> quote line\n> continues\n>\n> - quoted item\n> - another";
        let note = doc(&format!("# Q\n\n{quote}\n\nafter\n"));
        assert_eq!(
            outline(&note),
            [
                (
                    "quote line continues\nquoted item\nanother",
                    "Q".to_string()
                ),
                ("after", "Q".into()),
            ]
        );
        assert_eq!(spans(&note), [(quote, [3, 7]), ("after", [9, 9])]);
    }

    #[test]
    fn an_html_block_is_one_block_without_tags() {
        let note = doc("<div>\nhello <b>world</b>\n</div>\n\nafter\n");
        assert_eq!(texts(&note), ["hello world", "after"]);
        assert_eq!(
            spans(&note)[0],
            ("<div>\nhello <b>world</b>\n</div>", [1, 3])
        );
    }

    #[test]
    fn empty_blocks_and_rules_are_not_blocks() {
        let note = doc("- \n- real\n\n---\n\n![](x.png)\n\nend\n");
        assert_eq!(texts(&note), ["real", "end"]);
    }

    #[test]
    fn crlf_lines_and_sources() {
        let note = doc("# A\r\n\r\npara one\r\nline two\r\n\r\n- item\r\n");
        assert_eq!(
            outline(&note),
            [("para one line two", "A".to_string()), ("item", "A".into())]
        );
        assert_eq!(
            spans(&note),
            [("para one\r\nline two", [3, 4]), ("- item", [6, 6])]
        );
    }

    // ---- notes() ---------------------------------------------------------

    fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn open(root: &Path) -> Box<dyn Vault> {
        Box::new(ObsidianVault::new(root).unwrap())
    }

    fn paths(vault: &dyn Vault) -> Vec<String> {
        let notes = vault.notes().unwrap();
        notes.into_iter().map(|n| n.path.to_string()).collect()
    }

    fn note(raw: &str) -> NotePath {
        NotePath::parse(raw).unwrap()
    }

    #[test]
    fn new_needs_an_existing_folder() {
        let dir = tempdir().unwrap();
        assert!(ObsidianVault::new(&dir.path().join("missing")).is_err());
        write(dir.path(), "file.md", "x");
        assert!(ObsidianVault::new(&dir.path().join("file.md")).is_err());
        assert!(ObsidianVault::new(dir.path()).is_ok());
    }

    #[test]
    fn notes_are_the_visible_markdown_files_sorted_by_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "z.md", "zulu");
        write(root, "B/Deep/d.MD", "delta");
        write(root, "B/c.md", "# C\n\ngamma");
        write(root, "a.md", "alpha");
        for hidden in [
            ".obsidian/app.md",
            ".trash/old.md",
            ".chaos-kb/index.md",
            ".git/x.md",
            ".hidden.md",
            "B/.secret.md",
            "B/.drafts/y.md",
        ] {
            write(root, hidden, "hidden");
        }
        for other in ["notes.txt", "image.png", "README", "md", "B/data.json"] {
            write(root, other, "other");
        }
        write(root, "bad:name.md", "rejected by NotePath");
        write(root, " lead.md", "NotePath would rename it");

        let notes = open(root).notes().unwrap();
        let listed: Vec<_> = notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(listed, ["B/Deep/d.MD", "B/c.md", "a.md", "z.md"]);
        assert_eq!(outline(&notes[1]), [("gamma", "C".to_string())]);
        assert_eq!(texts(&notes[2]), ["alpha"]);
    }

    #[cfg(unix)]
    #[test]
    fn notes_never_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(outside.path(), "secret.md", "outside");
        write(dir.path(), "real.md", "real");
        write(dir.path(), "Folder/inner.md", "inner");
        symlink(dir.path().join("real.md"), dir.path().join("alias.md")).unwrap();
        symlink(dir.path().join("Folder"), dir.path().join("FolderLink")).unwrap();
        symlink(outside.path(), dir.path().join("Out")).unwrap();
        symlink(outside.path().join("secret.md"), dir.path().join("s.md")).unwrap();
        assert_eq!(
            paths(open(dir.path()).as_ref()),
            ["Folder/inner.md", "real.md"]
        );
    }

    #[test]
    fn notes_skip_files_over_the_size_limit() {
        let dir = tempdir().unwrap();
        let max = usize::try_from(MAX_NOTE_BYTES).unwrap();
        write(dir.path(), "max.md", "x".repeat(max));
        write(dir.path(), "over.md", "x".repeat(max + 1));
        assert_eq!(paths(open(dir.path()).as_ref()), ["max.md"]);
    }

    #[test]
    fn notes_read_invalid_utf8_lossily() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bad.md", b"ok \xff here");
        let notes = open(dir.path()).notes().unwrap();
        assert_eq!(texts(&notes[0]), ["ok \u{fffd} here"]);
    }

    // ---- place -----------------------------------------------------------

    const CAR: &str = "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\n\
                       ## Tyres\n\nWinter set.\n";

    #[test]
    fn place_into_a_new_note() {
        assert_eq!(place("", None, "Fact."), "Fact.\n");
        assert_eq!(place("", Some("Oil"), "Fact."), "## Oil\n\nFact.\n");
        assert_eq!(place("", None, "\n\n  Fact.\n\n"), "  Fact.\n");
    }

    #[test]
    fn place_without_heading_goes_at_the_end_after_one_blank_line() {
        assert_eq!(place("intro", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n\n", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n\n\n", None, "new"), "intro\n\n\nnew\n");
        assert_eq!(place(CAR, None, "new"), format!("{CAR}\nnew\n"));
    }

    #[test]
    fn place_at_the_end_of_a_section_before_the_next_heading_of_its_level() {
        assert_eq!(
            place(CAR, Some("Oil"), "Check monthly."),
            "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\nCheck monthly.\n\n\
             ## Tyres\n\nWinter set.\n"
        );
        assert_eq!(
            place(CAR, Some("  bRaNd "), "Or Mobil."),
            "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\nOr Mobil.\n\n\
             ## Tyres\n\nWinter set.\n"
        );
        assert_eq!(
            place(CAR, Some("Tyres"), "Summer set."),
            format!("{CAR}\nSummer set.\n")
        );
        assert_eq!(place(CAR, Some("car"), "Sold."), format!("{CAR}\nSold.\n"));
        assert_eq!(
            place("## A\ntext\n## B\nb\n", Some("A"), "new"),
            "## A\ntext\n\nnew\n\n## B\nb\n"
        );
        assert_eq!(
            place("## A\n## B\n", Some("A"), "new"),
            "## A\n\nnew\n\n## B\n"
        );
        assert_eq!(
            place("## Oil ##\nx\n", Some("oil"), "y"),
            "## Oil ##\nx\n\ny\n"
        );
    }

    #[test]
    fn place_adds_a_missing_heading_at_the_end() {
        assert_eq!(
            place("# Car\n\nintro\n", Some("Oil"), "Fact."),
            "# Car\n\nintro\n\n## Oil\n\nFact.\n"
        );
        assert_eq!(
            place("# Car", Some(" Oil "), "Fact."),
            "# Car\n\n## Oil\n\nFact.\n"
        );
        assert_eq!(
            place("#Oil\n", Some("Oil"), "Fact."),
            "#Oil\n\n## Oil\n\nFact.\n",
            "`#Oil` is not a heading"
        );
        assert_eq!(
            place("    ## Oil\n", Some("Oil"), "Fact."),
            "    ## Oil\n\n## Oil\n\nFact.\n",
            "an indented `## Oil` is code"
        );
    }

    #[test]
    fn place_ignores_headings_inside_code_fences() {
        let fenced = "# Notes\n\n```md\n## Oil\ninside\n```\n\n~~~\n## Oil\n~~~\n\ntail\n";
        assert_eq!(
            place(fenced, Some("Oil"), "Fact."),
            format!("{fenced}\n## Oil\n\nFact.\n")
        );
        let long = "````\n```\n## Oil\n````\n## Oil\nreal\n";
        assert_eq!(
            place(long, Some("Oil"), "Fact."),
            format!("{long}\nFact.\n"),
            "three backticks do not close a four-backtick fence"
        );
        let hidden_next = "## A\n```\n## B\n```\n";
        assert_eq!(
            place(hidden_next, Some("A"), "new"),
            format!("{hidden_next}\nnew\n")
        );
    }

    #[test]
    fn place_keeps_existing_bytes() {
        let existing = "# Здоровье  \r\n\r\n## Лекарства\t\r\n\r\n- Аспирин\r\n\r\n\r\n\
                        ## Врачи\r\nТерапевт   ";
        assert_eq!(
            place(existing, Some("лекарства"), "Витамин D"),
            "# Здоровье  \r\n\r\n## Лекарства\t\r\n\r\n- Аспирин\r\n\r\n\r\nВитамин D\n\n\
             ## Врачи\r\nТерапевт   "
        );
        for (heading, text) in [(None, "x"), (Some("ВРАЧИ"), "Хирург"), (Some("Новое"), "y")]
        {
            let placed = place(existing, heading, text);
            let (a, b) = (existing.as_bytes(), placed.as_bytes());
            let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
            let suffix = a.iter().rev().zip(b.iter().rev());
            let suffix = suffix.take_while(|(x, y)| x == y).count();
            assert!(
                prefix + suffix >= a.len() && placed.contains(text),
                "{heading:?}: {placed:?}"
            );
        }
    }

    // ---- append ------------------------------------------------------------

    /// Every entry under `root` whose name starts with `.` (temp files).
    fn dot_entries(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with('.') {
                found.push(entry.path());
            } else if entry.file_type().unwrap().is_dir() {
                found.extend(dot_entries(&entry.path()));
            }
        }
        found
    }

    #[test]
    fn append_creates_the_note_and_its_folders_then_extends_the_section() {
        let dir = tempdir().unwrap();
        let vault = open(dir.path());
        let list = note("Health/Meds/List");
        vault
            .append(&list, Some("Daily"), "Aspirin 100 mg")
            .unwrap();
        vault.append(&list, Some("daily"), "Vitamin D").unwrap();
        vault.append(&list, None, "Ask the doctor.").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("Health/Meds/List.md")).unwrap(),
            "## Daily\n\nAspirin 100 mg\n\nVitamin D\n\nAsk the doctor.\n"
        );
        let notes = vault.notes().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            texts(&notes[0]),
            ["Aspirin 100 mg", "Vitamin D", "Ask the doctor."]
        );
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[test]
    fn append_rejects_empty_text_and_multiline_headings() {
        let dir = tempdir().unwrap();
        let vault = open(dir.path());
        assert!(vault.append(&note("inbox"), None, " \n\t ").is_err());
        assert!(
            vault
                .append(&note("inbox"), Some("A\n# B"), "text")
                .is_err()
        );
        assert!(!dir.path().join("inbox.md").exists());
    }

    #[test]
    fn append_refuses_to_rewrite_a_note_that_is_not_utf8() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bin.md", b"ok \xff");
        assert!(open(dir.path()).append(&note("bin"), None, "x").is_err());
        assert_eq!(fs::read(dir.path().join("bin.md")).unwrap(), b"ok \xff");
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_folder_that_leaves_the_vault() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), dir.path().join("Escape")).unwrap();
        let vault = open(dir.path());
        assert!(vault.append(&note("Escape/x"), None, "leak").is_err());
        assert!(vault.append(&note("Escape/Sub/x"), None, "leak").is_err());
        assert_eq!(
            fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing may be created outside the vault"
        );
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_note() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(outside.path(), "target.md", "original");
        symlink(outside.path().join("target.md"), dir.path().join("link.md")).unwrap();
        assert!(open(dir.path()).append(&note("link"), None, "x").is_err());
        let target = fs::read_to_string(outside.path().join("target.md")).unwrap();
        assert_eq!(target, "original");
        let link = fs::symlink_metadata(dir.path().join("link.md")).unwrap();
        assert!(link.file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn append_keeps_the_note_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let file = dir.path().join("private.md");
        write(dir.path(), "private.md", "secret\n");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        open(dir.path())
            .append(&note("private"), None, "more")
            .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "secret\n\nmore\n");
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_vault_opened_through_a_symlink_writes_into_its_real_folder() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let links = tempdir().unwrap();
        symlink(dir.path(), links.path().join("vault")).unwrap();
        let vault = open(&links.path().join("vault"));
        vault.append(&note("A/b"), None, "fact").unwrap();
        let written = fs::read_to_string(dir.path().join("A/b.md")).unwrap();
        assert_eq!(written, "fact\n");
    }
}

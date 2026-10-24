//! chaos-kb — a personal knowledge base for agents.
//!
//! Hexagonal layout: `domain` (pure rules) ← `ports` (traits) ← `application`
//! (use-cases); `adapters` implement the ports; `entry` wires adapters into
//! the application for the CLI and the MCP server.

pub mod adapters;
pub mod application;
pub mod domain;
pub mod entry;
pub mod ports;

#[cfg(test)]
mod layering {
    use std::{fs, path::Path};

    /// Whether Rust source `text` names `layer`: as `crate::<layer>`, through
    /// `super::` (`super::super::<layer>`), or inside a `crate::{..}` group,
    /// which this crate never writes so that every path shows its layer.
    fn reaches(text: &str, layer: &str) -> bool {
        text.contains(&format!("crate::{layer}"))
            || text.contains(&format!("super::{layer}"))
            || text.contains("crate::{")
    }

    /// Files under `src/<dir>` that reach any of `forbidden`.
    fn offending(dir: &str, forbidden: &[&str]) -> Vec<String> {
        fn walk(p: &Path, forbidden: &[&str], out: &mut Vec<String>) {
            for entry in fs::read_dir(p).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, forbidden, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = fs::read_to_string(&path).unwrap();
                    for f in forbidden.iter().filter(|f| reaches(&text, f)) {
                        out.push(format!("{} reaches {f}", path.display()));
                    }
                }
            }
        }
        let mut out = Vec::new();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(dir);
        walk(&root, forbidden, &mut out);
        out
    }

    #[test]
    fn every_way_of_naming_a_layer_is_seen() {
        for text in [
            "use crate::adapters::obsidian;",
            "use super::super::adapters::obsidian;",
            "use crate::{domain::text, adapters::obsidian};",
        ] {
            assert!(reaches(text, "adapters"), "{text}");
        }
        assert!(!reaches("use crate::domain::text;", "adapters"));
        assert!(!reaches("use super::super::testing;", "adapters"));
    }

    #[test]
    fn domain_is_pure() {
        let bad = offending("domain", &["ports", "application", "adapters", "entry"]);
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn ports_depend_on_domain_only() {
        let bad = offending("ports", &["application", "adapters", "entry"]);
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn application_never_touches_adapters() {
        let bad = offending("application", &["adapters", "entry"]);
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn adapters_never_touch_application_or_entry() {
        let bad = offending("adapters", &["application", "entry"]);
        assert!(bad.is_empty(), "{bad:?}");
    }
}

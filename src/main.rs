//! Chaos Substrate 1.0 — knowledge-graph memory for multi-repository projects.
//!
//! Hexagonal layout: `domain` (pure) ← `ports` (traits) ← `application`
//! (use-cases) ; `adapters` implement ports ; `entry` wires them.

mod adapters;
mod application;
mod domain;
mod entry;
mod ports;

fn main() {
    if let Err(err) = entry::cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod layering {
    use std::{fs, path::Path};

    fn offending(dir: &str, forbidden: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(p: &Path, forbidden: &[&str], out: &mut Vec<String>) {
            for entry in fs::read_dir(p).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, forbidden, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = fs::read_to_string(&path).unwrap();
                    for f in forbidden {
                        if text.contains(&format!("crate::{f}")) {
                            out.push(format!("{} uses crate::{f}", path.display()));
                        }
                    }
                }
            }
        }
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(dir), forbidden, &mut out);
        out
    }

    #[test]
    fn domain_is_pure() {
        assert!(offending("domain", &["ports", "application", "adapters", "entry"]).is_empty());
    }

    #[test]
    fn ports_depend_on_domain_only() {
        assert!(offending("ports", &["application", "adapters", "entry"]).is_empty());
    }

    #[test]
    fn application_never_touches_adapters() {
        let bad = offending("application", &["adapters", "entry"]);
        assert!(bad.is_empty(), "{bad:?}");
    }
}

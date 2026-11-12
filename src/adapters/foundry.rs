//! Foundry layout reader: finds every directory holding `foundry.toml` or
//! `remappings.txt` and reads its remappings, `libs` and `src` into the
//! domain [`SolidityLayout`]. Neither file is indexed; they only steer import
//! resolution and which vendored directories are skipped. Never fails: an
//! unreadable or malformed file is logged and skipped.

use crate::domain::solidity::{
    normalize_join, parse_remapping, parse_remappings, FoundryProject, Remapping, SolidityLayout,
};
use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FOUNDRY_TOML: &str = "foundry.toml";
const REMAPPINGS_TXT: &str = "remappings.txt";

/// Read the Foundry projects under `root`, walking with the same rules as
/// the file listing (`.gitignore` honoured, `.git` and `skip_dirs` skipped).
pub fn read_layout(root: &Path, skip_dirs: &[String]) -> SolidityLayout {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let skip: Vec<String> = skip_dirs.to_vec();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0 || !(name == ".git" || skip.iter().any(|d| *d == name))
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        if name != FOUNDRY_TOML && name != REMAPPINGS_TXT {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        dirs.insert(rel.parent().map(Path::to_path_buf).unwrap_or_default());
    }
    let projects = dirs.iter().map(|rel| read_project(root, rel)).collect();
    SolidityLayout { projects }
}

fn read_project(root: &Path, rel: &Path) -> FoundryProject {
    let dir = rel.to_string_lossy().replace('\\', "/");
    let abs = root.join(rel);
    let mut remappings: Vec<Remapping> = Vec::new();
    let mut libs: Vec<String> = Vec::new();
    let mut src = String::new();
    let mut auto_detect = true;
    let mut toml_remaps: Vec<Remapping> = Vec::new();
    let toml_path = abs.join(FOUNDRY_TOML);
    let has_toml = toml_path.is_file();
    if has_toml {
        match std::fs::read_to_string(&toml_path).map(|t| toml::from_str::<toml::Value>(&t)) {
            Ok(Ok(v)) => {
                let profiles = v.get("profile").and_then(toml::Value::as_table);
                let default = profiles.and_then(|p| p.get("default"));
                // Other profiles first, `default` last: on a tie it wins.
                let mut ordered: Vec<&toml::Value> = profiles
                    .map(|p| {
                        p.iter()
                            .filter(|(k, _)| k.as_str() != "default")
                            .map(|(_, v)| v)
                            .collect()
                    })
                    .unwrap_or_default();
                ordered.extend(default);
                for p in &ordered {
                    for r in strings(p.get("remappings")) {
                        toml_remaps.extend(parse_remapping(&r));
                    }
                }
                libs = strings(default.and_then(|d| d.get("libs")));
                if libs.is_empty() {
                    libs = ordered
                        .iter()
                        .map(|p| strings(p.get("libs")))
                        .find(|l| !l.is_empty())
                        .unwrap_or_default();
                }
                src = default
                    .and_then(|d| d.get("src"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                auto_detect = default
                    .and_then(|d| d.get("auto_detect_remappings"))
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true);
            }
            Ok(Err(e)) => {
                tracing::warn!(path = %toml_path.display(), error = %e, "foundry.toml does not parse; skipped")
            }
            Err(e) => {
                tracing::warn!(path = %toml_path.display(), error = %e, "foundry.toml unreadable; skipped")
            }
        }
        if libs.is_empty() {
            libs = vec!["lib".to_string()];
        }
        if src.is_empty() {
            src = "src".to_string();
        }
    }
    // Lowest precedence first: auto-detected, then remappings.txt, then the
    // foundry.toml entries (they win ties, as `forge remappings` prints them).
    if has_toml && auto_detect {
        for lib in &libs {
            let Ok(entries) = std::fs::read_dir(abs.join(lib)) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().join("src").is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            for name in names {
                remappings.push(Remapping {
                    context: String::new(),
                    prefix: format!("{name}/"),
                    target: format!("{}/", normalize_join(lib, &format!("{name}/src"))),
                });
            }
        }
    }
    let txt = abs.join(REMAPPINGS_TXT);
    if txt.is_file() {
        match std::fs::read_to_string(&txt) {
            Ok(t) => remappings.extend(parse_remappings(&t)),
            Err(e) => {
                tracing::warn!(path = %txt.display(), error = %e, "remappings.txt unreadable; skipped")
            }
        }
    }
    remappings.extend(toml_remaps);
    FoundryProject {
        dir,
        remappings,
        libs,
        src,
    }
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn read_layout_merges_txt_and_toml() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::write(
            r.join("remappings.txt"),
            "@openzeppelin-contracts-5.6.0/=dependencies/@openzeppelin-contracts-5.6.0/\n",
        )
        .unwrap();
        fs::write(
            r.join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nlibs = [\"dependencies\"]\nauto_detect_remappings = false\nremappings = [\"x/=y/\"]\n\n[profile.ci]\nremappings = [\"ci/=c/\"]\n",
        )
        .unwrap();
        fs::create_dir_all(r.join("nested/sub")).unwrap();
        fs::write(r.join("nested/sub/remappings.txt"), "a/=b/\n").unwrap();
        fs::create_dir_all(r.join("node_modules/pkg")).unwrap();
        fs::write(r.join("node_modules/pkg/foundry.toml"), "").unwrap();
        let layout = read_layout(r, &["node_modules".to_string()]);
        assert_eq!(layout.projects.len(), 2, "{layout:?}");
        let root = &layout.projects[0];
        assert_eq!(root.dir, "");
        let prefixes: Vec<&str> = root.remappings.iter().map(|m| m.prefix.as_str()).collect();
        assert_eq!(prefixes, ["@openzeppelin-contracts-5.6.0/", "ci/", "x/"]);
        assert_eq!(root.libs, vec!["dependencies"]);
        assert_eq!(root.src, "src");
        let nested = &layout.projects[1];
        assert_eq!(nested.dir, "nested/sub");
        assert!(nested.libs.is_empty(), "no foundry.toml: nothing to skip");
        assert_eq!(layout.skip_paths(), vec!["dependencies"]);
    }

    #[test]
    fn read_layout_auto_detects_lib_src() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::write(r.join("foundry.toml"), "[profile.default]\n").unwrap();
        fs::create_dir_all(r.join("lib/forge-std/src")).unwrap();
        fs::create_dir_all(r.join("lib/nosrc")).unwrap();
        let layout = read_layout(r, &[]);
        let p = &layout.projects[0];
        assert_eq!(p.libs, vec!["lib"]);
        assert_eq!(
            p.remappings,
            vec![Remapping {
                context: String::new(),
                prefix: "forge-std/".into(),
                target: "lib/forge-std/src/".into(),
            }]
        );
        assert_eq!(
            layout.remap("src/A.sol", "forge-std/Test.sol").unwrap().0,
            "lib/forge-std/src/Test.sol"
        );
        assert_eq!(layout.skip_paths(), vec!["lib"]);
    }

    #[test]
    fn read_layout_tolerates_bad_toml() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::write(r.join("foundry.toml"), "[profile.default\nlibs = ").unwrap();
        fs::write(r.join("remappings.txt"), "a/=b/\n").unwrap();
        let layout = read_layout(r, &[]);
        assert_eq!(layout.projects.len(), 1);
        assert_eq!(layout.projects[0].remappings.len(), 1);
        assert_eq!(layout.projects[0].libs, vec!["lib"]);
        assert!(read_layout(&r.join("missing"), &[]).projects.is_empty());
    }
}

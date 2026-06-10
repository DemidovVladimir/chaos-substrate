//! **Prototype / spike** — structure-first feature extraction, compared against
//! the current import-graph Louvain communities.
//!
//! The thesis (see the conversation that produced this): a "feature" should come
//! from the project's own STRUCTURE — workspace packages, module folders, route
//! groups — not from clustering the import graph (which produces blobs named
//! after the most-imported third-party package and glue files that are just
//! import lists). The AST already gives us the symbols and their files; here we
//! group those by directory structure instead of by graph community.
//!
//! This is READ-ONLY and runs entirely off the EXISTING index (no re-analyze):
//! it reads nodes/files from Postgres, derives structural features, and prints a
//! side-by-side comparison with the persisted Louvain communities for the same
//! folder so the quality can be judged before committing to anything.
//!
//! Algorithm (deterministic):
//!   1. A *package root* is any directory containing a `package.json`.
//!   2. Each code file is assigned to its nearest enclosing package root.
//!   3. Within a package, the directory tree is **cut** at the highest level whose
//!      subtree holds ≤ `MAX_FILES` files — each cut node becomes one feature,
//!      named by its directory path. Big dirs split into children; small ones roll
//!      up. No graph, no embeddings, no clustering.

use crate::{layering, storage::Storage};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// A directory subtree is emitted as one feature once it holds at most this many
/// code files; larger dirs split into their children.
const MAX_FILES: usize = 25;
/// A persisted community at/above this member count is flagged as a "blob".
const BLOB_MEMBERS: i32 = 80;

/// Code file extensions considered for structural features (manifests/configs are
/// used only to locate package roots, never grouped as features themselves).
fn is_code_file(path: &str) -> bool {
    matches!(
        path.rsplit('.').next().unwrap_or(""),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "rs" | "sol"
    )
}

/// Node kinds that are real definitions (a feature's API surface), not imports.
fn is_definition(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "method"
            | "module"
            | "type_alias"
            | "deployment_resource"
    )
}

struct StructFeature {
    name: String,
    files: Vec<String>,
    symbols: Vec<String>,
    role: String,
    languages: Vec<(String, usize)>,
    is_cdk: bool,
}

/// Run the prototype comparison for one folder and print it.
pub async fn run(storage: &Storage, repo: &str, folder: &str) -> Result<()> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let folder = folder.trim().trim_matches('/');
    if folder.is_empty() {
        anyhow::bail!("pass a folder, e.g. `desci-infra` or `desci-ecosystem`");
    }

    // 1. Pull every node under the folder, grouped by file.
    let rows = storage.load_symbols_under_path(repo.id, folder).await?;
    let mut by_file: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut package_roots: BTreeSet<String> = BTreeSet::new();
    for (path, name, kind) in &rows {
        if path.ends_with("package.json") {
            package_roots.insert(dir_of(path).to_string());
        }
        by_file
            .entry(path.clone())
            .or_default()
            .push((name.clone(), kind.clone()));
    }
    let code_files: Vec<String> = by_file
        .keys()
        .filter(|p| is_code_file(p))
        .cloned()
        .collect();

    // 2. Assign each code file to its nearest enclosing package root ("" = none).
    let mut by_package: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &code_files {
        let pkg = nearest_package_root(f, &package_roots).unwrap_or_default();
        by_package.entry(pkg).or_default().push(f.clone());
    }

    // 3. Cut each package's directory tree into features.
    let mut features: Vec<StructFeature> = Vec::new();
    for (pkg, files) in &by_package {
        let root = if pkg.is_empty() { folder } else { pkg.as_str() };
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut cuts: Vec<(String, Vec<String>)> = Vec::new();
        cut_tree(root, &refs, MAX_FILES, &mut cuts);
        for (name, fs) in cuts {
            features.push(build_feature(name, fs, &by_file));
        }
    }
    features.sort_by(|a, b| b.files.len().cmp(&a.files.len()).then(a.name.cmp(&b.name)));

    // 4. The current Louvain communities touching this folder.
    let community_ids: BTreeSet<_> = storage
        .communities_under_paths(repo.id, std::slice::from_ref(&folder.to_string()))
        .await?
        .into_iter()
        .collect();
    let hierarchy = storage.load_community_hierarchy(&repo, 60).await?;
    let louvain: Vec<_> = hierarchy
        .communities
        .iter()
        .filter(|c| community_ids.contains(&c.id))
        .collect();
    let blobs = louvain
        .iter()
        .filter(|c| c.member_count >= BLOB_MEMBERS)
        .count();
    let glue = louvain
        .iter()
        .filter(|c| !c.top_members.iter().any(|(_, k, _)| is_definition(k)))
        .count();
    let louvain_sizes: Vec<i32> = {
        let mut v: Vec<i32> = louvain.iter().map(|c| c.member_count).collect();
        v.sort_unstable();
        v
    };

    // 5. Print the comparison.
    let with_symbols = features.iter().filter(|f| !f.symbols.is_empty()).count();
    let sizes: Vec<usize> = {
        let mut v: Vec<usize> = features.iter().map(|f| f.files.len()).collect();
        v.sort_unstable();
        v
    };
    println!("================ STRUCTURE-FIRST (prototype) — {folder} ================");
    println!(
        "features: {}   files: {}   (median {}/feature, max {})",
        features.len(),
        code_files.len(),
        median(&sizes),
        sizes.last().copied().unwrap_or(0),
    );
    println!(
        "with >=1 real symbol: {with_symbols}/{}   packages detected: {}   CDK features: {}",
        features.len(),
        package_roots.len(),
        features.iter().filter(|f| f.is_cdk).count(),
    );
    println!("\ntop features (name — files / symbols — role):");
    for f in features.iter().take(20) {
        let langs = f
            .languages
            .iter()
            .map(|(l, n)| format!("{l} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let syms = f
            .symbols
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {}{}\n      {} files, {} symbols [{}] {}\n      symbols: {}",
            f.name,
            if f.is_cdk { "  (CDK)" } else { "" },
            f.files.len(),
            f.symbols.len(),
            f.role,
            if langs.is_empty() {
                String::new()
            } else {
                format!("· {langs}")
            },
            if syms.is_empty() { "—".into() } else { syms },
        );
    }

    println!("\n================ CURRENT LOUVAIN (same folder) ================");
    println!(
        "communities touching {folder}: {}   median members: {}   max: {}",
        louvain.len(),
        median(
            &louvain_sizes
                .iter()
                .map(|&x| x as usize)
                .collect::<Vec<_>>()
        ),
        louvain_sizes.last().copied().unwrap_or(0),
    );
    println!(
        "blobs (>={BLOB_MEMBERS} members): {blobs}   import-list/glue (no real symbol): {glue}",
    );
    println!("\nlargest / worst communities:");
    let mut worst: Vec<_> = louvain.clone();
    worst.sort_by(|a, b| b.member_count.cmp(&a.member_count));
    for c in worst.iter().take(12) {
        let has_def = c.top_members.iter().any(|(_, k, _)| is_definition(k));
        println!(
            "  {} — {} members {}{}",
            c.label,
            c.member_count,
            if c.member_count >= BLOB_MEMBERS {
                "[BLOB] "
            } else {
                ""
            },
            if has_def { "" } else { "[import-list/glue]" },
        );
    }
    Ok(())
}

/// Build a feature from a directory subtree of files.
fn build_feature(
    name: String,
    files: Vec<String>,
    by_file: &BTreeMap<String, Vec<(String, String)>>,
) -> StructFeature {
    let mut symbols: Vec<String> = Vec::new();
    let mut langs: BTreeMap<String, usize> = BTreeMap::new();
    let mut is_cdk = false;
    for f in &files {
        if let Some(ext_lang) = lang_of(f) {
            *langs.entry(ext_lang.to_string()).or_insert(0) += 1;
        }
        if f.contains("-stack.") || f.contains("/stacks/") || f.contains("/cdk/") {
            is_cdk = true;
        }
        if let Some(members) = by_file.get(f) {
            for (n, k) in members {
                if is_definition(k) {
                    symbols.push(n.clone());
                }
            }
        }
    }
    symbols.sort();
    symbols.dedup();
    let members: Vec<(String, String, String)> = files
        .iter()
        .map(|p| (String::new(), String::new(), p.clone()))
        .collect();
    let role = layering::classify_community(&members).as_str().to_string();
    let mut languages: Vec<(String, usize)> = langs.into_iter().collect();
    languages.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    StructFeature {
        name,
        files,
        symbols,
        role,
        languages,
        is_cdk,
    }
}

/// Cut a directory tree: emit `root` as one feature when its subtree holds at most
/// `max` files; otherwise recurse into child directories (files sitting directly
/// in `root` form their own group).
fn cut_tree(root: &str, files: &[&str], max: usize, out: &mut Vec<(String, Vec<String>)>) {
    if files.len() <= max {
        out.push((
            root.to_string(),
            files.iter().map(|s| s.to_string()).collect(),
        ));
        return;
    }
    let prefix = format!("{root}/");
    let mut children: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut loose: Vec<&str> = Vec::new();
    for &f in files {
        let rest = f.strip_prefix(&prefix).unwrap_or(f);
        match rest.split_once('/') {
            Some((seg, _)) => children.entry(format!("{root}/{seg}")).or_default().push(f),
            None => loose.push(f),
        }
    }
    // If the tree can't actually be split (everything is loose), emit as one
    // feature rather than recursing forever.
    if children.is_empty() {
        out.push((
            root.to_string(),
            loose.iter().map(|s| s.to_string()).collect(),
        ));
        return;
    }
    for (child, fs) in children {
        cut_tree(&child, &fs, max, out);
    }
    if !loose.is_empty() {
        out.push((
            root.to_string(),
            loose.iter().map(|s| s.to_string()).collect(),
        ));
    }
}

/// The longest package root that is a directory-prefix of `file`.
fn nearest_package_root(file: &str, roots: &BTreeSet<String>) -> Option<String> {
    roots
        .iter()
        .filter(|r| !r.is_empty() && file.starts_with(&format!("{r}/")))
        .max_by_key(|r| r.len())
        .cloned()
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn lang_of(path: &str) -> Option<&'static str> {
    Some(match path.rsplit('.').next().unwrap_or("") {
        "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" => "Python",
        "rs" => "Rust",
        "sol" => "Solidity",
        _ => return None,
    })
}

fn median(sorted: &[usize]) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    sorted[sorted.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_tree_splits_big_dirs_and_rolls_up_small() {
        // 30 files under app/, split across two subtrees of 15 each → two features,
        // not one blob.
        let mut files: Vec<String> = Vec::new();
        for i in 0..15 {
            files.push(format!("app/tokens/f{i}.ts"));
            files.push(format!("app/holders/g{i}.ts"));
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        cut_tree("app", &refs, 25, &mut out);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"app/tokens"));
        assert!(names.contains(&"app/holders"));
        assert!(!names.contains(&"app"), "big dir must not be one blob");
    }

    #[test]
    fn cut_tree_keeps_small_dir_whole() {
        let files = ["lib/a.ts", "lib/b.ts", "lib/c.ts"];
        let mut out = Vec::new();
        cut_tree("lib", &files, 25, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "lib");
        assert_eq!(out[0].1.len(), 3);
    }

    #[test]
    fn nearest_package_root_picks_deepest() {
        let roots: BTreeSet<String> = ["packages/ui".to_string(), "packages/ui/sub".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            nearest_package_root("packages/ui/sub/x.ts", &roots).as_deref(),
            Some("packages/ui/sub")
        );
        assert_eq!(
            nearest_package_root("packages/ui/y.ts", &roots).as_deref(),
            Some("packages/ui")
        );
        assert_eq!(nearest_package_root("apps/other/z.ts", &roots), None);
    }
}

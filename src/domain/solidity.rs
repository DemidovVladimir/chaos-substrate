//! Solidity project layout and ABI type rules. Pure: no I/O.
//!
//! * [`SolidityLayout`]: the Foundry projects of a repository (their
//!   remappings and dependency directories), read by `adapters::foundry` and
//!   used by `application::analyze` to resolve `import "src/X.sol"` and
//!   `import "@scope/pkg/X.sol"` the way solc does.
//! * [`elementary`] / [`split_array_suffix`]: the canonical spelling of ABI
//!   types (`uint` → `uint256`, `address payable` → `address`), the input of
//!   canonical signatures, selectors and event topics.

use serde::Serialize;

/// One `[context:]prefix=target` remapping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Remapping {
    /// Importer path prefix (relative to the project dir) the remapping is
    /// limited to; empty = every importer.
    pub context: String,
    pub prefix: String,
    /// Replacement for `prefix`, relative to the project dir.
    pub target: String,
}

/// A directory holding `foundry.toml` and/or `remappings.txt`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FoundryProject {
    /// Repo-relative directory; `""` = the repository root.
    pub dir: String,
    /// In precedence order: on a tie the later entry wins (solc rule).
    pub remappings: Vec<Remapping>,
    /// Dependency directories relative to `dir` (foundry.toml `libs`, `["lib"]`
    /// by default). Empty when the directory has no foundry.toml.
    pub libs: Vec<String>,
    /// foundry.toml `src` (default `src`); empty without a foundry.toml.
    pub src: String,
}

/// Every Foundry project of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SolidityLayout {
    pub projects: Vec<FoundryProject>,
}

/// Parse one remapping line; `None` for blanks, `#` / `//` comments and lines
/// without `=`.
pub fn parse_remapping(line: &str) -> Option<Remapping> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
        return None;
    }
    let (lhs, target) = line.split_once('=')?;
    let (context, prefix) = lhs.split_once(':').unwrap_or(("", lhs));
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return None;
    }
    Some(Remapping {
        context: context.trim().to_string(),
        prefix: prefix.to_string(),
        target: target.trim().to_string(),
    })
}

/// Every remapping in a `remappings.txt` body, in file order.
pub fn parse_remappings(text: &str) -> Vec<Remapping> {
    text.lines().filter_map(parse_remapping).collect()
}

/// `path` lies under `dir` (component-wise; `""` holds everything).
fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Join `rel` onto `base` and collapse `.` / `..` components.
pub fn normalize_join(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

impl SolidityLayout {
    /// The deepest project whose directory holds `path`.
    pub fn project_for(&self, path: &str) -> Option<&FoundryProject> {
        self.projects
            .iter()
            .filter(|p| under(path, &p.dir))
            .max_by_key(|p| p.dir.split('/').filter(|s| !s.is_empty()).count())
    }

    /// The solc remapping rule for `spec` imported by `importer`: among the
    /// remappings whose context prefixes the importer (relative to its project
    /// dir) and whose prefix prefixes `spec`, the longest context wins, then
    /// the longest prefix, then the one listed last. Returns the repo-relative
    /// target path and the remapping used.
    pub fn remap(&self, importer: &str, spec: &str) -> Option<(String, &Remapping)> {
        let project = self.project_for(importer)?;
        let rel = if project.dir.is_empty() {
            importer
        } else {
            importer
                .strip_prefix(project.dir.as_str())
                .map(|r| r.trim_start_matches('/'))
                .unwrap_or(importer)
        };
        let mut best: Option<&Remapping> = None;
        for r in &project.remappings {
            if !rel.starts_with(r.context.as_str()) || !spec.starts_with(r.prefix.as_str()) {
                continue;
            }
            let better = best.is_none_or(|b| {
                (r.context.len(), r.prefix.len()) >= (b.context.len(), b.prefix.len())
            });
            if better {
                best = Some(r);
            }
        }
        let r = best?;
        let target = format!("{}{}", r.target, &spec[r.prefix.len()..]);
        Some((normalize_join(&project.dir, &target), r))
    }

    /// The project directory of `importer` (`""` without a project): the
    /// base path solc resolves `src/...` imports against.
    pub fn base_dir(&self, importer: &str) -> &str {
        self.project_for(importer).map_or("", |p| p.dir.as_str())
    }

    /// Include paths for `importer`: its project's libs (`lib` by default)
    /// joined to the project dir, then `node_modules`.
    pub fn lib_dirs(&self, importer: &str) -> Vec<String> {
        let (dir, libs) = match self.project_for(importer) {
            Some(p) if !p.libs.is_empty() => (p.dir.as_str(), p.libs.clone()),
            Some(p) => (p.dir.as_str(), vec!["lib".to_string()]),
            None => ("", vec!["lib".to_string()]),
        };
        let mut out: Vec<String> = libs.iter().map(|l| normalize_join(dir, l)).collect();
        out.push(normalize_join(dir, "node_modules"));
        out
    }

    /// Vendored dependency directories (every project dir joined with each of
    /// its foundry.toml `libs`), skipped by path when listing files.
    pub fn skip_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .projects
            .iter()
            .flat_map(|p| p.libs.iter().map(|l| normalize_join(&p.dir, l)))
            .filter(|p| !p.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Canonical ABI spelling of an elementary type (`uint` → `uint256`, `int` →
/// `int256`, `address payable` → `address`, `byte` → `bytes1`, `fixed` →
/// `fixed128x18`, `ufixed` → `ufixed128x18`); other elementary types are kept.
/// `None` for anything that is not elementary (user-defined names, arrays,
/// mappings, function types).
pub fn elementary(t: &str) -> Option<String> {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let fixed = match t.as_str() {
        "uint" => Some("uint256"),
        "int" => Some("int256"),
        "address" | "address payable" => Some("address"),
        "byte" => Some("bytes1"),
        "fixed" => Some("fixed128x18"),
        "ufixed" => Some("ufixed128x18"),
        "bool" | "string" | "bytes" => Some(t.as_str()),
        _ => None,
    };
    if let Some(f) = fixed {
        return Some(f.to_string());
    }
    let bits = |n: &str| {
        n.parse::<u32>()
            .ok()
            .filter(|b| *b > 0 && *b <= 256 && b % 8 == 0)
    };
    if let Some(n) = t.strip_prefix("uint").or_else(|| t.strip_prefix("int")) {
        return bits(n).map(|_| t.clone());
    }
    if let Some(n) = t.strip_prefix("bytes") {
        return n
            .parse::<u32>()
            .ok()
            .filter(|b| (1..=32).contains(b))
            .map(|_| t.clone());
    }
    let mx = t.strip_prefix("ufixed").or_else(|| t.strip_prefix("fixed"));
    if let Some((m, n)) = mx.and_then(|r| r.split_once('x')) {
        let n_ok = n.parse::<u32>().ok().is_some_and(|n| n <= 80);
        return (bits(m).is_some() && n_ok).then(|| t.clone());
    }
    None
}

/// Split a type into its base and its array suffix: `uint[3][]` →
/// (`uint`, `[3][]`); no suffix → (`t`, `""`).
pub fn split_array_suffix(t: &str) -> (&str, &str) {
    let t = t.trim();
    match t.find('[') {
        Some(i) => (t[..i].trim_end(), &t[i..]),
        None => (t, ""),
    }
}

/// Canonical array suffix (`[ 3 ][]` → `[3][]`); `None` when a dimension is
/// not a decimal literal (a constant name, an expression).
pub fn canonical_array_suffix(suffix: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = suffix.trim();
    while !rest.is_empty() {
        let inner_end = rest.strip_prefix('[')?.find(']')?;
        let inner = rest[1..1 + inner_end].trim();
        if !inner.is_empty() && !inner.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push('[');
        out.push_str(inner);
        out.push(']');
        rest = rest[inner_end + 2..].trim_start();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(dir: &str, remaps: &str, libs: &[&str]) -> FoundryProject {
        FoundryProject {
            dir: dir.into(),
            remappings: parse_remappings(remaps),
            libs: libs.iter().map(|s| s.to_string()).collect(),
            src: "src".into(),
        }
    }

    #[test]
    fn parse_remappings_reads_context_and_skips_comments() {
        let r = parse_remappings("ctx:@a/=lib/a/\n\n# x\n// y\n@b/ = lib/b/src/\nnoequals\n");
        assert_eq!(
            r,
            vec![
                Remapping {
                    context: "ctx".into(),
                    prefix: "@a/".into(),
                    target: "lib/a/".into()
                },
                Remapping {
                    context: String::new(),
                    prefix: "@b/".into(),
                    target: "lib/b/src/".into()
                },
            ]
        );
    }

    #[test]
    fn remap_picks_longest_context_then_prefix_then_last() {
        // onchainlabs/remappings.txt:1
        let ocl = SolidityLayout {
            projects: vec![project(
                "",
                "@openzeppelin-contracts-5.6.0/=dependencies/@openzeppelin-contracts-5.6.0/",
                &["dependencies"],
            )],
        };
        let (path, r) = ocl
            .remap(
                "src/OnChainLab.sol",
                "@openzeppelin-contracts-5.6.0/proxy/ERC1967/ERC1967Proxy.sol",
            )
            .unwrap();
        assert_eq!(
            path,
            "dependencies/@openzeppelin-contracts-5.6.0/proxy/ERC1967/ERC1967Proxy.sol"
        );
        assert_eq!(r.prefix, "@openzeppelin-contracts-5.6.0/");
        // labs-contracts/remappings.txt: the longer prefix wins in both orders.
        let labs = "@openzeppelin/contracts-upgradeable/=dependencies/@openzeppelin-contracts-upgradeable-5.5.0/\n@openzeppelin/contracts/=dependencies/@openzeppelin-contracts-5.5.0/\nforge-std/=dependencies/forge-std-1.12.0/src/\n";
        let layout = SolidityLayout {
            projects: vec![project("", labs, &["dependencies"])],
        };
        let up = layout
            .remap(
                "src/AccessResolver.sol",
                "@openzeppelin/contracts-upgradeable/proxy/utils/Initializable.sol",
            )
            .unwrap();
        assert_eq!(
            up.0,
            "dependencies/@openzeppelin-contracts-upgradeable-5.5.0/proxy/utils/Initializable.sol"
        );
        let plain = layout
            .remap(
                "src/AccessResolver.sol",
                "@openzeppelin/contracts/access/Ownable.sol",
            )
            .unwrap();
        assert_eq!(
            plain.0,
            "dependencies/@openzeppelin-contracts-5.5.0/access/Ownable.sol"
        );
        let fs = layout.remap("script/Deploy.s.sol", "forge-std/Script.sol");
        assert_eq!(
            fs.unwrap().0,
            "dependencies/forge-std-1.12.0/src/Script.sol"
        );
        // Context beats a longer prefix; on a full tie the last one wins.
        let ctx = SolidityLayout {
            projects: vec![project("", "@a/=x/\n@a/=y/\ntest/:@a/=t/\n@a/b/=z/", &[])],
        };
        assert_eq!(ctx.remap("src/A.sol", "@a/b/c.sol").unwrap().0, "z/c.sol");
        assert_eq!(ctx.remap("src/A.sol", "@a/q.sol").unwrap().0, "y/q.sol");
        assert_eq!(
            ctx.remap("test/A.t.sol", "@a/b/c.sol").unwrap().0,
            "t/b/c.sol"
        );
        assert!(ctx.remap("src/A.sol", "./x.sol").is_none());
    }

    #[test]
    fn project_for_prefers_nested_dir() {
        let layout = SolidityLayout {
            projects: vec![
                project("", "", &["lib"]),
                project("contracts", "@x/=deps/x/", &["deps"]),
            ],
        };
        assert_eq!(
            layout.project_for("contracts/src/A.sol").unwrap().dir,
            "contracts"
        );
        assert_eq!(layout.project_for("contractsX/A.sol").unwrap().dir, "");
        assert_eq!(layout.base_dir("contracts/src/A.sol"), "contracts");
        assert_eq!(
            layout.remap("contracts/src/A.sol", "@x/Y.sol").unwrap().0,
            "contracts/deps/x/Y.sol"
        );
        assert_eq!(
            layout.lib_dirs("contracts/src/A.sol"),
            vec!["contracts/deps", "contracts/node_modules"]
        );
        assert_eq!(layout.skip_paths(), vec!["contracts/deps", "lib"]);
        assert_eq!(
            SolidityLayout::default().lib_dirs("A.sol"),
            vec!["lib", "node_modules"]
        );
        assert!(SolidityLayout::default().skip_paths().is_empty());
    }

    #[test]
    fn elementary_types_canonicalize() {
        for (t, want) in [
            ("uint", Some("uint256")),
            ("int", Some("int256")),
            ("address  payable", Some("address")),
            ("address", Some("address")),
            ("byte", Some("bytes1")),
            ("bytes", Some("bytes")),
            ("bytes32", Some("bytes32")),
            ("bytes33", None),
            ("uint8", Some("uint8")),
            ("uint7", None),
            ("int256", Some("int256")),
            ("fixed", Some("fixed128x18")),
            ("ufixed128x18", Some("ufixed128x18")),
            ("string", Some("string")),
            ("bool", Some("bool")),
            ("IERC20", None),
            ("uint256[]", None),
        ] {
            assert_eq!(elementary(t).as_deref(), want, "{t}");
        }
        assert_eq!(split_array_suffix("uint[3][]"), ("uint", "[3][]"));
        assert_eq!(split_array_suffix("Foo.Bar"), ("Foo.Bar", ""));
        assert_eq!(canonical_array_suffix("[ 3 ][]").as_deref(), Some("[3][]"));
        assert_eq!(canonical_array_suffix("[N]"), None);
        assert_eq!(canonical_array_suffix("").as_deref(), Some(""));
        assert_eq!(normalize_join("a/b", "../c/./d.sol"), "a/c/d.sol");
    }
}

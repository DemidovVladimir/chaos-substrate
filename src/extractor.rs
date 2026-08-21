use crate::{
    config::IndexingConfig,
    models::{
        EdgeKind, ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, Language,
        NodeKind, SourceFile,
    },
    weights::{self, EdgeWeight},
};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use syn::{Item, ItemImpl};
use uuid::Uuid;

pub struct RustRepositoryExtractor {
    indexing: IndexingConfig,
    /// Absolute directory paths to prune from the source walk, on TOP of the
    /// basename-based `indexing.skip_dirs`. Used when indexing a project-level
    /// DOCS directory (see `project add-docs`): the docs root may physically
    /// contain the project's member repos as subdirectories, and those must not
    /// be re-indexed here. Empty for ordinary repo analysis.
    prune_paths: Vec<PathBuf>,
}

/// Calibrated against the embedder context window (EmbeddingGemma: 2,048
/// tokens). Code tokenizes at roughly 3.3 chars/token, so 6,000 chars plus the
/// ~150-char context header stays under the window with margin.
const MAX_CHUNK_CHARS: usize = 6_000;

impl RustRepositoryExtractor {
    pub fn new(indexing: IndexingConfig) -> Self {
        Self {
            indexing,
            prune_paths: Vec::new(),
        }
    }

    /// Prune these absolute directory subtrees from the walk in addition to the
    /// configured `skip_dirs`. Used by `project add-docs` to keep nested member
    /// repos out of a project-docs index.
    pub fn with_prune_paths(mut self, prune_paths: Vec<PathBuf>) -> Self {
        self.prune_paths = prune_paths;
        self
    }

    /// Extract the entire repository: walk `root` for every indexable file and
    /// build the full knowledge graph + chunks.
    pub fn extract(
        &self,
        root: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
    ) -> Result<ExtractionResult> {
        let paths = self.source_paths(root);
        self.extract_files(root, repo_id, commit_sha, paths)
    }

    /// Extract only the supplied paths (used by incremental `chaos add`). Paths
    /// that are not indexable source/doc files, or that no longer exist on
    /// disk, are silently skipped so callers can pass a raw git-diff list. The
    /// returned [`ExtractionResult`] has no `files` when nothing indexable
    /// remains.
    ///
    /// Call edges are resolved only among the supplied paths; cross-file edges
    /// into unchanged files are not rebuilt here (a full [`extract`] does that).
    pub fn extract_paths(
        &self,
        root: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        paths: &[PathBuf],
    ) -> Result<ExtractionResult> {
        let filtered = paths
            .iter()
            .filter(|path| path.is_file() && is_indexable_path(path))
            .cloned()
            .collect::<Vec<_>>();
        self.extract_files(root, repo_id, commit_sha, filtered)
    }

    fn extract_files(
        &self,
        root: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        paths: Vec<PathBuf>,
    ) -> Result<ExtractionResult> {
        let mut result = ExtractionResult::empty();
        let repo_node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: None,
            kind: NodeKind::Repository,
            stable_id: "repo".into(),
            name: root
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("repository")
                .into(),
            line_start: None,
            line_end: None,
            metadata: json!({"root": root.display().to_string()}),
        };
        result.nodes.push(repo_node.clone());

        let mut symbol_names: HashMap<String, Uuid> = HashMap::new();
        let mut calls: Vec<crate::lang::CallSite> = Vec::new();
        // GraphQL type/fragment references, resolved AFTER the file loop by a
        // graphql-only post-pass (never through `symbol_names` — see
        // `lang::graphql::resolve_graphql_edges`).
        let mut graphql_refs: Vec<crate::lang::graphql::PendingRef> = Vec::new();
        // The repo's own workspace package names, so JS/TS/Solidity extraction
        // can tell an internal workspace import (kept) from a third-party
        // node_modules one (dropped, so it doesn't form or name a god-node
        // feature). The crate and Python module roots do the same for `use`
        // statements and Python absolute imports.
        let workspace_packages = self.workspace_package_names(root);
        let workspace_crates = self.workspace_crate_names(root);
        let python_roots = self.python_module_roots(root);
        for path in paths {
            // source_paths/extract_paths only pass indexable files; the guard
            // keeps a direct caller from slipping an unknown file through.
            let Some(file_kind) = detect_file_kind(&path) else {
                continue;
            };
            match file_kind {
                FileKind::CargoManifest => self.extract_cargo(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::PackageJson => self.extract_package_json(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::CdkJson => self.extract_cdk_json(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::JsonConfig => self.extract_json_config(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::Markdown => self.extract_markdown_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::Pdf => self.extract_pdf_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?,
                FileKind::Rust => self.extract_rust_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut symbol_names,
                    &workspace_crates,
                    &mut result,
                )?,
                FileKind::JsTs(language) => self.extract_lang_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    language,
                    crate::lang::ImportFilter::NpmWorkspace(&workspace_packages),
                    crate::lang::javascript::extract,
                    &mut symbol_names,
                    &mut calls,
                    &mut graphql_refs,
                    &mut result,
                )?,
                FileKind::Python => self.extract_lang_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    Language::Python,
                    crate::lang::ImportFilter::PythonRoots(&python_roots),
                    crate::lang::python::extract,
                    &mut symbol_names,
                    &mut calls,
                    &mut graphql_refs,
                    &mut result,
                )?,
                FileKind::Solidity => self.extract_lang_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    Language::Solidity,
                    // npm-style bare imports (`@openzeppelin/...`) are external
                    // unless they name a workspace package; relative `.sol`
                    // imports dedupe.
                    crate::lang::ImportFilter::NpmWorkspace(&workspace_packages),
                    crate::lang::solidity::extract,
                    &mut symbol_names,
                    &mut calls,
                    &mut graphql_refs,
                    &mut result,
                )?,
                FileKind::GraphQL => self.extract_lang_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    Language::GraphQL,
                    // GraphQL emits no imports, so the filter is inert;
                    // reusing the npm-workspace set avoids a third variant.
                    crate::lang::ImportFilter::NpmWorkspace(&workspace_packages),
                    crate::lang::graphql::extract,
                    &mut symbol_names,
                    &mut calls,
                    &mut graphql_refs,
                    &mut result,
                )?,
            }
        }

        add_call_edges(repo_id, &mut result, &calls, &symbol_names);
        crate::lang::graphql::resolve_graphql_edges(repo_id, &mut result, &graphql_refs);
        crate::lang::graphql::surface_split_file_root_fields(repo_id, &mut result);
        deduplicate_nodes(&mut result);
        split_large_chunks(&mut result);
        Ok(result)
    }

    fn source_paths(&self, root: &Path) -> Vec<PathBuf> {
        let skip_dirs = self.indexing.skip_dirs.clone();
        let prune_paths = self.prune_paths.clone();
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false).filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if skip_dirs.iter().any(|skip| skip == &name) {
                return false;
            }
            !prune_paths.iter().any(|p| entry.path().starts_with(p))
        });

        builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().map(|ft| ft.is_file()).unwrap_or(false))
            .map(|entry| entry.into_path())
            .filter(|path| is_indexable_path(path))
            .collect()
    }

    /// Collect the repo's own workspace package names — the `name` field of every
    /// `package.json` in the tree (`node_modules` is already excluded by
    /// `skip_dirs`). This is how we tell an internal workspace import
    /// (`@moleculexyz/ds2`, kept) from a third-party one (`react`, dropped). Always
    /// scans the whole repo, even on an incremental `add`, so the classification is
    /// stable regardless of which files changed.
    fn workspace_package_names(&self, root: &Path) -> HashSet<String> {
        let skip_dirs = self.indexing.skip_dirs.clone();
        let prune_paths = self.prune_paths.clone();
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false).filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if skip_dirs.iter().any(|skip| skip == &name) {
                return false;
            }
            !prune_paths.iter().any(|p| entry.path().starts_with(p))
        });
        let mut names = HashSet::new();
        for entry in builder.build().filter_map(Result::ok) {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) != Some("package.json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
                    if !name.is_empty() {
                        names.insert(name.to_string());
                    }
                }
            }
        }
        names
    }

    /// Collect the repo's own crate names — the `[package].name` of every
    /// `Cargo.toml` in the tree, normalized `-`→`_` as they appear in `use`
    /// paths. A `use` whose first segment is none of these (and not
    /// `crate`/`self`/`super`) is external (std/third-party) and dropped.
    fn workspace_crate_names(&self, root: &Path) -> HashSet<String> {
        let skip_dirs = self.indexing.skip_dirs.clone();
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false).filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            !skip_dirs.iter().any(|skip| skip == &name)
        });
        let mut names = HashSet::new();
        for entry in builder.build().filter_map(Result::ok) {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) != Some("Cargo.toml") {
                continue;
            }
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(value) = content.parse::<toml::Value>() else {
                continue;
            };
            if let Some(name) = value
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                if !name.is_empty() {
                    names.insert(name.replace('-', "_"));
                }
            }
        }
        names
    }

    /// Collect the repo's own top-level Python module roots: `*.py` stems and
    /// directories containing Python files, at the repo root and under `src/`.
    /// An absolute `import x.y` whose first segment is none of these is
    /// external (stdlib or site-packages) and dropped.
    fn python_module_roots(&self, root: &Path) -> HashSet<String> {
        let mut roots = HashSet::new();
        for base in [root.to_path_buf(), root.join("src")] {
            let Ok(entries) = fs::read_dir(&base) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if self.indexing.skip_dirs.iter().any(|skip| skip == name) {
                    continue;
                }
                if path.is_dir() {
                    let has_python = path.join("__init__.py").exists()
                        || fs::read_dir(&path).is_ok_and(|dir| {
                            dir.filter_map(Result::ok).any(|e| {
                                e.path().extension().and_then(|x| x.to_str()) == Some("py")
                            })
                        });
                    if has_python {
                        roots.insert(name.to_string());
                    }
                } else if path.extension().and_then(|x| x.to_str()) == Some("py") {
                    roots.insert(name.to_string());
                }
            }
        }
        roots
    }

    fn extract_markdown_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Markdown,
            weights::CONTAINS_DOC,
            json!({"source_priority": "supplemental"}),
            result,
        )?;
        let content = file.content.clone();
        let rel = file.path.clone();

        // Heading-aware sectioning: one chunk per heading section keeps each
        // embedded unit semantically whole (a blind size split of a long README
        // cuts mid-topic and embeds poorly). Sections are chunks only — they
        // all attach to the file node, so the graph gains no extra nodes.
        let sections = markdown_sections(&content);
        if sections.is_empty() {
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(file_node_id),
                "documentation",
                &format!("Documentation file: {rel}\n\n{content}"),
                Some(1),
                Some(file.line_count),
                json!({
                    "kind": "documentation",
                    "file": rel,
                    "source_priority": "supplemental",
                    "guidance": "Documentation can add context but source code should be prioritized when they disagree."
                }),
            ));
            return Ok(());
        }

        for section in sections {
            let title_path = if section.heading_path.is_empty() {
                rel.clone()
            } else {
                format!("{rel} > {}", section.heading_path.join(" > "))
            };
            let section_name = section
                .heading_path
                .last()
                .cloned()
                .unwrap_or_else(|| "preamble".to_string());
            let base_meta = json!({
                "kind": "documentation",
                "file": rel,
                "section": section_name,
                "heading_path": section.heading_path,
                "source_priority": "supplemental",
                "guidance": "Documentation can add context but source code should be prioritized when they disagree."
            });

            let full = format!("Documentation: {title_path}\n\n{}", section.text);
            if full.len() <= MAX_CHUNK_CHARS {
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(file_node_id),
                    "documentation",
                    &full,
                    Some(section.line_start as i32),
                    Some(section.line_end as i32),
                    base_meta,
                ));
                continue;
            }

            // Oversized section: pack whole markdown BLOCKS into parts (the
            // generic splitter would happily cut inside a code fence or a
            // table). Every part keeps the heading-path context header and
            // the split metadata, so stats/dedup see them like any split.
            let parts = pack_markdown_section(
                &section.text,
                MAX_CHUNK_CHARS.saturating_sub(DOC_HEADER_ALLOWANCE),
            );
            let total = parts.len();
            let parent_hash = hash(&full);
            for (idx, part) in parts.into_iter().enumerate() {
                let mut metadata = base_meta.clone();
                stamp_split_part(&mut metadata, idx + 1, total, &parent_hash);
                let line_start = section.line_start + part.line_offset;
                let line_end =
                    (line_start + part.line_count.saturating_sub(1)).min(section.line_end);
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(file_node_id),
                    "documentation",
                    &format!(
                        "Documentation: {title_path} (part {}/{})\n\n{}",
                        idx + 1,
                        total,
                        part.text
                    ),
                    Some(line_start as i32),
                    Some(line_end.max(line_start) as i32),
                    metadata,
                ));
            }
        }
        Ok(())
    }

    fn extract_pdf_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let content = pdf_extract::extract_text(path).unwrap_or_default();
        let line_count = content.lines().count().max(1) as i32;
        let file = SourceFile {
            id: Uuid::new_v4(),
            repo_id,
            commit_sha,
            path: rel.clone(),
            language: Language::Pdf,
            content: content.clone(),
            content_hash: hash(&content),
            line_count,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_PDF,
            json!({"source_priority": "supplemental", "extractor": "pdf_text"}),
        ));
        if !content.trim().is_empty() {
            let base_meta = json!({
                "kind": "documentation",
                "file": rel,
                "source_priority": "supplemental",
                "format": "pdf",
                "guidance": "PDF text can add context but source code should be prioritized when they disagree."
            });
            let full = format!("PDF document: {rel}\n\n{content}");
            if full.len() <= MAX_CHUNK_CHARS {
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(file_node.id),
                    "pdf_documentation",
                    &full,
                    Some(1),
                    Some(line_count),
                    base_meta,
                ));
            } else {
                // Oversized PDF text: pack at page (`\f`) and paragraph
                // boundaries with a document-context header per part, instead
                // of letting the generic splitter cut mid-flow.
                let budget = MAX_CHUNK_CHARS.saturating_sub(DOC_HEADER_ALLOWANCE);
                let has_pages = content.contains('\u{c}');
                let parts = pack_pdf_units(pdf_doc_units(&content, budget), budget);
                let total = parts.len();
                let parent_hash = hash(&full);
                for (idx, pdf_part) in parts.into_iter().enumerate() {
                    let mut metadata = base_meta.clone();
                    stamp_split_part(&mut metadata, idx + 1, total, &parent_hash);
                    if has_pages {
                        if let Some(obj) = metadata.as_object_mut() {
                            obj.insert(
                                "pages".into(),
                                json!(format!("{}-{}", pdf_part.page_start, pdf_part.page_end)),
                            );
                        }
                    }
                    let pages_line = if has_pages {
                        format!("\nPages: {}-{}", pdf_part.page_start, pdf_part.page_end)
                    } else {
                        String::new()
                    };
                    let line_start = (pdf_part.part.line_offset + 1) as i32;
                    let line_end = (pdf_part.part.line_offset + pdf_part.part.line_count) as i32;
                    result.chunks.push(chunk_for_node(
                        repo_id,
                        Some(file.id),
                        Some(file_node.id),
                        "pdf_documentation",
                        &format!(
                            "PDF document: {rel} (part {}/{}){pages_line}\n\n{}",
                            idx + 1,
                            total,
                            pdf_part.part.text
                        ),
                        Some(line_start),
                        Some(line_end.min(line_count).max(line_start)),
                        metadata,
                    ));
                }
            }
        }
        result.nodes.push(file_node);
        Ok(())
    }

    fn extract_cargo(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Rust,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let content = file.content.clone();
        let rel = file.path.clone();

        let parsed: toml::Value =
            toml::from_str(&content).unwrap_or(toml::Value::Table(Default::default()));
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(table) = parsed.get(section).and_then(|v| v.as_table()) {
                for name in table.keys() {
                    let node = KnowledgeNode {
                        id: Uuid::new_v4(),
                        repo_id,
                        file_id: Some(file.id),
                        kind: NodeKind::Dependency,
                        stable_id: format!("{rel}:cargo:dependency:{name}"),
                        name: name.clone(),
                        line_start: find_line(&content, name).map(|v| v as i32),
                        line_end: find_line(&content, name).map(|v| v as i32),
                        metadata: json!({"section": section}),
                    };
                    result.edges.push(edge(
                        repo_id,
                        file_node_id,
                        node.id,
                        EdgeKind::DependsOn,
                        weights::DEPENDS_ON,
                        json!({}),
                    ));
                    result.chunks.push(chunk_for_node(
                        repo_id,
                        Some(file.id),
                        Some(node.id),
                        "dependency",
                        &format!("File: {rel}\nDependency: {name}\nSection: {section}\n"),
                        node.line_start,
                        node.line_end,
                        json!({"dependency": name, "section": section}),
                    ));
                    result.nodes.push(node);
                }
            }
        }
        Ok(())
    }

    fn extract_package_json(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Json,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let content = file.content.clone();
        let rel = file.path.clone();

        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap_or(json!({}));

        // Dependency and script nodes/edges are retained for the knowledge
        // graph, but their embeddings are aggregated into a single chunk per
        // manifest instead of one chunk per entry, which otherwise inflates the
        // embedding count enormously for little retrieval value.
        let mut dependency_names: Vec<String> = Vec::new();
        let mut dependency_lines: Vec<String> = Vec::new();
        for section in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(table) = parsed.get(section).and_then(|v| v.as_object()) {
                for (name, version) in table {
                    let node = KnowledgeNode {
                        id: Uuid::new_v4(),
                        repo_id,
                        file_id: Some(file.id),
                        kind: NodeKind::Dependency,
                        stable_id: format!("{rel}:npm:dependency:{name}"),
                        name: name.clone(),
                        line_start: find_line(&content, name).map(|v| v as i32),
                        line_end: find_line(&content, name).map(|v| v as i32),
                        metadata: json!({"ecosystem": "npm", "section": section, "version": version}),
                    };
                    result.edges.push(edge(
                        repo_id,
                        file_node_id,
                        node.id,
                        EdgeKind::DependsOn,
                        weights::DEPENDS_ON,
                        json!({}),
                    ));
                    let version = version.as_str().unwrap_or_default();
                    dependency_lines.push(format!("- {name}@{version} ({section})"));
                    dependency_names.push(name.clone());
                    result.nodes.push(node);
                }
            }
        }
        if !dependency_lines.is_empty() {
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(file_node_id),
                "dependency",
                &format!(
                    "File: {rel}\nEcosystem: npm\nDependencies ({}):\n{}\n",
                    dependency_names.len(),
                    dependency_lines.join("\n")
                ),
                None,
                None,
                json!({"ecosystem": "npm", "dependency": dependency_names}),
            ));
        }

        let mut script_names: Vec<String> = Vec::new();
        let mut script_lines: Vec<String> = Vec::new();
        if let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) {
            for (name, command) in scripts {
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id,
                    file_id: Some(file.id),
                    kind: NodeKind::Script,
                    stable_id: format!("{rel}:npm:script:{name}"),
                    name: format!("npm script {name}"),
                    line_start: find_line(&content, name).map(|v| v as i32),
                    line_end: find_line(&content, name).map(|v| v as i32),
                    metadata: json!({"ecosystem": "npm", "script": name, "command": command}),
                };
                result.edges.push(edge(
                    repo_id,
                    file_node_id,
                    node.id,
                    EdgeKind::Defines,
                    weights::DEFINES_SCRIPT,
                    json!({}),
                ));
                let command = command.as_str().unwrap_or_default();
                script_lines.push(format!("- {name}: {command}"));
                script_names.push(name.clone());
                result.nodes.push(node);
            }
        }
        if !script_lines.is_empty() {
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(file_node_id),
                "script",
                &format!(
                    "File: {rel}\nNPM scripts ({}):\n{}\n",
                    script_names.len(),
                    script_lines.join("\n")
                ),
                None,
                None,
                json!({"ecosystem": "npm", "script": script_names}),
            ));
        }
        Ok(())
    }

    fn extract_json_config(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Json,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let rel = file.path.clone();
        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(file_node_id),
            "config",
            &format!(
                "File: {rel}\nJavaScript/TypeScript configuration:\n\n{content}",
                content = file.content
            ),
            Some(1),
            Some(file.line_count),
            json!({"config": rel}),
        ));
        Ok(())
    }

    fn extract_cdk_json(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Json,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let content = file.content.clone();
        let rel = file.path.clone();

        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap_or(json!({}));
        let app = parsed.get("app").and_then(|v| v.as_str()).unwrap_or("");
        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: Some(file.id),
            kind: NodeKind::DeploymentResource,
            stable_id: format!("{rel}:aws-cdk:app"),
            name: "AWS CDK app".into(),
            line_start: find_line(&content, "app").map(|v| v as i32).or(Some(1)),
            line_end: Some(file.line_count),
            metadata: json!({"technology": "aws_cdk", "config": rel, "app": app}),
        };
        result.edges.push(edge(
            repo_id,
            file_node_id,
            node.id,
            EdgeKind::Configures,
            weights::CONFIGURES_APP,
            json!({"technology": "aws_cdk"}),
        ));
        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(node.id),
            "aws_cdk_app",
            &format!("Technology: AWS CDK\nFile: {rel}\nCDK app command: {app}\n\n{content}"),
            node.line_start,
            node.line_end,
            json!({"technology": "aws_cdk", "kind": "cdk_app", "file": rel, "app": app}),
        ));
        result.nodes.push(node);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn extract_rust_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        symbol_names: &mut HashMap<String, Uuid>,
        workspace_crates: &HashSet<String>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            Language::Rust,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let content = file.content.clone();
        let rel = file.path.clone();

        // A broken .rs file degrades like every other code language — warn +
        // whole-file fallback chunk — instead of aborting the whole run.
        let chunks_before = result.chunks.len();
        let syntax = match syn::parse_file(&content) {
            Ok(syntax) => syntax,
            Err(err) => {
                crate::lang::warn_parse_failure(&rel, &err.to_string());
                crate::lang::emit_whole_file_fallback(
                    repo_id,
                    &file,
                    file_node_id,
                    chunks_before,
                    result,
                    "parse_failed",
                );
                return Ok(());
            }
        };

        crate::user_surface::emit_surface_entries(
            repo_id,
            &file,
            file_node_id,
            crate::user_surface::collect_rust_surface(&syntax),
            result,
        );

        for item in &syntax.items {
            self.add_rust_item(
                repo_id,
                &file,
                file_node_id,
                item,
                &[],
                workspace_crates,
                &content,
                symbol_names,
                result,
            );
        }
        Ok(())
    }

    /// Emit one top-level (or inline-module-nested) Rust item. `mod_path` is
    /// the chain of inline module names leading here; nested items get it as
    /// a `tests::`-style stable_id prefix so `mod tests { fn helper() }`
    /// cannot collide with a top-level `helper`.
    #[allow(clippy::too_many_arguments)]
    fn add_rust_item(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        item: &Item,
        mod_path: &[String],
        workspace_crates: &HashSet<String>,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) {
        use crate::user_surface::span_lines;
        match item {
            Item::Fn(item) => {
                let name = item.sig.ident.to_string();
                let kind = if item.attrs.iter().any(|a| a.path().is_ident("test")) {
                    NodeKind::Test
                } else {
                    NodeKind::Function
                };
                let (start, end) = span_lines(item);
                self.add_symbol(
                    repo_id,
                    file,
                    file_node_id,
                    weights::CONTAINS_CODE,
                    kind.clone(),
                    &name,
                    rust_stable_id(&file.path, &kind, mod_path, &name),
                    start,
                    end,
                    &slice_lines(content, start, end),
                    symbol_names,
                    result,
                );
            }
            Item::Struct(item) => {
                let name = item.ident.to_string();
                let (start, end) = span_lines(item);
                self.add_symbol(
                    repo_id,
                    file,
                    file_node_id,
                    weights::CONTAINS_CODE,
                    NodeKind::Struct,
                    &name,
                    rust_stable_id(&file.path, &NodeKind::Struct, mod_path, &name),
                    start,
                    end,
                    &slice_lines(content, start, end),
                    symbol_names,
                    result,
                );
            }
            Item::Enum(item) => {
                let name = item.ident.to_string();
                let (start, end) = span_lines(item);
                self.add_symbol(
                    repo_id,
                    file,
                    file_node_id,
                    weights::CONTAINS_CODE,
                    NodeKind::Enum,
                    &name,
                    rust_stable_id(&file.path, &NodeKind::Enum, mod_path, &name),
                    start,
                    end,
                    &slice_lines(content, start, end),
                    symbol_names,
                    result,
                );
            }
            Item::Trait(item) => {
                let name = item.ident.to_string();
                let (start, end) = span_lines(item);
                self.add_symbol(
                    repo_id,
                    file,
                    file_node_id,
                    weights::CONTAINS_CODE,
                    NodeKind::Trait,
                    &name,
                    rust_stable_id(&file.path, &NodeKind::Trait, mod_path, &name),
                    start,
                    end,
                    &slice_lines(content, start, end),
                    symbol_names,
                    result,
                );
            }
            Item::Mod(item) => {
                let name = item.ident.to_string();
                let (start, end) = span_lines(item);
                match &item.content {
                    Some((_, items)) => {
                        // Inline module: the chunk is just the doc comments and
                        // the `mod name {` header — the items inside are
                        // extracted individually below, so a whole-body chunk
                        // would only duplicate them (and blind-split).
                        let (ident_line, _) = span_lines(&item.ident);
                        let header = slice_lines(content, start, ident_line.max(start));
                        self.add_symbol(
                            repo_id,
                            file,
                            file_node_id,
                            weights::CONTAINS_CODE,
                            NodeKind::Module,
                            &name,
                            rust_stable_id(&file.path, &NodeKind::Module, mod_path, &name),
                            start,
                            end,
                            &header,
                            symbol_names,
                            result,
                        );
                        let mut nested = mod_path.to_vec();
                        nested.push(name);
                        for inner in items {
                            self.add_rust_item(
                                repo_id,
                                file,
                                file_node_id,
                                inner,
                                &nested,
                                workspace_crates,
                                content,
                                symbol_names,
                                result,
                            );
                        }
                    }
                    None => {
                        // `mod foo;` file-module declaration — one tiny chunk.
                        self.add_symbol(
                            repo_id,
                            file,
                            file_node_id,
                            weights::CONTAINS_CODE,
                            NodeKind::Module,
                            &name,
                            rust_stable_id(&file.path, &NodeKind::Module, mod_path, &name),
                            start,
                            end,
                            &slice_lines(content, start, end),
                            symbol_names,
                            result,
                        );
                    }
                }
            }
            Item::Impl(item) => self.add_impl(
                repo_id,
                file,
                file_node_id,
                item,
                mod_path,
                content,
                symbol_names,
                result,
            ),
            Item::Use(item) => {
                // External uses (std/core/alloc/third-party crates) are
                // dropped — the declared dependency list lives in the
                // Cargo.toml nodes. Internal uses dedupe to one node per
                // identical statement repo-wide; the per-importer file and
                // line live on the `Imports` edge.
                if !use_tree_is_internal(&item.tree, workspace_crates) {
                    return;
                }
                let text = quote_use(item);
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id,
                    file_id: None,
                    kind: NodeKind::Concept,
                    stable_id: format!("import:rust:{}", hash(&text)),
                    name: text.clone(),
                    line_start: None,
                    line_end: None,
                    metadata: json!({"import": text}),
                };
                result.edges.push(edge(
                    repo_id,
                    file_node_id,
                    node.id,
                    EdgeKind::Imports,
                    weights::IMPORTS_RUST,
                    json!({
                        "file": file.path,
                        "line": find_line(content, &text)
                    }),
                ));
                result.nodes.push(node);
            }
            _ => {}
        }
    }

    /// Shared dispatch for the AST language modules (JS/TS, Python, Solidity —
    /// the languages driven through [`crate::lang::FileExtraction`]): register
    /// the file, run the language's `extract`, and degrade uniformly — a parse
    /// failure warns and falls through to the whole-file fallback chunk
    /// instead of aborting the run.
    #[allow(clippy::too_many_arguments)]
    fn extract_lang_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        language: Language,
        import_filter: crate::lang::ImportFilter<'_>,
        extract: fn(&mut crate::lang::FileExtraction<'_>) -> Result<()>,
        symbol_names: &mut HashMap<String, Uuid>,
        calls: &mut Vec<crate::lang::CallSite>,
        graphql_refs: &mut Vec<crate::lang::graphql::PendingRef>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let (file, file_node_id) = begin_file(
            root,
            path,
            repo_id,
            commit_sha,
            repo_node_id,
            language,
            weights::CONTAINS_CODE,
            json!({}),
            result,
        )?;
        let chunks_before = result.chunks.len();
        let mut ctx = crate::lang::FileExtraction {
            repo_id,
            file: &file,
            file_node_id,
            lines: crate::lang::LineIndex::new(&file.content),
            symbol_names,
            result,
            calls,
            graphql_refs,
            import_filter,
        };
        let reason = match extract(&mut ctx) {
            Ok(()) => "no_symbols_extracted",
            Err(err) => {
                crate::lang::warn_parse_failure(&file.path, &format!("{err:#}"));
                "parse_failed"
            }
        };
        crate::lang::emit_whole_file_fallback(
            repo_id,
            &file,
            file_node_id,
            chunks_before,
            result,
            reason,
        );
        Ok(())
    }

    /// Emit an impl block: the Impl node's chunk carries only the impl header,
    /// the non-fn items (consts/types), and a method ROSTER — each method is
    /// extracted as its own `Method` node + chunk, contained by the impl (a
    /// local star, like Solidity contract→member). The roster deliberately
    /// lists bare names with no parentheses so the `name(` call heuristic
    /// cannot fabricate call edges from it.
    #[allow(clippy::too_many_arguments)]
    fn add_impl(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        item: &ItemImpl,
        mod_path: &[String],
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) {
        use crate::user_surface::span_lines;
        let name = impl_name(item);
        let (start, end) = span_lines(item);

        let mut methods: Vec<&syn::ImplItemFn> = Vec::new();
        let mut extra = String::new();
        for inner in &item.items {
            match inner {
                syn::ImplItem::Fn(f) => methods.push(f),
                syn::ImplItem::Const(c) => {
                    let (s, e) = span_lines(c);
                    extra.push_str(&slice_lines(content, s, e));
                    extra.push('\n');
                }
                syn::ImplItem::Type(t) => {
                    let (s, e) = span_lines(t);
                    extra.push_str(&slice_lines(content, s, e));
                    extra.push('\n');
                }
                _ => {}
            }
        }

        let header_end = item
            .items
            .first()
            .map(|first| span_lines(first).0.saturating_sub(1).max(start))
            .unwrap_or(end);
        let mut body = slice_lines(content, start, header_end);
        if !extra.trim().is_empty() {
            body.push('\n');
            body.push_str(extra.trim_end());
            body.push('\n');
        }
        if !methods.is_empty() {
            let names: Vec<String> = methods.iter().map(|f| f.sig.ident.to_string()).collect();
            body.push_str(&format!(
                "\nMethods (extracted separately): {}",
                names.join(", ")
            ));
        }

        let impl_node_id = self.add_symbol(
            repo_id,
            file,
            file_node_id,
            weights::CONTAINS_CODE,
            NodeKind::Impl,
            &name,
            rust_stable_id(&file.path, &NodeKind::Impl, mod_path, &name),
            start,
            end,
            &body,
            symbol_names,
            result,
        );

        for f in methods {
            let method_name = f.sig.ident.to_string();
            let kind = if f.attrs.iter().any(|a| a.path().is_ident("test")) {
                NodeKind::Test
            } else {
                NodeKind::Method
            };
            let (m_start, m_end) = span_lines(f);
            let qualified = if mod_path.is_empty() {
                format!("{name}::{method_name}")
            } else {
                format!("{}::{}::{}", mod_path.join("::"), name, method_name)
            };
            self.add_symbol(
                repo_id,
                file,
                impl_node_id,
                weights::CONTAINS_MEMBER,
                kind,
                &method_name,
                format!("{}:method:{}", file.path, qualified),
                m_start,
                m_end,
                &slice_lines(content, m_start, m_end),
                symbol_names,
                result,
            );
        }
    }

    /// Emit one Rust symbol node, its `Contains` edge from `parent_node_id`,
    /// and its chunk. Line ranges come from syn spans (span-exact, doc
    /// comments included); `code` is the chunk body. Returns the node id so
    /// impls can parent their methods.
    #[allow(clippy::too_many_arguments)]
    fn add_symbol(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        parent_node_id: Uuid,
        contains_weight: EdgeWeight,
        kind: NodeKind,
        name: &str,
        stable_id: String,
        line_start: usize,
        line_end: usize,
        code: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Uuid {
        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: Some(file.id),
            kind: kind.clone(),
            stable_id,
            name: name.to_string(),
            line_start: Some(line_start as i32),
            line_end: Some(line_end as i32),
            metadata: json!({"language": "rust", "file": file.path}),
        };
        let node_id = node.id;
        symbol_names.entry(name.to_string()).or_insert(node.id);
        result.edges.push(edge(
            repo_id,
            parent_node_id,
            node.id,
            EdgeKind::Contains,
            contains_weight,
            json!({}),
        ));
        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(node.id),
            kind.as_str(),
            &format!(
                "Language: Rust\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
                file.path,
                name,
                kind.as_str(),
                line_start,
                line_end,
                code
            ),
            Some(line_start as i32),
            Some(line_end as i32),
            json!({"symbol": name, "kind": kind.as_str(), "file": file.path}),
        ));
        result.nodes.push(node);
        node_id
    }
}

/// Stable id for a Rust symbol: `{file}:{kind}:{name}`, with the inline-module
/// path inserted (`{file}:{kind}:tests::name`) so nested names can't collide
/// with top-level ones. Top-level ids keep their historical shape (no churn).
fn rust_stable_id(file_path: &str, kind: &NodeKind, mod_path: &[String], name: &str) -> String {
    if mod_path.is_empty() {
        format!("{}:{}:{}", file_path, kind.as_str(), name)
    } else {
        format!(
            "{}:{}:{}::{}",
            file_path,
            kind.as_str(),
            mod_path.join("::"),
            name
        )
    }
}

pub fn current_commit(root: &Path) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Returns the id of the innermost code symbol in `file` whose line range
/// encloses `line` (smallest span wins). `None` if no symbol encloses the line.
fn innermost_caller(
    file_symbols: &HashMap<String, Vec<(Uuid, i32, i32)>>,
    file: &str,
    line: i32,
) -> Option<Uuid> {
    let symbols = file_symbols.get(file)?;
    symbols
        .iter()
        .filter(|(_, start, end)| *start <= line && line <= *end)
        .min_by_key(|(_, start, end)| end - start)
        .map(|(id, _, _)| *id)
}

/// True for node kinds that name a callable/definable code symbol that a call
/// edge can target or originate from.
fn is_code_symbol_kind(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Struct
            | NodeKind::Trait
            | NodeKind::Enum
            | NodeKind::TypeAlias
            | NodeKind::Module
            | NodeKind::Test
            | NodeKind::Concept
    )
}

/// Resolve discovered call sites to `Calls` edges.
///
/// AST languages (Python/Solidity/JS-TS) contribute precise `CallSite`s that
/// are resolved file-scoped first (prefer a same-file definition of the
/// callee) then globally. Rust keeps the legacy `content.contains("name(")`
/// heuristic but also resolves callees file-scoped first. The global
/// fallback is GATED on the callee name having exactly one definition in the
/// repo: a name defined in several places (`new`, `handle`, `run`, …) would
/// bind to whichever file happened to be walked first and glue unrelated
/// features together. A `(source, target)` set dedups across both passes so
/// at most one `Calls` edge exists per ordered pair.
fn add_call_edges(
    repo_id: Uuid,
    result: &mut ExtractionResult,
    ast_calls: &[crate::lang::CallSite],
    global_symbols: &HashMap<String, Uuid>,
) {
    // Index code-symbol nodes for resolution.
    let mut by_file_name: HashMap<(String, String), Uuid> = HashMap::new();
    let mut file_symbols: HashMap<String, Vec<(Uuid, i32, i32)>> = HashMap::new();
    // How many definitions each symbol name has repo-wide (any language) —
    // the ambiguity gate for the global fallbacks below.
    let mut global_count: HashMap<String, usize> = HashMap::new();
    // Rust-only global map (kept separate so non-rust callees can't shadow the
    // legacy rust heuristic, and vice versa); BTreeMap so the scan below emits
    // edges in canonical name order. Carries (first definition, count).
    let mut rust_global: BTreeMap<String, (Uuid, usize)> = BTreeMap::new();
    // Ids of rust symbol nodes, for O(1) "is this chunk a rust source?" lookups.
    let mut rust_source_ids: HashSet<Uuid> = HashSet::new();

    for node in &result.nodes {
        if !is_code_symbol_kind(&node.kind) {
            continue;
        }
        // GraphQL SDL types reuse the classic kinds (Struct/Trait/Enum/
        // TypeAlias) but must not enter the call machinery in either
        // direction: as callees they'd capture same-named cross-language
        // calls (and make repo-unique code symbols ambiguous), and as
        // `innermost_caller` candidates an embedded-SDL node spanning its
        // host template would claim calls inside `${…}` interpolation holes.
        if node.metadata.get("language").and_then(|v| v.as_str()) == Some("graphql") {
            continue;
        }
        let Some(file) = node.metadata.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let (Some(start), Some(end)) = (node.line_start, node.line_end) else {
            continue;
        };
        by_file_name
            .entry((file.to_string(), node.name.clone()))
            .or_insert(node.id);
        file_symbols
            .entry(file.to_string())
            .or_default()
            .push((node.id, start, end));
        *global_count.entry(node.name.clone()).or_default() += 1;
        if node.metadata.get("language").and_then(|v| v.as_str()) == Some("rust") {
            let entry = rust_global.entry(node.name.clone()).or_insert((node.id, 0));
            entry.1 += 1;
            rust_source_ids.insert(node.id);
        }
    }

    let mut seen: HashSet<(Uuid, Uuid)> = HashSet::new();

    // ----- AST-language edges (precise) -----
    for cs in ast_calls {
        let Some(caller) = innermost_caller(&file_symbols, &cs.file, cs.line) else {
            continue;
        };
        let Some(target) = by_file_name
            .get(&(cs.file.clone(), cs.callee.clone()))
            .copied()
            .or_else(|| {
                // Global fallback only for repo-unique callee names.
                (global_count.get(&cs.callee).copied() == Some(1))
                    .then(|| global_symbols.get(&cs.callee).copied())
                    .flatten()
            })
        else {
            continue;
        };
        if caller == target {
            continue;
        }
        if seen.insert((caller, target)) {
            result.edges.push(edge(
                repo_id,
                caller,
                target,
                EdgeKind::Calls,
                weights::CALLS_HEURISTIC,
                json!({"detector": "ast_call_site", "callee": cs.callee}),
            ));
        }
    }

    // ----- Rust edges (legacy contains heuristic, now file-scoped) -----
    // Snapshot just what the rust pass needs so the later mutable edge pushes
    // don't conflict with the immutable borrow of `result.chunks`.
    let rust_chunks: Vec<(Uuid, Option<String>, String)> = result
        .chunks
        .iter()
        .filter_map(|chunk| {
            let source = chunk.node_id?;
            if !rust_source_ids.contains(&source) {
                return None;
            }
            let file = chunk
                .metadata
                .get("file")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some((source, file, chunk.content.clone()))
        })
        .collect();

    for (source, chunk_file, content) in rust_chunks {
        for (name, (global_id, count)) in &rust_global {
            if !content.contains(&format!("{name}(")) {
                continue;
            }
            // Prefer a same-file rust symbol named `name`; the global fallback
            // applies only when the name has a single rust definition.
            let same_file = chunk_file
                .as_deref()
                .and_then(|f| by_file_name.get(&(f.to_string(), name.clone())).copied());
            let target = match same_file {
                Some(target) => target,
                None if *count == 1 => *global_id,
                None => continue,
            };
            if target == source {
                continue;
            }
            if seen.insert((source, target)) {
                result.edges.push(edge(
                    repo_id,
                    source,
                    target,
                    EdgeKind::Calls,
                    weights::CALLS_HEURISTIC,
                    json!({"detector": "name_call_heuristic", "callee": name}),
                ));
            }
        }
    }
}

/// Read a source file, register it, and emit its `File` node + `Contains`
/// edge. Returns the `SourceFile` and the file node's id so callers can attach
/// symbols. Centralizes the prelude every language extractor used to repeat.
#[allow(clippy::too_many_arguments)]
fn begin_file(
    root: &Path,
    path: &Path,
    repo_id: Uuid,
    commit_sha: Option<String>,
    repo_node_id: Uuid,
    language: Language,
    contains: EdgeWeight,
    contains_meta: serde_json::Value,
    result: &mut ExtractionResult,
) -> Result<(SourceFile, Uuid)> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let file = SourceFile {
        id: Uuid::new_v4(),
        repo_id,
        commit_sha,
        path: rel.clone(),
        language,
        content: content.clone(),
        content_hash: hash(&content),
        line_count: content.lines().count() as i32,
    };
    result.files.push(file.clone());

    let file_node = file_node(repo_id, &file, &rel);
    let file_node_id = file_node.id;
    result.edges.push(edge(
        repo_id,
        repo_node_id,
        file_node_id,
        EdgeKind::Contains,
        contains,
        contains_meta,
    ));
    result.nodes.push(file_node);
    Ok((file, file_node_id))
}

fn file_node(repo_id: Uuid, file: &SourceFile, rel: &str) -> KnowledgeNode {
    KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id,
        file_id: Some(file.id),
        kind: NodeKind::File,
        stable_id: format!("file:{rel}"),
        name: rel.to_string(),
        line_start: Some(1),
        line_end: Some(file.line_count),
        metadata: json!({"path": rel, "language": file.language.as_str()}),
    }
}

/// Everything the extractor knows how to index, resolved from a path by
/// [`detect_file_kind`]. One variant per extraction routine, so the
/// `extract_files` dispatch is a single exhaustive match and adding a
/// language/manifest kind cannot silently miss a dispatch arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    CargoManifest,
    PackageJson,
    CdkJson,
    /// `tsconfig.json` / `jsconfig.json`.
    JsonConfig,
    Markdown,
    Pdf,
    Rust,
    JsTs(Language),
    Python,
    Solidity,
    /// `.graphql` / `.gql` / `.graphqls` (SDL or executable documents).
    GraphQL,
}

/// Classify `path` as one of the indexable [`FileKind`]s, or `None` when the
/// extractor has no routine for it. The detection sets are disjoint (exact
/// manifest file names vs. per-language extensions), so ordering carries no
/// meaning.
pub(crate) fn detect_file_kind(path: &Path) -> Option<FileKind> {
    match path.file_name().and_then(|s| s.to_str()) {
        Some("Cargo.toml") => return Some(FileKind::CargoManifest),
        Some("package.json") => return Some(FileKind::PackageJson),
        Some("cdk.json") => return Some(FileKind::CdkJson),
        Some("tsconfig.json" | "jsconfig.json") => return Some(FileKind::JsonConfig),
        _ => {}
    }
    if markdown_language(path).is_some() {
        return Some(FileKind::Markdown);
    }
    if solidity_language(path).is_some() {
        return Some(FileKind::Solidity);
    }
    if pdf_language(path).is_some() {
        return Some(FileKind::Pdf);
    }
    if python_language(path).is_some() {
        return Some(FileKind::Python);
    }
    if graphql_language(path).is_some() {
        return Some(FileKind::GraphQL);
    }
    if path.extension().and_then(|s| s.to_str()) == Some("rs") {
        return Some(FileKind::Rust);
    }
    js_ts_language(path).map(FileKind::JsTs)
}

/// True when `path` is a file the extractor knows how to index. Shared by the
/// full-repo walk ([`RustRepositoryExtractor::source_paths`]) and the
/// incremental path filter ([`RustRepositoryExtractor::extract_paths`]) so both
/// agree on exactly which files become knowledge.
pub(crate) fn is_indexable_path(path: &Path) -> bool {
    detect_file_kind(path).is_some()
}

fn js_ts_language(path: &Path) -> Option<Language> {
    let file_name = path.file_name()?.to_str()?;
    if file_name.ends_with(".d.ts") {
        return Some(Language::TypeScript);
    }
    match path.extension()?.to_str()? {
        "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        _ => None,
    }
}

fn markdown_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "md" | "mdx" => Some(Language::Markdown),
        _ => None,
    }
}

fn solidity_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "sol" => Some(Language::Solidity),
        _ => None,
    }
}

fn pdf_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "pdf" => Some(Language::Pdf),
        _ => None,
    }
}

fn python_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "py" | "pyi" => Some(Language::Python),
        _ => None,
    }
}

fn graphql_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "graphql" | "gql" | "graphqls" => Some(Language::GraphQL),
        _ => None,
    }
}

pub(crate) fn is_js_ts_test_file(path: &str) -> bool {
    path.contains(".test.")
        || path.contains(".spec.")
        || path.contains("__tests__/")
        || path.contains("__test__/")
}

pub(crate) fn is_test_symbol(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test")
        || lower.ends_with("test")
        || lower.starts_with("spec")
        || lower.ends_with("spec")
}

pub(crate) fn is_python_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("_test.py")
        || lower.contains("/test_")
        || lower.starts_with("test_")
        || lower.contains("/tests/")
}

pub(crate) fn looks_like_cdk_file(content: &str) -> bool {
    content.contains("aws-cdk-lib")
        || content.contains("@aws-cdk/")
        || content.contains("constructs")
        || content.contains("extends Stack")
        || content.contains("cdk.")
}

pub(crate) fn cdk_service(construct_type: &str) -> &'static str {
    let lower = construct_type.to_ascii_lowercase();
    if lower.contains("lambda") || lower.contains("function") {
        "lambda"
    } else if lower.contains("dynamodb") || lower.contains("table") {
        "dynamodb"
    } else if lower.contains("appsync") || lower.contains("graphql") {
        "appsync"
    } else if lower.contains("s3") || lower.contains("bucket") {
        "s3"
    } else if lower.contains("cloudfront") || lower.contains("distribution") {
        "cloudfront"
    } else if lower.contains("sqs") || lower.contains("queue") {
        "sqs"
    } else if lower.contains("sns") || lower.contains("topic") {
        "sns"
    } else if lower.contains("iam") || lower.contains("role") || lower.contains("policy") {
        "iam"
    } else if lower.contains("apigateway") || lower.contains("api") {
        "api_gateway"
    } else if lower.contains("event") || lower.contains("rule") {
        "eventbridge"
    } else {
        "aws"
    }
}

/// Build a weighted knowledge edge. The `weight` carries the `cost` and
/// `confidence` that drive multigraph routing; see [`crate::weights`] for the
/// rationale behind each value and the named constants used at call sites.
pub(crate) fn edge(
    repo_id: Uuid,
    source: Uuid,
    target: Uuid,
    kind: EdgeKind,
    weight: EdgeWeight,
    metadata: serde_json::Value,
) -> KnowledgeEdge {
    KnowledgeEdge {
        id: Uuid::new_v4(),
        repo_id,
        source_node_id: source,
        target_node_id: target,
        kind,
        cost: weight.cost,
        confidence: weight.confidence,
        metadata,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn chunk_for_node(
    repo_id: Uuid,
    file_id: Option<Uuid>,
    node_id: Option<Uuid>,
    chunk_type: &str,
    content: &str,
    line_start: Option<i32>,
    line_end: Option<i32>,
    metadata: serde_json::Value,
) -> KnowledgeChunk {
    KnowledgeChunk {
        id: Uuid::new_v4(),
        repo_id,
        file_id,
        node_id,
        chunk_type: chunk_type.into(),
        content: content.into(),
        content_hash: hash(content),
        line_start,
        line_end,
        metadata,
    }
}

pub(crate) fn hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn deduplicate_nodes(result: &mut ExtractionResult) {
    let mut canonical_by_stable_id: HashMap<String, Uuid> = HashMap::new();
    let mut rewrite: HashMap<Uuid, Uuid> = HashMap::new();
    let mut unique_nodes = Vec::with_capacity(result.nodes.len());

    for node in result.nodes.drain(..) {
        if let Some(canonical_id) = canonical_by_stable_id.get(&node.stable_id).copied() {
            rewrite.insert(node.id, canonical_id);
        } else {
            canonical_by_stable_id.insert(node.stable_id.clone(), node.id);
            unique_nodes.push(node);
        }
    }

    for edge in &mut result.edges {
        if let Some(id) = rewrite.get(&edge.source_node_id) {
            edge.source_node_id = *id;
        }
        if let Some(id) = rewrite.get(&edge.target_node_id) {
            edge.target_node_id = *id;
        }
    }
    result
        .edges
        .retain(|edge| edge.source_node_id != edge.target_node_id);

    for chunk in &mut result.chunks {
        if let Some(node_id) = chunk.node_id {
            if let Some(id) = rewrite.get(&node_id) {
                chunk.node_id = Some(*id);
            }
        }
    }

    result.nodes = unique_nodes;
}

/// Stamp the metadata every chunk splitter records on a part: its 1-based
/// index, the part count, and the parent content's hash (what ties parts back
/// to the unsplit chunk in stats/dedup).
fn stamp_split_part(
    metadata: &mut serde_json::Value,
    part: usize,
    total: usize,
    parent_hash: &str,
) {
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("split_part".into(), json!(part));
        obj.insert("split_total".into(), json!(total));
        obj.insert("parent_content_hash".into(), json!(parent_hash));
    }
}

fn split_large_chunks(result: &mut ExtractionResult) {
    let mut chunks = Vec::with_capacity(result.chunks.len());
    for chunk in result.chunks.drain(..) {
        if chunk.content.len() <= MAX_CHUNK_CHARS {
            chunks.push(chunk);
            continue;
        }

        // Every chunk_for_node caller prefixes a header block followed by one
        // blank line before the body, and the body's first line corresponds to
        // the chunk's line_start. Counting the header lines once lets each
        // split part carry its real source line range instead of inheriting
        // the parent's full range.
        let body_start = chunk
            .content
            .lines()
            .position(|l| l.trim().is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        let parts = split_content(&chunk.content, MAX_CHUNK_CHARS);
        let part_count = parts.len();
        for (idx, part) in parts.into_iter().enumerate() {
            let mut metadata = chunk.metadata.clone();
            stamp_split_part(&mut metadata, idx + 1, part_count, &chunk.content_hash);
            let (line_start, line_end) = part_line_range(&chunk, body_start, &part);
            let content = format!(
                "{}\nChunk part: {}/{}\n\n{}",
                chunk_context_header(&chunk),
                idx + 1,
                part_count,
                part.content
            );
            chunks.push(KnowledgeChunk {
                id: Uuid::new_v4(),
                repo_id: chunk.repo_id,
                file_id: chunk.file_id,
                node_id: chunk.node_id,
                chunk_type: chunk.chunk_type.clone(),
                content_hash: hash(&content),
                content,
                line_start,
                line_end,
                metadata,
            });
        }
    }
    result.chunks = chunks;
}

/// Map a split part's position within the parent chunk content back to real
/// source lines. `body_start` is the number of content lines (header + blank)
/// preceding the body, whose first line is the parent's `line_start`.
fn part_line_range(
    chunk: &KnowledgeChunk,
    body_start: usize,
    part: &SplitPart,
) -> (Option<i32>, Option<i32>) {
    let Some(parent_start) = chunk.line_start else {
        return (chunk.line_start, chunk.line_end);
    };
    let body_off = part.line_offset.saturating_sub(body_start);
    let body_end = (part.line_offset + part.line_count).saturating_sub(body_start);
    if body_end == 0 {
        // The part is entirely inside the synthetic header.
        return (Some(parent_start), Some(parent_start));
    }
    let mut start = parent_start.saturating_add(body_off as i32);
    let mut end = parent_start.saturating_add(body_end.saturating_sub(1) as i32);
    if let Some(parent_end) = chunk.line_end {
        start = start.min(parent_end);
        end = end.min(parent_end);
    }
    (Some(start), Some(end.max(start)))
}

fn chunk_context_header(chunk: &KnowledgeChunk) -> String {
    let file = chunk
        .metadata
        .get("file")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let symbol = chunk
        .metadata
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    format!(
        "Chunk type: {}\nFile: {}\nSymbol: {}",
        chunk.chunk_type, file, symbol
    )
}

/// Maximum heading depth that starts a new documentation section; deeper
/// headings (H4+) stay inside their parent section so API-reference style
/// documents don't shatter into confetti.
const MARKDOWN_SECTION_DEPTH: usize = 3;

/// One heading-delimited section of a markdown document, with its real
/// 1-based line range and the heading titles leading to it.
struct MdSection {
    heading_path: Vec<String>,
    line_start: usize,
    line_end: usize,
    text: String,
}

/// Split markdown into heading-delimited sections using a real CommonMark
/// parser (so `#` lines inside fenced code blocks are NOT treated as
/// headings). Returns an empty vec when the document has no headings at or
/// above [`MARKDOWN_SECTION_DEPTH`] — the caller falls back to one
/// whole-file chunk.
fn markdown_sections(content: &str) -> Vec<MdSection> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    // (byte offset of the heading start, level, heading text)
    let mut boundaries: Vec<(usize, usize, String)> = Vec::new();
    let mut iter = Parser::new(content).into_offset_iter();
    while let Some((event, range)) = iter.next() {
        let Event::Start(Tag::Heading { level, .. }) = event else {
            continue;
        };
        let level = level as usize;
        if level > MARKDOWN_SECTION_DEPTH {
            continue;
        }
        let mut title = String::new();
        for (inner, _) in iter.by_ref() {
            match inner {
                Event::End(TagEnd::Heading(_)) => break,
                Event::Text(t) => title.push_str(&t),
                Event::Code(t) => title.push_str(&t),
                _ => {}
            }
        }
        boundaries.push((range.start, level, title.trim().to_string()));
    }
    if boundaries.is_empty() {
        return Vec::new();
    }

    let lines = crate::lang::LineIndex::new(content);
    let mut sections = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();

    let mut push_section = |path: &[(usize, String)], start: usize, end: usize| {
        let text = content[start..end].trim_end();
        if text.trim().is_empty() {
            return;
        }
        sections.push(MdSection {
            heading_path: path.iter().map(|(_, t)| t.clone()).collect(),
            line_start: lines.line(start),
            line_end: lines.line(start + text.len().saturating_sub(1)),
            text: text.to_string(),
        });
    };

    // Preamble before the first heading is its own (path-less) section.
    push_section(&[], 0, boundaries[0].0);
    for (i, (start, level, title)) in boundaries.iter().enumerate() {
        let end = boundaries
            .get(i + 1)
            .map(|(next, _, _)| *next)
            .unwrap_or(content.len());
        path.retain(|(l, _)| l < level);
        path.push((*level, title.clone()));
        push_section(&path, *start, end);
    }
    sections
}

/// Headroom reserved for the per-part `Documentation: … (part i/n)` context
/// header when packing oversized documentation into parts.
const DOC_HEADER_ALLOWANCE: usize = 256;

/// Top-level markdown block classification, for atomicity decisions: code
/// fences, tables and lists must never be cut mid-block.
enum MdBlockKind {
    /// Fenced (with its info string) or indented code.
    Code(Option<String>),
    Table,
    Heading,
    Other,
}

/// One packable unit of a documentation section: a whole top-level markdown
/// block, or a structure-preserving piece of an oversized one (a re-fenced
/// code slice, a table slice with its header repeated). `line_offset` is
/// 0-based within the source text the unit came from.
struct DocUnit {
    text: String,
    line_offset: usize,
    line_count: usize,
    /// Prefer starting a new part here (sub-headings) once a part is
    /// reasonably full, so parts align with the document's own structure.
    starts_group: bool,
}

/// A packed documentation part, positioned within its source text.
struct DocPart {
    text: String,
    line_offset: usize,
    line_count: usize,
}

/// Collect the byte ranges of every TOP-LEVEL block of a markdown text using
/// the real parser — so a blank line inside a code fence is part of the
/// fence, not a break opportunity.
fn collect_markdown_blocks(text: &str) -> Vec<(std::ops::Range<usize>, MdBlockKind)> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<(usize, MdBlockKind)> = None;
    // Tables are a GFM extension — without this option a table parses as one
    // big paragraph and loses its atomicity/header-repeat handling.
    for (event, range) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    let kind = match &tag {
                        Tag::CodeBlock(CodeBlockKind::Fenced(info)) => {
                            MdBlockKind::Code(Some(info.to_string()))
                        }
                        Tag::CodeBlock(CodeBlockKind::Indented) => MdBlockKind::Code(None),
                        Tag::Table(_) => MdBlockKind::Table,
                        Tag::Heading { .. } => MdBlockKind::Heading,
                        _ => MdBlockKind::Other,
                    };
                    current = Some((range.start, kind));
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some((start, kind)) = current.take() {
                        blocks.push((start..range.end.max(start), kind));
                    }
                }
            }
            Event::Rule if depth == 0 => {
                blocks.push((range.start..range.end, MdBlockKind::Other));
            }
            _ => {}
        }
    }
    blocks
}

/// Expand a section's top-level blocks into packable units no larger than
/// `budget`. Whole blocks stay atomic; an oversized code fence is split at
/// blank lines INSIDE it and each piece re-wrapped in fences (with the
/// original info string) so every part remains valid markdown; an oversized
/// table repeats its header + separator rows on each slice; anything else
/// falls back to the generic structural splitter.
fn section_doc_units(text: &str, budget: usize) -> Vec<DocUnit> {
    let lines = crate::lang::LineIndex::new(text);
    let mut units = Vec::new();
    for (range, kind) in collect_markdown_blocks(text) {
        let block_text = text[range.clone()].trim_end();
        if block_text.is_empty() {
            continue;
        }
        let line_offset = lines.line(range.start).saturating_sub(1);
        let line_count = block_text.lines().count().max(1);
        if block_text.len() <= budget {
            units.push(DocUnit {
                text: block_text.to_string(),
                line_offset,
                line_count,
                starts_group: matches!(kind, MdBlockKind::Heading),
            });
            continue;
        }
        match kind {
            MdBlockKind::Code(Some(info)) => {
                let block_lines: Vec<&str> = block_text.lines().collect();
                let opens = block_lines.first().is_some_and(|l| {
                    l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~")
                });
                let closes = block_lines.len() > 1
                    && block_lines.last().is_some_and(|l| {
                        l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~")
                    });
                let inner_start = usize::from(opens);
                let inner_end = block_lines.len() - usize::from(closes);
                let inner = block_lines[inner_start..inner_end].join("\n");
                let fence_overhead = info.len() + 10;
                for piece in split_content(&inner, budget.saturating_sub(fence_overhead).max(64)) {
                    units.push(DocUnit {
                        text: format!("```{info}\n{}\n```", piece.content),
                        line_offset: line_offset + inner_start + piece.line_offset,
                        line_count: piece.line_count,
                        starts_group: false,
                    });
                }
            }
            MdBlockKind::Table => {
                let block_lines: Vec<&str> = block_text.lines().collect();
                if block_lines.len() <= 2 {
                    units.push(DocUnit {
                        text: block_text.to_string(),
                        line_offset,
                        line_count,
                        starts_group: false,
                    });
                    continue;
                }
                let header = format!("{}\n{}", block_lines[0], block_lines[1]);
                let mut row_idx = 2usize;
                while row_idx < block_lines.len() {
                    let mut slice = header.clone();
                    let slice_first_row = row_idx;
                    while row_idx < block_lines.len()
                        && slice.len() + block_lines[row_idx].len() < budget
                    {
                        slice.push('\n');
                        slice.push_str(block_lines[row_idx]);
                        row_idx += 1;
                    }
                    if row_idx == slice_first_row {
                        // A single row larger than the budget: take it anyway
                        // rather than loop forever.
                        slice.push('\n');
                        slice.push_str(block_lines[row_idx]);
                        row_idx += 1;
                    }
                    units.push(DocUnit {
                        text: slice,
                        line_offset: line_offset + slice_first_row,
                        line_count: row_idx - slice_first_row,
                        starts_group: false,
                    });
                }
            }
            _ => {
                for piece in split_content(block_text, budget) {
                    units.push(DocUnit {
                        text: piece.content,
                        line_offset: line_offset + piece.line_offset,
                        line_count: piece.line_count,
                        starts_group: false,
                    });
                }
            }
        }
    }
    units
}

/// Greedily pack units into parts of at most `budget` chars, joining with
/// blank lines (block separation). A `starts_group` unit (a markdown
/// sub-heading) starts a new part once the current one is ≥ 60% full, so parts
/// align with document structure; PDF units always carry `starts_group:
/// false`, which is what lets both packers share this one implementation.
/// Each unit carries a `Copy` tag (the PDF page number; `()` for markdown) and
/// each packed part reports the first and last tag it covers.
fn pack_units<T: Copy>(
    units: Vec<(DocUnit, T)>,
    budget: usize,
    initial_tag: T,
) -> Vec<(DocPart, T, T)> {
    let mut parts: Vec<(DocPart, T, T)> = Vec::new();
    let mut text = String::new();
    let mut offset = 0usize;
    let mut end = 0usize;
    let mut tag_start = initial_tag;
    let mut tag_end = initial_tag;
    for (unit, tag) in units {
        let separator = if text.is_empty() { 0 } else { 2 };
        let overflow = text.len() + separator + unit.text.len() > budget;
        let group_break = unit.starts_group && text.len() >= budget * 3 / 5;
        if !text.is_empty() && (overflow || group_break) {
            parts.push((
                DocPart {
                    text: std::mem::take(&mut text),
                    line_offset: offset,
                    line_count: end.saturating_sub(offset).max(1),
                },
                tag_start,
                tag_end,
            ));
        }
        if text.is_empty() {
            offset = unit.line_offset;
            tag_start = tag;
        } else {
            text.push_str("\n\n");
        }
        text.push_str(&unit.text);
        end = unit.line_offset + unit.line_count;
        tag_end = tag;
    }
    if !text.trim().is_empty() {
        parts.push((
            DocPart {
                text,
                line_offset: offset,
                line_count: end.saturating_sub(offset).max(1),
            },
            tag_start,
            tag_end,
        ));
    }
    parts
}

/// [`pack_units`] for markdown documentation sections (no tag).
fn pack_doc_units(units: Vec<DocUnit>, budget: usize) -> Vec<DocPart> {
    pack_units(
        units.into_iter().map(|unit| (unit, ())).collect(),
        budget,
        (),
    )
    .into_iter()
    .map(|(part, _, _)| part)
    .collect()
}

/// A packed PDF part with the 1-based page range it covers (pages are only
/// known when the text extractor emitted form feeds).
struct PdfPart {
    part: DocPart,
    page_start: usize,
    page_end: usize,
}

/// Build packable units from extracted PDF text: form feeds (`\f`) are page
/// breaks, blank lines are paragraph breaks. Each unit carries its 1-based
/// page number (always 1 when there are no form feeds).
fn pdf_doc_units(content: &str, budget: usize) -> Vec<(DocUnit, usize)> {
    fn push_paragraph(
        units: &mut Vec<(DocUnit, usize)>,
        para: &[&str],
        line_offset: usize,
        page: usize,
        budget: usize,
    ) {
        let text = para.join("\n");
        if text.trim().is_empty() {
            return;
        }
        if text.len() <= budget {
            units.push((
                DocUnit {
                    text: text.trim_end().to_string(),
                    line_offset,
                    line_count: para.len(),
                    starts_group: false,
                },
                page,
            ));
        } else {
            for piece in split_content(&text, budget) {
                units.push((
                    DocUnit {
                        text: piece.content,
                        line_offset: line_offset + piece.line_offset,
                        line_count: piece.line_count,
                        starts_group: false,
                    },
                    page,
                ));
            }
        }
    }

    let mut units = Vec::new();
    let mut line_base = 0usize;
    for (page_idx, page_text) in content.split('\u{c}').enumerate() {
        let lines: Vec<&str> = page_text.lines().collect();
        let mut para: Vec<&str> = Vec::new();
        let mut para_start = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                push_paragraph(
                    &mut units,
                    &para,
                    line_base + para_start,
                    page_idx + 1,
                    budget,
                );
                para.clear();
            } else {
                if para.is_empty() {
                    para_start = i;
                }
                para.push(line);
            }
        }
        push_paragraph(
            &mut units,
            &para,
            line_base + para_start,
            page_idx + 1,
            budget,
        );
        line_base += lines.len();
    }
    units
}

/// [`pack_units`] for PDF text, tagging each part with the 1-based page range
/// it covers.
fn pack_pdf_units(units: Vec<(DocUnit, usize)>, budget: usize) -> Vec<PdfPart> {
    pack_units(units, budget, 1)
        .into_iter()
        .map(|(part, page_start, page_end)| PdfPart {
            part,
            page_start,
            page_end,
        })
        .collect()
}

/// Split an oversized markdown section into structure-respecting parts.
/// Falls back to the generic structural splitter when the parser finds no
/// blocks at all (pathological input).
fn pack_markdown_section(text: &str, budget: usize) -> Vec<DocPart> {
    let units = section_doc_units(text, budget);
    if units.is_empty() {
        return split_content(text, budget)
            .into_iter()
            .map(|piece| DocPart {
                text: piece.content,
                line_offset: piece.line_offset,
                line_count: piece.line_count,
            })
            .collect();
    }
    pack_doc_units(units, budget)
}

/// One part of an oversized chunk: the text plus its position within the
/// parent content (0-based line offset and number of source lines consumed),
/// so `split_large_chunks` can attribute a real line range to each part.
struct SplitPart {
    content: String,
    line_offset: usize,
    line_count: usize,
}

/// A structural boundary is only taken once the current part has reached this
/// fraction of `max_chars`; below it the splitter hard-breaks at the cap,
/// avoiding degenerate tiny parts when boundaries cluster near a part's start.
const SPLIT_MIN_FILL_RATIO: f64 = 0.4;

/// Split oversized chunk content at structural boundaries instead of raw
/// character positions: a part preferably ends right before a line that
/// follows a blank line (paragraph/statement-group break) or a line whose
/// indentation returns to the part's first content line (top-of-block).
fn split_content(content: &str, max_chars: usize) -> Vec<SplitPart> {
    let lines: Vec<&str> = content.lines().collect();
    let min_fill = (max_chars as f64 * SPLIT_MIN_FILL_RATIO) as usize;
    let mut parts: Vec<SplitPart> = Vec::new();
    let mut start = 0usize;
    let mut idx = 0usize;
    let mut current_len = 0usize;
    let mut base_indent: Option<usize> = None;
    // (line index to break before, accumulated chars up to that line)
    let mut last_boundary: Option<(usize, usize)> = None;

    let flush = |parts: &mut Vec<SplitPart>, from: usize, to: usize| {
        if to <= from {
            return;
        }
        let text = lines[from..to].join("\n");
        if !text.trim().is_empty() {
            parts.push(SplitPart {
                content: text.trim_end().to_string(),
                line_offset: from,
                line_count: to - from,
            });
        }
    };

    while idx < lines.len() {
        let line = lines[idx];

        if line.len() > max_chars {
            flush(&mut parts, start, idx);
            for piece in split_long_line(line, max_chars) {
                parts.push(SplitPart {
                    content: piece,
                    line_offset: idx,
                    line_count: 1,
                });
            }
            idx += 1;
            start = idx;
            current_len = 0;
            base_indent = None;
            last_boundary = None;
            continue;
        }

        if current_len + line.len() + 1 > max_chars && idx > start {
            match last_boundary {
                Some((boundary, chars)) if boundary > start && chars >= min_fill => {
                    flush(&mut parts, start, boundary);
                    start = boundary;
                }
                _ => {
                    flush(&mut parts, start, idx);
                    start = idx;
                }
            }
            current_len = lines[start..idx].iter().map(|l| l.len() + 1).sum::<usize>();
            base_indent = lines[start..=idx.min(lines.len() - 1)]
                .iter()
                .find(|l| !l.trim().is_empty())
                .map(|l| indent_width(l));
            last_boundary = None;
        }

        if base_indent.is_none() && !line.trim().is_empty() {
            base_indent = Some(indent_width(line));
        }
        if idx > start {
            let after_blank = lines[idx - 1].trim().is_empty();
            // A dedent boundary sits between two lines at the part's base
            // indentation (a new top-level construct after a completed one);
            // requiring the previous line at base too avoids breaking right
            // before a closing brace/bracket line.
            let dedented = !after_blank
                && !line.trim().is_empty()
                && base_indent.is_some_and(|base| {
                    indent_width(line) <= base && indent_width(lines[idx - 1]) <= base
                });
            if after_blank || dedented {
                last_boundary = Some((idx, current_len));
            }
        }

        current_len += line.len() + 1;
        idx += 1;
    }

    flush(&mut parts, start, lines.len());
    parts
}

/// Leading whitespace width of a line (tabs count as one column).
fn indent_width(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn split_long_line(line: &str, max_chars: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        if current.len() + ch.len_utf8() > max_chars && !current.is_empty() {
            parts.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Resolve a `./`/`../` import specifier against the importing file's
/// directory, purely lexically (no filesystem access, so it is deterministic
/// and works for extensionless TS-style specifiers). Two files importing the
/// same target produce the same normalized path and share one node.
pub(crate) fn normalize_relative_import(file_path: &str, module: &str) -> String {
    let mut segments: Vec<&str> = file_path.split('/').collect();
    segments.pop(); // drop the file name
    for part in module.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Resolve a Python leading-dot import (`.mod`, `..pkg.mod`, emitted by the
/// parser as dots + dotted path) against the importing file's package
/// directory: one dot = the current package, each further dot one level up.
pub(crate) fn python_relative_target(file_path: &str, module: &str) -> String {
    let dots = module.bytes().take_while(|b| *b == b'.').count();
    let rest = &module[dots..];
    let mut segments: Vec<&str> = file_path.split('/').collect();
    segments.pop(); // drop the file name; one dot = this package
    for _ in 1..dots {
        segments.pop();
    }
    for part in rest.split('.') {
        if !part.is_empty() {
            segments.push(part);
        }
    }
    segments.join("/")
}

pub(crate) fn is_bare_module_specifier(module: &str) -> bool {
    !module.starts_with('.')
        && !module.starts_with('/')
        && !module.starts_with("~/")
        && !module.starts_with("@/")
}

/// The npm "package" portion of an import specifier — what you'd find in
/// `node_modules`. `@scope/name/sub` → `@scope/name`; `name/sub` → `name`. Used to
/// match an import against the repo's own workspace package names.
pub(crate) fn import_package_portion(module: &str) -> &str {
    if let Some(rest) = module.strip_prefix('@') {
        // Scoped: keep `@scope/name` (the first two segments).
        match rest.split('/').nth(1) {
            Some(_) => {
                let mut slashes = 0;
                let end = module
                    .char_indices()
                    .find(|(_, c)| {
                        if *c == '/' {
                            slashes += 1;
                        }
                        slashes == 2
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(module.len());
                &module[..end]
            }
            None => module,
        }
    } else {
        module.split('/').next().unwrap_or(module)
    }
}

/// Whether a JavaScript/TypeScript import points **outside** the repo — a
/// third-party `node_modules` package the user can already learn about from their
/// package manager. Relative paths (`./`, `../`), root (`/`), and the common
/// internal aliases (`@/`, `~/`) are internal; a *bare* specifier is internal only
/// when its package portion is one of the repo's own workspace packages (e.g.
/// `@moleculexyz/ds2`), and external otherwise (`react`, `viem`, `@anthropic-ai/sdk`).
///
/// External imports are dropped from the knowledge graph so they don't form or
/// name god-node "features"; the repo's real dependency list still lives in the
/// `package.json` dependency nodes.
pub(crate) fn is_external_import(module: &str, workspace_packages: &HashSet<String>) -> bool {
    if !is_bare_module_specifier(module) {
        return false;
    }
    let pkg = import_package_portion(module);
    !workspace_packages.contains(pkg) && !workspace_packages.contains(module)
}

fn quote_use(item: &syn::ItemUse) -> String {
    format!("use {}", use_tree_to_string(&item.tree))
}

/// True when a `use` tree resolves inside the repo: its first path segment is
/// `crate`/`self`/`super` or one of the repo's own crate names. Top-level
/// groups (`use {a, b}`) are internal when any branch is.
fn use_tree_is_internal(tree: &syn::UseTree, workspace_crates: &HashSet<String>) -> bool {
    match tree {
        syn::UseTree::Path(path) => {
            let first = path.ident.to_string();
            matches!(first.as_str(), "crate" | "self" | "super")
                || workspace_crates.contains(&first)
        }
        syn::UseTree::Name(name) => {
            let first = name.ident.to_string();
            matches!(first.as_str(), "crate" | "self" | "super")
                || workspace_crates.contains(&first)
        }
        syn::UseTree::Rename(rename) => {
            let first = rename.ident.to_string();
            matches!(first.as_str(), "crate" | "self" | "super")
                || workspace_crates.contains(&first)
        }
        syn::UseTree::Glob(_) => false,
        syn::UseTree::Group(group) => group
            .items
            .iter()
            .any(|item| use_tree_is_internal(item, workspace_crates)),
    }
}

fn use_tree_to_string(tree: &syn::UseTree) -> String {
    match tree {
        syn::UseTree::Path(path) => format!("{}::{}", path.ident, use_tree_to_string(&path.tree)),
        syn::UseTree::Name(name) => name.ident.to_string(),
        syn::UseTree::Rename(rename) => format!("{} as {}", rename.ident, rename.rename),
        syn::UseTree::Glob(_) => "*".into(),
        syn::UseTree::Group(group) => group
            .items
            .iter()
            .map(use_tree_to_string)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn impl_name(item: &ItemImpl) -> String {
    let target = match item.self_ty.as_ref() {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    };
    if let Some((_, trait_path, _)) = &item.trait_ {
        let trait_name = trait_path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "trait".into());
        format!("{trait_name} for {target}")
    } else {
        target
    }
}

fn find_line(content: &str, needle: &str) -> Option<usize> {
    content
        .lines()
        .position(|line| line.contains(needle))
        .map(|idx| idx + 1)
}

pub(crate) fn slice_lines(content: &str, start: usize, end: usize) -> String {
    content
        .lines()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::fs;
    use std::path::Path;

    #[test]
    fn extract_paths_indexes_only_supplied_indexable_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(root.join("b.rs"), "pub fn beta() {}\n").unwrap();
        fs::write(root.join("notes.txt"), "not indexable\n").unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract_paths(
                root,
                Uuid::new_v4(),
                None,
                &[
                    root.join("a.rs"),
                    root.join("notes.txt"),
                    root.join("missing.rs"),
                ],
            )
            .unwrap();

        // Only a.rs is indexed: notes.txt is not indexable, missing.rs absent,
        // and b.rs was not supplied.
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "a.rs");
        assert!(result.nodes.iter().any(|n| n.name == "alpha"));
        assert!(!result.nodes.iter().any(|n| n.name == "beta"));
    }

    #[test]
    fn detect_file_kind_matches_legacy_indexable_set() {
        // Parity fixture: every path the pre-FileKind `is_indexable_path`
        // accepted (manifests, every extension, `.d.ts`) or rejected.
        let cases: &[(&str, bool)] = &[
            ("Cargo.toml", true),
            ("crates/core/Cargo.toml", true),
            ("package.json", true),
            ("infra/cdk.json", true),
            ("tsconfig.json", true),
            ("jsconfig.json", true),
            ("config/other.json", false),
            ("xCargo.toml", false),
            ("src/main.rs", true),
            ("README.md", true),
            ("docs/guide.mdx", true),
            ("NOTES.MD", true),
            ("contracts/Token.sol", true),
            ("paper.pdf", true),
            ("app.ts", true),
            ("app.tsx", true),
            ("app.mts", true),
            ("app.cts", true),
            ("app.js", true),
            ("app.jsx", true),
            ("app.mjs", true),
            ("app.cjs", true),
            ("types.d.ts", true),
            ("service.py", true),
            ("stubs.pyi", true),
            ("schema.graphql", true),
            ("queries.gql", true),
            ("types.graphqls", true),
            ("notes.txt", false),
            ("Makefile", false),
            ("image.png", false),
        ];
        for (path, expected) in cases {
            let path = Path::new(path);
            assert_eq!(
                detect_file_kind(path).is_some(),
                *expected,
                "{}",
                path.display()
            );
            assert_eq!(is_indexable_path(path), *expected, "{}", path.display());
        }
        // The data-carrying kinds resolve to the right language.
        assert_eq!(
            detect_file_kind(Path::new("types.d.ts")),
            Some(FileKind::JsTs(Language::TypeScript))
        );
        assert_eq!(
            detect_file_kind(Path::new("app.mjs")),
            Some(FileKind::JsTs(Language::JavaScript))
        );
        assert_eq!(
            detect_file_kind(Path::new("schema.GRAPHQL")),
            Some(FileKind::GraphQL)
        );
    }

    #[test]
    fn extract_paths_with_no_indexable_files_yields_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("readme.txt"), "plain text\n").unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract_paths(root, Uuid::new_v4(), None, &[root.join("readme.txt")])
            .unwrap();

        assert!(result.files.is_empty());
    }

    fn write_minimal_pdf(path: &Path) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let font_id = doc.new_object_id();
        let content_id = doc.new_object_id();
        let catalog_id = doc.new_object_id();

        doc.objects.insert(
            font_id,
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Helvetica",
            }
            .into(),
        );
        doc.objects.insert(
            content_id,
            Stream::new(
                dictionary! {},
                b"BT /F1 24 Tf 72 720 Td (OnChainLab PDF evidence) Tj ET".to_vec(),
            )
            .into(),
        );
        doc.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! {"Font" => dictionary! {"F1" => font_id}},
                "Contents" => content_id,
            }
            .into(),
        );
        doc.objects.insert(
            pages_id,
            dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }
            .into(),
        );
        doc.objects.insert(
            catalog_id,
            dictionary! {
                "Type" => "Catalog",
                "Pages" => pages_id,
            }
            .into(),
        );
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn extracts_typescript_symbols_and_package_metadata() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{
              "scripts": {"test": "vitest"},
              "dependencies": {"@aws-cdk/core": "^1.0.0"},
              "devDependencies": {"typescript": "^5.0.0"}
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("handler.ts"),
            r#"
              import type { Construct } from "constructs";
              import { Stack } from "aws-cdk-lib";

              export interface HandlerProps {
                name: string;
              }

              export type HandlerMode = "sync" | "async";

              export class HandlerStack extends Stack {
                configure() {
                  return buildThing();
                }
              }

              export function buildThing() {
                return "ok";
              }

              export const runHandler = async () => buildThing();
            "#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result
            .files
            .iter()
            .any(|file| file.path == "handler.ts" && file.language == Language::TypeScript));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "HandlerStack" && node.kind == NodeKind::Struct));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "HandlerProps" && node.kind == NodeKind::Trait));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "HandlerMode" && node.kind == NodeKind::TypeAlias));
        assert!(result.nodes.iter().any(|node| node.name == "typescript"));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "npm script test"));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "HandlerProps" && node.kind == NodeKind::Trait));
        assert!(result.edges.iter().any(|edge| edge.kind == EdgeKind::Calls));
        assert!(result
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("Language: typescript")));
    }

    #[test]
    fn aggregates_package_json_dependencies_and_scripts_into_single_chunks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{
              "scripts": {"build": "tsc", "test": "vitest", "lint": "eslint ."},
              "dependencies": {"aws-cdk-lib": "^2.0.0", "constructs": "^10.0.0", "zod": "^3.0.0"},
              "devDependencies": {"typescript": "^5.0.0", "vitest": "^1.0.0"}
            }"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        // Chunks must be aggregated: exactly one dependency chunk and one script
        // chunk per manifest, not one per dependency / script.
        let dependency_chunks = result
            .chunks
            .iter()
            .filter(|chunk| chunk.chunk_type == "dependency")
            .count();
        let script_chunks = result
            .chunks
            .iter()
            .filter(|chunk| chunk.chunk_type == "script")
            .count();
        assert_eq!(
            dependency_chunks, 1,
            "expected one aggregated dependency chunk"
        );
        assert_eq!(script_chunks, 1, "expected one aggregated script chunk");

        // The aggregated chunks must remain searchable: every dependency and
        // script name appears in its chunk content.
        let dep_chunk = result
            .chunks
            .iter()
            .find(|chunk| chunk.chunk_type == "dependency")
            .expect("dependency chunk present");
        for name in ["aws-cdk-lib", "constructs", "zod", "typescript", "vitest"] {
            assert!(
                dep_chunk.content.contains(name),
                "dependency chunk should mention {name}"
            );
        }
        // Preserve query-layer filtering, which keys off metadata.dependency.
        assert!(dep_chunk.metadata.get("dependency").is_some());

        let script_chunk = result
            .chunks
            .iter()
            .find(|chunk| chunk.chunk_type == "script")
            .expect("script chunk present");
        for name in ["build", "test", "lint"] {
            assert!(
                script_chunk.content.contains(name),
                "script chunk should mention {name}"
            );
        }
        assert!(script_chunk.metadata.get("script").is_some());

        // The knowledge graph must be preserved: individual dependency and
        // script nodes still exist.
        let dependency_nodes = result
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Dependency)
            .count();
        let script_nodes = result
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Script)
            .count();
        assert_eq!(dependency_nodes, 5, "all dependency nodes preserved");
        assert_eq!(script_nodes, 3, "all script nodes preserved");
    }

    #[test]
    fn extracts_aws_cdk_stacks_and_resources() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cdk.json"),
            r#"{"app":"npx ts-node --prefer-ts-exts bin/app.ts"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("stack.ts"),
            r#"
              import * as cdk from "aws-cdk-lib";
              import { Construct } from "constructs";
              import * as lambda from "aws-cdk-lib/aws-lambda";
              import * as dynamodb from "aws-cdk-lib/aws-dynamodb";

              export class InfraStack extends cdk.Stack {
                constructor(scope: Construct, id: string) {
                  super(scope, id);
                  const table = new dynamodb.Table(this, "JobsTable", {});
                  const fn = new lambda.Function(this, "WorkerFunction", {});
                }
              }
            "#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result.chunks.iter().any(|chunk| {
            chunk.chunk_type == "aws_cdk_app" && chunk.content.contains("CDK app command")
        }));
        assert!(result.chunks.iter().any(|chunk| {
            chunk.chunk_type == "aws_cdk_stack" && chunk.content.contains("InfraStack")
        }));
        assert!(result.chunks.iter().any(|chunk| {
            chunk.chunk_type == "aws_cdk_resource" && chunk.content.contains("JobsTable")
        }));
        assert!(result.nodes.iter().any(|node| {
            node.kind == NodeKind::DeploymentResource && node.name.contains("WorkerFunction")
        }));
    }

    #[test]
    fn detects_javascript_extensions() {
        assert_eq!(
            js_ts_language(Path::new("index.js")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            js_ts_language(Path::new("component.tsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            js_ts_language(Path::new("types.d.ts")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn extracts_markdown_as_supplemental_documentation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("README.md"),
            "# Project\n\nThis documentation may explain source behavior.\n",
        )
        .unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn source_truth() {}\n").unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result
            .files
            .iter()
            .any(|file| file.path == "README.md" && file.language == Language::Markdown));
        let doc_chunk = result
            .chunks
            .iter()
            .find(|chunk| chunk.chunk_type == "documentation")
            .expect("documentation chunk");
        assert_eq!(
            doc_chunk
                .metadata
                .get("source_priority")
                .and_then(|v| v.as_str()),
            Some("supplemental")
        );
        assert!(result.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Contains
                && edge.cost > 0.1
                && edge
                    .metadata
                    .get("source_priority")
                    .and_then(|v| v.as_str())
                    == Some("supplemental")
        }));
    }

    #[test]
    fn markdown_splits_into_heading_sections() {
        let content = "intro line\n\n# Setup\n\ninstall things\n\n## Docker\n\nrun the container\n\n# Usage\n\ncall the cli\n";
        let sections = markdown_sections(content);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].heading_path, Vec::<String>::new()); // preamble
        assert_eq!(sections[0].line_start, 1);
        assert_eq!(sections[1].heading_path, vec!["Setup"]);
        assert_eq!(sections[2].heading_path, vec!["Setup", "Docker"]);
        assert!(sections[2].text.contains("run the container"));
        assert!(!sections[2].text.contains("install things"));
        assert_eq!(sections[3].heading_path, vec!["Usage"]);
        // Line ranges advance and match the source.
        assert_eq!(sections[1].line_start, 3);
        assert!(sections[2].line_start > sections[1].line_start);
        assert!(sections[3].line_end >= sections[3].line_start);
    }

    #[test]
    fn markdown_preamble_becomes_own_section() {
        let sections = markdown_sections("frontmatter prose\n\n# First\n\nbody\n");
        assert_eq!(sections.len(), 2);
        assert!(sections[0].heading_path.is_empty());
        assert!(sections[0].text.contains("frontmatter prose"));
    }

    #[test]
    fn markdown_without_headings_falls_back_to_single_chunk() {
        assert!(markdown_sections("just prose\n\nno headings here\n").is_empty());

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("NOTES.md"), "just prose\n\nno headings\n").unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let docs: Vec<_> = result
            .chunks
            .iter()
            .filter(|c| c.chunk_type == "documentation")
            .collect();
        assert_eq!(docs.len(), 1);
        assert!(docs[0].content.starts_with("Documentation file: NOTES.md"));
    }

    #[test]
    fn markdown_deep_headings_stay_in_parent_section() {
        let content =
            "# Api\n\n## Client\n\n### Methods\n\n#### get\n\ndetails\n\n#### post\n\nmore\n";
        let sections = markdown_sections(content);
        // H4 headings don't open sections; they stay inside "Methods".
        assert_eq!(sections.len(), 3);
        let methods = sections.last().unwrap();
        assert_eq!(methods.heading_path, vec!["Api", "Client", "Methods"]);
        assert!(methods.text.contains("#### get"));
        assert!(methods.text.contains("#### post"));
    }

    #[test]
    fn markdown_headings_inside_code_fences_are_not_sections() {
        let content = "# Real\n\n```sh\n# not a heading\necho hi\n```\n\nafter\n";
        let sections = markdown_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, vec!["Real"]);
        assert!(sections[0].text.contains("# not a heading"));
    }

    /// Count of fence-delimiter lines must be even in every part — an odd
    /// count means a part starts or ends inside a code block.
    fn fence_lines(text: &str) -> usize {
        text.lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count()
    }

    #[test]
    fn oversized_markdown_section_keeps_code_fences_whole() {
        // Paragraphs + fenced blocks with BLANK LINES INSIDE (the trap the
        // generic splitter falls into) — no part may cut a fence open.
        let para = "Some prose explaining the API in a sentence or two.";
        let fence = format!(
            "```graphql\nmutation {{\n  doThing(arg: 1)\n\n  andMore(arg: 2)\n}}\n\n{}```",
            "query { x }\n".repeat(8)
        );
        let text = format!("{para}\n\n{fence}\n\n{para}\n\n{para}\n\n{fence}\n\n{para}\n");
        let parts = pack_markdown_section(&text, 300);
        assert!(parts.len() > 1);
        for part in &parts {
            assert_eq!(
                fence_lines(&part.text) % 2,
                0,
                "part cuts a code fence open:\n{}",
                part.text
            );
        }
    }

    #[test]
    fn oversized_code_fence_splits_rewrapped() {
        // A single fence far bigger than the budget: pieces must each be
        // re-wrapped as valid fenced blocks with the original language tag.
        let body = "select column_a, column_b from some_table;\n".repeat(40);
        let text = format!("```sql\n{body}```");
        let parts = pack_markdown_section(&text, 400);
        assert!(parts.len() > 1);
        for part in &parts {
            for piece in part.text.split("\n\n") {
                if piece.contains("select") {
                    assert!(piece.starts_with("```sql"), "piece lost its fence: {piece}");
                    assert!(piece.trim_end().ends_with("```"));
                }
            }
        }
    }

    #[test]
    fn oversized_table_repeats_header_on_each_part() {
        let mut table = String::from("| Event | Description |\n| --- | --- |\n");
        for i in 0..60 {
            table.push_str(&format!(
                "| Transfer{i} | Emitted when token {i} moves between two accounts somewhere |\n"
            ));
        }
        let parts = pack_markdown_section(&table, 500);
        assert!(parts.len() > 1);
        for part in &parts {
            let mut lines = part.text.lines();
            assert_eq!(lines.next(), Some("| Event | Description |"));
            assert_eq!(lines.next(), Some("| --- | --- |"));
        }
    }

    #[test]
    fn oversized_doc_sections_keep_heading_context_per_part() {
        let dir = tempfile::tempdir().unwrap();
        let fence = format!("```js\n{}```", "callApi(arg);\n".repeat(500));
        let body = format!("# Api\n\n## Mutations\n\nIntro paragraph.\n\n{fence}\n\nOutro.\n");
        assert!(body.len() > MAX_CHUNK_CHARS);
        fs::write(dir.path().join("REF.md"), &body).unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let parts: Vec<_> = result
            .chunks
            .iter()
            .filter(|c| c.chunk_type == "documentation" && c.metadata.get("split_part").is_some())
            .collect();
        assert!(
            parts.len() > 1,
            "oversized section must be packed into parts"
        );
        let mut prev_end = 0i32;
        for part in &parts {
            // Every part carries the heading-path context header…
            assert!(
                part.content
                    .starts_with("Documentation: REF.md > Api > Mutations (part"),
                "missing context header: {}",
                &part.content[..part.content.len().min(90)]
            );
            // …keeps fences balanced…
            assert_eq!(fence_lines(&part.content) % 2, 0);
            // …and advances through real line ranges.
            assert!(part.line_start.unwrap() > prev_end);
            prev_end = part.line_end.unwrap();
        }
        // The generic splitter must NOT have touched documentation chunks.
        assert!(parts.iter().all(|p| !p.content.contains("Chunk part:")));
    }

    #[test]
    fn pdf_text_packs_at_page_and_paragraph_boundaries() {
        let page = format!(
            "{}\n\n{}\n\n{}",
            "First paragraph of the page with some words.",
            "Second paragraph that is reasonably long as well.",
            "Third paragraph closing the page."
        );
        let content = format!("{page}\u{c}{page}\u{c}{page}");
        let budget = 220usize;
        let parts = pack_pdf_units(pdf_doc_units(&content, budget), budget);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.part.text.len() <= budget);
            assert!(p.page_end >= p.page_start);
            // Paragraphs stay whole: every piece present is a complete one.
            for piece in p.part.text.split("\n\n") {
                assert!(piece.ends_with('.'), "paragraph cut mid-flow: {piece}");
            }
        }
        assert_eq!(parts.first().unwrap().page_start, 1);
        assert_eq!(parts.last().unwrap().page_end, 3);
    }

    #[test]
    fn extracts_solidity_contracts_members_imports_and_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("OnChainLab.sol"),
            r#"
pragma solidity ^0.8.24;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import "./RootValidator.sol";

interface IValidator {
    event Validated(address indexed account);
}

contract OnChainLab is Ownable, IValidator {
    event AccountProvisioned(uint256 indexed tokenId, address account);
    modifier onlyEntryPoint() { _; }

    constructor(address owner) {}

    function execute(address target) external onlyEntryPoint {
        target.call("");
    }
}
"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result
            .files
            .iter()
            .any(|file| { file.path == "OnChainLab.sol" && file.language == Language::Solidity }));
        assert!(result.nodes.iter().any(|node| {
            node.name == "OnChainLab"
                && node.kind == NodeKind::Struct
                && node.metadata.get("solidity_kind").and_then(|v| v.as_str()) == Some("contract")
        }));
        assert!(result
            .nodes
            .iter()
            .any(|node| { node.name == "execute" && node.kind == NodeKind::Function }));
        assert!(result
            .nodes
            .iter()
            .any(|node| { node.name == "AccountProvisioned" && node.kind == NodeKind::Concept }));
        // Third-party npm-style solidity imports are external — dropped like
        // JS/TS externals; the dependency list lives in the manifest nodes.
        assert!(!result
            .nodes
            .iter()
            .any(|node| { node.name.contains("@openzeppelin") }));
        assert!(result
            .edges
            .iter()
            .any(|edge| edge.kind == EdgeKind::Implements));
    }

    #[test]
    fn detects_python_extensions() {
        assert_eq!(
            python_language(Path::new("service.py")),
            Some(Language::Python)
        );
        assert_eq!(
            python_language(Path::new("stubs.pyi")),
            Some(Language::Python)
        );
        assert_eq!(python_language(Path::new("main.rs")), None);
    }

    #[test]
    fn extracts_python_functions_classes_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("service.py"),
            r#"
import os
from typing import List
from .helpers import build


class AuthService:
    def authenticate(self, token: str) -> bool:
        return build(token)


def login(user):
    svc = AuthService()
    return svc.authenticate(user)
"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result
            .files
            .iter()
            .any(|file| file.path == "service.py" && file.language == Language::Python));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "AuthService" && node.kind == NodeKind::Struct));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "authenticate" && node.kind == NodeKind::Function));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "login" && node.kind == NodeKind::Function));
        // Stdlib/site-packages imports are external — dropped from the graph.
        assert!(!result
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Dependency && node.name == "os"));
        assert!(!result.nodes.iter().any(|node| node.name == "typing"));
        // The relative import is internal: normalized against the file's
        // package directory and deduped repo-wide.
        assert!(result.nodes.iter().any(|node| {
            node.kind == NodeKind::Dependency && node.stable_id == "import:path:helpers"
        }));
        assert!(result.edges.iter().any(|edge| edge.kind == EdgeKind::Calls));
        assert!(result
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("Language: python")));
    }

    #[test]
    fn python_captures_methods_and_relative_imports() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("svc.py"),
            "from .util import helper\nclass Service:\n    def run(self):\n        return helper()\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "Service" && n.kind == NodeKind::Struct));
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "run" && n.kind == NodeKind::Function));
        // `.util` normalizes against the importing file's package directory.
        assert!(result
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::Dependency && n.stable_id == "import:path:util"));
    }

    #[test]
    fn indexes_pdf_files_as_supplemental_documents() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_pdf(&dir.path().join("paper.pdf"));

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        assert!(result
            .files
            .iter()
            .any(|file| file.path == "paper.pdf" && file.language == Language::Pdf));
        assert!(result.chunks.iter().any(|chunk| {
            chunk.chunk_type == "pdf_documentation"
                && chunk.content.contains("OnChainLab PDF evidence")
                && chunk
                    .metadata
                    .get("source_priority")
                    .and_then(|v| v.as_str())
                    == Some("supplemental")
        }));
    }

    #[test]
    fn deduplicates_repo_global_stable_ids() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("packages/a")).unwrap();
        fs::create_dir_all(dir.path().join("packages/b")).unwrap();
        let package_json = r#"{"dependencies":{"typescript":"^5.0.0"},"scripts":{"build":"tsc"}}"#;
        fs::write(dir.path().join("packages/a/package.json"), package_json).unwrap();
        fs::write(dir.path().join("packages/b/package.json"), package_json).unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        let mut seen = HashMap::new();
        for node in &result.nodes {
            assert!(
                seen.insert(node.stable_id.clone(), node.id).is_none(),
                "duplicate stable_id: {}",
                node.stable_id
            );
        }
        assert!(result.nodes.iter().any(|node| node.name == "typescript"));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "npm script build"));
        assert!(result
            .nodes
            .iter()
            .any(|node| node.name == "npm script build" && node.kind == NodeKind::Script));
    }

    #[test]
    fn solidity_members_resolve_within_contract() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Vault.sol"),
            "interface IVault { function deposit() external; }\ncontract Vault is IVault, Pausable {\n    function deposit() public {}\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "Vault" && n.kind == NodeKind::Struct));
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "IVault" && n.kind == NodeKind::Trait));
        let bases: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| {
                n.metadata.get("relationship").and_then(|v| v.as_str()) == Some("inheritance")
            })
            .map(|n| n.name.as_str())
            .collect();
        assert!(bases.contains(&"IVault") && bases.contains(&"Pausable"));
    }

    #[test]
    fn drops_external_imports_keeps_internal_and_workspace() {
        let dir = tempfile::tempdir().unwrap();
        // A workspace package: its package.json name marks `@acme/ui` as internal.
        fs::create_dir_all(dir.path().join("packages/ui")).unwrap();
        fs::write(
            dir.path().join("packages/ui/package.json"),
            r#"{"name":"@acme/ui"}"#,
        )
        .unwrap();
        // Two app files importing: a third-party pkg (react), a workspace pkg
        // (@acme/ui), and a relative module (./helper).
        fs::write(
            dir.path().join("a.ts"),
            "import React from \"react\";\nimport { Button } from \"@acme/ui\";\nimport helper from \"./helper\";\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.ts"),
            "import React from \"react\";\nimport { Card } from \"@acme/ui/card\";\nimport other from \"./helper\";\n",
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        let count = |name: &str| {
            result
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Dependency && n.name == name)
                .count()
        };
        // External (node_modules) import is dropped entirely — no feature node.
        assert_eq!(count("react"), 0, "external `react` should be dropped");
        // Workspace package import is kept (deduped to one shared bare node) — it's
        // the user's own code, a legitimate feature relationship.
        assert_eq!(count("@acme/ui"), 1, "workspace `@acme/ui` kept");
        assert_eq!(count("@acme/ui/card"), 1, "workspace subpath kept");
        // Both files import the same relative target — ONE shared node, named
        // by its normalized path, with one Imports edge per importing file.
        assert_eq!(count("helper"), 1, "relative `./helper` deduped repo-wide");
        let helper = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "import:path:helper")
            .expect("normalized relative import node");
        assert_eq!(
            helper.file_id, None,
            "shared node must not belong to one file"
        );
        assert_eq!(
            result
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Imports && e.target_node_id == helper.id)
                .count(),
            2,
            "one Imports edge per importing file"
        );
    }

    #[test]
    fn rust_impl_methods_become_method_nodes_and_chunks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("svc.rs"),
            r#"pub struct Service;

impl Service {
    pub const LIMIT: usize = 8;

    pub fn new() -> Self {
        Service
    }

    pub fn run(&self) -> usize {
        Self::LIMIT
    }
}
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let impl_node = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Impl && n.name == "Service")
            .expect("impl node");
        let new_method = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Method && n.name == "new")
            .expect("method node for new");
        let run_method = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Method && n.name == "run")
            .expect("method node for run");
        assert_eq!(new_method.stable_id, "svc.rs:method:Service::new");
        // Span-exact method lines.
        assert_eq!(new_method.line_start, Some(6));
        assert_eq!(new_method.line_end, Some(8));
        // Methods are contained by the impl (local star), not the file.
        assert!(result.edges.iter().any(|e| e.kind == EdgeKind::Contains
            && e.source_node_id == impl_node.id
            && e.target_node_id == run_method.id));
        // The impl chunk has the const + roster but no method bodies.
        let impl_chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(impl_node.id))
            .expect("impl chunk");
        assert!(impl_chunk.content.contains("LIMIT"));
        assert!(impl_chunk
            .content
            .contains("Methods (extracted separately): new, run"));
        assert!(!impl_chunk.content.contains("Service\n    }"));
        // Each method has its own chunk with its body.
        let run_chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(run_method.id))
            .expect("method chunk");
        assert!(run_chunk.content.contains("Self::LIMIT"));
        assert_eq!(run_chunk.chunk_type, "method");
    }

    #[test]
    fn rust_trait_impl_method_stable_ids_disambiguate() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("fmt.rs"),
            r#"pub struct Thing;

impl Thing {
    pub fn fmt(&self) -> String {
        String::new()
    }
}

impl std::fmt::Display for Thing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "thing")
    }
}
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let fmt_ids: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Method && n.name == "fmt")
            .map(|n| n.stable_id.as_str())
            .collect();
        assert_eq!(fmt_ids.len(), 2, "both fmt methods extracted: {fmt_ids:?}");
        assert!(fmt_ids.contains(&"fmt.rs:method:Thing::fmt"));
        assert!(fmt_ids
            .iter()
            .any(|id| id.contains("Display") && id.ends_with("::fmt")));
    }

    #[test]
    fn rust_inline_module_items_extracted_individually() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            r#"pub fn helper() -> usize {
    1
}

mod tests {
    #[test]
    fn helper() {
        assert_eq!(1, 1);
    }
}
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // Top-level fn keeps its historical stable_id; the nested test fn gets
        // the module prefix, so the two `helper`s don't dedupe into one node.
        assert!(result
            .nodes
            .iter()
            .any(|n| n.stable_id == "lib.rs:function:helper"));
        let nested = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "lib.rs:test:tests::helper")
            .expect("nested test fn extracted with mod prefix");
        assert_eq!(nested.kind, NodeKind::Test);
        // The module chunk is header-only: no test body inside.
        let module = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Module && n.name == "tests")
            .expect("module node");
        let module_chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(module.id))
            .expect("module chunk");
        assert!(!module_chunk.content.contains("assert_eq!"));
    }

    #[test]
    fn rust_span_lines_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        // A comment mentioning `fn target` BEFORE the real definition would
        // fool the old text-search line attribution; spans are exact.
        fs::write(
            dir.path().join("x.rs"),
            "// the fn target lives below\n\npub fn target() -> usize {\n    1\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let target = result
            .nodes
            .iter()
            .find(|n| n.name == "target" && n.kind == NodeKind::Function)
            .unwrap();
        assert_eq!(target.line_start, Some(3));
        assert_eq!(target.line_end, Some(5));
    }

    #[test]
    fn rust_method_call_edges_resolve() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub struct S;\n\nimpl S {\n    pub fn uniquely_named_helper(&self) -> usize {\n        1\n    }\n\n    pub fn driver(&self) -> usize {\n        self.uniquely_named_helper()\n    }\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let helper = result
            .nodes
            .iter()
            .find(|n| n.name == "uniquely_named_helper" && n.kind == NodeKind::Method)
            .unwrap();
        let driver = result
            .nodes
            .iter()
            .find(|n| n.name == "driver" && n.kind == NodeKind::Method)
            .unwrap();
        assert!(result.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.source_node_id == driver.id
            && e.target_node_id == helper.id));
    }

    #[test]
    fn impl_roster_does_not_fabricate_call_edges() {
        let dir = tempfile::tempdir().unwrap();
        // `solo` is called nowhere; it must not gain a Calls edge merely from
        // appearing in the impl chunk's method roster.
        fs::write(
            dir.path().join("a.rs"),
            "pub struct S;\n\nimpl S {\n    pub fn solo_method_name(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let solo = result
            .nodes
            .iter()
            .find(|n| n.name == "solo_method_name")
            .unwrap();
        assert!(!result
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && e.target_node_id == solo.id));
    }

    #[test]
    fn ts_class_chunk_excludes_method_bodies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("widget.ts"),
            r#"export class Widget {
  private count: number = 0;
  readonly label: string = "w";

  render(): string {
    return uniqueRenderBody();
  }

  reset(): void {
    this.count = 0;
  }
}

function uniqueRenderBody(): string {
  return "x";
}
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let class = result
            .nodes
            .iter()
            .find(|n| n.name == "Widget" && n.kind == NodeKind::Struct)
            .expect("class node");
        let class_chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(class.id))
            .expect("class chunk");
        // Header + fields + roster, no method bodies.
        assert!(class_chunk.content.contains("export class Widget"));
        assert!(class_chunk.content.contains("private count"));
        assert!(class_chunk.content.contains("readonly label"));
        assert!(class_chunk
            .content
            .contains("Methods (extracted separately): render, reset"));
        assert!(!class_chunk.content.contains("uniqueRenderBody()"));
        // Methods still have their own chunks with full bodies.
        let render = result
            .nodes
            .iter()
            .find(|n| n.name == "render" && n.kind == NodeKind::Function)
            .expect("method symbol");
        let render_chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(render.id))
            .expect("method chunk");
        assert!(render_chunk.content.contains("uniqueRenderBody()"));
    }

    #[test]
    fn rust_drops_external_uses_keeps_and_dedupes_workspace_uses() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "use std::fmt;\nuse serde::Serialize;\nuse crate::models::Thing;\npub fn a() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.rs"),
            "use crate::models::Thing;\nuse my_crate::other;\npub fn b() {}\n",
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // std/third-party uses are dropped.
        assert!(!result.nodes.iter().any(|n| n.name == "use std::fmt"));
        assert!(!result.nodes.iter().any(|n| n.name.contains("serde")));
        // crate-internal and workspace-crate uses are kept; the identical
        // statement in two files dedupes to ONE shared node with 2 edges.
        let shared = result
            .nodes
            .iter()
            .find(|n| n.name == "use crate::models::Thing")
            .expect("internal use kept");
        assert!(shared.stable_id.starts_with("import:rust:"));
        assert_eq!(shared.file_id, None);
        assert_eq!(
            result
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Imports && e.target_node_id == shared.id)
                .count(),
            2
        );
        assert!(result.nodes.iter().any(|n| n.name == "use my_crate::other"));
    }

    #[test]
    fn python_drops_external_imports_keeps_internal() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("mypkg")).unwrap();
        fs::write(dir.path().join("mypkg/__init__.py"), "").unwrap();
        fs::write(
            dir.path().join("mypkg/mod.py"),
            "def inner():\n    return 1\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import os\nimport requests\nimport mypkg.mod\n\ndef main():\n    return mypkg.mod.inner()\n",
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let deps: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Dependency)
            .map(|n| n.stable_id.as_str())
            .collect();
        assert!(!deps
            .iter()
            .any(|s| s.ends_with(":os") || s.contains("import:path:os")));
        assert!(!deps.iter().any(|s| s.contains("requests")));
        assert!(
            deps.contains(&"import:path:mypkg/mod"),
            "internal absolute import kept: {deps:?}"
        );
    }

    #[test]
    fn relative_imports_dedupe_to_one_node_per_target() {
        // Different specifiers, same target: `src/a.ts` imports `./lib/utils`,
        // `src/deep/b.ts` imports `../lib/utils` — both normalize to
        // `src/lib/utils` and share one node.
        assert_eq!(
            normalize_relative_import("src/a.ts", "./lib/utils"),
            "src/lib/utils"
        );
        assert_eq!(
            normalize_relative_import("src/deep/b.ts", "../lib/utils"),
            "src/lib/utils"
        );
        // Python: dots resolve against the package directory.
        assert_eq!(
            python_relative_target("pkg/sub/mod.py", ".util"),
            "pkg/sub/util"
        );
        assert_eq!(
            python_relative_target("pkg/sub/mod.py", "..core.db"),
            "pkg/core/db"
        );
        assert_eq!(python_relative_target("top.py", ".x"), "x");
    }

    #[test]
    fn external_import_classification() {
        let ws: HashSet<String> = ["@acme/ui".to_string(), "shared-lib".to_string()]
            .into_iter()
            .collect();
        // Third-party → external (dropped).
        assert!(is_external_import("react", &ws));
        assert!(is_external_import("@anthropic-ai/sdk", &ws));
        assert!(is_external_import("next/navigation", &ws));
        // Workspace packages (and their subpaths) → internal.
        assert!(!is_external_import("@acme/ui", &ws));
        assert!(!is_external_import("@acme/ui/card", &ws));
        assert!(!is_external_import("shared-lib/util", &ws));
        // Relative / alias / root → always internal.
        assert!(!is_external_import("./foo", &ws));
        assert!(!is_external_import("../bar", &ws));
        assert!(!is_external_import("@/app/x", &ws));
        assert!(!is_external_import("~/lib/y", &ws));
        // Package-portion extraction.
        assert_eq!(import_package_portion("@scope/name/sub"), "@scope/name");
        assert_eq!(import_package_portion("name/sub/deep"), "name");
        assert_eq!(import_package_portion("react"), "react");
    }

    #[test]
    fn javascript_captures_methods_and_arrow_functions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("widget.ts"),
            "import { dep } from \"./dep\";\nexport const make = () => 1;\nexport class Widget {\n  render() { return make(); }\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "make" && n.kind == NodeKind::Function));
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "Widget" && n.kind == NodeKind::Struct));
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "render" && n.kind == NodeKind::Function));
        assert!(result
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::Dependency && n.stable_id == "import:path:dep"));
    }

    #[test]
    fn call_edges_resolve_to_local_definition() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.py"),
            "def helper():\n    return 1\ndef run():\n    return helper()\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.py"), "def helper():\n    return 2\n").unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let a_helper = result
            .nodes
            .iter()
            .find(|n| {
                n.name == "helper"
                    && n.metadata.get("file").and_then(|v| v.as_str()) == Some("a.py")
            })
            .unwrap();
        let run = result.nodes.iter().find(|n| n.name == "run").unwrap();
        assert!(result.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.source_node_id == run.id
            && e.target_node_id == a_helper.id));
    }

    #[test]
    fn ambiguous_global_callee_emits_no_call_edge() {
        let dir = tempfile::tempdir().unwrap();
        // `helper` is defined in two files; `caller.py` defines none locally,
        // so the global fallback must NOT pick one arbitrarily.
        fs::write(dir.path().join("a.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(dir.path().join("b.py"), "def helper():\n    return 2\n").unwrap();
        fs::write(
            dir.path().join("caller.py"),
            "def run():\n    return helper()\ndef use_unique():\n    return unique_fn()\n",
        )
        .unwrap();
        fs::write(dir.path().join("c.py"), "def unique_fn():\n    return 3\n").unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let run = result.nodes.iter().find(|n| n.name == "run").unwrap();
        assert!(
            !result
                .edges
                .iter()
                .any(|e| e.kind == EdgeKind::Calls && e.source_node_id == run.id),
            "ambiguous callee must not resolve globally"
        );
        // A repo-unique name still resolves across files.
        let user = result
            .nodes
            .iter()
            .find(|n| n.name == "use_unique")
            .unwrap();
        let unique = result.nodes.iter().find(|n| n.name == "unique_fn").unwrap();
        assert!(result.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.source_node_id == user.id
            && e.target_node_id == unique.id));
    }

    #[test]
    fn call_edges_ignore_names_only_in_comments() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("c.py"),
            "def helper():\n    return 1\ndef run():\n    # helper() in a comment\n    return 2\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        let run = result.nodes.iter().find(|n| n.name == "run").unwrap();
        let helper = result.nodes.iter().find(|n| n.name == "helper").unwrap();
        assert!(!result.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.source_node_id == run.id
            && e.target_node_id == helper.id));
    }

    #[test]
    fn invalid_source_file_degrades_to_file_node_without_aborting() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok.py"), "def fine():\n    return 1\n").unwrap();
        fs::write(
            dir.path().join("broken.py"),
            "def (:\n    this is not python\n",
        )
        .unwrap();
        fs::write(dir.path().join("broken.rs"), "fn broken( {{{ not rust\n").unwrap();
        fs::write(
            dir.path().join("broken.sol"),
            "contract {{{ this is not solidity ]]]\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        // Must NOT return Err — one bad file cannot abort the whole run.
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
        // The good file's symbol is present.
        assert!(result
            .nodes
            .iter()
            .any(|n| n.name == "fine" && n.kind == NodeKind::Function));
        for broken in ["broken.py", "broken.rs", "broken.sol"] {
            // The broken file still produced a File node (degrade, not drop)…
            assert!(
                result
                    .nodes
                    .iter()
                    .any(|n| n.kind == NodeKind::File && n.name == broken),
                "file node for {broken}"
            );
            // …plus a whole-file fallback chunk, so it stays retrievable.
            assert!(
                result.chunks.iter().any(|c| {
                    c.metadata.get("kind").and_then(|v| v.as_str()) == Some("whole_file_fallback")
                        && c.metadata.get("file").and_then(|v| v.as_str()) == Some(broken)
                }),
                "whole-file fallback chunk for {broken}"
            );
            // No symbols were fabricated from the broken file.
            assert!(
                !result.nodes.iter().any(|n| {
                    n.metadata.get("file").and_then(|v| v.as_str()) == Some(broken)
                        && n.kind == NodeKind::Function
                }),
                "no fabricated symbols for {broken}"
            );
        }
        // The parser-rejected files carry the parse_failed reason.
        for broken in ["broken.py", "broken.rs"] {
            assert!(
                result.chunks.iter().any(|c| {
                    c.metadata.get("file").and_then(|v| v.as_str()) == Some(broken)
                        && c.metadata.get("reason").and_then(|v| v.as_str()) == Some("parse_failed")
                }),
                "parse_failed reason for {broken}"
            );
        }
    }

    /// Find the graphql-language node named `name` (types, operations,
    /// fragments, and surface fields all carry `metadata.language: "graphql"`).
    fn graphql_node<'a>(result: &'a ExtractionResult, name: &str) -> &'a KnowledgeNode {
        result
            .nodes
            .iter()
            .find(|n| {
                n.name == name
                    && n.metadata.get("language").and_then(|v| v.as_str()) == Some("graphql")
            })
            .unwrap_or_else(|| panic!("graphql node {name}"))
    }

    fn has_edge(
        result: &ExtractionResult,
        kind: EdgeKind,
        source: &KnowledgeNode,
        target: &KnowledgeNode,
    ) -> bool {
        result.edges.iter().any(|e| {
            e.kind == kind && e.source_node_id == source.id && e.target_node_id == target.id
        })
    }

    #[test]
    fn graphql_sdl_types_root_fields_and_edges_extracted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("schema.graphql"),
            r#"schema {
  query: MyQuery
}

type MyQuery {
  user(id: ID!): User
  search(term: String!): SearchResult
}

interface Node {
  id: ID!
}

type User implements Node {
  id: ID!
  role: Role
  created: DateTime
  home: Location
}

type Location {
  lat: Float
  lng: Float
}

enum Role {
  ADMIN
  MEMBER
}

union SearchResult = User | Location

scalar DateTime

input UserFilter {
  role: Role
}

directive @auth(requires: Role) on FIELD_DEFINITION
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // SDL kinds map onto the existing node-kind vocabulary.
        let my_query = graphql_node(&result, "MyQuery");
        assert_eq!(my_query.kind, NodeKind::Struct);
        assert_eq!(my_query.stable_id, "schema.graphql:graphql:object:MyQuery");
        assert_eq!(graphql_node(&result, "Node").kind, NodeKind::Trait);
        assert_eq!(graphql_node(&result, "Role").kind, NodeKind::Enum);
        assert_eq!(
            graphql_node(&result, "SearchResult").kind,
            NodeKind::TypeAlias
        );
        assert_eq!(graphql_node(&result, "DateTime").kind, NodeKind::TypeAlias);
        assert_eq!(graphql_node(&result, "UserFilter").kind, NodeKind::Struct);
        assert_eq!(graphql_node(&result, "auth").kind, NodeKind::TypeAlias);

        let user = graphql_node(&result, "User");
        let node_iface = graphql_node(&result, "Node");
        assert!(has_edge(&result, EdgeKind::Implements, user, node_iface));

        // Field/union/input type references become UsesType edges.
        let role = graphql_node(&result, "Role");
        let location = graphql_node(&result, "Location");
        let search = graphql_node(&result, "SearchResult");
        let filter = graphql_node(&result, "UserFilter");
        assert!(has_edge(&result, EdgeKind::UsesType, my_query, user));
        assert!(has_edge(&result, EdgeKind::UsesType, user, role));
        assert!(has_edge(&result, EdgeKind::UsesType, user, location));
        assert!(has_edge(&result, EdgeKind::UsesType, search, user));
        assert!(has_edge(&result, EdgeKind::UsesType, filter, role));

        // Hub gate: no UsesType edges TO the custom scalar, even though
        // `User.created: DateTime` references it.
        let scalar = graphql_node(&result, "DateTime");
        assert!(
            !result
                .edges
                .iter()
                .any(|e| e.kind == EdgeKind::UsesType && e.target_node_id == scalar.id),
            "no UsesType edges to a custom scalar"
        );

        // Root fields honor `schema { query: MyQuery }` and become
        // user-surface nodes with a Defines edge from the file node.
        let field = graphql_node(&result, "MyQuery.user");
        assert_eq!(field.kind, NodeKind::GraphqlField);
        assert_eq!(field.metadata["operation_type"], "query");
        assert_eq!(field.metadata["parent_type"], "MyQuery");
        assert_eq!(field.metadata["args"][0], "id: ID!");
        assert_eq!(
            graphql_node(&result, "MyQuery.search").kind,
            NodeKind::GraphqlField
        );
        let file_node = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::File && n.name == "schema.graphql")
            .unwrap();
        assert!(has_edge(&result, EdgeKind::Defines, file_node, field));
        // Non-root types expose no surface fields.
        assert!(!result
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::GraphqlField && n.name.starts_with("User.")));
    }

    #[test]
    fn graphql_type_extension_links_to_base_across_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.graphql"),
            "type Query {\n  user: User\n}\n\ntype User {\n  id: ID!\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.graphql"),
            "extend type Query {\n  audits: [User!]!\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // Linked, not folded: base and extension stay separate nodes.
        let base = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "a.graphql:graphql:object:Query")
            .expect("base type node");
        let ext = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "b.graphql:graphql:extend_object:Query")
            .expect("extension node");
        let extends = result
            .edges
            .iter()
            .find(|e| {
                e.kind == EdgeKind::UsesType
                    && e.source_node_id == ext.id
                    && e.target_node_id == base.id
            })
            .expect("extension -> base edge");
        assert_eq!(extends.metadata["relation"], "extends");

        // The extension's field type resolves cross-file too.
        let user = graphql_node(&result, "User");
        assert!(has_edge(&result, EdgeKind::UsesType, ext, user));

        // Extending a root type surfaces its fields.
        let field = graphql_node(&result, "Query.audits");
        assert_eq!(field.kind, NodeKind::GraphqlField);
        assert_eq!(field.metadata["operation_type"], "query");
    }

    #[test]
    fn graphql_executable_documents_extract_operations_and_fragments() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("types.graphql"),
            "type Query {\n  user(id: ID!): User\n  health: String\n}\n\ntype User {\n  id: ID!\n  name: String\n}\n\ntype AdminUser {\n  permissions: [String!]!\n}\n\ninput UserFilter {\n  name: String\n}\n",
        )
        .unwrap();
        // The spread references UserParts BEFORE its definition, and the
        // types live in another file: both must resolve via the post-pass.
        fs::write(
            dir.path().join("queries.graphql"),
            "query GetUser($id: ID!, $filter: UserFilter) {\n  user(id: $id) {\n    ...UserParts\n    ... on AdminUser {\n      permissions\n    }\n  }\n}\n\nfragment UserParts on User {\n  id\n  name\n}\n",
        )
        .unwrap();
        fs::write(dir.path().join("anon.graphql"), "{\n  health\n}\n").unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let op = graphql_node(&result, "GetUser");
        assert_eq!(op.kind, NodeKind::GraphqlOperation);
        assert_eq!(op.stable_id, "queries.graphql:graphql:operation:GetUser");
        assert_eq!(op.metadata["operation_type"], "query");
        assert_eq!(op.metadata["root_fields"], json!(["user"]));

        // Anonymous operations synthesize operation-type + first root field.
        let anon = graphql_node(&result, "query:health");
        assert_eq!(anon.kind, NodeKind::GraphqlOperation);

        let fragment = graphql_node(&result, "UserParts");
        assert_eq!(fragment.kind, NodeKind::GraphqlFragment);
        assert_eq!(fragment.metadata["type_condition"], "User");

        // Spread -> Calls (forward reference inside the file).
        assert!(has_edge(&result, EdgeKind::Calls, op, fragment));
        // Inline fragment + variable types -> UsesType (cross-file).
        let admin = graphql_node(&result, "AdminUser");
        let filter = graphql_node(&result, "UserFilter");
        assert!(has_edge(&result, EdgeKind::UsesType, op, admin));
        assert!(has_edge(&result, EdgeKind::UsesType, op, filter));
        // Fragment type condition -> UsesType (cross-file).
        let user = graphql_node(&result, "User");
        assert!(has_edge(&result, EdgeKind::UsesType, fragment, user));
    }

    #[test]
    fn graphql_broken_file_degrades_and_partial_tree_still_extracts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("partial.graphql"),
            "type Good {\n  id: ID!\n}\n\ntype {{{%%\n",
        )
        .unwrap();
        fs::write(dir.path().join("broken.graphql"), "%%% not graphql &&&\n").unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        // Must NOT return Err — apollo-parser degrades, never aborts the run.
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // The recoverable definition in the partially-broken file survives.
        assert_eq!(graphql_node(&result, "Good").kind, NodeKind::Struct);

        // The unrecoverable file degrades to a file node + fallback chunk.
        assert!(result
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::File && n.name == "broken.graphql"));
        assert!(
            result.chunks.iter().any(|c| {
                c.metadata.get("kind").and_then(|v| v.as_str()) == Some("whole_file_fallback")
                    && c.metadata.get("file").and_then(|v| v.as_str()) == Some("broken.graphql")
                    && c.metadata.get("reason").and_then(|v| v.as_str()) == Some("parse_failed")
            }),
            "whole-file fallback chunk with parse_failed reason for broken.graphql"
        );
    }

    #[test]
    fn graphql_uses_type_fan_in_cap_drops_hub_edges() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("shared.graphql"),
            "type SharedThing {\n  id: ID!\n}\n",
        )
        .unwrap();
        // 14 distinct files referencing one type: only the first 12 distinct
        // files (deterministic order) keep their UsesType edges.
        for i in 0..14 {
            fs::write(
                dir.path().join(format!("t{i:02}.graphql")),
                format!("type Consumer{i:02} {{\n  s: SharedThing\n}}\n"),
            )
            .unwrap();
        }
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let shared = graphql_node(&result, "SharedThing");
        let fan_in = result
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::UsesType && e.target_node_id == shared.id)
            .count();
        assert_eq!(fan_in, 12, "fan-in cap keeps 12 of 14 referencing files");
    }

    #[test]
    fn graphql_type_does_not_capture_same_named_python_call() {
        let dir = tempfile::tempdir().unwrap();
        // The repo's only `User` definition is a GraphQL SDL type: the
        // unique-name global fallback must NOT resolve the Python call to it.
        fs::write(
            dir.path().join("schema.graphql"),
            "type User {\n  id: ID!\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("app.py"),
            "def make_user():\n    return User(1)\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let user_type = graphql_node(&result, "User");
        let maker = result.nodes.iter().find(|n| n.name == "make_user").unwrap();
        assert!(
            !result.edges.iter().any(|e| e.kind == EdgeKind::Calls
                && e.source_node_id == maker.id
                && e.target_node_id == user_type.id),
            "a Python call must not resolve to a GraphQL schema type"
        );
    }

    #[test]
    fn graphql_type_does_not_make_a_code_symbol_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        // `User` is repo-unique among CODE symbols; the same-named schema type
        // must not bump global_count and kill the cross-file resolution.
        fs::write(
            dir.path().join("a.ts"),
            "export function User() {\n  return 1;\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.ts"),
            "export function build() {\n  return User();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("schema.graphql"),
            "type User {\n  id: ID!\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let ts_user = result
            .nodes
            .iter()
            .find(|n| {
                n.name == "User" && n.metadata.get("file").and_then(|v| v.as_str()) == Some("a.ts")
            })
            .unwrap();
        let build = result.nodes.iter().find(|n| n.name == "build").unwrap();
        assert!(
            result.edges.iter().any(|e| e.kind == EdgeKind::Calls
                && e.source_node_id == build.id
                && e.target_node_id == ts_user.id),
            "cross-file call to the TS `User` must survive indexing schema.graphql"
        );
    }

    #[test]
    fn embedded_graphql_type_never_claims_interpolation_calls() {
        let dir = tempfile::tempdir().unwrap();
        // The embedded `type Widget` node spans the whole host template, so
        // without the language gate it wins innermost_caller for the call
        // inside the `${…}` hole and a Calls edge is emitted FROM the schema
        // type TO the JS helper.
        fs::write(
            dir.path().join("schema.ts"),
            r#"import { gql } from "@apollo/client";

export function fieldList() { return "id name"; }

export const WIDGET = gql`
  type Widget {
    id: ID!
  }
  ${fieldList()}
`;
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let widget = graphql_node(&result, "Widget");
        assert!(
            !result
                .edges
                .iter()
                .any(|e| e.kind == EdgeKind::Calls && e.source_node_id == widget.id),
            "an embedded schema type must not be attributed calls from interpolation holes"
        );
    }

    #[test]
    fn graphql_scalar_extension_links_to_its_base() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.graphql"), "scalar DateTime\n").unwrap();
        fs::write(
            dir.path().join("b.graphql"),
            "extend scalar DateTime @specifiedBy(url: \"https://example.com/iso\")\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let base = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "a.graphql:graphql:scalar:DateTime")
            .expect("base scalar node");
        let ext = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "b.graphql:graphql:extend_scalar:DateTime")
            .expect("scalar extension node");
        let extends = result
            .edges
            .iter()
            .find(|e| {
                e.kind == EdgeKind::UsesType
                    && e.source_node_id == ext.id
                    && e.target_node_id == base.id
            })
            .expect("extension -> base edge despite the scalar gate");
        assert_eq!(extends.metadata["relation"], "extends");
    }

    #[test]
    fn graphql_extension_link_survives_the_fan_in_cap() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("shared.graphql"),
            "type SharedThing {\n  id: ID!\n}\n",
        )
        .unwrap();
        // 13 consumer files fill the fan-in cap (12 slots) before the
        // extension file (sorted last) is processed.
        for i in 0..13 {
            fs::write(
                dir.path().join(format!("t{i:02}.graphql")),
                format!("type Consumer{i:02} {{\n  s: SharedThing\n}}\n"),
            )
            .unwrap();
        }
        fs::write(
            dir.path().join("z_extend.graphql"),
            "extend type SharedThing {\n  extra: Int\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let shared = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "shared.graphql:graphql:object:SharedThing")
            .expect("base type node");
        let ext = result
            .nodes
            .iter()
            .find(|n| n.stable_id == "z_extend.graphql:graphql:extend_object:SharedThing")
            .expect("extension node");
        let extends = result
            .edges
            .iter()
            .find(|e| {
                e.kind == EdgeKind::UsesType
                    && e.source_node_id == ext.id
                    && e.target_node_id == shared.id
            })
            .expect("extension -> base edge despite a full fan-in cap");
        assert_eq!(extends.metadata["relation"], "extends");
        // The cap itself still holds for plain uses_type refs.
        let plain_fan_in = result
            .edges
            .iter()
            .filter(|e| {
                e.kind == EdgeKind::UsesType
                    && e.target_node_id == shared.id
                    && e.metadata["relation"] == "uses_type"
            })
            .count();
        assert_eq!(
            plain_fan_in, 12,
            "cap keeps 12 of 13 plain referencing files"
        );
    }

    #[test]
    fn graphql_extend_schema_keeps_default_root_fields() {
        let dir = tempfile::tempdir().unwrap();
        // `extend schema` ADDS to the (default) roots per the spec — it must
        // not wipe the Query default and unsurface Query.user.
        fs::write(
            dir.path().join("schema.graphql"),
            "extend schema {\n  subscription: Sub\n}\n\ntype Query {\n  user(id: ID!): User\n}\n\ntype Sub {\n  ticks: Int\n}\n\ntype User {\n  id: ID!\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let user_field = graphql_node(&result, "Query.user");
        assert_eq!(user_field.kind, NodeKind::GraphqlField);
        assert_eq!(user_field.metadata["operation_type"], "query");
        let ticks = graphql_node(&result, "Sub.ticks");
        assert_eq!(ticks.metadata["operation_type"], "subscription");
    }

    #[test]
    fn graphql_split_file_custom_root_surfaces_fields() {
        let dir = tempfile::tempdir().unwrap();
        // The mapping and the root type live in DIFFERENT files: the run-level
        // post-pass must aggregate the mapping and surface MyQuery.labs.
        fs::write(
            dir.path().join("schema.graphql"),
            "schema {\n  query: MyQuery\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("query.graphql"),
            "type MyQuery {\n  labs: [Lab!]!\n}\n\ntype Lab {\n  id: ID!\n}\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let field = graphql_node(&result, "MyQuery.labs");
        assert_eq!(field.kind, NodeKind::GraphqlField);
        assert_eq!(field.metadata["operation_type"], "query");
        assert_eq!(field.metadata["parent_type"], "MyQuery");
        assert_eq!(field.metadata["file"], "query.graphql");
        // Exactly once — the post-pass must not double-emit anything.
        assert_eq!(
            result
                .nodes
                .iter()
                .filter(|n| n.name == "MyQuery.labs")
                .count(),
            1
        );
        let file_node = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::File && n.name == "query.graphql")
            .unwrap();
        assert!(has_edge(&result, EdgeKind::Defines, file_node, field));
    }

    #[test]
    fn ts_gql_tagged_template_and_call_extract_embedded_documents() {
        let dir = tempfile::tempdir().unwrap();
        // Line numbers matter: the operation template spans host lines 3-10
        // (incl. the `${EXTRA}` interpolation hole), the call-form fragment
        // template spans host lines 12-16.
        fs::write(
            dir.path().join("queries.ts"),
            r#"import { gql } from "@apollo/client";

export const GET_USER = gql`
  query GetUser($id: ID!) {
    user(id: $id) {
      ...UserParts
    }
  }
  ${EXTRA}
`;

export const USER_PARTS = gql(`
  fragment UserParts on User {
    name
  }
`);
"#,
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        // Tagged template -> operation node anchored to the HOST template span.
        let op = graphql_node(&result, "GetUser");
        assert_eq!(op.kind, NodeKind::GraphqlOperation);
        assert_eq!(op.stable_id, "queries.ts:graphql:operation:GetUser");
        assert_eq!(op.metadata["origin"], "gql-tagged-template");
        assert_eq!(op.metadata["file"], "queries.ts");
        assert_eq!(op.metadata["root_fields"], json!(["user"]));
        assert_eq!(op.line_start, Some(3));
        assert_eq!(op.line_end, Some(10));

        // gql() call form -> fragment node anchored to its template span.
        let fragment = graphql_node(&result, "UserParts");
        assert_eq!(fragment.kind, NodeKind::GraphqlFragment);
        assert_eq!(fragment.metadata["origin"], "gql-call");
        assert_eq!(fragment.line_start, Some(12));
        assert_eq!(fragment.line_end, Some(16));

        // The spread resolves across embedded documents via the post-pass.
        assert!(has_edge(&result, EdgeKind::Calls, op, fragment));

        // The chunk body is the GraphQL document, never a host-file slice.
        let chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(op.id))
            .expect("operation chunk");
        assert!(chunk.content.contains("query GetUser"));
        assert!(chunk.content.contains("...UserParts"));
        assert!(
            !chunk.content.contains("export const"),
            "chunk body must be the document, not a host slice"
        );
    }

    #[test]
    fn python_gql_call_extracts_embedded_document() {
        let dir = tempfile::tempdir().unwrap();
        // The gql("""…""") call spans host lines 3-9.
        fs::write(
            dir.path().join("client.py"),
            "from gql import gql\n\nGET_USER = gql(\"\"\"\nquery GetUser {\n  user {\n    id\n  }\n}\n\"\"\")\n",
        )
        .unwrap();
        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let op = graphql_node(&result, "GetUser");
        assert_eq!(op.kind, NodeKind::GraphqlOperation);
        assert_eq!(op.stable_id, "client.py:graphql:operation:GetUser");
        assert_eq!(op.metadata["origin"], "gql-call");
        assert_eq!(op.metadata["file"], "client.py");
        assert_eq!(op.metadata["root_fields"], json!(["user"]));
        assert_eq!(op.line_start, Some(3));
        assert_eq!(op.line_end, Some(9));

        let chunk = result
            .chunks
            .iter()
            .find(|c| c.node_id == Some(op.id))
            .expect("operation chunk");
        assert!(chunk.content.contains("query GetUser"));
        assert!(
            !chunk.content.contains("GET_USER ="),
            "chunk body must be the document, not a host slice"
        );
    }

    #[test]
    fn splits_large_chunks_before_embedding() {
        let mut result = ExtractionResult::empty();
        let content = "a".repeat(MAX_CHUNK_CHARS + 100);
        result.chunks.push(KnowledgeChunk {
            id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            file_id: None,
            node_id: None,
            chunk_type: "config".into(),
            content_hash: hash(&content),
            content,
            line_start: Some(1),
            line_end: Some(1),
            metadata: json!({"file": "package-lock.json"}),
        });

        split_large_chunks(&mut result);

        assert!(result.chunks.len() > 1);
        assert!(result
            .chunks
            .iter()
            .all(|chunk| chunk.content.len() <= MAX_CHUNK_CHARS + 256));
        assert!(result
            .chunks
            .iter()
            .all(|chunk| chunk.metadata.get("parent_content_hash").is_some()));
    }

    #[test]
    fn split_prefers_blank_line_boundaries() {
        // Paragraphs of ~70-char lines; the only structural boundaries are the
        // blank lines between them.
        let paragraph = format!("{}\n{}\n", "x".repeat(70), "y".repeat(70));
        let content = vec![paragraph; 40].join("\n");
        let parts = split_content(&content, 1_000);
        assert!(parts.len() > 1);
        for part in &parts[..parts.len() - 1] {
            // Every break lands after a completed paragraph, so no part ends
            // mid-paragraph (its last line is a full paragraph line).
            assert!(!part.content.ends_with('x') || part.content.ends_with(&"x".repeat(70)));
        }
        // Boundary-split parts must not be degenerate slivers.
        assert!(parts
            .iter()
            .take(parts.len() - 1)
            .all(|p| p.content.len() >= 400));
    }

    #[test]
    fn split_prefers_dedent_boundaries_for_code() {
        // Top-level "fn"-like blocks with indented bodies and no blank lines:
        // the dedent back to column 0 is the only structural boundary.
        let block = format!("fn item() {{\n    {}\n}}\n", "b".repeat(80));
        let content = vec![block; 40].join("");
        let parts = split_content(&content, 1_000);
        assert!(parts.len() > 1);
        for part in &parts[..parts.len() - 1] {
            assert!(
                part.content.ends_with('}'),
                "part should end at a block boundary, got: …{:?}",
                &part.content[part.content.len().saturating_sub(20)..]
            );
        }
    }

    #[test]
    fn split_parts_carry_correct_line_ranges() {
        let mut result = ExtractionResult::empty();
        // Header (2 lines incl. blank) + 300 body lines starting at source line 10.
        let body: Vec<String> = (0..300)
            .map(|i| format!("line {i} {}", "z".repeat(40)))
            .collect();
        let content = format!("Documentation file: doc.md\n\n{}", body.join("\n"));
        result.chunks.push(KnowledgeChunk {
            id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            file_id: None,
            node_id: None,
            chunk_type: "documentation".into(),
            content_hash: hash(&content),
            content,
            line_start: Some(10),
            line_end: Some(309),
            metadata: json!({"file": "doc.md"}),
        });

        split_large_chunks(&mut result);

        assert!(result.chunks.len() > 1);
        let first = &result.chunks[0];
        assert_eq!(first.line_start, Some(10));
        let mut prev_end = 0i32;
        for chunk in &result.chunks {
            let start = chunk.line_start.unwrap();
            let end = chunk.line_end.unwrap();
            assert!(
                start >= 10 && end <= 309,
                "range {start}-{end} outside parent"
            );
            assert!(end >= start);
            assert!(
                start > prev_end,
                "parts must advance: {start} after {prev_end}"
            );
            prev_end = end;
        }
        assert_eq!(result.chunks.last().unwrap().line_end, Some(309));
    }

    #[test]
    fn split_falls_back_to_char_cap_for_one_long_line() {
        let parts = split_content(&"a".repeat(5_000), 1_000);
        assert!(parts.len() >= 5);
        assert!(parts.iter().all(|p| p.content.len() <= 1_000));
        assert!(parts.iter().all(|p| p.line_offset == 0));
    }

    #[test]
    fn rust_user_surface_nodes_are_extracted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            r#"
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a repository.
    Analyze { path: String },
}

fn main() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let url2 = std::env::var("DATABASE_URL").unwrap();
    let app = axum::Router::new().route("/health", get(health));
}
"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let analyze = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::CliCommand && n.name == "analyze")
            .expect("clap subcommand node");
        assert_eq!(analyze.metadata["help"], "Index a repository.");
        assert_eq!(analyze.metadata["framework"], "clap");

        // Read twice, one node (per-file dedup by stable id).
        let env_nodes: Vec<_> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::EnvVar && n.name == "DATABASE_URL")
            .collect();
        assert_eq!(env_nodes.len(), 1);

        let route = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::HttpRoute)
            .expect("axum route node");
        assert_eq!(route.name, "GET /health");

        // Env reads attach via Configures, entrypoints via Defines.
        assert!(result
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Configures && e.target_node_id == env_nodes[0].id));
        assert!(result
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Defines && e.target_node_id == route.id));

        // Each surface node carries a chunk typed by its kind.
        assert!(result
            .chunks
            .iter()
            .any(|c| c.chunk_type == "cli_command" && c.node_id == Some(analyze.id)));
        assert!(result
            .chunks
            .iter()
            .any(|c| c.chunk_type == "env_var" && c.node_id == Some(env_nodes[0].id)));
    }

    #[test]
    fn js_user_surface_nodes_are_extracted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("server.ts"),
            r#"
const port = process.env.PORT;
const secret = process.env["API_SECRET"];

app.get('/users/:id', (req, res) => res.json({}));
fastify.post('/orders', handler);

program.command('serve [options]').action(run);

// Client-side calls must NOT register as routes.
axios.get('/not-a-route');
"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let route_names: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::HttpRoute)
            .map(|n| n.name.as_str())
            .collect();
        assert!(route_names.contains(&"GET /users/:id"));
        assert!(route_names.contains(&"POST /orders"));
        assert!(!route_names.iter().any(|n| n.contains("/not-a-route")));

        let env_names: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::EnvVar)
            .map(|n| n.name.as_str())
            .collect();
        assert!(env_names.contains(&"PORT"));
        assert!(env_names.contains(&"API_SECRET"));

        let serve = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::CliCommand)
            .expect("commander command node");
        assert_eq!(serve.name, "serve");
    }

    #[test]
    fn python_user_surface_nodes_are_extracted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("service.py"),
            r#"
import os
import click

token = os.environ["TOKEN"]
port = os.getenv("PORT")
host = os.environ.get("HOST")

@app.get("/items")
async def list_items():
    return []

@app.route("/legacy", methods=["GET", "POST"])
def legacy():
    return ""

@cli.command()
def sync_data():
    pass

def build_cli(subparsers):
    subparsers.add_parser("run", help="Run the worker.")
"#,
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

        let env_names: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::EnvVar)
            .map(|n| n.name.as_str())
            .collect();
        assert!(env_names.contains(&"TOKEN"));
        assert!(env_names.contains(&"PORT"));
        assert!(env_names.contains(&"HOST"));

        let route_names: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::HttpRoute)
            .map(|n| n.name.as_str())
            .collect();
        assert!(route_names.contains(&"GET /items"));
        assert!(route_names.contains(&"GET|POST /legacy"));

        let cli_names: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::CliCommand)
            .map(|n| n.name.as_str())
            .collect();
        // click renames underscores to hyphens; argparse keeps the literal name.
        assert!(cli_names.contains(&"sync-data"));
        assert!(cli_names.contains(&"run"));

        let run = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::CliCommand && n.name == "run")
            .unwrap();
        assert_eq!(run.metadata["help"], "Run the worker.");
    }
}

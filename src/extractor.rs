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
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};
use syn::{Item, ItemImpl};
use uuid::Uuid;

pub struct RustRepositoryExtractor {
    indexing: IndexingConfig,
}

const MAX_CHUNK_CHARS: usize = 2_000;

/// Compile a pattern that is statically known to be valid. Used only for the
/// `LazyLock` regex tables below, so a malformed literal is a programmer error.
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("hard-coded extractor regex must compile")
}

/// Symbol patterns for JavaScript/TypeScript, compiled once and reused for every
/// indexed file instead of being rebuilt on each call.
static JS_TS_SYMBOL_PATTERNS: LazyLock<Vec<(NodeKind, Regex)>> = LazyLock::new(|| {
    vec![
        (
            NodeKind::Function,
            regex(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\("),
        ),
        (
            NodeKind::Function,
            regex(
                r"(?m)^\s*(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?\([^=]*\)\s*=>",
            ),
        ),
        (
            NodeKind::Function,
            regex(
                r"(?m)^\s*(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?[A-Za-z_$][\w$]*\s*=>",
            ),
        ),
        (
            NodeKind::Struct,
            regex(r"(?m)^\s*(?:export\s+)?class\s+([A-Za-z_$][\w$]*)\b"),
        ),
        (
            NodeKind::Trait,
            regex(r"(?m)^\s*(?:export\s+)?interface\s+([A-Za-z_$][\w$]*)\b"),
        ),
        (
            NodeKind::Enum,
            regex(r"(?m)^\s*(?:export\s+)?enum\s+([A-Za-z_$][\w$]*)\b"),
        ),
        (
            NodeKind::TypeAlias,
            regex(r"(?m)^\s*(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\b"),
        ),
    ]
});

impl RustRepositoryExtractor {
    pub fn new(indexing: IndexingConfig) -> Self {
        Self { indexing }
    }

    pub fn extract(
        &self,
        root: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
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
        for path in self.source_paths(root) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if rel.ends_with("Cargo.toml") {
                self.extract_cargo(root, &path, repo_id, repo_node.id, &mut result)?;
                continue;
            }
            if rel.ends_with("package.json") {
                self.extract_package_json(root, &path, repo_id, repo_node.id, &mut result)?;
                continue;
            }
            if rel.ends_with("cdk.json") {
                self.extract_cdk_json(root, &path, repo_id, repo_node.id, &mut result)?;
                continue;
            }
            if rel.ends_with("tsconfig.json") || rel.ends_with("jsconfig.json") {
                self.extract_json_config(root, &path, repo_id, repo_node.id, &mut result)?;
                continue;
            }
            if markdown_language(&path).is_some() {
                self.extract_markdown_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?;
                continue;
            }
            if solidity_language(&path).is_some() {
                self.extract_solidity_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut symbol_names,
                    &mut result,
                )?;
                continue;
            }
            if pdf_language(&path).is_some() {
                self.extract_pdf_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut result,
                )?;
                continue;
            }
            if python_language(&path).is_some() {
                self.extract_python_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut symbol_names,
                    &mut result,
                )?;
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                self.extract_rust_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    &mut symbol_names,
                    &mut result,
                )?;
            }
            if let Some(language) = js_ts_language(&path) {
                self.extract_js_ts_file(
                    root,
                    &path,
                    repo_id,
                    commit_sha.clone(),
                    repo_node.id,
                    language,
                    &mut symbol_names,
                    &mut result,
                )?;
            }
        }

        add_call_edges(&mut result, &symbol_names);
        deduplicate_nodes(&mut result);
        split_large_chunks(&mut result);
        Ok(result)
    }

    fn source_paths(&self, root: &Path) -> Vec<PathBuf> {
        let skip_dirs = self.indexing.skip_dirs.clone();
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false).filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            !skip_dirs.iter().any(|skip| skip == &name)
        });

        builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().map(|ft| ft.is_file()).unwrap_or(false))
            .map(|entry| entry.into_path())
            .filter(|path| {
                path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml")
                    || path.file_name().and_then(|s| s.to_str()) == Some("package.json")
                    || path.file_name().and_then(|s| s.to_str()) == Some("cdk.json")
                    || path.file_name().and_then(|s| s.to_str()) == Some("tsconfig.json")
                    || path.file_name().and_then(|s| s.to_str()) == Some("jsconfig.json")
                    || path.extension().and_then(|s| s.to_str()) == Some("rs")
                    || markdown_language(path).is_some()
                    || solidity_language(path).is_some()
                    || pdf_language(path).is_some()
                    || js_ts_language(path).is_some()
                    || python_language(path).is_some()
            })
            .collect()
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
        let content = fs::read_to_string(path)?;
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
            language: Language::Markdown,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_DOC,
            json!({"source_priority": "supplemental"}),
        ));
        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(file_node.id),
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
        result.nodes.push(file_node);
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
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(file_node.id),
                "pdf_documentation",
                &format!("PDF document: {rel}\n\n{content}"),
                Some(1),
                Some(line_count),
                json!({
                    "kind": "documentation",
                    "file": rel,
                    "source_priority": "supplemental",
                    "format": "pdf",
                    "guidance": "PDF text can add context but source code should be prioritized when they disagree."
                }),
            ));
        }
        result.nodes.push(file_node);
        Ok(())
    }

    fn extract_cargo(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let file = SourceFile {
            id: Uuid::new_v4(),
            repo_id,
            commit_sha: current_commit(root),
            path: rel.clone(),
            language: Language::Rust,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

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
                        file_node.id,
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
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let file = SourceFile {
            id: Uuid::new_v4(),
            repo_id,
            commit_sha: current_commit(root),
            path: rel.clone(),
            language: Language::Json,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap_or(json!({}));
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
                        file_node.id,
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
                        &format!(
                            "File: {rel}\nEcosystem: npm\nDependency: {name}\nSection: {section}\nVersion: {version}\n"
                        ),
                        node.line_start,
                        node.line_end,
                        json!({"ecosystem": "npm", "dependency": name, "section": section, "version": version}),
                    ));
                    result.nodes.push(node);
                }
            }
        }

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
                    file_node.id,
                    node.id,
                    EdgeKind::Defines,
                    weights::DEFINES_SCRIPT,
                    json!({}),
                ));
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(node.id),
                    "script",
                    &format!("File: {rel}\nNPM script: {name}\nCommand: {command}\n"),
                    node.line_start,
                    node.line_end,
                    json!({"ecosystem": "npm", "script": name, "command": command}),
                ));
                result.nodes.push(node);
            }
        }
        Ok(())
    }

    fn extract_json_config(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let file = SourceFile {
            id: Uuid::new_v4(),
            repo_id,
            commit_sha: current_commit(root),
            path: rel.clone(),
            language: Language::Json,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(file_node.id),
            "config",
            &format!("File: {rel}\nJavaScript/TypeScript configuration:\n\n{content}"),
            Some(1),
            Some(file.line_count),
            json!({"config": rel}),
        ));
        result.nodes.push(file_node);
        Ok(())
    }

    fn extract_cdk_json(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        repo_node_id: Uuid,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let file = SourceFile {
            id: Uuid::new_v4(),
            repo_id,
            commit_sha: current_commit(root),
            path: rel.clone(),
            language: Language::Json,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

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
            file_node.id,
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
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
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
            language: Language::Rust,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

        let syntax = syn::parse_file(&content)
            .with_context(|| format!("failed to parse Rust syntax in {rel}"))?;

        for item in syntax.items {
            match item {
                Item::Fn(item) => {
                    let name = item.sig.ident.to_string();
                    let kind = if item.attrs.iter().any(|a| a.path().is_ident("test")) {
                        NodeKind::Test
                    } else {
                        NodeKind::Function
                    };
                    self.add_symbol(
                        repo_id,
                        &file,
                        file_node.id,
                        kind,
                        &name,
                        "fn",
                        &content,
                        symbol_names,
                        result,
                    );
                }
                Item::Struct(item) => self.add_symbol(
                    repo_id,
                    &file,
                    file_node.id,
                    NodeKind::Struct,
                    &item.ident.to_string(),
                    "struct",
                    &content,
                    symbol_names,
                    result,
                ),
                Item::Enum(item) => self.add_symbol(
                    repo_id,
                    &file,
                    file_node.id,
                    NodeKind::Enum,
                    &item.ident.to_string(),
                    "enum",
                    &content,
                    symbol_names,
                    result,
                ),
                Item::Trait(item) => self.add_symbol(
                    repo_id,
                    &file,
                    file_node.id,
                    NodeKind::Trait,
                    &item.ident.to_string(),
                    "trait",
                    &content,
                    symbol_names,
                    result,
                ),
                Item::Mod(item) => self.add_symbol(
                    repo_id,
                    &file,
                    file_node.id,
                    NodeKind::Module,
                    &item.ident.to_string(),
                    "mod",
                    &content,
                    symbol_names,
                    result,
                ),
                Item::Impl(item) => self.add_impl(
                    repo_id,
                    &file,
                    file_node.id,
                    &item,
                    &content,
                    symbol_names,
                    result,
                ),
                Item::Use(item) => {
                    let text = quote_use(&item);
                    let node = KnowledgeNode {
                        id: Uuid::new_v4(),
                        repo_id,
                        file_id: Some(file.id),
                        kind: NodeKind::Concept,
                        stable_id: format!("{}:use:{}", rel, hash(&text)),
                        name: text.clone(),
                        line_start: find_line(&content, &text).map(|v| v as i32),
                        line_end: find_line(&content, &text).map(|v| v as i32),
                        metadata: json!({"import": text}),
                    };
                    result.edges.push(edge(
                        repo_id,
                        file_node.id,
                        node.id,
                        EdgeKind::Imports,
                        weights::IMPORTS_RUST,
                        json!({}),
                    ));
                    result.nodes.push(node);
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn extract_js_ts_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        language: Language,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
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
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

        self.extract_js_ts_imports(repo_id, &file, file_node.id, &content, result)?;
        self.extract_js_ts_symbols(repo_id, &file, file_node.id, &content, symbol_names, result)?;
        self.extract_aws_cdk_knowledge(repo_id, &file, file_node.id, &content, result)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn extract_solidity_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
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
            language: Language::Solidity,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

        self.extract_solidity_imports(repo_id, &file, file_node.id, &content, result)?;
        self.extract_solidity_symbols(
            repo_id,
            &file,
            file_node.id,
            &content,
            symbol_names,
            result,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn extract_python_file(
        &self,
        root: &Path,
        path: &Path,
        repo_id: Uuid,
        commit_sha: Option<String>,
        repo_node_id: Uuid,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
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
            language: Language::Python,
            content: content.clone(),
            content_hash: hash(&content),
            line_count: content.lines().count() as i32,
        };
        result.files.push(file.clone());

        let file_node = file_node(repo_id, &file, &rel);
        result.edges.push(edge(
            repo_id,
            repo_node_id,
            file_node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
            json!({}),
        ));
        result.nodes.push(file_node.clone());

        self.extract_python_imports(repo_id, &file, file_node.id, &content, result)?;
        self.extract_python_symbols(repo_id, &file, file_node.id, &content, symbol_names, result)?;
        Ok(())
    }

    fn extract_python_imports(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        // Matches `import pkg`, `import pkg.sub`, `from pkg import x`, and the
        // relative forms `from . import x` / `from .pkg import y`.
        let import_re = Regex::new(
            r"(?m)^\s*(?:from\s+(\.*[A-Za-z0-9_.]*)\s+import\b|import\s+([A-Za-z0-9_.]+))",
        )?;
        for captures in import_re.captures_iter(content) {
            let module = captures
                .get(1)
                .or_else(|| captures.get(2))
                .map(|v| v.as_str().trim())
                .unwrap_or_default();
            if module.is_empty() {
                continue;
            }
            let line = captures
                .get(0)
                .and_then(|m| find_line(content, m.as_str().trim()))
                .map(|v| v as i32);
            let is_bare = is_bare_module_specifier(module);
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: if is_bare { None } else { Some(file.id) },
                kind: NodeKind::Dependency,
                stable_id: import_stable_id(file, module, is_bare),
                name: module.to_string(),
                line_start: if is_bare { None } else { line },
                line_end: if is_bare { None } else { line },
                metadata: json!({
                    "module": module,
                    "language": "python",
                    "scope": if is_bare { "bare" } else { "relative" }
                }),
            };
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Imports,
                weights::IMPORTS_MODULE,
                json!({"file": file.path, "module": module, "line": line}),
            ));
            result.nodes.push(node);
        }
        Ok(())
    }

    fn extract_python_symbols(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let patterns = [
            (
                NodeKind::Function,
                "function",
                Regex::new(r"(?m)^[ \t]*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")?,
            ),
            (
                NodeKind::Struct,
                "class",
                Regex::new(r"(?m)^[ \t]*class\s+([A-Za-z_][A-Za-z0-9_]*)\b")?,
            ),
        ];

        for (kind, python_kind, pattern) in patterns {
            for captures in pattern.captures_iter(content) {
                let Some(name) = captures.get(1).map(|v| v.as_str()) else {
                    continue;
                };
                let line = captures
                    .get(0)
                    .and_then(|m| find_line(content, m.as_str().trim()))
                    .or_else(|| find_line(content, name))
                    .unwrap_or(1);
                let end = find_python_block_end(content, line).unwrap_or(line);
                let code = slice_lines(content, line, end);
                let chunk_kind = if is_python_test_file(&file.path) || is_test_symbol(name) {
                    NodeKind::Test
                } else {
                    kind.clone()
                };
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id,
                    file_id: Some(file.id),
                    kind: chunk_kind.clone(),
                    stable_id: format!("{}:{}:{}", file.path, chunk_kind.as_str(), name),
                    name: name.to_string(),
                    line_start: Some(line as i32),
                    line_end: Some(end as i32),
                    metadata: json!({
                        "language": "python",
                        "file": file.path,
                        "python_kind": python_kind
                    }),
                };
                symbol_names.entry(name.to_string()).or_insert(node.id);
                result.edges.push(edge(
                    repo_id,
                    file_node_id,
                    node.id,
                    EdgeKind::Contains,
                    weights::CONTAINS_CODE,
                    json!({"language": "python", "kind": python_kind}),
                ));
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(node.id),
                    chunk_kind.as_str(),
                    &format!(
                        "Language: python\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
                        file.path, name, python_kind, line, end, code
                    ),
                    Some(line as i32),
                    Some(end as i32),
                    json!({"symbol": name, "kind": chunk_kind.as_str(), "python_kind": python_kind, "file": file.path}),
                ));
                result.nodes.push(node);
            }
        }
        Ok(())
    }

    fn extract_solidity_imports(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let import_re = Regex::new(r#"(?m)^\s*import\s+(?:[^'"]+\s+from\s+)?['"]([^'"]+)['"]"#)?;
        for captures in import_re.captures_iter(content) {
            let Some(module) = captures.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let line = find_line(content, module).map(|v| v as i32);
            let is_bare = is_bare_module_specifier(module);
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: if is_bare { None } else { Some(file.id) },
                kind: NodeKind::Dependency,
                stable_id: import_stable_id(file, module, is_bare),
                name: module.to_string(),
                line_start: if is_bare { None } else { line },
                line_end: if is_bare { None } else { line },
                metadata: json!({
                    "module": module,
                    "language": "solidity",
                    "scope": if is_bare { "bare" } else { "relative" }
                }),
            };
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Imports,
                weights::IMPORTS_SOLIDITY,
                json!({"file": file.path, "module": module, "line": line}),
            ));
            result.nodes.push(node);
        }
        Ok(())
    }

    fn extract_solidity_symbols(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let type_re = Regex::new(
            r"(?m)^\s*(?:abstract\s+)?(contract|interface|library)\s+([A-Za-z_][A-Za-z0-9_]*)\s*([^{;]*)",
        )?;
        for captures in type_re.captures_iter(content) {
            let kind_text = captures.get(1).map(|v| v.as_str()).unwrap_or("contract");
            let Some(name) = captures.get(2).map(|v| v.as_str()) else {
                continue;
            };
            let suffix = captures.get(3).map(|v| v.as_str()).unwrap_or_default();
            let line = find_line(content, name).unwrap_or(1);
            let end = find_block_end(content, line).unwrap_or(line);
            let code = slice_lines(content, line, end);
            let node_kind = match kind_text {
                "interface" => NodeKind::Trait,
                "library" => NodeKind::Module,
                _ => NodeKind::Struct,
            };
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: Some(file.id),
                kind: node_kind.clone(),
                stable_id: format!("{}:solidity:{}:{}", file.path, kind_text, name),
                name: name.to_string(),
                line_start: Some(line as i32),
                line_end: Some(end as i32),
                metadata: json!({
                    "language": "solidity",
                    "file": file.path,
                    "solidity_kind": kind_text
                }),
            };
            symbol_names.entry(name.to_string()).or_insert(node.id);
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Defines,
                weights::DEFINES_SYMBOL,
                json!({"language": "solidity", "kind": kind_text}),
            ));
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(node.id),
                node_kind.as_str(),
                &format!(
                    "Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
                    file.path, name, kind_text, line, end, code
                ),
                Some(line as i32),
                Some(end as i32),
                json!({"symbol": name, "kind": node_kind.as_str(), "solidity_kind": kind_text, "file": file.path}),
            ));
            add_solidity_inheritance_edges(repo_id, node.id, suffix, result);
            result.nodes.push(node);
        }

        let member_patterns = [
            (
                NodeKind::Function,
                "function",
                Regex::new(r"(?m)^\s*function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")?,
            ),
            (
                NodeKind::Function,
                "constructor",
                Regex::new(r"(?m)^\s*(constructor)\s*\(")?,
            ),
            (
                NodeKind::Function,
                "fallback",
                Regex::new(r"(?m)^\s*(fallback|receive)\s*\(")?,
            ),
            (
                NodeKind::Concept,
                "event",
                Regex::new(r"(?m)^\s*event\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")?,
            ),
            (
                NodeKind::Concept,
                "modifier",
                Regex::new(r"(?m)^\s*modifier\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")?,
            ),
        ];

        for (kind, solidity_kind, pattern) in member_patterns {
            for captures in pattern.captures_iter(content) {
                let Some(name) = captures.get(1).map(|v| v.as_str()) else {
                    continue;
                };
                let line = find_line(content, name).unwrap_or(1);
                let end = find_block_end(content, line).unwrap_or(line);
                let code = slice_lines(content, line, end);
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id,
                    file_id: Some(file.id),
                    kind: kind.clone(),
                    stable_id: format!("{}:solidity:{}:{}", file.path, solidity_kind, name),
                    name: name.to_string(),
                    line_start: Some(line as i32),
                    line_end: Some(end as i32),
                    metadata: json!({
                        "language": "solidity",
                        "file": file.path,
                        "solidity_kind": solidity_kind
                    }),
                };
                symbol_names.entry(name.to_string()).or_insert(node.id);
                result.edges.push(edge(
                    repo_id,
                    file_node_id,
                    node.id,
                    EdgeKind::Contains,
                    weights::CONTAINS_MEMBER,
                    json!({"language": "solidity", "kind": solidity_kind}),
                ));
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(node.id),
                    kind.as_str(),
                    &format!(
                        "Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
                        file.path, name, solidity_kind, line, end, code
                    ),
                    Some(line as i32),
                    Some(end as i32),
                    json!({"symbol": name, "kind": kind.as_str(), "solidity_kind": solidity_kind, "file": file.path}),
                ));
                result.nodes.push(node);
            }
        }
        Ok(())
    }

    fn extract_js_ts_imports(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        let import_re = Regex::new(
            r#"(?m)^\s*(?:import\s+(?:type\s+)?(?:[^'"]+\s+from\s+)?|export\s+[^'"]+\s+from\s+|const\s+\w+\s*=\s*require\()\s*['"]([^'"]+)['"]"#,
        )?;
        for captures in import_re.captures_iter(content) {
            let Some(module) = captures.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let line = find_line(content, module).map(|v| v as i32);
            let is_bare = is_bare_module_specifier(module);
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: if is_bare { None } else { Some(file.id) },
                kind: NodeKind::Dependency,
                stable_id: import_stable_id(file, module, is_bare),
                name: module.to_string(),
                line_start: if is_bare { None } else { line },
                line_end: if is_bare { None } else { line },
                metadata: json!({
                    "module": module,
                    "language": file.language.as_str(),
                    "scope": if is_bare { "bare" } else { "relative" }
                }),
            };
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Imports,
                weights::IMPORTS_MODULE,
                json!({"file": file.path, "module": module, "line": line}),
            ));
            result.nodes.push(node);
        }
        Ok(())
    }

    fn extract_js_ts_symbols(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        for (kind, pattern) in JS_TS_SYMBOL_PATTERNS.iter() {
            for captures in pattern.captures_iter(content) {
                let Some(name) = captures.get(1).map(|v| v.as_str()) else {
                    continue;
                };
                let line = find_line(content, name).unwrap_or(1);
                let end = find_block_end(content, line).unwrap_or(line);
                let code = slice_lines(content, line, end);
                let chunk_kind = if is_js_ts_test_file(&file.path) || is_test_symbol(name) {
                    NodeKind::Test
                } else {
                    kind.clone()
                };
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id,
                    file_id: Some(file.id),
                    kind: chunk_kind.clone(),
                    stable_id: format!("{}:{}:{}", file.path, chunk_kind.as_str(), name),
                    name: name.to_string(),
                    line_start: Some(line as i32),
                    line_end: Some(end as i32),
                    metadata: json!({"language": file.language.as_str(), "file": file.path}),
                };
                symbol_names.entry(name.to_string()).or_insert(node.id);
                result.edges.push(edge(
                    repo_id,
                    file_node_id,
                    node.id,
                    EdgeKind::Contains,
                    weights::CONTAINS_CODE,
                    json!({}),
                ));
                result.chunks.push(chunk_for_node(
                    repo_id,
                    Some(file.id),
                    Some(node.id),
                    chunk_kind.as_str(),
                    &format!(
                        "Language: {}\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
                        file.language.as_str(),
                        file.path,
                        name,
                        chunk_kind.as_str(),
                        line,
                        end,
                        code
                    ),
                    Some(line as i32),
                    Some(end as i32),
                    json!({"symbol": name, "kind": chunk_kind.as_str(), "file": file.path}),
                ));
                result.nodes.push(node);
            }
        }
        Ok(())
    }

    fn extract_aws_cdk_knowledge(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        content: &str,
        result: &mut ExtractionResult,
    ) -> Result<()> {
        if !looks_like_cdk_file(content) {
            return Ok(());
        }

        let stack_re = Regex::new(
            r"(?m)^\s*(?:export\s+)?class\s+([A-Za-z_$][\w$]*)\s+extends\s+(?:[A-Za-z_$][\w$]*\.)?Stack\b",
        )?;
        for captures in stack_re.captures_iter(content) {
            let Some(name) = captures.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let line = find_line(content, name).unwrap_or(1);
            let end = find_block_end(content, line).unwrap_or(line);
            let code = slice_lines(content, line, end);
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: Some(file.id),
                kind: NodeKind::DeploymentResource,
                stable_id: format!("{}:aws-cdk:stack:{name}", file.path),
                name: name.to_string(),
                line_start: Some(line as i32),
                line_end: Some(end as i32),
                metadata: json!({"technology": "aws_cdk", "resource_kind": "stack", "file": file.path}),
            };
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Defines,
                weights::DEFINES_SYMBOL,
                json!({"technology": "aws_cdk"}),
            ));
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(node.id),
                "aws_cdk_stack",
                &format!(
                    "Technology: AWS CDK\nFile: {}\nStack: {}\nLines: {}-{}\n\n{}",
                    file.path, name, line, end, code
                ),
                Some(line as i32),
                Some(end as i32),
                json!({"technology": "aws_cdk", "kind": "stack", "symbol": name, "file": file.path}),
            ));
            result.nodes.push(node);
        }

        let construct_re = Regex::new(
            r#"(?m)\bnew\s+((?:[A-Za-z_$][\w$]*\.)?[A-Z][A-Za-z0-9_$]*)\s*\(\s*this\s*,\s*['"]([^'"]+)['"]"#,
        )?;
        for captures in construct_re.captures_iter(content) {
            let Some(construct_type) = captures.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let Some(logical_id) = captures.get(2).map(|v| v.as_str()) else {
                continue;
            };
            let line = find_line(content, logical_id).unwrap_or(1);
            let end = find_block_end(content, line).unwrap_or(line);
            let code = slice_lines(content, line, end);
            let service = cdk_service(construct_type);
            let node = KnowledgeNode {
                id: Uuid::new_v4(),
                repo_id,
                file_id: Some(file.id),
                kind: NodeKind::DeploymentResource,
                stable_id: format!(
                    "{}:aws-cdk:resource:{}:{}",
                    file.path, construct_type, logical_id
                ),
                name: format!("{construct_type} {logical_id}"),
                line_start: Some(line as i32),
                line_end: Some(end as i32),
                metadata: json!({
                    "technology": "aws_cdk",
                    "resource_kind": "construct",
                    "construct_type": construct_type,
                    "logical_id": logical_id,
                    "service": service,
                    "file": file.path
                }),
            };
            result.edges.push(edge(
                repo_id,
                file_node_id,
                node.id,
                EdgeKind::Configures,
                weights::CONFIGURES,
                json!({"technology": "aws_cdk", "service": service}),
            ));
            result.chunks.push(chunk_for_node(
                repo_id,
                Some(file.id),
                Some(node.id),
                "aws_cdk_resource",
                &format!(
                    "Technology: AWS CDK\nFile: {}\nResource: {}\nLogical ID: {}\nService: {}\nLines: {}-{}\n\n{}",
                    file.path, construct_type, logical_id, service, line, end, code
                ),
                Some(line as i32),
                Some(end as i32),
                json!({
                    "technology": "aws_cdk",
                    "kind": "resource",
                    "construct_type": construct_type,
                    "logical_id": logical_id,
                    "service": service,
                    "file": file.path
                }),
            ));
            result.nodes.push(node);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_impl(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        item: &ItemImpl,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) {
        let name = impl_name(item);
        self.add_symbol(
            repo_id,
            file,
            file_node_id,
            NodeKind::Impl,
            &name,
            "impl",
            content,
            symbol_names,
            result,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_symbol(
        &self,
        repo_id: Uuid,
        file: &SourceFile,
        file_node_id: Uuid,
        kind: NodeKind,
        name: &str,
        keyword: &str,
        content: &str,
        symbol_names: &mut HashMap<String, Uuid>,
        result: &mut ExtractionResult,
    ) {
        let line = find_item_line(content, keyword, name).unwrap_or(1);
        let end = find_block_end(content, line).unwrap_or(line);
        let code = slice_lines(content, line, end);
        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: Some(file.id),
            kind: kind.clone(),
            stable_id: format!("{}:{}:{}", file.path, kind.as_str(), name),
            name: name.to_string(),
            line_start: Some(line as i32),
            line_end: Some(end as i32),
            metadata: json!({"language": "rust", "file": file.path}),
        };
        symbol_names.entry(name.to_string()).or_insert(node.id);
        result.edges.push(edge(
            repo_id,
            file_node_id,
            node.id,
            EdgeKind::Contains,
            weights::CONTAINS_CODE,
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
                line,
                end,
                code
            ),
            Some(line as i32),
            Some(end as i32),
            json!({"symbol": name, "kind": kind.as_str(), "file": file.path}),
        ));
        result.nodes.push(node);
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

fn add_call_edges(result: &mut ExtractionResult, symbols: &HashMap<String, Uuid>) {
    let chunks = result.chunks.clone();
    for chunk in chunks {
        let Some(source) = chunk.node_id else {
            continue;
        };
        for (name, target) in symbols {
            if source != *target && chunk.content.contains(&format!("{name}(")) {
                result.edges.push(edge(
                    chunk.repo_id,
                    source,
                    *target,
                    EdgeKind::Calls,
                    weights::CALLS_HEURISTIC,
                    json!({"detector": "name_call_heuristic", "callee": name}),
                ));
            }
        }
    }
}

fn add_solidity_inheritance_edges(
    repo_id: Uuid,
    contract_node_id: Uuid,
    suffix: &str,
    result: &mut ExtractionResult,
) {
    let suffix = suffix.trim();
    let Some(inherits) = suffix
        .strip_prefix("is ")
        .map(|rest| rest.trim().trim_end_matches(';'))
    else {
        return;
    };

    for base in inherits.split(',') {
        let base = base.split_whitespace().next().unwrap_or_default().trim();
        if base.is_empty() {
            continue;
        }
        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: None,
            kind: NodeKind::Concept,
            stable_id: format!("solidity:inheritance:{base}"),
            name: base.to_string(),
            line_start: None,
            line_end: None,
            metadata: json!({"language": "solidity", "relationship": "inheritance"}),
        };
        result.edges.push(edge(
            repo_id,
            contract_node_id,
            node.id,
            EdgeKind::Implements,
            weights::IMPLEMENTS,
            json!({"language": "solidity", "base": base}),
        ));
        result.nodes.push(node);
    }
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

fn is_js_ts_test_file(path: &str) -> bool {
    path.contains(".test.")
        || path.contains(".spec.")
        || path.contains("__tests__/")
        || path.contains("__test__/")
}

fn is_test_symbol(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test")
        || lower.ends_with("test")
        || lower.starts_with("spec")
        || lower.ends_with("spec")
}

fn is_python_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("_test.py")
        || lower.contains("/test_")
        || lower.starts_with("test_")
        || lower.contains("/tests/")
}

fn looks_like_cdk_file(content: &str) -> bool {
    content.contains("aws-cdk-lib")
        || content.contains("@aws-cdk/")
        || content.contains("constructs")
        || content.contains("extends Stack")
        || content.contains("cdk.")
}

fn cdk_service(construct_type: &str) -> &'static str {
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
fn edge(
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
fn chunk_for_node(
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

pub fn hash(input: &str) -> String {
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

fn split_large_chunks(result: &mut ExtractionResult) {
    let mut chunks = Vec::with_capacity(result.chunks.len());
    for chunk in result.chunks.drain(..) {
        if chunk.content.len() <= MAX_CHUNK_CHARS {
            chunks.push(chunk);
            continue;
        }

        let parts = split_content(&chunk.content, MAX_CHUNK_CHARS);
        let part_count = parts.len();
        for (idx, part) in parts.into_iter().enumerate() {
            let mut metadata = chunk.metadata.clone();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert("split_part".into(), json!(idx + 1));
                obj.insert("split_total".into(), json!(part_count));
                obj.insert("parent_content_hash".into(), json!(chunk.content_hash));
            }
            let content = format!(
                "{}\nChunk part: {}/{}\n\n{}",
                chunk_context_header(&chunk),
                idx + 1,
                part_count,
                part
            );
            chunks.push(KnowledgeChunk {
                id: Uuid::new_v4(),
                repo_id: chunk.repo_id,
                file_id: chunk.file_id,
                node_id: chunk.node_id,
                chunk_type: chunk.chunk_type.clone(),
                content_hash: hash(&content),
                content,
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                metadata,
            });
        }
    }
    result.chunks = chunks;
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

fn split_content(content: &str, max_chars: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        if current.len() + line.len() + 1 > max_chars && !current.is_empty() {
            parts.push(current.trim_end().to_string());
            current.clear();
        }

        if line.len() > max_chars {
            for piece in split_long_line(line, max_chars) {
                if !current.is_empty() {
                    parts.push(current.trim_end().to_string());
                    current.clear();
                }
                parts.push(piece);
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }

    if !current.trim().is_empty() {
        parts.push(current.trim_end().to_string());
    }

    parts
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

fn import_stable_id(file: &SourceFile, module: &str, is_bare: bool) -> String {
    if is_bare {
        format!("import:bare:{module}")
    } else {
        format!("{}:import:{}", file.path, hash(module))
    }
}

fn is_bare_module_specifier(module: &str) -> bool {
    !module.starts_with('.')
        && !module.starts_with('/')
        && !module.starts_with("~/")
        && !module.starts_with("@/")
}

fn quote_use(item: &syn::ItemUse) -> String {
    format!("use {}", use_tree_to_string(&item.tree))
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

fn find_item_line(content: &str, keyword: &str, name: &str) -> Option<usize> {
    let pattern = Regex::new(&format!(
        r"\b{}\s+{}\b",
        regex::escape(keyword),
        regex::escape(name)
    ))
    .ok()?;
    content
        .lines()
        .position(|line| pattern.is_match(line))
        .map(|idx| idx + 1)
        .or_else(|| find_line(content, name))
}

fn find_line(content: &str, needle: &str) -> Option<usize> {
    content
        .lines()
        .position(|line| line.contains(needle))
        .map(|idx| idx + 1)
}

fn find_block_end(content: &str, start_line: usize) -> Option<usize> {
    let mut depth = 0_i32;
    let mut saw_brace = false;
    for (idx, line) in content
        .lines()
        .enumerate()
        .skip(start_line.saturating_sub(1))
    {
        for ch in line.chars() {
            match ch {
                '{' => {
                    saw_brace = true;
                    depth += 1;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if saw_brace && depth <= 0 {
            return Some(idx + 1);
        }
        if !saw_brace && line.trim_end().ends_with(';') {
            return Some(idx + 1);
        }
    }
    Some(start_line)
}

/// Resolve the last line of a Python block (`def`/`class`) using indentation
/// instead of braces: the block continues while subsequent non-blank lines are
/// indented deeper than the header line.
fn find_python_block_end(content: &str, start_line: usize) -> Option<usize> {
    let lines = content.lines().collect::<Vec<_>>();
    if start_line == 0 || start_line > lines.len() {
        return None;
    }
    let header_indent = indent_width(lines[start_line - 1]);
    let mut end = start_line;
    for (idx, line) in lines.iter().enumerate().skip(start_line) {
        if line.trim().is_empty() {
            continue;
        }
        if indent_width(line) <= header_indent {
            break;
        }
        end = idx + 1;
    }
    Some(end)
}

fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum()
}

fn slice_lines(content: &str, start: usize, end: usize) -> String {
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
        assert!(result
            .nodes
            .iter()
            .any(|node| { node.name == "@openzeppelin/contracts/access/Ownable.sol" }));
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
        assert!(result
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Dependency && node.name == "os"));
        assert!(result.nodes.iter().any(|node| node.name == "typing"));
        assert!(result.edges.iter().any(|edge| edge.kind == EdgeKind::Calls));
        assert!(result
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("Language: python")));
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
    fn deduplicates_bare_js_import_targets() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.ts"),
            "import React from \"react\";\nimport helper from \"./helper\";\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.ts"),
            "import React from \"react\";\nimport other from \"./helper\";\n",
        )
        .unwrap();

        let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
        let result = extractor
            .extract(dir.path(), Uuid::new_v4(), Some("test".into()))
            .unwrap();

        let react_nodes = result
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Dependency && node.name == "react")
            .count();
        let helper_nodes = result
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Dependency && node.name == "./helper")
            .count();

        assert_eq!(react_nodes, 1);
        assert_eq!(helper_nodes, 2);
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
}

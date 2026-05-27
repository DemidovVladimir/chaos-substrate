use crate::graph_export::{GraphExport, GraphExportNode};
use anyhow::Result;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub fn write_obsidian_vault(path: &Path, graph: &GraphExport) -> Result<ObsidianSummary> {
    fs::create_dir_all(path)?;
    fs::create_dir_all(path.join("Topics"))?;
    fs::create_dir_all(path.join("Nodes"))?;
    fs::create_dir_all(path.join(".obsidian"))?;

    let topics = build_topics(&graph.nodes);
    let topic_note_by_name = write_topic_notes(path, &topics)?;
    let node_note_by_id = write_node_notes(path, graph, &topic_note_by_name)?;
    write_index(path, graph, &topics)?;
    write_edges_note(path, graph, &node_note_by_id)?;
    write_obsidian_config(path)?;

    Ok(ObsidianSummary {
        output: path.to_path_buf(),
        topics: topics.len(),
        node_notes: node_note_by_id.len(),
        edges: graph.edges.len(),
    })
}

pub struct ObsidianSummary {
    pub output: PathBuf,
    pub topics: usize,
    pub node_notes: usize,
    pub edges: usize,
}

struct Topic {
    name: String,
    nodes: Vec<Uuid>,
}

fn build_topics(nodes: &[GraphExportNode]) -> Vec<Topic> {
    let mut topics: HashMap<String, Vec<Uuid>> = HashMap::new();
    for node in nodes {
        topics.entry(infer_topic(node)).or_default().push(node.id);
    }
    let mut topics = topics
        .into_iter()
        .map(|(name, nodes)| Topic { name, nodes })
        .collect::<Vec<_>>();
    topics.sort_by(|a, b| b.nodes.len().cmp(&a.nodes.len()).then(a.name.cmp(&b.name)));
    topics
}

fn write_topic_notes(path: &Path, topics: &[Topic]) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    for topic in topics {
        let note_name = format!("Topic - {}", safe_filename(&topic.name));
        names.insert(topic.name.clone(), note_name.clone());
        let mut body = String::new();
        body.push_str("---\n");
        body.push_str("kind: topic\n");
        body.push_str(&format!("node_count: {}\n", topic.nodes.len()));
        body.push_str("---\n\n");
        body.push_str(&format!("# {}\n\n", topic.name));
        body.push_str(&format!("Related nodes: {}\n\n", topic.nodes.len()));
        body.push_str(
            "Use Obsidian backlinks and local graph from this note to inspect this topic.\n",
        );
        fs::write(path.join("Topics").join(format!("{note_name}.md")), body)?;
    }
    Ok(names)
}

fn write_node_notes(
    path: &Path,
    graph: &GraphExport,
    topic_note_by_name: &HashMap<String, String>,
) -> Result<HashMap<Uuid, String>> {
    let mut note_by_id = HashMap::new();
    for node in &graph.nodes {
        let note_name = node_note_name(node);
        note_by_id.insert(node.id, note_name);
    }

    let mut incoming: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut outgoing: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for edge in &graph.edges {
        outgoing.entry(edge.source).or_default().push(edge.target);
        incoming.entry(edge.target).or_default().push(edge.source);
    }

    for node in &graph.nodes {
        let note_name = note_by_id.get(&node.id).expect("node note exists");
        let topic = infer_topic(node);
        let topic_link = topic_note_by_name
            .get(&topic)
            .map(|name| format!("[[{name}]]"))
            .unwrap_or_else(|| topic.clone());

        let mut body = String::new();
        body.push_str("---\n");
        body.push_str(&format!("id: {}\n", node.id));
        body.push_str(&format!("kind: {}\n", node.kind));
        body.push_str(&format!("topic: \"{}\"\n", yaml_escape(&topic)));
        if let Some(file) = &node.file_path {
            body.push_str(&format!("file: \"{}\"\n", yaml_escape(file)));
        }
        if let Some(line) = node.line_start {
            body.push_str(&format!("line_start: {line}\n"));
        }
        if let Some(line) = node.line_end {
            body.push_str(&format!("line_end: {line}\n"));
        }
        body.push_str("---\n\n");
        body.push_str(&format!("# {}\n\n", node.name));
        body.push_str(&format!("Kind: `{}`\n\n", node.kind));
        body.push_str(&format!("Topic: {topic_link}\n\n"));
        body.push_str(&format!("Stable ID: `{}`\n\n", node.stable_id));
        if let Some(file) = &node.file_path {
            body.push_str(&format!("File: `{file}`\n\n"));
        }
        if node.line_start.is_some() || node.line_end.is_some() {
            body.push_str(&format!(
                "Lines: `{}`\n\n",
                line_range(node.line_start, node.line_end)
            ));
        }

        push_links("Outgoing", outgoing.get(&node.id), &note_by_id, &mut body);
        push_links("Incoming", incoming.get(&node.id), &note_by_id, &mut body);

        body.push_str("## Metadata\n\n```json\n");
        body.push_str(&serde_json::to_string_pretty(&node.metadata)?);
        body.push_str("\n```\n");

        fs::write(path.join("Nodes").join(format!("{note_name}.md")), body)?;
    }

    Ok(note_by_id)
}

fn write_index(path: &Path, graph: &GraphExport, topics: &[Topic]) -> Result<()> {
    let mut body = String::new();
    body.push_str(&format!("# {} Knowledge Graph\n\n", graph.repository.name));
    body.push_str(&format!("Repository: `{}`\n\n", graph.repository.root_path));
    body.push_str(&format!("Nodes: `{}`\n\n", graph.nodes.len()));
    body.push_str(&format!("Edges: `{}`\n\n", graph.edges.len()));
    body.push_str("## Topics\n\n");
    for topic in topics {
        body.push_str(&format!(
            "- [[Topic - {}]] - {} nodes\n",
            safe_filename(&topic.name),
            topic.nodes.len()
        ));
    }
    fs::write(path.join("README.md"), body)?;
    Ok(())
}

fn write_edges_note(
    path: &Path,
    graph: &GraphExport,
    note_by_id: &HashMap<Uuid, String>,
) -> Result<()> {
    let mut body = String::new();
    body.push_str("# Graph Edges\n\n");
    body.push_str(
        "This note is a compact edge manifest. Open node notes for navigable backlinks.\n\n",
    );
    for edge in &graph.edges {
        let Some(source) = note_by_id.get(&edge.source) else {
            continue;
        };
        let Some(target) = note_by_id.get(&edge.target) else {
            continue;
        };
        body.push_str(&format!("- [[{source}]] --{}--> [[{target}]]\n", edge.kind));
    }
    fs::write(path.join("Edges.md"), body)?;
    Ok(())
}

fn write_obsidian_config(path: &Path) -> Result<()> {
    fs::write(
        path.join(".obsidian").join("app.json"),
        "{\n  \"alwaysUpdateLinks\": true,\n  \"showUnsupportedFiles\": false\n}\n",
    )?;
    fs::write(
        path.join(".obsidian").join("graph.json"),
        "{\n  \"collapse-filter\": false,\n  \"search\": \"-path:Edges\",\n  \"showTags\": true,\n  \"showAttachments\": false,\n  \"hideUnresolved\": true\n}\n",
    )?;
    Ok(())
}

fn push_links(
    heading: &str,
    ids: Option<&Vec<Uuid>>,
    note_by_id: &HashMap<Uuid, String>,
    body: &mut String,
) {
    body.push_str(&format!("## {heading}\n\n"));
    let Some(ids) = ids else {
        body.push_str("_None._\n\n");
        return;
    };
    for id in ids.iter().take(80) {
        if let Some(note) = note_by_id.get(id) {
            body.push_str(&format!("- [[{note}]]\n"));
        }
    }
    if ids.len() > 80 {
        body.push_str(&format!("- ... {} more\n", ids.len() - 80));
    }
    body.push('\n');
}

fn node_note_name(node: &GraphExportNode) -> String {
    let id = node.id.to_string();
    let short_id = &id[..8];
    format!(
        "{} - {} - {}",
        node.kind,
        safe_filename(&node.name),
        short_id
    )
}

fn infer_topic(node: &GraphExportNode) -> String {
    let path = node
        .file_path
        .as_deref()
        .or_else(|| node.metadata.get("file").and_then(|v| v.as_str()))
        .or_else(|| node.stable_id.split(':').next())
        .unwrap_or("");
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return if node.kind == "dependency" {
            "external imports".into()
        } else {
            "workspace".into()
        };
    }
    match parts.as_slice() {
        ["apps" | "packages" | "crates", name, ..] => format!("{}: {name}", parts[0]),
        ["services" | "lambdas" | "lambda", name, ..] => format!("service: {name}"),
        ["docs", area, ..] => format!("docs: {area}"),
        ["docs", ..] => "docs".into(),
        ["src", area, ..] => format!("src: {area}"),
        ["src", ..] => "src".into(),
        ["lib" | "bin", ..] => "library".into(),
        [top, ..] => (*top).into(),
        [] => "workspace".into(),
    }
}

fn safe_filename(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out = out
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(['.', '-', ' '])
        .to_string();
    if out.is_empty() {
        "untitled".into()
    } else {
        out.chars().take(96).collect()
    }
}

fn yaml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn line_range(start: Option<i32>, end: Option<i32>) -> String {
    match (start, end) {
        (Some(start), Some(end)) if start != end => format!("{start}-{end}"),
        (Some(start), _) => start.to_string(),
        _ => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{infer_topic, safe_filename};
    use crate::graph_export::GraphExportNode;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn infers_topics_from_workspace_paths() {
        let first = node("packages/client-sdk/src/index.ts");
        assert_eq!(infer_topic(&first), "packages: client-sdk");

        let second = node("lambda/api/index.ts");
        assert_eq!(infer_topic(&second), "service: api");
    }

    #[test]
    fn sanitizes_note_names() {
        assert_eq!(safe_filename("a/b:c*?"), "a-b-c");
    }

    fn node(file_path: &str) -> GraphExportNode {
        GraphExportNode {
            id: Uuid::new_v4(),
            kind: "function".into(),
            stable_id: file_path.into(),
            name: "thing".into(),
            file_path: Some(file_path.into()),
            line_start: None,
            line_end: None,
            chunk_count: 1,
            metadata: json!({}),
        }
    }
}

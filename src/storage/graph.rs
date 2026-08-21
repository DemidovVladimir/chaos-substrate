//! Whole-graph and node-level reads: L0 exports, exact-name resolution, and
//! reverse-edge consumer lookups.

use super::Storage;
use crate::graph_export::{GraphExport, GraphExportEdge, GraphExportNode, GraphRepository};
use crate::models::{KnowledgeEdge, KnowledgeNode, Repository};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Storage {
    /// Load every node for a repo in canonical `stable_id` order. Used by the
    /// community-detection layer (L1), which must see the whole graph.
    pub async fn load_all_nodes(&self, repo_id: Uuid) -> Result<Vec<KnowledgeNode>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, file_id, kind, stable_id, name, line_start, line_end, metadata
            from nodes
            where repo_id = $1
            order by stable_id, id
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_node).collect())
    }

    /// Load every edge for a repo in a stable order. Used by L1 detection.
    pub async fn load_all_edges(&self, repo_id: Uuid) -> Result<Vec<KnowledgeEdge>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1
            order by source_node_id, target_node_id, kind
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    pub async fn load_edges_for_nodes(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<Vec<KnowledgeEdge>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select id, repo_id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1 and (source_node_id = any($2) or target_node_id = any($2))
            "#,
        )
        .bind(repo_id)
        .bind(node_ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    pub async fn load_graph_export(&self, repo: &Repository) -> Result<GraphExport> {
        let node_rows = sqlx::query(
            r#"
            select n.id, n.kind, n.stable_id, n.name, f.path as file_path,
                   n.line_start, n.line_end, n.metadata, count(c.id)::bigint as chunk_count
            from nodes n
            left join files f on f.id = n.file_id
            left join chunks c on c.node_id = n.id
            where n.repo_id = $1
            group by n.id, n.kind, n.stable_id, n.name, f.path,
                     n.line_start, n.line_end, n.metadata
            order by n.kind, f.path nulls first, n.line_start nulls first, n.name
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;

        let edge_rows = sqlx::query(
            r#"
            select id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1
            order by kind, source_node_id, target_node_id
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;

        Ok(GraphExport {
            repository: GraphRepository {
                id: repo.id,
                name: repo.name.clone(),
                root_path: repo.root_path.clone(),
                current_commit_sha: repo.current_commit_sha.clone(),
            },
            nodes: node_rows
                .into_iter()
                .map(|row| GraphExportNode {
                    id: row.get("id"),
                    kind: row.get("kind"),
                    stable_id: row.get("stable_id"),
                    name: row.get("name"),
                    file_path: row.get("file_path"),
                    line_start: row.get("line_start"),
                    line_end: row.get("line_end"),
                    chunk_count: row.get("chunk_count"),
                    metadata: row.get("metadata"),
                })
                .collect(),
            edges: edge_rows
                .into_iter()
                .map(|row| GraphExportEdge {
                    id: row.get("id"),
                    source: row.get("source_node_id"),
                    target: row.get("target_node_id"),
                    kind: row.get("kind"),
                    cost: row.get("cost"),
                    confidence: row.get("confidence"),
                    metadata: row.get("metadata"),
                })
                .collect(),
        })
    }

    /// Resolve a target symbol / surface string to the graph node(s) named
    /// EXACTLY `name` (case-insensitive), excluding structural repository/file
    /// nodes. The first half of `chaos_usage`: a function name resolves to its
    /// definition (whose consumers come from [`Storage::consumers_of_nodes`]),
    /// while an env-var / route / CLI name resolves to the per-file user-surface
    /// nodes that ARE the use sites. Embedder-free.
    pub async fn nodes_by_name_exact(&self, repo_id: Uuid, name: &str) -> Result<Vec<NodeRef>> {
        let rows = sqlx::query(
            r#"
            select n.id, n.kind, n.name, f.path as file_path, n.line_start
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1
              and lower(n.name) = lower($2)
              and n.kind not in ('repository', 'file')
            order by n.kind, f.path nulls last, n.line_start nulls last
            "#,
        )
        .bind(repo_id)
        .bind(name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| NodeRef {
                id: row.get("id"),
                kind: row.get("kind"),
                name: row.get("name"),
                file: row.get("file_path"),
                line_start: row.get("line_start"),
            })
            .collect())
    }

    /// Every node of one `kind`, projected like [`Storage::nodes_by_name_exact`].
    /// Backs `chaos_usage`'s qualified-suffix fallback: a bare GraphQL field
    /// name (`user`) misses the exact lookup because SDL surface nodes are
    /// qualified (`Query.user`), so the caller fetches the `graphql_field`
    /// nodes and suffix-matches client-side — deterministic and embedder-free.
    pub async fn nodes_by_kind_refs(&self, repo_id: Uuid, kind: &str) -> Result<Vec<NodeRef>> {
        let rows = sqlx::query(
            r#"
            select n.id, n.kind, n.name, f.path as file_path, n.line_start
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1
              and n.kind = $2
            order by n.name, f.path nulls last, n.line_start nulls last
            "#,
        )
        .bind(repo_id)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| NodeRef {
                id: row.get("id"),
                kind: row.get("kind"),
                name: row.get("name"),
                file: row.get("file_path"),
                line_start: row.get("line_start"),
            })
            .collect())
    }

    /// Reverse-edge lookup: every node that REFERENCES one of `target_ids` via an
    /// edge whose kind is in `kinds`, projected to the source (consuming) node's
    /// name, kind, file and top-level subfolder. This is "who consumes node X",
    /// index-backed by `edges_target_node_idx`. An empty `kinds` means all edge
    /// kinds. Embedder-free.
    ///
    /// Caveat surfaced by the caller: `calls`/`imports` edges resolve cross-file
    /// only when the callee name is repo-unique, so consumers of an
    /// ambiguously-named symbol may be undercounted (the literal sweep backstops).
    pub async fn consumers_of_nodes(
        &self,
        repo_id: Uuid,
        target_ids: &[Uuid],
        kinds: &[String],
    ) -> Result<Vec<ConsumerRow>> {
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select e.kind as edge_kind,
                   sn.name as consumer_name,
                   sn.kind as consumer_kind,
                   sf.path as consumer_file,
                   coalesce(nullif(split_part(coalesce(sf.path, ''), '/', 1), ''), '(root)') as top_folder,
                   sn.line_start
            from edges e
            join nodes sn on sn.id = e.source_node_id
            left join files sf on sf.id = sn.file_id
            where e.repo_id = $1
              and e.target_node_id = any($2)
              and ($3::text[] = '{}'::text[] or e.kind = any($3))
            order by top_folder, consumer_file nulls last, sn.line_start nulls last
            "#,
        )
        .bind(repo_id)
        .bind(target_ids)
        .bind(kinds)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ConsumerRow {
                edge_kind: row.get("edge_kind"),
                consumer_name: row.get("consumer_name"),
                consumer_kind: row.get("consumer_kind"),
                consumer_file: row.get("consumer_file"),
                line_start: row.get("line_start"),
            })
            .collect())
    }
}

/// A node resolved by exact name, used by `chaos_usage` to turn a target string
/// into the graph node(s) whose consumers (or own use sites) we then report.
/// Produced by [`Storage::nodes_by_name_exact`] and [`Storage::nodes_by_kind_refs`].
#[derive(Debug, Clone)]
pub struct NodeRef {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub file: Option<String>,
    pub line_start: Option<i32>,
}

/// One reverse-edge consumer: a node that references a target node, projected to
/// its source symbol + file + top-level subfolder. Produced by
/// [`Storage::consumers_of_nodes`].
#[derive(Debug, Clone)]
pub struct ConsumerRow {
    pub edge_kind: String,
    pub consumer_name: String,
    pub consumer_kind: String,
    pub consumer_file: Option<String>,
    pub line_start: Option<i32>,
}

fn row_to_node(row: sqlx::postgres::PgRow) -> KnowledgeNode {
    let kind: String = row.get("kind");
    KnowledgeNode {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        file_id: row.get("file_id"),
        kind: crate::models::NodeKind::from_str(&kind).unwrap_or_else(|| {
            tracing::warn!(kind = %kind, "unknown node kind in database; defaulting to concept");
            crate::models::NodeKind::Concept
        }),
        stable_id: row.get("stable_id"),
        name: row.get("name"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        metadata: row.get("metadata"),
    }
}

fn row_to_edge(row: sqlx::postgres::PgRow) -> KnowledgeEdge {
    let kind: String = row.get("kind");
    KnowledgeEdge {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        source_node_id: row.get("source_node_id"),
        target_node_id: row.get("target_node_id"),
        kind: crate::models::EdgeKind::from_str(&kind).unwrap_or_else(|| {
            tracing::warn!(kind = %kind, "unknown edge kind in database; defaulting to mentions");
            crate::models::EdgeKind::Mentions
        }),
        cost: row.get("cost"),
        confidence: row.get("confidence"),
        metadata: row.get("metadata"),
    }
}

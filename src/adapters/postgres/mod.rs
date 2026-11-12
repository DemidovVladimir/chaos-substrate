//! Postgres + pgvector adapter: the production implementation of the
//! `IndexStore`, `SearchStore` and `CommunityStore` ports.
//!
//! One `sqlx::PgPool`, the migrations in `migrations/`, no ORM.
//! Vectors travel as text literals (`[0.1,0.2,...]`) cast to `::vector`.

mod community;
mod index;
mod search;

use crate::domain::model::{
    Chunk, ChunkKind, Community, Edge, EdgeKind, Language, Node, NodeKind, Repo, SourceFile,
};
use anyhow::{bail, Context, Result};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

/// The migrator for `./migrations`, embedded at compile time.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Tables that only exist in the pre-1.0 (0.x) schema.
const LEGACY_TABLES: &[&str] = &[
    "analysis_runs",
    "cross_repo_links",
    "projects",
    "community_embeddings",
];

/// Postgres-backed store implementing all three persistence ports.
#[derive(Clone)]
pub struct Postgres {
    pool: PgPool,
}

impl Postgres {
    /// Connect (lazily, with a small pool) to `database_url`.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .context("connect to Postgres")?;
        Ok(Self { pool })
    }

    /// Apply `migrations/`. A database holding the legacy 0.x schema (legacy
    /// tables, or `_sqlx_migrations` rows with versions we do not ship) is
    /// refused with a message pointing at `chaos migrate --reset`. With
    /// `reset = true` the whole `public` schema is dropped and recreated first.
    pub async fn migrate(&self, reset: bool) -> Result<()> {
        if reset {
            sqlx::raw_sql("drop schema public cascade; create schema public;")
                .execute(&self.pool)
                .await
                .context("reset schema public")?;
        } else if let Some(reason) = self.legacy_schema_reason().await? {
            bail!("database holds a legacy Chaos schema ({reason}); run `chaos migrate --reset` to rebuild it (a full re-index follows)");
        }
        MIGRATOR
            .run(&self.pool)
            .await
            .context("run database migrations")?;
        Ok(())
    }

    /// Why the database looks like a legacy 0.x schema, if it does (`doctor` uses this too).
    pub async fn legacy_schema_reason(&self) -> Result<Option<String>> {
        for table in LEGACY_TABLES {
            let exists: Option<String> = sqlx::query_scalar("select to_regclass($1)::text")
                .bind(format!("public.{table}"))
                .fetch_one(&self.pool)
                .await?;
            if exists.is_some() {
                return Ok(Some(format!("table {table} exists")));
            }
        }
        let has_history: Option<String> =
            sqlx::query_scalar("select to_regclass('public._sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await?;
        if has_history.is_none() {
            return Ok(None);
        }
        let applied: Vec<i64> = sqlx::query_scalar("select version from _sqlx_migrations")
            .fetch_all(&self.pool)
            .await?;
        let foreign: Vec<i64> = applied
            .into_iter()
            .filter(|v| !MIGRATOR.migrations.iter().any(|m| m.version == *v))
            .collect();
        if foreign.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("unknown migration versions {foreign:?}")))
        }
    }

    /// Versions of the shipped migrations not yet applied to this database.
    pub async fn pending_migrations(&self) -> Result<Vec<i64>> {
        let has_history: Option<String> =
            sqlx::query_scalar("select to_regclass('public._sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await?;
        let applied: Vec<i64> = if has_history.is_some() {
            sqlx::query_scalar("select version from _sqlx_migrations where success")
                .fetch_all(&self.pool)
                .await?
        } else {
            Vec::new()
        };
        Ok(MIGRATOR
            .migrations
            .iter()
            .map(|m| m.version)
            .filter(|v| !applied.contains(v))
            .collect())
    }

    /// Server version plus whether the `vector` extension is installed.
    pub async fn ping(&self) -> Result<String> {
        let version: String = sqlx::query_scalar("select version()")
            .fetch_one(&self.pool)
            .await?;
        let vector: Option<String> =
            sqlx::query_scalar("select extversion from pg_extension where extname = 'vector'")
                .fetch_optional(&self.pool)
                .await?;
        let vector = match vector {
            Some(v) => format!("pgvector {v}"),
            None => "pgvector NOT installed".to_string(),
        };
        Ok(format!("{version}; {vector}"))
    }
}

/// Render a vector as a pgvector text literal; non-finite values become 0.
pub fn vector_literal(vector: &[f32]) -> String {
    let body = vector
        .iter()
        .map(|v| {
            if v.is_finite() {
                v.to_string()
            } else {
                "0".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

fn parse_kind<T>(parse: fn(&str) -> Option<T>, value: &str, what: &str) -> Result<T> {
    parse(value).with_context(|| format!("unknown {what} {value:?} in database"))
}

fn json_strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn row_to_repo(row: &PgRow) -> Repo {
    Repo {
        id: row.get("id"),
        name: row.get("name"),
        root_path: row.get("root_path"),
        commit_sha: row.get("commit_sha"),
        worktree_paths: row
            .get::<Option<serde_json::Value>, _>("worktree_paths")
            .map(|v| json_strings(&v)),
        listing_rules: row.get("listing_rules"),
    }
}

pub(crate) fn row_to_file(row: &PgRow) -> Result<SourceFile> {
    Ok(SourceFile {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        path: row.get("path"),
        language: parse_kind(Language::parse, row.get("language"), "language")?,
        content_hash: row.get("content_hash"),
        line_count: row.get("line_count"),
    })
}

pub(crate) fn row_to_node(row: &PgRow) -> Result<Node> {
    Ok(Node {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        file_id: row.get("file_id"),
        kind: parse_kind(NodeKind::parse, row.get("kind"), "node kind")?,
        stable_id: row.get("stable_id"),
        name: row.get("name"),
        signature: row.get("signature"),
        doc: row.get("doc"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        metadata: row.get("metadata"),
    })
}

pub(crate) fn row_to_edge(row: &PgRow) -> Result<Edge> {
    Ok(Edge {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        source_node_id: row.get("source_node_id"),
        target_node_id: row.get("target_node_id"),
        kind: parse_kind(EdgeKind::parse, row.get("kind"), "edge kind")?,
        confidence: row.get("confidence"),
    })
}

pub(crate) fn row_to_chunk(row: &PgRow) -> Result<Chunk> {
    Ok(Chunk {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        file_id: row.get("file_id"),
        node_id: row.get("node_id"),
        kind: parse_kind(ChunkKind::parse, row.get("kind"), "chunk kind")?,
        header: row.get("header"),
        body: row.get("body"),
        content_hash: row.get("content_hash"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        metadata: row.get("metadata"),
    })
}

pub(crate) fn row_to_community(row: &PgRow) -> Community {
    Community {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        label: row.get("label"),
        member_count: row.get("member_count"),
        summary: row.get("summary"),
        summary_hash: row.get("summary_hash"),
        top_members: json_strings(&row.get::<serde_json::Value, _>("top_members")),
        files: json_strings(&row.get::<serde_json::Value, _>("files")),
    }
}

/// Column list shared by every chunk-returning query (alias `c`).
pub(crate) const CHUNK_COLS: &str =
    "c.id, c.repo_id, c.file_id, c.node_id, c.kind, c.header, c.body, c.content_hash, c.line_start, c.line_end, c.metadata";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{IndexBundle, ModelRef};
    use crate::ports::community_store::CommunityStore;
    use crate::ports::index_store::IndexStore;
    use crate::ports::search_store::SearchStore;
    use serde_json::json;
    use uuid::Uuid;

    fn model() -> ModelRef {
        ModelRef {
            provider: "test".into(),
            model_id: "scripted".into(),
            dimensions: 3,
        }
    }

    fn node_id(repo: Uuid, stable: &str) -> Uuid {
        Uuid::new_v5(&repo, stable.as_bytes())
    }

    fn chunk(repo: Uuid, file: Uuid, node: Option<Uuid>, header: &str, body: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            repo_id: repo,
            file_id: Some(file),
            node_id: node,
            kind: ChunkKind::Unit,
            header: header.into(),
            body: body.into(),
            content_hash: crate::domain::hash::sha256_hex(&format!("{header}\n{body}")),
            line_start: Some(1),
            line_end: Some(3),
            metadata: json!({}),
        }
    }

    fn bundle(repo: Uuid) -> IndexBundle {
        let f1 = Uuid::new_v5(&repo, b"file:src/a.rs");
        let f2 = Uuid::new_v5(&repo, b"file:src/b.rs");
        let file = |id, path: &str| SourceFile {
            id,
            repo_id: repo,
            path: path.into(),
            language: Language::Rust,
            content_hash: format!("h-{path}"),
            line_count: 3,
        };
        let n = |file, stable: &str, name: &str, kind| Node {
            id: node_id(repo, stable),
            repo_id: repo,
            file_id: Some(file),
            kind,
            stable_id: stable.into(),
            name: name.into(),
            signature: None,
            doc: None,
            line_start: Some(1),
            line_end: Some(3),
            metadata: json!({}),
        };
        let alpha = n(
            f1,
            "rust:src/a.rs:function:alpha",
            "alpha",
            NodeKind::Function,
        );
        let beta = n(
            f2,
            "rust:src/b.rs:function:beta",
            "beta",
            NodeKind::Function,
        );
        let env = n(
            f1,
            "rust:src/a.rs:env_var:CHAOS_PORT",
            "CHAOS_PORT",
            NodeKind::EnvVar,
        );
        let edge = |s: &Node, t: &Node| Edge {
            id: Uuid::new_v4(),
            repo_id: repo,
            source_node_id: s.id,
            target_node_id: t.id,
            kind: EdgeKind::Calls,
            confidence: 1.0,
        };
        let dangling = Edge {
            target_node_id: Uuid::new_v4(),
            ..edge(&alpha, &beta)
        };
        IndexBundle {
            chunks: vec![
                chunk(
                    repo,
                    f1,
                    Some(alpha.id),
                    "file: src/a.rs\nfn alpha",
                    "fn alpha() { zebra_mango() }",
                ),
                chunk(
                    repo,
                    f2,
                    Some(beta.id),
                    "file: src/b.rs\nfn beta",
                    "fn beta() { println!(\"quokka\") }",
                ),
            ],
            edges: vec![edge(&alpha, &beta), dangling],
            nodes: vec![alpha, beta, env],
            files: vec![file(f1, "src/a.rs"), file(f2, "src/b.rs")],
        }
    }

    #[tokio::test]
    async fn round_trip_against_real_postgres() {
        let Ok(url) = std::env::var("CHAOS_TEST_DATABASE_URL") else {
            eprintln!("CHAOS_TEST_DATABASE_URL not set; skipping Postgres round-trip test");
            return;
        };
        let pg = Postgres::connect(&url).await.unwrap();
        pg.migrate(true).await.unwrap();
        assert!(pg.ping().await.unwrap().contains("pgvector"));
        // Re-running migrate on our own schema is fine (no legacy detected).
        pg.migrate(false).await.unwrap();

        let repo = Repo {
            id: Uuid::new_v4(),
            name: "t".into(),
            root_path: "/tmp/chaos-adapter-test".into(),
            commit_sha: None,
            worktree_paths: Some(vec!["stale.rs".into()]),
            listing_rules: Some("v0:old".into()),
        };
        pg.upsert_repo(&repo).await.unwrap();
        assert_eq!(pg.find_repo(&repo.root_path).await.unwrap().unwrap(), repo);
        let dirty = vec!["src/a b.rs".to_string(), "src/é.ts".to_string()];
        pg.set_repo_stamp(repo.id, Some("abc"), &dirty, "v1:rules")
            .await
            .unwrap();
        let stamped = pg.list_repos().await.unwrap().remove(0);
        assert_eq!(stamped.commit_sha.as_deref(), Some("abc"));
        assert_eq!(stamped.worktree_paths, Some(dirty));
        assert_eq!(stamped.listing_rules.as_deref(), Some("v1:rules"));
        // A run marks the stamp incomplete (NULL) while it writes …
        let incomplete = Repo {
            worktree_paths: None,
            ..stamped
        };
        pg.upsert_repo(&incomplete).await.unwrap();
        let read = pg.find_repo(&repo.root_path).await.unwrap().unwrap();
        assert_eq!(read, incomplete);
        assert_eq!(read.commit_sha.as_deref(), Some("abc"), "base kept");
        // … and completes it at the end.
        pg.set_repo_stamp(repo.id, Some("def"), &[], "v1:rules")
            .await
            .unwrap();
        let stamped = pg.find_repo(&repo.root_path).await.unwrap().unwrap();
        assert_eq!(stamped.commit_sha.as_deref(), Some("def"));
        assert_eq!(stamped.worktree_paths, Some(Vec::new()));
        assert!(pg.pending_migrations().await.unwrap().is_empty());
        // A row stamped before migration 003 reads as incomplete, so its next
        // `add` reconciles every file once instead of trusting `[]`.
        let legacy = Uuid::new_v4();
        sqlx::query("insert into repositories (id, name, root_path, commit_sha) values ($1, 'old', '/tmp/chaos-adapter-legacy', 'abc')")
            .bind(legacy)
            .execute(&pg.pool)
            .await
            .unwrap();
        let old = pg
            .find_repo("/tmp/chaos-adapter-legacy")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.commit_sha.as_deref(), Some("abc"));
        assert_eq!((old.worktree_paths, old.listing_rules), (None, None));
        sqlx::query("delete from repositories where id = $1")
            .bind(legacy)
            .execute(&pg.pool)
            .await
            .unwrap();

        let b = bundle(repo.id);
        pg.replace_repo(repo.id, &b).await.unwrap();
        assert_eq!(pg.count_chunks(repo.id).await.unwrap(), 2);
        let mut hashes = pg.file_hashes(repo.id).await.unwrap();
        hashes.sort();
        assert_eq!(
            hashes,
            vec![
                ("src/a.rs".to_string(), "h-src/a.rs".to_string()),
                ("src/b.rs".to_string(), "h-src/b.rs".to_string())
            ]
        );
        let graph = pg.load_graph(repo.id).await.unwrap();
        assert_eq!(
            (graph.files.len(), graph.nodes.len(), graph.edges.len()),
            (2, 3, 1)
        );

        let m = model();
        let missing = pg.chunks_missing_embeddings(repo.id, &m).await.unwrap();
        assert_eq!(missing.len(), 2);
        let alpha_hash = b.chunks[0].content_hash.clone();
        let rows: Vec<_> = missing
            .iter()
            .map(|c| {
                let v = if c.content_hash == alpha_hash {
                    vec![1.0, 0.0, 0.0]
                } else {
                    vec![0.0, 1.0, 0.0]
                };
                (c.id, c.content_hash.clone(), v)
            })
            .collect();
        pg.save_embeddings(&m, &rows).await.unwrap();
        assert!(pg
            .chunks_missing_embeddings(repo.id, &m)
            .await
            .unwrap()
            .is_empty());

        // Same content hashes with fresh chunk ids → embeddings preserved.
        let mut b2 = b.clone();
        for c in &mut b2.chunks {
            c.id = Uuid::new_v4();
        }
        pg.replace_repo(repo.id, &b2).await.unwrap();
        assert!(pg
            .chunks_missing_embeddings(repo.id, &m)
            .await
            .unwrap()
            .is_empty());

        // Search channels.
        let sem = pg
            .semantic(repo.id, &m, &[1.0, 0.0, 0.0], None, 5)
            .await
            .unwrap();
        assert_eq!(sem[0].chunk.content_hash, alpha_hash);
        assert!((sem[0].score - 1.0).abs() < 1e-4 && sem[0].path == "src/a.rs");
        let kw = pg.keyword(repo.id, "quokka", None, 5).await.unwrap();
        assert_eq!(kw.len(), 1);
        assert_eq!(kw[0].path, "src/b.rs");
        let kw_id = pg.keyword(repo.id, "zebraMango", None, 5).await.unwrap();
        assert_eq!(
            kw_id.len(),
            1,
            "identifier splitting should match zebra_mango"
        );
        let lit = pg.literal(repo.id, "ZEBRA_mango", None, 5).await.unwrap();
        assert_eq!(lit.len(), 1);
        assert_eq!(lit[0].score, 0.75);
        let f_ids = vec![b.files[1].id];
        assert!(pg
            .literal(repo.id, "zebra", Some(&f_ids), 5)
            .await
            .unwrap()
            .is_empty());

        // Graph lookups.
        let alpha = b.nodes[0].id;
        assert_eq!(
            pg.edges_for_nodes(repo.id, &[alpha]).await.unwrap().len(),
            1
        );
        assert_eq!(pg.nodes_by_ids(&[alpha]).await.unwrap()[0].name, "alpha");
        assert_eq!(pg.chunks_for_nodes(&[alpha]).await.unwrap().len(), 1);
        assert_eq!(
            pg.surfaces(repo.id, &[NodeKind::EnvVar], None)
                .await
                .unwrap()[0]
                .name,
            "CHAOS_PORT"
        );
        assert_eq!(
            pg.file_paths(&[b.files[0].id]).await.unwrap()[0].1,
            "src/a.rs"
        );

        // GraphQL entry lookups, in a second repo: an SDL field, a TypeScript
        // node with the same qualified name (not SDL), a resolver, a
        // registration and an operation.
        let gq = Repo {
            id: Uuid::new_v4(),
            name: "gq".into(),
            root_path: "/tmp/chaos-adapter-test-gql".into(),
            commit_sha: None,
            worktree_paths: None,
            listing_rules: None,
        };
        pg.upsert_repo(&gq).await.unwrap();
        let gf = Uuid::new_v5(&gq.id, b"file:src/labs.graphql");
        let gn = |stable: &str, name: &str, kind, metadata| Node {
            id: node_id(gq.id, stable),
            repo_id: gq.id,
            file_id: Some(gf),
            kind,
            stable_id: stable.into(),
            name: name.into(),
            signature: None,
            doc: None,
            line_start: Some(1),
            line_end: Some(3),
            metadata,
        };
        let field = gn(
            "graphql:src/labs.graphql:field:Mutation.createLab",
            "createLab",
            NodeKind::Field,
            json!({"language": "graphql", "qualified_name": "Mutation.createLab"}),
        );
        let not_sdl = gn(
            "typescript:src/labs.graphql:field:Mutation.createLab",
            "createLab",
            NodeKind::Field,
            json!({"language": "typescript", "qualified_name": "Mutation.createLab"}),
        );
        let resolver = gn(
            "typescript:src/labs.graphql:resolver:originalHandler::createLab",
            "createLab",
            NodeKind::Resolver,
            json!({"qualified_name": "originalHandler::createLab"}),
        );
        let registrar = gn(
            "typescript:src/labs.graphql:method:Config::getResolvers",
            "getResolvers",
            NodeKind::Method,
            json!({}),
        );
        let op = gn(
            "graphql:src/labs.graphql:operation:CreateLab",
            "CreateLab",
            NodeKind::Operation,
            json!({"root_fields": ["Mutation.createLab", "Query.lab"]}),
        );
        let ge = |s: &Node, t: &Node, kind, confidence| Edge {
            id: Uuid::new_v4(),
            repo_id: gq.id,
            source_node_id: s.id,
            target_node_id: t.id,
            kind,
            confidence,
        };
        let gb = IndexBundle {
            files: vec![SourceFile {
                id: gf,
                repo_id: gq.id,
                path: "src/labs.graphql".into(),
                language: Language::GraphQL,
                content_hash: "h-gql".into(),
                line_count: 3,
            }],
            edges: vec![
                ge(&resolver, &field, EdgeKind::Resolves, 0.9),
                ge(&registrar, &field, EdgeKind::Binds, 1.0),
            ],
            nodes: vec![
                field.clone(),
                not_sdl,
                resolver.clone(),
                registrar,
                op.clone(),
            ],
            chunks: vec![],
        };
        pg.replace_repo(gq.id, &gb).await.unwrap();
        let coords = |c: &[&str]| c.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let fields = pg
            .graphql_fields(gq.id, &coords(&["Mutation.createLab"]))
            .await
            .unwrap();
        assert_eq!(
            fields.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![field.id]
        );
        assert!(pg.graphql_fields(gq.id, &[]).await.unwrap().is_empty());
        assert!(pg
            .graphql_fields(repo.id, &coords(&["Mutation.createLab"]))
            .await
            .unwrap()
            .is_empty());
        let ops = pg
            .operations_selecting(gq.id, &coords(&["Query.lab", "Query.none"]))
            .await
            .unwrap();
        assert_eq!(ops.iter().map(|n| n.id).collect::<Vec<_>>(), vec![op.id]);
        assert!(pg
            .operations_selecting(gq.id, &coords(&["Query.none"]))
            .await
            .unwrap()
            .is_empty());
        let gg = pg.load_graph(gq.id).await.unwrap();
        assert!(gg
            .nodes
            .iter()
            .any(|n| n.id == resolver.id && n.kind == NodeKind::Resolver));
        let mut kinds: Vec<(EdgeKind, f32)> =
            gg.edges.iter().map(|e| (e.kind, e.confidence)).collect();
        kinds.sort_by_key(|(k, _)| k.as_str());
        assert_eq!(
            kinds,
            vec![(EdgeKind::Binds, 1.0), (EdgeKind::Resolves, 0.9)]
        );

        // Communities: a summary embedding is kept under its summary_hash.
        let cid = Uuid::new_v5(&repo.id, b"community:0");
        let comm = Community {
            id: cid,
            repo_id: repo.id,
            label: "src".into(),
            member_count: 2,
            summary: Some("does alpha things".into()),
            summary_hash: Some("h1".into()),
            top_members: vec!["alpha".into()],
            files: vec!["src/a.rs".into()],
        };
        pg.replace_communities(
            repo.id,
            std::slice::from_ref(&comm),
            &[(cid, alpha), (cid, b.nodes[1].id)],
        )
        .await
        .unwrap();
        assert_eq!(
            pg.communities_needing_summary(repo.id, &m)
                .await
                .unwrap()
                .len(),
            1
        );
        pg.save_summary_embedding(cid, &m, &[0.0, 1.0, 0.0])
            .await
            .unwrap();
        assert!(pg
            .communities_needing_summary(repo.id, &m)
            .await
            .unwrap()
            .is_empty());
        let other = ModelRef {
            model_id: "other".into(),
            ..m.clone()
        };
        assert_eq!(
            pg.communities_needing_summary(repo.id, &other)
                .await
                .unwrap()
                .len(),
            1
        );
        pg.replace_communities(repo.id, std::slice::from_ref(&comm), &[(cid, alpha)])
            .await
            .unwrap();
        let loaded = pg.load_communities(repo.id).await.unwrap();
        assert_eq!(loaded[0].summary.as_deref(), Some("does alpha things"));
        assert_eq!(loaded[0].summary_hash.as_deref(), Some("h1"));
        assert!(pg
            .communities_needing_summary(repo.id, &m)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(loaded[0].top_members, vec!["alpha".to_string()]);
        assert_eq!(pg.load_members(repo.id).await.unwrap(), vec![(cid, alpha)]);
        let ranked = pg
            .communities_by_embedding(repo.id, &m, &[0.0, 1.0, 0.0], 5)
            .await
            .unwrap();
        assert_eq!(ranked[0].0.id, cid);
        assert!((ranked[0].1 - 1.0).abs() < 1e-4);
        assert_eq!(
            pg.community_file_ids(&[cid]).await.unwrap(),
            vec![b.files[0].id]
        );
        // Same summary under a new id keeps its vector; a changed summary loses it.
        let moved = Community {
            id: Uuid::new_v5(&repo.id, b"community:1"),
            ..comm.clone()
        };
        pg.replace_communities(repo.id, std::slice::from_ref(&moved), &[(moved.id, alpha)])
            .await
            .unwrap();
        assert!(pg
            .communities_needing_summary(repo.id, &m)
            .await
            .unwrap()
            .is_empty());
        let changed = Community {
            summary: Some("does other things".into()),
            summary_hash: Some("h2".into()),
            ..comm
        };
        pg.replace_communities(repo.id, &[changed], &[(cid, alpha)])
            .await
            .unwrap();
        assert_eq!(
            pg.communities_needing_summary(repo.id, &m)
                .await
                .unwrap()
                .len(),
            1
        );

        // `add` of b.rs alone: the edge alpha → beta comes from a.rs, which is
        // not re-analyzed, so the merge must keep it.
        let readd_b = IndexBundle {
            files: vec![b.files[1].clone()],
            nodes: vec![b.nodes[1].clone()],
            chunks: vec![b.chunks[1].clone()],
            ..Default::default()
        };
        pg.merge_files(repo.id, &["src/b.rs".to_string()], &readd_b)
            .await
            .unwrap();
        let g = pg.load_graph(repo.id).await.unwrap();
        assert_eq!(g.edges.len(), 1, "{:?}", g.edges);
        assert_eq!(
            (g.edges[0].source_node_id, g.edges[0].target_node_id),
            (alpha, b.nodes[1].id)
        );

        // Incremental merge: purge b.rs, re-insert a.rs with a new chunk.
        let mut b3 = IndexBundle {
            files: vec![b.files[0].clone()],
            nodes: vec![b.nodes[0].clone()],
            ..Default::default()
        };
        b3.chunks.push(chunk(
            repo.id,
            b.files[0].id,
            Some(alpha),
            "file: src/a.rs\nfn alpha",
            "fn alpha() { changed }",
        ));
        pg.merge_files(repo.id, &["src/b.rs".to_string()], &b3)
            .await
            .unwrap();
        let g = pg.load_graph(repo.id).await.unwrap();
        assert_eq!(g.files.len(), 1);
        assert!(g.nodes.iter().all(|n| n.file_id == Some(b.files[0].id)));
        assert_eq!(pg.count_chunks(repo.id).await.unwrap(), 1);
        assert_eq!(
            pg.chunks_missing_embeddings(repo.id, &m)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(pg
            .keyword(repo.id, "quokka", None, 5)
            .await
            .unwrap()
            .is_empty());

        // ABI kinds round-trip: the store parses `abi` files, nodes and `uses_abi` edges.
        let abi_repo = Repo {
            id: Uuid::new_v4(),
            name: "abi".into(),
            root_path: "/tmp/chaos-adapter-test-abi".into(),
            commit_sha: None,
            worktree_paths: None,
            listing_rules: None,
        };
        pg.upsert_repo(&abi_repo).await.unwrap();
        let mut ab = bundle(abi_repo.id);
        ab.files[1].path = "abis/Token.json".into();
        ab.files[1].language = Language::Abi;
        ab.nodes[1].kind = NodeKind::Abi;
        ab.edges[0].kind = EdgeKind::UsesAbi;
        pg.replace_repo(abi_repo.id, &ab).await.unwrap();
        let g = pg.load_graph(abi_repo.id).await.unwrap();
        assert!(g.files.iter().any(|f| f.language == Language::Abi));
        assert!(g.nodes.iter().any(|n| n.kind == NodeKind::Abi));
        assert_eq!(g.edges[0].kind, EdgeKind::UsesAbi);

        // Legacy schema detection: a 0.x table makes a plain migrate refuse.
        sqlx::raw_sql("create table analysis_runs (id int)")
            .execute(&pg.pool)
            .await
            .unwrap();
        let err = pg.migrate(false).await.unwrap_err().to_string();
        assert!(err.contains("chaos migrate --reset"), "{err}");
        pg.migrate(true).await.unwrap();
        assert_eq!(pg.list_repos().await.unwrap().len(), 0);
    }
}

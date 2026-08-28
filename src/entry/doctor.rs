//! `chaos doctor`: config, Postgres (+ pgvector + schema state), embedder
//! (real embedding of "chaos doctor", dimension check), git. Prints one OK/FAIL
//! line per check; the caller exits non-zero when any check fails.

use crate::adapters::{config::Config, embedding::build_embedder, postgres::Postgres};
use anyhow::Result;
use std::{path::Path, process::Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

pub fn render(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(8);
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!(
            "{:<4} {:<width$}  {}\n",
            if c.ok { "OK" } else { "FAIL" },
            c.name,
            c.detail,
            width = width
        ));
    }
    let failed = checks.iter().filter(|c| !c.ok).count();
    out.push_str(&if failed == 0 {
        format!("all {} checks passed\n", checks.len())
    } else {
        format!("{failed} of {} checks FAILED\n", checks.len())
    });
    out
}

pub async fn checks(config_path: Option<&Path>) -> Vec<Check> {
    let mut out = Vec::new();
    let config = match Config::load(config_path) {
        Ok(c) => {
            out.push(Check::ok(
                "config",
                format!(
                    "{:?} {} ({}d){}",
                    c.embedding.provider,
                    c.embedding.model,
                    c.embedding.dimensions,
                    c.embedding
                        .base_url
                        .as_deref()
                        .map(|u| format!(" @ {u}"))
                        .unwrap_or_default()
                ),
            ));
            c
        }
        Err(e) => {
            out.push(Check::fail("config", format!("{e:#}")));
            out.push(git_check());
            return out;
        }
    };

    match Postgres::connect(&config.storage.database_url).await {
        Ok(store) => {
            match store.ping().await {
                Ok(info) => {
                    let has_vector = !info.contains("NOT installed");
                    out.push(Check::ok("postgres", info.clone()));
                    out.push(if has_vector {
                        Check::ok("pgvector", "extension installed")
                    } else {
                        Check::fail("pgvector", "extension missing — `create extension vector`")
                    });
                }
                Err(e) => out.push(Check::fail("postgres", format!("{e:#}"))),
            }
            let schema = schema_check(&store).await;
            let schema_ok = schema.ok;
            out.push(schema);
            if schema_ok {
                out.push(repos_check(&store).await);
            }
        }
        Err(e) => out.push(Check::fail("postgres", format!("{e:#}"))),
    }

    match build_embedder(&config.embedding) {
        Ok(embedder) => match embedder.embed("chaos doctor").await {
            Ok(v) if v.len() == config.embedding.dimensions => out.push(Check::ok(
                "embedder",
                format!(
                    "{}/{} returned {} dims",
                    embedder.provider(),
                    embedder.model_id(),
                    v.len()
                ),
            )),
            Ok(v) => out.push(Check::fail(
                "embedder",
                format!(
                    "returned {} dims, config says {}",
                    v.len(),
                    config.embedding.dimensions
                ),
            )),
            Err(e) => out.push(Check::fail("embedder", format!("{e:#}"))),
        },
        Err(e) => out.push(Check::fail("embedder", format!("{e:#}"))),
    }

    out.push(git_check());
    out
}

/// Indexed repositories with their chunk counts (only on a current schema).
async fn repos_check(store: &Postgres) -> Check {
    use crate::ports::index_store::IndexStore;
    match store.list_repos().await {
        Ok(repos) if repos.is_empty() => Check::ok("repos", "none indexed yet — run `chaos index`"),
        Ok(repos) => {
            let mut parts = Vec::new();
            for r in &repos {
                let chunks = store.count_chunks(r.id).await.unwrap_or(0);
                parts.push(format!("{} ({chunks} chunks)", r.root_path));
            }
            Check::ok("repos", parts.join("; "))
        }
        Err(e) => Check::fail("repos", format!("{e:#}")),
    }
}

async fn schema_check(store: &Postgres) -> Check {
    match store.legacy_schema_reason().await {
        Ok(Some(reason)) => {
            return Check::fail(
                "schema",
                format!("legacy 0.x schema detected ({reason}) — run `chaos migrate --reset`"),
            )
        }
        Ok(None) => {}
        Err(e) => return Check::fail("schema", format!("{e:#}")),
    }
    let applied: Result<Option<String>, sqlx::Error> =
        sqlx::query_scalar("select to_regclass('public.repositories')::text")
            .fetch_one(store.pool())
            .await;
    match applied {
        Ok(Some(_)) => Check::ok("schema", "migrations applied"),
        Ok(None) => Check::fail("schema", "migrations not applied — run `chaos migrate`"),
        Err(e) => Check::fail("schema", e.to_string()),
    }
}

fn git_check() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => {
            Check::ok("git", String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        Ok(o) => Check::fail("git", String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Check::fail("git", format!("git not found: {e}")),
    }
}

/// Run every check, print the table; `Ok(true)` when all passed.
pub async fn run(config_path: Option<&Path>) -> Result<bool> {
    let checks = checks(config_path).await;
    print!("{}", render(&checks));
    Ok(checks.iter().all(|c| c.ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_marks_failures() {
        let text = render(&[Check::ok("config", "fine"), Check::fail("embedder", "down")]);
        assert!(text.contains("OK   config"));
        assert!(text.contains("FAIL embedder  down"));
        assert!(text.contains("1 of 2 checks FAILED"));
    }
}

//! Embeddings over the OpenAI-compatible `POST {base_url}/embeddings` API,
//! which Ollama (`http://localhost:11434/v1`), OpenRouter and OpenAI all
//! serve. Model-specific prompt formats (EmbeddingGemma's `task: … | query:`)
//! are applied here, and are part of the embedder id.

use crate::ports::Embedder;
use anyhow::Result;
use serde_json::Value;

/// How queries and passages are worded before embedding. The wording shapes
/// the vectors, so the format is part of the embedder id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// EmbeddingGemma's published retrieval prompts.
    GemmaRetrieval,
    /// Text as is, with the context line before a passage.
    Plain,
}

impl Format {
    fn for_model(model: &str) -> Format {
        let _ = model;
        todo!()
    }

    fn name(self) -> &'static str {
        todo!()
    }

    fn query(self, query: &str) -> String {
        let _ = query;
        todo!()
    }

    fn document(self, context: &str, text: &str) -> String {
        let _ = (context, text);
        todo!()
    }
}

fn endpoint(base_url: &str) -> String {
    let _ = base_url;
    todo!()
}

fn request_body(model: &str, inputs: &[String]) -> Value {
    let _ = (model, inputs);
    todo!()
}

fn parse_response(body: &str, expected: usize) -> Result<Vec<Vec<f32>>> {
    let _ = (body, expected);
    todo!()
}

fn status_error(status: u16, body: &str, api_key: Option<&str>) -> anyhow::Error {
    let _ = (status, body, api_key);
    todo!()
}

pub struct HttpEmbedder {
    id: String,
}

impl HttpEmbedder {
    /// `base_url` like `http://localhost:11434/v1`; `api_key` is sent as a
    /// bearer token when present. Nothing is contacted until the first embed.
    pub fn new(base_url: &str, model: &str, api_key: Option<String>) -> HttpEmbedder {
        let _ = (base_url, model, api_key);
        todo!()
    }
}

impl Embedder for HttpEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed_documents(&self, passages: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        let _ = passages;
        todo!()
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let _ = query;
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::vector::{dot, unit};
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    const GEMMA: Format = Format::GemmaRetrieval;
    const PLAIN: Format = Format::Plain;
    const KEY: &str = "sk-test-0123456789abcdef";

    #[test]
    fn request_body_is_model_and_input_array() {
        let inputs = ["first".to_string(), "second one".to_string()];
        assert_eq!(
            request_body("embeddinggemma", &inputs),
            json!({"model": "embeddinggemma", "input": ["first", "second one"]})
        );
    }

    #[test]
    fn endpoint_joins_without_double_slash() {
        for base in ["http://localhost:11434/v1", "http://localhost:11434/v1/"] {
            assert_eq!(endpoint(base), "http://localhost:11434/v1/embeddings");
        }
    }

    #[test]
    fn format_follows_the_model_name() {
        for model in [
            "embeddinggemma",
            "embeddinggemma:300m",
            "google/embeddinggemma-300m",
        ] {
            assert_eq!(Format::for_model(model), GEMMA, "{model}");
        }
        for model in ["nomic-embed-text", "openai/text-embedding-3-small"] {
            assert_eq!(Format::for_model(model), PLAIN, "{model}");
        }
    }

    #[test]
    fn gemma_uses_the_published_retrieval_prompts() {
        assert_eq!(
            GEMMA.query("when is the oil due"),
            "task: search result | query: when is the oil due"
        );
        assert_eq!(
            GEMMA.document("Car > Oil", "Every 10,000 km."),
            "title: Car > Oil | text: Every 10,000 km."
        );
        assert_eq!(
            GEMMA.document("", "Every 10,000 km."),
            "title: none | text: Every 10,000 km."
        );
    }

    #[test]
    fn plain_puts_the_context_line_before_the_text() {
        assert_eq!(PLAIN.query("when is the oil due"), "when is the oil due");
        assert_eq!(
            PLAIN.document("Car > Oil", "Every 10,000 km."),
            "Car > Oil\n\nEvery 10,000 km."
        );
        assert_eq!(PLAIN.document("", "Every 10,000 km."), "Every 10,000 km.");
    }

    #[test]
    fn id_names_model_and_prompt_format() {
        let gemma = HttpEmbedder::new("http://localhost:11434/v1", "embeddinggemma", None);
        assert_eq!(gemma.id(), "embeddinggemma|gemma-retrieval-v1");
        let plain = HttpEmbedder::new(
            "https://openrouter.ai/api/v1",
            "openai/text-embedding-3-small",
            Some(KEY.into()),
        );
        assert_eq!(plain.id(), "openai/text-embedding-3-small|plain-v1");
    }

    /// An embeddings response holding `(index, embedding)` items in this order.
    fn response(items: &[(usize, &[f32])]) -> String {
        let data: Vec<Value> = items
            .iter()
            .map(|(i, v)| json!({"object": "embedding", "index": i, "embedding": v}))
            .collect();
        json!({"object": "list", "model": "m", "data": data}).to_string()
    }

    #[test]
    fn parse_orders_vectors_by_index() {
        let body = response(&[(2, &[0.0, 0.5]), (0, &[1.0, 0.0]), (1, &[0.0, 1.0])]);
        assert_eq!(
            parse_response(&body, 3).unwrap(),
            [vec![1.0, 0.0], vec![0.0, 1.0], vec![0.0, 0.5]]
        );
    }

    #[test]
    fn parse_rejects_a_count_mismatch() {
        let body = response(&[(0, &[1.0])]);
        assert!(parse_response(&body, 2).is_err());
        assert!(parse_response(&body, 0).is_err());
    }

    #[test]
    fn parse_rejects_repeated_or_missing_indexes() {
        assert!(parse_response(&response(&[(0, &[1.0]), (0, &[2.0])]), 2).is_err());
        assert!(parse_response(&response(&[(0, &[1.0]), (2, &[2.0])]), 2).is_err());
    }

    #[test]
    fn parse_rejects_ragged_dimensions() {
        let body = response(&[(0, &[1.0, 0.0]), (1, &[1.0])]);
        assert!(parse_response(&body, 2).is_err());
    }

    #[test]
    fn parse_rejects_an_empty_vector() {
        assert!(parse_response(&response(&[(0, &[])]), 1).is_err());
        assert!(parse_response(&response(&[(0, &[]), (1, &[])]), 2).is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        for body in [
            "",
            "<html>502 Bad Gateway</html>",
            r#"{"error": "model not found"}"#,
            r#"{"data": 5}"#,
            r#"{"data": [{"embedding": [1.0]}]}"#,
            r#"{"data": [{"index": 0, "embedding": [1.0, null]}]}"#,
        ] {
            assert!(parse_response(body, 1).is_err(), "{body}");
        }
    }

    #[test]
    fn status_error_quotes_status_and_at_most_200_chars_of_body() {
        let msg = status_error(503, &"ж".repeat(500), None).to_string();
        assert!(msg.contains("503"), "{msg}");
        assert_eq!(msg.matches('ж').count(), 200, "{msg}");
        let short = status_error(404, "model \"nope\" not found", None).to_string();
        assert!(short.contains("404") && short.contains("model \"nope\" not found"));
    }

    #[test]
    fn status_error_never_includes_the_key() {
        let echoed = format!(r#"{{"error": "invalid api key {KEY}"}}"#);
        let at_the_cut = format!("{}{KEY}", "a".repeat(190));
        for body in [echoed, at_the_cut] {
            let msg = status_error(401, &body, Some(KEY)).to_string();
            assert!(msg.contains("401"), "{msg}");
            assert!(!msg.contains(KEY) && !msg.contains("sk-test"), "{msg}");
        }
    }

    #[test]
    fn no_documents_means_no_request() {
        // A port nobody listens on: any request would fail to connect.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let embedder = HttpEmbedder::new(&format!("http://{port}/v1"), "embeddinggemma", None);
        assert!(embedder.embed_documents(&[]).unwrap().is_empty());
    }

    /// Answer one HTTP request on a loopback port with `status` and `body`.
    /// Returns the base URL and the request as received: (head, payload).
    fn serve_once(status: u16, body: &'static str) -> (String, JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1/", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(listener.accept().unwrap().0);
            let mut head = String::new();
            while !head.ends_with("\r\n\r\n") && reader.read_line(&mut head).unwrap() > 0 {}
            let length = head
                .lines()
                .find_map(|l| {
                    let l = l.to_ascii_lowercase();
                    l.strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap_or(0);
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).unwrap();
            let reply = format!(
                "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            (head, String::from_utf8(payload).unwrap())
        });
        (base, server)
    }

    #[test]
    fn sends_the_bearer_key_but_never_reports_it() {
        let (base, server) = serve_once(
            401,
            r#"{"error": "invalid api key sk-test-0123456789abcdef"}"#,
        );
        let embedder = HttpEmbedder::new(&base, "text-embedding-3-small", Some(KEY.into()));
        let err = format!("{:#}", embedder.embed_query("oil").unwrap_err());
        let (head, payload) = server.join().unwrap();
        assert!(head.starts_with("POST /v1/embeddings "), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains(&format!("authorization: bearer {KEY}")),
            "{head}"
        );
        let sent: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            sent,
            json!({"model": "text-embedding-3-small", "input": ["oil"]})
        );
        assert!(err.contains("401"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    #[test]
    fn sends_no_authorization_without_a_key() {
        let (base, server) = serve_once(500, "model failed to load");
        let embedder = HttpEmbedder::new(&base, "embeddinggemma", None);
        let err = format!("{:#}", embedder.embed_documents(&[("", "x")]).unwrap_err());
        let (head, _) = server.join().unwrap();
        assert!(
            !head.to_ascii_lowercase().contains("authorization:"),
            "{head}"
        );
        assert!(
            err.contains("500") && err.contains("model failed to load"),
            "{err}"
        );
    }

    /// Real semantics against the local Ollama; skipped when it is not running.
    #[test]
    fn live_embeddinggemma_ranks_by_meaning() {
        let ollama = "127.0.0.1:11434".parse().unwrap();
        if TcpStream::connect_timeout(&ollama, Duration::from_secs(1)).is_err() {
            eprintln!("Ollama not reachable on localhost:11434; skipping live embedder test");
            return;
        }
        let embedder = HttpEmbedder::new("http://localhost:11434/v1", "embeddinggemma", None);
        let docs = embedder
            .embed_documents(&[
                (
                    "Car > Maintenance",
                    "Change the engine oil and the oil filter every 10,000 km.",
                ),
                (
                    "",
                    "I graduated with a bachelor's degree in physics in 2012.",
                ),
            ])
            .unwrap();
        let query = embedder
            .embed_query("how often do I change the engine oil")
            .unwrap();
        assert_eq!(query.len(), 768);
        assert!(docs.iter().all(|d| d.len() == 768));
        let query = unit(query).unwrap();
        let score = |doc: &Vec<f32>| dot(&query, &unit(doc.clone()).unwrap());
        let (oil, physics) = (score(&docs[0]), score(&docs[1]));
        eprintln!("live embeddinggemma cosine: oil {oil:.4}, physics {physics:.4}");
        assert!(oil > physics, "oil {oil} <= physics {physics}");
    }
}

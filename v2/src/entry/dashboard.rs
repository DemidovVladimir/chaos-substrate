//! The dashboard: one web page served over HTTP on 127.0.0.1. `GET /` is the
//! page, `GET /api/overview` what the knowledge base holds, and
//! `POST /api/tools/kb_<tool>` runs a tool exactly as an MCP call does (the
//! arguments as a JSON object, parsed by the clap definition). Every request
//! must name this server as its host, which keeps out web pages that point a
//! domain of theirs at 127.0.0.1. The tools also refuse a post from any other
//! page's origin and any body that is not JSON, which a browser can only
//! send to another site after asking, and this server never says yes.

use super::cli::{self, Settings, Tool};
use super::tools;
use crate::domain::presence::DEFAULT_MIN_SCORE;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Where the dashboard listens unless `--port` says otherwise.
pub const DEFAULT_PORT: u16 = 7341;

/// The page: one file with its styles and script, nothing loaded from
/// elsewhere.
const PAGE: &str = include_str!("dashboard.html");

/// What the page may do: run its own inline script and styles and call this
/// server, nothing else (no other site, no frames, no forms).
const POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                      connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; \
                      frame-ancestors 'none'";

/// Largest request line plus headers, and largest body, in bytes.
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

/// How long a connection may stay silent while sending or receiving.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What the router reads from a request.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// The path, without its query.
    pub path: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Serve the dashboard of the configured knowledge base on
/// `127.0.0.1:<port>` until the process is stopped.
pub fn serve(settings: &Settings, port: u16) -> Result<()> {
    // A missing vault fails now, not on the first page load.
    cli::with_kb(settings, |_| Ok(()))?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("listening on 127.0.0.1:{port}"))?;
    let port = listener.local_addr()?.port();
    eprintln!("chaos-kb dashboard: http://127.0.0.1:{port}/ (Ctrl-C stops it)");
    run(listener, settings)
}

/// Answer every connection on its own thread: a browser may keep one
/// connection open and idle while it sends its request on another. Each
/// request opens the knowledge base afresh, as a CLI command does, so the
/// index and vault locks keep them in turn.
fn run(listener: TcpListener, settings: &Settings) -> Result<()> {
    let port = listener.local_addr()?.port();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let settings = settings.clone();
        std::thread::spawn(move || answer(stream, port, &settings));
    }
    Ok(())
}

/// Read one request from `stream`, answer it and close the connection.
fn answer(mut stream: TcpStream, port: u16, settings: &Settings) {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let response = match read_request(&mut stream) {
        Ok(request) => respond(&request, port, &|| overview(settings), &|tool| {
            cli::execute(settings, tool)
        }),
        Err(refused) => refused,
    };
    // The browser may have gone; there is no one left to tell.
    let _ = write_response(&mut stream, &response);
}

/// The overview of the configured knowledge base, with its vault folder, the
/// line `check` draws by default, and this build's version.
fn overview(settings: &Settings) -> Result<Value> {
    cli::with_kb(settings, |kb| {
        let mut overview = serde_json::to_value(kb.overview()?)?;
        let vault = settings.vault.as_deref().map(|v| v.display().to_string());
        overview["vault"] = json!(vault);
        overview["min_score"] = json!(DEFAULT_MIN_SCORE);
        overview["version"] = json!(env!("CARGO_PKG_VERSION"));
        Ok(overview)
    })
}

/// Answer one request to the server listening on `port`. `overview` and
/// `run` reach the knowledge base (stand-ins in tests).
pub fn respond(
    request: &Request,
    port: u16,
    overview: &dyn Fn() -> Result<Value>,
    run: &dyn Fn(&Tool) -> Result<Value>,
) -> Response {
    let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host = request.host.as_deref().map(str::to_ascii_lowercase);
    if !host.is_some_and(|h| hosts.contains(&h)) {
        return failure(
            403,
            &format!("this dashboard answers requests to http://127.0.0.1:{port} only"),
        );
    }
    let method = request.method.as_str();
    match request.path.as_str() {
        "/" if method == "GET" => Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE.as_bytes().to_vec(),
        },
        "/api/overview" if method == "GET" => outcome(overview()),
        "/" | "/api/overview" => failure(405, "use GET"),
        path => match path.strip_prefix("/api/tools/") {
            Some(name) if method == "POST" => tool(request, &hosts, name, run),
            Some(_) => failure(405, "use POST with a JSON object of arguments"),
            None => failure(404, "not found"),
        },
    }
}

/// Run the tool `name` with the request's JSON arguments.
fn tool(
    request: &Request,
    hosts: &[String; 2],
    name: &str,
    run: &dyn Fn(&Tool) -> Result<Value>,
) -> Response {
    let ours = |origin: &str| hosts.iter().any(|h| origin == format!("http://{h}"));
    if request.origin.as_deref().is_some_and(|o| !ours(o)) {
        return failure(403, "tools answer this dashboard's own page only");
    }
    let json = request.content_type.as_deref().is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t == "application/json" || t.starts_with("application/json;")
    });
    if !json {
        return failure(415, "send the arguments as application/json");
    }
    let arguments: Value = match serde_json::from_slice(&request.body) {
        Ok(arguments) => arguments,
        Err(e) => return failure(400, &format!("the body is not JSON: {e}")),
    };
    match tools::command(name, &arguments) {
        None => failure(404, &format!("unknown tool: {name}")),
        Some(Err(e)) => failure(400, &format!("{e:#}")),
        Some(Ok(tool)) => outcome(run(&tool)),
    }
}

/// A report as `200`, or its error as `500`.
fn outcome(report: Result<Value>) -> Response {
    match report {
        Ok(report) => Response {
            status: 200,
            content_type: "application/json",
            body: report.to_string().into_bytes(),
        },
        Err(e) => failure(500, &format!("{e:#}")),
    }
}

/// `{"error": message}` with `status`.
fn failure(status: u16, message: &str) -> Response {
    Response {
        status,
        content_type: "application/json",
        body: json!({ "error": message }).to_string().into_bytes(),
    }
}

/// Read one HTTP/1.1 request: the head up to [`MAX_HEAD`], then a body of
/// its `Content-Length` up to [`MAX_BODY`]. A request that cannot be served
/// comes back as the response that says why.
fn read_request(stream: &mut impl Read) -> Result<Request, Response> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (mut request, head, length) = loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|_| failure(400, "the request could not be read"))?;
        if n == 0 {
            return Err(failure(400, "the request ended early"));
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(head)) => {
                let (request, length) = head_of(&parsed)?;
                break (request, head, length);
            }
            Ok(httparse::Status::Partial) if buf.len() <= MAX_HEAD => {}
            Ok(httparse::Status::Partial) => {
                return Err(failure(431, "the request headers are too large"));
            }
            Err(e) => return Err(failure(400, &format!("bad request: {e}"))),
        }
    };
    let mut body = buf.split_off(head);
    while body.len() < length {
        let n = stream
            .read(&mut chunk)
            .map_err(|_| failure(400, "the request body could not be read"))?;
        if n == 0 {
            return Err(failure(400, "the request body ended early"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    request.body = body;
    Ok(request)
}

/// The request's method, path and the headers the router reads, and the
/// length of its body.
fn head_of(parsed: &httparse::Request) -> Result<(Request, usize), Response> {
    let header = |name: &str| {
        let found = parsed
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name));
        found.map(|h| String::from_utf8_lossy(h.value).trim().to_string())
    };
    if header("transfer-encoding").is_some() {
        return Err(failure(501, "send the body with a Content-Length"));
    }
    let length = match header("content-length") {
        None => 0,
        Some(n) => n
            .parse::<usize>()
            .map_err(|_| failure(400, "the Content-Length is not a number"))?,
    };
    if length > MAX_BODY {
        return Err(failure(413, "the request body is too large"));
    }
    let target = parsed.path.unwrap_or("/");
    let request = Request {
        method: parsed.method.unwrap_or("").to_string(),
        path: target.split('?').next().unwrap_or("/").to_string(),
        host: header("host"),
        origin: header("origin"),
        content_type: header("content-type"),
        body: Vec::new(),
    };
    Ok((request, length))
}

/// Write `response` with the headers every answer carries.
fn write_response(out: &mut impl Write, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nContent-Security-Policy: {POLICY}\r\n\
         Connection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(&response.body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::path::PathBuf;

    const PORT: u16 = 7341;

    fn untouched_overview() -> Result<Value> {
        panic!("must not read the overview")
    }

    fn untouched(tool: &Tool) -> Result<Value> {
        panic!("must not run {tool:?}")
    }

    fn echo(tool: &Tool) -> Result<Value> {
        Ok(json!({ "ran": format!("{tool:?}") }))
    }

    fn get(path: &str) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            host: Some(format!("127.0.0.1:{PORT}")),
            ..Request::default()
        }
    }

    fn post(tool: &str, body: &str) -> Request {
        Request {
            method: "POST".into(),
            path: format!("/api/tools/{tool}"),
            origin: Some(format!("http://127.0.0.1:{PORT}")),
            content_type: Some("application/json".into()),
            body: body.as_bytes().to_vec(),
            ..get("/")
        }
    }

    fn status(request: &Request) -> u16 {
        respond(request, PORT, &untouched_overview, &untouched).status
    }

    fn body(response: &Response) -> Value {
        serde_json::from_slice(&response.body).unwrap()
    }

    #[test]
    fn the_page_is_served_whole_and_loads_nothing_from_elsewhere() {
        let page = respond(&get("/"), PORT, &untouched_overview, &untouched);
        assert_eq!(page.status, 200);
        assert_eq!(page.content_type, "text/html; charset=utf-8");
        let html = String::from_utf8(page.body).unwrap();
        assert!(html.starts_with("<!doctype html>"), "{html}");
        for outside in ["http://", "https://", "//cdn", "src=\"/"] {
            let found = html.match_indices(outside);
            let found = found.map(|(i, _)| html[i..].chars().take(40).collect::<String>());
            let found: Vec<String> = found.filter(|s| !s.contains("www.w3.org")).collect();
            assert!(found.is_empty(), "{outside}: {found:?}");
        }
    }

    #[test]
    fn only_requests_naming_this_server_are_answered() {
        for host in [
            None,
            Some("evil.example"),
            Some("127.0.0.1:80"),
            Some("127.0.0.1"),
        ] {
            let request = Request {
                host: host.map(str::to_string),
                ..get("/")
            };
            assert_eq!(status(&request), 403, "{host:?}");
        }
        let local = Request {
            host: Some(format!("LOCALHOST:{PORT}")),
            ..get("/")
        };
        assert_eq!(status(&local), 200);
    }

    #[test]
    fn the_overview_is_json_and_its_failure_a_500() {
        let fine = || Ok(json!({"synced": true}));
        let done = respond(&get("/api/overview"), PORT, &fine, &untouched);
        assert_eq!((done.status, body(&done)), (200, json!({"synced": true})));
        let broken = || Err(anyhow!("index is locked").context("reading the index"));
        let failed = respond(&get("/api/overview"), PORT, &broken, &untouched);
        assert_eq!(failed.status, 500);
        assert_eq!(
            body(&failed),
            json!({"error": "reading the index: index is locked"})
        );
    }

    #[test]
    fn tools_run_parsed_like_an_mcp_call() {
        let done = respond(
            &post("kb_forget", r#"{"text": "Tea at 5", "note": "Habits"}"#),
            PORT,
            &untouched_overview,
            &echo,
        );
        assert_eq!(done.status, 200);
        let forget = Tool::Forget {
            text: "Tea at 5".into(),
            note: Some("Habits".into()),
        };
        assert_eq!(body(&done), echo(&forget).unwrap());
        let broken = |_: &Tool| Err(anyhow!("the embedder is down"));
        let failed = respond(&post("kb_sync", "{}"), PORT, &untouched_overview, &broken);
        assert_eq!(failed.status, 500);
        assert_eq!(body(&failed), json!({"error": "the embedder is down"}));
    }

    #[test]
    fn bad_calls_say_why() {
        let bad = respond(
            &post("kb_check", "{}"),
            PORT,
            &untouched_overview,
            &untouched,
        );
        assert_eq!(bad.status, 400);
        assert!(body(&bad)["error"].as_str().unwrap().contains("required"));
        assert_eq!(status(&post("kb_check", "{nope")), 400);
        for name in ["kb_nope", "kb_mcp", "kb_dashboard", ""] {
            assert_eq!(status(&post(name, "{}")), 404, "{name}");
        }
        assert_eq!(status(&get("/api/tools/kb_sync")), 405);
        assert_eq!(
            status(&Request {
                method: "POST".into(),
                ..get("/")
            }),
            405
        );
        assert_eq!(status(&get("/favicon.ico")), 404);
    }

    #[test]
    fn tools_refuse_other_pages_and_bodies_that_are_not_json() {
        let from = |origin: &str| Request {
            origin: Some(origin.into()),
            ..post("kb_sync", "{}")
        };
        for origin in ["https://evil.example", "null", "http://127.0.0.1:1"] {
            assert_eq!(status(&from(origin)), 403, "{origin}");
        }
        for kind in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let request = Request {
                content_type: kind.map(str::to_string),
                ..post("kb_sync", "{}")
            };
            assert_eq!(status(&request), 415, "{kind:?}");
        }
        // A local program sends no Origin; it could run the CLI just as well.
        let local = Request {
            origin: None,
            content_type: Some("Application/JSON; charset=utf-8".into()),
            ..post("kb_sync", "{}")
        };
        let done = respond(&local, PORT, &untouched_overview, &echo);
        assert_eq!(done.status, 200);
    }

    /// Hands out its bytes a few at a time, as a socket may.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len()).min(7);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    fn read(raw: &str) -> Result<Request, Response> {
        read_request(&mut Trickle(raw.as_bytes()))
    }

    #[test]
    fn a_request_is_read_with_its_body() {
        let raw = "POST /api/tools/kb_check?x=1 HTTP/1.1\r\nHost: 127.0.0.1:7341\r\n\
                   origin: http://127.0.0.1:7341\r\nContent-Type: application/json\r\n\
                   Content-Length: 16\r\n\r\n{\"query\":\"tea\"}\nEXTRA";
        let request = read(raw).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/tools/kb_check");
        assert_eq!(request.host.as_deref(), Some("127.0.0.1:7341"));
        assert_eq!(request.origin.as_deref(), Some("http://127.0.0.1:7341"));
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert_eq!(request.body, b"{\"query\":\"tea\"}\n");
        let get = read("GET / HTTP/1.1\r\nHost: localhost:7341\r\n\r\n").unwrap();
        assert_eq!((get.path.as_str(), get.body.len()), ("/", 0));
    }

    #[test]
    fn requests_that_cannot_be_served_are_refused() {
        let code = |raw: &str| read(raw).unwrap_err().status;
        assert_eq!(code("nonsense\r\n\r\n"), 400);
        assert_eq!(code("GET / HTTP/1.1\r\nHost: x\r\n"), 400, "ends early");
        let chunked = "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(code(chunked), 501);
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(code(&big), 413);
        assert_eq!(code("POST / HTTP/1.1\r\nContent-Length: ten\r\n\r\n"), 400);
        let short = "POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}";
        assert_eq!(code(short), 400);
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(code(&huge), 431);
    }

    #[test]
    fn every_response_carries_the_policy_and_closes() {
        let mut out = Vec::new();
        write_response(&mut out, &failure(404, "not found")).unwrap();
        let text = String::from_utf8(out).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 404 Not Found\r\n"), "{head}");
        for line in [
            "Content-Type: application/json",
            "Content-Length: 21",
            "Cache-Control: no-store",
            "X-Content-Type-Options: nosniff",
            "Connection: close",
            "frame-ancestors 'none'",
        ] {
            assert!(head.contains(line), "{line}: {head}");
        }
        assert_eq!(body, r#"{"error":"not found"}"#);
    }

    #[test]
    fn a_real_browser_round_trip() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("Car.md"), "# Car\n\nOil every 10k.\n").unwrap();
        let settings = Settings {
            vault: Some(PathBuf::from(vault.path())),
            index: None,
            embed_url: "http://127.0.0.1:9/v1".into(),
            embed_model: "none".into(),
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || run(listener, &settings));
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let base = format!("http://127.0.0.1:{port}");

        let mut page = agent.get(format!("{base}/")).call().unwrap();
        assert_eq!(page.status(), 200);
        let csp = page.headers().get("content-security-policy").unwrap();
        assert!(csp.to_str().unwrap().contains("connect-src 'self'"));
        assert!(
            page.body_mut()
                .read_to_string()
                .unwrap()
                .contains("<title>")
        );

        let mut overview = agent.get(format!("{base}/api/overview")).call().unwrap();
        assert_eq!(overview.status(), 200);
        let overview: Value = overview.body_mut().read_json().unwrap();
        assert_eq!(overview["notes"][0]["path"], "Car.md");
        assert_eq!(overview["notes"][0]["state"], "new");
        assert_eq!(overview["synced"], false);
        let shown = vault.path().display().to_string();
        assert_eq!(overview["vault"], shown);

        let evil = agent
            .post(format!("{base}/api/tools/kb_sync"))
            .header("Origin", "https://evil.example")
            .header("Content-Type", "application/json")
            .send("{}")
            .unwrap();
        assert_eq!(evil.status(), 403);
        let dry = agent
            .post(format!("{base}/api/tools/kb_sync"))
            .header("Content-Type", "application/json")
            .send(r#"{"dry_run": true}"#);
        let mut dry = dry.unwrap();
        assert_eq!(dry.status(), 200);
        let report: Value = dry.body_mut().read_json().unwrap();
        assert_eq!(report["added"], json!(["Car.md"]));
        assert_eq!(report["added"], overview["pending"]["added"]);
    }
}

//! Site map: keeps the documentation site in `site/` tied to the code.
//!
//! | Check | Fails when |
//! |---|---|
//! | pages agree | `site/assets/nav.js`, `site/sources.toml` and the `site/*.html` files do not name the same pages, or a page names an undeclared chapter |
//! | sources exist | a path in `sources.toml` does not exist (a folder prefix ends in `/`) |
//! | src covered | a `src/**/*.rs` file is explained by no page and is not listed under `[glue]` |
//! | pages well-formed | a page misses a shared asset, a `<title>` or the `Checked against the code on YYYY-MM-DD.` footer |
//! | links resolve | a relative `href` or `src` points to a file that does not exist |
//! | fonts only | a page or the stylesheet loads anything from outside the site except Google Fonts |
//! | named files exist | a page names a `src/…` or `tests/…` Rust file that does not exist |
//! | full hashes | a page shows a hash or key shorter than 64 hex characters |
//!
//! After a code change, update every `[pages.<slug>]` whose `sources` lists the
//! changed file or one of its parent folders, and bump that page's footer date.
//!
//! `sources.toml` uses a restricted TOML subset read by [`parse_map`]: full-line
//! `#` comments, `[glue]` and `[pages.<slug>]` headers, and one
//! `sources = [ "path", … ]` list per header. No `toml` dependency is needed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The footer every page ends with, before its date.
const FOOTER: &str = "Checked against the code on ";

/// Every page loads these three files.
const ASSETS: [&str; 3] = [
    r#"href="assets/site.css""#,
    r#"src="assets/nav.js""#,
    r#"src="assets/site.js""#,
];

/// The only hosts a page may load anything from.
const FONT_HOSTS: [&str; 2] = ["fonts.googleapis.com", "fonts.gstatic.com"];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn site() -> PathBuf {
    crate_root().join("site")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// ---- sources.toml --------------------------------------------------------

/// `[glue] sources` plus one `[pages.<slug>] sources` list per page.
#[derive(Debug, Default, PartialEq)]
struct Map {
    glue: Vec<String>,
    pages: BTreeMap<String, Vec<String>>,
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Store the list read under `header` (`glue` or a page slug).
fn put(map: &mut Map, header: &str, items: Vec<String>) {
    if header == "glue" {
        map.glue = items;
    } else {
        map.pages.insert(header.to_string(), items);
    }
}

/// Parse the restricted `sources.toml` format; an error names its line.
fn parse_map(text: &str) -> Result<Map, String> {
    let mut map = Map::default();
    let mut seen = BTreeSet::new();
    // A header whose `sources` list has not been read yet.
    let mut header: Option<String> = None;
    // A `sources = [` list that spans lines: its header and text so far.
    let mut open: Option<(String, String)> = None;
    for (i, raw) in text.lines().enumerate() {
        let at = i + 1;
        let line = raw.trim();
        if let Some((h, mut body)) = open.take() {
            if !line.starts_with('#') {
                if let Some(rest) = line.strip_suffix(']') {
                    body.push_str(rest);
                    put(
                        &mut map,
                        &h,
                        items(&body).map_err(|e| format!("line {at}: {e}"))?,
                    );
                    continue;
                }
                body.push_str(line);
                body.push('\n');
            }
            open = Some((h, body));
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if let Some(h) = &header {
                return Err(format!("line {at}: [{h}] has no `sources` list"));
            }
            let key = match name.strip_prefix("pages.") {
                _ if name == "glue" => name.to_string(),
                Some(slug) if is_slug(slug) => slug.to_string(),
                _ => return Err(format!("line {at}: unknown header [{name}]")),
            };
            if !seen.insert(key.clone()) {
                return Err(format!("line {at}: [{name}] appears twice"));
            }
            header = Some(key);
            continue;
        }
        let Some(rest) = line.strip_prefix("sources = [") else {
            return Err(format!("line {at}: expected a [header] or `sources = [`"));
        };
        let Some(h) = header.take() else {
            return Err(format!("line {at}: `sources` without a header of its own"));
        };
        match rest.strip_suffix(']') {
            Some(inline) => put(
                &mut map,
                &h,
                items(inline).map_err(|e| format!("line {at}: {e}"))?,
            ),
            None => open = Some((h, format!("{rest}\n"))),
        }
    }
    if let Some((h, _)) = open {
        return Err(format!("the `sources` list of [{h}] is never closed"));
    }
    if let Some(h) = header {
        return Err(format!("[{h}] has no `sources` list"));
    }
    Ok(map)
}

/// `"a", "b",` → `["a", "b"]`: quoted paths, separated by commas or lines.
fn items(body: &str) -> Result<Vec<String>, String> {
    body.split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            item.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .filter(|s| !s.is_empty() && !s.contains(['"', '\\']))
                .map(str::to_string)
                .ok_or_else(|| format!("{item:?} is not a quoted path"))
        })
        .collect()
}

fn load_map() -> Map {
    let path = site().join("sources.toml");
    parse_map(&read(&path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Does the `sources` entry `source` cover the crate file `file`?
fn covers(source: &str, file: &str) -> bool {
    match source.strip_suffix('/') {
        Some(dir) => file.starts_with(&format!("{dir}/")),
        None => file == source,
    }
}

// ---- nav.js and the page files ------------------------------------------

/// Every `field: "value"` in `text`, in order.
fn fields(text: &str, field: &str) -> Vec<String> {
    let needle = format!("{field}: \"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let end = after.find('"').expect("an unterminated string in nav.js");
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

fn nav_text() -> String {
    read(&site().join("assets/nav.js"))
}

fn nav_slugs() -> Vec<String> {
    fields(&nav_text(), "slug")
}

/// `<slug>` of every `site/<slug>.html` except `index.html`.
fn html_slugs() -> BTreeSet<String> {
    let entries = fs::read_dir(site()).unwrap_or_else(|e| panic!("site/: {e}"));
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".html").map(str::to_string)
        })
        .filter(|slug| slug != "index")
        .collect()
}

/// `(file name, html)` of the index and of every page in `nav.js`.
fn pages() -> Vec<(String, String)> {
    let pages: Vec<(String, String)> = std::iter::once("index".to_string())
        .chain(nav_slugs())
        .filter_map(|slug| {
            let name = format!("{slug}.html");
            let html = fs::read_to_string(site().join(&name)).ok()?;
            Some((name, html))
        })
        .collect();
    assert!(!pages.is_empty(), "no pages found in site/");
    pages
}

// ---- HTML scanning ------------------------------------------------------

/// `html` without the bodies of its `<script>` elements; the tags stay, so
/// their `src` attributes are still seen.
fn without_scripts(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let Some(open_end) = rest[start..].find('>') else {
            break;
        };
        let body = start + open_end + 1;
        out.push_str(&rest[..body]);
        rest = rest[body..]
            .find("</script>")
            .map_or("", |close| &rest[body + close..]);
    }
    out.push_str(rest);
    out
}

/// Values of every `attr="…"` in `html` that follows whitespace.
fn attributes(html: &str, attr: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&needle) {
        let spaced = rest[..i].ends_with(char::is_whitespace);
        let after = &rest[i + needle.len()..];
        let end = after.find('"').unwrap_or(after.len());
        if spaced {
            out.push(after[..end].to_string());
        }
        rest = &after[end..];
    }
    out
}

/// Every opening tag `<name …>` in `html`.
fn tags<'h>(html: &'h str, name: &str) -> Vec<&'h str> {
    let needle = format!("<{name}");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i..];
        let end = after.find('>').map_or(after.len(), |e| e + 1);
        let next = after[needle.len()..].chars().next();
        if next.is_some_and(|c| c.is_whitespace() || c == '>') {
            out.push(&after[..end]);
        }
        rest = &after[end.max(1)..];
    }
    out
}

fn is_external(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("//")
}

/// The host of an absolute URL.
fn host(url: &str) -> &str {
    let rest = url.split_once("//").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// URLs a stylesheet loads: `url(…)` and `@import …;`.
fn css_loads(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (needle, stop) in [("url(", ')'), ("@import ", ';')] {
        let mut rest = css;
        while let Some(i) = rest.find(needle) {
            let after = &rest[i + needle.len()..];
            let end = after.find(stop).unwrap_or(after.len());
            let url = after[..end].trim().trim_matches(['"', '\'']);
            out.push(url.to_string());
            rest = &after[end..];
        }
    }
    out
}

/// The bodies of a page's `<style>` elements.
fn styles(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<style") {
        let Some(open_end) = rest[i..].find('>') else {
            break;
        };
        let body = &rest[i + open_end + 1..];
        let end = body.find("</style>").unwrap_or(body.len());
        out.push(&body[..end]);
        rest = &body[end..];
    }
    out
}

/// True when `date` reads `YYYY-MM-DD` with a plausible month and day.
fn is_date(date: &str) -> bool {
    let digits = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_digit());
    match date.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => {
            let (m, d): (u32, u32) = (m.parse().unwrap(), d.parse().unwrap());
            (1..=12).contains(&m) && (1..=31).contains(&d)
        }
        _ => false,
    }
}

/// Runs of ASCII letters and digits in `text`, each with the text after it.
fn words(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        if c.is_ascii_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((&text[s..i], &text[i..]));
        }
    }
    out
}

// ---- tests ---------------------------------------------------------------

#[test]
fn the_map_format_is_read_strictly() {
    let text = "# comment\n[glue]\nsources = [\"src/a/mod.rs\"]\n\n\
                [pages.big-picture]\nsources = [\n  \"src/main.rs\",\n  # note\n  \"src/x/\",\n]\n";
    let map = parse_map(text).unwrap();
    assert_eq!(map.glue, ["src/a/mod.rs"]);
    assert_eq!(map.pages["big-picture"], ["src/main.rs", "src/x/"]);
    for bad in [
        "[pages.a]\n",
        "[pages.Bad]\nsources = []\n",
        "[other]\nsources = []\n",
        "sources = [\"x\"]\n",
        "[pages.a]\nsources = [x]\n",
        "[pages.a]\nsources = [\"x\"]\nsources = [\"y\"]\n",
        "[pages.a]\nsources = [\"x\"]\n[pages.a]\nsources = [\"y\"]\n",
        "[pages.a]\nsources = [\n\"x\",\n",
        "[pages.a]\nwhat = 1\n",
    ] {
        assert!(parse_map(bad).is_err(), "{bad:?} must be refused");
    }
    assert!(covers("src/x/", "src/x/y.rs") && !covers("src/x/", "src/xy.rs"));
    assert!(covers("src/a.rs", "src/a.rs") && !covers("src/a.rs", "src/a.rss"));
}

#[test]
fn the_scanners_find_what_they_look_for() {
    let html = "<link rel=\"x\" href=\"a.css\"><script src=\"b.js\">var s = ' src=\"c.js\"';</script>\
                <div data-src=\"d\"></div><style>@import \"e.css\"; p { background: url(f.png) }</style>";
    let markup = without_scripts(html);
    assert_eq!(attributes(&markup, "src"), ["b.js"]);
    assert_eq!(attributes(&markup, "href"), ["a.css"]);
    assert_eq!(tags(&markup, "link").len(), 1);
    assert_eq!(css_loads(styles(&markup)[0]), ["f.png", "e.css"]);
    assert_eq!(host("https://fonts.gstatic.com"), "fonts.gstatic.com");
    assert_eq!(host("//cdn.example/x.js"), "cdn.example");
    assert!(is_date("2026-10-24") && !is_date("2026-13-01") && !is_date("26-10-24"));
    let found: Vec<&str> = words("a1, 0x01 é bc").iter().map(|(w, _)| *w).collect();
    assert_eq!(found, ["a1", "0x01", "bc"]);
}

#[test]
fn nav_sources_and_page_files_name_the_same_pages() {
    let map = load_map();
    let slugs = nav_slugs();
    let in_nav: BTreeSet<String> = slugs.iter().cloned().collect();
    assert_eq!(
        slugs.len(),
        in_nav.len(),
        "site/assets/nav.js lists a slug twice"
    );
    let in_map: BTreeSet<String> = map.pages.keys().cloned().collect();
    let in_files = html_slugs();
    assert!(
        in_nav == in_map && in_nav == in_files,
        "the page lists disagree:\n  nav.js:       {in_nav:?}\n  sources.toml: {in_map:?}\n  site/*.html:  {in_files:?}"
    );
    assert!(
        site().join("index.html").is_file(),
        "site/index.html is missing"
    );

    let nav = nav_text();
    let chapters: BTreeSet<String> = fields(&nav, "id").into_iter().collect();
    let used = fields(&nav, "chapter");
    assert_eq!(used.len(), slugs.len(), "every nav.js page needs a chapter");
    let unknown: Vec<&String> = used.iter().filter(|c| !chapters.contains(*c)).collect();
    assert!(
        unknown.is_empty(),
        "nav.js pages name undeclared chapters: {unknown:?}"
    );
}

#[test]
fn every_source_path_exists() {
    let map = load_map();
    let all = map
        .pages
        .iter()
        .flat_map(|(slug, paths)| paths.iter().map(move |p| (slug.as_str(), p)))
        .chain(map.glue.iter().map(|p| ("glue", p)));
    let dead: Vec<String> = all
        .filter(|(_, p)| {
            let path = crate_root().join(p.trim_end_matches('/'));
            if p.ends_with('/') {
                !path.is_dir()
            } else {
                !path.is_file()
            }
        })
        .map(|(slug, p)| format!("[{slug}] {p}"))
        .collect();
    assert!(
        dead.is_empty(),
        "site/sources.toml names paths that do not exist (moved or deleted? fix the path and re-check the page):\n{}",
        dead.join("\n")
    );
}

#[test]
fn every_src_file_is_explained_by_a_page() {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(crate_root()).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let map = load_map();
    let mut files = Vec::new();
    walk(&crate_root().join("src"), &mut files);
    files.sort();
    let unmapped: Vec<&str> = files
        .iter()
        .filter(|f| {
            !map.pages.values().flatten().any(|s| covers(s, f))
                && !map.glue.iter().any(|s| covers(s, f))
        })
        .map(String::as_str)
        .collect();
    assert!(
        unmapped.is_empty(),
        "src files no site page explains: add each to the [pages.<slug>] it belongs to (or to [glue] if it only declares modules) in site/sources.toml, and explain it on that page:\n{}",
        unmapped.join("\n")
    );
}

#[test]
fn pages_are_well_formed() {
    let mut problems = Vec::new();
    for (name, html) in pages() {
        for asset in ASSETS {
            if !html.contains(asset) {
                problems.push(format!("{name}: does not load {asset}"));
            }
        }
        let title = html
            .split_once("<title>")
            .and_then(|(_, rest)| rest.split_once("</title>"))
            .map(|(t, _)| t.trim());
        if title.is_none_or(str::is_empty) {
            problems.push(format!("{name}: no <title>"));
        }
        let stamped = html.split(FOOTER).skip(1).any(|rest| {
            rest.get(..11)
                .is_some_and(|d| d.ends_with('.') && is_date(&d[..10]))
        });
        if !stamped {
            problems.push(format!("{name}: no \"{FOOTER}YYYY-MM-DD.\" footer"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn relative_links_resolve() {
    let mut problems = Vec::new();
    for (name, html) in pages() {
        let markup = without_scripts(&html);
        let urls = attributes(&markup, "href")
            .into_iter()
            .chain(attributes(&markup, "src"));
        for url in urls {
            let skipped = ["#", "mailto:", "data:"];
            if is_external(&url) || skipped.iter().any(|s| url.starts_with(s)) {
                continue;
            }
            let file = url.split(['#', '?']).next().unwrap_or_default();
            if file.is_empty() || !site().join(file).is_file() {
                problems.push(format!("{name}: links {url}, which does not exist"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn only_google_fonts_load_from_outside() {
    let mut problems = Vec::new();
    let allowed = |url: &str| FONT_HOSTS.contains(&host(url));
    for (name, html) in pages() {
        let markup = without_scripts(&html);
        let mut loads: Vec<String> = Vec::new();
        for tag in [
            "script", "img", "iframe", "source", "audio", "video", "embed",
        ] {
            for t in tags(&markup, tag) {
                loads.extend(attributes(t, "src"));
            }
        }
        for t in tags(&markup, "link") {
            loads.extend(attributes(t, "href"));
        }
        for css in styles(&markup) {
            loads.extend(css_loads(css));
        }
        for url in loads.into_iter().filter(|u| is_external(u) && !allowed(u)) {
            problems.push(format!("{name}: loads {url}"));
        }
    }
    let css = read(&site().join("assets/site.css"));
    for url in css_loads(&css).into_iter().filter(|u| is_external(u)) {
        problems.push(format!("assets/site.css: loads {url}"));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn rust_files_a_page_names_exist() {
    let mut problems = Vec::new();
    for (name, html) in pages() {
        for root in ["src/", "tests/"] {
            let mut rest = html.as_str();
            while let Some(i) = rest.find(root) {
                let before = rest[..i].chars().next_back();
                let after = &rest[i..];
                let len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "_/.".contains(c)))
                    .unwrap_or(after.len());
                let path = after[..len].trim_end_matches('.');
                let standalone = before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                if standalone && path.ends_with(".rs") && !crate_root().join(path).is_file() {
                    problems.push(format!("{name}: names {path}, which does not exist"));
                }
                rest = &after[len.max(1)..];
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn hashes_are_never_shortened() {
    let mut problems = Vec::new();
    for (name, html) in pages() {
        let text = without_scripts(&html);
        for (word, after) in words(&text) {
            let hex = word
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
            let mixed = word.chars().any(|c| c.is_ascii_digit())
                && word.chars().any(|c| c.is_ascii_alphabetic());
            let cut = after.starts_with('…') || after.starts_with("...");
            let long = word.len() >= 12 || (cut && word.len() >= 6);
            if hex && mixed && long && word.len() != 64 {
                problems.push(format!("{name}: {word} is not a full 64-character hash"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

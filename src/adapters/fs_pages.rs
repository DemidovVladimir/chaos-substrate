//! Filesystem adapter for [`PageStore`]: renders generated HTML pages from a
//! JSON manifest with ONE light editorial theme.
//!
//! Every page is a single self-contained HTML file: the theme CSS, the manifest
//! embedded as `<script type="application/json" id="chaos-manifest">`, and a
//! small vanilla-JS renderer that reads the manifest and builds the DOM. Use
//! cases supply data only; the shape of each page lives here, keyed by
//! [`PageKind`]. Pages are content-hash gated: writing the same `content_hash`
//! over an existing page is a no-op.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

use crate::ports::pages::{PageKind, PageStore, MANIFEST_ID};

/// Filesystem-backed page writer (stateless; templates are compiled in).
#[derive(Debug, Default, Clone, Copy)]
pub struct FsPages;

/// Escape text for HTML body content and double- or single-quoted attributes.
/// `&` is replaced first so later entities are not re-escaped.
pub fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// Make a JSON document safe inside a `<script>` block: `</` and `<!--` are
/// the only sequences that can terminate or confuse the parser, so `<` is
/// emitted as `\u003c` (valid JSON, identical value after `JSON.parse`).
pub fn escape_script_json(json: &str) -> String {
    json.replace("</", "\\u003c/").replace("<!--", "\\u003c!--")
}

/// The light editorial theme shared by every generated page.
pub const THEME_CSS: &str = r#":root{
  --ink-900:rgb(2,9,21);--ink-700:rgb(15,39,63);--ink-500:rgb(30,50,74);--ink-300:rgb(68,117,154);
  --ink-200:rgb(127,138,152);--border:rgb(213,220,230);--border-soft:rgb(238,242,246);
  --surface-0:#fff;--surface-1:rgb(249,249,249);--surface-2:rgb(244,249,252);--surface-3:rgb(238,242,246);
  --blue-50:rgb(244,248,253);--blue-100:rgb(233,241,250);--blue-300:rgb(193,219,250);
  --blue-600:rgb(68,117,154);--blue-700:rgb(27,88,156);--purple-100:rgb(241,234,255);--purple-500:rgb(112,70,198);
  --ok:rgb(22,140,90);--warn:rgb(186,110,0);
  --font:"Inter",ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
  --mono:ui-monospace,"SF Mono",Menlo,Consolas,monospace;
  --radius:12px;--shadow:0 1px 2px rgba(2,9,21,.06),0 6px 20px rgba(2,9,21,.05);
}
*{box-sizing:border-box}html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--surface-1);color:var(--ink-700);font:15px/1.55 var(--font)}
a{color:var(--blue-700);text-decoration:none}a:hover{text-decoration:underline}
.wrap{max-width:1080px;margin:0 auto;padding:0 24px 64px}
.topbar{position:sticky;top:0;z-index:5;background:rgba(255,255,255,.92);backdrop-filter:blur(8px);border-bottom:1px solid var(--border-soft)}
.topbar .wrap{display:flex;align-items:center;gap:12px;padding:12px 24px;font-size:13px;color:var(--ink-200)}
.topbar .mark{width:22px;height:22px;color:var(--blue-700)}.topbar strong{color:var(--ink-900)}
.hero{padding:44px 0 20px}.hero .kicker{font-size:12px;letter-spacing:.12em;text-transform:uppercase;color:var(--blue-600);font-weight:600}
.hero h1{margin:6px 0 10px;font-size:32px;line-height:1.15;color:var(--ink-900);letter-spacing:-.01em}
.hero p{margin:0;max-width:760px;color:var(--ink-500);font-size:16px}
.nutshell{margin:12px 0 28px;padding:16px 20px;border-left:4px solid var(--blue-300);background:var(--blue-50);border-radius:0 var(--radius) var(--radius) 0;color:var(--ink-500)}
section{margin:28px 0}section h2{font-size:20px;margin:0 0 4px;color:var(--ink-900)}
section .sub{margin:0 0 14px;font-size:13px;color:var(--ink-200)}
.card{background:var(--surface-0);border:1px solid var(--border);border-radius:var(--radius);box-shadow:var(--shadow);padding:16px 18px;margin:0 0 12px}
.card h3{margin:0 0 6px;font-size:16px;color:var(--ink-900)}.card p{margin:4px 0;color:var(--ink-500)}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:12px}.grid .card{margin:0}
.chip{display:inline-block;padding:2px 9px;border-radius:999px;background:var(--blue-100);color:var(--blue-700);font-size:12px;font-weight:600;margin:0 6px 4px 0;font-family:var(--mono)}
.chip.k{background:var(--purple-100);color:var(--purple-500)}.chip.f{background:var(--surface-3);color:var(--ink-500);font-weight:500}
table{width:100%;border-collapse:collapse;background:var(--surface-0);border:1px solid var(--border);border-radius:var(--radius);overflow:hidden;font-size:14px}
th,td{padding:9px 12px;border-bottom:1px solid var(--border-soft);text-align:left;vertical-align:top}th{background:var(--surface-2);color:var(--ink-300);font-size:12px;text-transform:uppercase;letter-spacing:.06em}
tr:last-child td{border-bottom:0}td code{font-family:var(--mono);font-size:12.5px;color:var(--ink-700)}
.tablewrap{overflow-x:auto}
pre{margin:8px 0 0;padding:14px 16px;background:var(--surface-2);border:1px solid var(--border-soft);border-radius:10px;overflow-x:auto;font:12.5px/1.5 var(--mono);color:var(--ink-700)}
pre code{background:none;padding:0}.excerpt .meta{font-size:12.5px;color:var(--ink-200);font-family:var(--mono)}.excerpt .meta b{color:var(--ink-700)}
.bar{height:6px;border-radius:3px;background:var(--surface-3);overflow:hidden;min-width:80px}.bar i{display:block;height:100%;background:var(--blue-600)}
.prov{font-size:13px}.prov li{margin:0 0 4px}.prov .src{font-family:var(--mono);color:var(--purple-500)}
.muted{color:var(--ink-200);font-size:13px}.empty{color:var(--ink-200);font-style:italic}
footer{margin-top:48px;padding-top:16px;border-top:1px solid var(--border-soft);font-size:12px;color:var(--ink-200)}
@media(max-width:640px){.hero h1{font-size:24px}.wrap{padding:0 14px 40px}}
"#;

/// Shared client-side helpers: manifest loading + tiny DOM builders.
const REPORT_JS: &str = r#"
var M=(function(){try{return JSON.parse(document.getElementById("chaos-manifest").textContent)}catch(e){return {}}})();
function h(tag,attrs,kids){var el=document.createElement(tag);attrs=attrs||{};for(var k in attrs){if(k==="text")el.textContent=attrs[k];else if(k==="html")el.innerHTML=attrs[k];else el.setAttribute(k,attrs[k]);}
 (kids||[]).forEach(function(c){if(c==null)return;el.appendChild(typeof c==="string"?document.createTextNode(c):c)});return el;}
function arr(v){return Array.isArray(v)?v:[]}function str(v){return v==null?"":String(v)}function num(v){return typeof v==="number"?v:0}
function sec(title,sub,body){var s=h("section",{},[h("h2",{text:title})]);if(sub)s.appendChild(h("p",{class:"sub",text:sub}));if(body&&body.length){body.forEach(function(b){s.appendChild(b)})}else s.appendChild(h("p",{class:"empty",text:"Nothing recorded."}));return s;}
function table(cols,rows,cell){var t=h("table",{},[h("thead",{},[h("tr",{},cols.map(function(c){return h("th",{text:c})}))])]);var tb=h("tbody");rows.forEach(function(r,i){tb.appendChild(h("tr",{},cell(r,i).map(function(c){return h("td",{},[typeof c==="string"?document.createTextNode(c):c])})))});t.appendChild(tb);return h("div",{class:"tablewrap"},[t]);}
function chips(list,cls){return h("div",{},arr(list).map(function(x){return h("span",{class:"chip "+(cls||""),text:str(x)})}))}
function pct(v){var b=h("div",{class:"bar",title:String(v)});b.appendChild(h("i",{style:"width:"+Math.max(0,Math.min(100,Math.round(num(v)*100)))+"%"}));return b;}
function link(label,href){return href?h("a",{href:href,text:label}):h("span",{text:label})}
function provenance(){var p=arr(M.provenance);if(!p.length)return null;return sec("How this was generated","Provenance breadcrumbs recorded while gathering the evidence.",[h("ul",{class:"prov"},p.map(function(b){return h("li",{},[h("span",{class:"src",text:str(b.source)+"/"+str(b.method)})," ",str(b.detail),b.locator?h("span",{class:"muted",text:"  "+str(b.locator)}):null])}))]);}
function mount(hero,body){var root=document.getElementById("root");root.appendChild(hero);body.forEach(function(s){if(s)root.appendChild(s)});var pv=provenance();if(pv)root.appendChild(pv);
 root.appendChild(h("footer",{},["Generated by Chaos Substrate · content hash ",h("code",{text:str(M.content_hash)})]));}
function hero(kicker,title,lead){var d=h("div",{class:"hero"},[h("div",{class:"kicker",text:kicker}),h("h1",{text:title})]);if(lead)d.appendChild(h("p",{text:lead}));return d;}
function summaryBand(text){return text?h("div",{class:"nutshell",text:text}):null}
"#;

/// Feature answer: question, features, excerpts, graph edges, surfaces.
const FEATURE_JS: &str = r#"
(function(){
 var feats=arr(M.features),ex=arr(M.excerpts),edges=arr(M.edges),surf=arr(M.surfaces);
 var body=[];
 body.push(sec("Features involved",feats.length+" feature(s) matched by meaning and graph proximity.",feats.map(function(f){
  var c=h("div",{class:"card"},[h("h3",{text:str(f.label)})]);
  if(f.summary)c.appendChild(h("p",{text:str(f.summary)}));
  var meta=[];if(f.member_count!=null)meta.push(num(f.member_count)+" members");if(f.score!=null)meta.push("relevance "+Math.round(num(f.score)*100)+"%");
  if(meta.length)c.appendChild(h("p",{class:"muted",text:meta.join(" · ")}));
  if(arr(f.symbols).length)c.appendChild(chips(f.symbols));
  return c;})));
 body.push(sec("Code excerpts","Verbatim source the answer is grounded in.",ex.map(function(e){
  var range=e.start_line!=null?" : "+num(e.start_line)+(e.end_line!=null?"–"+num(e.end_line):""):"";
  var meta=h("div",{class:"meta"},[h("b",{text:str(e.path)}),range,e.symbol?"   ":"",e.symbol?h("span",{class:"chip k",text:str(e.symbol)}):null,e.retrieved_by?h("span",{class:"chip f",text:str(e.retrieved_by)}):null]);
  return h("div",{class:"card excerpt"},[meta,h("pre",{},[h("code",{text:str(e.code||e.content)})])]);})));
 body.push(sec("Graph edges","Typed relations between the symbols above.",edges.length?[table(["From","Kind","To"],edges,function(e){return [h("code",{text:str(e.from)}),h("span",{class:"chip k",text:str(e.kind)}),h("code",{text:str(e.to)})]})]:[]));
 body.push(sec("User surfaces","Routes, commands, env vars and GraphQL fields the feature exposes.",surf.length?[table(["Kind","Surface","Where"],surf,function(s){return [h("span",{class:"chip",text:str(s.kind)}),h("code",{text:str(s.value||s.name)}),str(s.path)]})]:[]));
 mount(hero("Feature",str(M.title||M.question||"Feature"),M.title?str(M.question):""),[summaryBand(str(M.answer||M.summary))].concat(body));
})();
"#;

/// Site index: one card per community, linking to its page.
const SITE_INDEX_JS: &str = r#"
(function(){
 var feats=arr(M.features);
 var cards=feats.map(function(f){
  var files=arr(f.files).length||num(f.file_count);
  var c=h("div",{class:"card"},[h("h3",{},[link(str(f.label),str(f.page||f.href))]),
   h("p",{class:"muted",text:num(f.member_count)+" members · "+files+" files"+(f.layer?" · "+str(f.layer):"")})]);
  var s=str(f.summary);if(s)c.appendChild(h("p",{text:s.length>220?s.slice(0,217)+"…":s}));
  if(arr(f.languages).length)c.appendChild(chips(f.languages,"f"));
  return c;});
 mount(hero("Feature site",str(M.title||M.repo||"Features"),str(M.repo?"Repository: "+M.repo:"")),[summaryBand(str(M.summary)),sec("Features",feats.length+" feature(s); click a card for its drill-down page.",cards.length?[h("div",{class:"grid"},cards)]:[])]);
})();
"#;

/// One community's drill-down page.
const SITE_PAGE_JS: &str = r#"
(function(){
 var mem=arr(M.members),files=arr(M.files),rel=arr(M.relations),prior=arr(M.related_pages);
 var body=[summaryBand(str(M.summary))];
 body.push(sec("Top members",mem.length+" symbol(s) ranked by degree.",mem.length?[table(["Symbol","Kind","File"],mem,function(m){return [h("code",{text:str(m.name||m.label)}),h("span",{class:"chip k",text:str(m.kind)}),str(m.path)]})]:[]));
 body.push(sec("Files","Source files this feature lives in.",files.length?[chips(files.map(function(f){return typeof f==="string"?f:str(f.path)}),"f")]:[]));
 body.push(sec("Relations to other features","Quotient-graph edges; in-scope neighbours link to their page.",rel.length?[table(["Direction","Kind","Feature","Weight"],rel,function(r){return [str(r.direction||"→"),h("span",{class:"chip k",text:str(r.kind)}),link(str(r.label),str(r.page||r.href)),String(num(r.weight))]})]:[]));
 body.push(sec("Prior related pages","Previously generated pages overlapping this feature.",prior.map(function(p){return h("div",{class:"card"},[h("h3",{},[link(str(p.title||p.path),str(p.href||p.path))]),p.reason?h("p",{class:"muted",text:str(p.reason)}):null])})));
 if(M.index)body.push(h("p",{},[link("← Back to the feature index",str(M.index))]));
 mount(hero("Feature",str(M.label||M.title||"Feature"),str(M.repo?"Repository: "+M.repo:"")),body);
})();
"#;

/// Correlation report: intra-repo pairs + cross-repo links.
const CORRELATIONS_JS: &str = r#"
(function(){
 var intra=arr(M.intra),cross=arr(M.cross);
 var body=[summaryBand(str(M.summary))];
 body.push(sec("Intra-repo correlations","Feature pairs that share files or symbols.",intra.length?[table(["Feature A","Feature B","Shared","Strength"],intra,function(p){return [str(p.a||p.left),str(p.b||p.right),chips(arr(p.shared).slice(0,8),"f"),pct(p.score)]})]:[]));
 body.push(sec("Cross-repo links","Consumer → provider links detected from the persisted index.",cross.length?[table(["Consumer","Provider","Kind","Evidence","Confidence"],cross,function(l){return [str(l.consumer),str(l.provider),h("span",{class:"chip k",text:str(l.kind)}),h("code",{text:str(l.evidence)}),pct(l.confidence)]})]:[]));
 mount(hero("Correlations",str(M.title||"Feature correlations"),str(M.repo||M.project||"")),body);
})();
"#;

const PAGE_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>__TITLE__</title>
<style>__THEME__</style>
</head>
<body>
<div class="topbar"><div class="wrap"><svg class="mark" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="5" cy="6" r="2.4"/><circle cx="19" cy="9" r="2.4"/><circle cx="11" cy="18" r="2.4"/><path d="M7 7 17 8.4M16.8 11 12.4 15.8M9 16 6 8.3"/></svg><strong>Chaos Substrate</strong><span>__KIND__</span></div></div>
<main class="wrap" id="root"></main>
<script type="application/json" id="__MANIFEST_ID__">__DATA__</script>
<script>__JS__</script>
</body>
</html>
"#;

fn kind_label(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature => "feature",
        PageKind::SiteIndex => "site index",
        PageKind::SitePage => "feature page",
        PageKind::Correlations => "correlations",
    }
}

fn kind_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature => FEATURE_JS,
        PageKind::SiteIndex => SITE_INDEX_JS,
        PageKind::SitePage => SITE_PAGE_JS,
        PageKind::Correlations => CORRELATIONS_JS,
    }
}

fn manifest_title(kind: PageKind, manifest: &Value) -> String {
    for key in ["title", "label", "question", "repo"] {
        if let Some(s) = manifest.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
    }
    format!("Chaos {}", kind_label(kind))
}

/// Render a full HTML document for `kind` from `manifest` (pure; no I/O).
pub fn render_page(kind: PageKind, manifest: &Value) -> String {
    let data = escape_script_json(&manifest.to_string());
    PAGE_TEMPLATE
        .replace("__TITLE__", &html_escape(&manifest_title(kind, manifest)))
        .replace("__KIND__", kind_label(kind))
        .replace("__THEME__", THEME_CSS)
        .replace("__MANIFEST_ID__", MANIFEST_ID)
        .replace("__DATA__", &data)
        .replace("__JS__", &format!("{REPORT_JS}\n{}", kind_js(kind)))
}

/// Extract the embedded manifest JSON from a page's HTML, if present.
pub fn extract_manifest(html: &str) -> Option<Value> {
    let open = format!("id=\"{MANIFEST_ID}\">");
    let start = html.find(&open)? + open.len();
    let end = html[start..].find("</script>")? + start;
    serde_json::from_str(&html[start..end]).ok()
}

impl PageStore for FsPages {
    fn write_page(&self, path: &Path, kind: PageKind, manifest: &Value) -> Result<bool> {
        let new_hash = manifest.get("content_hash").and_then(Value::as_str);
        if let (Some(new_hash), Some(existing)) = (new_hash, self.read_manifest(path)) {
            if existing.get("content_hash").and_then(Value::as_str) == Some(new_hash) {
                return Ok(false);
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, render_page(kind, manifest))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(true)
    }

    fn read_manifest(&self, path: &Path) -> Option<Value> {
        let html = std::fs::read_to_string(path).ok()?;
        extract_manifest(&html)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escapes() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#039;"
        );
        let j = escape_script_json(r#"{"x":"</script><!--"}"#);
        assert!(!j.contains("</"));
        assert!(!j.contains("<!--"));
        let v: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["x"], "</script><!--");
    }

    #[test]
    fn write_cached_then_rewrite_on_hash_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("f.html");
        let m1 = json!({"content_hash":"h1","title":"T","question":"why?","features":[]});
        assert!(FsPages.write_page(&path, PageKind::Feature, &m1).unwrap());
        assert!(!FsPages.write_page(&path, PageKind::Feature, &m1).unwrap());
        let m2 = json!({"content_hash":"h2","title":"T2"});
        assert!(FsPages.write_page(&path, PageKind::Feature, &m2).unwrap());
        assert_eq!(FsPages.read_manifest(&path).unwrap(), m2);
        assert!(FsPages
            .read_manifest(&dir.path().join("missing.html"))
            .is_none());
    }

    #[test]
    fn manifest_round_trip_survives_script_breakout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.html");
        let m = json!({"content_hash":"h","label":"</script><script>alert(1)</script><!--",
            "summary":"a & b < c","members":[{"name":"f","kind":"fn","path":"src/a.rs"}]});
        for kind in [
            PageKind::SiteIndex,
            PageKind::SitePage,
            PageKind::Correlations,
        ] {
            std::fs::remove_file(&path).ok();
            FsPages.write_page(&path, kind, &m).unwrap();
            let html = std::fs::read_to_string(&path).unwrap();
            let open = format!("id=\"{MANIFEST_ID}\">");
            let start = html.find(&open).unwrap() + open.len();
            let block = &html[start..start + html[start..].find("</script>").unwrap()];
            assert!(
                !block.contains("</") && !block.contains("<!--"),
                "breakout leaked"
            );
            assert_eq!(FsPages.read_manifest(&path).unwrap(), m);
            assert!(html.contains("&lt;/script&gt;"), "title must be escaped");
        }
    }
}

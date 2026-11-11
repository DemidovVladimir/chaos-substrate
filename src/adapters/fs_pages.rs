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
  --topbar:rgba(255,255,255,.92);
  --lang-typescript:#2a78d6;--lang-solidity:#eb6834;--lang-python:#1baf7a;--lang-graphql:#eda100;
  --lang-markdown:#e87ba4;--lang-javascript:#008300;--lang-rust:#6250d6;--lang-other:rgb(127,138,152);
}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){color-scheme:dark;
  --ink-900:#eef3f8;--ink-700:#d2dbe6;--ink-500:#b0bccb;--ink-300:#8fb4d8;--ink-200:#8a95a3;--border:#2b394a;--border-soft:#203041;
  --surface-0:#121b26;--surface-1:#0b1119;--surface-2:#162231;--surface-3:#1d2b3b;--blue-50:#12233a;--blue-100:#163257;--blue-300:#2c5d93;
  --blue-600:#6ea4d9;--blue-700:#8dbff2;--purple-100:#2a2244;--purple-500:#b29cf2;--ok:#4cc38a;--warn:#e3a443;
  --shadow:0 1px 2px rgba(0,0,0,.45),0 6px 20px rgba(0,0,0,.3);--topbar:rgba(11,17,25,.9);
  --lang-typescript:#3987e5;--lang-solidity:#d95926;--lang-python:#199e70;--lang-graphql:#c98500;
  --lang-markdown:#d55181;--lang-javascript:#008300;--lang-rust:#9085e9;--lang-other:#8a95a3;}}
:root[data-theme="dark"]{color-scheme:dark;
  --ink-900:#eef3f8;--ink-700:#d2dbe6;--ink-500:#b0bccb;--ink-300:#8fb4d8;--ink-200:#8a95a3;--border:#2b394a;--border-soft:#203041;
  --surface-0:#121b26;--surface-1:#0b1119;--surface-2:#162231;--surface-3:#1d2b3b;--blue-50:#12233a;--blue-100:#163257;--blue-300:#2c5d93;
  --blue-600:#6ea4d9;--blue-700:#8dbff2;--purple-100:#2a2244;--purple-500:#b29cf2;--ok:#4cc38a;--warn:#e3a443;
  --shadow:0 1px 2px rgba(0,0,0,.45),0 6px 20px rgba(0,0,0,.3);--topbar:rgba(11,17,25,.9);
  --lang-typescript:#3987e5;--lang-solidity:#d95926;--lang-python:#199e70;--lang-graphql:#c98500;
  --lang-markdown:#d55181;--lang-javascript:#008300;--lang-rust:#9085e9;--lang-other:#8a95a3;}
*{box-sizing:border-box}html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--surface-1);color:var(--ink-700);font:15px/1.55 var(--font)}
a{color:var(--blue-700);text-decoration:none}a:hover{text-decoration:underline}
.wrap{max-width:1080px;margin:0 auto;padding:0 24px 64px}
.topbar{position:sticky;top:env(safe-area-inset-top,0px);z-index:5;background:var(--topbar);backdrop-filter:blur(8px);border-bottom:1px solid var(--border-soft)}
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
.mapwrap{overflow-x:auto;background:var(--surface-0);border:1px solid var(--border);border-radius:var(--radius);box-shadow:var(--shadow);padding:8px}
.fmap{display:block;font:12px var(--mono)}.fmap .band{fill:var(--surface-2);stroke:var(--border-soft)}
.fmap .bl{font:600 13px var(--font);fill:var(--ink-900)}.fmap .bm{font:12px var(--font);fill:var(--ink-200)}
.fmap .n rect{fill:var(--surface-0);stroke:var(--ink-200);stroke-dasharray:3 2}.fmap .n.ex rect{fill:var(--blue-50);stroke:var(--blue-600);stroke-dasharray:none}
.fmap .n text{fill:var(--ink-700)}.fmap .n .k{fill:var(--ink-200);font-size:10.5px}.fmap .n.pick{cursor:pointer}
.fmap .e{fill:none;stroke-width:1.4;opacity:.7}.fmap .e.calls{stroke:var(--blue-600)}.fmap .e.imports{stroke:var(--ink-200)}.fmap .e.other{stroke:var(--purple-500)}
.fmap .e.cross{stroke:var(--warn);stroke-width:1.3;stroke-dasharray:5 4;opacity:.45}
.fmap .ah.calls{fill:var(--blue-600)}.fmap .ah.imports{fill:var(--ink-200)}.fmap .ah.other{fill:var(--purple-500)}.fmap .ah.cross{fill:var(--warn)}
.fmap.focus .n:not(.on),.fmap.focus .e:not(.on){opacity:.12}.fmap .n.on rect{stroke-width:2}.fmap .e.on{opacity:1;stroke-width:2.4}
.legend{display:flex;flex-wrap:wrap;gap:6px 16px;margin:10px 2px 0;font-size:12.5px;color:var(--ink-500)}
.legend span::before{content:"";display:inline-block;width:18px;height:0;margin-right:6px;vertical-align:3px;border-top:2px solid}
.legend .ex::before{height:10px;vertical-align:-1px;border:1.5px solid var(--blue-600);background:var(--blue-50);border-radius:3px}
.legend .nb::before{height:10px;vertical-align:-1px;border:1.5px dashed var(--ink-200);border-radius:3px}
.legend .calls::before{border-color:var(--blue-600)}.legend .imports::before{border-color:var(--ink-200)}.legend .other::before{border-color:var(--purple-500)}.legend .cross::before{border-top:2px dashed var(--warn)}
.excerpt{scroll-margin-top:64px}details.card>summary{cursor:pointer;list-style:none}details.card>summary::-webkit-details-marker{display:none}
details.card>summary::before{content:"\25B8  ";color:var(--ink-200)}details.card[open]>summary::before{content:"\25BE  "}
.flash{animation:flash 1.6s ease-out}@keyframes flash{0%{box-shadow:0 0 0 3px var(--blue-300)}100%{box-shadow:var(--shadow)}}
a:focus-visible,button:focus-visible,input:focus-visible,summary:focus-visible,.gn:focus-visible{outline:2px solid var(--blue-600);outline-offset:2px}
.l-typescript{--lc:var(--lang-typescript)}.l-solidity{--lc:var(--lang-solidity)}.l-python{--lc:var(--lang-python)}.l-graphql{--lc:var(--lang-graphql)}
.l-markdown{--lc:var(--lang-markdown)}.l-javascript{--lc:var(--lang-javascript)}.l-rust{--lc:var(--lang-rust)}.l-other{--lc:var(--lang-other)}
.sw{display:inline-block;flex:none;width:9px;height:9px;border-radius:50%;background:var(--lc,var(--lang-other))}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(128px,1fr));gap:10px;margin:4px 0 8px}
.stat{background:var(--surface-0);border:1px solid var(--border);border-radius:10px;padding:12px 14px}
.stat .sv{font-weight:650;font-size:22px;line-height:1.2;color:var(--ink-900);font-variant-numeric:tabular-nums}
.stat .sl{font-size:11.5px;color:var(--ink-200);text-transform:uppercase;letter-spacing:.07em;margin-top:2px}
.n{display:block;text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.sha{font-size:11.5px;word-break:break-all}.path{overflow-wrap:anywhere;font-size:12px}
.gqt table td:nth-child(2){min-width:300px}.gqt table td:nth-child(3),.gqt table td:nth-child(4){min-width:220px}
.ctt table td:nth-child(4){min-width:320px}.ctt table td:nth-child(1){min-width:140px}
.langs{display:flex;flex-wrap:wrap;gap:2px 10px}.lchip{display:inline-flex;align-items:center;gap:5px;font-size:12px;color:var(--ink-500);white-space:nowrap}
.rchip{display:inline-flex;align-items:center;gap:6px;padding:2px 9px;border-radius:999px;background:var(--surface-3);color:var(--ink-700);font:600 12px var(--mono);margin:0 6px 4px 0}
.hub{display:grid;grid-template-columns:minmax(0,1fr) 340px;gap:12px;align-items:start}
.gbox{background:var(--surface-0);border:1px solid var(--border);border-radius:var(--radius);box-shadow:var(--shadow);overflow:hidden;min-width:0}
.gtools{display:flex;flex-wrap:wrap;gap:8px;align-items:center;padding:8px 10px;border-bottom:1px solid var(--border-soft)}
.gtools input{flex:1 1 180px;min-width:0;font:inherit;font-size:13px;padding:6px 10px;border:1px solid var(--border);border-radius:8px;background:var(--surface-1);color:var(--ink-700)}
.btn{font:inherit;font-size:13px;line-height:1.2;padding:6px 11px;border:1px solid var(--border);border-radius:8px;background:var(--surface-0);color:var(--ink-700);cursor:pointer}.btn:hover{background:var(--surface-2)}
.gsvg{display:block;width:100%;height:640px;touch-action:none;cursor:grab;background:var(--surface-0);user-select:none}.gsvg.dragging{cursor:grabbing}
.gsvg .cl{fill:var(--surface-2);stroke:var(--border)}
.gsvg .cll{font:600 14px var(--font);fill:var(--ink-900);paint-order:stroke;stroke:var(--surface-0);stroke-width:4px;stroke-linejoin:round}.gsvg .cll tspan{font-weight:400;font-size:12px;fill:var(--ink-200)}
.gsvg .ge{fill:none;transition:opacity .15s}.gsvg .ge.i{stroke:var(--ink-200);opacity:.5}.gsvg .ge.x{stroke:var(--purple-500);stroke-width:1.7;stroke-dasharray:6 4;opacity:.85}.gsvg .ah{fill:var(--purple-500)}
.gsvg .gn{cursor:pointer;transition:opacity .15s}
.gsvg .gl{font:500 11.5px var(--font);fill:var(--ink-700);paint-order:stroke;stroke:var(--surface-0);stroke-width:3.5px;stroke-linejoin:round;pointer-events:none}.gsvg .gl.h{display:none}
.gsvg.focus .gn:not(.on):not(.sel){opacity:.16}.gsvg.focus .ge:not(.on){opacity:.06}.gsvg .gn.on .gl,.gsvg .gn.sel .gl{display:block}.gsvg .ge.on{opacity:1}.gsvg .ge.i.on{stroke:var(--ink-500)}
.panel{background:var(--surface-0);border:1px solid var(--border);border-radius:var(--radius);box-shadow:var(--shadow);padding:16px 18px;max-height:700px;overflow:auto;min-width:0}
.panel h3{margin:8px 0 2px;font-size:17px;line-height:1.3;color:var(--ink-900);overflow-wrap:anywhere;text-wrap:balance}
.panel h4{margin:16px 0 6px;font-size:11.5px;text-transform:uppercase;letter-spacing:.07em;color:var(--ink-300)}
.panel .psum{color:var(--ink-500);font-size:14px}.plink{margin-top:16px;font-size:13px}
.conn{list-style:none;margin:0;padding:0;display:grid;gap:8px}.conn li{font-size:13px;color:var(--ink-500);overflow-wrap:anywhere}
.conn button{font:inherit;font-weight:600;color:var(--blue-700);background:none;border:0;padding:0;cursor:pointer;text-align:left;overflow-wrap:anywhere}.conn button:hover{text-decoration:underline}
.conn .ev{display:block;white-space:pre-wrap;font:11.5px/1.45 var(--mono);color:var(--ink-500);margin-top:2px}
.hubleg{margin-top:12px}.hubleg span::before{content:none}.hubleg .lgl{display:inline-flex;align-items:center;gap:6px}
.hubleg .lg-i::before,.hubleg .lg-x::before{content:"";display:inline-block;width:22px;height:0;margin-right:6px;vertical-align:3px}
.hubleg .lg-i::before{border-top:2px solid var(--ink-200)}.hubleg .lg-x::before{border-top:2px dashed var(--purple-500)}
.xl{font-size:13.5px;color:var(--ink-700);overflow-wrap:anywhere}.evl{display:grid;gap:3px}.evl code{font:11.5px/1.45 var(--mono);color:var(--ink-500);overflow-wrap:anywhere}
.fsum{font-size:13px;color:var(--ink-500)}
.gsvg .gn .dot{fill:var(--lc);stroke:var(--surface-0);stroke-width:2}.gsvg .gn .ring{fill:none;stroke:var(--ink-500);stroke-width:1.6;stroke-dasharray:2.5 2.5}
.gsvg .gn:hover .dot,.gsvg .gn.on .dot{stroke:var(--ink-700);stroke-width:1.5}.gsvg .gn.sel .dot{stroke:var(--ink-900);stroke-width:3}
.ringsw{display:inline-block;width:12px;height:12px;border-radius:50%;border:1.6px dashed var(--ink-500)}
.eph{margin:22px 0 4px;font-size:16px;color:var(--ink-900)}.epn{font-weight:600;color:var(--ink-900)}
.sig{font:12px/1.45 var(--mono);color:var(--ink-500);overflow-wrap:anywhere;white-space:pre-wrap}
.acc{display:inline-block;padding:1px 8px;border-radius:999px;font-size:11.5px;font-weight:600;white-space:nowrap}
.acc.read{background:var(--surface-3);color:var(--ink-500)}.acc.write{background:var(--purple-100);color:var(--purple-500)}
.ctr{display:flex;flex-wrap:wrap;align-items:baseline;gap:4px 10px;margin:14px 0 6px;color:var(--ink-900)}.ctr .chip{margin:0}
.evs{margin:6px 0 0;font-size:12.5px;overflow-wrap:anywhere}
.acc.listen{background:var(--blue-100);color:var(--blue-700)}.accs{display:flex;flex-wrap:wrap;gap:4px}
.mth{display:inline-block;min-width:58px;text-align:center;padding:1px 8px;border-radius:6px;font:600 11.5px var(--mono);background:var(--surface-3);color:var(--ink-700)}
.mct table td:nth-child(2){min-width:280px}.mct table td:nth-child(3),.rtt table td:nth-child(3),.hkt table td:nth-child(4){min-width:220px}.hkt table td:nth-child(3){min-width:180px}.hkn table td:nth-child(3){min-width:220px}
.epdoc{margin:12px 0 16px}.epdoc .dt{font-weight:600;color:var(--ink-900)}
.chap{margin:6px 0 0;padding-left:24px;color:var(--ink-500);font-size:14px}.chap li{margin:2px 0}
.tl{list-style:none;margin:8px 0 0;padding:0;display:grid;gap:5px}.tl li{display:flex;gap:12px;font-size:14px;color:var(--ink-500)}
.tl code{flex:none;min-width:64px;font:600 12.5px var(--mono);color:var(--blue-700);font-variant-numeric:tabular-nums}
.filter{display:block;width:100%;max-width:440px;font:inherit;font-size:14px;padding:8px 12px;border:1px solid var(--border);border-radius:8px;background:var(--surface-0);color:var(--ink-700);margin:0 0 12px}
details.repo{margin:0 0 6px;border-bottom:1px solid var(--border-soft)}details.repo>summary{cursor:pointer;font-weight:600;color:var(--ink-900);padding:10px 0}
details.repo>summary span{font-weight:400;color:var(--ink-200);margin-left:10px;font-size:13px}details.repo[open]{padding-bottom:12px}
@media(max-width:960px){.hub{grid-template-columns:1fr}.panel{max-height:none}}
@media(max-width:640px){.hero h1{font-size:24px}.wrap{padding:0 16px 40px}.gsvg{height:460px}}
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

/// Feature map (Feature + ProjectFeature pages): one swimlane per repository,
/// columns by call depth (callers left), cross-repo links between lanes.
const MAP_JS: &str = r#"
function jump(id){var el=document.getElementById(id);if(!el)return;if(el.tagName==="DETAILS")el.open=true;el.scrollIntoView({behavior:"smooth",block:"start"});el.classList.remove("flash");void el.offsetWidth;el.classList.add("flash");}
function excerptCard(e,id,open){
 var range=e.start_line!=null?" : "+num(e.start_line)+(e.end_line!=null?"\u2013"+num(e.end_line):""):"";
 var meta=h("summary",{class:"meta"},[h("b",{text:str(e.path)}),range,"   ",e.symbol?h("span",{class:"chip k",text:str(e.symbol)}):null,e.retrieved_by?h("span",{class:"chip f",text:str(e.retrieved_by)}):null]);
 var d=h("details",{class:"card excerpt",id:id},[meta,h("pre",{},[h("code",{text:str(e.code||e.content)})])]);if(open)d.open=true;return d;}
function featureMap(lanes,links,pick){
 var NS="http://www.w3.org/2000/svg",CW=204,CG=40,NH=26,RG=9,LH=32,LP=12,PX=14,MAXL=6,ROWS=9,GRID=4;
 function sv(tag,a,kids){var el=document.createElementNS(NS,tag);for(var k in a)el.setAttribute(k,a[k]);(kids||[]).forEach(function(c){if(c!=null)el.appendChild(typeof c==="string"?document.createTextNode(c):c)});return el;}
 var nodes={},list=[],E=[];
 function key(l,s){return l+"\u0001"+s}
 function node(l,s,info){var k=key(l,s),n=nodes[k];if(!n){n=nodes[k]={lane:l,sym:s,path:"",kind:"",ex:-1,out:[],inn:[],deg:0};list.push(n)}
  if(info.path&&!n.path)n.path=info.path;if(info.kind&&!n.kind)n.kind=info.kind;if(info.ex!=null&&n.ex<0){n.ex=info.ex;n.lines=info.lines}return n}
 lanes.forEach(function(L){
  arr(L.excerpts).forEach(function(e,i){node(L.key,str(e.symbol),{path:str(e.path),kind:str(e.kind),ex:i,lines:str(e.start_line)+"\u2013"+str(e.end_line)})});
  arr(L.edges).forEach(function(g){var a=node(L.key,str(g.from),{path:str(g.from_path)}),b=node(L.key,str(g.to),{path:str(g.to_path)});if(a===b)return;E.push({a:a,b:b,kind:str(g.kind)});a.out.push(b);b.inn.push(a);a.deg++;b.deg++});
 });
 arr(links).forEach(function(x){var a=nodes[key(str(x.from_repo),str(x.from))],b=nodes[key(str(x.to_repo),str(x.to))];if(a&&b&&a!==b){E.push({a:a,b:b,kind:"mentions",cross:true,term:str(x.term)});a.deg++;b.deg++}});
 function rank(n){return n.ex>=0?n.ex:1e4-n.deg}
 var y=4,width=0,bands=[];
 lanes.forEach(function(L){
  var ns=list.filter(function(n){return n.lane===L.key});if(!ns.length)return;
  ns.forEach(function(n){n.layer=0});
  for(var it=0;it<=MAXL;it++){var ch=false;ns.forEach(function(n){n.out.forEach(function(m){if(n.layer+1>m.layer&&n.layer<MAXL){m.layer=n.layer+1;ch=true}})});if(!ch)break}
  var linked=ns.filter(function(n){return n.out.length||n.inn.length}),lone=ns.filter(function(n){return !(n.out.length||n.inn.length)}).sort(function(p,q){return rank(p)-rank(q)});
  var layers=[];linked.forEach(function(n){(layers[n.layer]=layers[n.layer]||[]).push(n)});layers=layers.filter(Boolean);
  var phys=[];
  layers.forEach(function(c,ci){
   if(ci===0)c.sort(function(p,q){return rank(p)-rank(q)});
   else{c.forEach(function(n){var ps=n.inn.filter(function(m){return m.row!=null});n.bc=ps.length?ps.reduce(function(s,m){return s+m.row},0)/ps.length:1e3+rank(n)});c.sort(function(p,q){return p.bc-q.bc||rank(p)-rank(q)})}
   for(var i=0;i<c.length;i+=ROWS){var sl=c.slice(i,i+ROWS);sl.forEach(function(n,ri){n.row=ri});phys.push(sl)}
  });
  var rows=phys.length?Math.max.apply(null,phys.map(function(c){return c.length})):0,top=y+LH,G=Math.max(GRID,phys.length);
  phys.forEach(function(c,ci){c.forEach(function(n,ri){n.x=PX+ci*(CW+CG);n.y=top+ri*(NH+RG)})});
  var loneTop=top+rows*(NH+RG)+(rows&&lone.length?24:0);
  lone.forEach(function(n,i){n.x=PX+(i%G)*(CW+CG);n.y=loneTop+Math.floor(i/G)*(NH+RG)});
  var bh=loneTop-y+Math.ceil(lone.length/G)*(NH+RG)+LP;bands.push({label:str(L.label),y:y,h:bh,n:ns.length,loneY:rows&&lone.length?loneTop-8:null});
  width=Math.max(width,PX+Math.max(phys.length,Math.min(G,lone.length))*(CW+CG));y+=bh+12;
 });
 var W=Math.max(width+24,640),H=Math.max(y,80);
 var svg=sv("svg",{class:"fmap",width:W,height:H,viewBox:"0 0 "+W+" "+H,role:"img","aria-label":"Feature map"});
 var defs=sv("defs");["calls","imports","other","cross"].forEach(function(c){defs.appendChild(sv("marker",{id:"ah-"+c,viewBox:"0 0 10 10",refX:"9",refY:"5",markerWidth:"7",markerHeight:"7",orient:"auto-start-reverse"},[sv("path",{d:"M0,0L10,5L0,10z",class:"ah "+c})]))});svg.appendChild(defs);
 bands.forEach(function(b){svg.appendChild(sv("rect",{x:2,y:b.y,width:W-4,height:b.h,rx:10,class:"band"}));
  svg.appendChild(sv("text",{x:PX,y:b.y+21},[sv("tspan",{class:"bl"},[b.label]),sv("tspan",{class:"bm",dx:"10"},[b.n+" symbols"])]));
  if(b.loneY!=null)svg.appendChild(sv("text",{x:PX,y:b.loneY,class:"bm"},["no edge in the 1-hop graph"]));});
 var eg=sv("g"),ng=sv("g");svg.appendChild(eg);svg.appendChild(ng);
 function cls(e){return e.cross?"cross":e.kind==="calls"?"calls":e.kind==="imports"?"imports":"other"}
 E.forEach(function(e){var a=e.a,b=e.b,d;if(a.x==null||b.x==null)return;
  if(e.cross){var ax=a.x+CW/2,bx=b.x+CW/2,dn=b.y>a.y,ay=dn?a.y+NH:a.y,by=dn?b.y:b.y+NH,m=(ay+by)/2;d="M"+ax+","+ay+" C"+ax+","+m+" "+bx+","+m+" "+bx+","+by;}
  else if(b.x!==a.x){var fw=b.x>a.x,x1=fw?a.x+CW:a.x,x2=fw?b.x:b.x+CW,y1=a.y+NH/2,y2=b.y+NH/2,mx=(x1+x2)/2;d="M"+x1+","+y1+" C"+mx+","+y1+" "+mx+","+y2+" "+x2+","+y2;}
  else{var x=a.x+CW,y1=a.y+NH/2,y2=b.y+NH/2,o=22+Math.abs(y2-y1)*0.12;d="M"+x+","+y1+" C"+(x+o)+","+y1+" "+(x+o)+","+y2+" "+x+","+y2;}
  var c=cls(e),p=sv("path",{d:d,class:"e "+c,"marker-end":"url(#ah-"+c+")"});
  p.appendChild(sv("title",{},[e.cross?a.lane+" \u00b7 "+a.sym+"  \u2192  "+b.lane+" \u00b7 "+b.sym+"\nname '"+e.term+"' appears in its code":a.sym+"  "+e.kind+"  "+b.sym]));
  e.el=p;eg.appendChild(p);});
 var KT={function:"fn",method:"fn",type:"type",interface:"ifc",enum:"enum",type_alias:"type",field:"field",operation:"op",fragment:"frag",section:"\u00a7",file:"file",module:"mod"};
 function clip(s,n){return s.length>n?s.slice(0,n-1)+"\u2026":s}
 function label(n){var max=KT[n.kind]?20:25,s=n.sym;
  if(n.kind==="section"&&n.path){var stem=n.path.split("/").pop().replace(/\.mdx?$/,""),t=s.split(" > ").pop();
   if((stem+" \u203a "+t).length<=max)return stem+" \u203a "+t;
   return t.length<=14?clip(stem,Math.max(6,max-3-t.length))+" \u203a "+t:clip(t,max);}
  if(s.indexOf("/")>=0&&s.indexOf(" ")<0)return clip(s.split("/").pop(),max);
  if(s.length<=max)return s;var t=s.split(/ > |::|\./).pop();return t.length>=max?clip(t,max):"\u2026"+t}
 function focus(n,on){svg.classList.toggle("focus",on);n.el.classList.toggle("on",on);E.forEach(function(e){if(!e.el||(e.a!==n&&e.b!==n))return;e.el.classList.toggle("on",on);var o=e.a===n?e.b:e.a;if(o.el)o.el.classList.toggle("on",on)})}
 list.forEach(function(n){if(n.x==null)return;
  var g=sv("g",{class:"n"+(n.ex>=0?" ex":"")+(n.ex>=0&&pick?" pick":""),transform:"translate("+n.x+","+n.y+")"});
  g.appendChild(sv("rect",{width:CW,height:NH,rx:6}));g.appendChild(sv("text",{x:9,y:17},[label(n)]));
  if(KT[n.kind])g.appendChild(sv("text",{x:CW-8,y:17,class:"k","text-anchor":"end"},[KT[n.kind]]));
  g.appendChild(sv("title",{},[n.lane+" \u00b7 "+n.sym+(n.path?"\n"+n.path+(n.lines?" : "+n.lines:""):"")+(n.ex>=0?"\nretrieved excerpt"+(pick?" (click to open)":""):"\n1-hop neighbour")]));
  g.addEventListener("mouseenter",function(){focus(n,true)});g.addEventListener("mouseleave",function(){focus(n,false)});
  if(n.ex>=0&&pick)g.addEventListener("click",function(){pick(n.lane,n.ex)});
  n.el=g;ng.appendChild(g);});
 var kinds={};E.forEach(function(e){kinds[cls(e)]=1});
 var lg=[h("span",{class:"ex",text:"retrieved excerpt"}),h("span",{class:"nb",text:"1-hop neighbour"})];
 if(kinds.calls)lg.push(h("span",{class:"calls",text:"calls"}));if(kinds.imports)lg.push(h("span",{class:"imports",text:"imports"}));
 if(kinds.other)lg.push(h("span",{class:"other",text:"extends / implements / defines / queries"}));if(kinds.cross)lg.push(h("span",{class:"cross",text:"cross-repo: name appears verbatim in the other repo's code"}));
 return h("div",{},[h("div",{class:"mapwrap"},[svg]),h("div",{class:"legend"},lg)]);
}
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
 var lane=str(M.repo).split("/").pop()||"repo";
 if(ex.length)body.push(sec("Feature map","Retrieved symbols (solid) and their 1-hop neighbours (dashed), columns by call depth. Hover to trace, click a solid box to open its excerpt.",[featureMap([{key:lane,label:lane,excerpts:ex,edges:edges}],[],function(l,i){jump("ex-0-"+i)})]));
 body.push(sec("Code excerpts","Verbatim source the answer is grounded in.",ex.map(function(e,i){return excerptCard(e,"ex-0-"+i,true)})));
 body.push(sec("Graph edges","Typed relations between the symbols above.",edges.length?[table(["From","Kind","To"],edges,function(e){return [h("code",{text:str(e.from)}),h("span",{class:"chip k",text:str(e.kind)}),h("code",{text:str(e.to)})]})]:[]));
 body.push(sec("User surfaces","Routes, MCP tools, commands, env vars and GraphQL fields the feature exposes.",surf.length?[table(["Kind","Surface","Where"],surf,function(s){return [h("span",{class:"chip",text:str(s.kind)}),h("code",{text:str(s.value||s.name)}),str(s.path)]})]:[]));
 mount(hero("Feature",str(M.title||M.question||"Feature"),str(M.repo?"Repository: "+str(M.repo).split("/").pop():"")),[summaryBand(str(M.answer||M.summary))].concat(body));
})();
"#;

/// One question answered across a project's repos: map, cross-repo links,
/// then one section per repository.
const PROJECT_FEATURE_JS: &str = r#"
(function(){
 var repos=arr(M.repos),links=arr(M.links),idx={};repos.forEach(function(r,i){idx[str(r.alias)]=i});
 var lanes=repos.map(function(r){return {key:str(r.alias),label:str(r.alias),excerpts:arr(r.excerpts),edges:arr(r.edges)}});
 var nEx=0,nEd=0;repos.forEach(function(r){nEx+=arr(r.excerpts).length;nEd+=arr(r.edges).length});
 var body=[summaryBand(repos.length+" repositories \u00b7 "+nEx+" excerpts \u00b7 "+nEd+" graph edges \u00b7 "+links.length+" cross-repo links. Each repository answered the question from its own index; a cross-repo link means a symbol retrieved in one repository is named verbatim in code retrieved from another.")];
 body.push(sec("Feature map","One lane per repository; columns follow call depth (callers left). Solid boxes are retrieved excerpts, dashed boxes 1-hop neighbours, dashed orange lines cross-repo links. Hover to trace, click a solid box to open its excerpt.",[featureMap(lanes,links,function(l,i){jump("ex-"+idx[l]+"-"+i)})]));
 function side(r,sym,path){return h("div",{},[h("span",{class:"chip f",text:str(r)}),h("code",{text:str(sym)}),h("div",{class:"muted",text:str(path)})])}
 body.push(sec("Cross-repo links",links.length+" link(s): the symbol on the left is named verbatim in the code on the right.",links.length?[table(["Defined in","Name","Mentioned in"],links,function(l){return [side(l.from_repo,l.from,l.from_path),h("span",{class:"chip k",text:str(l.term)}),side(l.to_repo,l.to,l.to_path)]})]:[]));
 repos.forEach(function(r,ri){
  var feats=arr(r.features),ex=arr(r.excerpts),surf=arr(r.surfaces),kids=[];
  if(feats.length)kids.push(h("div",{class:"grid"},feats.map(function(f){var c=h("div",{class:"card"},[h("h3",{text:str(f.label)})]);if(f.summary)c.appendChild(h("p",{text:str(f.summary)}));if(f.score!=null)c.appendChild(h("p",{class:"muted",text:"relevance "+Math.round(num(f.score)*100)+"%"}));return c})));
  ex.forEach(function(e,i){kids.push(excerptCard(e,"ex-"+ri+"-"+i,i<2))});
  if(surf.length)kids.push(table(["Kind","Surface","Where"],surf,function(s){return [h("span",{class:"chip",text:str(s.kind)}),h("code",{text:str(s.name)}),str(s.path)+(s.line?":"+num(s.line):"")]}));
  body.push(sec(str(r.alias),feats.length+" feature(s) routed \u00b7 "+ex.length+" excerpt(s) \u00b7 "+arr(r.edges).length+" edge(s) \u00b7 "+str(r.repo),kids));
 });
 if(arr(M.warnings).length)body.push(sec("Warnings","Reported while answering.",[h("ul",{class:"prov"},arr(M.warnings).map(function(w){return h("li",{text:str(w)})}))]));
 mount(hero("Project feature",str(M.title||M.question||"Feature"),"Project "+str(M.project)+" \u00b7 "+repos.map(function(r){return str(r.alias)}).join(", ")),body);
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
 var EPS=entryPoints(M.entry_points,[str(M.repo)]);
 mount(hero("Feature site",str(M.title||M.repo||"Features"),str(M.repo?"Repository: "+M.repo:"")),[summaryBand(str(M.summary)),
  EPS.any?sec("Entry points","Where someone comes in from outside: the API, tools and routes a client or an agent calls, the contract functions anyone can call on-chain, the hooks the app's screens use, and for text, the chapters and timestamps a reader starts from.",EPS.body):null,
  sec("Features",feats.length+" feature(s); click a card for its drill-down page.",cards.length?[h("div",{class:"grid"},cards)]:[])]);
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

/// Entry-point rendering shared by the site index and the project hub: the
/// helpers both use and `entryPoints(block, repos)`, which turns a manifest's
/// `entry_points` block into page sections.
const ENTRY_JS: &str = r##"
var LANGS=["typescript","solidity","python","graphql","markdown","javascript","rust"];
function lk(l){return LANGS.indexOf(str(l))>=0?str(l):"other"}
function fmt(n){return num(n).toLocaleString("en-US")}
function sw(l){return h("i",{class:"sw l-"+lk(l)})}
function pathEl(p,tag){var parts=str(p).split("/"),kids=[];parts.forEach(function(x,i){kids.push(i<parts.length-1?x+"/":x);if(i<parts.length-1)kids.push(h("wbr"))});return h(tag||"div",{class:"muted path"},kids)}
function repoChip(alias,l){return h("span",{class:"rchip"},[l!=null?sw(l):null,str(alias)])}
function access(fn){var m=str(fn.mutability);return m==="view"||m==="pure"?"read":m==="payable"?"write · payable":"write"}
function accChip(fn){var a=access(fn);return h("span",{class:"acc "+(a==="read"?"read":"write"),text:a})}
function at(p,line){return pathEl(str(p)+(line?":"+num(line):""))}
var HOOK_DOES={query:["reads","read"],mutation:["writes","write"],subscription:["listens","listen"]};
function hookChips(kinds){return h("div",{class:"accs"},arr(kinds).map(function(k){var d=HOOK_DOES[str(k)]||[str(k),"read"];return h("span",{class:"acc "+d[1],text:d[0]})}))}
function entryPoints(EP,repos){
 EP=EP||{};
 var GQ=arr(EP.graphql),CT=arr(EP.contracts),MC=arr(EP.mcp_tools),RT=arr(EP.http_routes),HK=arr(EP.hooks),DC=arr(EP.documents),TL=arr(EP.timelines);
 var by={},rows=[],groups=[],multi=repos.length>1,body=[];
 function add(id,k,code){if(id)(by[str(id)]=by[str(id)]||[]).push({k:k,code:code})}
 function track(el,hay,g){rows.push({el:el,hay:hay.toLowerCase(),g:g})}
 function grp(title,count,open,kids){var d=h("details",{class:"repo"},[h("summary",{},[title,h("span",{text:count})])].concat(kids||[]));if(open)d.open=true;groups.push(d);return d}
 function perRepo(list){return repos.map(function(a){return {alias:a,rows:list.filter(function(x){return str(x.repo)===a})}}).filter(function(g){return g.rows.length})}
 function trs(t){return t.querySelectorAll("tbody tr")}
 function part(title,intro,els){els=els.filter(Boolean);if(!els.length)return;body.push(h("h3",{class:"eph",text:title}));body.push(h("p",{class:"muted",text:intro}));els.forEach(function(e){body.push(e)})}
 var n={graphql:GQ.length,functions:0,mcp:MC.length,routes:RT.length,hooks:HK.length,documents:DC.length,timeline:0};
 CT.forEach(function(c){arr(c.functions).forEach(function(fn){n.functions++;add(fn.feature,"contract function",str(c.name)+"."+str(fn.name))})});
 TL.forEach(function(t){n.timeline+=arr(t.entries).length});

 GQ.forEach(function(g){add(g.feature,"GraphQL field",str(g.field))});
 part("GraphQL API",GQ.length+" root fields, each listed once in the repository that owns the schema, with every operation in "+(multi?"this project":"this repository")+" that calls it. Directives such as @aws_lambda or @aws_api_key in the signature name the auth mode.",
  ["Mutation","Query","Subscription"].map(function(root){var rs=GQ.filter(function(g){return str(g.root)===root});if(!rs.length)return null;
   var t=table(["Field","Signature","Defined in","Called by"],rs,function(g){return [h("code",{class:"epn",text:str(g.name)}),h("code",{class:"sig",text:str(g.signature)}),
    h("div",{},(multi?[repoChip(g.repo)]:[]).concat(arr(g.files).map(function(p){return pathEl(p)}))),
    arr(g.callers).length?h("div",{class:"evl"},arr(g.callers).map(function(c){return h("div",{},[multi?h("span",{class:"rchip",text:str(c.repo)}):null,h("code",{text:str(c.op)}),pathEl(c.path)])})):h("span",{class:"muted",text:"no caller found"})]});
   t.classList.add("gqt");var d=grp(root,rs.length+" fields",true,[t]);
   trs(t).forEach(function(tr,i){var g=rs[i];track(tr,str(g.field)+" "+str(g.signature)+" "+arr(g.files).join(" ")+" "+arr(g.callers).map(function(c){return c.repo+" "+c.op+" "+c.path}).join(" "),d)});
   return d}));

 MC.forEach(function(x){add(x.feature,"MCP tool",str(x.name))});
 part("MCP tools",MC.length+" tools an AI agent can call through an MCP server. The name is what the agent sees when it lists the server's tools.",
  perRepo(MC).map(function(g){var t=table(["Tool","What it does","Defined in"],g.rows,function(x){return [h("code",{class:"epn",text:str(x.name)}),x.detail?h("span",{class:"fsum",text:str(x.detail)}):h("span",{class:"muted",text:"no description found"}),h("div",{},[at(x.path,x.line),x.owner&&str(x.owner)!==str(x.name)?h("div",{class:"muted",text:"in "+str(x.owner)}):null])]});
   t.classList.add("mct");var d=grp(g.alias,g.rows.length+" tools",true,[t]);trs(t).forEach(function(tr,i){var x=g.rows[i];track(tr,str(x.name)+" "+str(x.detail)+" "+str(x.path)+" "+str(x.owner),d)});return d}));

 RT.forEach(function(x){add(x.feature,"HTTP route",str(x.method)+" "+str(x.route))});
 part("HTTP routes",RT.length+" routes a browser, an app or another service can call over HTTP. A Next.js route handler is listed with the URL its folder gives it.",
  perRepo(RT).map(function(g){var t=table(["Method","Path","Defined in"],g.rows,function(x){return [h("span",{class:"mth",text:str(x.method)}),h("code",{class:"epn",text:str(x.route)}),h("div",{},[at(x.path,x.line),x.owner?h("div",{class:"muted",text:"in "+str(x.owner)}):null])]});
   t.classList.add("rtt");var d=grp(g.alias,g.rows.length+" routes",true,[t]);trs(t).forEach(function(tr,i){var x=g.rows[i];track(tr,str(x.method)+" "+str(x.route)+" "+str(x.path),d)});return d}));

 part("Smart contracts",n.functions+" public or external functions in "+CT.length+" contracts and interfaces. Anyone can call these directly on-chain, with no server in between. The Modifiers column lists only modifiers such as onlyOwner; checks written inside a function body (require, revert) are not shown, so an empty cell does not mean the function is unguarded. Deploy scripts, tests and libraries are left out.",
  perRepo(CT).map(function(g){var cs=g.rows,nf=0;cs.forEach(function(c){nf+=arr(c.functions).length});
   var d=grp(g.alias,cs.length+" contracts and interfaces · "+nf+" public functions",true,[]);
   cs.forEach(function(c){var fns=arr(c.functions);
    d.appendChild(h("div",{class:"ctr"},[h("strong",{text:str(c.name)}),h("span",{class:"chip f",text:str(c.kind)}),pathEl(c.path,"span")]));
    var t=table(["Function","Access","Modifiers","Signature"],fns,function(fn){return [h("code",{class:"epn",text:str(fn.name)}),accChip(fn),arr(fn.modifiers).length?h("code",{text:arr(fn.modifiers).join(", ")}):h("span",{class:"muted",text:"none"}),h("code",{class:"sig",text:str(fn.signature)})]});
    t.classList.add("ctt");d.appendChild(t);
    trs(t).forEach(function(tr,i){var fn=fns[i];track(tr,str(c.name)+" "+str(fn.name)+" "+str(fn.signature)+" "+arr(fn.modifiers).join(" ")+" "+access(fn)+" "+str(c.path),d)});
    if(arr(c.events).length)d.appendChild(h("p",{class:"muted evs",text:"Events: "+arr(c.events).join(", ")}))});
   return d}));

 HK.forEach(function(x){add(x.feature,"client hook",str(x.name))});
 function reach(x){return arr(x.reaches).length>0}
 part("Client hooks",HK.length+" React hooks the app's screens use to load or change data. A hook that wraps useQuery reads; one that wraps useMutation writes."+(HK.some(reach)?" Reaches lists the GraphQL fields its calls lead to, where the index can follow the calls.":""),
  perRepo(HK).map(function(g){var rch=g.rows.some(reach);var t=table(["Hook","Does"].concat(rch?["Reaches"]:[]).concat(["Defined in"]),g.rows,function(x){return [h("code",{class:"epn",text:str(x.name)}),h("div",{},[hookChips(x.kinds),h("div",{class:"muted",text:arr(x.wraps).join(", ")})])]
    .concat(rch?[reach(x)?h("div",{class:"evl"},arr(x.reaches).map(function(f){return h("code",{text:str(f)})})):h("span",{class:"muted",text:"none found"})]:[]).concat([at(x.path,x.line)])});
   t.classList.add(rch?"hkt":"hkn");var d=grp(g.alias,g.rows.length+" hooks",g.rows.length<=30,[t]);trs(t).forEach(function(tr,i){var x=g.rows[i];track(tr,str(x.name)+" "+arr(x.kinds).join(" ")+" "+arr(x.wraps).join(" ")+" "+arr(x.reaches).join(" ")+" "+str(x.path),d)});return d}));

 part("Document plan",DC.length+" documents in "+(multi?"repositories that are":"a repository that is")+" mostly text, in file order: each document's title and its chapters.",
  perRepo(DC).map(function(g){var d=grp(g.alias,g.rows.length+" documents",g.rows.length<=12,[]);
   g.rows.forEach(function(doc){var ch=arr(doc.chapters);var b=h("div",{class:"epdoc"},[h("div",{class:"dt",text:str(doc.title)}),at(doc.path,doc.line),ch.length?h("ol",{class:"chap"},ch.map(function(c){return h("li",{text:str(c.title)})})):null]);
    d.appendChild(b);track(b,str(doc.title)+" "+str(doc.path)+" "+ch.map(function(c){return str(c.title)}).join(" "),d)});return d}));

 part("Timeline",n.timeline+" timestamps in "+TL.length+" transcripts and timestamped documents: where each part starts.",
  TL.map(function(t){var es=arr(t.entries);var d=grp(str(t.title),(multi?str(t.repo)+" · ":"")+es.length+" timestamps",TL.length<=3,[at(t.path)]);
   var ol=h("ol",{class:"tl"});es.forEach(function(e){var li=h("li",{},[h("code",{text:str(e.t)}),h("span",{text:str(e.label)})]);ol.appendChild(li);track(li,str(e.t)+" "+str(e.label)+" "+str(t.title),d)});d.appendChild(ol);return d}));

 if(!body.length)body.push(h("p",{class:"empty",text:"No entry points found in the index."}));
 else{var filter=h("input",{type:"search",id:"ep-filter",class:"filter",placeholder:"Filter entry points by name, path, signature or caller","aria-label":"Filter entry points"});
  filter.addEventListener("input",function(){var q=filter.value.trim().toLowerCase(),hit=[];rows.forEach(function(r){var ok=!q||r.hay.indexOf(q)>=0;r.el.hidden=!ok;if(ok&&hit.indexOf(r.g)<0)hit.push(r.g)});
   groups.forEach(function(d){d.hidden=!!q&&hit.indexOf(d)<0;if(q&&hit.indexOf(d)>=0)d.open=true})});
  body.unshift(filter)}
 return {body:body,by:by,n:n,any:body.length>1};
}
"##;

/// Project hub: what was indexed, the feature graph across repos (force
/// layout, clusters per repo, colour = dominant language), a detail panel,
/// cross-repo links, question pages and every feature.
const HUB_JS: &str = r##"
(function(){
 var repos=arr(M.repos),feats=arr(M.features),edges=arr(M.edges),cross=arr(M.cross),qs=arr(M.questions),T=M.totals||{};
 var EPS=entryPoints(M.entry_points,repos.map(function(r){return str(r.alias)})),epBy=EPS.by,NE=EPS.n;
 var NS="http://www.w3.org/2000/svg";
 function sv(t,a,k){var e=document.createElementNS(NS,t);a=a||{};for(var x in a)e.setAttribute(x,a[x]);(k||[]).forEach(function(c){if(c!=null)e.appendChild(typeof c==="string"?document.createTextNode(c):c)});return e}

 /* ---- stats */
 function stat(label,value){return h("div",{class:"stat"},[h("div",{class:"sv",text:value}),h("div",{class:"sl",text:label})])}
 var stats=h("div",{class:"stats"},[stat("repositories",fmt(repos.length)),stat("files",fmt(T.files)),stat("symbols",fmt(T.symbols)),stat("graph edges",fmt(T.edges)),stat("embedded chunks",fmt(T.chunks)),stat("features",fmt(T.features)),stat("cross-repo links",fmt(cross.length))].concat([["GraphQL root fields",NE.graphql],["public contract functions",NE.functions],["MCP tools",NE.mcp],["HTTP routes",NE.routes],["client hooks",NE.hooks],["documents",NE.documents],["timestamps",NE.timeline]].filter(function(x){return x[1]>0}).map(function(x){return stat(x[0],fmt(x[1]))})));

 /* ---- graph model */
 var G=feats.filter(function(f){return f.drawn}),gi={},ri={};
 G.forEach(function(f,i){gi[str(f.id)]=i});repos.forEach(function(r,i){ri[str(r.alias)]=i});
 var L=[];
 edges.forEach(function(e){var a=gi[str(e.a)],b=gi[str(e.b)];if(a!=null&&b!=null&&a!==b)L.push({a:a,b:b,w:num(e.weight),kinds:e.kinds||{},t:"i"})});
 cross.forEach(function(c){var a=gi[str(c.from)],b=gi[str(c.to)];if(a!=null&&b!=null)L.push({a:a,b:b,w:1,t:"x",c:c})});
 var xdeg=G.map(function(){return 0});L.forEach(function(l){if(l.t==="x"){xdeg[l.a]++;xdeg[l.b]++}});
 var cnt=repos.map(function(r){return G.filter(function(f){return str(f.repo)===str(r.alias)}).length});
 var est=cnt.map(function(n){return 24+14*Math.sqrt(n)}),circ=est.reduce(function(a,e){return a+2*e+26},0),RAD=Math.max(110,circ/(2*Math.PI)),acc=0;
 var C=repos.map(function(r,i){var a=-Math.PI/2+(acc+est[i]+13)/circ*2*Math.PI;acc+=2*est[i]+26;return {x:Math.cos(a)*RAD,y:Math.sin(a)*RAD}});
 var seen={};
 var P=G.map(function(f){var c=ri[str(f.repo)]||0,k=seen[c]=(seen[c]||0)+1,o=C[c]||{x:0,y:0},th=k*2.399963,rr=16*Math.sqrt(k);
  return {x:o.x+Math.cos(th)*rr,y:o.y+Math.sin(th)*rr,vx:0,vy:0,r:Math.max(5,Math.min(24,3+Math.sqrt(num(f.size))*0.9)),c:c,f:f}});
 for(var t=0;t<360;t++){var al=1-t/360;
  for(var i=0;i<P.length;i++){var p=P[i];for(var j=i+1;j<P.length;j++){var q=P[j],dx=q.x-p.x,dy=q.y-p.y,d2=dx*dx+dy*dy||0.01,d=Math.sqrt(d2),min=p.r+q.r+10;
   var rep=(p.c===q.c?700:2400)*al/d2,fx=dx/d*rep,fy=dy/d*rep;if(d<min){var ov=(min-d)/2;fx+=dx/d*ov;fy+=dy/d*ov}
   p.vx-=fx;p.vy-=fy;q.vx+=fx;q.vy+=fy}}
  L.forEach(function(l){var p=P[l.a],q=P[l.b],dx=q.x-p.x,dy=q.y-p.y,d=Math.sqrt(dx*dx+dy*dy)||0.01,rest=l.t==="x"?200:36+p.r+q.r,k=l.t==="x"?0.003:0.015*Math.min(3,1+Math.log(1+l.w)/2),f=(d-rest)*k*al,fx=dx/d*f,fy=dy/d*f;p.vx+=fx;p.vy+=fy;q.vx-=fx;q.vy-=fy});
  P.forEach(function(p){var o=C[p.c]||{x:0,y:0};p.vx+=(o.x-p.x)*0.035*al;p.vy+=(o.y-p.y)*0.035*al;var v=Math.sqrt(p.vx*p.vx+p.vy*p.vy);if(v>24){p.vx*=24/v;p.vy*=24/v}p.x+=p.vx*0.6;p.y+=p.vy*0.6;p.vx*=0.5;p.vy*=0.5});
 }
 var x0=Infinity,y0=Infinity,x1=-Infinity,y1=-Infinity;P.forEach(function(p){x0=Math.min(x0,p.x-p.r);y0=Math.min(y0,p.y-p.r);x1=Math.max(x1,p.x+p.r);y1=Math.max(y1,p.y+p.r)});
 var clusters=repos.map(function(r,ci){var m=P.filter(function(p){return p.c===ci});if(!m.length)return null;var mx=0,my=0;m.forEach(function(p){mx+=p.x;my+=p.y});mx/=m.length;my/=m.length;var R=0;m.forEach(function(p){R=Math.max(R,Math.sqrt((p.x-mx)*(p.x-mx)+(p.y-my)*(p.y-my))+p.r)});R+=16;
  x0=Math.min(x0,mx-R);x1=Math.max(x1,mx+R);y0=Math.min(y0,my-R-24);y1=Math.max(y1,my+R);return {x:mx,y:my,R:R,alias:str(r.alias),n:num(r.features),shown:m.length}});
 if(!P.length){x0=-100;y0=-100;x1=100;y1=100}
 var PAD=70,home={x:x0-PAD,y:y0-PAD/2,w:(x1-x0)+2*PAD,h:(y1-y0)+PAD},vb={x:home.x,y:home.y,w:home.w,h:home.h};

 /* ---- graph drawing */
 var svg=sv("svg",{class:"gsvg",role:"img","aria-label":"Feature graph: "+G.length+" features of "+repos.length+" repositories",preserveAspectRatio:"xMidYMid meet"});
 function setVB(){svg.setAttribute("viewBox",vb.x+" "+vb.y+" "+vb.w+" "+vb.h)}setVB();
 svg.appendChild(sv("defs",{},[sv("marker",{id:"hub-ah",viewBox:"0 0 10 10",refX:"9",refY:"5",markerWidth:"7",markerHeight:"7",orient:"auto-start-reverse"},[sv("path",{d:"M0,0L10,5L0,10z",class:"ah"})])]));
 var gc=sv("g"),ge=sv("g"),gn=sv("g"),gt=sv("g");svg.appendChild(gc);svg.appendChild(ge);svg.appendChild(gn);svg.appendChild(gt);
 clusters.forEach(function(c){if(!c)return;gc.appendChild(sv("circle",{class:"cl",cx:c.x,cy:c.y,r:c.R}));
  gt.appendChild(sv("text",{class:"cll",x:c.x,y:c.y-c.R-8,"text-anchor":"middle"},[c.alias,sv("tspan",{dx:"6"},[c.shown<c.n?c.shown+" of "+c.n+" features":c.n+" features"])]))});
 L.forEach(function(l){var p=P[l.a],q=P[l.b];
  if(l.t==="x"){var mx=(p.x+q.x)/2,my=(p.y+q.y)/2,dx=q.x-p.x,dy=q.y-p.y,d=Math.sqrt(dx*dx+dy*dy)||1,ux=dx/d,uy=dy/d,cx=mx-dy*0.18,cy=my+dx*0.18,ex=q.x-ux*(q.r+3),ey=q.y-uy*(q.r+3);
   l.el=sv("path",{class:"ge x",d:"M"+p.x+","+p.y+" Q"+cx+","+cy+" "+ex+","+ey,"marker-end":"url(#hub-ah)"});}
  else l.el=sv("line",{class:"ge i",x1:p.x,y1:p.y,x2:q.x,y2:q.y,"stroke-width":(0.8+Math.log(1+l.w)*0.45).toFixed(2)});
  ge.appendChild(l.el)});
 var labelled={};var per={};G.forEach(function(f,i){var c=P[i].c;per[c]=(per[c]||0)+1;if(per[c]<=2)labelled[i]=1});
 xdeg.map(function(d,i){return [d,i]}).filter(function(x){return x[0]>0}).sort(function(a,b){return b[0]-a[0]||a[1]-b[1]}).slice(0,6).forEach(function(x){labelled[x[1]]=1});
 P.forEach(function(p,i){var f=p.f;
  p.el=sv("g",{class:"gn l-"+lk(f.lang),transform:"translate("+p.x.toFixed(1)+","+p.y.toFixed(1)+")",tabindex:"0",role:"button","aria-label":str(f.repo)+": "+str(f.label)},[
   sv("title",{},[str(f.repo)+" · "+str(f.label)+" · "+fmt(f.size)+" symbols"+(epBy[str(f.id)]?" · holds entry points":"")]),epBy[str(f.id)]?sv("circle",{class:"ring",r:(p.r+4).toFixed(1)}):null,sv("circle",{class:"dot",r:p.r.toFixed(1)}),
   sv("text",{class:"gl"+(labelled[i]?"":" h"),x:(p.r+4).toFixed(1),y:"4"},[str(f.label)])]);
  p.el.addEventListener("mouseenter",function(){hl([i])});p.el.addEventListener("mouseleave",function(){hl(null)});
  p.el.addEventListener("focus",function(){hl([i])});p.el.addEventListener("blur",function(){hl(null)});
  p.el.addEventListener("click",function(){select(i)});
  p.el.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();select(i)}});
  gn.appendChild(p.el)});
 var findHits=null;
 function hl(list){if(!list&&findHits)list=findHits;svg.classList.toggle("focus",!!list);P.forEach(function(p){p.el.classList.remove("on")});L.forEach(function(l){l.el.classList.remove("on")});
  if(!list)return;var on={};list.forEach(function(i){on[i]=1});
  if(list.length===1)L.forEach(function(l){if(on[l.a]||on[l.b]){l.el.classList.add("on");P[l.a].el.classList.add("on");P[l.b].el.classList.add("on")}});
  list.forEach(function(i){P[i].el.classList.add("on")})}

 /* ---- pan + zoom (drag to pan; buttons or pinch / ctrl+wheel to zoom) */
 function toSvg(cx,cy){var m=svg.getScreenCTM();if(!m)return {x:vb.x+vb.w/2,y:vb.y+vb.h/2};var pt=svg.createSVGPoint();pt.x=cx;pt.y=cy;return pt.matrixTransform(m.inverse())}
 function zoom(k,cx,cy){var c=cx==null?{x:vb.x+vb.w/2,y:vb.y+vb.h/2}:toSvg(cx,cy);var w=Math.max(home.w/12,Math.min(home.w*1.5,vb.w*k)),s=w/vb.w;vb.x=c.x-(c.x-vb.x)*s;vb.y=c.y-(c.y-vb.y)*s;vb.w*=s;vb.h*=s;setVB()}
 svg.addEventListener("wheel",function(ev){if(!ev.ctrlKey&&!ev.metaKey)return;ev.preventDefault();zoom(ev.deltaY>0?1.12:1/1.12,ev.clientX,ev.clientY)},{passive:false});
 var drag=null;
 svg.addEventListener("pointerdown",function(ev){if(ev.target.closest&&ev.target.closest(".gn"))return;var m=svg.getScreenCTM();drag={x:ev.clientX,y:ev.clientY,k:m?1/m.a:1};svg.classList.add("dragging");svg.setPointerCapture&&svg.setPointerCapture(ev.pointerId)});
 svg.addEventListener("pointermove",function(ev){if(!drag)return;vb.x-=(ev.clientX-drag.x)*drag.k;vb.y-=(ev.clientY-drag.y)*drag.k;drag.x=ev.clientX;drag.y=ev.clientY;setVB()});
 function endDrag(){drag=null;svg.classList.remove("dragging")}svg.addEventListener("pointerup",endDrag);svg.addEventListener("pointercancel",endDrag);

 /* ---- detail panel */
 var panel=h("aside",{class:"panel","aria-live":"polite"}),sel=null;
 function select(i){if(sel!=null)P[sel].el.classList.remove("sel");sel=i;P[i].el.classList.add("sel");var f=G[i];panel.innerHTML="";
  panel.appendChild(repoChip(f.repo,f.lang));panel.appendChild(h("h3",{text:str(f.label)}));
  panel.appendChild(h("p",{class:"muted",text:fmt(f.size)+" symbols · "+fmt(f.files)+" files · mostly "+(str(f.lang)||"unknown")}));
  if(f.summary)panel.appendChild(h("p",{class:"psum",text:str(f.summary)}));
  if(arr(f.top).length){panel.appendChild(h("h4",{text:"Central symbols"}));panel.appendChild(chips(f.top,"f"))}
  var ep=epBy[str(f.id)];
  if(ep){panel.appendChild(h("h4",{text:"Entry points here"}));var shown=ep.slice(0,14);
   panel.appendChild(h("ul",{class:"conn"},shown.map(function(x){return h("li",{},[h("code",{text:str(x.code)}),h("span",{class:"muted",text:"  "+str(x.k)})])}).concat(ep.length>shown.length?[h("li",{class:"muted",text:"and "+(ep.length-shown.length)+" more in the Entry points section"})]:[])))}
  var uses=[],used=[],xo=[],xi=[];L.forEach(function(l){if(l.t==="i"){if(l.a===i)uses.push(l);else if(l.b===i)used.push(l)}else{if(l.a===i)xo.push(l);else if(l.b===i)xi.push(l)}});
  function kinds(l){var k=l.kinds||{};return Object.keys(k).sort().map(function(x){return k[x]+" "+x}).join(", ")}
  function item(j,extra,ev){var b=h("button",{type:"button",text:str(G[j].label)});b.addEventListener("click",function(){select(j)});
   return h("li",{},[b,extra?h("span",{class:"muted",text:"  "+extra}):null,ev&&ev.length?h("span",{class:"ev",text:ev.join("\n")}):null])}
  function list(title,ls,fn){if(!ls.length)return;panel.appendChild(h("h4",{text:title}));panel.appendChild(h("ul",{class:"conn"},ls.map(fn)))}
  uses.sort(function(a,b){return b.w-a.w});used.sort(function(a,b){return b.w-a.w});
  list("Calls into other repositories",xo,function(l){return item(l.b,str(l.c.to_repo)+" · "+str(l.c.kind),arr(l.c.evidence))});
  list("Called from other repositories",xi,function(l){return item(l.a,str(l.c.from_repo)+" · "+str(l.c.kind),arr(l.c.evidence))});
  list("Uses (same repository)",uses,function(l){return item(l.b,kinds(l))});
  list("Used by (same repository)",used,function(l){return item(l.a,kinds(l))});
  if(!uses.length&&!used.length&&!xo.length&&!xi.length)panel.appendChild(h("p",{class:"empty",text:"No edge to another drawn feature."}));
  if(f.page)panel.appendChild(h("p",{class:"plink"},[link("Open the feature page",str(f.page)),h("span",{class:"muted",text:"  (generated next to the repository)"})]));
 }
 var start=0;xdeg.forEach(function(d,i){if(d>xdeg[start])start=i});
 if(!P.length)panel.appendChild(h("p",{class:"empty",text:"No features indexed yet."}));

 /* ---- toolbar + legend */
 var find=h("input",{type:"search",id:"hub-find",placeholder:"Find a feature or symbol","aria-label":"Find a feature or symbol in the graph"});
 find.addEventListener("input",function(){var q=find.value.trim().toLowerCase();if(q.length<2){findHits=null;hl(null);return}
  findHits=[];G.forEach(function(f,i){var hay=(str(f.label)+" "+arr(f.top).join(" ")+" "+str(f.summary)+" "+str(f.repo)).toLowerCase();if(hay.indexOf(q)>=0)findHits.push(i)});hl(findHits.length?findHits:[])});
 find.addEventListener("keydown",function(ev){if(ev.key==="Enter"&&findHits&&findHits.length)select(findHits[0])});
 function btn(label,aria,fn){var b=h("button",{type:"button",class:"btn","aria-label":aria,text:label});b.addEventListener("click",fn);return b}
 var tools=h("div",{class:"gtools"},[find,btn("+","Zoom in",function(){zoom(1/1.25)}),btn("−","Zoom out",function(){zoom(1.25)}),btn("Reset view","Reset the view",function(){vb={x:home.x,y:home.y,w:home.w,h:home.h};setVB()})]);
 var present={};G.forEach(function(f){present[lk(f.lang)]=1});
 var legend=h("div",{class:"legend hubleg"},Object.keys(present).sort(function(a,b){return (LANGS.indexOf(a)+99)%99-(LANGS.indexOf(b)+99)%99}).map(function(l){return h("span",{class:"lgl"},[sw(l),l])}).concat([h("span",{class:"lg-i",text:"uses (same repository; thicker = more edges)"}),h("span",{class:"lg-x",text:"cross-repo link, arrow points at the provider"}),h("span",{class:"lgl",text:"circle area ∝ symbols in the feature"}),h("span",{class:"lgl"},[h("i",{class:"ringsw"}),"holds entry points (API field, contract function, MCP tool, route or client hook)"])]));
 var graphBox=h("div",{class:"hub"},[h("div",{class:"gbox"},[tools,svg]),panel]);

 /* ---- tables */
 var indexed=table(["Repository","Indexed commit","Files","Symbols","Edges","Chunks","Features","Languages",""],repos,function(r){
  return [h("div",{},[h("strong",{text:str(r.alias)}),pathEl(r.path)]),
   r.commit?h("code",{class:"sha",text:str(r.commit)}):h("span",{class:"muted",text:"no commit"}),
   h("span",{class:"n",text:fmt(r.files)}),h("span",{class:"n",text:fmt(r.symbols)}),h("span",{class:"n",text:fmt(r.edges)}),h("span",{class:"n",text:fmt(r.chunks)}),h("span",{class:"n",text:fmt(r.features)}),
   h("div",{class:"langs"},arr(r.languages).map(function(p){return h("span",{class:"lchip"},[sw(p[0]),str(p[0])+" "+fmt(p[1])])})),
   r.site?link("feature site",str(r.site)):""]});
 var byId={};feats.forEach(function(f){byId[str(f.id)]=f});
 function side(repo,label,id){var f=byId[str(id)];return h("div",{},[repoChip(repo,f?f.lang:null),h("div",{class:"xl",text:str(label)})])}
 var crossT=table(["Consumer","Provider","Kind","Evidence"],cross,function(c){return [side(c.from_repo,c.from_label,c.from),side(c.to_repo,c.to_label,c.to),h("span",{class:"chip k",text:str(c.kind)}),h("div",{class:"evl"},arr(c.evidence).map(function(e){return h("code",{text:str(e)})}))]});
 var qcards=qs.map(function(q){return h("div",{class:"card"},[h("h3",{},[link(str(q.title),str(q.href))]),h("div",{},arr(q.repos).map(function(a){return repoChip(a)}))])});

 var filter=h("input",{type:"search",id:"hub-filter",class:"filter",placeholder:"Filter features by name, symbol or summary","aria-label":"Filter features"});
 var groups=repos.map(function(r){var fs=feats.filter(function(f){return str(f.repo)===str(r.alias)});
  var rows=[];var t=table(["Feature","Language","Symbols","Files","Summary"],fs,function(f){return [link(str(f.label),str(f.page)),h("span",{class:"lchip"},[sw(f.lang),str(f.lang)||"—"]),h("span",{class:"n",text:fmt(f.size)}),h("span",{class:"n",text:fmt(f.files)}),h("span",{class:"fsum",text:str(f.summary)})]});
  var trs=t.querySelectorAll("tbody tr");fs.forEach(function(f,i){rows.push({tr:trs[i],hay:(str(f.label)+" "+arr(f.top).join(" ")+" "+str(f.summary)).toLowerCase()})});
  var cnt=h("span",{text:fs.length+" features"});
  var d=h("details",{class:"repo"},[h("summary",{},[str(r.alias),cnt]),t]);return {d:d,rows:rows,cnt:cnt,n:fs.length}});
 filter.addEventListener("input",function(){var q=filter.value.trim().toLowerCase();groups.forEach(function(g){var m=0;g.rows.forEach(function(r){var ok=!q||r.hay.indexOf(q)>=0;r.tr.hidden=!ok;if(ok)m++});
  g.cnt.textContent=q?m+" of "+g.n+" features match":g.n+" features";g.d.hidden=!!q&&m===0;if(q)g.d.open=m>0})});

 var body=[stats];
 body.push(sec("Feature graph",G.length+" of "+feats.length+" features drawn: the "+"largest in each repository plus every feature with a cross-repo link. Click a circle to see what it contains and what it connects to; drag to pan, pinch or use the buttons to zoom.",[graphBox,legend]));
 body.push(sec("Entry points","Where someone comes in from outside: the API and tools a client or an agent calls, the contract functions anyone can call on-chain, the hooks the app's screens use, and for text, the chapters and timestamps a reader starts from.",EPS.body));
 body.push(sec("What was indexed","One row per repository: the commit that was indexed and what the index holds. Symbols are functions, types, contracts, GraphQL fields and operations, and Markdown sections.",[indexed]));
 body.push(sec("Cross-repo links",cross.length+" consumer → provider links found from the index: a package one repository imports from another, a client call to a route another repository registers, or a GraphQL operation calling a field in the schema another repository owns.",cross.length?[crossT].concat(M.correlations?[h("p",{class:"muted"},["Full report with "+fmt(M.intra_pairs)+" same-repository feature pairs: ",link("correlations page",str(M.correlations))])]:[]):[]));
 if(qs.length)body.push(sec("Questions answered across repositories",qs.length+" question page(s): each traces one feature through every repository it touches.",[h("div",{class:"grid"},qcards)]));
 body.push(sec("All features","Every feature of every repository, largest first. Each links to its own page.",[filter].concat(groups.map(function(g){return g.d}))));
 mount(hero("Project map",str(M.project),"Every repository in this project, indexed by Chaos Substrate: what each one holds and how its features connect."),body);
 if(P.length)select(start);
})();
"##;

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
        PageKind::ProjectFeature => "project feature",
        PageKind::SiteIndex => "site index",
        PageKind::SitePage => "feature page",
        PageKind::Correlations => "correlations",
        PageKind::ProjectIndex => "project map",
    }
}

fn kind_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature => FEATURE_JS,
        PageKind::ProjectFeature => PROJECT_FEATURE_JS,
        PageKind::SiteIndex => SITE_INDEX_JS,
        PageKind::SitePage => SITE_PAGE_JS,
        PageKind::Correlations => CORRELATIONS_JS,
        PageKind::ProjectIndex => HUB_JS,
    }
}

/// The entry-point renderer, only on pages that list entry points.
fn kind_entry_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::SiteIndex | PageKind::ProjectIndex => ENTRY_JS,
        PageKind::Feature
        | PageKind::ProjectFeature
        | PageKind::SitePage
        | PageKind::Correlations => "",
    }
}

/// The feature-map helpers, only on pages that draw a map.
fn kind_map_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature | PageKind::ProjectFeature => MAP_JS,
        PageKind::SiteIndex
        | PageKind::SitePage
        | PageKind::Correlations
        | PageKind::ProjectIndex => "",
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
        .replace(
            "__JS__",
            &format!(
                "{REPORT_JS}\n{}{}{}",
                kind_map_js(kind),
                kind_entry_js(kind),
                kind_js(kind)
            ),
        )
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
    fn feature_map_only_on_feature_pages() {
        let m = json!({"content_hash":"h","question":"q"});
        for (kind, map) in [
            (PageKind::Feature, true),
            (PageKind::ProjectFeature, true),
            (PageKind::SiteIndex, false),
            (PageKind::SitePage, false),
            (PageKind::Correlations, false),
            (PageKind::ProjectIndex, false),
        ] {
            let html = render_page(kind, &m);
            assert_eq!(html.contains("function featureMap("), map, "{kind:?}");
        }
        assert!(render_page(PageKind::ProjectFeature, &m).contains("Cross-repo links"));
        for (kind, entry) in [
            (PageKind::Feature, false),
            (PageKind::SitePage, false),
            (PageKind::SiteIndex, true),
            (PageKind::ProjectIndex, true),
        ] {
            let html = render_page(kind, &m);
            assert_eq!(html.contains("function entryPoints("), entry, "{kind:?}");
        }
    }

    #[test]
    fn manifest_round_trip_survives_script_breakout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.html");
        let m = json!({"content_hash":"h","label":"</script><script>alert(1)</script><!--",
            "summary":"a & b < c","members":[{"name":"f","kind":"fn","path":"src/a.rs"}]});
        for kind in [
            PageKind::ProjectFeature,
            PageKind::SiteIndex,
            PageKind::SitePage,
            PageKind::Correlations,
            PageKind::ProjectIndex,
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

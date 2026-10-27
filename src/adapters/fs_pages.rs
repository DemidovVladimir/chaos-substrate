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
 body.push(sec("User surfaces","Routes, commands, env vars and GraphQL fields the feature exposes.",surf.length?[table(["Kind","Surface","Where"],surf,function(s){return [h("span",{class:"chip",text:str(s.kind)}),h("code",{text:str(s.value||s.name)}),str(s.path)]})]:[]));
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
        PageKind::ProjectFeature => "project feature",
        PageKind::SiteIndex => "site index",
        PageKind::SitePage => "feature page",
        PageKind::Correlations => "correlations",
    }
}

fn kind_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature => FEATURE_JS,
        PageKind::ProjectFeature => PROJECT_FEATURE_JS,
        PageKind::SiteIndex => SITE_INDEX_JS,
        PageKind::SitePage => SITE_PAGE_JS,
        PageKind::Correlations => CORRELATIONS_JS,
    }
}

/// The feature-map helpers, only on pages that draw a map.
fn kind_map_js(kind: PageKind) -> &'static str {
    match kind {
        PageKind::Feature | PageKind::ProjectFeature => MAP_JS,
        PageKind::SiteIndex | PageKind::SitePage | PageKind::Correlations => "",
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
            &format!("{REPORT_JS}\n{}{}", kind_map_js(kind), kind_js(kind)),
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
        ] {
            let html = render_page(kind, &m);
            assert_eq!(html.contains("function featureMap("), map, "{kind:?}");
        }
        assert!(render_page(PageKind::ProjectFeature, &m).contains("Cross-repo links"));
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

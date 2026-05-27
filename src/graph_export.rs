use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct GraphExport {
    pub repository: GraphRepository,
    pub nodes: Vec<GraphExportNode>,
    pub edges: Vec<GraphExportEdge>,
}

#[derive(Debug, Serialize)]
pub struct GraphRepository {
    pub id: Uuid,
    pub name: String,
    pub root_path: String,
    pub current_commit_sha: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GraphExportNode {
    pub id: Uuid,
    pub kind: String,
    pub stable_id: String,
    pub name: String,
    pub file_path: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub chunk_count: i64,
    pub metadata: Value,
}

#[derive(Debug, Serialize)]
pub struct GraphExportEdge {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    pub kind: String,
    pub cost: f64,
    pub confidence: f64,
    pub metadata: Value,
}

pub fn write_graph_html(path: &Path, graph: &GraphExport) -> Result<()> {
    let json = serde_json::to_string(graph)?;
    let html = render_graph_html(&json);
    std::fs::write(path, html)?;
    Ok(())
}

fn render_graph_html(graph_json: &str) -> String {
    GRAPH_HTML.replace("__GRAPH_DATA__", &escape_script_json(graph_json))
}

fn escape_script_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

const GRAPH_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Chaos Substrate Graph</title>
<style>
* { box-sizing: border-box; }
body {
  margin: 0;
  height: 100vh;
  overflow: hidden;
  font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  color: #172033;
  background: #f4f6fb;
}
.app {
  display: grid;
  grid-template-columns: 336px 1fr 380px;
  height: 100vh;
}
.panel {
  overflow: auto;
  padding: 18px;
  border-right: 1px solid #dbe2ee;
  background: #ffffff;
  box-shadow: 0 18px 48px rgba(36, 49, 79, .08);
}
.details {
  border-right: 0;
  border-left: 1px solid #dbe2ee;
}
.brand {
  display: grid;
  gap: 4px;
  padding-bottom: 14px;
  border-bottom: 1px solid #e6ebf3;
}
h1, h2 {
  margin: 0 0 12px;
  font-size: 16px;
  line-height: 1.25;
}
h2 { margin-top: 18px; }
.meta, .hint, .stat {
  color: #657086;
  font-size: 12px;
  line-height: 1.45;
}
.stats {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
  margin: 16px 0;
}
.stat {
  padding: 10px;
  border: 1px solid #e1e7f0;
  background: linear-gradient(180deg, #ffffff, #f7f9fd);
  border-radius: 8px;
}
.stat strong {
  display: block;
  color: #172033;
  font-size: 20px;
}
input[type="search"] {
  width: 100%;
  height: 38px;
  padding: 7px 10px;
  border: 1px solid #cbd5e1;
  border-radius: 8px;
  background: #ffffff;
  color: #172033;
  font: inherit;
}
input[type="search"]:focus {
  outline: 2px solid #8fd9d2;
  border-color: #0f9f95;
}
label {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 7px 0;
  font-size: 13px;
}
button {
  height: 34px;
  border: 1px solid #cbd5e1;
  border-radius: 8px;
  background: #ffffff;
  color: #172033;
  font: inherit;
  cursor: pointer;
}
button:hover { background: #eef6ff; border-color: #8bb8e8; }
.toolbar {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
  margin-top: 12px;
}
.kind-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  flex: 0 0 auto;
}
.edge-row {
  display: grid;
  grid-template-columns: 10px 1fr auto;
  gap: 8px;
  align-items: center;
  margin: 8px 0;
  color: #36435a;
  font-size: 13px;
}
.edge-line {
  width: 10px;
  height: 2px;
  border-radius: 2px;
}
.edge-count {
  color: #657086;
  font-variant-numeric: tabular-nums;
}
.render-status {
  margin-top: 10px;
  padding: 8px 10px;
  border: 1px solid #e1e7f0;
  border-radius: 8px;
  background: #f7f9fd;
  color: #4b5870;
  font-size: 12px;
  line-height: 1.4;
}
.stage {
  position: relative;
  overflow: hidden;
  background:
    radial-gradient(circle at 20% 20%, rgba(15, 159, 149, .14), transparent 28%),
    radial-gradient(circle at 82% 18%, rgba(230, 92, 73, .12), transparent 24%),
    linear-gradient(135deg, rgba(23, 32, 51, .05) 25%, transparent 25%) 0 0 / 28px 28px,
    #f8fafc;
}
canvas {
  display: block;
  width: 100%;
  height: 100%;
}
.badge {
  display: inline-block;
  padding: 3px 8px;
  border-radius: 999px;
  background: #e6fbf8;
  color: #08756f;
  font-size: 12px;
  margin-bottom: 8px;
}
.kv {
  margin: 8px 0;
  font-size: 13px;
  line-height: 1.45;
  overflow-wrap: anywhere;
}
.kv strong {
  display: block;
  color: #657086;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0;
}
pre {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  padding: 10px;
  border: 1px solid #e1e7f0;
  border-radius: 8px;
  background: #f7f9fd;
  font-size: 12px;
  line-height: 1.35;
}
@media (max-width: 1000px) {
  .app { grid-template-columns: 260px 1fr; }
  .details { display: none; }
}
</style>
</head>
<body>
<div class="app">
  <aside class="panel">
    <div class="brand">
      <h1 id="title">Knowledge Graph</h1>
      <div class="meta" id="repoMeta"></div>
    </div>
    <div class="stats">
      <div class="stat"><strong id="nodeCount">0</strong>nodes</div>
      <div class="stat"><strong id="edgeCount">0</strong>edges</div>
    </div>
    <input id="search" type="search" placeholder="Filter nodes">
    <div class="toolbar">
      <button id="fit">Fit</button>
      <button id="reset">Reset</button>
    </div>
    <div class="render-status" id="renderMeta"></div>
    <h2>Kinds</h2>
    <div id="kinds"></div>
    <h2>Edges</h2>
    <div id="edgeKinds"></div>
  </aside>
  <main class="stage">
    <canvas id="graph"></canvas>
  </main>
  <aside class="panel details">
    <h1>Selection</h1>
    <div id="details" class="hint">No node selected.</div>
  </aside>
</div>
<script>
const graphData = __GRAPH_DATA__;
const canvas = document.getElementById('graph');
const ctx = canvas.getContext('2d');
const state = {
  scale: 1,
  offsetX: 0,
  offsetY: 0,
  pointer: null,
  selected: null,
  hover: null,
  search: '',
  enabledKinds: new Set(),
  draggingNode: null,
  panning: false,
  lastX: 0,
  lastY: 0,
  renderLimit: graphData.nodes.length > 3000 ? 2500 : 10000,
  renderNodes: [],
  renderNodeIds: new Set(),
  renderQueued: false
};
const palette = {
  repository: '#174ea6',
  file: '#64748b',
  module: '#0f9f95',
  function: '#e65c49',
  method: '#c24132',
  struct: '#a142f4',
  enum: '#9334e6',
  trait: '#7b1fa2',
  impl: '#6d4c41',
  test: '#188038',
  dependency: '#d97706',
  concept: '#2563eb',
  script: '#0891b2',
  type_alias: '#4f46e5',
  deployment_resource: '#16a34a',
  topic: '#111827'
};
const edgePalette = {
  contains: '#64748b',
  imports: '#0f9f95',
  calls: '#e65c49',
  depends_on: '#d97706',
  defines: '#2563eb',
  configures: '#16a34a',
  deploys: '#7c3aed'
};
const kindPriority = {
  repository: 0,
  file: 1,
  module: 2,
  function: 3,
  method: 4,
  struct: 5,
  enum: 6,
  trait: 7,
  impl: 8,
  test: 9,
  deployment_resource: 10,
  type_alias: 11,
  script: 12,
  dependency: 13,
  concept: 14,
  topic: -1
};
const rawNodes = graphData.nodes.map((node, index) => ({
  ...node,
  x: 0,
  y: 0,
  pinned: false,
  visible: true,
  priority: (kindPriority[node.kind] ?? 50) * 100000 - Math.min(99999, node.chunk_count || 0) + index / 100000
}));
const topicByName = new Map();
for (const node of rawNodes) {
  const topicName = inferTopic(node);
  node.topic = topicName;
  if (!topicByName.has(topicName)) {
    topicByName.set(topicName, {
      id: `topic:${topicName}`,
      kind: 'topic',
      stable_id: `topic:${topicName}`,
      name: topicName,
      file_path: null,
      line_start: null,
      line_end: null,
      chunk_count: 0,
      metadata: { inferred: true, topic: topicName },
      x: 0,
      y: 0,
      pinned: false,
      visible: true,
      priority: -1000000,
      topic: topicName,
      children: []
    });
  }
  topicByName.get(topicName).children.push(node);
}
const topicNodes = [...topicByName.values()].sort((a, b) => b.children.length - a.children.length || a.name.localeCompare(b.name));
const nodes = [...topicNodes, ...rawNodes];
const nodeById = new Map(nodes.map(node => [node.id, node]));
const edges = graphData.edges
  .map(edge => ({...edge, sourceNode: nodeById.get(edge.source), targetNode: nodeById.get(edge.target)}))
  .filter(edge => edge.sourceNode && edge.targetNode);
const kinds = [...new Set(nodes.map(node => node.kind))].sort();
kinds.forEach(kind => state.enabledKinds.add(kind));

document.getElementById('title').textContent = graphData.repository.name + ' Graph';
document.getElementById('repoMeta').textContent = graphData.repository.root_path;
document.getElementById('nodeCount').textContent = String(rawNodes.length);
document.getElementById('edgeCount').textContent = String(edges.length);

const kindBox = document.getElementById('kinds');
for (const kind of kinds) {
  const label = document.createElement('label');
  const check = document.createElement('input');
  const dot = document.createElement('span');
  check.type = 'checkbox';
  check.checked = !['concept', 'file'].includes(kind);
  if (!check.checked) state.enabledKinds.delete(kind);
  check.addEventListener('change', () => {
    if (check.checked) state.enabledKinds.add(kind);
    else state.enabledKinds.delete(kind);
    applyFilters();
    requestRender();
  });
  dot.className = 'kind-dot';
  dot.style.background = colorFor(kind);
  label.append(check, dot, document.createTextNode(kind));
  kindBox.append(label);
}
const edgeKindBox = document.getElementById('edgeKinds');
const edgeCounts = edges.reduce((counts, edge) => {
  counts.set(edge.kind, (counts.get(edge.kind) || 0) + 1);
  return counts;
}, new Map());
for (const [kind, count] of [...edgeCounts.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
  const row = document.createElement('div');
  const line = document.createElement('span');
  const name = document.createElement('span');
  const total = document.createElement('span');
  row.className = 'edge-row';
  line.className = 'edge-line';
  line.style.background = edgeColorFor(kind);
  name.textContent = kind;
  total.className = 'edge-count';
  total.textContent = String(count);
  row.append(line, name, total);
  edgeKindBox.append(row);
}
document.getElementById('search').addEventListener('input', event => {
  state.search = event.target.value.toLowerCase().trim();
  applyFilters();
  fitVisible();
  requestRender();
});
document.getElementById('fit').addEventListener('click', fitVisible);
document.getElementById('reset').addEventListener('click', () => {
  for (const node of nodes) node.pinned = false;
  state.scale = 1;
  state.offsetX = canvas.width / 2;
  state.offsetY = canvas.height / 2;
  requestRender();
});

function colorFor(kind) {
  return palette[kind] || '#3c4043';
}

function edgeColorFor(kind) {
  return edgePalette[kind] || '#64748b';
}

function radiusFor(node) {
  if (node.kind === 'topic') return 16 + Math.min(18, Math.sqrt(node.children.length));
  const base = node.kind === 'repository' ? 11 : node.kind === 'file' ? 7 : 5;
  return base + Math.min(7, Math.sqrt(Math.max(node.chunk_count, 0)));
}

function layoutNodes() {
  const ringRadius = Math.max(900, topicNodes.length * 95);
  topicNodes.forEach((topic, topicIndex) => {
    const angle = -Math.PI / 2 + topicIndex * 2 * Math.PI / Math.max(topicNodes.length, 1);
    topic.x = Math.cos(angle) * ringRadius;
    topic.y = Math.sin(angle) * ringRadius;
    const children = topic.children.sort((a, b) => a.priority - b.priority || a.name.localeCompare(b.name));
    children.forEach((node, index) => {
      const childAngle = angle + (index * 2.399963229728653);
      const radius = 90 + Math.sqrt(index + 1) * 19;
      const jitter = stableJitter(node.id);
      node.x = topic.x + Math.cos(childAngle) * radius + jitter.x;
      node.y = topic.y + Math.sin(childAngle) * radius + jitter.y;
    });
  });
}

function inferTopic(node) {
  const path = node.file_path
    || node.metadata?.file
    || (node.stable_id || '').split(':')[0]
    || '';
  const parts = path.split('/').filter(Boolean);
  if (!parts.length) {
    if (node.kind === 'dependency') return 'external imports';
    return 'workspace';
  }
  if ((parts[0] === 'apps' || parts[0] === 'packages' || parts[0] === 'crates') && parts[1]) return `${parts[0]}: ${parts[1]}`;
  if ((parts[0] === 'services' || parts[0] === 'lambdas' || parts[0] === 'lambda') && parts[1]) return `service: ${parts[1]}`;
  if (parts[0] === 'docs') return parts[1] ? `docs: ${parts[1]}` : 'docs';
  if (parts[0] === 'src') return parts[1] ? `src: ${parts[1]}` : 'src';
  if (parts[0] === 'lib' || parts[0] === 'bin') return 'library';
  return parts[0];
}

function stableJitter(value) {
  let hash = 0;
  for (let i = 0; i < value.length; i++) {
    hash = (hash * 31 + value.charCodeAt(i)) | 0;
  }
  return {
    x: ((hash & 255) / 255 - 0.5) * 8,
    y: (((hash >> 8) & 255) / 255 - 0.5) * 8
  };
}

function applyFilters() {
  const candidates = [];
  for (const node of rawNodes) {
    const haystack = [node.name, node.kind, node.stable_id, node.file_path || ''].join(' ').toLowerCase();
    if (state.enabledKinds.has(node.kind) && (!state.search || haystack.includes(state.search))) {
      candidates.push(node);
    }
  }
  candidates.sort((a, b) => a.priority - b.priority);
  const render = [];
  const renderedTopics = new Set();
  const byTopic = new Map();
  for (const node of candidates) {
    if (!byTopic.has(node.topic)) byTopic.set(node.topic, []);
    byTopic.get(node.topic).push(node);
  }
  const activeTopics = topicNodes
    .filter(topic => byTopic.has(topic.name))
    .sort((a, b) => (byTopic.get(b.name)?.length || 0) - (byTopic.get(a.name)?.length || 0));
  for (const topic of activeTopics) {
    render.push(topic);
    renderedTopics.add(topic.name);
  }
  if (state.search) {
    for (const node of candidates.slice(0, Math.max(0, state.renderLimit - render.length))) {
      render.push(node);
    }
  } else {
    const perTopic = Math.max(12, Math.floor((state.renderLimit - render.length) / Math.max(activeTopics.length, 1)));
    for (const topic of activeTopics) {
      const group = byTopic.get(topic.name) || [];
      for (const node of group.slice(0, perTopic)) {
        render.push(node);
      }
    }
    if (render.length < state.renderLimit) {
      const rendered = new Set(render.map(node => node.id));
      for (const node of candidates) {
        if (render.length >= state.renderLimit) break;
        if (!rendered.has(node.id)) render.push(node);
      }
    }
  }
  state.renderNodes = render.slice(0, state.renderLimit + renderedTopics.size);
  state.renderNodeIds = new Set(state.renderNodes.map(node => node.id));
  for (const node of nodes) {
    node.visible = state.renderNodeIds.has(node.id);
  }
  updateRenderMeta(candidates.length);
}

function updateRenderMeta(totalMatches) {
  const renderMeta = document.getElementById('renderMeta');
  const hidden = Math.max(0, totalMatches - state.renderNodes.length);
  renderMeta.textContent = hidden
    ? `Rendering ${state.renderNodes.length.toLocaleString()} of ${totalMatches.toLocaleString()} matching nodes. Search or toggle kinds to narrow the graph.`
    : `Rendering ${state.renderNodes.length.toLocaleString()} matching nodes.`;
}

function resize() {
  const box = canvas.parentElement.getBoundingClientRect();
  const ratio = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.floor(box.width * ratio));
  canvas.height = Math.max(1, Math.floor(box.height * ratio));
  canvas.style.width = box.width + 'px';
  canvas.style.height = box.height + 'px';
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  if (!state.offsetX && !state.offsetY) {
    state.offsetX = box.width / 2;
    state.offsetY = box.height / 2;
  }
  requestRender();
}
window.addEventListener('resize', resize);
resize();
layoutNodes();
applyFilters();

function screenToWorld(x, y) {
  const ratio = window.devicePixelRatio || 1;
  return {
    x: (x - state.offsetX) / state.scale,
    y: (y - state.offsetY) / state.scale
  };
}

function worldToScreen(x, y) {
  return {
    x: x * state.scale + state.offsetX,
    y: y * state.scale + state.offsetY
  };
}

function hitTest(x, y) {
  const world = screenToWorld(x, y);
  for (let i = state.renderNodes.length - 1; i >= 0; i--) {
    const node = state.renderNodes[i];
    const dx = node.x - world.x;
    const dy = node.y - world.y;
    if (Math.sqrt(dx * dx + dy * dy) <= radiusFor(node) + 4 / state.scale) return node;
  }
  return null;
}

canvas.addEventListener('pointerdown', event => {
  const node = hitTest(event.offsetX, event.offsetY);
  state.lastX = event.offsetX;
  state.lastY = event.offsetY;
  if (node) {
    state.draggingNode = node;
    node.pinned = true;
    selectNode(node);
  } else {
    state.panning = true;
  }
  canvas.setPointerCapture(event.pointerId);
});
canvas.addEventListener('pointermove', event => {
  const nextHover = hitTest(event.offsetX, event.offsetY);
  const hoverChanged = nextHover !== state.hover;
  state.hover = nextHover;
  if (state.draggingNode) {
    const world = screenToWorld(event.offsetX, event.offsetY);
    state.draggingNode.x = world.x;
    state.draggingNode.y = world.y;
    requestRender();
  } else if (state.panning) {
    state.offsetX += event.offsetX - state.lastX;
    state.offsetY += event.offsetY - state.lastY;
    requestRender();
  } else if (hoverChanged) {
    requestRender();
  }
  state.lastX = event.offsetX;
  state.lastY = event.offsetY;
});
canvas.addEventListener('pointerup', event => {
  state.draggingNode = null;
  state.panning = false;
  canvas.releasePointerCapture(event.pointerId);
});
canvas.addEventListener('wheel', event => {
  event.preventDefault();
  const before = screenToWorld(event.offsetX, event.offsetY);
  const factor = event.deltaY < 0 ? 1.12 : 0.89;
  state.scale = Math.max(0.08, Math.min(5, state.scale * factor));
  state.offsetX = event.offsetX - before.x * state.scale;
  state.offsetY = event.offsetY - before.y * state.scale;
  requestRender();
}, {passive: false});

function selectNode(node) {
  state.selected = node;
  const details = document.getElementById('details');
  if (node.kind === 'topic') {
    details.innerHTML = `
      <span class="badge">topic</span>
      <div class="kv"><strong>Name</strong>${escapeHtml(node.name)}</div>
      <div class="kv"><strong>Related Nodes</strong>${node.children.length.toLocaleString()}</div>
      <h2>Metadata</h2>
      <pre>${escapeHtml(JSON.stringify(node.metadata, null, 2))}</pre>
    `;
    requestRender();
    return;
  }
  const lines = node.line_start ? `${node.line_start}${node.line_end ? '-' + node.line_end : ''}` : '';
  details.innerHTML = `
    <span class="badge">${escapeHtml(node.kind)}</span>
    <div class="kv"><strong>Name</strong>${escapeHtml(node.name)}</div>
    <div class="kv"><strong>Stable ID</strong>${escapeHtml(node.stable_id)}</div>
    <div class="kv"><strong>File</strong>${escapeHtml(node.file_path || '')}</div>
    <div class="kv"><strong>Lines</strong>${escapeHtml(lines)}</div>
    <div class="kv"><strong>Chunks</strong>${node.chunk_count}</div>
    <div class="kv"><strong>ID</strong>${escapeHtml(node.id)}</div>
    <h2>Metadata</h2>
    <pre>${escapeHtml(JSON.stringify(node.metadata, null, 2))}</pre>
  `;
  requestRender();
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, ch => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;'
  }[ch]));
}

function requestRender() {
  if (state.renderQueued) return;
  state.renderQueued = true;
  requestAnimationFrame(() => {
    state.renderQueued = false;
    draw();
  });
}

function isOnScreen(point, margin, width, height) {
  return point.x >= -margin && point.x <= width + margin && point.y >= -margin && point.y <= height + margin;
}

function draw() {
  const ratio = window.devicePixelRatio || 1;
  const w = canvas.width / ratio;
  const h = canvas.height / ratio;
  ctx.clearRect(0, 0, w, h);
  ctx.lineCap = 'round';
  for (const node of state.renderNodes) {
    if (node.kind === 'topic') continue;
    const topic = topicByName.get(node.topic);
    if (!topic || !topic.visible || !state.renderNodeIds.has(topic.id)) continue;
    const source = worldToScreen(topic.x, topic.y);
    const target = worldToScreen(node.x, node.y);
    if (!isOnScreen(source, 60, w, h) && !isOnScreen(target, 60, w, h)) continue;
    ctx.globalAlpha = 0.13;
    ctx.strokeStyle = '#111827';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(source.x, source.y);
    ctx.lineTo(target.x, target.y);
    ctx.stroke();
  }
  for (const edge of edges) {
    if (!edge.sourceNode.visible || !edge.targetNode.visible) continue;
    const source = worldToScreen(edge.sourceNode.x, edge.sourceNode.y);
    const target = worldToScreen(edge.targetNode.x, edge.targetNode.y);
    if (!isOnScreen(source, 40, w, h) && !isOnScreen(target, 40, w, h)) continue;
    ctx.globalAlpha = edge.kind === 'contains' ? 0.38 : 0.22;
    ctx.strokeStyle = edgeColorFor(edge.kind);
    ctx.lineWidth = edge.kind === 'contains' ? 1.2 : 1;
    ctx.beginPath();
    ctx.moveTo(source.x, source.y);
    ctx.lineTo(target.x, target.y);
    ctx.stroke();
  }
  ctx.globalAlpha = 1;
  for (const node of state.renderNodes) {
    const point = worldToScreen(node.x, node.y);
    const radius = radiusFor(node) * state.scale;
    if (!isOnScreen(point, Math.max(20, radius + 80), w, h)) continue;
    ctx.fillStyle = colorFor(node.kind);
    ctx.strokeStyle = state.selected === node ? '#111827' : state.hover === node ? '#202124' : '#ffffff';
    ctx.lineWidth = state.selected === node ? 3 : 2;
    ctx.beginPath();
    ctx.arc(point.x, point.y, Math.max(3, radius), 0, Math.PI * 2);
    ctx.fill();
    ctx.stroke();
    if (node.kind === 'topic' || (state.scale > 0.45 && (radius > 6 || state.hover === node || state.selected === node))) {
      ctx.fillStyle = '#172033';
      ctx.font = node.kind === 'topic'
        ? '700 13px ui-sans-serif, system-ui, sans-serif'
        : '12px ui-sans-serif, system-ui, sans-serif';
      ctx.fillText(node.name.slice(0, 42), point.x + radius + 4, point.y + 4);
    }
  }
}

function fitVisible() {
  const visibleNodes = state.renderNodes;
  if (!visibleNodes.length) return;
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const node of visibleNodes) {
    minX = Math.min(minX, node.x);
    minY = Math.min(minY, node.y);
    maxX = Math.max(maxX, node.x);
    maxY = Math.max(maxY, node.y);
  }
  const ratio = window.devicePixelRatio || 1;
  const w = canvas.width / ratio;
  const h = canvas.height / ratio;
  const graphW = Math.max(1, maxX - minX);
  const graphH = Math.max(1, maxY - minY);
  state.scale = Math.max(0.08, Math.min(3, Math.min((w - 80) / graphW, (h - 80) / graphH)));
  state.offsetX = w / 2 - (minX + graphW / 2) * state.scale;
  state.offsetY = h / 2 - (minY + graphH / 2) * state.scale;
  requestRender();
}

fitVisible();
requestRender();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::escape_script_json;

    #[test]
    fn escapes_json_for_script_context() {
        let escaped = escape_script_json(r#"{"name":"</script><img src=x>","amp":"&"}"#);
        assert!(!escaped.contains("</script>"));
        assert!(escaped.contains("\\u003c/script\\u003e"));
        assert!(escaped.contains("\\u0026"));
    }
}

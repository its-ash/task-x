import type { GraphView } from './protocol.js';

export type NodeShape = 'rect' | 'round' | 'stadium' | 'diamond' | 'cylinder' | 'subroutine' | 'trapezoid';
export type EdgeStyle = 'solid' | 'dashed' | 'thick' | 'open';
export type FlowDirection = 'LR' | 'TD';

export interface FlowNode {
  id: string;
  label: string;
  shape: NodeShape;
  x: number;
  y: number;
  color?: string;
}

export interface FlowEdge {
  id: string;
  from: string;
  to: string;
  label: string;
  style: EdgeStyle;
}

export interface FlowGraph {
  direction: FlowDirection;
  nodes: FlowNode[];
  edges: FlowEdge[];
}

const NODE_W = 180;
const NODE_H = 56;
const GAP_X = 72;
const GAP_Y = 64;

const SHAPE_SYNTAX: Record<NodeShape, [string, string]> = {
  rect: ['[', ']'],
  round: ['(', ')'],
  stadium: ['([', '])'],
  diamond: ['{', '}'],
  cylinder: ['[(', ')]'],
  subroutine: ['[[', ']]'],
  trapezoid: ['[/', '/]'],
};

function sanitizeId(raw: string, fallback: string): string {
  const clean = raw.replace(/[^a-zA-Z0-9_]/g, '_').replace(/^(\d)/, '_$1');
  return clean || fallback;
}

function escapeLabel(text: string): string {
  return text.replace(/"/g, '&quot;').replace(/\r?\n/g, ' ').trim();
}

function unquote(text: string): string {
  const t = text.trim();
  if ((t.startsWith('"') && t.endsWith('"')) || (t.startsWith("'") && t.endsWith("'"))) {
    return t.slice(1, -1).replace(/&quot;/g, '"');
  }
  return t.replace(/&quot;/g, '"');
}

function parseNodeToken(token: string): { id: string; label?: string; shape?: NodeShape } | null {
  const s = token.trim();
  if (!s) return null;
  const patterns: [RegExp, NodeShape][] = [
    [/^([a-zA-Z_][\w-]*)\s*\(\[([\s\S]+)\]\)$/, 'stadium'],
    [/^([a-zA-Z_][\w-]*)\s*\[\(([\s\S]+)\)\]$/, 'cylinder'],
    [/^([a-zA-Z_][\w-]*)\s*\[\[([\s\S]+)\]\]$/, 'subroutine'],
    [/^([a-zA-Z_][\w-]*)\s*\[\/([\s\S]+)\/\]$/, 'trapezoid'],
    [/^([a-zA-Z_][\w-]*)\s*\{([\s\S]+)\}$/, 'diamond'],
    [/^([a-zA-Z_][\w-]*)\s*\(([\s\S]+)\)$/, 'round'],
    [/^([a-zA-Z_][\w-]*)\s*\[([\s\S]+)\]$/, 'rect'],
  ];
  for (const [re, shape] of patterns) {
    const m = re.exec(s);
    if (m) return { id: m[1], label: unquote(m[2]), shape };
  }
  const bare = /^([a-zA-Z_][\w-]*)$/.exec(s);
  return bare ? { id: bare[1] } : null;
}

export function layoutFlow(graph: FlowGraph): FlowGraph {
  const nodes = graph.nodes.map((n) => ({ ...n }));
  const edges = graph.edges.map((e) => ({ ...e }));
  if (!nodes.length) return { direction: graph.direction, nodes, edges };

  const indeg = new Map<string, number>();
  const adj = new Map<string, string[]>();
  for (const n of nodes) {
    indeg.set(n.id, 0);
    adj.set(n.id, []);
  }
  for (const e of edges) {
    if (!adj.has(e.from) || !indeg.has(e.to) || e.from === e.to) continue;
    adj.get(e.from)!.push(e.to);
    indeg.set(e.to, (indeg.get(e.to) ?? 0) + 1);
  }

  const layer = new Map<string, number>();
  const q: string[] = [];
  for (const n of nodes) if ((indeg.get(n.id) ?? 0) === 0) q.push(n.id);
  if (!q.length) q.push(nodes[0].id);

  const visited = new Set<string>();
  while (q.length) {
    const id = q.shift()!;
    if (visited.has(id)) continue;
    visited.add(id);
    const cur = layer.get(id) ?? 0;
    layer.set(id, cur);
    for (const next of adj.get(id) ?? []) {
      const nextLayer = Math.max(layer.get(next) ?? 0, cur + 1);
      layer.set(next, nextLayer);
      const deg = (indeg.get(next) ?? 1) - 1;
      indeg.set(next, deg);
      if (deg <= 0 && !visited.has(next)) q.push(next);
    }
  }
  for (const n of nodes) {
    if (!layer.has(n.id)) layer.set(n.id, 0);
  }

  const groups = new Map<number, FlowNode[]>();
  for (const n of nodes) {
    const l = layer.get(n.id) ?? 0;
    const list = groups.get(l) ?? [];
    list.push(n);
    groups.set(l, list);
  }

  const layers = [...groups.keys()].sort((a, b) => a - b);
  const horizontal = graph.direction === 'LR';
  for (let li = 0; li < layers.length; li++) {
    const group = groups.get(layers[li])!;
    for (let gi = 0; gi < group.length; gi++) {
      const node = group[gi];
      if (horizontal) {
        node.x = 60 + li * (NODE_W + GAP_X);
        node.y = 60 + gi * (NODE_H + GAP_Y);
      } else {
        node.x = 60 + gi * (NODE_W + GAP_X);
        node.y = 60 + li * (NODE_H + GAP_Y);
      }
    }
  }
  return { direction: graph.direction, nodes, edges };
}

function parseSequence(lines: string[]): FlowGraph {
  const nodesMap = new Map<string, FlowNode>();
  const edges: FlowEdge[] = [];
  const ensure = (id: string, label?: string) => {
    const existing = nodesMap.get(id);
    if (existing) {
      if (label && existing.label === id) existing.label = label;
      return;
    }
    nodesMap.set(id, { id, label: label || id, shape: 'round', x: 0, y: 0 });
  };

  let edgeIdx = 0;
  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith('%%') || line.startsWith('sequenceDiagram')) continue;
    const part = /^(?:participant|actor)\s+([a-zA-Z_][\w-]*)(?:\s+as\s+(.+))?$/.exec(line);
    if (part) {
      ensure(part[1], part[2]?.trim());
      continue;
    }
    const msg = /^([a-zA-Z_][\w-]*?)\s*(-->>|->>|-->|->|--x|-x|--\)|-\))\s*([a-zA-Z_][\w-]*)\s*:\s*(.*)$/.exec(line);
    if (msg) {
      const [, from, arrow, to, label] = msg;
      ensure(from);
      ensure(to);
      edges.push({
        id: `e${++edgeIdx}`,
        from,
        to,
        label: label.trim(),
        style: arrow.startsWith('--') ? 'dashed' : 'solid',
      });
    }
  }
  return layoutFlow({ direction: 'LR', nodes: [...nodesMap.values()], edges });
}

export function parseMermaid(src: string): FlowGraph {
  const lines = src.replace(/\r\n?/g, '\n').split('\n');
  const first = lines.find((l) => l.trim() && !l.trim().startsWith('%%'))?.trim() ?? '';
  if (/^sequenceDiagram\b/.test(first)) return parseSequence(lines);

  let direction: FlowDirection = 'LR';
  const dirMatch = /^(?:flowchart|graph)\s+(TD|TB|BT|LR|RL)\b/i.exec(first);
  if (dirMatch) {
    const d = dirMatch[1].toUpperCase();
    direction = d === 'LR' || d === 'RL' ? 'LR' : 'TD';
  }

  const nodesMap = new Map<string, FlowNode>();
  const edges: FlowEdge[] = [];
  const upsert = (tok: { id: string; label?: string; shape?: NodeShape }) => {
    const cur = nodesMap.get(tok.id);
    if (cur) {
      if (tok.label) cur.label = tok.label;
      if (tok.shape) cur.shape = tok.shape;
      return;
    }
    nodesMap.set(tok.id, { id: tok.id, label: tok.label || tok.id, shape: tok.shape || 'rect', x: 0, y: 0 });
  };

  let edgeIdx = 0;
  const edgeRe = /^(.+?)\s*(-->|-\.->|==>|---|--[^>]+-->|-\.[^>]+-\.->|==[^>]+==>)\s*(?:\|([^|]*)\|\s*)?(.+)$/;

  for (const raw of lines) {
    const line = raw.trim().replace(/;$/, '');
    if (!line || line.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|classDef|class|style|linkStyle|click|direction)\b/i.test(line)) {
      continue;
    }
    const em = edgeRe.exec(line);
    if (em) {
      const left = parseNodeToken(em[1]);
      const right = parseNodeToken(em[4]);
      if (left && right) {
        upsert(left);
        upsert(right);
        const rawArrow = em[2];
        let label = (em[3] ?? '').trim();
        let style: EdgeStyle = 'solid';
        if (rawArrow.includes('-.')) {
          style = 'dashed';
          if (!label) label = rawArrow.replace(/^-\.\s*/, '').replace(/\s*-\.->$/, '').trim();
        } else if (rawArrow.includes('==')) {
          style = 'thick';
          if (!label) label = rawArrow.replace(/^==\s*/, '').replace(/\s*==>$/, '').trim();
        } else if (rawArrow === '---') {
          style = 'open';
        } else if (!label && rawArrow.length > 3) {
          label = rawArrow.replace(/^--\s*/, '').replace(/\s*-->$/, '').trim();
        }
        edges.push({ id: `e${++edgeIdx}`, from: left.id, to: right.id, label: unquote(label), style });
        continue;
      }
    }
    const single = parseNodeToken(line);
    if (single) upsert(single);
  }

  return layoutFlow({ direction, nodes: [...nodesMap.values()], edges });
}

export function serializeMermaid(graph: FlowGraph): string {
  const dir = graph.direction === 'TD' ? 'TD' : 'LR';
  const lines: string[] = [`flowchart ${dir}`];
  const usedIds = new Map<string, string>();
  const seen = new Set<string>();

  for (let i = 0; i < graph.nodes.length; i++) {
    const n = graph.nodes[i];
    let id = sanitizeId(n.id, `N${i + 1}`);
    let suffix = 2;
    while (seen.has(id)) id = `${sanitizeId(n.id, `N${i + 1}`)}_${suffix++}`;
    seen.add(id);
    usedIds.set(n.id, id);
    const [open, close] = SHAPE_SYNTAX[n.shape] ?? SHAPE_SYNTAX.rect;
    const label = escapeLabel(n.label || id);
    lines.push(`    ${id}${open}"${label}"${close}`);
  }

  for (const e of graph.edges) {
    const from = usedIds.get(e.from);
    const to = usedIds.get(e.to);
    if (!from || !to) continue;
    const arrow = e.style === 'dashed' ? '-.->' : e.style === 'thick' ? '==>' : e.style === 'open' ? '---' : '-->';
    const label = escapeLabel(e.label);
    lines.push(label ? `    ${from} ${arrow}|"${label}"| ${to}` : `    ${from} ${arrow} ${to}`);
  }

  for (const n of graph.nodes) {
    if (!n.color) continue;
    const id = usedIds.get(n.id);
    if (id && /^#[0-9a-fA-F]{3,8}$/.test(n.color)) {
      lines.push(`    style ${id} stroke:${n.color},stroke-width:2px`);
    }
  }

  return lines.join('\n');
}

export function graphDataToFlow(data: GraphView, direction: FlowDirection = 'LR', maxNodes = 24): FlowGraph {
  const picked = data.nodes.slice(0, maxNodes);
  const indexMap = new Map<number, string>();
  const nodes: FlowNode[] = picked.map((n, i) => {
    const id = sanitizeId(n.id, `M${i + 1}`);
    indexMap.set(i, id);
    const shape: NodeShape = n.inCycle ? 'diamond' : n.files > 1 ? 'subroutine' : 'round';
    return { id, label: n.id, shape, x: 0, y: 0 };
  });

  const seenEdges = new Set<string>();
  const edges: FlowEdge[] = [];
  let edgeIdx = 0;
  for (const e of data.edges) {
    const from = indexMap.get(e.from);
    const to = indexMap.get(e.to);
    if (!from || !to || from === to) continue;
    const key = `${from}->${to}:${e.kind}`;
    if (seenEdges.has(key)) continue;
    seenEdges.add(key);
    edges.push({
      id: `e${++edgeIdx}`,
      from,
      to,
      label: e.weight > 1 ? `${e.kind} (${e.weight})` : e.kind,
      style: e.kind === 'reference' ? 'dashed' : 'solid',
    });
  }

  return layoutFlow({ direction, nodes, edges });
}

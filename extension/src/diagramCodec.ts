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
  w?: number;
  color?: string;
  group?: string;
}

export interface FlowGroup {
  id: string;
  label: string;
  x?: number;
  y?: number;
  w?: number;
  h?: number;
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
  groups?: FlowGroup[];
}

const MIN_NODE_W = 168;
const MAX_NODE_W = 280;
const NODE_H = 56;
const GAP_X = 84;
const GAP_Y = 72;

export function nodeWidth(n: Pick<FlowNode, 'id' | 'label' | 'shape'>): number {
  const len = (n.label || n.id).length;
  const pad = n.shape === 'diamond' ? 76 : n.shape === 'subroutine' ? 56 : 44;
  return Math.max(MIN_NODE_W, Math.min(MAX_NODE_W, Math.round(len * 7.4 + pad)));
}

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
  const nodes = graph.nodes.map((n) => ({ ...n, w: nodeWidth(n) }));
  const edges = graph.edges.map((e) => ({ ...e }));
  if (!nodes.length) return { direction: graph.direction, nodes, edges };

  const nodeIds = new Set(nodes.map((n) => n.id));
  const rawAdj = new Map<string, string[]>(nodes.map((n) => [n.id, []]));
  const rawIndeg = new Map<string, number>(nodes.map((n) => [n.id, 0]));
  for (const e of edges) {
    if (!nodeIds.has(e.from) || !nodeIds.has(e.to) || e.from === e.to) continue;
    rawAdj.get(e.from)!.push(e.to);
    rawIndeg.set(e.to, (rawIndeg.get(e.to) ?? 0) + 1);
  }

  // Step 1: Break cycles via DFS from sources (then lowest in-degree) so DAG edges always flow forward.
  const state = new Map<string, 0 | 1 | 2>();
  const dagEdges: { from: string; to: string }[] = [];
  const dfs = (u: string) => {
    state.set(u, 1);
    for (const v of rawAdj.get(u) ?? []) {
      const st = state.get(v) ?? 0;
      if (st === 1) continue; // Back-edge (cycle): skip for layering.
      dagEdges.push({ from: u, to: v });
      if (st === 0) dfs(v);
    }
    state.set(u, 2);
  };
  const orderedSeeds = [...nodes].sort((a, b) => (rawIndeg.get(a.id) ?? 0) - (rawIndeg.get(b.id) ?? 0));
  for (const n of orderedSeeds) if ((state.get(n.id) ?? 0) === 0) dfs(n.id);

  // Step 2: Longest-path layering so every step appears strictly after all of its prerequisites.
  const layer = new Map<string, number>(nodes.map((n) => [n.id, 0]));
  for (let i = 0; i < nodes.length; i++) {
    let changed = false;
    for (const e of dagEdges) {
      const next = (layer.get(e.from) ?? 0) + 1;
      if (next > (layer.get(e.to) ?? 0)) {
        layer.set(e.to, next);
        changed = true;
      }
    }
    if (!changed) break;
  }

  const groups = new Map<number, FlowNode[]>();
  for (const n of nodes) {
    const l = layer.get(n.id) ?? 0;
    const list = groups.get(l) ?? [];
    list.push(n);
    groups.set(l, list);
  }

  const preds = new Map<string, string[]>(nodes.map((n) => [n.id, []]));
  const succs = new Map<string, string[]>(nodes.map((n) => [n.id, []]));
  for (const e of dagEdges) {
    preds.get(e.to)!.push(e.from);
    succs.get(e.from)!.push(e.to);
  }

  // Step 3: 4-pass bidirectional barycenter ordering to minimize edge crossings.
  const layers = [...groups.keys()].sort((a, b) => a - b);
  const posInLayer = new Map<string, number>();
  for (const l of layers) groups.get(l)!.forEach((n, i) => posInLayer.set(n.id, i));
  const sortLayer = (l: number, neighbors: Map<string, string[]>) => {
    const group = groups.get(l)!;
    group.sort((a, b) => {
      const avg = (id: string) => {
        const ns = (neighbors.get(id) ?? []).filter((p) => posInLayer.has(p));
        return ns.length ? ns.reduce((s, p) => s + posInLayer.get(p)!, 0) / ns.length : posInLayer.get(id)!;
      };
      return avg(a.id) - avg(b.id);
    });
    group.forEach((n, i) => posInLayer.set(n.id, i));
  };
  for (let pass = 0; pass < 4; pass++) {
    for (let i = 1; i < layers.length; i++) sortLayer(layers[i], preds);
    for (let i = layers.length - 2; i >= 0; i--) sortLayer(layers[i], succs);
  }

  // Step 4: Centered placement with breathable spacing.
  const horizontal = graph.direction === 'LR';
  const layerSpans = layers.map((l) => {
    const g = groups.get(l)!;
    return horizontal
      ? g.length * NODE_H + Math.max(0, g.length - 1) * GAP_Y
      : g.reduce((s, n) => s + (n.w ?? MIN_NODE_W), 0) + Math.max(0, g.length - 1) * GAP_X;
  });
  const maxSpan = Math.max(0, ...layerSpans);
  const layerDepths = layers.map((l) => {
    const g = groups.get(l)!;
    return horizontal ? Math.max(...g.map((n) => n.w ?? MIN_NODE_W)) : NODE_H;
  });

  let primary = 60;
  for (let li = 0; li < layers.length; li++) {
    const group = groups.get(layers[li])!;
    let cross = 60 + Math.round((maxSpan - layerSpans[li]) / 2);
    for (const node of group) {
      const w = node.w ?? MIN_NODE_W;
      if (horizontal) {
        node.x = primary + Math.round((layerDepths[li] - w) / 2);
        node.y = cross;
        cross += NODE_H + GAP_Y;
      } else {
        node.x = cross;
        node.y = primary;
        cross += w + GAP_X;
      }
    }
    primary += layerDepths[li] + (horizontal ? GAP_X : GAP_Y);
  }
  const padX = 36;
  const padY = 32;
  for (let iter = 0; iter < 50; iter++) {
    let moved = false;
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const a = nodes[i], b = nodes[j];
        const wa = a.w ?? MIN_NODE_W, wb = b.w ?? MIN_NODE_W;
        const cax = a.x + wa / 2, cay = a.y + NODE_H / 2;
        const cbx = b.x + wb / 2, cby = b.y + NODE_H / 2;
        const ox = (wa + wb) / 2 + padX - Math.abs(cax - cbx);
        const oy = NODE_H + padY - Math.abs(cay - cby);
        if (ox > 0 && oy > 0) {
          moved = true;
          const preferY = horizontal ? oy <= ox * 1.4 : oy < ox * 0.7;
          if (preferY) {
            const shift = Math.ceil(oy / 2);
            if (cay <= cby) { a.y -= shift; b.y += shift; }
            else { b.y -= shift; a.y += shift; }
          } else {
            const shift = Math.ceil(ox / 2);
            if (cax <= cbx) { a.x -= shift; b.x += shift; }
            else { b.x -= shift; a.x += shift; }
          }
        }
      }
    }
    if (!moved) break;
  }
  const minX = Math.min(...nodes.map((n) => n.x));
  const minY = Math.min(...nodes.map((n) => n.y));
  if (minX < 48 || minY < 48) {
    const dx = minX < 48 ? 48 - minX : 0;
    const dy = minY < 48 ? 48 - minY : 0;
    for (const n of nodes) { n.x = Math.round(n.x + dx); n.y = Math.round(n.y + dy); }
  }
  return { direction: graph.direction, nodes, edges, ...(graph.groups?.length ? { groups: graph.groups } : {}) };
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
  const groupsMap = new Map<string, FlowGroup>();
  const groupStack: string[] = [];
  const edges: FlowEdge[] = [];
  const upsert = (tok: { id: string; label?: string; shape?: NodeShape }) => {
    const curGroup = groupStack[groupStack.length - 1];
    const cur = nodesMap.get(tok.id);
    if (cur) {
      if (tok.label) cur.label = tok.label;
      if (tok.shape) cur.shape = tok.shape;
      if (curGroup && !cur.group) cur.group = curGroup;
      return;
    }
    nodesMap.set(tok.id, {
      id: tok.id,
      label: tok.label || tok.id,
      shape: tok.shape || 'rect',
      x: 0,
      y: 0,
      ...(curGroup ? { group: curGroup } : {}),
    });
  };

  let edgeIdx = 0;
  let groupSeq = 1;
  const arrowSplitRe = /\s*(-->|-\.->|==>|---|--[^-\n>]+-->|-\.[^.\n>]+-\.->|==[^=\n>]+==>)\s*(?:\|([^|]*)\|\s*)?/;

  const parseEndpointList = (part: string): { id: string; label?: string; shape?: NodeShape }[] =>
    part
      .split(/\s*&\s*/)
      .map((p) => parseNodeToken(p))
      .filter((t): t is { id: string; label?: string; shape?: NodeShape } => !!t);

  for (const raw of lines) {
    const line = raw.trim().replace(/;$/, '');
    if (!line || line.startsWith('%%')) continue;
    const subMatch = /^subgraph\s+(.+)$/i.exec(line);
    if (subMatch) {
      const rest = subMatch[1].trim();
      const bracketed = /^([a-zA-Z_][\w-]*)\s*\[([\s\S]+)\]$/.exec(rest);
      const id = bracketed ? bracketed[1] : sanitizeId(unquote(rest), `Group_${groupSeq++}`);
      const label = bracketed ? unquote(bracketed[2]) : unquote(rest);
      if (!groupsMap.has(id)) groupsMap.set(id, { id, label });
      groupStack.push(id);
      continue;
    }
    if (/^end$/i.test(line)) {
      groupStack.pop();
      continue;
    }
    if (/^(?:flowchart|graph|classDef|class|style|linkStyle|click|direction)\b/i.test(line)) {
      continue;
    }

    // Parse chained edges (`A --> B --> C`) and `&` fan-in/fan-out (`A & B --> C`).
    let rest = line;
    let prev: { id: string; label?: string; shape?: NodeShape }[] | null = null;
    let matchedAnyEdge = false;
    while (true) {
      const m = arrowSplitRe.exec(rest);
      if (!m || m.index === undefined) break;
      const leftStr = rest.slice(0, m.index).trim();
      const rawArrow = m[1];
      let label = (m[2] ?? '').trim();
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
      const leftList = prev ?? parseEndpointList(leftStr);
      rest = rest.slice(m.index + m[0].length).trim();
      const nextArrow = arrowSplitRe.exec(rest);
      const rightStr = nextArrow && nextArrow.index !== undefined ? rest.slice(0, nextArrow.index).trim() : rest;
      const rightList = parseEndpointList(rightStr);
      if (!leftList.length || !rightList.length) break;
      matchedAnyEdge = true;
      for (const a of leftList) upsert(a);
      for (const b of rightList) upsert(b);
      for (const a of leftList) {
        for (const b of rightList) {
          edges.push({ id: `e${++edgeIdx}`, from: a.id, to: b.id, label: unquote(label), style });
        }
      }
      prev = rightList;
      if (!nextArrow || nextArrow.index === undefined) break;
      rest = rest.slice(nextArrow.index);
    }
    if (matchedAnyEdge) continue;

    const single = parseNodeToken(line);
    if (single) upsert(single);
  }

  const groups = [...groupsMap.values()];
  return layoutFlow({ direction, nodes: [...nodesMap.values()], edges, ...(groups.length ? { groups } : {}) });
}

export function serializeMermaid(graph: FlowGraph): string {
  const dir = graph.direction === 'TD' ? 'TD' : 'LR';
  const lines: string[] = [`flowchart ${dir}`];
  const usedIds = new Map<string, string>();
  const seen = new Set<string>();

  const emitNode = (n: FlowNode, i: number, indent: string) => {
    let id = sanitizeId(n.id, `N${i + 1}`);
    let suffix = 2;
    while (seen.has(id)) id = `${sanitizeId(n.id, `N${i + 1}`)}_${suffix++}`;
    seen.add(id);
    usedIds.set(n.id, id);
    const [open, close] = SHAPE_SYNTAX[n.shape] ?? SHAPE_SYNTAX.rect;
    const label = escapeLabel(n.label || id);
    lines.push(`${indent}${id}${open}"${label}"${close}`);
  };

  if (graph.groups?.length) {
    const grouped = new Set<string>();
    for (const grp of graph.groups) {
      const members = graph.nodes.filter((n) => n.group === grp.id);
      if (!members.length) continue;
      const gid = sanitizeId(grp.id, 'Group');
      lines.push(`    subgraph ${gid}["${escapeLabel(grp.label || gid)}"]`);
      for (const n of members) {
        grouped.add(n.id);
        emitNode(n, graph.nodes.indexOf(n), '        ');
      }
      lines.push('    end');
    }
    graph.nodes.forEach((n, i) => {
      if (!grouped.has(n.id)) emitNode(n, i, '    ');
    });
  } else {
    for (let i = 0; i < graph.nodes.length; i++) {
      emitNode(graph.nodes[i], i, '    ');
    }
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

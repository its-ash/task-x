import assert from 'node:assert/strict';
import { test } from 'node:test';
import { graphDataToFlow, parseMermaid, serializeMermaid } from '../diagramCodec.js';
import type { GraphView } from '../protocol.js';

test('parses flowchart nodes, shapes, edges, and round-trips to Mermaid', () => {
  const src = `flowchart LR
    Start(["Start"]) --> API["API Gateway"]
    API --> Check{"Auth OK?"}
    Check -->|"Yes"| DB[("Postgres")]
    Check -.->|"No"| Err["401 Unauthorized"]`;
  const g = parseMermaid(src);
  assert.equal(g.direction, 'LR');
  assert.equal(g.nodes.length, 5);
  assert.equal(g.edges.length, 4);
  assert.equal(g.nodes.find((n) => n.id === 'Start')?.shape, 'stadium');
  assert.equal(g.nodes.find((n) => n.id === 'Check')?.shape, 'diamond');
  assert.equal(g.nodes.find((n) => n.id === 'DB')?.shape, 'cylinder');
  assert.equal(g.edges.find((e) => e.to === 'Err')?.style, 'dashed');
  const out = serializeMermaid(g);
  assert.match(out, /^flowchart LR/);
  assert.match(out, /Check -\.\->\|"No"\| Err/);
});

test('parses sequenceDiagram participants and arrows into editable flow graph', () => {
  const src = `sequenceDiagram
    participant MT5 as Broker MT5 Server
    participant EXE as FXOpsProBridge64.exe
    participant HUB as Rust Control Plane
    MT5->>EXE: 1. Start Gateway
    EXE-->>HUB: 2. Telemetry`;
  const g = parseMermaid(src);
  assert.equal(g.nodes.length, 3);
  assert.equal(g.nodes[0].label, 'Broker MT5 Server');
  assert.equal(g.edges.length, 2);
  assert.equal(g.edges[0].style, 'solid');
  assert.equal(g.edges[1].style, 'dashed');
});

test('converts workspace GraphView into an auto-generated flow diagram', () => {
  const data: GraphView = {
    nodes: [
      { id: 'extension/src', language: 'typescript', symbols: 40, files: 10, degree: 2, inCycle: false },
      { id: 'runtime/src', language: 'rust', symbols: 80, files: 15, degree: 1, inCycle: false },
    ],
    edges: [{ from: 0, to: 1, weight: 3, kind: 'import' }],
    total: 2,
    stats: { files: 25, edges: 3, importEdges: 3, referenceEdges: 0, modules: 2, cycles: 0, orphans: 0, hubs: [], importCycles: [], buildMs: 10 },
  };
  const g = graphDataToFlow(data, 'TD');
  assert.equal(g.direction, 'TD');
  assert.equal(g.nodes.length, 2);
  assert.equal(g.edges.length, 1);
  assert.equal(g.edges[0].label, 'import (3)');
  const mmd = serializeMermaid(g);
  assert.match(mmd, /extension_src\[\["extension\/src"\]\]/);
});

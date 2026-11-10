import assert from 'node:assert/strict';
import { test } from 'node:test';
import { EXPORT_SIZES, exportedFiles, isWorkspaceSvg, svgPathFrom } from '../vector.js';

test('a finished render_svg call names the SVG to preview', () => {
  const raw = JSON.stringify({ viewBox: true, path: 'assets/logo/acme.svg', warnings: [] });
  assert.equal(svgPathFrom('render_svg', true, raw), 'assets/logo/acme.svg');
  assert.equal(svgPathFrom('designer › render_svg', true, raw), 'assets/logo/acme.svg', 'a subagent call');
  // Claude Code calls it over MCP: the JSON arrives inside a text block, with escaped quotes.
  const mcp = JSON.stringify([{ type: 'text', text: raw }]);
  assert.equal(svgPathFrom('mcp__task-x__render_svg', true, mcp), 'assets/logo/acme.svg');
});

test('no preview for failures, other tools, or paths outside the workspace', () => {
  const raw = (path: string) => JSON.stringify({ path });
  assert.equal(svgPathFrom('render_svg', false, raw('a.svg')), undefined);
  assert.equal(svgPathFrom('read_file', true, raw('a.svg')), undefined);
  assert.equal(svgPathFrom('render_svg', true, raw('/etc/x.svg')), undefined);
  assert.equal(svgPathFrom('render_svg', true, raw('../x.svg')), undefined);
  assert.equal(svgPathFrom('render_svg', true, raw('C:\\x\\a.svg')), undefined);
  assert.equal(svgPathFrom('render_svg', true, raw('notes.txt')), undefined);
  assert.equal(svgPathFrom('render_svg', true, 'not json'), undefined);
  assert.ok(isWorkspaceSvg('a/b/logo.SVG'));
  assert.ok(!isWorkspaceSvg('a/../../logo.svg'));
});

test('export sizes and results', () => {
  assert.deepEqual(EXPORT_SIZES.filter((s) => s.picked).map((s) => s.size), [512]);
  assert.ok(EXPORT_SIZES.every((s) => s.size >= 16 && s.size <= 4096), 'within the runtime limits');
  assert.deepEqual(exportedFiles({ files: [{ path: 'a-16.png', size: 16 }, { path: 'a-32.png' }] }), ['a-16.png', 'a-32.png']);
  assert.deepEqual(exportedFiles(null), []);
});

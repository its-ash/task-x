// Copies vendored browser assets into media/ so webviews load them locally: the VS Code codicon
// font and the highlight.js browser bundle (common languages) for code blocks.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const copy = (from, to, files) => {
  mkdirSync(to, { recursive: true });
  for (const f of files) copyFileSync(join(from, f), join(to, f));
};
copy(join(root, 'node_modules', '@vscode', 'codicons', 'dist'), join(root, 'media', 'codicons'), ['codicon.css', 'codicon.ttf']);
copy(join(root, 'node_modules', '@highlightjs', 'cdn-assets'), join(root, 'media', 'vendor'), ['highlight.min.js', 'LICENSE']);

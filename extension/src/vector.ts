/**
 * SVG previews in the chat: which tool results show one, and the PNG sizes offered for export.
 * No `vscode` import, so plain Node can test it.
 */

/** Sizes offered by "Export PNG…": favicon (16, 32, 48), Apple touch (180), Android (192, 512), large. */
export const EXPORT_SIZES: readonly { size: number; label: string; picked: boolean }[] = [
  { size: 16, label: 'Favicon', picked: false },
  { size: 32, label: 'Favicon', picked: false },
  { size: 48, label: 'Favicon', picked: false },
  { size: 180, label: 'Apple touch icon', picked: false },
  { size: 192, label: 'Android icon', picked: false },
  { size: 512, label: 'App icon', picked: true },
  { size: 1024, label: 'Large', picked: false },
  { size: 2048, label: 'Print', picked: false },
];

/** Long side of the chat preview, in pixels. */
export const PREVIEW_SIZE = 320;

/**
 * The workspace-relative SVG a finished `render_svg` call drew, or undefined. Works for the
 * agent's result (JSON) and for the same tool called by Claude Code over MCP (JSON inside text).
 * Absolute paths and paths that climb out of the workspace get no preview.
 */
export function svgPathFrom(tool: string, ok: boolean, raw: string): string | undefined {
  const base = (tool.split(' › ').pop() ?? tool).replace(/^mcp__task-x__/, '');
  if (!ok || base !== 'render_svg') return undefined;
  const m = /\\?"path\\?"\s*:\s*\\?"((?:[^"\\]|\\\\)+?)\\?"/.exec(raw);
  const path = m?.[1].replace(/\\\\/g, '\\');
  return path && isWorkspaceSvg(path) ? path : undefined;
}

/** A relative `.svg` path that stays inside the workspace. */
export function isWorkspaceSvg(path: string): boolean {
  if (!/\.svg$/i.test(path) || path.length > 1000) return false;
  if (path.startsWith('/') || path.startsWith('~') || /^[A-Za-z]:[\\/]/.test(path) || path.startsWith('\\\\')) return false;
  return !path.split(/[\\/]/).includes('..');
}

/** The exported files, from an `export_png` result. */
export function exportedFiles(result: unknown): string[] {
  const files = (result as { files?: { path?: unknown }[] } | null)?.files;
  return Array.isArray(files) ? files.map((f) => f?.path).filter((p): p is string => typeof p === 'string') : [];
}

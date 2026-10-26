# Workspace context

The runtime builds a context engine for the open folder. It sends only small, relevant parts of the repository to the model.

## Index

- The walker obeys `.gitignore`, `.ignore`, `.git/info/exclude`, and `.anvilignore`. It does not follow symlinks.
- **Hidden folders are skipped.** Any directory whose name starts with `.` is pruned before the walk descends into it, so package caches and build output such as `.pnpm-store`, `.nuxt`, `.next`, `.turbo`, `.nx`, `.cache`, `.venv`, and `.git` never reach the index. The exception is the project's own tooling folders, which the agent reads to follow the repo's conventions: `.github`, `.vscode`, `.devcontainer`, `.claude`, `.task-x`, `.anvil`, `.storybook`, `.changeset`, `.husky`. Hidden *files* are always indexed, since `.gitignore`, `.env`, `.eslintrc.json`, and `.prettierrc` are real project files.
- Default exclusions: dependency folders (`node_modules`, `vendor`, `bower_components`, `.venv`), build output (`target`, `dist`, `build`, `out`, `.next`, `.nuxt`, `.output`), package-manager caches (`.pnpm-store`, `.pnpm`, `.npm`, `.cache`, `.yarn`, `.turbo`, `.nx`), lock files, minified files, and source maps. Add more with the `task-x.index.exclude` setting.
- Secret-looking files (`.env*`, private keys and certificates, `.npmrc`, `.netrc`, `.pypirc`, `.ssh/`, `.aws/`, credential JSON files, Terraform state) are indexed like other files. Their edit diffs show no content.
- Binary files (a NUL byte in the first 8 KiB, or not UTF-8) and files larger than 1 MiB stay in the file list, but their content is not stored.
- Limits: 200,000 files and 256 MiB of stored text.
- Symbols come from Tree-sitter `tags.scm` queries for Rust, TypeScript/TSX, JavaScript, Python, and Go.
- Incremental updates: VS Code sends `workspace/filesChanged` (with a 300 ms debounce). A full scan keeps the parsed data of files with the same size and modification time.

## Project detection

The runtime reads manifests up to two folders deep (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `pom.xml`, Gradle files, and others). It finds package managers from lock files, frameworks and test frameworks from dependencies, and build, test, lint, format, and typecheck commands. Commands are argument arrays with a working folder, never shell strings.

## Instruction precedence

task-x sends one instruction file: `CLAUDE.md` at the workspace root, else `.github/copilot-instructions.md`. In a task-x project, `CLAUDE.md` is a symlink to `.github/copilot-instructions.md`, so the two are the same file and go to the model once. An empty file is skipped. No other files are read: no `AGENTS.md`, no nested or path-specific files, and no user-level files.

The Claude Code engine reads `CLAUDE.md` itself, so task-x passes it the Copilot file only when there is no `CLAUDE.md`. A symlink that points outside the workspace is not followed. The file is limited to 64 KiB.

Instructions are guidance only. They cannot grant permissions or change security policy.

## Retrieval

1. The query is split into terms. Identifiers are also split at camelCase and snake_case boundaries, and common words are dropped.
2. Each file gets a score from symbol-name matches (exact matches score highest), path matches, and term frequency weighted by IDF. Open editor files get extra score.
3. A symbol match gives that symbol's line range (up to 80 lines). Other files give a window around the best-matching line.
4. For the top files, the runtime adds the definitions of names they reference, when the name has exactly one definition.
5. Overlapping ranges merge. Snippets are taken in score order until the token budget (about 4 characters per token) is used.

Every snippet records its path, line range, score, and reason. Run **task-x: Preview Context for a Request…** to see them, and **task-x: Inspect Workspace Context** to see the project summary, instructions, and exclusions.

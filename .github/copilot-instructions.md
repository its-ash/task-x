# task-x — project instructions

task-x is a model-agnostic coding agent. `runtime/` is the Rust agent runtime (Tokio, JSON-RPC over stdio). `extension/` is the VS Code extension (TypeScript, CommonJS output).

## Rules

- Keep provider-specific code in `runtime/src/provider/<provider>.rs`. Other code uses the types in `provider/types.rs` only.
- Every protocol change updates `docs/protocol.md`, `extension/src/protocol.ts`, and the tests. A breaking change increments `PROTOCOL_VERSION` in both `runtime/src/rpc.rs` and `extension/src/protocol.ts`.
- Never log API keys, authorization headers, or prompt content. Keys live in VS Code SecretStorage only.
- `extension/src/rpc.ts` and `extension/src/runtime.ts` must not import `vscode`, so that plain Node can test them.
- Tests use mock providers. The default test suites must not need network access or paid keys.
- Track phase progress in `ROADMAP.md`. Mark an item done only after verification.

## Commands

- `make run` — debug build plus Extension Development Host
- `make build` — release runtime into `extension/bin/` plus extension build
- `cargo test --manifest-path runtime/Cargo.toml`
- `cargo clippy --manifest-path runtime/Cargo.toml --all-targets`
- `npm --prefix extension test`

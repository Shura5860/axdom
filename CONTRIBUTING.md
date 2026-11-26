# Contributing to axdom

## Prerequisites

- Rust `1.85+` (edition 2024)
- Linux: `at-spi2-core` (for live desktop testing)

## Local Checks

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Architecture Guidelines

- Keep platform-independent logic (pruning, redaction, `AppPolicy`, diffing, image cropping, MCP protocol) in `axdom-core` and `axdom-mcp`.
- Avoid `unwrap()` / `expect()` in production paths (daemon, MCP, backends); return typed `AxdomError` variants instead.
- Document every `unsafe` FFI block in `axdom-windows` and `axdom-macos` with its safety invariants.
- Add unit tests for new pruner rules, action normalizers, and MCP tools.

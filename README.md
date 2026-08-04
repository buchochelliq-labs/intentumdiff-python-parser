# intentumdiff-python-parser

[![CI](https://github.com/buchochelliq-labs/intentumdiff-python-parser/actions/workflows/ci.yml/badge.svg)](https://github.com/buchochelliq-labs/intentumdiff-python-parser/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust 1.95](https://img.shields.io/badge/rust-1.95-orange.svg)](https://www.rust-lang.org/)

The **Python parser plugin** for IntentumDiff — a Wasm component (WASI p2, Component Model)
implementing the `intentumdiff:plugin` parser interface in full-parse mode: it receives raw
Python source and emits a deterministic `SemanticNode` tree (tree-sitter based).

## Build

```bash
cargo build --release --target wasm32-wasip2
```

The component lands at `target/wasm32-wasip2/release/intentumdiff_python_parser.wasm`.
Toolchain: Rust 1.93.0 (pinned in CI).

## Test

```bash
cargo test
```

## Layout

```
src/lib.rs             the parser implementation
wit/plugin.wit         the intentumdiff:plugin contract (consumed from intentumdiff-plugin-sdk)
plugin_metadata.info   plugin metadata consumed by the host
patches/               vendored [patch.crates-io] crates this crate's graph needs
.claude/skills/        stamped copies of the plugin skills — edits belong in the
                       intentumdiff-plugin-sdk MASTER, not here
```

CI is a thin caller of the SDK's reusable `parser-ci.yml` — fixes to parser CI happen once
in `intentumdiff-plugin-sdk`, not per parser repo.

## Provenance

Migrated files-only (no history) from the IntentumDiff monorepo
(`buchochelliq-labs/intentumdiff`), which remains the archive of record.

License: MIT.

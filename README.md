# intentdiff-python-parser

The **Python parser plugin** for IntentDiff — a Wasm component (WASI p2, Component Model)
implementing the `intentdiff:plugin` parser interface in full-parse mode: it receives raw
Python source and emits a deterministic `SemanticNode` tree (tree-sitter based).

## Build

```bash
cargo build --release --target wasm32-wasip2
```

The component lands at `target/wasm32-wasip2/release/intentdiff_python_parser.wasm`.
Toolchain: Rust 1.93.0 (pinned in CI).

## Test

```bash
cargo test
```

## Layout

```
src/lib.rs             the parser implementation
wit/plugin.wit         the intentdiff:plugin contract (consumed from intentdiff-plugin-sdk)
plugin_metadata.info   plugin metadata consumed by the host
patches/               vendored [patch.crates-io] crates this crate's graph needs
.claude/skills/        stamped copies of the plugin skills — edits belong in the
                       intentdiff-plugin-sdk MASTER, not here
```

CI is a thin caller of the SDK's reusable `parser-ci.yml` — fixes to parser CI happen once
in `intentdiff-plugin-sdk`, not per parser repo.

## Provenance

Migrated files-only (no history) from the IntentDiff monorepo
(`buchochelliq-labs/intentdiff`), which remains the archive of record.

License: MIT.

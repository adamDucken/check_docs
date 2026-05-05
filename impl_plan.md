# check-docs v2 implementation plan

Goal: replace source parsing entirely with Rustdoc JSON. No `syn` file scanning fallback. If Rustdoc JSON cannot be generated or parsed, fail loudly with actionable error.

## Principles

- Single backend: Rustdoc JSON only.
- No lossy parsing, no source walking, no heuristic fallback.
- Resolve dependency alias and exact package through Cargo metadata.
- Generate JSON lazily per queried crate.
- Cache and reuse `target/doc/<crate_target>.json` when valid.
- Keep CLI output small and agent-friendly.

## Phase 1: Dependencies and removal

Update `Cargo.toml`:

- Add:
  - `rustdoc-types`
  - `serde`
  - `serde_json`
- Remove after migration:
  - `syn`
  - `quote`
  - `proc-macro2`
  - `walkdir`

Keep:

- `cargo_metadata`
- `tempfile` for tests

Risk: `rustdoc-types` must match nightly Rustdoc JSON format. Pin version. Add explicit error when `format_version` unsupported.

## Phase 2: Cargo metadata resolver

Create/replace resolver responsibilities:

1. Read metadata for `--root Cargo.toml`.
2. Identify selected root package by manifest path.
3. Map import crate name to direct dependency package id.
   - Handle renamed deps: import name is dependency edge name, not package name.
   - Normalize `-`/`_` carefully only where Rust crate identifiers require it.
4. Resolve exact `cargo_metadata::Package` from package id.
5. Find library-like target:
   - `lib`
   - `proc-macro`
6. Determine rustdoc JSON output name from target crate name, not dependency alias.

Errors must distinguish:

- no Cargo.toml
- dependency not direct
- package missing from metadata
- package has no doc-able library target
- std/core/alloc unsupported

## Phase 3: Rustdoc JSON generation

Create `src/rustdoc_json.rs`.

Responsibilities:

- Compute JSON path: `<target_dir>/doc/<target_name>.json`.
- Check existing JSON:
  - file exists
  - parses
  - crate version matches package version if available
  - format version supported
- If missing/stale, run:

```bash
cargo +nightly rustdoc -p <package-spec> -- -Z unstable-options --output-format json
```

Package spec must avoid ambiguity:

- Prefer exact package id/spec from metadata.
- If Cargo accepts `name@version`, use that.
- If renamed dependency points to package `serde_json`, run `-p serde_json@x.y.z`, not alias.

Command details:

- Run from project root or pass `--manifest-path <root/Cargo.toml>`.
- Preserve current features as Cargo resolves them.
- Capture stderr.
- On failure, print actionable message:
  - nightly missing
  - rustdoc JSON unsupported
  - package spec ambiguous
  - rustdoc failed

Config:

- env `CHECK_DOCS_TOOLCHAIN`, default `nightly`.
- optional `--regen` later, not first pass.

## Phase 4: Rustdoc graph lookup

Create `src/doc_graph.rs`.

Load:

```rust
let krate: rustdoc_types::Crate = serde_json::from_reader(file)?;
```

Core API:

```rust
pub fn find_item(krate: &Crate, import: &ImportPath) -> Result<ResolvedItem, Error>
```

Algorithm:

1. Start at `krate.root`.
2. Load root item from `krate.index`.
3. For each module segment:
   - inspect module child ids
   - match named module child with same name
   - also match public `use` item whose exported name equals segment
   - if `use`, follow referenced id, then continue
4. Resolve final item similarly:
   - direct child named item
   - public `use` item named item, follow id
5. If item resolves to another import/use, follow until concrete item.
6. Track visited ids to avoid cycles.
7. Return not found with path context.

Important observations from `syn.json`:

- `syn::ItemUse` is a root `use` item named `ItemUse` pointing to id `5843`.
- Canonical `.paths` says `syn::item::ItemUse`.
- Therefore `.paths` alone is insufficient for import lookup.
- Module `items` + `use` traversal is required.

Visibility:

- Only accept public items and public imports.
- Rustdoc JSON generated with `includes_private=false`, but still check visibility.

Glob handling:

- Verify how `rustdoc_types::ItemEnum::Import` represents glob imports.
- If glob import lacks concrete id, use resolved public children if available.
- If unresolved, return explicit unsupported-glob error. Do not guess.

## Phase 5: Formatting output

Create `src/format.rs`.

Map rustdoc item to current `SymbolDoc`-like report:

- crate name/version
- source path from `item.span.filename`
- location from `span.begin`
- item kind from `ItemEnum`
- item name from `item.name`
- docs from `item.docs`
- attrs from `item.attrs`
- definition from structured rustdoc data

Support first:

- struct
- enum
- trait
- function
- type alias
- constant
- static
- union
- macro
- proc macro / derive

Definition formatting can be intentionally compact:

- `pub struct Name { field: Type, ... }`
- `pub enum Name { Variant, ... }`
- `pub trait Name: Bounds { ... }`
- `pub fn name(args) -> Ret`

If exact signature formatting is hard, prefer structured partial output over fake precision.

## Phase 6: Delete v1 backend

Remove:

- source module resolver
- `WalkDir`
- `syn` parsing
- macro token heuristics
- lossy scanner
- reexport path heuristic

Delete tests that validate v1 internals.

Keep/replace tests around user behavior.

## Phase 7: Tests

Integration tests:

1. `use syn::ItemUse;`
   - resolves root reexport
   - docs include “A use declaration”
   - location points into `syn/src/item.rs`
2. `use syn::item::ItemUse;`
   - resolves canonical module path
3. Renamed dependency fixture:
   - dependency alias import resolves package target
4. Multiple-version fixture if practical:
   - exact package selected from dependency edge
5. Missing nightly:
   - simulate bad toolchain env
   - actionable failure
6. Missing item:
   - clear not found, no fallback
7. Std/core/alloc:
   - explicit unsupported message
8. Proc-macro dependency:
   - target kind `proc-macro` accepted

Unit tests:

- JSON graph traversal using tiny handcrafted `rustdoc_types::Crate` fixtures.
- Import/use following.
- Cycle detection.
- Formatter for each major item kind.

## Phase 8: CLI behavior

Current CLI can remain mostly stable:

```bash
check-docs 'use syn::ItemUse;' --root .
```

Failure examples:

- `check-docs: failed to generate rustdoc JSON for syn 2.0.117: nightly toolchain not found; install with rustup toolchain install nightly or set CHECK_DOCS_TOOLCHAIN`
- `check-docs: rustdoc JSON format 57 unsupported; supported: 56`
- `check-docs: crate 'foo' is not a direct dependency of selected package`

Exit code remains non-zero on all failures.

## Phase 9: Performance

- Lazy per crate.
- Reuse existing JSON.
- No whole dependency tree docs.
- No source scan.
- Optional future cache metadata sidecar:

```json
{
  "package_id": "...",
  "version": "...",
  "target": "...",
  "format_version": 56,
  "generated_at": "..."
}
```

## Implementation order

1. Add deps and `rustdoc_json.rs` loader/generator.
2. Upgrade resolver for alias/package target handling.
3. Implement graph traversal for modules + imports.
4. Implement minimal formatter for structs/enums/functions/traits/type aliases.
5. Wire `main.rs` to new backend.
6. Add `syn::ItemUse` integration test.
7. Delete v1 symbols code and deps.
8. Expand tests for edge cases.

## Non-goals for first v2

- No source parser fallback.
- No stable toolchain support.
- No full pretty-printer for every Rust signature.
- No docs for transitive dependencies unless directly imported as direct deps.
- No support for `std`, `core`, `alloc`.

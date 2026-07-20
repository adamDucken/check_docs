# check-docs Skill

Use `check-docs` for local Rust dependency docs from the exact crate version and feature set resolved by the target Cargo project. Prefer it over web search/docs.rs when working inside a Cargo project.

## What it does

`check-docs` resolves external `use` paths against the project at `--root`, generates/loads Rustdoc JSON for direct dependencies, and prints compact docs for requested items.

It reports:

- crate name + exact resolved version
- rustdoc JSON/source location
- item kind/name, including modules
- absolute file + line where declared when available
- definition/signature, including higher-ranked function-pointer binders
- deprecation details (`since` and `note`) when present
- semantic attributes: `repr`, `non_exhaustive`, and `must_use`
- struct/union fields with their Rust visibility, plus `private/stripped` note
  when fields are hidden
- enum variants, including explicit discriminants
- trait associated items, including provided associated-constant defaults
- constant and static initializers when Rustdoc preserves them; extern statics are
  labeled with their access safety instead of being shown with an invented initializer
- derives when Rustdoc exposes them via attrs or derived impls
- inherent public methods for structs/enums/unions
- direct non-blanket trait impls
- Rust doc comments

## When to use

Use when:

- User asks what an imported Rust function takes or returns.
- User asks what fields a struct has.
- User asks what variants an enum has.
- User asks what methods exist on a type.
- User asks what traits/derives a type implements.
- User asks what a trait requires.
- You see external `use crate_name::module::Item;` and need local docs/signature.
- You need docs for exact `Cargo.lock` version/features, not latest docs.rs.

Do not use for:

- `std`, `core`, or `alloc` items. Use official Rust docs.
- Local crate items (`crate::`, `self::`, `super::`). Inspect source directly.

## Command

```bash
check-docs 'use crate_name::module::item;' --root /path/to/cargo/project
```

If already in project root:

```bash
check-docs 'use crate_name::module::item;'
```

Quote the full use line as one shell argument.

## Examples

```bash
check-docs 'use syn::parse_file;' --root .
check-docs 'use cargo_metadata::MetadataCommand;' --root /home/adam/Desktop/rust/arbre_v1
check-docs 'use serde::Serialize;' --root .
check-docs 'use tokio::sync::{Mutex, RwLock, Semaphore};' --root .
```

## Supported input

- External crate paths: `use serde::Serialize;`
- Multi-item brace imports: `use tokio::sync::{Mutex, RwLock, Semaphore};`
- Single-item brace imports: `use syn::{File};`
- Nested brace imports: `use tower::{service_fn, util::{MapResponseLayer}};`
- Renamed imports resolve original item: `use syn::File as SynFile;`
- `self` imports inside non-root braces: `use tokio::sync::{self, mpsc};`
- Modules are supported and labeled: `use tokio::sync::watch;` -> `item: module watch`
- Enum variants are supported: `use facade::Number::One;`
- Raw identifiers are supported in crate aliases and item paths; their source spelling is preserved.
- Concrete items behind public external globs and multi-package re-export chains
  are followed through the resolved Cargo dependency graph.

Unsupported:

- glob imports: `use syn::*;`
- absolute leading paths: `use ::syn::File;`
- local paths: `use crate::Thing;`

## Dependency and feature workflow

`check-docs` runs Rustdoc through Cargo for the target project:

```bash
cargo +nightly-2025-09-10 rustdoc -p <dependency>@<version> --manifest-path <root>/Cargo.toml -- -Z unstable-options --output-format json
```

Consequences:

- It uses exact versions from the project lockfile/resolution.
- It respects dependency renames from `Cargo.toml`.
- It uses exactly the features enabled by the target project.
- It only works for direct dependencies of the selected package.

If dependency is missing:

1. Add it to `Cargo.toml` with needed features.
2. Run `cargo check` (or `cargo build`) to update resolution/lockfile.
3. Run `check-docs` again.

If item is “not found” but you believe it exists:

1. Check whether item is behind a crate feature.
2. Enable feature in `Cargo.toml`.
3. Run `cargo check`.
4. Re-run `check-docs`.

Example:

```toml
tokio = { version = "1", features = ["rt", "sync"] }
```

Then:

```bash
cargo check
check-docs 'use tokio::sync::mpsc::Sender;' --root .
```

## Nightly requirement

Rustdoc JSON requires the nightly pinned in `rust-toolchain.toml`, kept in sync
with `rustdoc-types 0.56.x`. Install it with:

```bash
rustup toolchain install nightly-2025-09-10 --component rustfmt clippy llvm-tools-preview
```

Or set:

```bash
CHECK_DOCS_TOOLCHAIN=<toolchain> check-docs 'use dependency::Item;' --root .
```

`CHECK_DOCS_TOOLCHAIN` is an advanced override; the selected toolchain must emit
the `rustdoc-types 0.56.x` schema.

## Agent workflow

1. Locate project root containing `Cargo.toml`.
2. Ensure target crate is a direct dependency. If not, add it with needed features and run `cargo check`.
3. Copy exact external `use` line for target item(s).
4. Prefer batch brace query for related items from same module.
5. Run `check-docs '<use line>' --root <project-root>`.
6. Use output definition/details/methods/impls/derives/docs for implementation or answer.
7. If “not found”, check feature flags before assuming item does not exist.
8. Use web/docs.rs only after local docs fail, are missing, or target item is not a direct dependency.

## Error interpretation

- `not a direct dependency`: add dependency to `Cargo.toml` or query from project where it is direct.
- `Rust standard library`: use <https://doc.rust-lang.org/std/>.
- `glob imports are not supported`: query concrete item path.
- `ambiguous across Rust namespaces`: the import spelling denotes more than one
  public symbol (for example, a trait and derive macro). Query a namespace-specific
  canonical path; the tool will not silently choose one rustdoc item.
- `external branches failed`: the item crossed an external re-export or glob, but
  no unique reachable normal dependency branch contained the requested concrete item.
- `failed to generate rustdoc JSON`: install/use the pinned nightly, run `cargo check`, inspect Cargo/Rustdoc stderr.
- `item not found`: likely wrong path, private item, disabled feature, or unsupported Rustdoc shape.
- `definition rendering unsupported for ...`: Rustdoc identified the item, but its schema does not contain enough source information to print a trustworthy Rust declaration.

## Best practices

- Query concrete item when you need methods/type details: `use tokio::sync::mpsc::Sender;`.
- Module query is OK for module docs: `use tokio::sync::watch;`.
- Prefer batch braces for related items: `use tokio::sync::{Mutex, RwLock, Semaphore};`.
- Prefer canonical module path if root re-export fails.
- For renamed dependencies, use import name used in code, not package name.
- After editing `Cargo.toml`, run `cargo check` before querying.
- Trust local output over docs.rs when versions/features differ.

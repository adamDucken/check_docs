# check-docs Skill

Use `check-docs` for local Rust dependency docs from the exact crate version and feature set resolved by the target Cargo project. Prefer it over web search/docs.rs when working inside a Cargo project.

## What it does

`check-docs` resolves an external `use` path against the project at `--root`, generates/loads Rustdoc JSON for that direct dependency, and prints compact docs for the requested item.

It reports:

- crate name + exact resolved version
- rustdoc JSON/source location
- item kind/name
- file + line where declared when available
- definition/signature
- struct/union fields
- enum variants
- trait associated items
- Rust doc comments

## When to use

Use when:

- User asks what an imported Rust function takes or returns.
- User asks what fields a struct has.
- User asks what variants an enum has.
- User asks what a trait requires.
- You see `use crate_name::module::Item;` and need local docs/signature.
- You need docs for exact `Cargo.lock` version, not latest docs.rs.

Do not use for:

- `std`, `core`, or `alloc` items. Use official Rust docs.
- Local crate items (`crate::`, `self::`, `super::`). Inspect source directly.
- Methods on concrete types. Query trait if method is trait-associated; otherwise inspect docs/source manually.

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
check-docs 'use syn::File;' --root /home/adam/Desktop/rust/arbre
```

## Supported input

- External crate paths only: `use serde::Serialize;`
- One item per query.
- Brace import allowed only with one item: `use syn::{File};`
- Renamed imports resolve original item: `use syn::File as SynFile;`

Unsupported:

- glob imports: `use syn::*;`
- multi-item braces: `use syn::{File, Item};`
- absolute leading paths: `use ::syn::File;`
- local paths: `use crate::Thing;`

## Dependency and feature workflow

`check-docs` runs Rustdoc through Cargo for the target project:

```bash
cargo +nightly rustdoc -p <dependency>@<version> --manifest-path <root>/Cargo.toml -- -Z unstable-options --output-format json
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

1. Check whether the item is behind a crate feature.
2. Enable the feature in `Cargo.toml`.
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

Rustdoc JSON requires nightly. If command fails with missing toolchain:

```bash
rustup toolchain install nightly
```

Or set:

```bash
CHECK_DOCS_TOOLCHAIN=<toolchain> check-docs 'use crate::Item;' --root .
```

Use a nightly compatible with the pinned `rustdoc-types` schema.

## Agent workflow

1. Locate project root containing `Cargo.toml`.
2. Ensure target crate is a direct dependency. If not, add it with needed features and run `cargo check`.
3. Copy exact external `use` line for target item.
4. Run `check-docs '<use line>' --root <project-root>`.
5. Use output definition/details/docs for implementation or answer.
6. If “not found”, check feature flags before assuming item does not exist.
7. Use web/docs.rs only after local docs fail, are missing, or target item is not a direct dependency.

## Error interpretation

- `not a direct dependency`: add dependency to `Cargo.toml` or query from project where it is direct.
- `Rust standard library`: use <https://doc.rust-lang.org/std/>.
- `glob imports are not supported`: query concrete item path.
- `brace imports must contain one item`: split into separate queries.
- `failed to generate rustdoc JSON`: install/use nightly, run `cargo check`, inspect Cargo/Rustdoc stderr.
- `item not found`: likely wrong path, private item, disabled feature, or unsupported method query.

## Best practices

- Query concrete item, not module: `use tokio::sync::mpsc::Sender;`, not `use tokio::sync::mpsc;`.
- Prefer canonical module path if root re-export fails.
- For renamed dependencies, use the import name used in code, not package name.
- After editing `Cargo.toml`, run `cargo check` before querying.
- Trust local output over docs.rs when versions/features differ.

# check-docs

`check-docs` is a command-line tool for inspecting Rust dependency APIs from the
exact package versions, features, targets, and dependency contexts resolved by a
local Cargo project. It generates Rustdoc JSON locally and prints compact,
source-linked documentation for requested imports.

## Requirements

- Rust 1.89 or newer to build `check-docs`.
- [Rustup](https://rustup.rs/) and `nightly-2025-09-10` for Rustdoc JSON.
- A current `Cargo.lock` in the project being queried.

Install the required nightly and the binary:

```bash
rustup toolchain install nightly-2025-09-10
cargo install check-docs --locked
```

## Usage

Pass a complete external `use` declaration and the Cargo project root:

```bash
check-docs 'use syn::parse_file;' --root /path/to/project
```

From the project root, `--root .` is implicit:

```bash
check-docs 'use syn::{parse_file, File};'
```

The report includes the resolved crate version and feature selection, source
location, declaration, attributes, documentation, fields or variants, methods,
and direct trait implementations when applicable.

### Features and workspaces

Feature flags follow Cargo semantics:

```bash
check-docs 'use dependency::Extra;' --features extra
check-docs 'use dependency::Extra;' --all-features
check-docs 'use dependency::DefaultApi;' --no-default-features
check-docs 'use dependency::Item;' --package workspace-member
```

Normal dependencies are queried by default. Opt in to other contexts:

```bash
check-docs 'use dev_dependency::Item;' --include-dev
check-docs 'use build_dependency::Item;' --include-build
```

Use `--target TRIPLE` to select a target explicitly. Otherwise, `check-docs`
honors Cargo's effective target configuration.

## Scope

Supported input includes concrete external paths, nested brace imports,
renames, modules, enum variants, raw identifiers, and public re-exports.

The following are intentionally unsupported:

- glob imports such as `use syn::*;`
- standard-library crates (`std`, `core`, and `alloc`)
- local paths beginning with `crate`, `self`, or `super`
- transitive dependencies not exposed through a direct dependency

`check-docs` never creates or updates the queried project's lockfile. Refresh a
missing or stale lockfile with `cargo check` or `cargo build`, then retry. Its
managed Rustdoc output is stored under the project's `target/check-docs`
directory.

## Toolchain override

The default nightly is pinned to the Rustdoc schema consumed by this release.
Advanced users may override it for the entire query:

```bash
CHECK_DOCS_TOOLCHAIN=<compatible-toolchain> check-docs 'use dependency::Item;'
```

The selected toolchain must support the target project and emit the
`rustdoc-types 0.56.x` JSON schema.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at
your option.

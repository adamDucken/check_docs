# check-docs Skill

Use `check-docs` when you need local Rust crate docs, signatures, definitions, or fields for items imported from current Cargo project dependencies. Prefer this over web search/docs.rs when crate source is already downloaded by Cargo.

## When to use

- User asks what an imported Rust function takes or returns.
- User asks what fields a struct has.
- User asks what variants an enum has.
- User asks what methods/associated items a trait requires.
- You see a `use ...::Item;` line and need docs/definition from exact `Cargo.lock` crate version.

## Command

```bash
check-docs 'use crate_name::module::item;' --root /path/to/cargo/project
```

If already in project root:

```bash
check-docs 'use crate_name::module::item;'
```

## Examples

```bash
check-docs 'use syn::parse_file;' --root .
```

```bash
check-docs 'use cargo_metadata::MetadataCommand;' --root /home/adam/Desktop/rust/arbre_v1
```

```bash
check-docs 'use serde::Serialize;' --root .
```

```bash
check-docs 'use syn::File;' --root /home/adam/Desktop/rust/arbre
```

## Output gives

- resolved crate + exact version from target Cargo project
- local Cargo registry source path
- item kind/name
- file + line where declared
- definition/signature
- derives for structs/enums/unions when available
- fields for structs/unions
- variants for enums
- associated items for traits
- Rust doc comments

## Rules

- Pass full `use` line as one quoted argument.
- Works for external crate items only.
- Supports free functions, structs, enums, traits, type aliases, consts, statics, unions, and some macro-generated items.
- Glob imports are unsupported.
- Brace imports must contain one item only, e.g. `use syn::{parse_file};`.
- Methods are not supported unless they are trait associated items shown through the trait.

## Agent workflow

1. Locate Rust project root containing `Cargo.toml`.
2. Copy exact `use` line containing target item.
3. Run `check-docs '<use line>' --root <project-root>`.
4. Use returned definition/details/docs to answer or implement code.
5. Do not web search unless `check-docs` fails or docs are missing.

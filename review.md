# Codebase Review

Date: 2026-05-26

Scope: `/home/adam/Desktop/rust/check_docs`, current working tree. I read all Rust source files:

- `src/cli.rs`
- `src/imports.rs`
- `src/main.rs`
- `src/resolver.rs`
- `src/rustdoc_json.rs`
- `src/symbols.rs`
- `tests/cli.rs`

Also checked `Cargo.toml` and ran focused runtime probes through the built binary.

## Verification

Commands run:

```text
cargo check
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
target/debug/check-docs 'use syn::ItemUse;' --root .
target/debug/check-docs 'use serde::Serialize;' --root .
target/debug/check-docs 'pub(crate) use syn::ItemUse;' --root .
target/debug/check-docs 'use syn::parse::Parse;' --root .
target/debug/check-docs 'use cargo_metadata::{NodeDep, DepKindInfo, DependencyKind, Package};' --root .
target/debug/check-docs 'use rustdoc_types::{Visibility, Use, Item};' --root .
```

Results:

- `cargo check`: passed.
- `cargo test`: passed, 50 tests.
- `cargo fmt --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: failed with two warnings promoted to errors.
- Exact dependency doc probes succeeded for `cargo_metadata`, `rustdoc_types`, and `syn::ItemUse`.
- Runtime probes exposed current false negatives for `serde::Serialize` and `pub(crate) use ...`.

## Findings

### High: direct-dependency public re-exports can fail as "not direct"
status: issue:closed - fixed

References:

- `src/main.rs:104-130`
- `src/main.rs:139-168`
- `src/symbols.rs:129-202`
- `src/resolver.rs:130-162`

Reproduction:

```text
$ target/debug/check-docs 'use serde::Serialize;' --root .
check-docs: item 'Serialize' is re-exported from external crate 'serde_core' but exact docs require that crate to be a direct dependency of the selected package; add/query 'serde_core' directly: crate 'serde_core' is not a direct dependency of the selected package
```

This is a bad failure mode. `serde` is a direct dependency and `serde::Serialize` is a normal public API path. The tool rejects it because rustdoc says the resolved item lives in `serde_core`, then `main.rs` insists that external re-export targets must also be direct root dependencies.

Impact: common public imports through facade crates fail even when the queried crate is direct and the import is valid. This undercuts the core product promise: "give me exact docs for this import."

Recommendation:

- Treat the imported re-export itself as a valid report even when the resolved target is transitive.
- If resolved docs are required, resolve external rustdoc crate IDs against the dependency graph, not only root direct dependencies.
- Add a regression test for `use serde::Serialize;` because this repo already has `serde` as a direct dependency.

### High: external re-export fallback can report the wrong item
status: issue:closed - fixed

References:

- `src/main.rs:242-269`

`find_external_symbol_with_fallback` first tries the external path from rustdoc. If that fails and the import had any module segment, it silently retries as a crate-root item with the same final name.

That is unsafe. A failed `dep::module::Thing` lookup can be reported as `dep::Thing` if a root item with that name exists. The output then looks successful but documents a different symbol than the public re-export path named.

Impact: false positives are worse than "not found" for a docs lookup tool. They can give the user a plausible definition for the wrong item.

Recommendation:

- Remove unconditional root fallback.
- Only follow an alternate path when rustdoc metadata proves that path is an alias/re-export.
- If keeping fallback, print it as an explicit heuristic and keep the original lookup failure visible.

### Medium: virtual workspace roots are unsupported
status: issue:closed - fixed

References:

- `src/main.rs:52-79`
- `src/resolver.rs:71-89`

`--root` is assumed to contain a package manifest. `package_for_manifest` canonicalizes `root/Cargo.toml` and then searches `metadata.packages` for an exact package `manifest_path` match. A virtual workspace root has no package, so this fails before dependency resolution.

Impact: many real Rust repos are workspaces. `check-docs --root .` at a workspace root cannot work unless the root is also a package. There is no `-p` or `--package` argument to select a member.

Recommendation:

- Support `--package <name-or-id>` for workspace roots.
- If metadata has `root_package()`, use it only when present.
- Emit a specific virtual-workspace error instead of "package for manifest not found."

### Medium: duplicate dependency contexts are treated as fatal ambiguity
status: issue:closed - fixed

References:

- `src/resolver.rs:91-118`
- `src/resolver.rs:130-149`
- `src/resolver.rs:303-314`

`package_dependencies` stores one entry per `dep_kind`, then `resolve_dependency` rejects any crate name with more than one entry. This means `--include-dev` or `--include-build` can make a normal dependency fail if the same crate also appears as dev/build dependency.

That is not inherently ambiguous for docs. If all entries resolve to the same `PackageId`, the rustdoc target and docs are the same. The current code converts a reporting/context issue into a hard failure.

Impact: projects that reuse crates in normal and dev/build contexts get avoidable false negatives.

Recommendation:

- Deduplicate by `(crate_name, package_id)`.
- Preserve all contexts for display.
- Only fail when one import name maps to different package IDs or different library targets.

### Medium: parser rejects valid visible use items
status: issue:closed - fixed

References:

- `src/imports.rs:43-61`

Reproduction:

```text
$ target/debug/check-docs 'pub(crate) use syn::ItemUse;' --root .
check-docs: invalid use import syntax: expected one of: identifier, `self`, `super`, `crate`, `try`, `*`, curly braces
```

`starts_like_use_item` only accepts text beginning with `use ` or `pub `. It misses valid Rust visibility forms like `pub(crate) use`, `pub(super) use`, and `pub(in path) use`.

Impact: users pasting real `use` lines from a module can get syntax errors before docs resolution.

Recommendation:

- Let `syn::parse_str::<ItemUse>` try the raw input first.
- Only synthesize `use ...;` if raw parsing fails because the user supplied a bare path.
- Add tests for `pub(crate) use syn::ItemUse;` and `pub(super) use ...`.

### Medium: trait item formatting is syntactically wrong
status: issue:closed - fixed

References:

- `src/symbols.rs:711-730`
- `src/symbols.rs:733-760`

Reproduction:

```text
$ target/debug/check-docs 'use syn::parse::Parse;' --root .
details:
  pub fn parse(input: ParseStream<'_>) -> Result<Self>
```

Trait methods are not written as `pub fn` inside trait definitions. `trait_details` reuses `fn_def`, and `fn_def` unconditionally prefixes functions with `pub `. That is correct for free functions and inherent public methods, but wrong for trait members.

Impact: generated definitions are not exact. This matters because the tool is explicitly about exact docs/signatures.

Recommendation:

- Split renderers by context: free function, inherent method, trait method, function pointer.
- Do not emit `pub` for trait methods or trait associated items.
- Add a regression test using `syn::parse::Parse`.

### Medium: Clippy fails with warnings denied
status: issue:closed - fixed

References:

- `src/rustdoc_json.rs:133`
- `src/symbols.rs:209-258`

`cargo clippy --all-targets -- -D warnings` fails:

```text
src/rustdoc_json.rs:133:31: clippy::ptr_arg
src/symbols.rs:213:5: clippy::only_used_in_recursion
```

`lock_timeout_message` takes `&PathBuf` where `&Path` is enough. `find_child` accepts `is_last`, but that parameter is only forwarded recursively and never changes local behavior.

Impact: not a runtime bug, but this codebase is small enough that Clippy should be clean under `-D warnings`. The `is_last` warning also points at dead API shape in symbol traversal.

Recommendation:

- Change `lock_timeout_message(path: &Path)`.
- Remove `is_last` from `find_child` unless a real last-segment distinction is implemented.
- Add Clippy to CI if this project has CI.

### Low: help exits inside the parser
status: issue:closed - fixed

References:

- `src/cli.rs:40-43`

`parse_args_from` calls `std::process::exit(0)` for `--help`. That couples parsing to process termination and prevents normal unit testing of help behavior without spawning the binary.

Impact: low in a small CLI, but it is unnecessary design debt.

Recommendation:

- Return a command enum, for example `ParsedCommand::Run(Args)` or `ParsedCommand::Help`.
- Let `main` own process exit behavior.

### Low: output and tests are too string-fragile
status: issue:closed - fixed

References:

- `src/main.rs:287-348`
- `tests/cli.rs:3-137`

The output is plain text assembled directly with `println!`, and integration tests assert substrings. This is acceptable for smoke tests, but brittle for a tool whose output contains structured facts: crate, version, target, source path, import, definition, docs, methods, impls.

Impact: behavior can regress while tests still pass, especially around multiline details and resolved-vs-imported items.

Recommendation:

- Keep text output for humans.
- Add a structured internal report type and test that directly.
- Consider optional JSON output later if this becomes agent-facing or script-facing.

## Coverage Gaps

Current tests are useful and numerous for a small crate, but still miss important real-world shapes:

- `serde::Serialize` facade re-export from a direct dependency.
- `pub(crate) use ...` and other visibility-qualified imports.
- virtual workspace root behavior.
- normal plus dev/build dependency duplicate context.
- trait-method formatting without `pub`.
- false-positive root fallback where `dep::module::Thing` fails but `dep::Thing` exists.
- Clippy in CI or local check target.

## Notes

`cargo metadata --filter-platform` behavior was spot-checked with a temporary `/tmp` fixture. Inactive target-specific dependencies were removed from the root resolve node for the non-matching target, so I am not flagging target filtering as a current bug.

The codebase is readable and the module split is reasonable. Main risk is semantic accuracy, not general Rust hygiene. For a docs lookup tool, false negatives on facade re-exports and false positives from fallback resolution should be treated as correctness bugs, not edge-case polish.

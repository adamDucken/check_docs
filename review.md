# Codebase Review

Date: 2026-05-25

Scope: current working tree of `/home/adam/Desktop/rust/check_docs`. The tree already had uncommitted edits and deleted markdown files before this review; this review does not assume those changes are mine or revert them.

## Repository Research

This is a single-binary Rust CLI published as `check-docs`.

Primary flow:

1. `src/cli.rs` parses `check-docs '<use ...;>' [--root PATH]`.
2. `src/imports.rs` parses a limited subset of Rust `use` paths into `ImportPath`.
3. `src/main.rs` rejects `std`/`core`/`alloc`, reads Cargo metadata for the selected root package, resolves the queried crate as a dependency, generates rustdoc JSON, resolves the item, and prints a compact report.
4. `src/resolver.rs` maps Cargo metadata package/dependency records to a doc-able library target.
5. `src/rustdoc_json.rs` shells out to `cargo +nightly rustdoc ... -- -Z unstable-options --output-format json`, validates the JSON schema version, and loads `rustdoc_types::Crate`.
6. `src/symbols.rs` walks rustdoc's module/use graph, follows re-exports, formats item definitions, fields, variants, methods, impls, derives, and docs.

Resolved local dependency versions observed in `Cargo.lock`:

- `cargo_metadata 0.18.1`
- `rustdoc-types 0.56.0`
- `serde 1.0.228`
- `serde_json 1.0.149`
- `tempfile 3.27.0`
- transitive `camino 1.2.2`

Validation performed:

- `cargo test`: passed, 25 unit tests and 8 integration tests.
- `cargo fmt --check`: passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `cargo run --quiet -- 'use cargo_metadata::CargoOpt;' --root .`: passed and produced docs.
- `cargo run --quiet -- 'use cargo_metadata::camino::Utf8PathBuf;' --root .`: failed, details below.
- `cargo run --quiet -- 'use cargo_metadata::camino;' --root .`: failed, details below.
- Local doc probes through the binary confirmed exact rustdoc shapes for `cargo_metadata::NodeDep`, `cargo_metadata::DependencyKind`, `cargo_metadata::{Metadata, Package, Target}`, and `rustdoc_types::{Item, Visibility, Generics, WherePredicate, GenericParamDefKind}`.

Overall assessment: the code is small, readable, well factored, and currently passes its checks. The strict concern is not style or Rust hygiene. The concern is correctness against the tool's stated promise: exact, feature-sensitive local docs for imported external items. Several core paths still approximate Rust/Cargo/rustdoc behavior enough that the tool will give false negatives or materially incomplete signatures in realistic crates.

## Findings

### High: external module re-exports are not resolved

Status: issue:closed - fixed

References:

- `src/symbols.rs:72-75`
- `src/symbols.rs:115-130`
- `src/main.rs:89-110`
- `src/main.rs:133-168`

The new external re-export path only works when the external item is the final segment. If rustdoc exposes an intermediate segment as an external crate/module re-export, `external_reexport` returns `Ok(None)` for `Followed::External(_)` before the last segment. That prevents resolution of common public API shapes such as `pub use camino;` followed by `cargo_metadata::camino::Utf8PathBuf`.

Reproduced:

```text
$ cargo run --quiet -- 'use cargo_metadata::camino::Utf8PathBuf;' --root .
check-docs: item 'Utf8PathBuf' is a public re-export with unsupported rustdoc external id in cargo_metadata 0.18.1 (.../target/doc/cargo_metadata.json): rustdoc item id Id(749) references external re-export from crate 'camino' (path camino); full item data is not present in this crate's rustdoc JSON; query/add the external crate directly if available
```

Also reproduced:

```text
$ cargo run --quiet -- 'use cargo_metadata::camino;' --root .
check-docs: item 'camino' not found in camino 1.2.2 (.../target/doc/camino.json): 'camino' not found under camino; 'camino' appears to be a module - query a concrete item inside that module
```

The second failure shows a related root-crate bug: `external_from_id` turns a one-component external path like `camino` into an import for `camino::camino`, not the external crate root/module.

Impact: a user querying a public item reachable through a direct dependency's public API receives a failure even though Cargo metadata has the transitive crate and the final item exists. This directly undermines the re-export feature.

Recommendation:

- When an external re-export is encountered before the final segment, preserve the unresolved tail and continue in the external crate: `external_crate::<remaining path>`.
- Treat a one-component `ItemSummary` equal to the external crate name as the external crate root, not as an item named after the crate.
- Add integration tests using a real public re-export shape, ideally `cargo_metadata::camino::Utf8PathBuf`, because it already exists in this dependency graph.

### High: external re-export package resolution can select the wrong crate or violate the direct-dependency contract

References:

- `src/main.rs:89-98`
- `src/resolver.rs:69-94`

When a re-export points outside the queried crate, `resolve_metadata_package` scans every package in the full metadata graph by package name or target name. It does not prove that the package is the crate referenced by rustdoc's external id, does not verify source, and does not require the crate to be a direct dependency of the selected package.

This creates two correctness failures:

- Duplicate versions or duplicate sources of the same crate name can make the lookup ambiguous or wrong.
- The tool's documented model says queried crates are direct dependencies, but this path silently documents transitive dependencies.

`rustdoc_types::ExternalCrate` provides a crate name but not enough package identity to safely pick one package from an arbitrary Cargo graph. A name-only scan is not an exact resolution mechanism.

Impact: in complex graphs, the tool can generate docs for the wrong version/source of a re-exported type, or fail with "matched multiple packages" even though the original direct dependency's rustdoc had enough information to explain the limitation.

Recommendation:

- Decide the contract explicitly: either external re-export traversal is best-effort/transitive, or exact docs are limited to direct dependencies.
- If exactness is required, prefer an actionable error that tells the user to add/query the external crate directly.
- If transitive traversal is retained, add package identity safeguards and tests with two versions of the same crate in the graph.

### High: reported definitions are not exact for generics and where clauses

References:

- `src/symbols.rs:470-493`
- `src/symbols.rs:578-584`
- `src/symbols.rs:605-633`
- `src/symbols.rs:676-699`

The definition formatter uses `generics(&...)` almost everywhere, but `generics` only prints generic parameter names. It ignores:

- lifetime outlives constraints
- type bounds
- type defaults
- const generic types
- const generic defaults
- all `where_predicates`

Local rustdoc probes confirmed that `rustdoc_types::Generics` contains `params` and `where_predicates`, and `GenericParamDefKind` contains the omitted information. The current formatter discards it.

Examples of information currently lost:

```rust
pub struct Cache<'a, T: Clone = String, const N: usize = 32>
where
    T: Send + Sync,
```

would be reduced toward:

```rust
pub struct Cache<'a, T, N>
```

Impact: the output can be materially wrong for signatures, type definitions, trait definitions, methods, trait impls, and associated items. This is not just cosmetic; bounds and where clauses are often the information a user is looking up.

Recommendation:

- Implement a real generic/where renderer.
- Include bounds/defaults for `Lifetime`, `Type`, and `Const` generic params.
- Render `WherePredicate::{BoundPredicate, LifetimePredicate, EqPredicate}`.
- Add tests with constrained type params, const generics, defaults, and where clauses.

### High: Cargo package selection loses package identity

References:

- `src/resolver.rs:110-112`
- `src/rustdoc_json.rs:108-116`

`package_spec` formats `-p` as `name@version`. That is not a full Cargo package identity. Cargo metadata package IDs include source/path identity, but this code discards it before invoking `cargo rustdoc`.

Impact: if the resolution contains same-name/same-version packages from different sources, or path/git overrides that collide on name/version, `cargo rustdoc -p name@version` can be ambiguous or select a package different from the resolved `PackageId`. That again violates the "exact resolved crate" promise.

Recommendation:

- Use the most specific package ID spec Cargo accepts from `cargo_metadata::PackageId`, not just `name@version`.
- Add a fixture with source/path ambiguity if Cargo permits it in one resolution; otherwise add a regression test around the exact package-id string used for rustdoc invocation.

### Medium: batch imports regenerate the same rustdoc JSON repeatedly

References:

- `src/main.rs:55-65`
- `src/rustdoc_json.rs:24-27`

`parse_use_lines` supports brace batches, but `run` processes each expanded import independently. `load_or_generate` always regenerates. Therefore:

```text
use cargo_metadata::{Metadata, Package};
```

runs `cargo +nightly rustdoc` twice for the same crate in the same process.

Impact: the supported batch syntax has poor performance. For larger crates, a multi-item query can become seconds or minutes slower than necessary. It also increases lock contention and the probability of stale lock problems.

Recommendation:

- Resolve and load each dependency crate once per invocation.
- Cache `(package_id, target_name) -> (Crate, json_path)` inside `run`.
- Keep the "always regenerate per invocation" policy if desired, but do not regenerate per item.

### Medium: lock files can become stale and block generation

References:

- `src/rustdoc_json.rs:35-66`
- `src/rustdoc_json.rs:69-72`

The rustdoc JSON lock is implemented with `create_new(true)` and is removed only by `Drop`. If the process is killed, panics across abort, or the machine loses power, the lock file remains. Later runs wait 30 seconds and fail.

Impact: a stale `target/doc/*.json.lock` can make the tool look broken until the user manually deletes the lock file.

Recommendation:

- Store PID and timestamp in the lock file and recover old locks conservatively.
- Or use an advisory file-lock crate with well-understood stale-lock behavior.
- Include the stale lock path and remediation in the timeout error if this simple lock stays.

### Medium: glob traversal can return false negatives

References:

- `src/symbols.rs:161-183`

In `find_child`, an unresolved glob import with `is_last` immediately returns an error. That prevents later glob imports from being searched. The recursive glob search also reuses the same `visited` set across sibling branches, so a failed branch can poison later branches and cause a false cycle/false miss.

Impact: rustdoc graphs with multiple public glob re-exports can fail even when a later glob would resolve the item. Public API facades often use glob re-exports heavily, so this is not an exotic shape.

Recommendation:

- Treat one failed glob branch as a branch failure, not as the whole lookup failure.
- Clone or scope `visited` per branch while keeping cycle detection inside each branch.
- Accumulate branch errors for diagnostics only after all candidates fail.

### Medium: the `use` parser rejects common valid Rust imports

References:

- `src/imports.rs:27-48`
- `src/imports.rs:71-93`

The parser is intentionally small, but the edge cases are now close to the documented support boundary. `parse_use_lines` strips aliases with a global `rsplit_once(" as ")` before brace expansion. This corrupts valid imports such as:

```rust
use syn::{ItemUse as IU, UseTree};
```

The parser also does not handle `self` in braces, comments, nested groups, or other token-aware Rust `use` syntax. Nested groups are documented as unsupported, but brace aliases are not clearly excluded and "Renamed imports" are documented as supported.

Impact: users can paste valid `use` statements and get parser errors unrelated to docs resolution.

Recommendation:

- Either explicitly narrow the documented grammar, or parse with `syn::ItemUse` and convert `UseTree` into the supported path set.
- Add tests for alias inside braces, `self`, and the documented unsupported forms.

### Medium: dependency-kind and target filters are ignored

References:

- `src/resolver.rs:35-47`

`package_dependencies` maps every `resolve.nodes[*].deps` entry by `dep.name`, ignoring `dep.dep_kinds`. Local doc probes confirmed `cargo_metadata::NodeDep` includes `dep_kinds: Vec<DepKindInfo>`, and `DependencyKind` distinguishes normal, development, and build dependencies.

Impact: target-specific, build-only, or dev-only crates can be accepted as "direct dependencies" without making the selected target context explicit. If the goal is exact docs for imports available to the selected package, this can report docs for crates that are not importable in the user's current target/module context. Duplicate dependency names across target cfgs can also overwrite each other in the `HashMap`.

Recommendation:

- Decide whether dev/build/target-specific dependencies are in scope.
- If they are in scope, report the dependency kind/target context.
- If they are not in scope, filter by `dep_kinds` and selected target.

### Medium: re-export alias docs and alias names are discarded

References:

- `src/symbols.rs:48-56`
- `src/symbols.rs:194-207`
- `src/symbols.rs:299-303`

`find_symbol` follows every `Use` before formatting the item. That means a public re-export with a different public name or its own doc comment is reported as the underlying item. The tests currently assert this behavior for `Alias -> Config`.

Impact: for public API docs, the re-export itself may be the documented surface. Docs.rs often displays re-export docs and public alias names because those are what users import.

Recommendation:

- Decide whether the report should describe the imported public symbol or the ultimate definition.
- If both are useful, print both: imported symbol/re-export and resolved definition.
- Preserve re-export docs when rustdoc provides them.

### Low: help handling exits inside the parser

References:

- `src/cli.rs:22-24`

`parse_args` calls `std::process::exit(0)` for help. This makes parser behavior harder to unit test and couples parsing to process control.

Impact: low for a small binary, but it is an avoidable testability/design smell.

Recommendation:

- Return an enum such as `Command::Help` or an error/status type and let `main` decide the exit code.

### Low: error classification is string-fragile

References:

- `src/main.rs:259-267`
- `src/symbols.rs:218-223`

`not_found_message` checks whether the context string contains `"external re-export"` to classify an external rustdoc ID. This couples control flow to English error text.

Impact: low today, but this will get brittle as error messages become more specific.

Recommendation:

- Use structured error enums for symbol traversal, especially `ExternalReexport`, `MissingItem`, `Cycle`, and `UnsupportedRustdocShape`.

## Test Coverage Gaps

The current tests are useful and pass, but they mostly exercise hand-built rustdoc graphs and happy-path integration cases. Missing strict-regression coverage:

- real external crate-root module re-export, such as `cargo_metadata::camino::Utf8PathBuf`
- external re-export ambiguity with duplicate crate names/versions
- generic bounds/defaults/where clauses in formatted definitions
- brace imports containing aliases
- multiple glob branches where one branch fails and a later branch succeeds
- same-crate batch import should invoke rustdoc once per crate per process
- stale lock file behavior
- target-specific/dev/build dependency policy

## Priority Fix Plan

1. Fix external module re-export traversal and add the `cargo_metadata::camino::Utf8PathBuf` integration test.
2. Define the external transitive dependency contract; either make it exact or explicitly fail with direct-dependency guidance.
3. Implement complete generic and where-clause rendering before claiming exact definitions/signatures.
4. Cache rustdoc JSON per package/target within one CLI invocation.
5. Replace or harden the lock file.
6. Replace the ad hoc `use` parser with `syn` or document a deliberately smaller grammar.

## Bottom Line

The implementation is clean enough to evolve, and the baseline checks pass. The strict issue is semantic accuracy. The tool is strongest for simple direct dependency items with simple signatures. It is currently weak for re-export-heavy APIs, generic-heavy APIs, package identity edge cases, and realistic pasted `use` syntax. Those are core cases for a docs lookup tool, so they should be treated as correctness work rather than polish.

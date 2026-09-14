use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

fn write_member(workspace: &TempDir, name: &str, dependencies: &str, source: &str) {
    let root = workspace.path().join(name);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\n{dependencies}\n"
        ),
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), source).unwrap();
}

fn workspace(members: &[&str]) -> TempDir {
    let workspace = TempDir::new().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = [{}]\nresolver = \"3\"\n",
            members
                .iter()
                .map(|member| format!("\"{member}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .unwrap();
    workspace
}

fn lock(workspace: &TempDir) {
    let output = Command::new("cargo")
        .args([
            "generate-lockfile",
            "--offline",
            "--manifest-path",
            workspace.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn query(workspace: &TempDir, import: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .env("CARGO_TARGET_DIR", workspace.path().join("target"))
        .args([
            import,
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .output()
        .unwrap()
}

fn assert_definition(output: Output, definition: &str) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == definition),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn equivalent_local_globs_resolve_once_but_distinct_definitions_are_ambiguous() {
    let workspace = workspace(&["app", "facade"]);
    write_member(&workspace, "app", "facade = { path = \"../facade\" }", "");
    write_member(
        &workspace,
        "facade",
        "",
        "pub struct Thing;\npub mod a { pub use crate::Thing; }\npub mod b { pub use crate::Thing; }\npub mod api { pub use crate::a::*; pub use crate::b::*; }\n",
    );
    lock(&workspace);

    assert_definition(
        query(&workspace, "use facade::api::Thing;"),
        "definition: pub struct Thing;",
    );

    fs::write(
        workspace.path().join("facade/src/lib.rs"),
        "pub struct Thing;\npub mod a { pub use crate::Thing; }\npub mod b { pub struct Thing; }\npub mod api { pub use crate::a::*; pub use crate::b::*; }\n",
    )
    .unwrap();
    let output = query(&workspace, "use facade::api::Thing;");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ambiguous"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn type_only_paths_ignore_unrelated_globbed_value_namespaces() {
    let workspace = workspace(&["app", "facade", "external_donor"]);
    write_member(&workspace, "app", "facade = { path = \"../facade\" }", "");
    write_member(
        &workspace,
        "facade",
        "external_donor = { path = \"../external_donor\" }",
        "pub mod local_donor { #[allow(non_camel_case_types)] pub struct local_node; }\npub use local_donor::*;\npub mod local_node { pub struct LocalThing; }\npub use external_donor::*;\npub mod external_node { pub struct ExternalThing; }\n",
    );
    write_member(
        &workspace,
        "external_donor",
        "",
        "#[allow(non_camel_case_types)] pub struct external_node;\n",
    );
    lock(&workspace);

    for (import, definition) in [
        (
            "use facade::local_node::LocalThing;",
            "definition: pub struct LocalThing;",
        ),
        (
            "use facade::external_node::ExternalThing;",
            "definition: pub struct ExternalThing;",
        ),
        (
            "use facade::local_node::{self};",
            "definition: pub mod local_node;",
        ),
        (
            "use facade::external_node::{self};",
            "definition: pub mod external_node;",
        ),
    ] {
        assert_definition(query(&workspace, import), definition);
    }
}

#[test]
fn module_and_extern_aliases_retain_the_cargo_dependency_name() {
    let workspace = workspace(&["app", "facade", "origin"]);
    write_member(&workspace, "app", "facade = { path = \"../facade\" }", "");
    write_member(
        &workspace,
        "facade",
        "dep_alias = { package = \"origin\", path = \"../origin\" }",
        "pub use dep_alias::api as route;\npub fn route() {}\npub use route::Thing;\npub extern crate dep_alias as source_alias;\n",
    );
    write_member(
        &workspace,
        "origin",
        "",
        "pub mod api { pub struct Thing; }\npub struct RootThing;\npub mod nested { pub struct Nested; }\n",
    );
    lock(&workspace);

    for (import, definition) in [
        ("use facade::Thing;", "definition: pub struct Thing;"),
        (
            "use facade::source_alias::RootThing;",
            "definition: pub struct RootThing;",
        ),
        (
            "use facade::source_alias::nested::Nested;",
            "definition: pub struct Nested;",
        ),
    ] {
        assert_definition(query(&workspace, import), definition);
        if import == "use facade::Thing;" {
            let raw: serde_json::Value = serde_json::from_slice(
                &fs::read(
                    workspace
                        .path()
                        .join("target/check-docs/generation/unit-0/doc/facade.json"),
                )
                .unwrap(),
            )
            .unwrap();
            let binding = raw["index"]
                .as_object()
                .unwrap()
                .values()
                .find(|item| item["inner"]["use"]["name"] == "Thing")
                .unwrap();
            let reexport = &binding["inner"]["use"];
            assert_eq!(reexport["source"], "route::Thing");
            let id = reexport["id"].as_u64().unwrap().to_string();
            assert_eq!(
                raw["paths"][&id]["path"],
                serde_json::json!(["origin", "api", "Thing"])
            );
        }
    }
}

#[test]
fn revisiting_a_module_after_consuming_an_alias_segment_is_not_a_cycle() {
    let workspace = workspace(&["app", "facade", "origin"]);
    write_member(&workspace, "app", "facade = { path = \"../facade\" }", "");
    write_member(
        &workspace,
        "facade",
        "origin = { path = \"../origin\" }",
        "pub mod api { pub use crate::api as again; pub use origin::Thing; pub use again::again::Thing as ViaAliases; }\npub mod a { pub use crate::b::*; }\npub mod b { pub use crate::a::*; }\n",
    );
    write_member(&workspace, "origin", "", "pub struct Thing;\n");
    lock(&workspace);

    assert_definition(
        query(&workspace, "use facade::api::again::Thing;"),
        "definition: pub struct Thing;",
    );

    assert_definition(
        query(&workspace, "use facade::api::ViaAliases;"),
        "definition: pub struct Thing;",
    );

    let output = query(&workspace, "use facade::a::Missing;");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not found"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

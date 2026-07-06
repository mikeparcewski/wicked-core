//! P12 SOURCES + DOCS — add a code source (local path / clone), editable markdown docs, and CLI
//! enrichment machinery (recon doc creation via a stub CLI).

use std::sync::Mutex;

/// `WICKED_HOME` is process-global, so tests that set it must not run concurrently.
static HOME_GUARD: Mutex<()> = Mutex::new(());

fn tmp_home(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("wicked-p12-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.to_string_lossy().to_string()
}

#[test]
fn add_source_accepts_local_path_and_rejects_missing() {
    let here = env!("CARGO_MANIFEST_DIR");
    let got = wicked_core::add_source(here, "wicked-core").expect("local path is a valid source");
    assert_eq!(got, here);
    assert!(
        wicked_core::add_source("/no/such/path/xyz", "x").is_err(),
        "a non-existent path is rejected"
    );
}

#[test]
fn docs_roundtrip_create_list_edit() {
    let _g = HOME_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("WICKED_HOME", tmp_home("docs"));
    let path = wicked_core::new_doc("Auth Recon", "first body").expect("create doc");
    let listed = wicked_core::list_docs();
    assert!(
        listed.iter().any(|d| d.title == "Auth Recon"),
        "the new doc is listed, got: {listed:?}"
    );
    assert!(wicked_core::read_doc(&path).unwrap().contains("first body"));

    // Edit + save.
    wicked_core::write_doc(&path, "# Auth Recon\n\nedited body").expect("save edit");
    assert!(wicked_core::read_doc(&path).unwrap().contains("edited body"));
}

#[test]
fn applications_crud_with_repos_and_docs() {
    let _g = HOME_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("WICKED_HOME", tmp_home("apps"));
    use wicked_core::{AppDoc, AppRepo, SeedKind};

    let app = wicked_core::create_app(
        "My App",
        SeedKind::Prompt,
        Some("build a thing".into()),
        None,
        None,
        100,
    )
    .expect("create app");
    assert_eq!(wicked_core::list_apps().len(), 1, "the app is persisted");

    wicked_core::attach_repo(
        &app.id,
        AppRepo {
            name: "core".into(),
            path: "/tmp/x".into(),
            graph_db: "/tmp/x/.wicked/code-graph.db".into(),
            origin: None,
        },
    )
    .unwrap();
    wicked_core::attach_doc(
        &app.id,
        AppDoc {
            title: "Spec".into(),
            path: "/tmp/spec.md".into(),
            indexed: false,
            ..Default::default()
        },
    )
    .unwrap();

    let got = wicked_core::get_app(&app.id).expect("reload");
    assert_eq!(got.seed, SeedKind::Prompt);
    assert_eq!(got.prompt.as_deref(), Some("build a thing"));
    assert_eq!(got.repos.len(), 1, "one repo attached");
    assert_eq!(got.docs.len(), 1, "one doc attached");

    wicked_core::delete_app(&app.id).unwrap();
    assert!(wicked_core::list_apps().is_empty(), "delete removes it");
}

#[test]
fn enrich_creates_a_recon_doc_per_cli() {
    let _g = HOME_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("WICKED_HOME", tmp_home("enrich"));
    let repo = env!("CARGO_MANIFEST_DIR");
    let top = vec![("recall".to_string(), "memory.rs".to_string())];
    // A stub "CLI" that just echoes — exercises the recon-doc machinery without a real agent.
    let clis = vec![("echo".to_string(), "echo {PROMPT}".to_string())];
    let docs = wicked_core::enrich_source(repo, "/tmp/none.db", "wicked-core", &top, &clis, 10);
    assert_eq!(docs.len(), 1, "one recon doc per CLI");
    assert_eq!(docs[0].cli, "echo");
    assert!(
        std::path::Path::new(&docs[0].doc_path).exists(),
        "the recon doc was saved to disk"
    );
    assert!(
        wicked_core::list_docs().iter().any(|d| d.title.contains("echo recon")),
        "the recon doc is listed in the docs view"
    );
}

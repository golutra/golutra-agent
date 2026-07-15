use std::fs;

use golutra_core::SideEffectType;
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[test]
fn plugin_lifecycle_requires_review_and_supports_rollback() {
    let home = tempdir().expect("home");
    let first = package("fixture", "1.0.0", "first");
    let second = package("fixture", "2.0.0", "second");
    let store = PluginStore::new(home.path()).expect("store");

    let first_revision = store.stage(first.path()).expect("stage first");
    assert_eq!(first_revision.state, PluginRevisionState::Staged);
    assert!(matches!(
        store.enable("fixture", &first_revision.revision_id),
        Err(PluginError::InvalidState(_))
    ));
    store
        .review("fixture", &first_revision.revision_id)
        .expect("review first");
    store
        .enable("fixture", &first_revision.revision_id)
        .expect("enable first");

    let second_revision = store.stage(second.path()).expect("stage second");
    store
        .review("fixture", &second_revision.revision_id)
        .expect("review second");
    store
        .enable("fixture", &second_revision.revision_id)
        .expect("enable second");
    assert_eq!(
        store.enabled().expect("enabled")[0].manifest.version,
        "2.0.0"
    );

    let rolled_back = store.rollback("fixture").expect("rollback");
    assert_eq!(rolled_back.revision_id, first_revision.revision_id);
    assert_eq!(
        store.enabled().expect("enabled")[0].manifest.version,
        "1.0.0"
    );
    store.disable("fixture").expect("disable");
    assert!(store.enabled().expect("enabled").is_empty());
}

#[test]
fn enabled_package_checksum_detects_post_review_mutation() {
    let home = tempdir().expect("home");
    let source = package("fixture", "1.0.0", "initial");
    let store = PluginStore::new(home.path()).expect("store");
    let revision = store.stage(source.path()).expect("stage");
    store
        .review("fixture", &revision.revision_id)
        .expect("review");
    store
        .enable("fixture", &revision.revision_id)
        .expect("enable");
    let installed = store.enabled().expect("enabled").remove(0);
    fs::write(installed.package_root.join("server.txt"), "tampered").expect("tamper");

    assert!(matches!(store.enabled(), Err(PluginError::Integrity(_))));
}

#[cfg(unix)]
#[test]
fn staging_rejects_symbolic_links() {
    use std::os::unix::fs::symlink;

    let home = tempdir().expect("home");
    let source = package("fixture", "1.0.0", "initial");
    symlink(
        source.path().join("server.txt"),
        source.path().join("link.txt"),
    )
    .expect("symlink");
    let store = PluginStore::new(home.path()).expect("store");

    assert!(matches!(
        store.stage(source.path()),
        Err(PluginError::InvalidManifest(_))
    ));
}

#[cfg(unix)]
#[test]
fn registry_and_packages_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("home");
    let source = package("fixture", "1.0.0", "initial");
    let store = PluginStore::new(home.path()).expect("store");
    let revision = store.stage(source.path()).expect("stage");
    let state = store.state().expect("state");
    let package = store
        .root()
        .join(&state.plugins[0].revisions[0].package_dir);

    assert_eq!(
        fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(store.root().join("registry.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(package.join("server.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        state.plugins[0].revisions[0].revision_id,
        revision.revision_id
    );
}

fn package(id: &str, version: &str, content: &str) -> tempfile::TempDir {
    let directory = tempdir().expect("package");
    let manifest = PluginManifest {
        schema_version: 1,
        id: id.to_owned(),
        version: version.to_owned(),
        display_name: Some("Fixture".to_owned()),
        description: Some("fixture plugin".to_owned()),
        server: McpServerManifest {
            command: "fixture-server".to_owned(),
            args: Vec::new(),
            env: vec!["FIXTURE_TOKEN".to_owned()],
        },
        permissions: PluginPermissions::default(),
        tools: vec![PluginToolManifest {
            name: "echo".to_owned(),
            description: Some("echo input".to_owned()),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
            output_schema: Some(json!({"type": "object"})),
            side_effect_type: SideEffectType::ExternalSystem,
        }],
    };
    fs::write(
        directory.path().join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).expect("manifest JSON"),
    )
    .expect("manifest");
    fs::write(directory.path().join("server.txt"), content).expect("server");
    directory
}

use std::fs;

use tempfile::tempdir;

use super::*;

#[test]
fn indexes_symbols_references_and_import_edges() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("lib.rs"),
        "use std::path::Path;\npub struct RuntimeHost;\nfn run(host: RuntimeHost) { let _ = host; }\n",
    )
    .expect("source");
    fs::write(
        workspace.path().join("client.ts"),
        "import { join } from 'node:path';\nexport class Client {}\nfunction create(): Client { return new Client(); }\n",
    )
    .expect("source");

    let graph = CodeIntelligence::new(workspace.path())
        .expect("indexer")
        .build()
        .expect("graph");
    let symbols = CodeIntelligence::query_symbols(&graph, "RuntimeHost", 10);
    let references = CodeIntelligence::query_references(&graph, "Client", 10);

    assert_eq!(graph.files_indexed, 2);
    assert_eq!(symbols.matches.len(), 1);
    assert!(!references.references.is_empty());
    assert!(
        graph
            .edges
            .iter()
            .any(|edge| edge.kind == CodeEdgeKind::Imports)
    );
}

#[test]
fn persists_owner_only_index_and_rejects_oversized_input() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("main.py"), "class Agent:\n    pass\n").expect("source");
    let graph = CodeIntelligence::new(workspace.path())
        .expect("indexer")
        .build()
        .expect("graph");
    let path = workspace.path().join("state").join("code-index.json");
    let store = CodeIndexStore::new(&path);
    store.save(&graph).expect("save");

    assert_eq!(store.load().expect("load"), Some(graph));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}

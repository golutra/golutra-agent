//! Conservative project-level verifier discovery.

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::workspace_scan;

const MAX_PROJECT_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_TEST_DISCOVERY_ENTRIES: usize = 256;
const MAX_TEST_DISCOVERY_DEPTH: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProjectVerifier {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub timeout_ms: u64,
    pub expected_exit_code: i32,
    pub max_output_bytes: usize,
}

#[must_use]
pub fn discover_project_verifiers(workspace_root: &Path) -> Vec<DiscoveredProjectVerifier> {
    let candidates = discover_project_verifier_candidates(workspace_root);
    // A workspace can legitimately contain several independent applications. Running every
    // ecosystem's default check would make an unrelated coding task depend on sibling projects.
    // Automatic verification is therefore limited to an unambiguous root project; callers with a
    // polyglot workspace can provide the intended verifier explicitly.
    if candidates.len() == 1 {
        candidates
    } else {
        Vec::new()
    }
}

fn discover_project_verifier_candidates(workspace_root: &Path) -> Vec<DiscoveredProjectVerifier> {
    let mut verifiers = Vec::new();
    if is_regular_project_file(&workspace_root.join("Cargo.toml")) {
        verifiers.push(verifier(
            "cargo_test",
            "cargo",
            ["test", "--workspace", "--all-targets"],
            30 * 60 * 1_000,
        ));
    }
    if let Some(node) = discover_node_verifier(workspace_root) {
        verifiers.push(node);
    }
    if has_python_tests(workspace_root) {
        verifiers.push(verifier(
            "python_tests",
            "python",
            ["-B", "-m", "pytest", "-q", "-p", "no:cacheprovider"],
            15 * 60 * 1_000,
        ));
    }
    if is_regular_project_file(&workspace_root.join("go.mod")) {
        verifiers.push(verifier(
            "go_test",
            "go",
            ["test", "./..."],
            15 * 60 * 1_000,
        ));
    }
    verifiers
}

fn discover_node_verifier(workspace_root: &Path) -> Option<DiscoveredProjectVerifier> {
    let package_json = read_project_file(&workspace_root.join("package.json"))?;
    let package: Value = serde_json::from_slice(&package_json).ok()?;
    let scripts = package.get("scripts")?.as_object()?;
    let script = ["test", "typecheck", "check", "build"]
        .into_iter()
        .find(|name| {
            scripts
                .get(*name)
                .and_then(Value::as_str)
                .is_some_and(is_meaningful_node_script)
        })?;
    let (program, args) = if is_regular_project_file(&workspace_root.join("pnpm-lock.yaml")) {
        ("pnpm", vec![script.to_owned()])
    } else if is_regular_project_file(&workspace_root.join("yarn.lock")) {
        ("yarn", vec![script.to_owned()])
    } else if is_regular_project_file(&workspace_root.join("bun.lock"))
        || is_regular_project_file(&workspace_root.join("bun.lockb"))
    {
        ("bun", vec!["run".to_owned(), script.to_owned()])
    } else {
        (
            "npm",
            vec!["run".to_owned(), script.to_owned(), "--silent".to_owned()],
        )
    };
    Some(DiscoveredProjectVerifier {
        name: format!("node_{script}"),
        program: program.to_owned(),
        args,
        cwd: ".".to_owned(),
        timeout_ms: 15 * 60 * 1_000,
        expected_exit_code: 0,
        max_output_bytes: 2 * 1024 * 1024,
    })
}

fn has_python_tests(workspace_root: &Path) -> bool {
    if is_regular_project_file(&workspace_root.join("pytest.ini"))
        || is_regular_project_file(&workspace_root.join("tox.ini"))
        || read_project_text(&workspace_root.join("pyproject.toml"))
            .is_some_and(|content| content.contains("[tool.pytest"))
        || read_project_text(&workspace_root.join("setup.cfg"))
            .is_some_and(|content| content.contains("[tool:pytest]"))
    {
        return true;
    }

    let has_python_project_marker = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "uv.lock",
        "poetry.lock",
    ]
    .into_iter()
    .any(|path| is_regular_project_file(&workspace_root.join(path)));
    if !has_python_project_marker {
        return false;
    }
    let mut remaining_entries = MAX_TEST_DISCOVERY_ENTRIES;
    contains_python_test_file(&workspace_root.join("tests"), 0, &mut remaining_entries)
}

fn is_regular_project_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn read_project_file(path: &Path) -> Option<Vec<u8>> {
    workspace_scan::read_regular_file_bounded(
        path,
        path.parent()?,
        MAX_PROJECT_MANIFEST_BYTES,
        &CancellationToken::new(),
        Instant::now().checked_add(Duration::from_secs(1))?,
    )
    .ok()
}

fn read_project_text(path: &Path) -> Option<String> {
    String::from_utf8(read_project_file(path)?).ok()
}

fn contains_python_test_file(path: &Path, depth: usize, remaining_entries: &mut usize) -> bool {
    if depth > MAX_TEST_DISCOVERY_DEPTH || *remaining_entries == 0 {
        return false;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries {
        if *remaining_entries == 0 {
            break;
        }
        *remaining_entries = remaining_entries.saturating_sub(1);
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_file() && is_python_test_file(&entry.path()) {
            return true;
        }
        if metadata.file_type().is_dir()
            && contains_python_test_file(&entry.path(), depth.saturating_add(1), remaining_entries)
        {
            return true;
        }
    }
    false
}

fn is_python_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            (name.starts_with("test_") || name.ends_with("_test.py")) && name.ends_with(".py")
        })
}

fn is_meaningful_node_script(script: &str) -> bool {
    let lower = script.trim().to_ascii_lowercase();
    !lower.is_empty()
        && !lower.contains("no test specified")
        && !matches!(lower.as_str(), "exit 0" | "true")
}

fn verifier<const N: usize>(
    name: &str,
    program: &str,
    args: [&str; N],
    timeout_ms: u64,
) -> DiscoveredProjectVerifier {
    DiscoveredProjectVerifier {
        name: name.to_owned(),
        program: program.to_owned(),
        args: args.into_iter().map(ToOwned::to_owned).collect(),
        cwd: ".".to_owned(),
        timeout_ms,
        expected_exit_code: 0,
        max_output_bytes: 2 * 1024 * 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_supported_project_verifier_candidates_without_shell_commands() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").expect("cargo manifest");
        fs::write(
            workspace.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .expect("package manifest");
        fs::write(
            workspace.path().join("pnpm-lock.yaml"),
            "lockfileVersion: 9\n",
        )
        .expect("node lock");
        fs::create_dir(workspace.path().join("tests")).expect("python tests");
        fs::write(
            workspace.path().join("pyproject.toml"),
            "[project]\nname = \"fixture\"\n",
        )
        .expect("python manifest");
        fs::write(
            workspace.path().join("tests/test_fixture.py"),
            "def test_ok(): pass\n",
        )
        .expect("python test");
        fs::write(
            workspace.path().join("go.mod"),
            "module example.test/project\n",
        )
        .expect("go manifest");

        let discovered = discover_project_verifier_candidates(workspace.path());

        assert_eq!(
            discovered
                .iter()
                .map(|verifier| verifier.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cargo_test", "node_test", "python_tests", "go_test"]
        );
        assert!(
            discovered
                .iter()
                .all(|verifier| !verifier.program.contains(' '))
        );
        let python = discovered
            .iter()
            .find(|verifier| verifier.name == "python_tests")
            .expect("python verifier");
        assert_eq!(
            python.args,
            ["-B", "-m", "pytest", "-q", "-p", "no:cacheprovider"]
        );
        assert!(
            discover_project_verifiers(workspace.path()).is_empty(),
            "ambiguous project roots require an explicit verifier"
        );
    }

    #[test]
    fn discovers_one_verifier_for_an_unambiguous_project_root() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").expect("cargo manifest");

        let discovered = discover_project_verifiers(workspace.path());

        assert_eq!(
            discovered
                .iter()
                .map(|verifier| verifier.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cargo_test"]
        );
    }

    #[test]
    fn ignores_the_default_failing_npm_placeholder() {
        let workspace = tempdir().expect("workspace");
        fs::write(
            workspace.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .expect("package manifest");

        assert!(discover_project_verifiers(workspace.path()).is_empty());
    }

    #[test]
    fn generic_tests_directory_does_not_imply_a_python_project() {
        let workspace = tempdir().expect("workspace");
        fs::create_dir(workspace.path().join("tests")).expect("tests");
        fs::write(
            workspace.path().join("tests/example.test.ts"),
            "export {};\n",
        )
        .expect("typescript test");

        assert!(discover_project_verifiers(workspace.path()).is_empty());
    }

    #[test]
    fn python_helper_files_do_not_trigger_pytest_discovery() {
        let workspace = tempdir().expect("workspace");
        fs::write(
            workspace.path().join("pyproject.toml"),
            "[project]\nname = \"fixture\"\n",
        )
        .expect("python manifest");
        fs::create_dir(workspace.path().join("tests")).expect("tests");
        fs::write(
            workspace.path().join("tests/helpers.py"),
            "def fixture(): return 1\n",
        )
        .expect("python helper");

        assert!(discover_project_verifiers(workspace.path()).is_empty());
    }

    #[test]
    fn python_test_discovery_uses_one_global_entry_budget() {
        let workspace = tempdir().expect("workspace");
        let tests = workspace.path().join("tests");
        fs::create_dir(&tests).expect("tests");
        for directory_index in 0..3 {
            let directory = tests.join(format!("group-{directory_index}"));
            fs::create_dir(&directory).expect("test group");
            for file_index in 0..100 {
                fs::write(
                    directory.join(format!("fixture-{file_index}.txt")),
                    "not a Python test\n",
                )
                .expect("fixture");
            }
        }
        let mut remaining_entries = MAX_TEST_DISCOVERY_ENTRIES;

        assert!(!contains_python_test_file(
            &tests,
            0,
            &mut remaining_entries
        ));
        assert_eq!(remaining_entries, 0);
    }

    #[test]
    fn oversized_project_manifests_are_not_loaded() {
        let workspace = tempdir().expect("workspace");
        let oversized = vec![b' '; usize::try_from(MAX_PROJECT_MANIFEST_BYTES).unwrap() + 1];
        fs::write(workspace.path().join("package.json"), oversized).expect("oversized manifest");

        assert!(discover_project_verifiers(workspace.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn project_manifest_symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        fs::write(
            outside.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .expect("outside manifest");
        symlink(
            outside.path().join("package.json"),
            workspace.path().join("package.json"),
        )
        .expect("manifest symlink");

        assert!(discover_project_verifiers(workspace.path()).is_empty());
    }
}

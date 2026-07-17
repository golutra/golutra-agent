use std::{io::Write, process::Stdio};

use golutra_core::{PostTaskJobStatus, VerificationResult};
use golutra_protocol::{RuntimeEvaluationWorkerRequest, RuntimeEvaluationWorkerResponse};

#[test]
fn sealed_worker_emits_a_complete_verified_trace() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_golutra-eval-worker"))
        .arg("--home")
        .arg(home.path())
        .arg("--workspace")
        .arg(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("worker");
    let request = serde_json::to_vec(&RuntimeEvaluationWorkerRequest {
        objective: "hello".to_owned(),
        payload: serde_json::json!({}),
    })
    .expect("request");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&request)
        .expect("write request");
    let output = child.wait_with_output().expect("worker output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: RuntimeEvaluationWorkerResponse =
        serde_json::from_slice(&output.stdout).expect("response");

    assert!(response.trace.integrity.complete);
    assert!(response.trace.evaluation.terminal);
    assert!(response.trace.post_task_jobs.iter().all(|job| matches!(
        job.status,
        PostTaskJobStatus::Succeeded | PostTaskJobStatus::Failed | PostTaskJobStatus::Cancelled
    )));
    assert_eq!(
        response
            .trace
            .verification
            .as_ref()
            .map(|record| record.result),
        Some(VerificationResult::Pass)
    );
}

use tokio::io::BufReader;

use super::*;

#[test]
fn inspect_auto_waits_for_governance_only_when_requested() {
    assert_eq!(
        inspect_wait_condition("auto", SnapshotPanes::Transcript).expect("transcript wait"),
        WaitCondition::TaskTerminal
    );
    assert_eq!(
        inspect_wait_condition("auto", SnapshotPanes::ResponseAndDeveloper)
            .expect("developer wait"),
        WaitCondition::EvaluationTerminal
    );
    assert_eq!(
        parse_wait_condition("event:task-completed:42").expect("event wait"),
        WaitCondition::Event {
            event_type: "task_completed".to_owned(),
            sequence_at_least: Some(42),
        }
    );
}

#[test]
fn row_ranges_and_views_are_strict() {
    assert_eq!(
        parse_row_range("2:9").expect("range"),
        RowRange { start: 2, end: 9 }
    );
    assert_eq!(
        parse_view("response+developer").expect("view"),
        (
            SnapshotScope::CurrentTurn,
            SnapshotPanes::ResponseAndDeveloper
        )
    );
    assert!(parse_row_range("0:1").is_err());
    assert!(parse_view("unknown").is_err());
}

#[cfg(unix)]
#[test]
fn socket_peer_uid_must_match_driver_effective_uid() {
    assert!(validate_peer_uid(501, 501).is_ok());
    let error = validate_peer_uid(502, 501).expect_err("mismatched peer UID");
    assert!(error.to_string().contains("peer_uid_mismatch"));
}

#[tokio::test]
async fn oversized_ndjson_line_is_drained_before_the_next_request() {
    let mut bytes = vec![b'x'; MAX_DRIVER_LINE_BYTES + 1];
    bytes.extend_from_slice(b"\n{}\n");
    let mut reader = BufReader::new(bytes.as_slice());

    let error = read_bounded_line(&mut reader)
        .await
        .expect_err("oversized line");
    assert!(error.contains("exceeds"));
    assert_eq!(
        read_bounded_line(&mut reader)
            .await
            .expect("next line")
            .expect("line"),
        b"{}"
    );
}

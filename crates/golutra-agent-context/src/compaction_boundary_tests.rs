//! 压缩失败时的历史保留与成功摘要的原子安装边界。

use super::*;

fn message(role: ProviderRole, content: String) -> ProviderMessage {
    ProviderMessage {
        role,
        content,
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        metadata: Default::default(),
    }
}

#[test]
fn fallback_finds_previous_summary_beyond_the_recent_fact_scan_limit() {
    let mut previous =
        "Keep public API stable. Pending: verify migrations. Test failure: fixture missing."
            .to_owned();
    for round in 0..4 {
        let envelope = compaction_summary_envelope(
            &previous,
            CompactionSourceRange { start: 0, end: 1 },
            100,
            compaction_source_checksum(&previous),
            512,
        );
        let mut messages = vec![message(
            ProviderRole::User,
            format!("{COMPACTION_SUMMARY_PREFIX}{envelope}"),
        )];
        messages.extend((0..40).map(|index| {
            message(
                ProviderRole::Tool,
                format!(
                    "round {round} observation {index}: {}",
                    "verbose log ".repeat(80)
                ),
            )
        }));
        previous = fallback_compaction_summary(&messages, &[], 512);
        assert!(previous.contains("Keep public API stable"));
        assert!(previous.contains("Pending: verify migrations"));
        assert!(previous.contains("Test failure: fixture missing"));
        assert!(previous.contains(&format!("round {round} observation 39")));
        assert!(estimate_tokens(&previous) <= 512);
    }
}

#[test]
fn oversized_model_summary_does_not_partially_replace_the_baseline() {
    let messages = (0..40)
        .map(|index| {
            message(
                ProviderRole::User,
                format!("observation {index}: {}", "fact ".repeat(200)),
            )
        })
        .collect::<Vec<_>>();
    let mut record = ContextWindowManager::new(4_096)
        .compact_if_needed(TurnId::new(), 0, &messages, &[], 0)
        .unwrap()
        .unwrap();
    let before = record.clone();
    let oversized = format!(
        "## Goal\n{}\n## Remaining Work\nNever drop this ending.",
        "fact ".repeat(8_000)
    );
    assert!(!record.apply_model_summary(&oversized));
    assert_eq!(
        record, before,
        "failed install must preserve messages, provenance and checksum"
    );
    let complete = "## Goal\n保持 API。\n## Remaining Work\nVerify \"quoted\" paths and \\ escapes.\n## Current State and Evidence\nTests not run.";
    assert!(record.apply_model_summary(complete));
    assert_eq!(
        parse_compaction_summary_envelope(&record.summary)
            .unwrap()
            .summary,
        complete
    );
    assert!(record.replacement_estimated_tokens <= record.budget_limit);
}

#[test]
fn complete_summary_budget_includes_json_escaping_and_provenance() {
    let text = "\"\\\n".repeat(120);
    let checksum = compaction_source_checksum(&text);
    let range = CompactionSourceRange { start: 0, end: 5 };
    let rendered =
        complete_compaction_summary_envelope(&text, range.clone(), 90, checksum.clone(), u64::MAX)
            .unwrap();
    let exact_budget = estimate_tokens(&rendered);
    assert!(
        complete_compaction_summary_envelope(
            &text,
            range.clone(),
            90,
            checksum.clone(),
            exact_budget - 1
        )
        .is_none()
    );
    let accepted =
        complete_compaction_summary_envelope(&text, range, 90, checksum, exact_budget).unwrap();
    assert_eq!(
        parse_compaction_summary_envelope(&accepted)
            .unwrap()
            .summary,
        text.trim()
    );
}

#[test]
fn consecutive_fallbacks_preserve_tool_pairs_and_protected_prefix() {
    let protected = message(ProviderRole::System, "Never disclose credentials.".into());
    let mut messages = vec![protected.clone()];
    for round in 0..4 {
        for index in 0..40 {
            let id = format!("call-{round}-{index}");
            let mut call = message(ProviderRole::Assistant, String::new());
            call.tool_calls.push(golutra_agent_llm::ProviderToolCall {
                tool_call_id: id.clone(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({"command":"cargo test"}),
            });
            let mut result = message(ProviderRole::Tool, "test output ".repeat(160));
            result.tool_call_id = Some(id);
            result.tool_name = Some("shell".into());
            messages.extend([call, result]);
        }
        let record = ContextWindowManager::new(16_384)
            .compact_if_needed(TurnId::new(), 1, &messages, &[], 0)
            .unwrap()
            .unwrap();
        messages = record.replacement_messages;
        assert_eq!(messages[0], protected);
        let calls = messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .map(|call| call.tool_call_id.as_str())
            .collect::<HashSet<_>>();
        let results = messages
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<HashSet<_>>();
        assert!(!calls.is_empty());
        assert_eq!(calls, results);
        assert!(record.replacement_estimated_tokens <= record.target_input_tokens);
    }
}

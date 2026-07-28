//! Terminal decision and user-facing completion policy.
//!
//! Keeping this deterministic policy outside the provider loop prevents the
//! model's final wording from deciding task status.  Only the fixed
//! `VerificationRecord` and governor output may choose a terminal action.

use std::collections::BTreeMap;

use golutra_core::{
    BudgetState, LoopAction, LoopDecision, LoopDecisionId, PolicyId, TaskId, ToolResultStatus,
    TurnId, VerificationRecord, VerificationResult, semantic_tool_failure_family,
};
use golutra_tools::ToolExecutionReport;

pub(crate) fn loop_decision(
    task_id: TaskId,
    turn_id: TurnId,
    verification: &VerificationRecord,
    budget_state: BudgetState,
) -> LoopDecision {
    let action = match verification.result {
        VerificationResult::Pass => LoopAction::StopSuccess,
        VerificationResult::Partial => LoopAction::StopPartial,
        VerificationResult::Fail => LoopAction::StopFailed,
        VerificationResult::Unknown => LoopAction::Blocked,
    };

    LoopDecision {
        decision_id: LoopDecisionId::new(),
        task_id,
        turn_id,
        action,
        reason: format!("verification result: {:?}", verification.result),
        evidence_refs: verification.evidence_refs.clone(),
        verification_ref: Some(verification.verification_id),
        policy_ref: Option::<PolicyId>::None,
        budget_state,
        tool_state: "p0_tool_reports_recorded".to_owned(),
        model_state: "p0_provider_response_recorded".to_owned(),
        next_step: None,
    }
}

pub(crate) fn final_message(
    assistant_message: Option<String>,
    tool_reports: &[ToolExecutionReport],
    verification: &VerificationRecord,
) -> Option<String> {
    if verification.result == VerificationResult::Pass
        && assistant_message
            .as_ref()
            .is_some_and(|message| !message.trim().is_empty())
    {
        return assistant_message;
    }

    match verification.result {
        VerificationResult::Pass => {
            let summary = tool_reports
                .iter()
                .rev()
                .map(|report| report.envelope.summary.trim())
                .find(|summary| !summary.is_empty())
                .unwrap_or("verified completion");
            Some(format!("Completed: {summary}"))
        }
        VerificationResult::Partial | VerificationResult::Fail | VerificationResult::Unknown => {
            Some(evidence_backed_failure_message(tool_reports, verification))
        }
    }
}

fn evidence_backed_failure_message(
    tool_reports: &[ToolExecutionReport],
    verification: &VerificationRecord,
) -> String {
    let failed_check = verification
        .checks
        .iter()
        .find(|check| !check.passed)
        .map(|check| check.message.trim())
        .filter(|message| !message.is_empty())
        .or_else(|| verification.residual_risks.first().map(String::as_str))
        .unwrap_or("completion criteria were not proven");
    let failed_reports = tool_reports
        .iter()
        .filter(|report| report.envelope.status != ToolResultStatus::Ok)
        .collect::<Vec<_>>();
    let mut families = BTreeMap::<String, usize>::new();
    for report in &failed_reports {
        let family = semantic_tool_failure_family(
            &report.envelope.tool_name,
            &report.envelope.structured_facts,
        )
        .unwrap_or_else(|| format!("tool:{}", report.envelope.tool_name));
        *families.entry(family).or_default() += 1;
    }
    let dominant_family = families.into_iter().max_by_key(|(_, failures)| *failures);
    let evidence = verification
        .evidence_refs
        .iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut message = format!(
        "Could not complete. Verification {:?}: {}.",
        verification.result,
        bounded_text(failed_check, 240)
    );
    if let Some((family, failures)) = dominant_family {
        message.push_str(&format!(
            " Root cause: `{family}`; failed tool attempts: {failures}."
        ));
    }
    if evidence.is_empty() {
        message.push_str(&format!(
            " Verification record: {}.",
            verification.verification_id
        ));
    } else {
        message.push_str(&format!(" Evidence: {}.", evidence.join(", ")));
    }
    message
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

pub(crate) fn accepts_text_response_without_evidence(
    requires_workspace_evidence: bool,
    assistant_message: Option<&str>,
    tool_reports: &[ToolExecutionReport],
) -> bool {
    !requires_workspace_evidence
        && tool_reports.is_empty()
        && assistant_message.is_some_and(|message| !message.trim().is_empty())
}

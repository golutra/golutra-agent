//! Terminal decision and user-facing completion policy.
//!
//! Keeping this deterministic policy outside the provider loop prevents the
//! model's final wording from deciding task status.  Only the fixed
//! `VerificationRecord` and governor output may choose a terminal action.

use golutra_core::{
    BudgetState, LoopAction, LoopDecision, LoopDecisionId, PolicyId, TaskId, TurnId,
    VerificationRecord, VerificationResult,
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
    let summaries = tool_reports
        .iter()
        .map(|report| report.envelope.summary.trim())
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>();

    if verification.result == VerificationResult::Pass
        && assistant_message
            .as_ref()
            .is_some_and(|message| !message.trim().is_empty())
    {
        return assistant_message;
    }

    if summaries.is_empty() {
        return match verification.result {
            VerificationResult::Pass => Some("Completed.".to_owned()),
            VerificationResult::Partial
            | VerificationResult::Fail
            | VerificationResult::Unknown => {
                Some("Task finished without enough evidence to verify completion.".to_owned())
            }
        };
    }

    match verification.result {
        VerificationResult::Pass => Some(format!("Completed: {}", summaries.join("; "))),
        VerificationResult::Partial | VerificationResult::Fail | VerificationResult::Unknown => {
            Some(format!(
                "Could not fully complete: {}",
                summaries.join("; ")
            ))
        }
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

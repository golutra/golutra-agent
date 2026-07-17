//! Context budget terminal policy.

use golutra_context::ContextError;
use golutra_core::{
    BudgetState, LoopAction, LoopDecision, LoopDecisionId, VerificationId, VerificationRecord,
    VerificationResult,
};
use golutra_verify::VerificationInput;

use super::{AgentLoopOutcome, AgentLoopTraceEvent, AgentTaskRequest, RuntimeVerificationService};

pub(crate) fn outcome<F>(
    request: &AgentTaskRequest,
    error: ContextError,
    trace: &mut F,
) -> AgentLoopOutcome
where
    F: FnMut(AgentLoopTraceEvent) + Send,
{
    let (planned, limit, action) = match error {
        ContextError::BudgetExceeded { planned, limit } => (planned, limit, LoopAction::Blocked),
        ContextError::UserActionRequired { planned, limit } => {
            (planned, limit, LoopAction::AskUser)
        }
    };
    let reason = format!("context budget exceeded: planned {planned} > limit {limit}");
    trace(AgentLoopTraceEvent::LoopGuardTriggered {
        trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
        reason: reason.clone(),
    });
    let verification = VerificationRecord {
        verification_id: VerificationId::new(),
        task_id: request.task_id,
        objective: request.objective.clone(),
        completion_criteria: request.completion_criteria.clone(),
        checks: Vec::new(),
        evidence_refs: Vec::new(),
        result: VerificationResult::Unknown,
        policy_status: "context_guard_blocked".to_owned(),
        residual_risks: vec![reason.clone()],
    };
    let verification_plan = RuntimeVerificationService::default().plan(&VerificationInput {
        task_id: request.task_id,
        objective: request.objective.clone(),
        completion_criteria: request.completion_criteria.clone(),
        evidence_refs: Vec::new(),
        command_checks: Vec::new(),
        requires_workspace_evidence: request.touched_code,
        code_files_changed: request.touched_code,
    });
    trace(AgentLoopTraceEvent::VerificationPlanned(
        verification_plan.clone(),
    ));
    let final_message = format!(
        "Cannot continue because the context budget is exhausted ({planned} > {limit}). Compact the conversation or reduce the request."
    );
    trace(AgentLoopTraceEvent::AssistantMessage {
        turn_id: request.turn_id,
        content: final_message.clone(),
    });
    AgentLoopOutcome {
        loop_decision: LoopDecision {
            decision_id: LoopDecisionId::new(),
            task_id: request.task_id,
            turn_id: request.turn_id,
            action,
            reason,
            evidence_refs: Vec::new(),
            verification_ref: Some(verification.verification_id),
            policy_ref: None,
            budget_state: BudgetState {
                planned_input_tokens: Some(planned),
                actual_input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                estimated_cost: None,
                budget_remaining: Some(0),
                compact_recommended: true,
                cost_risk: "blocked".to_owned(),
            },
            tool_state: "not_started_context_guard".to_owned(),
            model_state: "not_started_context_guard".to_owned(),
            next_step: Some("compact context or reduce the request".to_owned()),
        },
        verification,
        verification_plan,
        tool_reports: Vec::new(),
        final_message: Some(final_message),
        final_turn_id: request.turn_id,
    }
}

//! 在原生调用前校验目标与控制权；未知结果不能通过新建 Space 或读一次快照绕过。
use serde_json::Value;

use super::{ExternalToolOutput, failure, state::BrowserState};

pub(super) fn prepare(
    state: &mut BrowserState,
    action: &str,
    args: &Value,
) -> Option<ExternalToolOutput> {
    if state.phase == "running" {
        state.phase = "outcome_unknown".into();
        state.requires_reload = true;
    }
    if ["status", "help"].contains(&action) {
        return None;
    }
    if state.phase == "ended" {
        return Some(failure(
            "browser_task_ended",
            "Ego ended this task without a confirmed retained result; do not claim it. Use a new session for a new goal.",
        ));
    }
    if state.phase == "waiting_for_user" && !(action == "run" && args["resume_after_user"] == true)
    {
        return Some(failure(
            "user_in_control",
            "Only browser_run resume_after_user:true after user confirmation may resume.",
        ));
    }
    if (state.phase == "outcome_unknown" || state.requires_reload)
        && !(action == "run" && args.get("observation").is_some())
    {
        return Some(failure(
            "browser_outcome_unknown",
            "Use fixed observation without code. If requires_reload is true, observation alone cannot unlock actions; recovery:reload reloads all managed pages and may discard unsaved edits. Never replay uncertain effects.",
        ));
    }
    let initial = state.phase == "idle" || state.phase == "finished";
    if args["new_goal"] == true && !initial {
        return Some(failure(
            "browser_goal_active",
            "Finish the current goal before starting a new one; new_goal cannot bypass a stop or unknown outcome.",
        ));
    }
    if initial && (args.get("observation").is_some() || ["handoff", "finish"].contains(&action)) {
        return Some(failure(
            "no_active_browser_task",
            "No active task; observations never create or claim a Space. Use browser_run code to continue a retained goal.",
        ));
    }
    if action == "run" && initial {
        let reclaim = state.phase == "finished" && state.reclaimable && args["new_goal"] != true;
        if reclaim && args.get("page").is_some() {
            return Some(failure(
                "browser_labels_expired",
                "Finished labels have expired. Omit page when resuming; use the provided page and newly reported labels.",
            ));
        }
        state.reclaim_pending = reclaim;
        if !reclaim {
            state.generation += 1;
            state.space_id = None;
            state.reclaimable = false;
            state.default_page = None;
            state.requires_reload = false;
        }
    }
    None
}

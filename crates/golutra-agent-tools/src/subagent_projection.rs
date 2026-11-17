//! One result coverage contract for direct waits and background delivery.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{bounded_text, serialized_value_len};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FindingsCoverage {
    offset: usize,
    visible_chars: usize,
    total_chars: usize,
    complete: bool,
}

pub(crate) fn describe(facts: &mut Value, content: &str) {
    let offset = facts["child_result_offset"].as_u64().unwrap_or(0) as usize;
    let visible_chars = content.chars().count();
    let total_chars = facts["child_result_total_chars"]
        .as_u64()
        .map_or(offset.saturating_add(visible_chars), |n| n as usize);
    let complete = offset == 0
        && visible_chars == total_chars
        && facts["child_result_has_more"] != true
        && facts["model_visible_truncated"] != true
        && facts["truncated"] != true;
    facts["child_result_coverage"] = json!(FindingsCoverage {
        offset,
        visible_chars,
        total_chars,
        complete,
    });
}

/// A last page, status receipt, or truncated body cannot acknowledge full findings.
#[must_use]
pub fn subagent_findings_complete(facts: &Value, content: &str) -> bool {
    if content.trim().is_empty()
        || facts["child_terminal"] != true
        || facts["child_findings_available"] == false
        || facts["child_result_has_more"] == true
        || facts["model_visible_truncated"] == true
        || facts["truncated"] == true
        || facts["child_result_offset"].as_u64().unwrap_or(0) != 0
    {
        return false;
    }
    match facts.get("child_result_coverage") {
        Some(value) => {
            serde_json::from_value::<FindingsCoverage>(value.clone()).is_ok_and(|coverage| {
                coverage.complete
                    && coverage.offset == 0
                    && coverage.visible_chars == content.chars().count()
                    && coverage.visible_chars == coverage.total_chars
            })
        }
        // Older persisted projections predate explicit coverage.
        None => facts["child_result_total_chars"]
            .as_u64()
            .is_none_or(|total| total == content.chars().count() as u64),
    }
}

fn trim_body(item: &mut Value, body_key: &str, facts_key: &str, keep: usize) {
    let body = item[body_key].as_str().unwrap_or_default();
    let original_chars = body.chars().count();
    if original_chars <= keep {
        return;
    }
    let body: String = body.chars().take(keep).collect();
    item[body_key] = json!(body);
    let facts = &mut item[facts_key];
    let offset = facts["child_result_offset"].as_u64().unwrap_or(0);
    facts["model_visible_truncated"] = json!(true);
    facts["child_result_has_more"] = json!(true);
    facts["child_result_next_offset"] = json!(offset.saturating_add(keep as u64));
    facts["child_result_coverage"]["visible_chars"] = json!(keep);
    facts["child_result_coverage"]["complete"] = json!(false);
}

pub(crate) fn serialize(mut projection: Value, max_bytes: usize) -> String {
    let batch = projection["structured_facts"]["child_results"].is_array();
    if let Some(children) = projection["structured_facts"]["child_results"].as_array_mut() {
        let mut omitted = 0;
        children.retain(|child| {
            if let Some(count) = child["omitted_results"].as_u64() {
                omitted += count;
                false
            } else {
                true
            }
        });
        if omitted > 0 {
            projection["structured_facts"]["omitted_results"] = json!(omitted);
            projection["structured_facts"]["next_action"] =
                json!("read status for remaining requested child handles");
        }
    }
    if !batch {
        let body = projection["model_visible_excerpt"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !body.is_empty() {
            describe(&mut projection["structured_facts"], &body);
        }
    }
    // Short answers retain their full bodies even when another child is verbose.
    while serialized_value_len(&projection) > max_bytes {
        if batch {
            let children = projection["structured_facts"]["child_results"]
                .as_array_mut()
                .expect("batch array");
            let largest = children
                .iter()
                .enumerate()
                .filter_map(|(index, child)| {
                    child["content"]
                        .as_str()
                        .map(|body| (index, body.chars().count()))
                })
                .filter(|(_, chars)| *chars > 0)
                .max_by_key(|(_, chars)| *chars);
            if let Some((index, chars)) = largest {
                trim_body(&mut children[index], "content", "facts", chars / 2);
                continue;
            }
        } else {
            let chars = projection["model_visible_excerpt"]
                .as_str()
                .map_or(0, |body| body.chars().count());
            if chars > 0 {
                trim_body(
                    &mut projection,
                    "model_visible_excerpt",
                    "structured_facts",
                    chars / 2,
                );
                continue;
            }
        }
        break;
    }
    if serialized_value_len(&projection) <= max_bytes {
        return projection.to_string();
    }
    // Metadata may itself exceed a tiny page. Retain per-execution controls;
    // explicitly page the batch instead of replacing every answer with a status.
    projection
        .as_object_mut()
        .expect("projection object")
        .remove("summary");
    if let Some(children) = projection["structured_facts"]["child_results"].as_array_mut() {
        for child in children.iter_mut() {
            if child["facts"]["child_terminal"] != true && child.get("error").is_none() {
                let mut receipt = json!({
                    "child_session_id": child["child_session_id"],
                    "child_status": child["facts"]["child_status"],
                    "model_visible_truncated": true,
                });
                if let Some(task) = child["facts"].get("child_task_id") {
                    receipt["child_task_id"] = task.clone();
                }
                *child = receipt;
                continue;
            }
            child
                .as_object_mut()
                .expect("child object")
                .remove("summary");
            if let Some(facts) = child["facts"].as_object_mut() {
                facts.retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "child_task_id"
                            | "child_status"
                            | "child_terminal"
                            | "child_verification_status"
                            | "child_acceptance_status"
                            | "child_result_next_offset"
                            | "child_result_has_more"
                            | "model_visible_truncated"
                            | "needs_parent_input"
                            | "question_id"
                            | "waiting_for_user"
                    )
                });
            }
            // Keep a compact status alias for old callers of compact batch results.
            if let Some(status) = child.pointer("/facts/child_status").cloned() {
                child["child_status"] = status;
            }
            if let Some(error) = child["error"].as_str() {
                child["error"] = json!(bounded_text(error, 120));
            }
        }
        let mut omitted = projection["structured_facts"]["omitted_results"]
            .as_u64()
            .unwrap_or(0);
        while serialized_value_len(&projection) > max_bytes {
            let children = projection["structured_facts"]["child_results"]
                .as_array_mut()
                .unwrap();
            if children.pop().is_none() {
                break;
            }
            omitted += 1;
            projection["structured_facts"]["omitted_results"] = json!(omitted);
            projection["structured_facts"]["next_action"] =
                json!("read status for remaining requested child handles");
        }
    }
    if serialized_value_len(&projection) <= max_bytes {
        return projection.to_string();
    }
    // Generic control-fact fallback is bounded and never claims full coverage.
    projection["structured_facts"]["model_visible_truncated"] = json!(true);
    crate::serialize_model_tool_projection(
        projection,
        "subagent",
        crate::ToolResultStatus::Ok,
        "Read subagent status with a smaller limit.",
        max_bytes,
    )
}

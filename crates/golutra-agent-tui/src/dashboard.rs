//! Compact plan, task, and usage projections derived from durable events.

use std::collections::BTreeMap;

use golutra_agent_core::{
    TaskStatus, TokenUsageRecord, ToolResultStatus, VerificationAssertionStatus, VerificationResult,
};
use golutra_agent_protocol::{RuntimeEvent, RuntimeEventType};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DashboardTab {
    #[default]
    Plan,
    Tasks,
    Usage,
}

impl DashboardTab {
    pub(crate) const ALL: [Self; 3] = [Self::Plan, Self::Tasks, Self::Usage];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Tasks => "Tasks",
            Self::Usage => "Usage",
        }
    }

    pub(crate) fn cycle(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or_default();
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardState {
    pub(crate) tab: DashboardTab,
    pub(crate) scroll: usize,
}

impl DashboardState {
    pub(crate) fn new(tab: DashboardTab) -> Self {
        Self { tab, scroll: 0 }
    }

    pub(crate) fn set_tab(&mut self, tab: DashboardTab) {
        self.tab = tab;
        self.scroll = 0;
    }

    pub(crate) fn cycle(&mut self, forward: bool) {
        self.set_tab(self.tab.cycle(forward));
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, page_rows: usize) {
        if delta < 0 {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.scroll = self
                .scroll
                .saturating_add(delta as usize)
                .min(page_rows.saturating_mul(1_024));
        }
    }
}

pub(crate) fn dashboard_lines(tab: DashboardTab, events: &[RuntimeEvent]) -> Vec<String> {
    match tab {
        DashboardTab::Plan => plan_lines(events),
        DashboardTab::Tasks => task_lines(events),
        DashboardTab::Usage => usage_lines(events),
    }
}

fn plan_lines(events: &[RuntimeEvent]) -> Vec<String> {
    let latest_task = events.iter().rev().find_map(|event| event.task_id);
    let mut lines = Vec::new();
    for event in events
        .iter()
        .filter(|event| latest_task.is_none() || event.task_id == latest_task)
    {
        let marker = match event.event_type {
            RuntimeEventType::StepStarted => "[~]",
            RuntimeEventType::StepCompleted => "[x]",
            RuntimeEventType::VerificationPlanned => "[ ]",
            RuntimeEventType::VerificationAssertionCompleted => {
                match event
                    .payload
                    .pointer("/assertion/status")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<VerificationAssertionStatus>(v).ok())
                {
                    Some(
                        VerificationAssertionStatus::Pass
                        | VerificationAssertionStatus::NotApplicable,
                    ) => "[x]",
                    Some(VerificationAssertionStatus::Pending) => "[ ]",
                    _ => "[!]",
                }
            }
            RuntimeEventType::VerificationCompleted => match event
                .payload
                .pointer("/record/result")
                .or_else(|| event.payload.get("result"))
                .cloned()
                .and_then(|v| serde_json::from_value::<VerificationResult>(v).ok())
            {
                Some(VerificationResult::Pass) => "[x]",
                _ => "[!]",
            },
            RuntimeEventType::ContinuationDecided => "[~]",
            _ => continue,
        };
        lines.push(format!("{marker} {}", event_summary(event)));
    }
    if lines.is_empty() {
        lines.push("No execution plan has been recorded for this session.".to_owned());
    }
    lines
}

fn task_lines(events: &[RuntimeEvent]) -> Vec<String> {
    let mut tasks = BTreeMap::new();
    let mut background = Vec::new();
    let mut processes = BTreeMap::new();
    for event in events {
        if let Some(task_id) = event.task_id {
            let state = tasks
                .entry(task_id.to_string())
                .or_insert_with(|| "running".to_owned());
            if event.event_type.is_task_terminal() {
                *state = terminal_status(event)
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".into());
            } else if event.event_type == RuntimeEventType::TaskPaused {
                *state = "paused".to_owned();
            } else if event.event_type == RuntimeEventType::TaskResumed {
                *state = "running".to_owned();
            }
        }
        project_process_activity(event, &mut processes);
        if matches!(
            event.event_type,
            RuntimeEventType::PostTaskJobQueued
                | RuntimeEventType::PostTaskJobStarted
                | RuntimeEventType::PostTaskJobCompleted
                | RuntimeEventType::PostTaskJobFailed
        ) {
            background.push(format!("[bg] {}", event_summary(event)));
        }
    }
    let mut lines = tasks
        .into_iter()
        .map(|(task, state)| format!("task {}  {state}", short_id(&task)))
        .collect::<Vec<_>>();
    background.extend(processes.into_values().map(|(_, line)| line));
    if !background.is_empty() {
        lines.push(String::new());
        lines.push("Background activity".to_owned());
        lines.extend(background.into_iter().rev().take(20).rev());
    }
    if lines.is_empty() {
        lines.push("No tasks have been recorded for this session.".to_owned());
    }
    lines
}

fn terminal_status(event: &RuntimeEvent) -> Option<TaskStatus> {
    event
        .payload
        .get("status")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
}

/// 启动与观察按任务和受管进程归并；终态覆盖旧运行标记，不能把工具调用结束当成进程结束。
fn project_process_activity(
    event: &RuntimeEvent,
    processes: &mut BTreeMap<String, (Option<String>, String)>,
) {
    if !matches!(
        event.event_type,
        RuntimeEventType::ToolStarted | RuntimeEventType::ToolCompleted
    ) {
        return;
    }
    let envelope = event.payload.get("envelope").unwrap_or(&event.payload);
    let tool = envelope["tool_name"].as_str().unwrap_or_default();
    if !(tool.starts_with("process_") || matches!(tool, "shell" | "shell_session")) {
        return;
    }
    let Some(call) = envelope["tool_call_id"].as_str() else {
        return;
    };
    let scope = event.task_id.map(|id| id.to_string()).unwrap_or_default();
    let key = format!("{scope}:{call}");
    let process = envelope
        .pointer("/structured_facts/process_id")
        .and_then(|v| v.as_str())
        .map(|id| format!("{scope}:{id}"));
    if let Some(process) = &process {
        processes.retain(|_, (old, _)| old.as_ref() != Some(process));
    }
    let marker = if event.event_type == RuntimeEventType::ToolStarted
        || envelope
            .pointer("/structured_facts/process_state")
            .and_then(|v| v.as_str())
            == Some("running")
    {
        "[~]"
    } else if envelope
        .get("status")
        .cloned()
        .and_then(|v| serde_json::from_value::<ToolResultStatus>(v).ok())
        == Some(ToolResultStatus::Ok)
    {
        "[x]"
    } else {
        "[!]"
    };
    processes.insert(key, (process, format!("{marker} {}", event_summary(event))));
}

/// /status 明确展示当前任务、历史失败和实际构建来源，避免把后续轮完成理解为旧失败已恢复。
pub(crate) fn diagnostic_status_lines(events: &[RuntimeEvent]) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(event) = events.iter().rev().find(|e| {
        e.event_type.is_task_terminal()
            && matches!(
                terminal_status(e),
                Some(TaskStatus::Failed | TaskStatus::Blocked | TaskStatus::Partial)
            )
    }) {
        lines.push(format!(
            "last unsuccessful task {} · {}",
            event.task_id.map(|id| id.to_string()).unwrap_or_default(),
            event_summary(event)
        ));
    }
    if let Some(build) = events
        .iter()
        .rev()
        .find_map(|e| e.payload.pointer("/run_provenance/build"))
    {
        for (label, field) in [
            ("runtime version", "package_version"),
            ("runtime commit", "git_commit"),
            ("runtime source", "source_digest"),
        ] {
            if let Some(value) = build[field].as_str() {
                lines.push(format!("{label} {value}"));
            }
        }
        if build["dirty"] == true {
            lines.push("runtime build contains uncommitted changes".into());
        }
    }
    if let Ok(path) = std::env::current_exe() {
        lines.push(format!("TUI executable {}", path.display()));
    }
    lines
}

fn usage_lines(events: &[RuntimeEvent]) -> Vec<String> {
    let coverage = golutra_agent_protocol::ProviderUsageCoverage::from_events(events);
    let partial = coverage.has_unreported_attempts;
    let mut input = 0_u64;
    let mut uncached_input = 0_u64;
    let mut output = 0_u64;
    let mut reasoning = 0_u64;
    let mut cache_read = 0_u64;
    let mut cache_write = 0_u64;
    let mut provider_total = 0_u64;
    let mut aggregate_total = 0_u64;
    let mut input_complete = true;
    let mut output_complete = true;
    let mut reasoning_complete = true;
    let mut uncached_input_complete = true;
    let mut cache_read_complete = true;
    let mut cache_write_complete = true;
    let mut provider_total_complete = true;
    let mut aggregate_complete = true;
    let mut cost = 0.0_f64;
    let mut cost_complete = !partial;
    let mut samples = 0_u64;
    let mut model = None;
    for event in events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TokenUsageRecorded)
    {
        let Some(record_value) = event.payload.get("record") else {
            continue;
        };
        let Ok(record) = serde_json::from_value::<TokenUsageRecord>(record_value.clone()) else {
            continue;
        };
        let usage = record.usage();
        let input_tokens = usage.input_tokens_total;
        let output_tokens = usage.output_tokens;
        input_complete &= input_tokens.is_some();
        output_complete &= output_tokens.is_some();
        reasoning_complete &= usage.reasoning_tokens.is_some();
        input = input.saturating_add(input_tokens.unwrap_or_default());
        if let Some(tokens) = usage.input_tokens_non_cached {
            uncached_input = uncached_input.saturating_add(tokens);
        } else {
            uncached_input_complete = false;
        }
        output = output.saturating_add(output_tokens.unwrap_or_default());
        reasoning = reasoning.saturating_add(usage.reasoning_tokens.unwrap_or_default());
        if let Some(tokens) = usage.cache_read_tokens {
            cache_read = cache_read.saturating_add(tokens);
        } else {
            cache_read_complete = false;
        }
        if let Some(tokens) = usage.cache_write_tokens {
            cache_write = cache_write.saturating_add(tokens);
        } else {
            cache_write_complete = false;
        }
        if let Some(total) = usage.provider_total_tokens {
            provider_total = provider_total.saturating_add(total);
        } else {
            provider_total_complete = false;
        }
        if let Some(aggregate) = input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output))
        {
            aggregate_total = aggregate_total.saturating_add(aggregate);
        } else {
            aggregate_complete = false;
        }
        match record.estimated_cost {
            Some(value) if value.is_finite() && value >= 0.0 => cost += value,
            _ => cost_complete = false,
        }
        model = Some(record.model_id).or(model);
        samples = samples.saturating_add(1);
    }
    let context = events.iter().rev().find_map(|event| {
        (event.event_type == RuntimeEventType::ContextBuilt).then(|| {
            event
                .payload
                .get("planned_input_tokens")
                .and_then(|value| value.as_u64())
        })?
    });
    let mut lines = vec![
        format!(
            "Model        {}",
            model.unwrap_or_else(|| "not recorded".to_owned())
        ),
        format!("Requests     {}", coverage.logical_requests),
        "Request scope logical (retries share a request)".to_owned(),
        format!(
            "Input        {}",
            usage_value_with_coverage(input, input_complete, partial)
        ),
        format!(
            "Uncached input {}",
            usage_value_with_coverage(uncached_input, uncached_input_complete, partial)
        ),
        format!(
            "Cache read   {}",
            usage_value_with_coverage(cache_read, cache_read_complete, partial)
        ),
        format!(
            "Cache write  {}",
            usage_value_with_coverage(cache_write, cache_write_complete, partial)
        ),
        format!(
            "Output       {}",
            usage_value_with_coverage(output, output_complete, partial)
        ),
        format!(
            "Reasoning    {}",
            usage_value_with_coverage(reasoning, reasoning_complete, partial)
        ),
        format!(
            "Total        {}",
            usage_value_with_coverage(provider_total, provider_total_complete, partial)
        ),
        if cost_complete && samples > 0 {
            format!("Est. cost    ${cost:.6}")
        } else {
            "Est. cost    unknown".to_owned()
        },
    ];
    if !provider_total_complete || aggregate_total != provider_total {
        let aggregate = usage_value_with_coverage(aggregate_total, aggregate_complete, partial);
        lines.insert(7, format!("Aggregate    {aggregate}"));
    }
    if partial {
        lines.push("Usage coverage incomplete: some provider attempts have no usage".to_owned());
    }
    if let Some(context) = context {
        lines.push(format!("Last context {context} planned tokens"));
    }
    if let Some(rate_limit) = events
        .iter()
        .rev()
        .find(|event| event.event_type == RuntimeEventType::ProviderRateLimited)
    {
        lines.push(format!("Rate limit   {}", event_summary(rate_limit)));
    }
    lines
}

fn usage_value_with_coverage(value: u64, complete: bool, partial: bool) -> String {
    if partial && value > 0 {
        format!("at least {value} tokens")
    } else if complete && !partial {
        format!("{value} tokens")
    } else {
        "unknown".to_owned()
    }
}

fn event_summary(event: &RuntimeEvent) -> String {
    event
        .payload
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("runtime event recorded")
        .to_owned()
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_agent_core::{
        EventId, ProviderRequestId, ProviderResponseId, RUNTIME_EVENT_SCHEMA_VERSION, SessionId,
        TaskId, TokenBudgetSnapshotId, TokenUsageRecord, TurnId,
    };
    use golutra_agent_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    #[test]
    fn plan_and_tasks_render_business_outcomes_instead_of_event_names() {
        for (status, marker) in [("pass", "[x]"), ("fail", "[!]"), ("unknown", "[!]")] {
            let e = event(
                1,
                RuntimeEventType::VerificationAssertionCompleted,
                json!({"assertion":{"status":status},"summary":"check"}),
            );
            assert!(plan_lines(&[e])[0].starts_with(marker));
            let e = event(
                2,
                RuntimeEventType::VerificationCompleted,
                json!({"record":{"result":status},"summary":"verification"}),
            );
            assert!(plan_lines(&[e])[0].starts_with(marker));
        }
        for status in ["completed", "failed", "blocked"] {
            let e = event(1, RuntimeEventType::TaskCompleted, json!({"status":status}));
            assert!(task_lines(&[e])[0].ends_with(status));
        }
    }

    #[test]
    fn process_terminal_observation_replaces_running_launch_marker() {
        let mut events = vec![
            event(
                1,
                RuntimeEventType::ToolStarted,
                json!({"tool_name":"shell","tool_call_id":"start","summary":"launch"}),
            ),
            event(
                2,
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_name":"shell","tool_call_id":"start","status":"ok","structured_facts":{"process_id":"p","process_state":"running"}},"summary":"running"}),
            ),
            event(
                3,
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_name":"shell_session","tool_call_id":"wait","status":"ok","structured_facts":{"process_id":"p","process_state":"exited"}},"summary":"exited"}),
            ),
        ];
        let task = TaskId::new();
        for e in &mut events {
            e.task_id = Some(task);
        }
        let lines = task_lines(&events);
        assert!(lines.iter().any(|l| l == "[x] exited"));
        assert!(!lines.iter().any(|l| l.starts_with("[~]")));
    }

    fn event(
        sequence_no: u64,
        event_type: RuntimeEventType,
        payload: serde_json::Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: RUNTIME_EVENT_SCHEMA_VERSION,
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type,
            source: RuntimeEventSource::Runtime,
            timestamp: Utc::now(),
            payload,
            payload_ref: None,
            durable: true,
            causal_context: Default::default(),
            causal_links: Vec::new(),
        }
    }

    fn usage_record_json(
        input_tokens: u64,
        output_tokens: u64,
        provider_total_tokens: Option<u64>,
        estimated_cost: f64,
    ) -> serde_json::Value {
        usage_record_json_with_breakdown(
            input_tokens,
            output_tokens,
            provider_total_tokens,
            estimated_cost,
            Some(input_tokens),
            None,
            None,
        )
    }

    fn usage_record_json_with_breakdown(
        input_tokens: u64,
        output_tokens: u64,
        provider_total_tokens: Option<u64>,
        estimated_cost: f64,
        non_cached_input_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
    ) -> serde_json::Value {
        serde_json::to_value(TokenUsageRecord {
            session_id: None,
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: "provider".to_owned(),
            model_id: "m".to_owned(),
            request_event_id: ProviderRequestId::new(),
            response_event_id: ProviderResponseId::new(),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            reasoning_tokens: None,
            estimated_cost: Some(estimated_cost),
            budget_snapshot_ref: TokenBudgetSnapshotId::new(),
            attribution_ref: None,
            usage_source: "provider".to_owned(),
            cache_read_tokens,
            cache_write_tokens,
            non_cached_input_tokens,
            tool_schema_tokens_estimated: None,
            tool_result_tokens_estimated: None,
            tool_estimated_tokens: None,
            provider_total_tokens,
            usage_complete: true,
            cache_identity: None,
        })
        .expect("usage record serializes")
    }

    #[test]
    fn usage_projection_sums_durable_records() {
        let lines = usage_lines(&[
            event(
                1,
                RuntimeEventType::TokenUsageRecorded,
                json!({
                    "record": usage_record_json_with_breakdown(
                        10, 2, Some(12), 0.1, Some(6), Some(4), Some(2)
                    )
                }),
            ),
            event(
                2,
                RuntimeEventType::TokenUsageRecorded,
                json!({
                    "record": usage_record_json_with_breakdown(
                        5, 3, Some(8), 0.2, Some(3), Some(2), Some(0)
                    )
                }),
            ),
        ]);
        assert!(lines.iter().any(|line| line == "Input        15 tokens"));
        assert!(lines.iter().any(|line| line == "Uncached input 9 tokens"));
        assert!(lines.iter().any(|line| line == "Cache read   6 tokens"));
        assert!(lines.iter().any(|line| line == "Cache write  2 tokens"));
        assert!(lines.iter().any(|line| line == "Total        20 tokens"));
    }

    #[test]
    fn usage_projection_marks_unprojected_breakdowns_as_unknown() {
        let lines = usage_lines(&[event(
            1,
            RuntimeEventType::TokenUsageRecorded,
            json!({
                "record": usage_record_json_with_breakdown(
                    10, 2, Some(12), 0.0, None, None, None
                )
            }),
        )]);

        assert!(lines.iter().any(|line| line == "Uncached input unknown"));
        assert!(lines.iter().any(|line| line == "Cache read   unknown"));
        assert!(lines.iter().any(|line| line == "Cache write  unknown"));
    }

    #[test]
    fn usage_projection_marks_missing_provider_total_without_hiding_aggregate() {
        let lines = usage_lines(&[event(
            1,
            RuntimeEventType::TokenUsageRecorded,
            json!({"record": usage_record_json(10, 2, None, 0.0)}),
        )]);

        assert!(lines.iter().any(|line| line == "Total        unknown"));
        assert!(lines.iter().any(|line| line == "Aggregate    12 tokens"));
    }

    #[test]
    fn usage_projection_ignores_old_record_shape() {
        let lines = usage_lines(&[event(
            1,
            RuntimeEventType::TokenUsageRecorded,
            json!({
                "record": {
                    "model_id": "m",
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "cached_input_tokens": 4,
                    "tool_result_tokens": 6,
                    "total_tokens": 12
                }
            }),
        )]);

        assert!(lines.iter().any(|line| line == "Requests     0"));
        assert!(lines.iter().any(|line| line == "Total        0 tokens"));
    }

    #[test]
    fn usage_projection_distinguishes_unknown_cost_from_reported_zero() {
        for cost in [json!(null), json!(-1.0), json!(0.0)] {
            let mut record = usage_record_json(10, 2, Some(12), 0.0);
            record["estimated_cost"] = cost.clone();
            let lines = usage_lines(&[event(
                1,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": record}),
            )]);
            let expected = if cost == json!(0.0) {
                "Est. cost    $0.000000"
            } else {
                "Est. cost    unknown"
            };
            assert!(lines.iter().any(|line| line == expected));
        }
        let mut unknown = usage_record_json(10, 2, Some(12), 0.0);
        unknown["estimated_cost"] = json!(null);
        let lines = usage_lines(&[
            event(
                1,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": usage_record_json(10, 2, Some(12), 0.1)}),
            ),
            event(
                2,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": unknown}),
            ),
        ]);
        assert!(lines.iter().any(|line| line == "Est. cost    unknown"));
    }

    #[test]
    fn usage_projection_keeps_retry_consumption_unknown_after_success() {
        let record = usage_record_json(10, 2, Some(12), 0.1);
        let id = record["request_event_id"].clone();
        let mut events = vec![
            event(
                1,
                RuntimeEventType::ProviderStarted,
                json!({"provider_request_id": id}),
            ),
            event(
                2,
                RuntimeEventType::RetryScheduled,
                json!({"provider_request_id": id, "recovery":{"phase":"waiting", "attempt":1}}),
            ),
            event(
                3,
                RuntimeEventType::RetryScheduled,
                json!({"provider_request_id": id, "recovery":{"phase":"retrying", "attempt":1}}),
            ),
            event(
                4,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": record}),
            ),
            event(
                5,
                RuntimeEventType::ProviderCompleted,
                json!({"provider_request_id": id}),
            ),
        ];
        let lines = usage_lines(&events);
        assert!(lines.iter().any(|line| line == "Requests     1"));
        assert!(
            lines
                .iter()
                .any(|line| line == "Total        at least 12 tokens")
        );
        assert!(lines.iter().any(|line| line == "Est. cost    unknown"));
        events.retain(|event| event.event_type != RuntimeEventType::RetryScheduled);
        assert!(
            usage_lines(&events)
                .iter()
                .any(|line| line == "Total        12 tokens")
        );
        events.push(event(
            6,
            RuntimeEventType::ProviderFailed,
            json!({"provider_request_id": ProviderRequestId::new()}),
        ));
        assert!(
            usage_lines(&events)
                .iter()
                .any(|line| line == "Total        at least 12 tokens")
        );
    }

    #[test]
    fn usage_projection_does_not_invent_usage_for_a_failed_request() {
        let lines = usage_lines(&[event(
            1,
            RuntimeEventType::ProviderFailed,
            json!({"provider_request_id": ProviderRequestId::new()}),
        )]);
        assert!(lines.iter().any(|line| line == "Requests     1"));
        assert!(lines.iter().any(|line| line == "Total        unknown"));
        assert!(lines.iter().any(|line| line == "Est. cost    unknown"));
    }

    #[test]
    fn usage_projection_honors_incomplete_records_even_without_recovery_events() {
        let mut record = usage_record_json(10, 2, Some(12), 0.1);
        record["usage_complete"] = json!(false);
        let lines = usage_lines(&[event(
            1,
            RuntimeEventType::TokenUsageRecorded,
            json!({"record": record}),
        )]);
        assert!(
            lines
                .iter()
                .any(|line| line == "Total        at least 12 tokens")
        );
        assert!(lines.iter().any(|line| line == "Est. cost    unknown"));
    }
}

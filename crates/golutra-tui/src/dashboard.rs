//! Compact plan, task, and usage projections derived from durable events.

use std::collections::BTreeMap;

use golutra_core::TokenUsageRecord;
use golutra_protocol::{RuntimeEvent, RuntimeEventType};

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
                if event
                    .payload
                    .pointer("/assertion/status")
                    .and_then(|value| value.as_str())
                    .is_some_and(|status| status.eq_ignore_ascii_case("passed"))
                {
                    "[x]"
                } else {
                    "[!]"
                }
            }
            RuntimeEventType::VerificationCompleted => "[x]",
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
    for event in events {
        if let Some(task_id) = event.task_id {
            let state = tasks
                .entry(task_id.to_string())
                .or_insert_with(|| "running".to_owned());
            if event.event_type.is_task_terminal() {
                *state = format!("{:?}", event.event_type).to_lowercase();
            } else if event.event_type == RuntimeEventType::TaskPaused {
                *state = "paused".to_owned();
            } else if event.event_type == RuntimeEventType::TaskResumed {
                *state = "running".to_owned();
            }
        }
        if event.event_type == RuntimeEventType::ToolStarted
            && event
                .payload
                .get("tool_name")
                .and_then(|value| value.as_str())
                .is_some_and(|tool| tool.starts_with("process_") || tool == "shell")
        {
            background.push(format!("[~] {}", event_summary(event)));
        }
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

fn usage_lines(events: &[RuntimeEvent]) -> Vec<String> {
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut reasoning = 0_u64;
    let mut cached = 0_u64;
    let mut provider_total = 0_u64;
    let mut aggregate_total = 0_u64;
    let mut provider_total_complete = true;
    let mut aggregate_complete = true;
    let mut cost = 0.0_f64;
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
        input = input.saturating_add(input_tokens.unwrap_or_default());
        output = output.saturating_add(output_tokens.unwrap_or_default());
        reasoning = reasoning.saturating_add(usage.reasoning_tokens.unwrap_or_default());
        cached = cached.saturating_add(usage.cache_read_tokens.unwrap_or_default());
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
        cost += record.estimated_cost.unwrap_or_default();
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
        format!("Requests     {samples}"),
        format!("Input        {input} tokens"),
        format!("Cached       {cached} tokens"),
        format!("Output       {output} tokens"),
        format!("Reasoning    {reasoning} tokens"),
        if provider_total_complete {
            format!("Total        {provider_total} tokens")
        } else {
            "Total        unknown".to_owned()
        },
        format!("Est. cost    ${cost:.6}"),
    ];
    if !provider_total_complete || aggregate_total != provider_total {
        let aggregate = if aggregate_complete {
            format!("{aggregate_total} tokens")
        } else {
            "unknown".to_owned()
        };
        lines.insert(7, format!("Aggregate    {aggregate}"));
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
    use golutra_core::{
        EventId, ProviderRequestId, ProviderResponseId, RUNTIME_EVENT_SCHEMA_VERSION, SessionId,
        TaskId, TokenBudgetSnapshotId, TokenUsageRecord, TurnId,
    };
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

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
            cache_read_tokens: None,
            cache_write_tokens: None,
            non_cached_input_tokens: Some(input_tokens),
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
                json!({"record": usage_record_json(10, 2, Some(12), 0.1)}),
            ),
            event(
                2,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": usage_record_json(5, 3, Some(8), 0.2)}),
            ),
        ]);
        assert!(lines.iter().any(|line| line == "Input        15 tokens"));
        assert!(lines.iter().any(|line| line == "Total        20 tokens"));
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
}

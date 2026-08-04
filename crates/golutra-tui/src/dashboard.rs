//! Compact plan, task, and usage projections derived from durable events.

use std::collections::BTreeMap;

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
    let mut total = 0_u64;
    let mut cost = 0.0_f64;
    let mut samples = 0_u64;
    let mut model = None;
    for event in events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TokenUsageRecorded)
    {
        let Some(record) = event.payload.get("record") else {
            continue;
        };
        input = input.saturating_add(value_u64(record, "input_tokens"));
        output = output.saturating_add(value_u64(record, "output_tokens"));
        reasoning = reasoning.saturating_add(value_u64(record, "reasoning_tokens"));
        cached = cached.saturating_add(value_u64(record, "cached_input_tokens"));
        total = total.saturating_add(value_u64(record, "total_tokens"));
        cost += record
            .get("estimated_cost")
            .and_then(|value| value.as_f64())
            .unwrap_or_default();
        model = record
            .get("model_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .or(model);
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
        format!("Total        {total} tokens"),
        format!("Est. cost    ${cost:.6}"),
    ];
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

fn value_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
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
    use golutra_core::{EventId, RUNTIME_EVENT_SCHEMA_VERSION, SessionId, TaskId};
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

    #[test]
    fn usage_projection_sums_durable_records() {
        let lines = usage_lines(&[
            event(
                1,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": {"model_id": "m", "input_tokens": 10, "output_tokens": 2, "total_tokens": 12, "estimated_cost": 0.1}}),
            ),
            event(
                2,
                RuntimeEventType::TokenUsageRecorded,
                json!({"record": {"model_id": "m", "input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "estimated_cost": 0.2}}),
            ),
        ]);
        assert!(lines.iter().any(|line| line == "Input        15 tokens"));
        assert!(lines.iter().any(|line| line == "Total        20 tokens"));
    }
}

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::{TuiApp, composer_support::queued_prompts, inline_history::wrapped_history_rows};

pub(crate) enum SubmissionMode {
    Submit,
    Queue,
}

impl TuiApp {
    pub(crate) async fn send_enter_prompt(
        &mut self,
        transport: &super::RuntimeTransport,
        prompt: String,
    ) -> miette::Result<()> {
        if super::has_active_task(self) {
            let ack = self
                .submit_runtime_prompt_with_mode(transport, prompt.clone(), true)
                .await?;
            if !ack.is_some_and(|ack| {
                !ack.accepted
                    && ack.reason.as_deref() == Some("steering requires an active runtime task")
            }) {
                return Ok(());
            }
            self.defer_rejected_steer(prompt);
            self.refresh(transport).await?;
            return Ok(());
        }
        self.send_runtime_prompt(transport, prompt).await
    }
}

pub(crate) fn preview_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    if width < 8
        || app.overlay_surface().is_some()
        || app.transcript.search.is_some()
        || app.history_search.is_some()
    {
        return Vec::new();
    }
    let mut pending = queued_prompts(&app.events);
    pending.extend(app.rejected_steer_previews());
    let style = Style::default().fg(app.palette().subtle);
    let mut lines = Vec::new();
    for steer in [true, false] {
        let messages = pending
            .iter()
            .filter(|message| message.steer == steer)
            .collect::<Vec<_>>();
        if messages.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let title = if steer {
            "• Pending current-turn inputs"
        } else {
            "• Queued follow-up inputs"
        };
        lines.push(Line::styled(
            super::truncate_end_to_width(title, width as usize),
            style,
        ));
        if steer {
            lines.push(Line::styled(
                super::truncate_end_to_width("  Esc interrupt and send now", width as usize),
                style,
            ));
        }
        for message in messages.iter().take(3) {
            let source = message
                .prompt
                .lines()
                .take(4)
                .map(|line| Line::from(super::truncate_end_to_width(line, usize::from(width) * 4)))
                .collect();
            let rows = wrapped_history_rows(source, width.saturating_sub(4));
            for (index, row) in rows.iter().take(3).enumerate() {
                let text = row
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let prefix = if index == 0 { "  ↳ " } else { "    " };
                lines.push(Line::styled(format!("{prefix}{text}"), style));
            }
            if rows.len() > 3 || message.prompt.lines().take(5).count() > 4 {
                lines.push(Line::styled("    …", style));
            }
        }
        if messages.len() > 3 {
            lines.push(Line::styled(
                format!("  … {} more", messages.len() - 3),
                style,
            ));
        }
    }
    if !lines.is_empty() {
        lines.push(Line::from(Span::styled(
            super::truncate_end_to_width("  Alt+↑ edit last · Alt+Q manage", width as usize),
            style,
        )));
        lines.push(Line::default());
    }
    lines
}

pub(crate) fn visible_height(app: &TuiApp, width: u16, height: u16) -> u16 {
    let requested = preview_lines(app, width).len() as u16;
    let base = super::render::bottom_pane_height_for_width_without_popups(app, width)
        .saturating_sub(requested);
    requested.min(height.saturating_sub(base))
}

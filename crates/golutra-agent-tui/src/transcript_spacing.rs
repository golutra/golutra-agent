//! 统一消息边界间距；分隔行独立于正文，不能计入流式正文的提交游标。

use golutra_agent_core::EventId;
use ratatui::text::Line;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TranscriptTail {
    pub(crate) event_id: Option<EventId>,
    has_content: bool,
    ends_with_blank: bool,
}

impl TranscriptTail {
    pub(crate) fn needs_separator(&self, first: &Line<'_>, continuation: bool) -> bool {
        self.has_content && !continuation && !self.ends_with_blank && !blank_line(first)
    }

    pub(crate) fn observe(&mut self, last: &Line<'_>, event_id: Option<EventId>) {
        self.has_content = true;
        self.ends_with_blank = blank_line(last);
        self.event_id = event_id;
    }

    pub(crate) fn append(
        &mut self,
        target: &mut Vec<Line<'static>>,
        fragment: &[Line<'static>],
        event_id: Option<EventId>,
    ) {
        let (Some(first), Some(last)) = (fragment.first(), fragment.last()) else {
            return;
        };
        let continuation = event_id.is_some() && self.event_id == event_id;
        if self.needs_separator(first, continuation) {
            target.push(Line::default());
        }
        target.extend_from_slice(fragment);
        self.observe(last, event_id);
    }

    pub(crate) fn append_contiguous(
        &mut self,
        target: &mut Vec<Line<'static>>,
        fragment: &[Line<'static>],
        event_id: Option<EventId>,
    ) {
        if let Some(last) = fragment.last() {
            target.extend_from_slice(fragment);
            self.observe(last, event_id);
        }
    }
}

fn blank_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_boundaries_are_spaced_but_stream_fragments_are_not() {
        let first = EventId::new();
        let second = EventId::new();
        let mut tail = TranscriptTail::default();
        let mut lines = Vec::new();
        tail.append(&mut lines, &[Line::raw("first")], Some(first));
        tail.append(&mut lines, &[], Some(second));
        tail.append(&mut lines, &[Line::raw("continuation")], Some(first));
        tail.append(&mut lines, &[Line::raw("second")], Some(second));
        assert_eq!(
            lines,
            vec![
                Line::raw("first"),
                Line::raw("continuation"),
                Line::default(),
                Line::raw("second")
            ]
        );
    }

    #[test]
    fn local_command_breaks_stream_continuation_across_flushes() {
        let event = EventId::new();
        let mut tail = TranscriptTail::default();
        tail.append(&mut Vec::new(), &[Line::raw("stream")], Some(event));
        let mut command = Vec::new();
        tail.append(&mut command, &[Line::raw("status")], None);
        assert_eq!(command, vec![Line::default(), Line::raw("status")]);
        let mut continuation = Vec::new();
        tail.append(&mut continuation, &[Line::raw("rest")], Some(event));
        assert_eq!(continuation, vec![Line::default(), Line::raw("rest")]);
        let mut next_chunk = Vec::new();
        tail.append(&mut next_chunk, &[Line::raw("more")], Some(event));
        assert_eq!(next_chunk, vec![Line::raw("more")]);
    }

    #[test]
    fn existing_paragraph_or_header_blank_supplies_the_separator() {
        for (previous, incoming) in [
            (
                vec![Line::raw("first"), Line::default()],
                vec![Line::raw("second")],
            ),
            (
                vec![Line::raw("first")],
                vec![Line::default(), Line::raw("second")],
            ),
        ] {
            let mut tail = TranscriptTail::default();
            let mut lines = Vec::new();
            tail.append(&mut lines, &previous, Some(EventId::new()));
            tail.append(&mut lines, &incoming, Some(EventId::new()));
            assert_eq!(
                lines,
                vec![Line::raw("first"), Line::default(), Line::raw("second")]
            );
        }
    }
}

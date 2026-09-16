//! Pure interaction state shared by the interactive TUI and deterministic driver.

use super::ComposerInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiMousePress {
    Auth(usize),
    Resume(usize),
    Queue(usize),
    Approval(super::ApprovalChoice),
    QuestionOption { question: usize, option: usize },
    QuestionFreeText { question: usize },
    QuestionSubmit,
    Dashboard(super::DashboardTab),
    Settings(super::SettingsRow),
    Help(super::HelpTopic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiMouseActivation {
    AuthContinue,
    ResumeSession,
    Approval(super::ApprovalChoice),
    QuestionSubmit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum BodyViewMode {
    #[default]
    Auto,
    Transcript,
    Developer,
    Split,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TranscriptPresentation {
    #[default]
    Rich,
    Raw,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TranscriptSearchState {
    pub(crate) input: ComposerInput,
    pub(crate) matches: Vec<usize>,
    pub(crate) selected: usize,
}

impl TranscriptSearchState {
    pub(crate) fn rebuild(&mut self, lines: &[String]) {
        let query = self.input.text().trim().to_lowercase();
        self.matches = if query.is_empty() {
            Vec::new()
        } else {
            lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| line.to_lowercase().contains(&query).then_some(index))
                .collect()
        };
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub(crate) fn current_line(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    pub(crate) fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub(crate) fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or_else(|| self.matches.len().saturating_sub(1));
        }
    }

    pub(crate) fn status(&self) -> String {
        if self.input.text().trim().is_empty() {
            return "type to search transcript".to_owned();
        }
        if self.matches.is_empty() {
            return "no matches".to_owned();
        }
        format!("{} of {}", self.selected + 1, self.matches.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_search_is_case_insensitive_and_wraps_navigation() {
        let mut search = TranscriptSearchState::default();
        search.input.set_text("result");
        search.rebuild(&[
            "First result".to_owned(),
            "unrelated".to_owned(),
            "RESULT two".to_owned(),
        ]);

        assert_eq!(search.matches, vec![0, 2]);
        assert_eq!(search.current_line(), Some(0));
        search.select_previous();
        assert_eq!(search.current_line(), Some(2));
        search.select_next();
        assert_eq!(search.current_line(), Some(0));
    }

    #[test]
    fn transcript_search_clamps_selection_when_query_changes() {
        let mut search = TranscriptSearchState::default();
        search.input.set_text("a");
        search.rebuild(&["a".to_owned(), "a".to_owned(), "b".to_owned()]);
        search.select_next();
        assert_eq!(search.selected, 1);

        search.input.set_text("b");
        search.rebuild(&["a".to_owned(), "a".to_owned(), "b".to_owned()]);
        assert_eq!(search.matches, vec![2]);
        assert_eq!(search.selected, 0);
    }
}

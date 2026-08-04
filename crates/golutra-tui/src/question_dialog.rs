//! Validated single- and multi-select user questions requested by the runtime.

use golutra_core::{
    UserQuestionAnswer, UserQuestionMode, UserQuestionRequest, UserQuestionResolution,
};

use crate::ComposerInput;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum QuestionDialogFocus {
    #[default]
    Options,
    FreeText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionDialogState {
    pub(crate) request: UserQuestionRequest,
    pub(crate) question_index: usize,
    pub(crate) option_index: usize,
    pub(crate) focus: QuestionDialogFocus,
    selections: Vec<Vec<bool>>,
    free_text: Vec<ComposerInput>,
}

impl QuestionDialogState {
    pub(crate) fn new(request: UserQuestionRequest) -> Self {
        let selections = request
            .questions
            .iter()
            .map(|question| vec![false; question.options.len()])
            .collect();
        let free_text = vec![ComposerInput::default(); request.questions.len()];
        Self {
            request,
            question_index: 0,
            option_index: 0,
            focus: QuestionDialogFocus::Options,
            selections,
            free_text,
        }
    }

    pub(crate) fn current_question(&self) -> &golutra_core::UserQuestionPrompt {
        &self.request.questions[self.question_index]
    }

    pub(crate) fn move_option(&mut self, forward: bool) {
        let count = self.current_question().options.len();
        self.focus = QuestionDialogFocus::Options;
        self.option_index = if forward {
            (self.option_index + 1).min(count.saturating_sub(1))
        } else {
            self.option_index.saturating_sub(1)
        };
    }

    pub(crate) fn move_question(&mut self, forward: bool) {
        self.question_index = if forward {
            (self.question_index + 1).min(self.request.questions.len().saturating_sub(1))
        } else {
            self.question_index.saturating_sub(1)
        };
        self.option_index = 0;
    }

    pub(crate) fn toggle_current(&mut self) {
        let question_index = self.question_index;
        let option_index = self.option_index;
        self.focus = QuestionDialogFocus::Options;
        if self.current_question().mode == UserQuestionMode::Single {
            self.selections[question_index].fill(false);
            self.selections[question_index][option_index] = true;
        } else {
            self.selections[question_index][option_index] =
                !self.selections[question_index][option_index];
        }
    }

    pub(crate) fn focus(&mut self, question_index: usize, option_index: usize) -> bool {
        let Some(question) = self.request.questions.get(question_index) else {
            return false;
        };
        if option_index >= question.options.len() {
            return false;
        }
        self.question_index = question_index;
        self.option_index = option_index;
        self.focus = QuestionDialogFocus::Options;
        true
    }

    pub(crate) fn focus_free_text(&mut self, question_index: usize) -> bool {
        if question_index >= self.request.questions.len() {
            return false;
        }
        self.question_index = question_index;
        self.focus = QuestionDialogFocus::FreeText;
        true
    }

    pub(crate) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            QuestionDialogFocus::Options => QuestionDialogFocus::FreeText,
            QuestionDialogFocus::FreeText => QuestionDialogFocus::Options,
        };
    }

    pub(crate) fn current_free_text(&self) -> &ComposerInput {
        &self.free_text[self.question_index]
    }

    pub(crate) fn current_free_text_mut(&mut self) -> &mut ComposerInput {
        &mut self.free_text[self.question_index]
    }

    pub(crate) fn input_bytes(&self) -> usize {
        self.free_text
            .iter()
            .fold(0, |bytes, input| bytes.saturating_add(input.text().len()))
    }

    pub(crate) fn redact_text_with(&mut self, redact: fn(&str) -> String) {
        for question in &mut self.request.questions {
            question.header = redact(&question.header);
            question.question = redact(&question.question);
            for option in &mut question.options {
                option.label = redact(&option.label);
                option.description = option.description.as_deref().map(redact);
            }
        }
        for input in &mut self.free_text {
            input.set_text(redact(input.text()));
        }
    }

    pub(crate) fn is_free_text_focused(&self) -> bool {
        self.focus == QuestionDialogFocus::FreeText
    }

    pub(crate) fn free_text_is_filled(&self) -> bool {
        !self.current_free_text().text().trim().is_empty()
    }

    pub(crate) fn current_answered(&self) -> bool {
        self.selections[self.question_index]
            .iter()
            .any(|selected| *selected)
            || self.free_text_is_filled()
    }

    pub(crate) fn all_answered(&self) -> bool {
        self.selections
            .iter()
            .zip(&self.free_text)
            .all(|(selections, free_text)| {
                selections.iter().any(|selected| *selected) || !free_text.text().trim().is_empty()
            })
    }

    pub(crate) fn is_selected(&self, option_index: usize) -> bool {
        self.selections[self.question_index]
            .get(option_index)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn resolution(&self, reason: impl Into<String>) -> Option<UserQuestionResolution> {
        self.all_answered().then(|| UserQuestionResolution {
            question_id: self.request.question_id,
            answers: self
                .request
                .questions
                .iter()
                .zip(&self.selections)
                .zip(&self.free_text)
                .map(|((question, selections), free_text)| UserQuestionAnswer {
                    question_id: question.id.clone(),
                    selected_option_ids: question
                        .options
                        .iter()
                        .zip(selections)
                        .filter(|(_, selected)| **selected)
                        .map(|(option, _)| option.id.clone())
                        .collect(),
                    free_text: (!free_text.text().trim().is_empty()).then(|| free_text.trimmed()),
                })
                .collect(),
            reason: reason.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{
        QuestionId, TaskId, ToolCallId, TurnId, UserQuestionOption, UserQuestionPrompt,
    };

    use super::*;

    #[test]
    fn dialog_validates_single_and_multiple_answers() {
        let request = UserQuestionRequest {
            question_id: QuestionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            tool_call_id: ToolCallId::new(),
            questions: vec![
                prompt("format", UserQuestionMode::Single),
                prompt("checks", UserQuestionMode::Multiple),
            ],
        };
        let mut dialog = QuestionDialogState::new(request.clone());
        dialog.toggle_current();
        dialog.move_question(true);
        dialog.toggle_current();
        dialog.move_option(true);
        dialog.toggle_current();
        let resolution = dialog.resolution("test").expect("complete answers");
        request
            .validate_resolution(&resolution)
            .expect("valid resolution");
        assert_eq!(resolution.answers[1].selected_option_ids.len(), 2);
    }

    #[test]
    fn dialog_accepts_unicode_free_text_without_splitting_graphemes() {
        let request = UserQuestionRequest {
            question_id: QuestionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            tool_call_id: ToolCallId::new(),
            questions: vec![prompt("format", UserQuestionMode::Single)],
        };
        let mut dialog = QuestionDialogState::new(request.clone());

        assert!(dialog.focus_free_text(0));
        dialog.current_free_text_mut().insert_str("使用 YAML 👍🏽");
        dialog.current_free_text_mut().delete_backward();
        dialog.current_free_text_mut().insert_str("格式");

        let resolution = dialog.resolution("test").expect("free text answer");
        request
            .validate_resolution(&resolution)
            .expect("valid resolution");
        assert!(resolution.answers[0].selected_option_ids.is_empty());
        assert_eq!(
            resolution.answers[0].free_text.as_deref(),
            Some("使用 YAML 格式")
        );
    }

    fn prompt(id: &str, mode: UserQuestionMode) -> UserQuestionPrompt {
        UserQuestionPrompt {
            id: id.to_owned(),
            header: id.to_owned(),
            question: format!("Choose {id}"),
            mode,
            options: vec![
                UserQuestionOption {
                    id: "a".to_owned(),
                    label: "A".to_owned(),
                    description: None,
                },
                UserQuestionOption {
                    id: "b".to_owned(),
                    label: "B".to_owned(),
                    description: None,
                },
            ],
        }
    }
}

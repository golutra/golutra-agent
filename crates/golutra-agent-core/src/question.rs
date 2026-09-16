use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{QuestionId, TaskId, ToolCallId, TurnId};

const MAX_QUESTIONS: usize = 3;
const MAX_OPTIONS: usize = 8;
const MAX_TEXT_CHARS: usize = 2_048;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserQuestionMode {
    #[default]
    Single,
    Multiple,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserQuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserQuestionPrompt {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub mode: UserQuestionMode,
    pub options: Vec<UserQuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserQuestionRequest {
    pub question_id: QuestionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub questions: Vec<UserQuestionPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserQuestionAnswer {
    pub question_id: String,
    pub selected_option_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserQuestionResolution {
    pub question_id: QuestionId,
    pub answers: Vec<UserQuestionAnswer>,
    pub reason: String,
}

impl UserQuestionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.questions.is_empty() || self.questions.len() > MAX_QUESTIONS {
            return Err(format!(
                "questions must contain 1..={MAX_QUESTIONS} entries"
            ));
        }
        let mut question_ids = HashSet::new();
        for question in &self.questions {
            validate_text("question id", &question.id)?;
            validate_text("question header", &question.header)?;
            validate_text("question text", &question.question)?;
            if !question_ids.insert(question.id.as_str()) {
                return Err(format!("duplicate question id `{}`", question.id));
            }
            if question.options.len() < 2 || question.options.len() > MAX_OPTIONS {
                return Err(format!(
                    "question `{}` options must contain 2..={MAX_OPTIONS} entries",
                    question.id
                ));
            }
            let mut option_ids = HashSet::new();
            for option in &question.options {
                validate_text("option id", &option.id)?;
                validate_text("option label", &option.label)?;
                if let Some(description) = &option.description {
                    validate_text("option description", description)?;
                }
                if !option_ids.insert(option.id.as_str()) {
                    return Err(format!(
                        "question `{}` has duplicate option id `{}`",
                        question.id, option.id
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn validate_resolution(&self, resolution: &UserQuestionResolution) -> Result<(), String> {
        if resolution.question_id != self.question_id {
            return Err("question resolution does not match the pending request".to_owned());
        }
        if resolution.answers.len() != self.questions.len() {
            return Err("every pending question requires one answer".to_owned());
        }
        for question in &self.questions {
            let answer = resolution
                .answers
                .iter()
                .find(|answer| answer.question_id == question.id)
                .ok_or_else(|| format!("question `{}` is unanswered", question.id))?;
            let selected = answer.selected_option_ids.iter().collect::<HashSet<_>>();
            if let Some(free_text) = &answer.free_text {
                validate_text(&format!("question `{}` free text", question.id), free_text)?;
            }
            if selected.is_empty() && answer.free_text.is_none() {
                return Err(format!(
                    "question `{}` requires a selection or free text",
                    question.id
                ));
            }
            if question.mode == UserQuestionMode::Single && selected.len() > 1 {
                return Err(format!("question `{}` accepts one selection", question.id));
            }
            if selected.len() != answer.selected_option_ids.len() {
                return Err(format!("question `{}` repeats a selection", question.id));
            }
            if let Some(unknown) = selected.iter().find(|option_id| {
                !question
                    .options
                    .iter()
                    .any(|option| option.id == ***option_id)
            }) {
                return Err(format!(
                    "question `{}` has unknown option `{unknown}`",
                    question.id
                ));
            }
        }
        Ok(())
    }
}

fn validate_text(label: &str, value: &str) -> Result<(), String> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > MAX_TEXT_CHARS {
        return Err(format!(
            "{label} must contain between 1 and {MAX_TEXT_CHARS} characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> UserQuestionRequest {
        UserQuestionRequest {
            question_id: QuestionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            tool_call_id: ToolCallId::new(),
            questions: vec![UserQuestionPrompt {
                id: "format".to_owned(),
                header: "Output".to_owned(),
                question: "Which output format should be used?".to_owned(),
                mode: UserQuestionMode::Single,
                options: vec![
                    UserQuestionOption {
                        id: "json".to_owned(),
                        label: "JSON".to_owned(),
                        description: None,
                    },
                    UserQuestionOption {
                        id: "text".to_owned(),
                        label: "Text".to_owned(),
                        description: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn validates_answers_against_declared_questions() {
        let request = request();
        request.validate().expect("request");
        request
            .validate_resolution(&UserQuestionResolution {
                question_id: request.question_id,
                answers: vec![UserQuestionAnswer {
                    question_id: "format".to_owned(),
                    selected_option_ids: vec!["json".to_owned()],
                    free_text: None,
                }],
                reason: "answered by test".to_owned(),
            })
            .expect("resolution");
    }

    #[test]
    fn accepts_free_text_as_an_answer_or_option_note() {
        let request = request();
        for answer in [
            UserQuestionAnswer {
                question_id: "format".to_owned(),
                selected_option_ids: Vec::new(),
                free_text: Some("Use YAML because another service consumes it.".to_owned()),
            },
            UserQuestionAnswer {
                question_id: "format".to_owned(),
                selected_option_ids: vec!["json".to_owned()],
                free_text: Some("Pretty-print the document.".to_owned()),
            },
        ] {
            request
                .validate_resolution(&UserQuestionResolution {
                    question_id: request.question_id,
                    answers: vec![answer],
                    reason: "answered by test".to_owned(),
                })
                .expect("free text answer");
        }

        let mut multiple = request.clone();
        multiple.questions[0].mode = UserQuestionMode::Multiple;
        multiple
            .validate_resolution(&UserQuestionResolution {
                question_id: multiple.question_id,
                answers: vec![UserQuestionAnswer {
                    question_id: "format".to_owned(),
                    selected_option_ids: vec!["json".to_owned(), "text".to_owned()],
                    free_text: Some("Include a short compatibility note.".to_owned()),
                }],
                reason: "answered by test".to_owned(),
            })
            .expect("multiple selections with notes");
    }

    #[test]
    fn rejects_empty_or_oversized_free_text() {
        let request = request();
        for free_text in ["   ".to_owned(), "x".repeat(MAX_TEXT_CHARS + 1)] {
            let error = request
                .validate_resolution(&UserQuestionResolution {
                    question_id: request.question_id,
                    answers: vec![UserQuestionAnswer {
                        question_id: "format".to_owned(),
                        selected_option_ids: Vec::new(),
                        free_text: Some(free_text),
                    }],
                    reason: "answered by test".to_owned(),
                })
                .expect_err("invalid free text");
            assert!(error.contains("free text"), "{error}");
        }
    }
}

//! Structured approval choices for one pending tool invocation.

use golutra_core::{ApprovalRequest, ApprovalScope};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    #[default]
    Once,
    ResourcePrefix,
    Session,
    Deny,
}

impl ApprovalChoice {
    pub(crate) const ALL: [Self; 4] = [Self::Once, Self::ResourcePrefix, Self::Session, Self::Deny];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Once => "Allow once",
            Self::ResourcePrefix => "Allow matching resource",
            Self::Session => "Allow for this task",
            Self::Deny => "Deny",
        }
    }

    pub(crate) const fn detail(self) -> &'static str {
        match self {
            Self::Once => "Only this tool invocation",
            Self::ResourcePrefix => "Same tool and displayed resource prefix",
            Self::Session => "Later approval requests in this execution chain",
            Self::Deny => "Do not run this invocation",
        }
    }

    pub(crate) const fn scope(self) -> ApprovalScope {
        match self {
            Self::Once | Self::Deny => ApprovalScope::Once,
            Self::ResourcePrefix => ApprovalScope::ResourcePrefix,
            Self::Session => ApprovalScope::Session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalDialogState {
    pub(crate) request: ApprovalRequest,
    pub(crate) selected: usize,
    pub(crate) resource_prefix: String,
}

impl ApprovalDialogState {
    pub(crate) fn new(request: ApprovalRequest) -> Self {
        let resource_prefix = request.resource.clone();
        Self {
            request,
            selected: 0,
            resource_prefix,
        }
    }

    pub(crate) fn selected_choice(&self) -> ApprovalChoice {
        ApprovalChoice::ALL[self.selected.min(ApprovalChoice::ALL.len() - 1)]
    }

    pub(crate) fn move_selection(&mut self, forward: bool) {
        self.selected = if forward {
            (self.selected + 1).min(ApprovalChoice::ALL.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    pub(crate) fn select(&mut self, choice: ApprovalChoice) {
        self.selected = ApprovalChoice::ALL
            .iter()
            .position(|candidate| *candidate == choice)
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{ApprovalId, TaskId, ToolCallId, TurnId};

    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            approval_id: ApprovalId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            tool_call_id: ToolCallId::new(),
            tool_name: "shell".to_owned(),
            resource: "cargo test -p example".to_owned(),
            reason: "process execution requires approval".to_owned(),
        }
    }

    #[test]
    fn approval_selection_is_bounded_and_keeps_exact_resource_prefix() {
        let mut dialog = ApprovalDialogState::new(request());
        assert_eq!(dialog.resource_prefix, "cargo test -p example");
        dialog.move_selection(false);
        assert_eq!(dialog.selected_choice(), ApprovalChoice::Once);
        for _ in 0..8 {
            dialog.move_selection(true);
        }
        assert_eq!(dialog.selected_choice(), ApprovalChoice::Deny);
    }
}

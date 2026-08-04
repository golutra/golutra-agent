//! Session-local provider and execution controls.

use golutra_config::ProviderSettings;
use golutra_llm::{ProviderGenerationConfig, ProviderReasoningEffort};

use super::{ComposerInput, TuiPreferences};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    #[default]
    Guarded,
    Unrestricted,
}

impl PermissionMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Guarded => "guarded",
            Self::Unrestricted => "unrestricted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderChoice {
    pub(crate) profile_name: String,
    pub(crate) model_id: String,
    pub(crate) generation_config: Option<ProviderGenerationConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeControls {
    pub(crate) profile_name: Option<String>,
    pub(crate) model_id: String,
    pub(crate) custom_model: Option<String>,
    pub(crate) base_generation_config: Option<ProviderGenerationConfig>,
    pub(crate) reasoning_effort: Option<ProviderReasoningEffort>,
    pub(crate) reasoning_overridden: bool,
    pub(crate) permission_mode: PermissionMode,
}

impl RuntimeControls {
    pub(crate) fn discover(current_model: &str, yolo: bool) -> (Self, Vec<ProviderChoice>) {
        let settings = golutra_config::ProviderConfigPaths::global()
            .ok()
            .and_then(|paths| golutra_config::load_provider_settings(&paths).ok());
        Self::from_settings(settings.as_ref(), current_model, yolo)
    }

    pub(crate) fn from_settings(
        settings: Option<&ProviderSettings>,
        current_model: &str,
        yolo: bool,
    ) -> (Self, Vec<ProviderChoice>) {
        let choices = settings
            .map(|settings| {
                settings
                    .profiles
                    .iter()
                    .filter(|profile| profile.enabled)
                    .filter_map(|profile| {
                        profile.model_id.as_ref().map(|model_id| ProviderChoice {
                            profile_name: profile.name.clone(),
                            model_id: model_id.clone(),
                            generation_config: profile.generation_config.clone(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected = settings
            .and_then(|settings| settings.active_profile.as_deref())
            .and_then(|active| choices.iter().find(|choice| choice.profile_name == active));
        let model_id = selected
            .map(|choice| choice.model_id.clone())
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| clean_provider_model_label(current_model));
        let base_generation_config = selected.and_then(|choice| choice.generation_config.clone());
        let reasoning_effort = base_generation_config
            .as_ref()
            .and_then(|config| config.reasoning_effort);
        (
            Self {
                profile_name: selected.map(|choice| choice.profile_name.clone()),
                model_id,
                custom_model: None,
                base_generation_config,
                reasoning_effort,
                reasoning_overridden: false,
                permission_mode: if yolo {
                    PermissionMode::Unrestricted
                } else {
                    PermissionMode::Guarded
                },
            },
            choices,
        )
    }

    pub(crate) fn effective_model(&self) -> &str {
        self.custom_model.as_deref().unwrap_or(&self.model_id)
    }

    pub(crate) fn redact_text_with(&mut self, redact: fn(&str) -> String) {
        self.profile_name = self.profile_name.as_deref().map(redact);
        self.model_id = redact(&self.model_id);
        self.custom_model = self.custom_model.as_deref().map(redact);
    }

    pub(crate) fn generation_override(&self) -> Option<ProviderGenerationConfig> {
        self.reasoning_overridden.then(|| {
            let mut config = self.base_generation_config.clone().unwrap_or_default();
            config.reasoning_effort = self.reasoning_effort;
            config
        })
    }

    pub(crate) fn select_profile(&mut self, choice: &ProviderChoice) {
        self.profile_name = Some(choice.profile_name.clone());
        self.model_id = choice.model_id.clone();
        self.custom_model = None;
        self.base_generation_config = choice.generation_config.clone();
        self.reasoning_effort = self
            .base_generation_config
            .as_ref()
            .and_then(|config| config.reasoning_effort);
        self.reasoning_overridden = false;
    }

    pub(crate) fn set_custom_model(&mut self, model: impl Into<String>) -> Result<(), String> {
        let model = model.into();
        let model = model.trim();
        if model.is_empty() || model.chars().count() > 256 {
            return Err("model id must contain between 1 and 256 characters".to_owned());
        }
        self.custom_model = Some(model.to_owned());
        Ok(())
    }

    pub(crate) fn cycle_effort(&mut self, forward: bool) {
        const EFFORTS: [Option<ProviderReasoningEffort>; 5] = [
            None,
            Some(ProviderReasoningEffort::Low),
            Some(ProviderReasoningEffort::Medium),
            Some(ProviderReasoningEffort::High),
            Some(ProviderReasoningEffort::Xhigh),
        ];
        let current = EFFORTS
            .iter()
            .position(|effort| *effort == self.reasoning_effort)
            .unwrap_or_default();
        let next = if forward {
            (current + 1) % EFFORTS.len()
        } else {
            current.checked_sub(1).unwrap_or(EFFORTS.len() - 1)
        };
        self.reasoning_effort = EFFORTS[next];
        self.reasoning_overridden = true;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SettingsRow {
    #[default]
    Profile,
    Model,
    Reasoning,
    Permissions,
    Keymap,
    Theme,
    HighContrast,
    ReducedMotion,
    ScreenReader,
}

impl SettingsRow {
    pub(crate) const ALL: [Self; 9] = [
        Self::Profile,
        Self::Model,
        Self::Reasoning,
        Self::Permissions,
        Self::Keymap,
        Self::Theme,
        Self::HighContrast,
        Self::ReducedMotion,
        Self::ScreenReader,
    ];

    pub(crate) const fn is_runtime_control(self) -> bool {
        matches!(
            self,
            Self::Profile | Self::Model | Self::Reasoning | Self::Permissions
        )
    }

    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|row| *row == self)
            .unwrap_or_default()
    }

    pub(crate) fn move_by(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|row| *row == self)
            .unwrap_or_default();
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            index.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        };
        Self::ALL[next]
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsDialogState {
    pub(crate) draft: RuntimeControls,
    pub(crate) draft_preferences: TuiPreferences,
    pub(crate) choices: Vec<ProviderChoice>,
    pub(crate) selected_row: SettingsRow,
    pub(crate) model_input: ComposerInput,
    pub(crate) editing_model: bool,
    pub(crate) unrestricted_confirmation: bool,
    pub(crate) runtime_locked: bool,
}

impl SettingsDialogState {
    pub(crate) fn new(
        controls: &RuntimeControls,
        choices: &[ProviderChoice],
        preferences: &TuiPreferences,
        runtime_locked: bool,
    ) -> Self {
        let mut model_input = ComposerInput::default();
        model_input.set_text(controls.effective_model());
        Self {
            draft: controls.clone(),
            draft_preferences: preferences.clone(),
            choices: choices.to_vec(),
            selected_row: SettingsRow::Profile,
            model_input,
            editing_model: false,
            unrestricted_confirmation: false,
            runtime_locked,
        }
    }

    pub(crate) fn redact_text_with(&mut self, redact: fn(&str) -> String) {
        self.draft.redact_text_with(redact);
        for choice in &mut self.choices {
            choice.profile_name = redact(&choice.profile_name);
            choice.model_id = redact(&choice.model_id);
        }
        self.model_input.set_text(redact(self.model_input.text()));
    }

    pub(crate) fn cycle_selected(&mut self, forward: bool) -> bool {
        self.unrestricted_confirmation = false;
        if self.runtime_locked && self.selected_row.is_runtime_control() {
            return false;
        }
        match self.selected_row {
            SettingsRow::Profile => self.cycle_profile(forward),
            SettingsRow::Model => self.editing_model = true,
            SettingsRow::Reasoning => self.draft.cycle_effort(forward),
            SettingsRow::Permissions => {
                self.draft.permission_mode = match self.draft.permission_mode {
                    PermissionMode::Guarded => PermissionMode::Unrestricted,
                    PermissionMode::Unrestricted => PermissionMode::Guarded,
                };
            }
            SettingsRow::Keymap => {
                self.draft_preferences.keymap = self.draft_preferences.keymap.cycle(forward);
            }
            SettingsRow::Theme => {
                self.draft_preferences.theme = self.draft_preferences.theme.cycle(forward);
            }
            SettingsRow::HighContrast => {
                self.draft_preferences.high_contrast = !self.draft_preferences.high_contrast;
            }
            SettingsRow::ReducedMotion => {
                self.draft_preferences.reduced_motion = !self.draft_preferences.reduced_motion;
            }
            SettingsRow::ScreenReader => {
                self.draft_preferences.screen_reader = !self.draft_preferences.screen_reader;
            }
        }
        true
    }

    fn cycle_profile(&mut self, forward: bool) {
        if self.choices.is_empty() {
            return;
        }
        let current = self
            .draft
            .profile_name
            .as_deref()
            .and_then(|name| {
                self.choices
                    .iter()
                    .position(|choice| choice.profile_name == name)
            })
            .unwrap_or_default();
        let next = if forward {
            (current + 1) % self.choices.len()
        } else {
            current.checked_sub(1).unwrap_or(self.choices.len() - 1)
        };
        self.draft.select_profile(&self.choices[next]);
        self.model_input.set_text(self.draft.effective_model());
    }

    pub(crate) fn apply_model_input(&mut self) -> Result<(), String> {
        self.draft.set_custom_model(self.model_input.trimmed())?;
        self.editing_model = false;
        Ok(())
    }

    pub(crate) fn can_apply(&mut self) -> bool {
        if self.draft.permission_mode == PermissionMode::Unrestricted
            && !self.unrestricted_confirmation
        {
            self.unrestricted_confirmation = true;
            false
        } else {
            true
        }
    }
}

pub(crate) fn effort_label(value: Option<ProviderReasoningEffort>) -> &'static str {
    match value {
        None => "default",
        Some(ProviderReasoningEffort::Low) => "low",
        Some(ProviderReasoningEffort::Medium) => "medium",
        Some(ProviderReasoningEffort::High) => "high",
        Some(ProviderReasoningEffort::Xhigh) => "xhigh",
    }
}

fn clean_provider_model_label(value: &str) -> String {
    let value = value.trim();
    for suffix in [" xhigh", " high", " medium", " low", " thinking"] {
        if let Some(model) = value.strip_suffix(suffix) {
            return model.to_owned();
        }
    }
    if value.is_empty() {
        "unconfigured".to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use golutra_config::ProviderProfile;

    use super::*;

    #[test]
    fn provider_controls_follow_the_active_profile_without_mutating_settings() {
        let mut settings = ProviderSettings::default();
        let mut first = ProviderProfile::mock();
        first.name = "first".to_owned();
        first.model_id = Some("model-a".to_owned());
        let mut second = ProviderProfile::mock();
        second.name = "second".to_owned();
        second.model_id = Some("model-b".to_owned());
        second.generation_config = Some(ProviderGenerationConfig {
            reasoning_effort: Some(ProviderReasoningEffort::High),
            ..ProviderGenerationConfig::default()
        });
        settings.upsert_profile(first, true);
        settings.upsert_profile(second, false);

        let (mut controls, choices) =
            RuntimeControls::from_settings(Some(&settings), "ignored", false);
        assert_eq!(controls.effective_model(), "model-a");
        controls.select_profile(&choices[1]);
        assert_eq!(controls.profile_name.as_deref(), Some("second"));
        assert_eq!(controls.effective_model(), "model-b");
        assert_eq!(
            controls.reasoning_effort,
            Some(ProviderReasoningEffort::High)
        );
        assert_eq!(settings.active_profile.as_deref(), Some("first"));
    }

    #[test]
    fn unrestricted_settings_require_an_explicit_second_confirmation() {
        let (controls, choices) = RuntimeControls::from_settings(None, "model", false);
        let mut dialog =
            SettingsDialogState::new(&controls, &choices, &TuiPreferences::default(), false);
        dialog.draft.permission_mode = PermissionMode::Unrestricted;

        assert!(!dialog.can_apply());
        assert!(dialog.unrestricted_confirmation);
        assert!(dialog.can_apply());
    }
}

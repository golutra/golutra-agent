//! Durable, non-secret preferences for the interactive terminal surface.

use std::{fs, path::PathBuf};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

const TUI_PREFERENCES_VERSION: u32 = 1;
const TUI_PREFERENCES_FILE: &str = "tui.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum KeymapMode {
    #[default]
    Standard,
    Vim,
}

impl KeymapMode {
    pub(crate) const ALL: [Self; 2] = [Self::Standard, Self::Vim];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Vim => "vim",
        }
    }

    pub(crate) fn cycle(self, forward: bool) -> Self {
        cycle_value(self, &Self::ALL, forward)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ColorTheme {
    #[default]
    Classic,
    Amber,
    Monochrome,
}

impl ColorTheme {
    pub(crate) const ALL: [Self; 3] = [Self::Classic, Self::Amber, Self::Monochrome];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Amber => "amber",
            Self::Monochrome => "monochrome",
        }
    }

    pub(crate) fn cycle(self, forward: bool) -> Self {
        cycle_value(self, &Self::ALL, forward)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TuiPreferences {
    version: u32,
    pub(crate) keymap: KeymapMode,
    pub(crate) theme: ColorTheme,
    pub(crate) high_contrast: bool,
    pub(crate) reduced_motion: bool,
    pub(crate) screen_reader: bool,
    pub(crate) last_seen_version: Option<String>,
}

impl Default for TuiPreferences {
    fn default() -> Self {
        Self {
            version: TUI_PREFERENCES_VERSION,
            keymap: KeymapMode::Standard,
            theme: ColorTheme::Classic,
            high_contrast: false,
            reduced_motion: false,
            screen_reader: false,
            last_seen_version: None,
        }
    }
}

impl TuiPreferences {
    pub(crate) fn global_path() -> Result<PathBuf, String> {
        golutra_config::golutra_home()
            .map(|home| home.join(TUI_PREFERENCES_FILE))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn load_from(path: &std::path::Path) -> Result<Self, String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let preferences = serde_json::from_str::<Self>(&content)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
        if preferences.version != TUI_PREFERENCES_VERSION {
            return Err(format!(
                "unsupported TUI preferences version {} in {}",
                preferences.version,
                path.display()
            ));
        }
        Ok(preferences)
    }

    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("failed to create TUI preferences: {error}"))?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), self)
            .map_err(|error| format!("failed to encode TUI preferences: {error}"))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("failed to sync TUI preferences: {error}"))?;
        temporary
            .persist(path)
            .map_err(|error| format!("failed to save {}: {}", path.display(), error.error))?;
        Ok(())
    }

    pub(crate) const fn palette(&self) -> TuiPalette {
        let mut palette = match self.theme {
            ColorTheme::Classic => TuiPalette {
                text: Color::White,
                muted: Color::DarkGray,
                subtle: Color::Gray,
                accent: Color::Cyan,
                secondary: Color::Magenta,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                selected_foreground: Color::Black,
            },
            ColorTheme::Amber => TuiPalette {
                text: Color::White,
                muted: Color::DarkGray,
                subtle: Color::Gray,
                accent: Color::Yellow,
                secondary: Color::Cyan,
                success: Color::Green,
                warning: Color::LightYellow,
                error: Color::LightRed,
                selected_foreground: Color::Black,
            },
            ColorTheme::Monochrome => TuiPalette {
                text: Color::White,
                muted: Color::DarkGray,
                subtle: Color::Gray,
                accent: Color::White,
                secondary: Color::Gray,
                success: Color::White,
                warning: Color::White,
                error: Color::White,
                selected_foreground: Color::Black,
            },
        };
        if self.high_contrast {
            palette.text = Color::White;
            palette.muted = Color::Gray;
            palette.subtle = Color::White;
            palette.accent = Color::LightCyan;
            palette.secondary = Color::LightMagenta;
            palette.success = Color::LightGreen;
            palette.warning = Color::LightYellow;
            palette.error = Color::LightRed;
        }
        palette
    }

    pub(crate) fn has_unseen_release(&self) -> bool {
        self.last_seen_version.as_deref() != Some(env!("CARGO_PKG_VERSION"))
    }

    pub(crate) fn mark_current_release_seen(&mut self) {
        self.last_seen_version = Some(env!("CARGO_PKG_VERSION").to_owned());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TuiPalette {
    pub(crate) text: Color,
    pub(crate) muted: Color,
    pub(crate) subtle: Color,
    pub(crate) accent: Color,
    pub(crate) secondary: Color,
    pub(crate) success: Color,
    pub(crate) warning: Color,
    pub(crate) error: Color,
    pub(crate) selected_foreground: Color,
}

impl TuiPalette {
    pub(crate) const fn map_color(self, color: Color) -> Color {
        match color {
            Color::White => self.text,
            Color::DarkGray => self.muted,
            Color::Gray => self.subtle,
            Color::Cyan | Color::Blue | Color::LightBlue => self.accent,
            Color::Magenta | Color::LightMagenta => self.secondary,
            Color::Green | Color::LightGreen => self.success,
            Color::Yellow | Color::LightYellow => self.warning,
            Color::Red | Color::LightRed => self.error,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ComposerMode {
    #[default]
    Standard,
    VimInsert,
    VimNormal,
}

impl ComposerMode {
    pub(crate) const fn for_keymap(keymap: KeymapMode) -> Self {
        match keymap {
            KeymapMode::Standard => Self::Standard,
            KeymapMode::Vim => Self::VimInsert,
        }
    }

    pub(crate) const fn label(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::VimInsert => Some("INSERT"),
            Self::VimNormal => Some("NORMAL"),
        }
    }
}

fn cycle_value<T: Copy + PartialEq>(value: T, values: &[T], forward: bool) -> T {
    let index = values
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or_default();
    let next = if forward {
        (index + 1) % values.len()
    } else {
        index.checked_sub(1).unwrap_or(values.len() - 1)
    };
    values[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_without_runtime_or_secret_state() {
        let directory = tempfile::tempdir().expect("preferences directory");
        let path = directory.path().join("tui.json");
        let preferences = TuiPreferences {
            keymap: KeymapMode::Vim,
            theme: ColorTheme::Amber,
            high_contrast: true,
            reduced_motion: true,
            screen_reader: true,
            last_seen_version: Some("0.1.0".to_owned()),
            ..TuiPreferences::default()
        };

        preferences.save_to(&path).expect("save preferences");
        assert_eq!(
            TuiPreferences::load_from(&path).expect("load preferences"),
            preferences
        );
        let content = fs::read_to_string(path).expect("read preferences");
        assert!(!content.contains("credential"));
        assert!(!content.contains("workspace"));
    }

    #[test]
    fn high_contrast_overrides_muted_and_semantic_colors() {
        let preferences = TuiPreferences {
            high_contrast: true,
            ..TuiPreferences::default()
        };
        let palette = preferences.palette();

        assert_eq!(palette.muted, Color::Gray);
        assert_eq!(palette.error, Color::LightRed);
        assert_eq!(palette.accent, Color::LightCyan);
    }
}

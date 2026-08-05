//! Contextual keyboard reference and release notes.

use super::KeymapMode;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum HelpTopic {
    #[default]
    Overview,
    Composer,
    Navigation,
    Runtime,
    WhatsNew,
}

impl HelpTopic {
    pub(crate) const ALL: [Self; 5] = [
        Self::Overview,
        Self::Composer,
        Self::Navigation,
        Self::Runtime,
        Self::WhatsNew,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Composer => "Composer",
            Self::Navigation => "Navigation",
            Self::Runtime => "Runtime",
            Self::WhatsNew => "What's new",
        }
    }

    pub(crate) fn cycle(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|topic| *topic == self)
            .unwrap_or_default();
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            index.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        };
        Self::ALL[next]
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HelpDialogState {
    pub(crate) topic: HelpTopic,
    pub(crate) scroll: usize,
    pub(crate) context: String,
}

impl HelpDialogState {
    pub(crate) fn new(topic: HelpTopic, context: impl Into<String>) -> Self {
        Self {
            topic,
            scroll: 0,
            context: context.into(),
        }
    }

    pub(crate) fn set_topic(&mut self, topic: HelpTopic) {
        self.topic = topic;
        self.scroll = 0;
    }

    pub(crate) fn cycle(&mut self, forward: bool) {
        self.set_topic(self.topic.cycle(forward));
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, max_scroll: usize) {
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(max_scroll);
    }
}

pub(crate) fn help_lines(topic: HelpTopic, keymap: KeymapMode, context: &str) -> Vec<String> {
    match topic {
        HelpTopic::Overview => vec![
            format!("Current context  {context}"),
            format!("Keymap          {}", keymap.label()),
            String::new(),
            "F1 or ?          open and close this reference".to_owned(),
            "/settings        model, permissions, keymap and display".to_owned(),
            "/resume          search and manage sessions".to_owned(),
            "/plan            execution and verification plan".to_owned(),
            "/tasks           foreground and background activity".to_owned(),
            "/usage           tokens, context, cost and rate limits".to_owned(),
            "/terminal [cmd]  suspend Golutra and use the terminal".to_owned(),
            String::new(),
            "Tab switches tabs. Esc returns to the previous surface.".to_owned(),
        ],
        HelpTopic::Composer => composer_help_lines(keymap),
        HelpTopic::Navigation => vec![
            "PageUp/PageDown  scroll transcript views".to_owned(),
            "Home/End         oldest or latest visible content".to_owned(),
            "Ctrl+T           switch full transcript/split observations".to_owned(),
            "Ctrl+F           search transcript".to_owned(),
            "Alt+C            copy transcript through OSC52".to_owned(),
            "Alt+R            rich/raw transcript".to_owned(),
            "Ctrl+O           expand operation details".to_owned(),
            "Mouse wheel      scroll transcript views".to_owned(),
            "Mouse click      select visible choices".to_owned(),
        ],
        HelpTopic::Runtime => vec![
            "Ctrl+Enter       steer an active task".to_owned(),
            "Esc              interrupt an active task".to_owned(),
            "Ctrl+C twice     interrupt, then leave the TUI".to_owned(),
            "Alt+Q            edit or cancel queued prompts".to_owned(),
            "Alt+P            runtime dashboard".to_owned(),
            "/pause           pause an active task".to_owned(),
            "/continue        resume a paused task".to_owned(),
            "/abort           stop an active task".to_owned(),
            "/debug [expand|compact|off]  reload runtime observations".to_owned(),
            String::new(),
            "Approval and question dialogs keep the task paused until resolved.".to_owned(),
        ],
        HelpTopic::WhatsNew => vec![
            format!("Golutra {}", env!("CARGO_PKG_VERSION")),
            String::new(),
            "- Split transcript and runtime observations".to_owned(),
            "- Structured scoped approvals and agent questions".to_owned(),
            "- Searchable session management and runtime dashboards".to_owned(),
            "- Multiline composer, references, attachments and external editor".to_owned(),
            "- Standard/Vim keymaps, themes and accessible display modes".to_owned(),
            "- Inline terminal mode and suspended shell integration".to_owned(),
        ],
    }
}

fn composer_help_lines(keymap: KeymapMode) -> Vec<String> {
    let mut lines = vec![
        "Enter             submit prompt".to_owned(),
        "Shift+Enter       insert newline".to_owned(),
        "Ctrl+Enter        steer active task".to_owned(),
        "Ctrl+A / Ctrl+E   start / end".to_owned(),
        "Alt+B / Alt+F     previous / next word".to_owned(),
        "Ctrl+W            delete previous word".to_owned(),
        "Ctrl+K            delete to line end".to_owned(),
        "Ctrl+Z / Ctrl+Y   undo / redo".to_owned(),
        "Ctrl+R            reverse prompt-history search".to_owned(),
        "Alt+E             external editor".to_owned(),
        "Alt+S             stash or restore draft".to_owned(),
        "@                 complete files, skills and apps".to_owned(),
    ];
    if keymap == KeymapMode::Vim {
        lines.extend([
            String::new(),
            "Esc               normal mode".to_owned(),
            "i/a/I/A           enter insert mode".to_owned(),
            "h/j/k/l, w/b      move in normal mode".to_owned(),
            "0/$               line start / end".to_owned(),
            "x, D, dd          delete character / suffix / line".to_owned(),
            "u / Ctrl+R        undo / redo".to_owned(),
        ]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_help_is_only_shown_for_the_vim_keymap() {
        assert!(
            !help_lines(HelpTopic::Composer, KeymapMode::Standard, "composer")
                .iter()
                .any(|line| line.contains("dd"))
        );
        assert!(
            help_lines(HelpTopic::Composer, KeymapMode::Vim, "composer")
                .iter()
                .any(|line| line.contains("dd"))
        );
    }
}

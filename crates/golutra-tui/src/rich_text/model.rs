//! Semantic Markdown blocks and inline annotations shared by parsing and layout.

use pulldown_cmark::{Alignment, HeadingLevel};

#[derive(Clone, Debug, Default)]
pub(super) struct MarkdownDocument {
    pub(super) blocks: Vec<MarkdownBlock>,
}

#[derive(Clone, Debug)]
pub(super) enum MarkdownBlock {
    Paragraph(RichText),
    Heading {
        level: HeadingLevel,
        content: RichText,
    },
    Quote(Vec<MarkdownBlock>),
    List(MarkdownList),
    Code {
        language: Option<String>,
        source: String,
    },
    Rule,
    Table(MarkdownTable),
}

#[derive(Clone, Debug)]
pub(super) struct MarkdownList {
    pub(super) start: Option<u64>,
    pub(super) items: Vec<Vec<MarkdownBlock>>,
}

#[derive(Clone, Debug)]
pub(super) struct MarkdownTable {
    pub(super) alignments: Vec<Alignment>,
    pub(super) header: Vec<RichText>,
    pub(super) rows: Vec<Vec<RichText>>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RichText {
    pub(super) lines: Vec<Vec<TextRun>>,
}

#[derive(Clone, Debug)]
pub(super) struct TextRun {
    pub(super) text: String,
    pub(super) style: InlineStyle,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct InlineStyle {
    pub(super) emphasis: bool,
    pub(super) strong: bool,
    pub(super) strikethrough: bool,
    pub(super) tone: Option<InlineTone>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum InlineTone {
    Code,
    Link,
    Image,
    CheckedTask,
    UncheckedTask,
    Superscript,
    Subscript,
}

impl InlineStyle {
    pub(super) fn emphasis() -> Self {
        Self {
            emphasis: true,
            ..Self::default()
        }
    }

    pub(super) fn strong() -> Self {
        Self {
            strong: true,
            ..Self::default()
        }
    }

    pub(super) fn strikethrough() -> Self {
        Self {
            strikethrough: true,
            ..Self::default()
        }
    }

    pub(super) fn tone(tone: InlineTone) -> Self {
        Self {
            tone: Some(tone),
            ..Self::default()
        }
    }

    pub(super) fn patch(self, other: Self) -> Self {
        Self {
            emphasis: self.emphasis || other.emphasis,
            strong: self.strong || other.strong,
            strikethrough: self.strikethrough || other.strikethrough,
            tone: other.tone.or(self.tone),
        }
    }
}

impl RichText {
    pub(super) fn push_text(&mut self, value: &str, style: InlineStyle) {
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        for (index, part) in value.split('\n').enumerate() {
            if index > 0 {
                self.lines.push(Vec::new());
            }
            if !part.is_empty() {
                self.lines
                    .last_mut()
                    .expect("rich text has a current line")
                    .push(TextRun {
                        text: part.to_owned(),
                        style,
                    });
            }
        }
    }

    pub(super) fn hard_break(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.lines.push(Vec::new());
    }

    pub(super) fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().map(|run| run.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

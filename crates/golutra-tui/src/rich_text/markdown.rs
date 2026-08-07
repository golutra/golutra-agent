//! Pulldown-cmark adapter that builds a renderer-independent semantic document.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::model::{
    InlineStyle, InlineTone, MarkdownBlock, MarkdownDocument, MarkdownList, MarkdownTable, RichText,
};

pub(super) fn parse_markdown(source: &str) -> MarkdownDocument {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut parser = MarkdownParser::new();
    for event in Parser::new_ext(source, options) {
        parser.event(event);
    }
    parser.finish()
}

enum Container {
    Root(Vec<MarkdownBlock>),
    Quote(Vec<MarkdownBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Item(Vec<MarkdownBlock>),
}

enum InlineKind {
    Paragraph,
    Heading(HeadingLevel),
}

struct InlineBuilder {
    kind: InlineKind,
    content: RichText,
}

struct CodeBuilder {
    language: Option<String>,
    source: String,
}

struct LinkState {
    destination: String,
    label: String,
    image: bool,
}

struct TableBuilder {
    alignments: Vec<Alignment>,
    header: Vec<RichText>,
    rows: Vec<Vec<RichText>>,
    current_row: Option<Vec<RichText>>,
    current_cell: Option<RichText>,
    in_header: bool,
}

impl TableBuilder {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            current_row: None,
            current_cell: None,
            in_header: false,
        }
    }

    fn start_row(&mut self) {
        self.finish_cell();
        if self.current_row.is_some() {
            self.finish_row();
        }
        self.current_row = Some(Vec::new());
    }

    fn start_cell(&mut self) {
        self.finish_cell();
        if self.current_row.is_none() {
            self.current_row = Some(Vec::new());
        }
        self.current_cell = Some(RichText::default());
    }

    fn finish_cell(&mut self) {
        let Some(cell) = self.current_cell.take() else {
            return;
        };
        self.current_row.get_or_insert_with(Vec::new).push(cell);
    }

    fn finish_row(&mut self) {
        self.finish_cell();
        let Some(row) = self.current_row.take() else {
            return;
        };
        if self.in_header && self.header.is_empty() {
            self.header = row;
        } else {
            self.rows.push(row);
        }
    }

    fn finish(mut self) -> MarkdownTable {
        self.finish_row();
        MarkdownTable {
            alignments: self.alignments,
            header: self.header,
            rows: self.rows,
        }
    }
}

struct MarkdownParser {
    containers: Vec<Container>,
    inline: Option<InlineBuilder>,
    code: Option<CodeBuilder>,
    table: Option<TableBuilder>,
    styles: Vec<InlineStyle>,
    links: Vec<LinkState>,
}

impl MarkdownParser {
    fn new() -> Self {
        Self {
            containers: vec![Container::Root(Vec::new())],
            inline: None,
            code: None,
            table: None,
            styles: Vec::new(),
            links: Vec::new(),
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if self.code.is_some() {
            match event {
                Event::Text(text) => {
                    if let Some(code) = &mut self.code {
                        code.source.push_str(&text);
                    }
                }
                Event::End(TagEnd::CodeBlock) => self.finish_code(),
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.append_text(&text, self.current_style()),
            Event::Code(code) => {
                let style = self
                    .current_style()
                    .patch(InlineStyle::tone(InlineTone::Code));
                self.append_text(&code, style);
            }
            Event::SoftBreak => self.append_text(" ", self.current_style()),
            Event::HardBreak => self.hard_break(),
            Event::Rule => {
                self.finish_inline();
                self.push_block(MarkdownBlock::Rule);
            }
            Event::TaskListMarker(checked) => {
                let style = self.current_style().patch(InlineStyle::tone(if checked {
                    InlineTone::CheckedTask
                } else {
                    InlineTone::UncheckedTask
                }));
                self.append_text(if checked { "[x] " } else { "[ ] " }, style);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.append_text(&html, self.current_style());
            }
            Event::FootnoteReference(reference) => {
                self.append_text(&format!("[{reference}]"), self.current_style());
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                self.append_text(&value, self.current_style());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_inline(InlineKind::Paragraph),
            Tag::Heading { level, .. } => self.begin_inline(InlineKind::Heading(level)),
            Tag::BlockQuote(_) => {
                self.finish_inline();
                self.containers.push(Container::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.finish_inline();
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(language) => {
                        let language = language.trim();
                        (!language.is_empty()).then(|| language.to_owned())
                    }
                };
                self.code = Some(CodeBuilder {
                    language,
                    source: String::new(),
                });
            }
            Tag::List(start) => {
                self.finish_inline();
                self.containers.push(Container::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => self.containers.push(Container::Item(Vec::new())),
            Tag::Emphasis => self.styles.push(InlineStyle::emphasis()),
            Tag::Strong => self.styles.push(InlineStyle::strong()),
            Tag::Strikethrough => self.styles.push(InlineStyle::strikethrough()),
            Tag::Link { dest_url, .. } => {
                self.links.push(LinkState {
                    destination: dest_url.into_string(),
                    label: String::new(),
                    image: false,
                });
                self.styles.push(InlineStyle::tone(InlineTone::Link));
            }
            Tag::Image { dest_url, .. } => {
                let style = self
                    .current_style()
                    .patch(InlineStyle::tone(InlineTone::Image));
                self.append_text("[image: ", style);
                self.links.push(LinkState {
                    destination: dest_url.into_string(),
                    label: String::new(),
                    image: true,
                });
                self.styles.push(InlineStyle::tone(InlineTone::Image));
            }
            Tag::Table(alignments) => {
                self.finish_inline();
                self.table = Some(TableBuilder::new(alignments));
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_header = true;
                    table.start_row();
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.start_row();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.start_cell();
                }
            }
            Tag::FootnoteDefinition(_)
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {}
            Tag::Superscript => self.styles.push(InlineStyle::tone(InlineTone::Superscript)),
            Tag::Subscript => self.styles.push(InlineStyle::tone(InlineTone::Subscript)),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.finish_inline(),
            TagEnd::Heading(_) => {
                self.finish_inline();
            }
            TagEnd::BlockQuote(_) => {
                self.finish_inline();
                self.finish_quote();
            }
            TagEnd::List(_) => {
                self.finish_inline();
                self.finish_list();
            }
            TagEnd::Item => {
                self.finish_inline();
                self.finish_item();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => self.finish_link(),
            TagEnd::CodeBlock => self.finish_code(),
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.finish_cell();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.finish_row();
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.finish_row();
                    table.in_header = false;
                }
            }
            TagEnd::Table => self.finish_table(),
            TagEnd::FootnoteDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {}
        }
    }

    fn begin_inline(&mut self, kind: InlineKind) {
        if self.inline.is_some() {
            self.finish_inline();
        }
        self.inline = Some(InlineBuilder {
            kind,
            content: RichText::default(),
        });
    }

    fn finish_inline(&mut self) {
        let Some(inline) = self.inline.take() else {
            return;
        };
        let block = match inline.kind {
            InlineKind::Paragraph => MarkdownBlock::Paragraph(inline.content),
            InlineKind::Heading(level) => MarkdownBlock::Heading {
                level,
                content: inline.content,
            },
        };
        self.push_block(block);
    }

    fn finish_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        self.push_block(MarkdownBlock::Code {
            language: code.language,
            source: code.source,
        });
    }

    fn finish_quote(&mut self) {
        let Some(Container::Quote(blocks)) = self.containers.pop() else {
            return;
        };
        self.push_block(MarkdownBlock::Quote(blocks));
    }

    fn finish_item(&mut self) {
        let Some(Container::Item(blocks)) = self.containers.pop() else {
            return;
        };
        if let Some(Container::List { items, .. }) = self.containers.last_mut() {
            items.push(blocks);
        } else {
            for block in blocks {
                self.push_block(block);
            }
        }
    }

    fn finish_list(&mut self) {
        let Some(Container::List { start, items }) = self.containers.pop() else {
            return;
        };
        self.push_block(MarkdownBlock::List(MarkdownList { start, items }));
    }

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.push_block(MarkdownBlock::Table(table.finish()));
    }

    fn finish_link(&mut self) {
        self.styles.pop();
        let Some(link) = self.links.pop() else {
            return;
        };
        if link.image {
            let style = self
                .current_style()
                .patch(InlineStyle::tone(InlineTone::Image));
            self.append_text("]", style);
        }
        if !link.destination.is_empty() && link.label.trim() != link.destination {
            let style = self
                .current_style()
                .patch(InlineStyle::tone(InlineTone::Link));
            self.append_text(&format!(" ({})", link.destination), style);
        }
    }

    fn append_text(&mut self, value: &str, style: InlineStyle) {
        if let Some(link) = self.links.last_mut() {
            link.label.push_str(value);
        }
        if let Some(table) = &mut self.table {
            table
                .current_cell
                .get_or_insert_with(RichText::default)
                .push_text(value, style);
            return;
        }
        if self.inline.is_none() {
            self.begin_inline(InlineKind::Paragraph);
        }
        if let Some(inline) = &mut self.inline {
            inline.content.push_text(value, style);
        }
    }

    fn hard_break(&mut self) {
        if let Some(table) = &mut self.table {
            table
                .current_cell
                .get_or_insert_with(RichText::default)
                .hard_break();
        } else if let Some(inline) = &mut self.inline {
            inline.content.hard_break();
        }
    }

    fn current_style(&self) -> InlineStyle {
        self.styles
            .iter()
            .copied()
            .fold(InlineStyle::default(), InlineStyle::patch)
    }

    fn push_block(&mut self, block: MarkdownBlock) {
        match self.containers.last_mut() {
            Some(Container::Root(blocks))
            | Some(Container::Quote(blocks))
            | Some(Container::Item(blocks)) => blocks.push(block),
            Some(Container::List { items, .. }) => items.push(vec![block]),
            None => self.containers.push(Container::Root(vec![block])),
        }
    }

    fn finish(mut self) -> MarkdownDocument {
        self.finish_inline();
        self.finish_code();
        self.finish_table();
        while self.containers.len() > 1 {
            match self.containers.last() {
                Some(Container::Quote(_)) => self.finish_quote(),
                Some(Container::Item(_)) => self.finish_item(),
                Some(Container::List { .. }) => self.finish_list(),
                Some(Container::Root(_)) | None => break,
            }
        }
        let blocks = match self.containers.pop() {
            Some(Container::Root(blocks)) => blocks,
            _ => Vec::new(),
        };
        MarkdownDocument { blocks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_keeps_nested_blocks_separate_from_inline_styles() {
        let document = parse_markdown("## Result\n\n- **one**\n  - two\n\n> quoted");

        assert_eq!(document.blocks.len(), 3);
        assert!(matches!(document.blocks[0], MarkdownBlock::Heading { .. }));
        assert!(matches!(document.blocks[1], MarkdownBlock::List(_)));
        assert!(matches!(document.blocks[2], MarkdownBlock::Quote(_)));
    }

    #[test]
    fn parser_collects_table_cells_as_structured_content() {
        let document = parse_markdown("| Name | Status |\n| --- | --- |\n| parser | ready |");
        let MarkdownBlock::Table(table) = &document.blocks[0] else {
            panic!("expected table block");
        };

        assert_eq!(table.header[0].plain_text(), "Name");
        assert_eq!(table.rows[0][1].plain_text(), "ready");
    }
}

use super::*;

#[test]
fn incremental_layout_matches_canonical_markdown_at_every_character() {
    let documents = [
        "你好👨‍👩‍👧‍👦 e\u{301} **强调**\n\n第二段。\n\n# 标题\n\n结尾",
        "intro\n\n```python\nprint('hello')\n# 中文注释\nvalue = 123\n```\n\nafter",
        "```\n\n\nline\nlast line without newline",
        "标题\n===\n\n- one\n- two\n  - nested\n\n> quote\n> continued",
        "| 名称 | value |\n| --- | ---: |\n| a | 1 |\n| long name | 200 |",
        "[early][ref]\n\nparagraph\n\n[ref]: https://example.com\n",
        "- ```rust\n  fn main() {}\n  ```\n\n> ```text\n> a\n> b\n> ```",
        "before\n\n    indented code\n    more code\n\n---\n\nafter\n",
        "before\n\n  # indented heading\n\n  paragraph\n\nend",
        "[before][r]\n\nfirst\n\nsecond\n\n[r]: /target \"title\"\n\n[r]",
        "[r]: /target\n\n[r]\n\nparagraph\n\nmore [r]",
        "before\n\n<div>\nhtml\n\nmore\n</div>\n\nafter",
        "before\n\n<!-- comment\n\ncontinues -->\n\nafter",
        "---\n\n# heading\n\n---\n\nend\n",
    ];
    for width in [1, 5, 20, 80] {
        for source in documents {
            let mut cache = MarkdownCache::default();
            for end in source
                .char_indices()
                .map(|(offset, ch)| offset + ch.len_utf8())
            {
                let partial = &source[..end];
                assert_eq!(
                    cache.render(partial, width),
                    super::super::markdown_lines(partial, width),
                    "width={width}, source={partial:?}"
                );
                assert_eq!(
                    cache.stable_source_end(partial),
                    crate::stream_commit::stable_source_end(partial),
                    "archive boundary: {partial:?}"
                );
                // A second consumer must get identical styled lines from an exact hit.
                assert_eq!(
                    cache.render(partial, width),
                    super::super::markdown_lines(partial, width)
                );
            }
        }
    }
}

#[test]
fn streaming_block_boundaries_match_full_render_across_chunk_sizes() {
    let blocks = [
        "plain e\u{301} 👨‍👩‍👧‍👦 中文 **bold**",
        "- first\n\n  paragraph\n\n- last",
        "> quote\n>\n> - nested",
        "~~~rust\nlet x = 1;\n~~~",
        "    indented\n\n    code",
        "| a | b |\n| --- | --- |\n| x | long |",
        "[link][ref]",
        "[ref]: https://example.com",
        "---",
        "<div>\nhtml\n</div>",
    ];
    for first in blocks {
        for second in blocks {
            let source = format!("intro\n\n{first}\n\n{second}\n\nlast");
            for chunk_size in [1, 7, 31] {
                let mut cache = MarkdownCache::default();
                let ends = source
                    .char_indices()
                    .map(|(i, ch)| i + ch.len_utf8())
                    .collect::<Vec<_>>();
                for chunk in ends.chunks(chunk_size) {
                    let partial = &source[..*chunk.last().unwrap()];
                    assert_eq!(
                        cache.render(partial, 13),
                        super::super::markdown_lines(partial, 13),
                        "chunk_size={chunk_size}, source={partial:?}"
                    );
                    assert_eq!(
                        cache.stable_source_end(partial),
                        crate::stream_commit::stable_source_end(partial)
                    );
                }
            }
        }
    }
}

#[test]
fn completed_block_parse_is_retained_but_late_definitions_recompute_the_source() {
    let mut cache = MarkdownCache::default();
    cache.render("[early][ref]\n\nsecond\n\nlast\n", 40);
    let previous = cache.documents.back().unwrap();
    assert_eq!(previous.stable_blocks, 2);
    assert_eq!(
        &previous.source[..previous.stable_source_len],
        "[early][ref]\n\nsecond\n\n"
    );
    let source = "[early][ref]\n\nsecond\n\nlast\n\n[ref]: /resolved";
    assert_eq!(
        cache.render(source, 40),
        super::super::markdown_lines(source, 40)
    );
    assert!(cache.documents.back().unwrap().source_wide);
}

#[test]
fn prefix_reads_resize_and_final_replacement_do_not_poison_stream_layout() {
    let mut cache = MarkdownCache::default();
    for (text, width) in [
        ("first\n\nsecond", 30),
        ("first\n\n", 30),
        ("first\n\nsecond continued\n\nthird", 30),
        ("first\n\nsecond continued\n\nthird", 8),
        ("corrected **final**", 8),
        ("corrected **final**", 30),
    ] {
        assert_eq!(
            cache.render(text, width),
            super::super::markdown_lines(text, width)
        );
    }
}

#[test]
fn completed_blocks_are_retained_when_a_later_block_grows() {
    let mut cache = MarkdownCache::default();
    cache.render("first\n\nsecond", 40);
    let first = cache.documents.back().unwrap().blocks[0].lines.clone();
    cache.render("first\n\nsecond continued", 40);
    assert!(Arc::ptr_eq(
        &first,
        &cache.documents.back().unwrap().blocks[0].lines
    ));
    for i in 0..MAX_DOCUMENTS * 2 {
        cache.render(&format!("independent message {i}"), 40);
    }
    assert_eq!(cache.documents.len(), MAX_DOCUMENTS);
}

#[test]
fn complete_code_lines_append_without_relaying_out_the_prefix() {
    let source = "```python\na = 1\nb = 2\n";
    let mut cache = MarkdownCache::default();
    cache.render(source, 20);
    let old = cache.documents.back().unwrap().blocks[0].clone();
    let document = markdown::parse_markdown(&format!("{source}c = 3\n"));
    let appended = append_code(Some(&old), &document.blocks[0], 20).unwrap();
    assert_eq!(
        appended,
        super::super::markdown_lines(&format!("{source}c = 3\n"), 20)
    );
    assert_eq!(&appended[..old.lines.len()], old.lines.as_slice());
}

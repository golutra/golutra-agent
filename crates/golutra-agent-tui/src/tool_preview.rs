//! 命令与文件卡片的有界预览；行号来自持久 diff，不能从当前文件反推。

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

// 行号与符号属于展示侧栏；续行只保留缩进，红绿背景覆盖当前整条显示行。
pub(crate) fn number_width(lines: &[String]) -> usize {
    lines
        .iter()
        .filter_map(|line| numbered_parts(line).map(|(_, number, _)| number.len()))
        .max()
        .unwrap_or(1)
}

fn numbered_parts(value: &str) -> Option<(char, &str, &str)> {
    let sign = value.chars().next()?;
    if !matches!(sign, '+' | '-' | ' ') {
        return None;
    }
    let rest = value.get(1..)?.trim_start();
    let (number, content) = rest.split_once(' ')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((sign, number, content))
}

pub(crate) fn render_diff_line(
    value: &str,
    width: u16,
    number_width: usize,
    palette: super::TuiPalette,
) -> Option<Vec<Line<'static>>> {
    let (sign, number, content) = numbered_parts(value)?;
    let background = match sign {
        '+' => Color::Rgb(33, 41, 34),
        '-' => Color::Rgb(60, 23, 15),
        _ => Color::Reset,
    };
    let foreground = match sign {
        '+' => palette.success,
        '-' => palette.error,
        _ => palette.text,
    };
    let style = Style::default().fg(foreground).bg(background);
    let gutter = format!("{number:>number_width$} {sign} ");
    let gutter_width = gutter.len().min(usize::from(width.saturating_sub(1)));
    let available = usize::from(width).saturating_sub(gutter_width).max(1);
    let mut wrapped =
        super::rich_text::wrap_detail_spans(&[Span::styled(content.to_owned(), style)], available);
    if wrapped.is_empty() {
        wrapped.push(Line::default());
    }
    Some(
        wrapped
            .into_iter()
            .enumerate()
            .map(|(index, mut line)| {
                let prefix = if index == 0 {
                    gutter[..gutter_width].to_owned()
                } else {
                    " ".repeat(gutter_width)
                };
                line.spans.insert(
                    0,
                    Span::styled(prefix, Style::default().fg(palette.muted).bg(background)),
                );
                line.style = style;
                let padding = usize::from(width).saturating_sub(line.width());
                line.spans.push(Span::styled(" ".repeat(padding), style));
                line
            })
            .collect(),
    )
}

// 只截取 Diff 分区；后续参数不是修改内容，也不能触发“更多修改”提示。
pub(crate) fn file_preview(details: &[String], expanded: bool) -> Vec<String> {
    let Some(start) = details.iter().position(|line| line == "Diff") else {
        return Vec::new();
    };
    let diff = details[start + 1..]
        .iter()
        .take_while(|line| {
            !matches!(
                line.as_str(),
                "Arguments" | "Output" | "Facts" | "Child details"
            )
        })
        .collect::<Vec<_>>();
    let limit = if expanded { diff.len() } else { 14 };
    let mut result = details[..start]
        .iter()
        .filter(|line| {
            line.starts_with("Loading saved content")
                || line.starts_with("Saved output unavailable")
                || line.starts_with("Live preview")
        })
        .cloned()
        .collect::<Vec<_>>();
    result.extend(diff.iter().take(limit).map(|line| (*line).clone()));
    if diff.len() > limit {
        result.push("… more changes · Ctrl+O to view".to_owned());
    }
    result
}

// 保留原始 artifact 的标准补丁；这里只转换展示行，hunk 游标用于真实行号。
pub(crate) fn numbered_diff(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut old = None;
    let mut new = None;
    let mut rows = Vec::new();
    let mut file_count = 0;
    let mut old_path = None;
    let mut hunk_seen = false;
    for line in lines {
        if line.starts_with("@@ ") {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            old = fields.get(1).and_then(|s| hunk_range(s, '-'));
            new = fields.get(2).and_then(|s| hunk_range(s, '+'));
            if hunk_seen {
                rows.push((false, "…".to_owned()));
            }
            hunk_seen = true;
            continue;
        }
        if old.is_none() && new.is_none() {
            if let Some(path) = line.strip_prefix("--- ") {
                old_path = Some(path.to_owned());
                continue;
            }
            if let Some(path) = line.strip_prefix("+++ ") {
                let path = if path == "/dev/null" {
                    old_path.as_deref().unwrap_or(path)
                } else {
                    path
                };
                let path = path
                    .strip_prefix("a/")
                    .or_else(|| path.strip_prefix("b/"))
                    .unwrap_or(path);
                rows.push((true, path.to_owned()));
                file_count += 1;
                hunk_seen = false;
                continue;
            }
            if line.starts_with("diff --git ") || line.starts_with("index ") {
                continue;
            }
        }
        let numbered = match line.as_bytes().first() {
            Some(b'-') if old.is_some() => Some((advance(&mut old), '-')),
            Some(b'+') if new.is_some() => Some((advance(&mut new), '+')),
            Some(b' ') if new.is_some() && old.is_some() => {
                advance(&mut old);
                Some((advance(&mut new), ' '))
            }
            _ => None,
        };
        rows.push((
            false,
            if let Some((number, prefix)) = numbered {
                format!("{prefix} {number:>5} {}", &line[1..])
            } else {
                line
            },
        ));
    }
    rows.into_iter()
        .filter(|(header, _)| !header || file_count > 1)
        .map(|(_, line)| line)
        .collect()
}

fn hunk_range(value: &str, prefix: char) -> Option<std::ops::Range<u64>> {
    let mut fields = value.strip_prefix(prefix)?.split(',');
    let start: u64 = fields.next()?.parse().ok()?;
    let count: u64 = fields.next().unwrap_or("1").parse().ok()?;
    (count > 0).then(|| start..start.saturating_add(count))
}

fn advance(value: &mut Option<std::ops::Range<u64>>) -> u64 {
    let mut range = value.take().expect("validated hunk range");
    let number = range.start;
    range.start = range.start.saturating_add(1);
    *value = (!range.is_empty()).then_some(range);
    number
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_numbers_follow_each_hunk_and_preserve_indentation() {
        let lines = numbered_diff(
            [
                "@@ -40,2 +40,3 @@",
                " keep",
                "-    old",
                "+    新",
                "+    extra",
                "@@ -90 +91 @@",
                "-end",
                "+done",
            ]
            .map(str::to_owned),
        );
        assert_eq!(
            lines,
            [
                "     40 keep",
                "-    41     old",
                "+    41     新",
                "+    42     extra",
                "…",
                "-    90 end",
                "+    91 done"
            ]
        );
    }

    #[test]
    fn file_headers_and_lines_starting_with_signs_are_not_confused() {
        let lines = numbered_diff(
            [
                "--- a/a",
                "+++ b/a",
                "@@ -1 +1 @@",
                "--- old",
                "+++ new",
                "--- a/b",
                "+++ b/b",
                "@@ -0,0 +1 @@",
                "+created",
                "@@ -9 +0,0 @@",
                "-deleted",
            ]
            .map(str::to_owned),
        );
        assert_eq!(
            lines,
            [
                "a",
                "-     1 -- old",
                "+     1 ++ new",
                "b",
                "+     1 created",
                "…",
                "-     9 deleted"
            ]
        );
    }

    #[test]
    fn single_file_headers_are_hidden_without_mutating_patch() {
        let patch = "--- a/test.md\n+++ b/test.md\n@@ -6 +6 @@\n-注释\n+comment";
        let display = numbered_diff(patch.lines().map(str::to_owned));
        assert_eq!(display, ["-     6 注释", "+     6 comment"]);
        assert!(patch.contains("@@ -6 +6 @@"));
    }

    #[test]
    fn arguments_do_not_leak_or_count_as_more_changes() {
        let mut details = vec![
            "Diff".to_owned(),
            "-     6 old".to_owned(),
            "+     6 new".to_owned(),
            "Arguments".to_owned(),
        ];
        details.extend(std::iter::repeat_n("secret parameter".to_owned(), 30));
        for expanded in [false, true] {
            assert_eq!(
                file_preview(&details, expanded),
                ["-     6 old", "+     6 new"]
            );
        }
    }

    #[test]
    fn numbered_diff_wraps_with_blank_gutter_and_preserves_wide_text() {
        let palette = super::super::TuiPreferences::default().palette();
        let rows = render_diff_line("+    99 中文🙂hello世界", 12, 3, palette).unwrap();
        assert!(rows.len() > 1);
        assert!(rows[0].to_string().starts_with(" 99 + "));
        assert!(
            rows[1..]
                .iter()
                .all(|line| line.to_string().starts_with("      "))
        );
        assert_eq!(
            rows.iter()
                .map(|line| line.to_string()[6..].trim_end().to_owned())
                .collect::<String>(),
            "中文🙂hello世界"
        );
        assert!(
            rows.iter()
                .all(|line| line.style.bg == Some(Color::Rgb(33, 41, 34)))
        );
    }
}

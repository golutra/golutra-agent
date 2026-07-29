//! Conservative compatibility hints derived from legacy natural-language objectives.
//!
//! Explicit `TaskContract` values remain authoritative. These hints only keep
//! older command surfaces and deterministic mock runs aligned when a caller
//! has not supplied structured delivery fields.

use crate::task_contract::is_valid_workspace_relative_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWriteObjectiveHint {
    pub path: String,
    pub content: Option<String>,
}

#[must_use]
pub fn infer_legacy_write_objective(objective: &str) -> Option<LegacyWriteObjectiveHint> {
    let path = infer_legacy_write_path(objective)?;
    let content = infer_legacy_write_content(objective);
    Some(LegacyWriteObjectiveHint { path, content })
}

#[must_use]
pub fn infer_legacy_write_path(objective: &str) -> Option<String> {
    infer_write_path(objective)
}

#[must_use]
pub fn infer_legacy_write_content(objective: &str) -> Option<String> {
    let mut content =
        infer_marker_content(objective).or_else(|| infer_quoted_write_content(objective));
    if content.is_some()
        && requests_trailing_newline(objective)
        && let Some(value) = content.as_mut()
        && !value.ends_with('\n')
    {
        value.push('\n');
    }
    content
}

fn infer_write_path(objective: &str) -> Option<String> {
    let tokens = objective.split_whitespace().collect::<Vec<_>>();
    let words = tokens
        .iter()
        .map(|token| normalized_word(token))
        .collect::<Vec<_>>();

    for (index, word) in words.iter().enumerate() {
        if word != "file"
            || !words[index.saturating_sub(4)..index]
                .iter()
                .any(|word| is_write_verb(word))
        {
            continue;
        }
        let candidate = skip_path_connectors(&words, index + 1);
        if let Some(path) = tokens
            .get(candidate)
            .and_then(|token| normalize_path(token))
        {
            return Some(path);
        }
    }

    for (index, word) in words.iter().enumerate() {
        if !is_write_verb(word) {
            continue;
        }
        let candidate = skip_path_connectors(&words, index + 1);
        if let Some(path) = tokens
            .get(candidate)
            .and_then(|token| normalize_path(token))
        {
            return Some(path);
        }
    }
    None
}

fn skip_path_connectors(words: &[String], mut index: usize) -> usize {
    while words.get(index).is_some_and(|word| {
        matches!(
            word.as_str(),
            "a" | "an" | "the" | "file" | "called" | "named" | "as" | "at" | "to"
        )
    }) {
        index = index.saturating_add(1);
    }
    index
}

fn normalize_path(raw: &str) -> Option<String> {
    let mut token = raw.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\'' | '"' | ',' | ':' | ';' | '(' | ')' | '[' | ']'
        )
    });
    token = token.trim_end_matches(['!', '?']);
    if let Some(without_period) = token.strip_suffix('.')
        && is_path_like(without_period)
    {
        token = without_period;
    }
    let aliased = token
        .strip_prefix("/app/")
        .or_else(|| token.strip_prefix("/workspace/"))
        .unwrap_or(token);
    (!aliased.is_empty()
        && aliased.len() <= 512
        && !aliased.contains("://")
        && !aliased.contains(['<', '>', '*'])
        && is_path_like(aliased)
        && is_valid_workspace_relative_path(aliased))
    .then(|| aliased.to_owned())
}

fn is_path_like(value: &str) -> bool {
    value.contains('/')
        || value.contains('\\')
        || value.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
}

fn infer_marker_content(objective: &str) -> Option<String> {
    let lower = objective.to_ascii_lowercase();
    let english = [" with content ", " content is "]
        .iter()
        .find_map(|marker| {
            lower.find(marker).and_then(|start| {
                normalize_content(&objective[start.saturating_add(marker.len())..])
            })
        });
    english.or_else(|| {
        ["内容为", "内容是", "内容：", "内容:"]
            .iter()
            .find_map(|marker| {
                objective.find(marker).and_then(|start| {
                    normalize_content(&objective[start.saturating_add(marker.len())..])
                })
            })
    })
}

fn infer_quoted_write_content(objective: &str) -> Option<String> {
    let lower = objective.to_ascii_lowercase();
    for (start, _) in lower.match_indices("write") {
        let before_is_boundary = start == 0
            || !lower[..start]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_alphanumeric());
        let after_index = start.saturating_add("write".len());
        let after_is_boundary = lower
            .get(after_index..)
            .and_then(|value| value.chars().next())
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        if !before_is_boundary || !after_is_boundary {
            continue;
        }
        let remainder = objective.get(after_index..)?.trim_start();
        let Some(quote) = remainder
            .chars()
            .next()
            .filter(|character| matches!(character, '`' | '\'' | '"'))
        else {
            continue;
        };
        let quoted = &remainder[quote.len_utf8()..];
        let Some(end) = quoted.find(quote) else {
            continue;
        };
        if let Some(content) = normalize_content(&quoted[..end]) {
            return Some(content);
        }
    }
    None
}

fn normalize_content(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|character: char| {
            matches!(
                character,
                '`' | '\'' | '"' | ',' | '.' | ':' | ';' | '，' | '。' | '：'
            )
        })
        .to_owned();
    (!value.is_empty()).then_some(value)
}

fn requests_trailing_newline(objective: &str) -> bool {
    let lower = objective.to_ascii_lowercase();
    [
        "ends in a newline",
        "ends with a newline",
        "end in a newline",
        "end with a newline",
        "newline-terminated",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn normalized_word(token: &str) -> String {
    token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
}

fn is_write_verb(word: &str) -> bool {
    matches!(
        word,
        "create" | "created" | "output" | "save" | "saved" | "write" | "written"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_compact_write_file_objective() {
        assert_eq!(
            infer_legacy_write_objective("write file retained.txt with content retained"),
            Some(LegacyWriteObjectiveHint {
                path: "retained.txt".to_owned(),
                content: Some("retained".to_owned()),
            })
        );
    }

    #[test]
    fn infers_named_file_quoted_content_and_trailing_newline() {
        let objective = "Create a file called hello.txt in the current directory. Write \"Hello, world!\" to it. Make sure it ends in a newline.";

        assert_eq!(
            infer_legacy_write_objective(objective),
            Some(LegacyWriteObjectiveHint {
                path: "hello.txt".to_owned(),
                content: Some("Hello, world!\n".to_owned()),
            })
        );
    }

    #[test]
    fn broad_change_request_does_not_invent_a_delivery() {
        assert!(infer_legacy_write_objective("create a runtime module").is_none());
    }

    #[test]
    fn unsafe_delivery_paths_are_not_inferred_from_objectives() {
        assert_eq!(
            infer_legacy_write_path("create a file named `/app/output/maze.txt`"),
            Some("output/maze.txt".to_owned())
        );
        for objective in [
            "save to `/var/log/nginx/benchmark-access.log`",
            "write file ../outside.txt with content unsafe",
            r"write file C:\workspace\result.txt with content unsafe",
            r"write file \\server\share\result.txt with content unsafe",
        ] {
            assert_eq!(
                infer_legacy_write_path(objective),
                None,
                "inferred an unsafe delivery path from {objective:?}"
            );
        }
    }

    #[test]
    fn content_hint_remains_available_when_path_comes_from_a_structured_payload() {
        assert_eq!(
            infer_legacy_write_content("写入指定文件，内容为你好"),
            Some("你好".to_owned())
        );
    }
}

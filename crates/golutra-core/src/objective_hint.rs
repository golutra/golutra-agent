//! Conservative compatibility hints derived from legacy natural-language objectives.
//!
//! Explicit [`crate::TaskContract`] values remain authoritative. These helpers
//! support older command surfaces and deterministic mock runs; their output is
//! not a runtime delivery contract.

use crate::task_contract::is_valid_workspace_relative_path;

const MAX_OBJECTIVE_HINTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWriteObjectiveHint {
    pub path: String,
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitConversionObjectiveHint {
    pub source_path: String,
    pub output_path: String,
}

#[must_use]
pub fn infer_legacy_write_objective(objective: &str) -> Option<LegacyWriteObjectiveHint> {
    if !starts_with_direct_write_verb(objective) {
        return None;
    }
    let mut paths = infer_legacy_write_paths(objective);
    if paths.len() != 1 {
        return None;
    }
    let content = infer_legacy_write_content(objective)?;
    let path = paths.pop()?;
    Some(LegacyWriteObjectiveHint {
        path,
        content: Some(content),
    })
}

#[must_use]
pub fn infer_legacy_write_path(objective: &str) -> Option<String> {
    infer_legacy_write_paths(objective).into_iter().next()
}

/// Infer one direct `write <path>` target without interpreting broader prose
/// such as named report lists or output descriptions.
#[must_use]
pub fn infer_direct_legacy_write_path(objective: &str) -> Option<String> {
    let mut candidates = Vec::new();
    let mut has_other_delivery_verb = false;
    for clause in objective.split(['\n', ';', '；']) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
        let words = tokens
            .iter()
            .map(|token| normalized_word(token))
            .collect::<Vec<_>>();
        for (index, word) in words.iter().enumerate() {
            if is_write_verb(word) && word == "write" {
                let candidate = skip_path_connectors(&words, index.saturating_add(1));
                if let Some(path) = tokens
                    .get(candidate)
                    .and_then(|token| normalize_path(token))
                {
                    push_unique(&mut candidates, path);
                }
            } else if matches!(
                word.as_str(),
                "create" | "generate" | "output" | "produce" | "save"
            ) {
                has_other_delivery_verb = true;
            }
        }
    }
    (!has_other_delivery_verb && candidates.len() == 1)
        .then(|| candidates.pop().expect("one direct write candidate"))
}

#[must_use]
pub fn infer_legacy_write_paths(objective: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for clause in objective.split(['\n', ';', '；']) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
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
            let candidate = skip_path_connectors(&words, index.saturating_add(1));
            if let Some(path) = tokens
                .get(candidate)
                .and_then(|token| normalize_path(token))
            {
                push_unique(&mut paths, path);
            }
        }

        for (index, word) in words.iter().enumerate() {
            if !is_write_verb(word) {
                continue;
            }
            let candidate = skip_path_connectors(&words, index.saturating_add(1));
            if let Some(path) = tokens
                .get(candidate)
                .and_then(|token| normalize_path(token))
            {
                push_unique(&mut paths, path);
            }
        }
    }
    paths
}

#[must_use]
pub fn infer_explicit_conversion_objectives(
    objective: &str,
) -> Vec<ExplicitConversionObjectiveHint> {
    let mut conversions = Vec::new();
    for clause in objective.split(['\n', ';', '；']) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
        let words = tokens
            .iter()
            .map(|token| normalized_word(token))
            .collect::<Vec<_>>();
        for (verb_index, word) in words.iter().enumerate() {
            if !matches!(
                word.as_str(),
                "convert" | "converted" | "transcode" | "transcoded" | "transform" | "transformed"
            ) {
                continue;
            }
            let Some(connector_index) = words[verb_index.saturating_add(1)..]
                .iter()
                .position(|word| matches!(word.as_str(), "as" | "into" | "to"))
                .map(|index| index.saturating_add(verb_index).saturating_add(1))
            else {
                continue;
            };
            let source_path = tokens[verb_index.saturating_add(1)..connector_index]
                .iter()
                .find_map(|token| normalize_path(token));
            let output_path = tokens[connector_index.saturating_add(1)..]
                .iter()
                .find_map(|token| normalize_path(token));
            let (Some(source_path), Some(output_path)) = (source_path, output_path) else {
                continue;
            };
            if source_path == output_path
                || conversions
                    .iter()
                    .any(|conversion: &ExplicitConversionObjectiveHint| {
                        conversion.source_path == source_path
                            && conversion.output_path == output_path
                    })
            {
                continue;
            }
            conversions.push(ExplicitConversionObjectiveHint {
                source_path,
                output_path,
            });
            if conversions.len() == MAX_OBJECTIVE_HINTS {
                return conversions;
            }
        }
    }
    conversions
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
    let mut token = raw.trim();
    if let Some(quote) = token
        .chars()
        .next()
        .filter(|value| matches!(value, '`' | '\'' | '"'))
        && let Some(end) = token[quote.len_utf8()..].find(quote)
    {
        token = &token[quote.len_utf8()..quote.len_utf8().saturating_add(end)];
    }
    token = token.trim_matches(|character: char| {
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
    let token = token
        .strip_prefix("/app/")
        .or_else(|| token.strip_prefix("/workspace/"))
        .unwrap_or(token);
    (!token.is_empty()
        && token.len() <= 512
        && !token.contains("://")
        && !token.contains(['<', '>', '*'])
        && !token
            .chars()
            .any(|character| matches!(character, '=' | '$' | '{' | '}' | '(' | ')' | ',' | ';'))
        && is_path_like(token)
        && is_valid_workspace_relative_path(token))
    .then(|| token.to_owned())
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

fn push_unique(values: &mut Vec<String>, value: String) {
    if values.len() < MAX_OBJECTIVE_HINTS && !values.contains(&value) {
        values.push(value);
    }
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

fn starts_with_direct_write_verb(objective: &str) -> bool {
    let mut words = objective
        .split_whitespace()
        .map(normalized_word)
        .filter(|word| !word.is_empty());
    let first = words.next();
    let verb = if first.as_deref() == Some("please") {
        words.next()
    } else {
        first
    };
    verb.is_some_and(|word| is_write_verb(&word))
}

fn is_write_verb(word: &str) -> bool {
    matches!(
        word,
        "create"
            | "created"
            | "generate"
            | "generated"
            | "output"
            | "produce"
            | "produced"
            | "save"
            | "saved"
            | "write"
            | "written"
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
        let objective = "Create a file called hello.txt. Write \"Hello, world!\" to it. Make sure it ends in a newline.";

        assert_eq!(
            infer_legacy_write_objective(objective),
            Some(LegacyWriteObjectiveHint {
                path: "hello.txt".to_owned(),
                content: Some("Hello, world!\n".to_owned()),
            })
        );
    }

    #[test]
    fn collects_only_explicit_write_statements() {
        assert_eq!(
            infer_legacy_write_paths(
                "Read input.txt and write result.txt; create file summary.json"
            ),
            vec!["result.txt", "summary.json"]
        );
    }

    #[test]
    fn infers_only_explicit_conversion_pairs() {
        assert_eq!(
            infer_explicit_conversion_objectives("Convert input.csv into output.parquet"),
            vec![ExplicitConversionObjectiveHint {
                source_path: "input.csv".to_owned(),
                output_path: "output.parquet".to_owned(),
            }]
        );
        assert!(infer_explicit_conversion_objectives("Transform the dataset").is_empty());
    }

    #[test]
    fn broad_change_request_does_not_invent_a_delivery() {
        assert!(infer_legacy_write_objective("create a runtime module").is_none());
    }

    #[test]
    fn deterministic_write_hint_rejects_mentions_and_multi_output_requests() {
        for objective in [
            "Explain why `write file report.txt with content ok` is unsafe",
            "Do not write file report.txt with content ok",
            "Write first.txt with content alpha; create file second.txt with content beta",
            "Create a file named report.txt",
        ] {
            assert_eq!(infer_legacy_write_objective(objective), None, "{objective}");
        }
    }

    #[test]
    fn direct_write_path_can_follow_a_supporting_read_without_parsing_output_prose() {
        assert_eq!(
            infer_direct_legacy_write_path(
                "read input.txt and write results.txt; diagnostic: /tmp/verify.py"
            ),
            Some("results.txt".to_owned())
        );
        assert_eq!(
            infer_direct_legacy_write_path(
                "Write first.txt with content alpha; create file second.txt with content beta"
            ),
            None
        );
    }

    #[test]
    fn workspace_aliases_are_normalized_but_unsafe_paths_are_rejected() {
        assert_eq!(
            infer_legacy_write_path("create a file named `/app/output/result.txt`"),
            Some("output/result.txt".to_owned())
        );
        for objective in [
            "save to `/var/log/application.log`",
            "write file ../outside.txt with content unsafe",
            r"write file C:\workspace\result.txt with content unsafe",
            r"write file \\server\share\result.txt with content unsafe",
        ] {
            assert_eq!(infer_legacy_write_path(objective), None, "{objective}");
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

//! Conservative facts derived from natural-language objectives.
//!
//! Explicit `TaskContract` values remain authoritative. Legacy write hints only
//! fill missing delivery fields at older command boundaries; explicit conversion
//! pairs can also constrain runtime validation without changing the contract.

use crate::task_contract::is_valid_workspace_relative_path;

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
    let path = infer_legacy_write_path(objective)?;
    let content = infer_legacy_write_content(objective);
    Some(LegacyWriteObjectiveHint { path, content })
}

#[must_use]
pub fn infer_legacy_write_path(objective: &str) -> Option<String> {
    infer_legacy_write_paths(objective).into_iter().next()
}

#[must_use]
pub fn infer_legacy_write_paths(objective: &str) -> Vec<String> {
    const MAX_INFERRED_PATHS: usize = 64;
    const DELIVERY_NOUN_LOOKBACK: usize = 16;

    let mut paths = Vec::new();
    for clause in objective_clauses(objective) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
        let words = tokens
            .iter()
            .map(|token| normalized_word(token))
            .collect::<Vec<_>>();

        infer_strong_delivery_paths(clause, &tokens, &words, &mut paths, MAX_INFERRED_PATHS);
        for (index, word) in words.iter().enumerate() {
            if is_delivery_noun(word)
                && words[index.saturating_sub(DELIVERY_NOUN_LOOKBACK)..index]
                    .iter()
                    .any(|candidate| is_write_verb(candidate))
                && nearest_intent_is_delivery(&words, index)
            {
                let candidate = skip_delivery_connectors(&words, index.saturating_add(1));
                if let Some(path) = tokens
                    .get(candidate)
                    .and_then(|token| normalize_path(token))
                {
                    push_inferred_path(&mut paths, path, MAX_INFERRED_PATHS);
                }
            }

            if !is_write_verb(word) {
                continue;
            }
            if let Some(path) = delivery_path_after_verb(&tokens, &words, index) {
                push_inferred_path(&mut paths, path, MAX_INFERRED_PATHS);
            }
        }

        infer_cjk_delivery_paths(clause, &tokens, &mut paths, MAX_INFERRED_PATHS);
        infer_imperative_named_deliveries(&tokens, &words, &mut paths, MAX_INFERRED_PATHS);
    }
    for conversion in infer_explicit_conversion_objectives(objective) {
        push_inferred_path(&mut paths, conversion.output_path, MAX_INFERRED_PATHS);
    }
    infer_declared_delivery_lists(objective, &mut paths, MAX_INFERRED_PATHS);
    paths
}

#[must_use]
pub fn infer_explicit_conversion_objectives(
    objective: &str,
) -> Vec<ExplicitConversionObjectiveHint> {
    const MAX_INFERRED_CONVERSIONS: usize = 16;

    let mut conversions = Vec::new();
    for clause in objective_clauses(objective) {
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
                .position(|word| matches!(word.as_str(), "into" | "to"))
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
            if conversions.len() == MAX_INFERRED_CONVERSIONS {
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
    let had_path_separator = token.contains(['/', '\\']);
    let aliased = token
        .strip_prefix("/app/")
        .or_else(|| token.strip_prefix("/workspace/"))
        .unwrap_or(token);
    (!aliased.is_empty()
        && aliased.len() <= 512
        && !aliased.ends_with(['/', '\\'])
        && !matches!(aliased.to_ascii_lowercase().as_str(), "e.g" | "i.e")
        && !aliased.contains("://")
        && !aliased.contains(['<', '>', '*'])
        && !aliased
            .chars()
            .any(|character| matches!(character, '=' | '$' | '{' | '}' | '(' | ')' | ',' | ';'))
        && (had_path_separator || is_path_like(aliased))
        && is_valid_workspace_relative_path(aliased))
    .then(|| aliased.to_owned())
}

fn normalize_directory_path(raw: &str) -> Option<String> {
    let token = raw.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\'' | '"' | ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '!' | '?'
        )
    });
    let trimmed = token.trim_end_matches(['/', '\\']);
    if let Some(path) = normalize_path(trimmed) {
        return Some(path);
    }
    let token = trimmed.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\'' | '"' | ',' | ':' | ';' | '(' | ')' | '[' | ']'
        )
    });
    (!token.is_empty()
        && token.len() <= 128
        && token != "."
        && token != ".."
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && is_valid_workspace_relative_path(token))
    .then(|| token.to_owned())
}

fn normalize_bare_delivery_name(raw: &str) -> Option<String> {
    let token = raw.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\'' | '"' | ',' | ':' | ';' | '(' | ')' | '[' | ']' | '.'
        )
    });
    (!token.is_empty()
        && token.len() <= 128
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && token
            .chars()
            .any(|character| character == '_' || character == '-')
        && is_valid_workspace_relative_path(token))
    .then(|| token.to_owned())
}

fn push_inferred_path(paths: &mut Vec<String>, path: String, limit: usize) {
    if paths.len() < limit && !paths.contains(&path) {
        paths.push(path);
    }
}

fn skip_delivery_connectors(words: &[String], mut index: usize) -> usize {
    while words.get(index).is_some_and(|word| {
        matches!(
            word.as_str(),
            "a" | "an"
                | "as"
                | "at"
                | "be"
                | "called"
                | "current"
                | "directory"
                | "file"
                | "final"
                | "in"
                | "it"
                | "me"
                | "named"
                | "new"
                | "output"
                | "path"
                | "root"
                | "should"
                | "the"
                | "them"
                | "to"
                | "under"
        )
    }) {
        index = index.saturating_add(1);
    }
    index
}

fn delivery_path_after_verb(
    tokens: &[&str],
    words: &[String],
    verb_index: usize,
) -> Option<String> {
    let immediate = skip_delivery_connectors(words, verb_index.saturating_add(1));
    if let Some(path) = tokens
        .get(immediate)
        .and_then(|token| normalize_path(token))
        && nearest_intent_is_delivery(words, immediate)
    {
        return Some(path);
    }

    let end = words.len().min(verb_index.saturating_add(9));
    for index in verb_index.saturating_add(1)..end {
        if !matches!(words[index].as_str(), "as" | "at" | "to" | "under") {
            continue;
        }
        let candidate = skip_delivery_connectors(words, index.saturating_add(1));
        if let Some(path) = tokens
            .get(candidate)
            .and_then(|token| normalize_path(token))
            && nearest_intent_is_delivery(words, candidate)
        {
            return Some(path);
        }
    }
    None
}

fn nearest_intent_is_delivery(words: &[String], path_index: usize) -> bool {
    let latest_delivery = words[..path_index]
        .iter()
        .rposition(|word| is_write_verb(word));
    let latest_input = words[..path_index]
        .iter()
        .rposition(|word| is_input_marker(word));
    latest_delivery.is_some_and(|delivery| latest_input.is_none_or(|input| delivery > input))
}

fn infer_strong_delivery_paths(
    clause: &str,
    tokens: &[&str],
    words: &[String],
    paths: &mut Vec<String>,
    limit: usize,
) {
    let lower = clause.to_ascii_lowercase();
    let strong_final_context = declares_final_output(words)
        || lower.contains("final trained")
        || lower.contains("final artifact")
        || lower.contains("final directory")
        || lower.contains("output directory");
    let has_write_context = strong_final_context || words.iter().any(|word| is_write_verb(word));
    if !has_write_context {
        return;
    }

    for (index, token) in tokens.iter().enumerate() {
        if matches!(words[index].as_str(), "called" | "named") {
            let executable_name = words[index.saturating_sub(6)..index]
                .iter()
                .any(|word| matches!(word.as_str(), "binary" | "executable"));
            let declared_delivery = words[..index].iter().any(|word| is_delivery_noun(word))
                && words[..index].iter().any(|word| is_write_verb(word));
            let candidate = skip_delivery_connectors(words, index.saturating_add(1));
            if let Some(path) = tokens
                .get(candidate)
                .and_then(|token| {
                    normalize_path(token).or_else(|| {
                        executable_name
                            .then(|| normalize_bare_delivery_name(token))
                            .flatten()
                    })
                })
                .filter(|_| executable_name || declared_delivery)
            {
                push_inferred_path(paths, path, limit);
            }
        }
        if strong_final_context
            && let Some(path) = normalize_path(token).or_else(|| {
                let directory = token.trim_end_matches(['.', ',', ';', ':']);
                directory
                    .ends_with(['/', '\\'])
                    .then(|| normalize_directory_path(directory))
                    .flatten()
            })
        {
            push_inferred_path(paths, path, limit);
        }
    }
}

fn declares_final_output(words: &[String]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        if word != "final"
            || words.get(index.saturating_add(1)).map(String::as_str) != Some("output")
        {
            return false;
        }
        let declaration = &words[index.saturating_add(2)..words.len().min(index.saturating_add(8))];
        declaration
            .iter()
            .position(|word| matches!(word.as_str(), "must" | "should"))
            .is_some_and(|modal| {
                declaration[modal.saturating_add(1)..]
                    .iter()
                    .any(|word| word == "be")
            })
    })
}

fn infer_imperative_named_deliveries(
    tokens: &[&str],
    words: &[String],
    paths: &mut Vec<String>,
    limit: usize,
) {
    for (index, word) in words.iter().enumerate() {
        if word != "call"
            || (index > 0
                && !matches!(
                    words[index.saturating_sub(1)].as_str(),
                    "also" | "and" | "finally" | "must" | "please" | "should" | "then" | "to"
                ))
        {
            continue;
        }
        let mut kind_index = index.saturating_add(1);
        while words
            .get(kind_index)
            .is_some_and(|word| matches!(word.as_str(), "a" | "an" | "the" | "this"))
        {
            kind_index = kind_index.saturating_add(1);
        }
        let Some(kind) = words.get(kind_index).map(String::as_str) else {
            continue;
        };
        if !matches!(kind, "directory" | "file" | "folder" | "script") {
            continue;
        }
        let candidate = skip_delivery_connectors(words, kind_index.saturating_add(1));
        let path = tokens.get(candidate).and_then(|token| {
            if matches!(kind, "directory" | "folder") {
                normalize_directory_path(token)
            } else {
                normalize_path(token)
            }
        });
        if let Some(path) = path {
            push_inferred_path(paths, path, limit);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredDeliveryKind {
    File,
    Program,
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingDeliveryList {
    kind: DeclaredDeliveryKind,
    remaining: usize,
}

fn infer_declared_delivery_lists(objective: &str, paths: &mut Vec<String>, limit: usize) {
    let mut pending = Vec::<PendingDeliveryList>::new();
    for clause in objective_clauses(objective) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
        let words = tokens
            .iter()
            .map(|token| normalized_word(token))
            .collect::<Vec<_>>();

        for kind in [
            DeclaredDeliveryKind::File,
            DeclaredDeliveryKind::Program,
            DeclaredDeliveryKind::Script,
        ] {
            if let Some(remaining) = declared_delivery_count(&words, kind)
                && words.iter().any(|word| is_write_verb(word))
            {
                pending.push(PendingDeliveryList { kind, remaining });
            }
        }

        for declaration in &mut pending {
            if declaration.remaining == 0 || !clause_matches_declared_item(&words, declaration.kind)
            {
                continue;
            }
            let candidate = tokens.iter().find_map(|token| normalize_path(token));
            if let Some(path) = candidate {
                push_inferred_path(paths, path, limit);
                declaration.remaining = declaration.remaining.saturating_sub(1);
            }
        }
        pending.retain(|declaration| declaration.remaining > 0);
    }
}

fn declared_delivery_count(words: &[String], kind: DeclaredDeliveryKind) -> Option<usize> {
    let noun_index = words.iter().position(|word| match kind {
        DeclaredDeliveryKind::File => word == "files",
        DeclaredDeliveryKind::Program => word == "programs",
        DeclaredDeliveryKind::Script => word == "scripts",
    })?;
    words[..noun_index]
        .iter()
        .rev()
        .find_map(|word| match word.as_str() {
            "two" => Some(2),
            "three" => Some(3),
            "four" => Some(4),
            value => value
                .parse::<usize>()
                .ok()
                .filter(|count| (2..=16).contains(count)),
        })
}

fn clause_matches_declared_item(words: &[String], kind: DeclaredDeliveryKind) -> bool {
    let ordinal = words.iter().any(|word| {
        matches!(
            word.as_str(),
            "1" | "2" | "3" | "4" | "first" | "second" | "third" | "fourth"
        )
    });
    if !ordinal {
        return false;
    }
    match kind {
        DeclaredDeliveryKind::File => true,
        DeclaredDeliveryKind::Program => words.iter().any(|word| word == "program"),
        DeclaredDeliveryKind::Script => words.iter().any(|word| word == "script"),
    }
}

fn objective_clauses(objective: &str) -> Vec<&str> {
    let mut clauses = Vec::new();
    let mut start = 0_usize;
    for (index, character) in objective.char_indices() {
        let end = index.saturating_add(character.len_utf8());
        let boundary = matches!(character, '\n' | ';' | '；')
            || (matches!(character, '.' | '!' | '?')
                && objective[end..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
                && objective[end..]
                    .chars()
                    .find(|next| !next.is_whitespace())
                    .is_none_or(|next| next.is_ascii_uppercase() || next.is_ascii_digit()));
        if boundary {
            let clause = objective[start..end].trim();
            if !clause.is_empty() {
                clauses.push(clause);
            }
            start = end;
        }
    }
    let tail = objective[start..].trim();
    if !tail.is_empty() {
        clauses.push(tail);
    }
    clauses
}

fn infer_cjk_delivery_paths(clause: &str, tokens: &[&str], paths: &mut Vec<String>, limit: usize) {
    const DELIVERY_MARKERS: &[&str] = &["写入", "保存", "输出", "生成", "交付", "文件名"];
    const INPUT_MARKERS: &[&str] = &["读取", "输入", "源文件", "已有", "使用"];

    for token in tokens {
        let Some(path) = normalize_path(token) else {
            continue;
        };
        let Some(path_offset) = clause.find(token) else {
            continue;
        };
        let before = &clause[..path_offset];
        let delivery = DELIVERY_MARKERS
            .iter()
            .filter_map(|marker| before.rfind(marker))
            .max();
        let input = INPUT_MARKERS
            .iter()
            .filter_map(|marker| before.rfind(marker))
            .max();
        if delivery.is_some_and(|position| input.is_none_or(|input| position > input)) {
            push_inferred_path(paths, path, limit);
        }
    }
}

fn is_delivery_noun(word: &str) -> bool {
    matches!(
        word,
        "artifact" | "file" | "files" | "program" | "programs" | "report" | "script" | "scripts"
    )
}

fn is_input_marker(word: &str) -> bool {
    matches!(
        word,
        "existing"
            | "given"
            | "input"
            | "original"
            | "provided"
            | "read"
            | "reads"
            | "reading"
            | "source"
            | "use"
            | "uses"
            | "using"
    )
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
        "write"
            | "writes"
            | "written"
            | "save"
            | "saved"
            | "create"
            | "created"
            | "generate"
            | "generated"
            | "produce"
            | "produced"
            | "output"
            | "deliver"
            | "delivered"
            | "implement"
            | "implemented"
            | "place"
            | "store"
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
    fn infers_every_explicit_output_without_treating_markup_as_a_path() {
        let objective = r#"Save the collected data to a CSV file named 'books.csv'.
The report should be saved to a file named 'report.txt'.
<span class="book-price">${price}</span>"#;

        assert_eq!(
            infer_legacy_write_paths(objective),
            vec!["books.csv", "report.txt"]
        );
    }

    #[test]
    fn input_paths_and_diagnostics_are_not_inferred_as_deliveries() {
        assert_eq!(
            infer_legacy_write_paths(
                "read input.txt and write results.txt; diagnostic: /tmp/verify.py"
            ),
            vec!["results.txt"]
        );
        assert!(
            infer_legacy_write_paths(
                "Use the provided environment.yml in /app/project. Modify the environment.yml file and create the fixed environment."
            )
            .is_empty()
        );
        assert_eq!(
            infer_legacy_write_paths(
                "I've put an image at image.ppm. Write a C program image.c. Your output should be a new file reconstructed.ppm."
            ),
            vec!["image.c", "reconstructed.ppm"]
        );
        assert!(
            infer_legacy_write_paths(
                "Compile SQLite in /app/sqlite using the provided build.config."
            )
            .is_empty()
        );
    }

    #[test]
    fn read_only_final_output_reference_is_not_a_delivery_declaration() {
        assert!(
            infer_legacy_write_paths(
                "Finally, use less in the second pane to examine the final output.csv and verify it processed correctly."
            )
            .is_empty()
        );
    }

    #[test]
    fn conversion_inference_retains_only_the_explicit_output() {
        let objective =
            "Convert the file '/app/data.csv' into a Parquet file named '/app/data.parquet'.";

        assert_eq!(infer_legacy_write_paths(objective), vec!["data.parquet"]);
        assert_eq!(
            infer_explicit_conversion_objectives(objective),
            vec![ExplicitConversionObjectiveHint {
                source_path: "data.csv".to_owned(),
                output_path: "data.parquet".to_owned(),
            }]
        );
    }

    #[test]
    fn imperative_names_are_explicit_directory_and_script_deliveries() {
        let objective = "Call this folder \"c4_reshard/\". Please call the script \"revert.py\" and place it in the parent directory.";

        assert_eq!(
            infer_legacy_write_paths(objective),
            vec!["c4_reshard", "revert.py"]
        );
    }

    #[test]
    fn sentence_boundaries_and_nearest_intent_keep_inputs_out_of_deliveries() {
        let objective = "I have a decompressor in /app/decomp.c. It reads compressed data from stdin and writes decompressed data to stdout. I also have a file data.txt with text. Write me data.comp so cat data.comp through the decompressor gives exactly data.txt.";

        assert_eq!(infer_legacy_write_paths(objective), vec!["data.comp"]);
        assert_eq!(
            infer_legacy_write_paths(
                "Write a compressor that reads input.txt and writes result.comp."
            ),
            vec!["result.comp"]
        );
    }

    #[test]
    fn final_output_clause_retains_named_executable_and_every_explicit_file() {
        let objective = "Your final output should be a binary executable called cli_tool that can be run from the command line and the weights.json which it loads and a file called prediction.txt containing the result.";

        assert_eq!(
            infer_legacy_write_paths(objective),
            vec!["cli_tool", "weights.json", "prediction.txt"]
        );
    }

    #[test]
    fn declared_delivery_lists_retain_numbered_files_and_ordinal_scripts() {
        let objective = "Create two shell scripts to handle the workflow.\nThe first script, detector.sh, reads input.log.\nGenerate two JSON files:\n1. alert.json with alerts\n2. report.json with statistics\nThe second script, response.sh, handles an address.";

        assert_eq!(
            infer_legacy_write_paths(objective),
            vec!["detector.sh", "alert.json", "report.json", "response.sh"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Create two scripts: the first is called import_data.sh and the second is named export_data.sh."
            ),
            vec!["import_data.sh", "export_data.sh"]
        );
    }

    #[test]
    fn explicit_final_output_directories_are_delivery_paths() {
        assert_eq!(
            infer_legacy_write_paths(
                "Your final trained model must be located in the /app/trained_model directory."
            ),
            vec!["trained_model"]
        );
        assert_eq!(
            infer_legacy_write_paths("Save the final output directory as encrypted_data/."),
            vec!["encrypted_data"]
        );
    }

    #[test]
    fn delivery_inference_ignores_examples_inputs_and_requested_directories() {
        assert_eq!(
            infer_legacy_write_paths(
                "Create a file called solution.txt with the word found secrete_file.txt in the secrets.7z archive."
            ),
            vec!["solution.txt"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Create a file called maze_map.txt. You can create example test cases and/or unit tests."
            ),
            vec!["maze_map.txt"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Create a directory at /app/ssl/. Save the key as /app/ssl/server.key."
            ),
            vec!["ssl/server.key"]
        );
    }

    #[test]
    fn implemented_in_a_named_file_is_an_explicit_delivery() {
        assert_eq!(
            infer_legacy_write_paths(
                "The service should be implemented in a file named bitcoin_service.py."
            ),
            vec!["bitcoin_service.py"]
        );
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

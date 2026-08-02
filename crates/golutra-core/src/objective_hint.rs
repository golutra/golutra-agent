//! Conservative facts derived from natural-language objectives.
//!
//! Explicit `TaskContract` values remain authoritative. Legacy write hints only
//! fill missing delivery fields at older command boundaries; conversion pairs
//! help identify an explicitly named output without adding runtime requirements.

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

        infer_strong_delivery_paths(&tokens, &words, &mut paths, MAX_INFERRED_PATHS);
        for (index, word) in words.iter().enumerate() {
            if is_delivery_noun(word)
                && (index.saturating_sub(DELIVERY_NOUN_LOOKBACK)..index)
                    .any(|candidate| is_write_verb_at(&words, candidate))
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

            if !is_write_verb_at(&words, index) {
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
    let latest_delivery = (0..path_index)
        .rev()
        .find(|index| is_write_verb_at(words, *index));
    let latest_input = words[..path_index]
        .iter()
        .rposition(|word| is_input_marker(word));
    latest_delivery.is_some_and(|delivery| latest_input.is_none_or(|input| delivery > input))
}

fn infer_strong_delivery_paths(
    tokens: &[&str],
    words: &[String],
    paths: &mut Vec<String>,
    limit: usize,
) {
    let final_declaration_start = final_output_declaration_start(words);
    let strong_final_context = final_declaration_start
        .is_some_and(|start| output_declaration_has_write_intent(tokens, words, start));
    let has_write_context =
        strong_final_context || (0..words.len()).any(|index| is_write_verb_at(words, index));
    if !has_write_context {
        return;
    }
    let mut accepted_final_path = false;

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
                if final_declaration_start.is_some_and(|start| index >= start) {
                    accepted_final_path = true;
                }
            }
        }
        if strong_final_context
            && final_declaration_start.is_some_and(|start| {
                strong_final_path_is_delivery(tokens, words, start, index, accepted_final_path)
            })
            && let Some(path) = normalize_path(token).or_else(|| {
                let directory = token.trim_end_matches(['.', ',', ';', ':']);
                directory
                    .ends_with(['/', '\\'])
                    .then(|| normalize_directory_path(directory))
                    .flatten()
            })
        {
            push_inferred_path(paths, path, limit);
            accepted_final_path = true;
        }
    }
}

fn final_output_declaration_start(words: &[String]) -> Option<usize> {
    words.iter().enumerate().find_map(|(index, word)| {
        if word == "final"
            && words.get(index.saturating_add(1)).is_some_and(|next| {
                matches!(
                    next.as_str(),
                    "artifact" | "directory" | "model" | "output" | "trained"
                )
            })
        {
            return Some(index.saturating_add(2));
        }
        (word == "output"
            && words.get(index.saturating_add(1)).map(String::as_str) == Some("directory"))
        .then_some(index.saturating_add(2))
    })
}

fn output_declaration_has_write_intent(tokens: &[&str], words: &[String], start: usize) -> bool {
    let latest_write = (0..start)
        .rev()
        .find(|index| is_write_verb_at(words, *index));
    let latest_read = words[..start].iter().rposition(|word| {
        is_input_marker(word) || matches!(word.as_str(), "examine" | "inspect" | "list")
    });
    let write_precedes_declaration =
        latest_write.is_some_and(|write| latest_read.is_none_or(|read| write > read));
    let explicitly_assigned = words[start..words.len().min(start.saturating_add(8))]
        .iter()
        .any(|word| {
            matches!(
                word.as_str(),
                "are"
                    | "called"
                    | "can"
                    | "is"
                    | "located"
                    | "must"
                    | "named"
                    | "placed"
                    | "should"
                    | "will"
            )
        });
    let explicitly_labeled = start
        .checked_sub(1)
        .and_then(|index| tokens.get(index))
        .is_some_and(|token| token.ends_with(':'));
    write_precedes_declaration || explicitly_assigned || explicitly_labeled
}

fn strong_final_path_is_delivery(
    tokens: &[&str],
    words: &[String],
    declaration_start: usize,
    path_index: usize,
    accepted_final_path: bool,
) -> bool {
    if path_index < declaration_start {
        return false;
    }
    let list_boundary = (declaration_start..path_index).rev().find(|index| {
        matches!(words[*index].as_str(), "and" | "plus") || tokens[*index].ends_with(',')
    });
    let segment_start = list_boundary.map_or(declaration_start, |index| index.saturating_add(1));
    let segment = words[segment_start..path_index]
        .iter()
        .map(String::as_str)
        .filter(|word| {
            !matches!(
                *word,
                "a" | "an" | "can" | "final" | "may" | "must" | "should" | "the" | "will"
            )
        })
        .collect::<Vec<_>>();
    if segment.iter().any(|word| is_input_marker(word)) {
        return false;
    }
    if segment.is_empty() {
        return list_boundary.is_none() || accepted_final_path;
    }

    let last = segment.last().copied().unwrap_or_default();
    if matches!(
        last,
        "are"
            | "as"
            | "be"
            | "is"
            | "called"
            | "contain"
            | "containing"
            | "contains"
            | "include"
            | "includes"
            | "including"
            | "name"
            | "named"
    ) || matches!(
        last,
        "artifact"
            | "binary"
            | "directory"
            | "executable"
            | "file"
            | "folder"
            | "model"
            | "output"
            | "program"
            | "report"
            | "result"
            | "script"
    ) {
        return true;
    }
    if last == "of" && segment.iter().rev().take(3).any(|word| *word == "consist") {
        return true;
    }
    if matches!(last, "at" | "in" | "under")
        && segment.iter().rev().take(4).any(|word| {
            matches!(
                *word,
                "delivered" | "located" | "placed" | "saved" | "stored" | "written"
            )
        })
    {
        return true;
    }
    last == "to"
        && segment.iter().rev().take(4).any(|word| {
            matches!(
                *word,
                "deliver"
                    | "delivered"
                    | "output"
                    | "place"
                    | "save"
                    | "saved"
                    | "store"
                    | "stored"
                    | "write"
                    | "written"
            )
        })
}

fn infer_imperative_named_deliveries(
    tokens: &[&str],
    words: &[String],
    paths: &mut Vec<String>,
    limit: usize,
) {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.as_str(), "call" | "name")
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
                && (0..words.len()).any(|index| is_write_verb_at(&words, index))
            {
                pending.push(PendingDeliveryList { kind, remaining });
            }
        }

        for declaration in &mut pending {
            if declaration.remaining == 0 || !clause_matches_declared_item(&words, declaration.kind)
            {
                continue;
            }
            let candidate = declared_delivery_item_path(&tokens, &words, declaration.kind);
            if let Some(path) = candidate {
                push_inferred_path(paths, path, limit);
                declaration.remaining = declaration.remaining.saturating_sub(1);
            }
        }
        pending.retain(|declaration| declaration.remaining > 0);
    }
}

fn declared_delivery_item_path(
    tokens: &[&str],
    words: &[String],
    kind: DeclaredDeliveryKind,
) -> Option<String> {
    let candidates = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| normalize_path(token).map(|path| (index, path)))
        .collect::<Vec<_>>();

    for (index, path) in &candidates {
        let latest_delivery = words[..*index]
            .iter()
            .rposition(|word| is_declared_item_delivery_marker(word, kind));
        let latest_input = words[..*index]
            .iter()
            .rposition(|word| is_input_marker(word));
        if latest_delivery.is_some_and(|delivery| latest_input.is_none_or(|input| delivery > input))
        {
            return Some(path.clone());
        }
    }

    (candidates.len() == 1).then(|| candidates[0].1.clone())
}

fn is_declared_item_delivery_marker(word: &str, kind: DeclaredDeliveryKind) -> bool {
    is_write_verb(word)
        || matches!(word, "artifact" | "deliverable" | "output" | "result")
        || match kind {
            DeclaredDeliveryKind::File => word == "file",
            DeclaredDeliveryKind::Program => word == "program",
            DeclaredDeliveryKind::Script => word == "script",
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
        let ordinal_period = character == '.'
            && objective[..index]
                .split_whitespace()
                .next_back()
                .is_some_and(|token| {
                    !token.is_empty() && token.chars().all(|value| value.is_ascii_digit())
                })
            && objective[end..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        let boundary = matches!(character, '\n' | ';' | '；')
            || (matches!(character, '.' | '!' | '?')
                && !ordinal_period
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

fn is_write_verb_at(words: &[String], index: usize) -> bool {
    let Some(word) = words.get(index).map(String::as_str) else {
        return false;
    };
    if word != "output" {
        return is_write_verb(word);
    }
    let output_has_named_container = words
        .get(index.saturating_add(1))
        .is_some_and(|next| matches!(next.as_str(), "directory" | "file" | "folder"));
    let imperative_output = index == 0
        || index
            .checked_sub(1)
            .and_then(|prior| words.get(prior))
            .is_some_and(|prior| {
                matches!(
                    prior.as_str(),
                    "and" | "can" | "must" | "please" | "should" | "then" | "to" | "will"
                )
            });
    let output_noun = output_has_named_container && !imperative_output;
    !output_noun
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
    fn final_output_context_does_not_promote_inputs_versions_or_destinations() {
        assert_eq!(
            infer_legacy_write_paths(
                "Your final output should be report.pdf generated from source.csv."
            ),
            vec!["report.pdf"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Your final output should be report.txt and support Python 3.12."
            ),
            vec!["report.txt"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Your final output should be report.txt and be uploaded to example.com."
            ),
            vec!["report.txt"]
        );
        assert_eq!(
            infer_legacy_write_paths(
                "Your final output should run on Ubuntu 24.04 and be report.txt."
            ),
            vec!["report.txt"]
        );
        assert!(
            infer_legacy_write_paths(
                "Your final output should compare expected.json and actual.json."
            )
            .is_empty()
        );
        for objective in [
            "Inspect the output directory build/results/.",
            "Inspect output directory build/results/.",
            "Inspect the final output file report.txt.",
            "Compare the final output file report.txt with expected.txt.",
        ] {
            assert!(
                infer_legacy_write_paths(objective).is_empty(),
                "{objective}"
            );
        }
        assert_eq!(
            infer_legacy_write_paths("The output directory is build/results/"),
            vec!["build/results"]
        );
        assert_eq!(
            infer_legacy_write_paths("The final output file is report.txt."),
            vec!["report.txt"]
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
    fn declared_delivery_lists_choose_labeled_outputs_instead_of_inputs() {
        for objective in [
            "Create two files:\n1. source input.csv and result output.csv\n2. source source.json and result result.json",
            "Create two files:\n1. Source input.csv and result output.csv\n2. Source source.json and result result.json",
        ] {
            assert_eq!(
                infer_legacy_write_paths(objective),
                vec!["output.csv", "result.json"],
                "{objective}"
            );
        }
    }

    #[test]
    fn equivalent_explicit_naming_and_conversion_phrases_are_supported() {
        assert_eq!(
            infer_legacy_write_paths("Name the folder artifacts/ and name the script build.py."),
            vec!["artifacts", "build.py"]
        );
        for objective in [
            "Convert input.csv as output.parquet.",
            "Transform input.csv to output.parquet.",
        ] {
            assert_eq!(
                infer_explicit_conversion_objectives(objective),
                vec![ExplicitConversionObjectiveHint {
                    source_path: "input.csv".to_owned(),
                    output_path: "output.parquet".to_owned(),
                }],
                "{objective}"
            );
        }
    }

    #[test]
    fn external_exports_and_system_migrations_do_not_invent_delivery_paths() {
        for objective in [
            "Export report.csv to analytics.example.com.",
            "Migrate schema.sql to PostgreSQL 16.2.",
            "Serialize payload.json as JSON 3.0.",
        ] {
            assert!(
                infer_legacy_write_paths(objective).is_empty(),
                "{objective}"
            );
            assert!(
                infer_explicit_conversion_objectives(objective).is_empty(),
                "{objective}"
            );
        }
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

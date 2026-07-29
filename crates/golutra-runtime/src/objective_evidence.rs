//! Deterministic extraction and matching of requested delivery paths.
//!
//! Prompts routinely contain input paths, snippets, URLs, and example code.
//! Only path-like tokens near an explicit delivery verb become blocking
//! expectations; ambiguous text is left unverified instead of becoming a
//! false failure.

use std::{collections::HashSet, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryPathEvaluation {
    expected: Vec<String>,
    missing: Vec<String>,
}

impl DeliveryPathEvaluation {
    pub(crate) fn passed(&self) -> bool {
        self.missing.is_empty()
    }
}

pub(crate) fn evaluate_delivery_paths(
    objective: &str,
    completion_criteria: &[String],
    changed_files: &[&Path],
) -> Option<DeliveryPathEvaluation> {
    let objective_paths = extract_delivery_paths(objective, false);
    let infer_criterion_paths = !objective_paths.is_empty() || has_delivery_intent(objective);
    let mut expected = Vec::new();
    for criterion in completion_criteria {
        expected.extend(extract_delivery_paths(criterion, infer_criterion_paths));
    }
    expected.extend(objective_paths);
    deduplicate(&mut expected);
    if expected.is_empty() {
        return None;
    }
    let missing = expected
        .iter()
        .filter(|expected| {
            !changed_files
                .iter()
                .any(|observed| path_matches_expected(observed, expected))
        })
        .cloned()
        .collect();
    Some(DeliveryPathEvaluation { expected, missing })
}

fn extract_delivery_paths(text: &str, criterion: bool) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        for clause in line.split([';', '；']) {
            let tokens = clause.split_whitespace().collect::<Vec<_>>();
            for (index, token) in tokens.iter().enumerate() {
                let Some(candidate) = normalize_path_token(token) else {
                    continue;
                };
                if delivery_context(clause, &tokens, index, criterion) {
                    paths.push(candidate);
                }
            }
        }
    }
    paths
}

fn has_delivery_intent(text: &str) -> bool {
    let words = text
        .split(|character: char| !character.is_ascii_alphabetic())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    words.iter().any(|word| is_delivery_word(word))
        || [
            "写入", "保存", "输出", "创建", "生成", "修改", "更新", "交付",
        ]
        .iter()
        .any(|marker| text.contains(marker))
}

fn normalize_path_token(raw: &str) -> Option<String> {
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
            '`' | '\'' | '"' | ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '*'
        )
    });
    if token.is_empty()
        || token.len() > 512
        || is_non_path_abbreviation(token)
        || token.contains("://")
        || token
            .chars()
            .any(|character| matches!(character, '=' | '$' | '{' | '}' | '(' | ')' | ',' | ';'))
        || token.contains("</")
        || token.contains("><")
    {
        return None;
    }
    let path_like = token.contains('/')
        || token.contains('\\')
        || token.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        });
    path_like.then(|| token.to_owned())
}

fn is_non_path_abbreviation(token: &str) -> bool {
    matches!(token.to_ascii_lowercase().as_str(), "e.g" | "i.e")
}

fn delivery_context(line: &str, tokens: &[&str], index: usize, criterion: bool) -> bool {
    let start = index.saturating_sub(10);
    let before = tokens[start..index]
        .iter()
        .map(|token| normalized_word(token))
        .collect::<Vec<_>>();
    let after = tokens[index.saturating_add(1)..tokens.len().min(index.saturating_add(5))]
        .iter()
        .map(|token| normalized_word(token))
        .collect::<Vec<_>>();
    let last_delivery = before
        .iter()
        .rposition(|word| is_delivery_word(word.as_str()));
    let last_input = before.iter().rposition(|word| is_input_word(word.as_str()));
    if last_delivery.is_some_and(|delivery| last_input.is_none_or(|input| delivery > input)) {
        return true;
    }
    if criterion
        && after.iter().any(|word| {
            matches!(
                word.as_str(),
                "contains" | "exists" | "matches" | "equals" | "delivered" | "created"
            )
        })
    {
        return true;
    }
    const CJK_DELIVERY_MARKERS: &[&str] = &[
        "写入",
        "保存",
        "输出",
        "创建",
        "生成",
        "修改",
        "更新",
        "交付",
        "文件名",
    ];
    CJK_DELIVERY_MARKERS
        .iter()
        .any(|marker| line.contains(marker))
}

fn normalized_word(token: &str) -> String {
    token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
}

fn is_delivery_word(word: &str) -> bool {
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
            | "modify"
            | "modified"
            | "update"
            | "updated"
            | "copy"
            | "copied"
            | "put"
            | "place"
            | "placed"
            | "store"
            | "stored"
    )
}

fn is_input_word(word: &str) -> bool {
    matches!(
        word,
        "input"
            | "source"
            | "original"
            | "existing"
            | "read"
            | "reads"
            | "run"
            | "running"
            | "execute"
            | "executing"
            | "using"
            | "provided"
            | "added"
            | "from"
            | "given"
            | "deleted"
            | "located"
    )
}

fn path_matches_expected(path: &Path, expected: &str) -> bool {
    let observed = path.to_string_lossy().replace('\\', "/");
    let expected = expected.replace('\\', "/");
    if !expected.contains('<') {
        return observed == expected
            || observed.ends_with(&format!("/{expected}"))
            || expected.ends_with(&format!("/{observed}"));
    }
    let mut expression = String::new();
    let mut literal = String::new();
    let mut characters = expected.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '<' {
            literal.push(character);
            continue;
        }
        let mut placeholder = String::new();
        while let Some(next) = characters.peek().copied() {
            characters.next();
            if next == '>' {
                break;
            }
            placeholder.push(next);
        }
        if placeholder.is_empty()
            || !placeholder
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-'))
        {
            literal.push('<');
            literal.push_str(&placeholder);
            continue;
        }
        expression.push_str(&regex::escape(&literal));
        literal.clear();
        expression.push_str("[^/]+");
    }
    expression.push_str(&regex::escape(&literal));
    regex::Regex::new(&format!("{expression}$")).is_ok_and(|pattern| pattern.is_match(&observed))
}

fn deduplicate(paths: &mut Vec<String>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn evaluate(objective: &str, changed: &[&str]) -> Option<DeliveryPathEvaluation> {
        let changed = changed.iter().map(PathBuf::from).collect::<Vec<_>>();
        let paths = changed.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        evaluate_delivery_paths(objective, &[], &paths)
    }

    #[test]
    fn extracts_explicit_outputs_without_treating_html_as_a_path() {
        let objective = r#"Save the collected data to a CSV file named 'books.csv'.
The report should be saved to a file named 'report.txt'.
<span class="book-price">${price}</span>"#;
        let result = evaluate(objective, &["/app/books.csv", "/app/report.txt"])
            .expect("delivery expectations");

        assert!(result.passed());
        assert_eq!(result.expected, vec!["books.csv", "report.txt"]);
    }

    #[test]
    fn example_abbreviations_are_not_delivery_paths() {
        let result = evaluate(
            "Write the integer to /app/answer.txt without separators (e.g. 1000000).",
            &["/app/answer.txt"],
        )
        .expect("delivery expectation");

        assert!(result.passed());
        assert_eq!(result.expected, vec!["/app/answer.txt"]);
    }

    #[test]
    fn ignores_input_and_embedded_code_paths() {
        let objective = "Original file: /app/data.csv. Table.read('test.qdp',format='ascii.qdp')";

        assert!(evaluate(objective, &["/app/output.txt"]).is_none());
    }

    #[test]
    fn matches_placeholder_delivery_paths() {
        let result = evaluate(
            "Write observations to /app/output/<maze_id>.txt",
            &["/app/output/maze-42.txt"],
        )
        .expect("delivery expectation");

        assert!(result.passed());
    }

    #[test]
    fn criteria_can_name_a_path_before_the_validation_verb() {
        let changed = [PathBuf::from("/tmp/results.txt")];
        let paths = changed.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        let result = evaluate_delivery_paths(
            "update the result",
            &["results.txt contains expected".to_owned()],
            &paths,
        )
        .expect("criterion delivery expectation");

        assert!(result.passed());
    }

    #[test]
    fn existing_file_validation_does_not_require_a_workspace_change() {
        let result = evaluate_delivery_paths(
            "verify the existing results.txt without changing it",
            &["results.txt contains the expected result".to_owned()],
            &[],
        );

        assert!(result.is_none());
    }

    #[test]
    fn clause_boundaries_keep_diagnostic_paths_out_of_deliveries() {
        let result = evaluate(
            "read input.txt and write results.txt; diagnostic: /tmp/verify.py",
            &["/workspace/results.txt"],
        )
        .expect("delivery expectation");

        assert_eq!(result.expected, vec!["results.txt"]);
        assert!(result.passed());
    }

    #[test]
    fn observed_benchmark_input_paths_are_not_delivery_requirements() {
        let conda = evaluate(
            "Create a new conda environment using the provided environment.yml file in /app/project.\nModify the environment.yml file and verify it by running the test_imports.py script.",
            &["/app/project/environment.yml"],
        )
        .expect("environment file is the requested delivery");
        assert_eq!(conda.expected, vec!["environment.yml"]);
        assert!(conda.passed());

        assert!(
            evaluate(
                "I have created a repository and added an info.md file containing my CV. Transform the repository using content from info.md.",
                &["/app/index.html"],
            )
            .is_none()
        );
        assert!(
            evaluate(
                "Compile SQLite in /app/sqlite with gcov instrumentation and make it available in PATH.",
                &["/app/sqlite/sqlite3"],
            )
            .is_none()
        );
    }

    #[test]
    fn maze_reference_path_does_not_override_the_requested_output_pattern() {
        let result = evaluate(
            "Use /app/maze_1.txt as a reference to test your code. Once explored, create /app/output/<maze_id>.txt.",
            &["/app/output/maze-7.txt"],
        )
        .expect("maze output delivery");

        assert_eq!(result.expected, vec!["/app/output/<maze_id>.txt"]);
        assert!(result.passed());
    }
}
